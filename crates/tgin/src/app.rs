//! The running process: the engine, the sources that feed it, the data and
//! admin planes, hot reload, and the shutdown sequence.

use std::collections::BTreeSet;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use tgin_botapi::ApiError;
use tgin_botapi::gateway::{Gateway, GatewayBot, GatewayMetrics};
use tgin_core::config::{self, Bot, Config, Ingress, Webhook, WebhookSecret};
use tgin_core::engine::{BuildError, Engine, EngineRuntime};
use tgin_core::metrics::CoreMetrics;
use tgin_core::reply::ReplyHandle;
use tgin_mtproto::account::Registry;
use tgin_runtime::backoff::Backoff;
use tgin_runtime::config::Env;
use tgin_runtime::http::{
    self, ClientOptions, RunningServer, ServerOptions, TlsFiles, Via, refuse_loops,
};
use tgin_runtime::lifecycle::{Lifecycle, ShutdownTiming, SignalKind, Signals};
use tgin_runtime::obs::{Health, LogHandle, MetricsRegistry, Readiness};
use tgin_runtime::secret::{Redactor, Secret};
use tgin_runtime::supervisor::{Exit, Supervisor};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::accounts::{AccountNetwork, AccountSources, AccountsCollector, Board};
use crate::admin;
use crate::cli::{LoadError, VERSION, commit};
use crate::data_server::DataRoutes;
use crate::exit::Outcome;
use crate::mtproto_rpc::RpcEndpoint;
use crate::reload::{self, ReloadQueue, Reloader, Requests};
use crate::sources::{Kind, Sources, source_task};

/// How long the listeners and the supervised tasks get to finish once the
/// drain has ended.
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// How often shutdown checks whether the queues have drained.
const DRAIN_POLL: Duration = Duration::from_millis(10);
/// How long the drain waits, once nothing is pending, for the downstreams'
/// calls through the gateway: a bot answers an update after acknowledging
/// it, so its call can come in after the queues have emptied.
const GATEWAY_QUIET: Duration = Duration::from_secs(1);
/// How often the sources are checked for having all failed.
const SOURCES_CHECK: Duration = Duration::from_secs(1);
/// The readiness component of the engine.
const ENGINE: &str = "engine";
/// The readiness component of the data plane.
const DATA_PLANE: &str = "data_plane";
/// The readiness component of the admin plane.
const ADMIN_PLANE: &str = "admin_plane";
/// Open connections the admin plane allows.
const ADMIN_MAX_CONNECTIONS: usize = 256;
/// How long an admin client may take to send a request head.
const ADMIN_HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// The supervised task that forgets idle rate-limit keys.
const GATEWAY_SWEEP: &str = "gateway-sweep";
/// How often it runs.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// Why tgin could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Engine(#[from] BuildError),
    #[error("bot `{bot}`: {source}")]
    Bot { bot: String, source: ApiError },
    #[error("the engine has no source for bot `{0}`")]
    NoSource(String),
    #[error("the data plane could not start: {0}")]
    Serve(io::Error),
    #[error("the admin plane could not start: {0}")]
    Admin(io::Error),
}

/// What tgin starts from: a loaded config and how to load it again.
pub struct Boot {
    pub cfg: Config,
    /// The config file, which every reload reads again.
    pub path: PathBuf,
    /// The bytes `cfg` was loaded from. Once the config watcher runs, the
    /// file is compared with them, so a change made before it began is
    /// reloaded too.
    pub loaded: Vec<u8>,
    /// Resolves `${env:…}` and `${file:…}` references of reloaded configs.
    pub env: Arc<dyn Env>,
    /// Masks secrets in logs; reloads add the secrets of each new config.
    pub redactor: Arc<ArcSwap<Redactor>>,
    /// Sets the level of the process's log subscriber, so a reload applies
    /// `log.level`; `None` without one.
    pub log: Option<LogHandle>,
    /// How the config's accounts reach Telegram: over TCP
    /// ([`tcp_network`](crate::accounts::tcp_network)); tests replace it
    /// with a scripted Telegram.
    pub accounts_network: AccountNetwork,
}

impl Boot {
    /// Reads and loads the config file at `path`, resolving references with
    /// `env`; the redactor masks the config's secrets.
    ///
    /// # Errors
    ///
    /// The file cannot be read, or its errors.
    pub fn load(path: &Path, env: Arc<dyn Env>) -> Result<Self, LoadError> {
        let loaded = std::fs::read(path).map_err(LoadError::Read)?;
        let text = std::str::from_utf8(&loaded)
            .map_err(|err| LoadError::Read(io::Error::new(io::ErrorKind::InvalidData, err)))?;
        let cfg = config::load(text, path, &*env).map_err(LoadError::Invalid)?;
        let redactor = Arc::new(ArcSwap::from_pointee(Redactor::new(secrets(&cfg))));
        Ok(Self {
            cfg,
            path: path.to_owned(),
            loaded,
            env,
            redactor,
            log: None,
            accounts_network: crate::accounts::tcp_network(),
        })
    }
}

/// Starts tgin and runs it until it shuts down, handling signals from the
/// start on: a terminating signal while tgin starts stops what has started
/// and ends cleanly, and a second one forces the exit at once.
///
/// # Errors
///
/// The start failed, as for [`App::start`].
pub async fn launch(boot: Boot, mut signals: Signals) -> Result<Outcome, StartError> {
    let handles = Handles::new();
    let first = {
        // Dropping the start, when a signal wins, cancels the Bot API calls
        // it is waiting on.
        let start = App::assemble(boot, handles.clone());
        tokio::select! {
            started = start => {
                return match started {
                    Ok(app) => Ok(app.run(signals).await),
                    Err(err) => {
                        handles.stop_now().await;
                        Err(err)
                    }
                };
            }
            kind = next_terminating(&mut signals, &handles.reloads) => kind,
        }
    };
    tracing::info!(cause = ?first, "shutdown requested while starting; stopping what started");
    tokio::select! {
        biased;
        kind = next_terminating(&mut signals, &handles.reloads) => {
            handles.force(kind).await;
            Ok(Outcome::Forced)
        }
        () = handles.stop_now() => Ok(Outcome::Clean),
    }
}

/// tgin, started: sources poll, the data plane serves, consumers deliver,
/// the admin plane answers and reloads apply.
pub struct App {
    handles: Handles,
    shared: Arc<Shared>,
    data_addrs: Vec<SocketAddr>,
    admin_addrs: Vec<SocketAddr>,
    /// The data plane's routes; `None` without a data plane.
    routes: Option<Arc<DataRoutes>>,
}

/// What the admin plane and the reloads share with the app.
pub(crate) struct Shared {
    pub(crate) lifecycle: Arc<Lifecycle>,
    pub(crate) health: Arc<Health>,
    pub(crate) metrics: Arc<MetricsRegistry>,
    pub(crate) core: CoreMetrics,
    pub(crate) engine: Arc<Engine>,
    pub(crate) supervisor: Arc<Supervisor>,
    pub(crate) redactor: Arc<ArcSwap<Redactor>>,
    /// The config in effect; a reload swaps it once the engine applied it.
    pub(crate) config: ArcSwap<Config>,
    /// The Bot API gateway; a reload gives it the new bots.
    pub(crate) gateway: Arc<Gateway>,
    pub(crate) sources: Sources,
    pub(crate) reloads: ReloadQueue,
    /// Whether an admin listener is bound to a non-loopback address. The
    /// listeners are bound once, from the start-up config: a reload that
    /// changes `admin.listen` takes effect only after a restart.
    pub(crate) admin_exposed: bool,
    /// The accounts' logins (`/v1/accounts/{name}/login`).
    pub(crate) accounts: Arc<crate::admin::Accounts>,
    /// The accounts' supervisors (`GET /v1/accounts`, reloads).
    pub(crate) account_sources: Arc<AccountSources>,
}

/// What a starting app shares with whoever stops it, so that a start cut
/// short can still stop what it started.
#[derive(Clone)]
struct Handles {
    lifecycle: Arc<Lifecycle>,
    /// Stops the engine's consumer tasks gracefully.
    stop: CancellationToken,
    supervisor: Arc<Supervisor>,
    /// The data plane once it listens; taken by the shutdown, which waits
    /// for the listeners to stop.
    server: Arc<Mutex<Option<RunningServer>>>,
    /// The admin plane once it listens; closed last.
    admin: Arc<Mutex<Option<RunningServer>>>,
    /// The engine and its metrics once built.
    engine: Arc<OnceLock<(Arc<Engine>, CoreMetrics)>>,
    /// The accounts once their supervisors exist.
    accounts: Arc<OnceLock<Arc<AccountSources>>>,
    reloads: ReloadQueue,
    /// The queues' receiving ends until the reload task takes them;
    /// requests made while tgin starts wait there.
    requests: Arc<Mutex<Option<Requests>>>,
    /// Stops the reload task: no reload once the shutdown has begun.
    stop_reloads: CancellationToken,
}

/// Why the shutdown started.
#[derive(Clone, Copy, Debug)]
enum Cause {
    Signal(SignalKind),
    AllSourcesFailed,
}

impl App {
    /// Builds the engine, starts the admin plane, the data plane and the
    /// sources, registers webhooks, and reports ready.
    ///
    /// # Errors
    ///
    /// The engine cannot be built, a bot's API client cannot be set up, or
    /// the admin or data plane cannot listen. Nothing keeps running then.
    pub async fn start(boot: Boot) -> Result<Self, StartError> {
        let handles = Handles::new();
        let started = Self::assemble(boot, handles.clone()).await;
        if started.is_err() {
            handles.stop_now().await;
        }
        started
    }

    async fn assemble(boot: Boot, handles: Handles) -> Result<Self, StartError> {
        let Boot {
            cfg,
            path,
            loaded,
            env,
            redactor,
            log,
            accounts_network,
        } = boot;
        let health = Arc::new(Health::default());
        let starting = || Readiness::NotReady("starting".to_owned());
        health.set(ENGINE, starting());
        if !cfg.admin.listen.is_empty() {
            health.set(ADMIN_PLANE, starting());
        }
        if cfg.server.is_some() {
            health.set(DATA_PLANE, starting());
        }
        for bot in cfg.bots.iter().filter(|bot| bot.ingress.is_some()) {
            health.set(&source_task(&bot.name), starting());
        }

        let mut registry = MetricsRegistry::new(VERSION, commit().unwrap_or("unknown"));
        let core = CoreMetrics::register(registry.registry_mut());
        let gateway_metrics = GatewayMetrics::register(registry.registry_mut());
        // Flood sleeps of RPC calls end when the process stops ingesting.
        let endpoint = Arc::new(RpcEndpoint::new(handles.lifecycle.ingest_token()));
        let board = Arc::new(Board::default());
        registry
            .registry_mut()
            .register_collector(Box::new(AccountsCollector {
                board: Arc::clone(&board),
                rpc: endpoint.metrics(),
            }));
        let metrics = Arc::new(registry);
        let client = http::client(&ClientOptions::default());
        let supervisor = Arc::clone(&handles.supervisor);
        let force = handles.lifecycle.force_token();
        let ingest = handles.lifecycle.ingest_token();
        // This process's name in the `Via` of its pushes; both planes refuse
        // a request that carries it, so tgin never acts on its own pushes.
        let via = Via::generate();
        // Push consumers read the reply executor through this handle; the
        // gateway that executes replies needs the engine first.
        let reply = ReplyHandle::default();
        let engine = Arc::new(Engine::new(
            &cfg,
            core.clone(),
            client.clone(),
            EngineRuntime {
                supervisor: Arc::clone(&supervisor),
                stop: handles.stop.clone(),
                force: force.clone(),
                via: via.clone(),
                reply: Arc::clone(&reply),
            },
        )?);
        // Set once, here: the slot is new with `handles`.
        let _ = handles.engine.set((Arc::clone(&engine), core.clone()));
        health.set(ENGINE, Readiness::Ready);
        // Before the sources, which record each bot's identity in it.
        let gateway = Arc::new(Gateway::new(Arc::clone(&engine), gateway_metrics, &via));
        gateway.set_bots(gateway_bots(&cfg, &engine), trusted_proxies(&cfg));
        // Set once, here, before any source admits an update.
        let _ = reply.set(gateway.reply_executor());
        spawn_sweep(&supervisor, Arc::clone(&gateway), handles.stop.clone());

        let known_secrets: BTreeSet<String> = secrets(&cfg).into_iter().collect();
        let cfg = Arc::new(cfg);
        // The admin plane's logins and the account supervisors know each
        // other: a starting account waits for a login of its session, and
        // a login asks whether its account runs and wakes it at the end.
        let logins_slot: Arc<OnceLock<Weak<crate::admin::Accounts>>> = Arc::default();
        let registry = Arc::new(Registry::with_logins({
            let slot = Arc::clone(&logins_slot);
            move |name| {
                slot.get()
                    .and_then(Weak::upgrade)
                    .is_some_and(|logins| logins.login_in_progress(name))
            }
        }));
        let account_sources = Arc::new(AccountSources::new(
            Arc::clone(&engine),
            core.clone(),
            accounts_network,
            registry,
            Arc::clone(&endpoint),
            board,
        ));
        let logins = Arc::new(crate::admin::Accounts::new(Arc::new(
            crate::admin::Sessions(Arc::clone(&account_sources)),
        )));
        let _ = logins_slot.set(Arc::downgrade(&logins));
        let _ = handles.accounts.set(Arc::clone(&account_sources));
        let shared = Arc::new(Shared {
            lifecycle: Arc::clone(&handles.lifecycle),
            health: Arc::clone(&health),
            metrics,
            core,
            engine: Arc::clone(&engine),
            supervisor: Arc::clone(&supervisor),
            redactor,
            config: ArcSwap::new(Arc::clone(&cfg)),
            gateway: Arc::clone(&gateway),
            sources: Sources::new(
                Arc::clone(&engine),
                Arc::clone(&supervisor),
                Arc::clone(&health),
                client,
                Arc::clone(&gateway),
                ingest.clone(),
                force,
            ),
            reloads: handles.reloads.clone(),
            admin_exposed: cfg.admin.listen.iter().any(|addr| !addr.ip().is_loopback()),
            accounts: logins,
            account_sources: Arc::clone(&account_sources),
        });

        // Before any source starts, so the probes answer during a slow
        // start.
        let admin_addrs = if cfg.admin.listen.is_empty() {
            Vec::new()
        } else {
            let opts = ServerOptions {
                listen: cfg.admin.listen.clone(),
                tls: None,
                max_connections: ADMIN_MAX_CONNECTIONS,
                header_read_timeout: ADMIN_HEADER_TIMEOUT,
            };
            let admin = refuse_loops(
                admin::router(Arc::clone(&shared)),
                via.clone(),
                "admin",
                admin::loop_detected,
            );
            let running = http::serve(admin, &opts).await.map_err(StartError::Admin)?;
            let addrs = running.local_addrs.clone();
            *handles.admin.lock() = Some(running);
            tracing::info!(listen = ?addrs, "admin plane listening");
            health.set(ADMIN_PLANE, Readiness::Ready);
            addrs
        };

        let sources = &shared.sources;
        let mut prepared = Vec::new();
        for bot in cfg.bots.iter().filter(|bot| bot.ingress.is_some()) {
            prepared.push(sources.prepare(bot)?);
        }
        // Usernames first, so `/cmd@bot` rules match from the first update.
        sources.identify(&prepared).await;

        let (routes, data_addrs, tls) = match &cfg.server {
            Some(server) => {
                let routes = Arc::new(DataRoutes::new(
                    &cfg,
                    server,
                    &engine,
                    &|bot| sources.ingest_of(bot),
                    gateway,
                    via,
                    endpoint,
                ));
                let tls_files = server.tls.as_ref().map(|tls| TlsFiles {
                    cert: tls.cert_file.expose().clone(),
                    key: tls.key_file.expose().clone(),
                });
                // Read before the server loads them: a change in between is
                // only reloaded again.
                let tls_bytes = match &tls_files {
                    Some(files) => Some(reload::read_tls(files).await.map_err(StartError::Serve)?),
                    None => None,
                };
                let opts = ServerOptions {
                    listen: server.listen.clone(),
                    tls: tls_files,
                    max_connections: server.max_connections,
                    header_read_timeout: server.header_read_timeout,
                };
                let running = http::serve(routes.service(), &opts)
                    .await
                    .map_err(StartError::Serve)?;
                let data_addrs = running.local_addrs.clone();
                let tls = running.tls().zip(tls_bytes);
                *handles.server.lock() = Some(running);
                tracing::info!(listen = ?data_addrs, "data plane listening");
                health.set(DATA_PLANE, Readiness::Ready);
                (Some(routes), data_addrs, tls)
            }
            None => (None, Vec::new(), None),
        };

        let (long_polls, webhooks): (Vec<_>, Vec<_>) = prepared
            .into_iter()
            .partition(|bot| bot.kind() == Some(Kind::LongPoll));
        sources.start_webhooks(webhooks).await;
        for bot in long_polls {
            sources.start_long_poll(bot);
        }
        shared.account_sources.apply(&cfg).await;

        handles.lifecycle.set_running();
        tracing::info!(version = VERSION, "tgin started");

        if let Some(requests) = handles.requests.lock().take() {
            let reloader = Reloader {
                shared: Arc::clone(&shared),
                routes: routes.clone(),
                tls,
                path,
                env,
                loaded,
                secrets: known_secrets,
                log,
            };
            tokio::spawn(reloader.run(requests, handles.stop_reloads.clone()));
        }
        Ok(Self {
            handles,
            shared,
            data_addrs,
            admin_addrs,
            routes,
        })
    }

    /// The data plane's bound addresses, in `server.listen` order; empty
    /// without a data plane.
    pub fn data_addrs(&self) -> &[SocketAddr] {
        &self.data_addrs
    }

    /// The admin plane's bound addresses, in `admin.listen` order; empty
    /// without an admin plane.
    pub fn admin_addrs(&self) -> &[SocketAddr] {
        &self.admin_addrs
    }

    /// The process's metrics.
    pub fn metrics(&self) -> Arc<MetricsRegistry> {
        Arc::clone(&self.shared.metrics)
    }

    /// The readiness of every component.
    pub fn health(&self) -> Arc<Health> {
        Arc::clone(&self.shared.health)
    }

    /// What masks secrets in logs, current with the last reload.
    pub fn redactor(&self) -> Arc<ArcSwap<Redactor>> {
        Arc::clone(&self.shared.redactor)
    }

    /// The data plane's open connections.
    pub fn connections(&self) -> Connections {
        Connections(Arc::clone(&self.handles.server))
    }

    /// Rebuilds the data plane's routes from `cfg`, against the engine as it
    /// is now, and serves them from the next request on; requests already
    /// running finish on the old routes and the listeners stay as they are.
    /// Returns `false`, changing nothing, when the app or `cfg` has no data
    /// plane.
    pub fn set_routes(&self, cfg: &Config) -> bool {
        let (Some(routes), Some(server)) = (&self.routes, &cfg.server) else {
            return false;
        };
        let sources = &self.shared.sources;
        routes.rebuild(cfg, server, &self.shared.engine, &|bot| {
            sources.ingest_of(bot)
        });
        true
    }

    /// Runs until a terminating signal (or every source failing), then
    /// drains and shuts down. SIGHUP reloads the config. A further
    /// terminating signal forces the exit; when the shutdown began because
    /// every source failed, it takes two.
    pub async fn run(self, mut signals: Signals) -> Outcome {
        let reloads = self.handles.reloads.clone();
        let cause = tokio::select! {
            kind = next_terminating(&mut signals, &reloads) => Cause::Signal(kind),
            () = all_sources_failed(&self.shared.sources) => Cause::AllSourcesFailed,
        };
        let mut escalate = matches!(cause, Cause::Signal(_));
        let shutdown = self.shutdown(cause);
        tokio::pin!(shutdown);
        let forced = loop {
            tokio::select! {
                outcome = &mut shutdown => return outcome,
                kind = next_terminating(&mut signals, &reloads) => {
                    if escalate {
                        break kind;
                    }
                    tracing::info!(signal = ?kind, "already shutting down; another signal forces the exit");
                    escalate = true;
                }
            }
        };
        self.handles.force(forced).await;
        Outcome::Forced
    }

    /// The drain: readiness off and reloads refused, lame duck, ingest off,
    /// queues drained (or the grace spent), data plane and tasks closed,
    /// then the admin plane.
    async fn shutdown(&self, cause: Cause) -> Outcome {
        let runtime = &self.shared.config.load().runtime;
        let timing = ShutdownTiming {
            lame_duck: runtime.lame_duck,
            grace: runtime.shutdown_grace,
        };
        let (timing, outcome) = match cause {
            Cause::Signal(kind) => (timing.for_signal(kind), Outcome::Clean),
            Cause::AllSourcesFailed => (timing, Outcome::AllSourcesFailed),
        };
        let lifecycle = &self.handles.lifecycle;
        let engine = &self.shared.engine;
        let serving = !self.data_addrs.is_empty();
        lifecycle.begin_drain();
        self.handles.stop_reloads.cancel();
        match cause {
            Cause::Signal(kind) => tracing::info!(
                cause = ?kind,
                lame_duck = ?timing.lame_duck,
                grace = ?timing.grace,
                "shutdown started; draining"
            ),
            Cause::AllSourcesFailed => tracing::error!(
                lame_duck = ?timing.lame_duck,
                grace = ?timing.grace,
                "every source failed permanently; shutting down"
            ),
        }
        // Without a data plane no load balancer routes here.
        if serving {
            tokio::time::sleep(timing.lame_duck).await;
        }
        lifecycle.ingest_token().cancel();
        self.shared.sources.webhooks_stopped();
        let accounts = &self.shared.account_sources;
        // Everything an account admitted is committed by now; its session
        // serves RPC calls until the close.
        accounts.stop_ingest();
        engine.begin_drain();
        let gateway = &self.shared.gateway;
        let rpc = accounts.endpoint();
        // Until nothing is pending and, for `GATEWAY_QUIET` since then, no
        // call has been forwarded to Telegram, through the gateway or the
        // accounts' RPC endpoint; calls the gateway answers itself, polls
        // included, do not count. Without a data plane no call can come.
        let quiet = if serving {
            GATEWAY_QUIET
        } else {
            Duration::ZERO
        };
        let drained = tokio::time::timeout(timing.grace, async {
            let mut busy = Instant::now();
            loop {
                let now = Instant::now();
                if engine.pending() > 0 {
                    busy = now;
                } else if let Some(forwarded) = gateway.upstream_busy_at(now) {
                    busy = busy.max(forwarded);
                }
                if let Some(called) = rpc.busy_at(now) {
                    busy = busy.max(called);
                }
                if now.saturating_duration_since(busy) >= quiet {
                    return;
                }
                tokio::time::sleep(DRAIN_POLL).await;
            }
        })
        .await
        .is_ok();
        if !drained {
            tracing::warn!(pending = engine.pending(), grace = ?timing.grace, "the queues and the gateway's calls did not settle within the shutdown grace");
        }

        self.handles.stop.cancel();
        let deadline = Instant::now() + CLOSE_GRACE;
        let server = self.handles.server.lock().take();
        if let Some(server) = server {
            close(server, CLOSE_GRACE, deadline, "data").await;
        }
        accounts.close(deadline).await;
        let aborted = self.handles.supervisor.shutdown(deadline).await;
        lifecycle.force_token().cancel();
        if aborted > 0 {
            tracing::warn!(
                aborted,
                "tasks still running at the shutdown deadline were aborted"
            );
        }
        self.handles.finish();
        // Last, so that the probes and the status answer during the drain.
        let admin = self.handles.admin.lock().take();
        if let Some(admin) = admin {
            close(admin, CLOSE_GRACE, Instant::now() + CLOSE_GRACE, "admin").await;
        }
        outcome
    }
}

/// Stops `server` accepting, gives its connections `grace`, and waits for
/// it until `deadline`.
async fn close(server: RunningServer, grace: Duration, deadline: Instant, plane: &str) {
    server.graceful_shutdown(grace);
    if tokio::time::timeout_at(deadline, server.wait())
        .await
        .is_err()
    {
        tracing::warn!(plane, "a listener did not close within its grace");
    }
}

impl Handles {
    fn new() -> Self {
        let lifecycle = Arc::new(Lifecycle::new());
        let (reloads, requests) = ReloadQueue::new(Arc::clone(&lifecycle));
        Self {
            lifecycle,
            stop: CancellationToken::new(),
            supervisor: Arc::new(Supervisor::new()),
            server: Arc::new(Mutex::new(None)),
            admin: Arc::new(Mutex::new(None)),
            engine: Arc::new(OnceLock::new()),
            accounts: Arc::new(OnceLock::new()),
            reloads,
            requests: Arc::new(Mutex::new(Some(requests))),
            stop_reloads: CancellationToken::new(),
        }
    }

    /// Stops whatever has started, without a drain: ingest, consumer tasks
    /// and listeners at once, the supervised tasks within the close grace.
    /// For a start that failed or was cut short.
    async fn stop_now(&self) {
        self.lifecycle.begin_drain();
        self.stop_reloads.cancel();
        self.lifecycle.ingest_token().cancel();
        self.stop.cancel();
        self.lifecycle.force_token().cancel();
        let deadline = Instant::now() + CLOSE_GRACE;
        let server = self.server.lock().take();
        if let Some(server) = server {
            close(server, Duration::ZERO, deadline, "data").await;
        }
        let admin = self.admin.lock().take();
        if let Some(admin) = admin {
            close(admin, Duration::ZERO, deadline, "admin").await;
        }
        if let Some(accounts) = self.accounts.get() {
            accounts.close(deadline).await;
        }
        self.supervisor.shutdown(deadline).await;
        self.finish();
    }

    /// A second signal: stop everything now.
    async fn force(&self, kind: SignalKind) {
        tracing::warn!(cause = ?kind, "second shutdown signal; exiting without completing the drain");
        self.stop_reloads.cancel();
        self.lifecycle.force_token().cancel();
        self.lifecycle.ingest_token().cancel();
        self.stop.cancel();
        for server in [&self.server, &self.admin] {
            if let Some(server) = server.lock().take() {
                server.graceful_shutdown(Duration::ZERO);
            }
        }
        if let Some(accounts) = self.accounts.get() {
            accounts.close(Instant::now()).await;
        }
        self.supervisor.shutdown(Instant::now()).await;
        self.finish();
    }

    /// Counts and logs what is left undelivered.
    fn finish(&self) {
        if let Some((engine, core)) = self.engine.get() {
            let abandoned = engine.pending();
            core.shutdown_abandoned(u64::try_from(abandoned).unwrap_or(u64::MAX));
            if abandoned > 0 {
                tracing::warn!(abandoned, "exiting with updates not delivered");
            } else {
                tracing::info!(abandoned, "shutdown complete");
            }
        }
        self.lifecycle.set_stopped();
    }
}

/// Counts the data plane's open connections.
#[derive(Clone)]
pub struct Connections(Arc<Mutex<Option<RunningServer>>>);

impl Connections {
    pub fn get(&self) -> usize {
        self.0
            .lock()
            .as_ref()
            .map_or(0, RunningServer::connection_count)
    }
}

/// Every secret value of `cfg`, for log redaction: every secret the config
/// expanded ([`Config::secrets`]: bot tokens, webhook and push
/// `secret_token`s, the admin token, pull `auth_token`s, accounts'
/// `api_hash`es, phones and RPC client tokens), with the forms derived
/// from them: bot tokens' '~' forms (a self-hosted server's directory name
/// for the bot) and `%3A` and `%3a` forms (as a `files.url` origin's paths
/// carry them), and automatic webhook secrets.
pub fn secrets(cfg: &Config) -> Vec<String> {
    let mut secrets = cfg.secrets().to_vec();
    for bot in &cfg.bots {
        let token = bot.token.expose();
        secrets.extend(config::Upstream::token_dirs(token));
        secrets.extend(["%3A", "%3a"].map(|colon| token.replace(':', colon)));
        if let Some(Ingress::Webhook(hook)) = &bot.ingress
            && let Some(secret) = webhook_secret(bot, hook)
        {
            secrets.push(secret.expose().clone());
        }
    }
    secrets
}

/// The bots of `cfg` as the gateway serves them, with the engine's ids;
/// `engine` has applied `cfg`.
pub(crate) fn gateway_bots(cfg: &Config, engine: &Engine) -> Vec<GatewayBot> {
    cfg.bots
        .iter()
        .filter_map(|bot| {
            let Some(source) = engine.source_id(&bot.name) else {
                tracing::error!(bot = %bot.name, "the engine has no source for this bot; the gateway does not serve it");
                return None;
            };
            Some(GatewayBot {
                name: Arc::from(bot.name.as_str()),
                source,
                token: bot.token.clone(),
                upstream: Arc::clone(&bot.upstream),
                allow_local_files: bot.allow_local_files,
                gateway: bot.gateway.clone(),
            })
        })
        .collect()
}

/// Supervises [`GATEWAY_SWEEP`]: every [`SWEEP_EVERY`], each gateway bot's
/// limiter forgets the keys whose quota is back to a fresh key's, so a
/// stream of one-off chats does not grow it for ever. `stop` ends it.
fn spawn_sweep(supervisor: &Supervisor, gateway: Arc<Gateway>, stop: CancellationToken) {
    supervisor.spawn(
        Arc::from(GATEWAY_SWEEP),
        Backoff::new(Duration::from_secs(1), SWEEP_EVERY, Duration::ZERO),
        stop,
        move || sweep(Arc::clone(&gateway)),
    );
}

/// One run of [`GATEWAY_SWEEP`]; it ends only when its task is stopped.
async fn sweep(gateway: Arc<Gateway>) -> Exit {
    let mut every = tokio::time::interval_at(Instant::now() + SWEEP_EVERY, SWEEP_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        gateway.sweep_limiters(Instant::now());
    }
}

/// The proxies whose forwarding headers the data plane trusts.
pub(crate) fn trusted_proxies(cfg: &Config) -> Vec<tgin_botapi::IpNet> {
    cfg.server
        .as_ref()
        .map(|server| server.trusted_proxies.clone())
        .unwrap_or_default()
}

/// The secret Telegram must send with each update of `bot`'s webhook.
pub(crate) fn webhook_secret(bot: &Bot, hook: &Webhook) -> Option<Secret<String>> {
    match &hook.secret_token {
        WebhookSecret::Auto => Some(Secret::new(tgin_botapi::webhook_secret(&bot.token))),
        WebhookSecret::None => None,
        WebhookSecret::Token(secret) => Some(secret.clone()),
    }
}

/// The next signal that asks to shut down; SIGHUP on the way queues a
/// reload. Never returns once the signal stream has ended.
async fn next_terminating(signals: &mut Signals, reloads: &ReloadQueue) -> SignalKind {
    loop {
        match signals.recv().await {
            Some(kind) if kind.is_terminating() => return kind,
            Some(_) => reloads.signal(),
            None => std::future::pending().await,
        }
    }
}

/// Returns once every source has failed for good, checking each second.
async fn all_sources_failed(sources: &Sources) {
    let mut ticks = tokio::time::interval(SOURCES_CHECK);
    loop {
        ticks.tick().await;
        if sources.all_failed() {
            return;
        }
    }
}
