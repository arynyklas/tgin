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

use parking_lot::{Mutex, RwLock};
use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeMetric as _};
use prometheus_client::metrics::MetricType;
use prometheus_client::metrics::counter::ConstCounter;
use prometheus_client::metrics::gauge::ConstGauge;
use tgin_core::config::{self, Config};
use tgin_core::engine::Engine;
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

/// The accounts of the running config.
pub(crate) struct AccountSources {
    engine: Arc<Engine>,
    network: AccountNetwork,
    registry: Arc<Registry>,
    endpoint: Arc<RpcEndpoint>,
    board: Arc<Board>,
    running: Mutex<BTreeMap<String, Running>>,
}

impl AccountSources {
    pub(crate) fn new(
        engine: Arc<Engine>,
        network: AccountNetwork,
        registry: Arc<Registry>,
        endpoint: Arc<RpcEndpoint>,
        board: Arc<Board>,
    ) -> Self {
        Self {
            engine,
            network,
            registry,
            endpoint,
            board,
            running: Mutex::new(BTreeMap::new()),
        }
    }

    /// The RPC endpoint the accounts are served on.
    pub(crate) fn endpoint(&self) -> &Arc<RpcEndpoint> {
        &self.endpoint
    }

    /// Makes the running accounts those of `cfg`: starts the new ones,
    /// stops the removed ones, restarts those whose `api_id`, `api_hash` or
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
        for account in closing {
            account.close().await;
        }
        for config in starting {
            self.start(config);
        }
    }

    fn start(&self, config: config::Account) {
        let limiter = Arc::new(AccountLimiter::new(config.rpc.max_rps));
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
        self.endpoint.insert(RpcAccount::new(
            &config.name,
            &config.rpc,
            account.rpc(),
            account.affected(),
            Arc::clone(&limiter),
        ));
        self.board
            .0
            .write()
            .insert(config.name.clone(), account.clone());
        tracing::info!(account = %config.name, "account started");
        self.running.lock().insert(
            config.name.clone(),
            Running {
                account,
                config,
                limiter,
            },
        );
    }

    /// Whether the account `name` holds, or will take again on its own,
    /// its session: logins and logouts wait.
    pub(crate) fn running(&self, name: &str) -> bool {
        self.running
            .lock()
            .get(name)
            .is_some_and(|run| run.account.state().active())
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

    /// Stops every account's ingest; their RPC goes on until
    /// [`close`](Self::close).
    pub(crate) fn stop_ingest(&self) {
        for run in self.running.lock().values() {
            run.account.stop_ingest();
        }
    }

    /// Closes every account, aborting those still running at `deadline`.
    pub(crate) async fn close(&self, deadline: Instant) {
        let accounts: Vec<Account> = std::mem::take(&mut *self.running.lock())
            .into_values()
            .map(|run| run.account)
            .collect();
        for account in accounts {
            if tokio::time::timeout_at(deadline, account.close())
                .await
                .is_err()
            {
                tracing::warn!(account = %account.name(), "the account did not close in time");
                account.abort();
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
