//! The accounts of the running config: one supervised
//! [`Account`](tgin_mtproto::account::Account) each, feeding the engine
//! ([`AccountIngest`]) and served on the raw-TL RPC endpoint, plus their
//! status (`GET /v1/accounts`) and their `tgin_mtproto_*` metrics.
//!
//! A reload restarts an account whose `api_id`, `api_hash` or
//! `session_dir` changed, and applies a change of its `rpc` settings in
//! place, through the endpoint's account map. The shutdown stops every
//! account's ingest when the process stops ingesting, waits for RPC calls
//! in its drain, then closes the accounts.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeMetric as _};
use prometheus_client::metrics::MetricType;
use prometheus_client::metrics::counter::ConstCounter;
use prometheus_client::metrics::gauge::ConstGauge;
use tgin_core::config::{self, Config};
use tgin_core::engine::Engine;
use tgin_core::metrics::CoreMetrics;
use tgin_mtproto::InitParams;
use tgin_mtproto::account::{
    Account, AccountSpec, AccountState, AccountStatus, Network, Registry, TcpNetwork,
};
use tokio::time::Instant;

use crate::account_ingest::AccountIngest;
use crate::mtproto_rpc::{AccountLimiter, RpcAccount, RpcEndpoint, RpcMetrics};

/// How an account of the config reaches Telegram: over TCP in production
/// ([`tcp_network`]); tests put a scripted Telegram here.
pub type AccountNetwork = Arc<dyn Fn(&config::Account) -> Arc<dyn Network> + Send + Sync>;

/// Telegram over TCP, with the account's `api_id`.
#[must_use]
pub fn tcp_network() -> AccountNetwork {
    Arc::new(|account| Arc::new(TcpNetwork::new(InitParams::new(account.api_id))))
}

/// The accounts that run, by name, as the status and the metrics read them.
#[derive(Default)]
pub(crate) struct Board(RwLock<BTreeMap<String, Account>>);

impl Board {
    pub(crate) fn statuses(&self) -> Vec<AccountStatus> {
        self.0.read().values().map(Account::status).collect()
    }
}

/// One running account and the config it runs from.
struct Running {
    account: Account,
    config: config::Account,
    limiter: Arc<AccountLimiter>,
}

/// How long a logout waits for a running account's RPC calls before it
/// closes the account's session.
pub(crate) const PARK_RPC_WAIT: Duration = Duration::from_secs(10);
/// How long starting an account waits for it to read its session and
/// claim its user, so accounts claim in config order.
const CHECK_WAIT: Duration = Duration::from_secs(5);

/// The accounts of the running config.
pub(crate) struct AccountSources {
    engine: Arc<Engine>,
    core: CoreMetrics,
    network: AccountNetwork,
    registry: Arc<Registry>,
    endpoint: Arc<RpcEndpoint>,
    board: Arc<Board>,
    running: Mutex<BTreeMap<String, Running>>,
    /// The shutdown stopped the accounts: nothing starts any more.
    stopped: AtomicBool,
}

impl AccountSources {
    pub(crate) fn new(
        engine: Arc<Engine>,
        core: CoreMetrics,
        network: AccountNetwork,
        registry: Arc<Registry>,
        endpoint: Arc<RpcEndpoint>,
        board: Arc<Board>,
    ) -> Self {
        Self {
            engine,
            core,
            network,
            registry,
            endpoint,
            board,
            running: Mutex::new(BTreeMap::new()),
            stopped: AtomicBool::new(false),
        }
    }

    /// The RPC endpoint the accounts are served on.
    pub(crate) fn endpoint(&self) -> &Arc<RpcEndpoint> {
        &self.endpoint
    }

    /// Makes the running accounts those of `cfg`: starts the new ones, in
    /// config order (of two accounts of one user, the first wins), stops
    /// the removed ones, restarts those whose `api_id`, `api_hash` or
    /// `session_dir` changed (the old one releases the session first), and
    /// applies `rpc` changes in place.
    pub(crate) async fn apply(&self, cfg: &Config) {
        let mut closing = Vec::new();
        let mut starting = Vec::new();
        {
            let mut running = self.running.lock();
            let names: Vec<String> = running.keys().cloned().collect();
            for name in names {
                let wanted = cfg.accounts.iter().find(|a| a.name == name);
                match wanted {
                    Some(new) => {
                        let Some(run) = running.get_mut(&name) else {
                            continue;
                        };
                        if restarts(&run.config, new) {
                            if let Some(run) = running.remove(&name) {
                                tracing::info!(account = %name, "the account's session settings changed; it restarts");
                                closing.push(run.account);
                            }
                            starting.push(new.clone());
                        } else {
                            if run.config.rpc != new.rpc {
                                run.limiter.set_max_rps(new.rpc.max_rps);
                                self.endpoint.insert(RpcAccount::new(
                                    &name,
                                    &new.rpc,
                                    run.account.rpc(),
                                    run.account.affected(),
                                    Arc::clone(&run.limiter),
                                ));
                                tracing::info!(account = %name, "the account's RPC settings changed");
                            }
                            run.config = new.clone();
                        }
                    }
                    None => {
                        if let Some(run) = running.remove(&name) {
                            self.endpoint.remove(&name);
                            self.board.0.write().remove(&name);
                            self.core.remove_source(&name);
                            tracing::info!(account = %name, "the account was removed; it stops");
                            closing.push(run.account);
                        }
                    }
                }
            }
            for account in &cfg.accounts {
                if !running.contains_key(&account.name)
                    && !starting.iter().any(|a| a.name == account.name)
                {
                    starting.push(account.clone());
                }
            }
        }
        starting.sort_by_key(|a| cfg.accounts.iter().position(|c| c.name == a.name));
        // Closing releases their users: an account refused as a duplicate
        // of one of them looks again.
        for account in closing {
            account.close().await;
        }
        for config in starting {
            self.start(config).await;
        }
    }

    /// Starts the account of `config`, unless the shutdown stopped the
    /// accounts, and waits (bounded) until it has claimed its user.
    async fn start(&self, config: config::Account) {
        let limiter = Arc::new(AccountLimiter::new(config.rpc.max_rps));
        let account = {
            // Under the lock the shutdown stops the accounts with: an
            // account started here is stopped by it, or not started.
            let mut running = self.running.lock();
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }
            let spec = AccountSpec {
                name: config.name.clone(),
                session_dir: config.session_dir.expose().clone(),
                charge: Some(Arc::clone(&limiter) as Arc<dyn tgin_mtproto::rpc::Charge>),
            };
            let sink = Arc::new(AccountIngest::new(Arc::clone(&self.engine), &config.name));
            let account = Account::start(
                spec,
                sink,
                (self.network)(&config),
                Arc::clone(&self.registry),
            );
            running.insert(
                config.name.clone(),
                Running {
                    account: account.clone(),
                    config: config.clone(),
                    limiter: Arc::clone(&limiter),
                },
            );
            account
        };
        self.endpoint.insert(RpcAccount::new(
            &config.name,
            &config.rpc,
            account.rpc(),
            account.affected(),
            limiter,
        ));
        self.board
            .0
            .write()
            .insert(config.name.clone(), account.clone());
        tracing::info!(account = %config.name, "account started");
        let _ = tokio::time::timeout(CHECK_WAIT, account.checked()).await;
    }

    /// Whether the account `name` holds, or will take again on its own,
    /// its session.
    pub(crate) fn running(&self, name: &str) -> bool {
        self.running
            .lock()
            .get(name)
            .is_some_and(|run| run.account.state().active())
    }

    /// Parks the account `name` for a logout: its ingest stops (everything
    /// admitted is committed), its RPC calls get [`PARK_RPC_WAIT`], and its
    /// session is released. It starts again at once and waits, as
    /// `logged_out`, while the logout holds the session.
    pub(crate) async fn park(&self, name: &str) {
        let run = self.running.lock().remove(name);
        let Some(run) = run else {
            return;
        };
        tracing::info!(account = %name, "the account is parked for a logout");
        run.account.park(PARK_RPC_WAIT).await;
        self.start(run.config).await;
    }

    /// The session of account `name` changed outside it (a login or a
    /// logout ended).
    pub(crate) fn wake(&self, name: &str) {
        if let Some(run) = self.running.lock().get(name) {
            run.account.wake();
        }
    }

    pub(crate) fn statuses(&self) -> Vec<AccountStatus> {
        self.board.statuses()
    }

    /// Stops every account's ingest, for good: no account starts after
    /// this, a reload's included. Their RPC goes on until
    /// [`close`](Self::close).
    pub(crate) fn stop_ingest(&self) {
        let running = self.running.lock();
        self.stopped.store(true, Ordering::SeqCst);
        for run in running.values() {
            run.account.stop_ingest();
        }
    }

    /// Closes every account, aborting those still running at `deadline`.
    pub(crate) async fn close(&self, deadline: Instant) {
        let accounts: Vec<Account> = {
            let mut running = self.running.lock();
            self.stopped.store(true, Ordering::SeqCst);
            std::mem::take(&mut *running)
                .into_values()
                .map(|run| run.account)
                .collect()
        };
        for account in accounts {
            if tokio::time::timeout_at(deadline, account.close())
                .await
                .is_err()
            {
                tracing::warn!(account = %account.name(), "the account did not close in time");
                account.abort();
                account.close().await;
            }
        }
    }
}

/// Whether a change from `old` to `new` needs the account to restart: its
/// session or the app it logs in as changed.
fn restarts(old: &config::Account, new: &config::Account) -> bool {
    old.api_id != new.api_id || old.api_hash != new.api_hash || old.session_dir != new.session_dir
}

/// The `tgin_mtproto_*` metrics, read at each scrape. With no account in
/// the config, and none ever served, it writes nothing.
#[derive(Debug)]
pub(crate) struct AccountsCollector {
    pub(crate) board: Arc<Board>,
    pub(crate) rpc: Arc<RpcMetrics>,
}

impl std::fmt::Debug for Board {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Board")
            .field("accounts", &self.0.read().len())
            .finish()
    }
}

impl Collector for AccountsCollector {
    fn encode(&self, mut encoder: DescriptorEncoder) -> std::fmt::Result {
        let accounts = self.board.statuses();
        if !accounts.is_empty() {
            let mut states = encoder.encode_descriptor(
                "mtproto_account_state",
                "Account state (1 = current)",
                None,
                MetricType::Gauge,
            )?;
            for status in &accounts {
                let current = status.state.index();
                for (i, state) in AccountState::NAMES.iter().enumerate() {
                    let labels = [("account", &*status.name), ("state", *state)];
                    ConstGauge::new(i64::from(i == current))
                        .encode(states.encode_family(&labels)?)?;
                }
            }
            let mut updates = encoder.encode_descriptor(
                "mtproto_updates",
                "Updates an account admitted",
                None,
                MetricType::Counter,
            )?;
            for status in &accounts {
                let labels = [("account", &*status.name)];
                ConstCounter::new(status.updates).encode(updates.encode_family(&labels)?)?;
            }
            let mut dropped = encoder.encode_descriptor(
                "mtproto_updates_dropped",
                "Updates an account dropped, by reason",
                None,
                MetricType::Counter,
            )?;
            for status in &accounts {
                for (reason, n) in [
                    ("undecodable", status.dropped.undecodable),
                    ("full", status.dropped.full),
                ] {
                    let labels = [("account", &*status.name), ("reason", reason)];
                    ConstCounter::new(n).encode(dropped.encode_family(&labels)?)?;
                }
            }
        }
        self.rpc.encode(&mut encoder)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tgin_core::engine::EngineRuntime;
    use tgin_mtproto::testkit::FakeNetwork;
    use tgin_runtime::config::MapEnv;
    use tgin_runtime::obs::MetricsRegistry;
    use tgin_runtime::supervisor::Supervisor;
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// After the shutdown stopped the accounts, a reload still on its way
    /// starts none.
    #[tokio::test]
    async fn nothing_starts_once_the_accounts_are_stopped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = format!(
            "version: 1\naccounts:\n  main:\n    api_id: 1\n    api_hash: h\n    session_dir: '{}'\n",
            dir.path().join("main").display()
        );
        let cfg = tgin_core::config::load(&yaml, Path::new("tgin.yaml"), &MapEnv::default())
            .expect("valid config");
        let mut registry = MetricsRegistry::new("test", "test");
        let core = CoreMetrics::register(registry.registry_mut());
        let engine = Engine::new(
            &cfg,
            core.clone(),
            reqwest::Client::new(),
            EngineRuntime {
                supervisor: Arc::new(Supervisor::new()),
                stop: CancellationToken::new(),
                force: CancellationToken::new(),
                via: tgin_runtime::http::Via::generate(),
                reply: Default::default(),
            },
        )
        .expect("engine");
        let net = FakeNetwork::new();
        let network: AccountNetwork = {
            let net = net.clone();
            Arc::new(move |_| Arc::new(net.clone()))
        };
        let sources = AccountSources::new(
            Arc::new(engine),
            core,
            network,
            Arc::new(Registry::default()),
            Arc::new(RpcEndpoint::new(CancellationToken::new())),
            Arc::new(Board::default()),
        );
        sources.stop_ingest();
        sources.apply(&cfg).await;
        assert!(sources.statuses().is_empty(), "no account started");
        assert!(!dir.path().join("main").exists(), "no session opened");
        assert!(net.dials().is_empty());
    }
}
