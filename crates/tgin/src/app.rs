//! The running process: the engine, the sources that feed it, the data
//! plane, and the shutdown sequence.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::Router;
use parking_lot::Mutex;
use tgin_botapi::{ApiError, BotClient, BotUser, LongPollOptions, LongPollSource, SetWebhook};
use tgin_core::config::{Bot, Config, ConsumerKind, Ingress, Webhook, WebhookSecret};
use tgin_core::engine::{BuildError, Engine, EngineRuntime};
use tgin_core::ids::SourceId;
use tgin_core::metrics::{CoreMetrics, SourceMetrics, SourceStateLabel};
use tgin_runtime::backoff::{Backoff, honor_retry_after};
use tgin_runtime::http::{self, ClientOptions, RunningServer, ServerOptions, TlsFiles};
use tgin_runtime::lifecycle::{Lifecycle, ShutdownTiming, SignalKind, Signals};
use tgin_runtime::obs::{Health, MetricsRegistry, Readiness};
use tgin_runtime::secret::Secret;
use tgin_runtime::supervisor::{Exit, Supervisor, TaskState};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::cli::{VERSION, commit};
use crate::data_server::{self, Probes};
use crate::exit::Outcome;

/// How long the listeners and the supervised tasks get to finish once the
/// drain has ended.
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// How often shutdown checks whether the queues have drained.
const DRAIN_POLL: Duration = Duration::from_millis(10);
/// How often the sources are checked for having all failed.
const SOURCES_CHECK: Duration = Duration::from_secs(1);
/// The readiness component of the engine.
const ENGINE: &str = "engine";
/// The readiness component of the data plane.
const DATA_PLANE: &str = "data_plane";
/// The supervisor's task-name prefix of long-poll sources.
const SOURCE_PREFIX: &str = "source:";
/// The supervisor's task-name prefix of a webhook bot's background `getMe`.
const IDENTITY_PREFIX: &str = "identity:";
/// The backoff of a webhook bot's background `getMe`: `(initial, max, floor)`.
const IDENTITY_BACKOFF: (Duration, Duration, Duration) = (
    Duration::from_secs(1),
    Duration::from_secs(300),
    Duration::from_millis(100),
);

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
    Serve(std::io::Error),
}

/// Starts tgin and runs it until it shuts down, handling signals from the
/// start on: a terminating signal while tgin starts stops what has started
/// and ends cleanly, and a second one forces the exit at once.
///
/// # Errors
///
/// The start failed, as for [`App::start`].
pub async fn launch(cfg: Config, mut signals: Signals) -> Result<Outcome, StartError> {
    let handles = Handles::new();
    let first = {
        // Dropping the start, when a signal wins, cancels the Bot API calls
        // it is waiting on.
        let start = App::assemble(cfg, handles.clone());
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
            kind = next_terminating(&mut signals) => kind,
        }
    };
    tracing::info!(cause = ?first, "shutdown requested while starting; stopping what started");
    tokio::select! {
        biased;
        kind = next_terminating(&mut signals) => {
            handles.force(kind).await;
            Ok(Outcome::Forced)
        }
        () = handles.stop_now() => Ok(Outcome::Clean),
    }
}

/// tgin, started: sources poll, the data plane serves, consumers deliver.
pub struct App {
    handles: Handles,
    timing: ShutdownTiming,
    health: Arc<Health>,
    metrics: Arc<MetricsRegistry>,
    engine: Arc<Engine>,
    data_addrs: Vec<SocketAddr>,
    /// The data plane's routes, which the listeners serve from the next
    /// request on when replaced; `None` without a data plane.
    routes: Option<Arc<ArcSwap<Router>>>,
    probes: Probes,
    /// The webhook sources' metrics, whose state follows the data plane.
    webhook_metrics: Vec<SourceMetrics>,
    /// Whether every source failing ends the process: only with long-poll
    /// sources and no webhook, which could still deliver.
    exit_when_sources_fail: bool,
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
    /// The engine and its metrics once built.
    engine: Arc<OnceLock<(Arc<Engine>, CoreMetrics)>>,
}

/// Why the shutdown started.
#[derive(Clone, Copy, Debug)]
enum Cause {
    Signal(SignalKind),
    AllSourcesFailed,
}

/// A webhook bot, for its start-up calls.
struct WebhookBot<'a> {
    id: SourceId,
    bot: &'a Bot,
    hook: &'a Webhook,
    client: BotClient,
    metrics: SourceMetrics,
}

impl App {
    /// Builds the engine, starts the data plane and the sources, registers
    /// webhooks, and reports ready.
    ///
    /// # Errors
    ///
    /// The engine cannot be built, a bot's API client cannot be set up, or
    /// the data plane cannot listen. Nothing keeps running then.
    pub async fn start(cfg: Config) -> Result<Self, StartError> {
        let handles = Handles::new();
        let started = Self::assemble(cfg, handles.clone()).await;
        if started.is_err() {
            handles.stop_now().await;
        }
        started
    }

    async fn assemble(cfg: Config, handles: Handles) -> Result<Self, StartError> {
        let health = Arc::new(Health::default());
        let starting = || Readiness::NotReady("starting".to_owned());
        health.set(ENGINE, starting());
        if cfg.server.is_some() {
            health.set(DATA_PLANE, starting());
        }
        for bot in cfg.bots.iter().filter(|bot| bot.ingress.is_some()) {
            health.set(&source_task(&bot.name), starting());
        }

        let mut registry = MetricsRegistry::new(VERSION, commit().unwrap_or("unknown"));
        let core = CoreMetrics::register(registry.registry_mut());
        let metrics = Arc::new(registry);
        let client = http::client(&ClientOptions::default());
        let supervisor = Arc::clone(&handles.supervisor);
        let force = handles.lifecycle.force_token();
        let ingest = handles.lifecycle.ingest_token();
        let engine = Arc::new(Engine::new(
            &cfg,
            core.clone(),
            client.clone(),
            EngineRuntime {
                supervisor: Arc::clone(&supervisor),
                stop: handles.stop.clone(),
                force: force.clone(),
            },
        )?);
        // Set once, here: the slot is new with `handles`.
        let _ = handles.engine.set((Arc::clone(&engine), core));
        health.set(ENGINE, Readiness::Ready);

        // Source ids are the engine's, never config positions.
        let mut long_polls = Vec::new();
        let mut webhooks = Vec::new();
        for bot in &cfg.bots {
            let Some(ingress) = &bot.ingress else {
                continue;
            };
            let (id, metrics) = engine_source(&engine, &bot.name)
                .ok_or_else(|| StartError::NoSource(bot.name.clone()))?;
            let api = BotClient::new(client.clone(), bot.api_url.expose(), &bot.token).map_err(
                |source| StartError::Bot {
                    bot: bot.name.clone(),
                    source,
                },
            )?;
            match ingress {
                Ingress::LongPoll(poll) => long_polls.push((id, bot, poll, api, metrics)),
                Ingress::Webhook(hook) => webhooks.push(WebhookBot {
                    id,
                    bot,
                    hook,
                    client: api,
                    metrics,
                }),
            }
        }

        // Usernames first, so `/cmd@bot` rules match from the first update;
        // a bot whose first `getMe` fails keeps trying in the background.
        for hook in &webhooks {
            match hook.client.get_me().await {
                Ok(user) => engine.set_username(hook.id, user.username),
                Err(err) => {
                    tracing::warn!(
                        source = %hook.bot.name,
                        error = %err,
                        "getMe failed; commands addressed to this bot by @username do not match until it succeeds"
                    );
                    learn_username(&supervisor, &engine, &ingest, &force, hook, &err);
                }
            }
        }

        let probes = Probes {
            lifecycle: Arc::clone(&handles.lifecycle),
            health: Arc::clone(&health),
        };
        let (routes, data_addrs) = match &cfg.server {
            Some(server) => {
                let router = data_server::router(&cfg, server, &engine, &ingest, probes.clone());
                let routes = Arc::new(ArcSwap::from_pointee(router));
                let opts = ServerOptions {
                    listen: server.listen.clone(),
                    tls: server.tls.as_ref().map(|tls| TlsFiles {
                        cert: tls.cert_file.expose().clone(),
                        key: tls.key_file.expose().clone(),
                    }),
                    max_connections: server.max_connections,
                    header_read_timeout: server.header_read_timeout,
                };
                let running = http::serve(data_server::delegating(Arc::clone(&routes)), &opts)
                    .await
                    .map_err(StartError::Serve)?;
                let data_addrs = running.local_addrs.clone();
                *handles.server.lock() = Some(running);
                tracing::info!(listen = ?data_addrs, "data plane listening");
                health.set(DATA_PLANE, Readiness::Ready);
                (Some(routes), data_addrs)
            }
            None => (None, Vec::new()),
        };

        for hook in &webhooks {
            hook.metrics.set_state(SourceStateLabel::Running);
            if hook.hook.register {
                register(hook).await;
            } else {
                tracing::info!(source = %hook.bot.name, "webhook served; not registered with Telegram (register: false)");
            }
            health.set(&source_task(&hook.bot.name), Readiness::Ready);
        }

        for (id, bot, poll, api, metrics) in long_polls {
            let name: Arc<str> = Arc::from(bot.name.as_str());
            let identity_engine = Arc::clone(&engine);
            let source = Arc::new(LongPollSource::new(
                id,
                Arc::clone(&name),
                api,
                LongPollOptions {
                    timeout: poll.timeout,
                    limit: poll.limit,
                    allowed_updates: poll.allowed_updates.clone(),
                    delete_webhook_on_start: poll.delete_webhook_on_start,
                    ..LongPollOptions::default()
                },
                Arc::clone(&engine),
                metrics,
                Arc::new(move |id: SourceId, user: &BotUser| {
                    identity_engine.set_username(id, user.username.clone());
                }),
            ));
            let ingest = ingest.clone();
            // The supervisor's token aborts; the source stops on `ingest`
            // by itself.
            supervisor.spawn(
                Arc::from(source_task(&name)),
                Backoff::new(
                    Duration::from_secs(1),
                    Duration::from_secs(60),
                    Duration::ZERO,
                ),
                force.clone(),
                move || {
                    let source = Arc::clone(&source);
                    let ingest = ingest.clone();
                    async move { source.run(ingest).await }
                },
            );
            health.set(&source_task(&name), Readiness::Ready);
        }

        let long_poll_sources = cfg
            .bots
            .iter()
            .filter(|bot| matches!(bot.ingress, Some(Ingress::LongPoll(_))))
            .count();
        handles.lifecycle.set_running();
        tracing::info!(version = VERSION, "tgin started");
        Ok(Self {
            timing: ShutdownTiming {
                lame_duck: cfg.runtime.lame_duck,
                grace: cfg.runtime.shutdown_grace,
            },
            exit_when_sources_fail: long_poll_sources > 0 && webhooks.is_empty(),
            webhook_metrics: webhooks.into_iter().map(|hook| hook.metrics).collect(),
            handles,
            health,
            metrics,
            engine,
            data_addrs,
            routes,
            probes,
        })
    }

    /// The data plane's bound addresses, in `server.listen` order; empty
    /// without a data plane.
    pub fn data_addrs(&self) -> &[SocketAddr] {
        &self.data_addrs
    }

    /// The process's metrics.
    pub fn metrics(&self) -> Arc<MetricsRegistry> {
        Arc::clone(&self.metrics)
    }

    /// The readiness of every component.
    pub fn health(&self) -> Arc<Health> {
        Arc::clone(&self.health)
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
        let ingest = self.handles.lifecycle.ingest_token();
        let router = data_server::router(cfg, server, &self.engine, &ingest, self.probes.clone());
        routes.store(Arc::new(router));
        true
    }

    /// Runs until a terminating signal (or every source failing), then
    /// drains and shuts down. A further terminating signal forces the exit;
    /// when the shutdown began because every source failed, it takes two.
    pub async fn run(self, mut signals: Signals) -> Outcome {
        let cause = tokio::select! {
            kind = next_terminating(&mut signals) => Cause::Signal(kind),
            () = all_sources_failed(&self.handles.supervisor, self.exit_when_sources_fail) => Cause::AllSourcesFailed,
        };
        let mut escalate = matches!(cause, Cause::Signal(_));
        let shutdown = self.shutdown(cause);
        tokio::pin!(shutdown);
        let forced = loop {
            tokio::select! {
                outcome = &mut shutdown => return outcome,
                kind = next_terminating(&mut signals) => {
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

    /// The drain: readiness off, lame duck, ingest off, queues drained (or
    /// the grace spent), listeners and tasks closed.
    async fn shutdown(&self, cause: Cause) -> Outcome {
        let (timing, outcome) = match cause {
            Cause::Signal(kind) => (self.timing.for_signal(kind), Outcome::Clean),
            Cause::AllSourcesFailed => (self.timing, Outcome::AllSourcesFailed),
        };
        let lifecycle = &self.handles.lifecycle;
        let serving = !self.data_addrs.is_empty();
        lifecycle.begin_drain();
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
        for metrics in &self.webhook_metrics {
            metrics.set_state(SourceStateLabel::Stopped);
        }
        self.engine.begin_drain();
        let drained = tokio::time::timeout(timing.grace, async {
            while self.engine.pending() > 0 {
                tokio::time::sleep(DRAIN_POLL).await;
            }
        })
        .await
        .is_ok();
        if !drained {
            tracing::warn!(pending = self.engine.pending(), grace = ?timing.grace, "the queues did not drain within the shutdown grace");
        }

        self.handles.stop.cancel();
        let deadline = Instant::now() + CLOSE_GRACE;
        let server = self.handles.server.lock().take();
        if let Some(server) = server {
            server.graceful_shutdown(CLOSE_GRACE);
            if tokio::time::timeout_at(deadline, server.wait())
                .await
                .is_err()
            {
                tracing::warn!("the data plane did not close within its grace");
            }
        }
        let aborted = self.handles.supervisor.shutdown(deadline).await;
        lifecycle.force_token().cancel();
        if aborted > 0 {
            tracing::warn!(
                aborted,
                "tasks still running at the shutdown deadline were aborted"
            );
        }
        self.handles.finish();
        outcome
    }
}

impl Handles {
    fn new() -> Self {
        Self {
            lifecycle: Arc::new(Lifecycle::new()),
            stop: CancellationToken::new(),
            supervisor: Arc::new(Supervisor::new()),
            server: Arc::new(Mutex::new(None)),
            engine: Arc::new(OnceLock::new()),
        }
    }

    /// Stops whatever has started, without a drain: ingest, consumer tasks
    /// and listeners at once, the supervised tasks within the close grace.
    /// For a start that failed or was cut short.
    async fn stop_now(&self) {
        self.lifecycle.begin_drain();
        self.lifecycle.ingest_token().cancel();
        self.stop.cancel();
        self.lifecycle.force_token().cancel();
        let deadline = Instant::now() + CLOSE_GRACE;
        let server = self.server.lock().take();
        if let Some(server) = server {
            server.graceful_shutdown(Duration::ZERO);
            if tokio::time::timeout_at(deadline, server.wait())
                .await
                .is_err()
            {
                tracing::warn!("the data plane did not close within its grace");
            }
        }
        self.supervisor.shutdown(deadline).await;
        self.finish();
    }

    /// A second signal: stop everything now.
    async fn force(&self, kind: SignalKind) {
        tracing::warn!(cause = ?kind, "second shutdown signal; exiting without completing the drain");
        self.lifecycle.force_token().cancel();
        self.lifecycle.ingest_token().cancel();
        self.stop.cancel();
        if let Some(server) = self.server.lock().take() {
            server.graceful_shutdown(Duration::ZERO);
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

/// Every secret value of `cfg`, for log redaction: bot tokens, webhook
/// secrets (automatic ones derived), the admin token, pull `auth_token`s
/// and push endpoint `secret_token`s.
pub fn secrets(cfg: &Config) -> Vec<String> {
    let mut secrets = Vec::new();
    for bot in &cfg.bots {
        secrets.push(bot.token.expose().clone());
        if let Some(Ingress::Webhook(hook)) = &bot.ingress
            && let Some(secret) = webhook_secret(bot, hook)
        {
            secrets.push(secret.expose().clone());
        }
    }
    if let Some(token) = &cfg.admin.token {
        secrets.push(token.expose().clone());
    }
    for consumer in &cfg.consumers {
        match &consumer.kind {
            ConsumerKind::Pull(pull) => {
                if let Some(token) = &pull.auth_token {
                    secrets.push(token.expose().clone());
                }
            }
            ConsumerKind::Push(push) => {
                for endpoint in &push.endpoints {
                    if let Some(token) = &endpoint.secret_token {
                        secrets.push(token.expose().clone());
                    }
                }
            }
        }
    }
    secrets
}

/// The secret Telegram must send with each update of `bot`'s webhook.
pub(crate) fn webhook_secret(bot: &Bot, hook: &Webhook) -> Option<Secret<String>> {
    match &hook.secret_token {
        WebhookSecret::Auto => Some(Secret::new(tgin_botapi::webhook_secret(&bot.token))),
        WebhookSecret::None => None,
        WebhookSecret::Token(secret) => Some(secret.clone()),
    }
}

/// The supervisor's name of a bot's source, also its readiness component.
fn source_task(bot: &str) -> String {
    format!("{SOURCE_PREFIX}{bot}")
}

/// The engine's id and metrics of the bot `name`.
fn engine_source(engine: &Engine, name: &str) -> Option<(SourceId, SourceMetrics)> {
    let id = engine.source_id(name)?;
    let state = engine.state();
    let info = state.sources.get(usize::from(id.0))?.as_ref()?;
    Some((id, info.metrics.clone()))
}

/// Whether Telegram rejected the bot token, which no retry fixes.
fn token_rejected(err: &ApiError) -> bool {
    matches!(
        err,
        ApiError::Http {
            status: 401 | 403 | 404,
            ..
        }
    )
}

/// The server's `retry_after`, if it sent one.
fn retry_after(err: &ApiError) -> Option<Duration> {
    match err {
        ApiError::Http { retry_after, .. } => *retry_after,
        _ => None,
    }
}

/// After the first `getMe` of a webhook bot failed with `first`, keeps
/// calling it in the background, backing off from 1 s to 5 min, until it
/// succeeds (the username then goes to the engine), Telegram rejects the
/// token, or ingest stops.
fn learn_username(
    supervisor: &Supervisor,
    engine: &Arc<Engine>,
    ingest: &CancellationToken,
    force: &CancellationToken,
    hook: &WebhookBot<'_>,
    first: &ApiError,
) {
    if token_rejected(first) {
        return;
    }
    let (id, name) = (hook.id, Arc::<str>::from(hook.bot.name.as_str()));
    let (engine, client, ingest) = (Arc::clone(engine), hook.client.clone(), ingest.clone());
    let first_retry_after = retry_after(first);
    supervisor.spawn(
        Arc::from(format!("{IDENTITY_PREFIX}{name}")),
        Backoff::new(
            Duration::from_secs(1),
            Duration::from_secs(60),
            Duration::ZERO,
        ),
        force.clone(),
        move || {
            let (engine, client, ingest, name) = (
                Arc::clone(&engine),
                client.clone(),
                ingest.clone(),
                Arc::clone(&name),
            );
            async move {
                let (initial, max, floor) = IDENTITY_BACKOFF;
                let mut backoff = Backoff::new(initial, max, floor);
                let mut server_wait = first_retry_after;
                loop {
                    let delay = honor_retry_after(
                        backoff.next_delay(&mut rand::rng()),
                        server_wait,
                        max,
                    );
                    let sleep = tokio::time::sleep(delay);
                    if ingest.run_until_cancelled(sleep).await.is_none() {
                        return Exit::Done;
                    }
                    match ingest.run_until_cancelled(client.get_me()).await {
                        None => return Exit::Done,
                        Some(Ok(user)) => {
                            tracing::info!(source = %name, "getMe succeeded; commands addressed to this bot by @username match");
                            engine.set_username(id, user.username);
                            return Exit::Done;
                        }
                        Some(Err(err)) if token_rejected(&err) => {
                            tracing::warn!(source = %name, error = %err, "Telegram rejected the bot token; no longer calling getMe");
                            return Exit::Done;
                        }
                        Some(Err(err)) => {
                            tracing::debug!(source = %name, error = %err, "getMe failed again");
                            server_wait = retry_after(&err);
                        }
                    }
                }
            }
        },
    );
}

/// Calls `setWebhook` for `hook`. A failure is logged, not fatal: Telegram
/// may still have the webhook from an earlier run.
async fn register(hook: &WebhookBot<'_>) {
    let name = &hook.bot.name;
    let Some(public_url) = &hook.hook.public_url else {
        return;
    };
    let joined = format!(
        "{}{}",
        public_url.expose().as_str().trim_end_matches('/'),
        hook.hook.path.expose()
    );
    let Ok(url) = url::Url::parse(&joined) else {
        tracing::error!(source = %name, "the webhook URL (public_url + path) is not a valid URL; not registering");
        return;
    };
    let certificate = match &hook.hook.certificate_file {
        Some(file) => match tokio::fs::read(file.expose()).await {
            Ok(pem) => Some(pem.into()),
            Err(err) => {
                tracing::error!(source = %name, file = ?file, error = %err, "reading the webhook certificate failed; not registering");
                return;
            }
        },
        None => None,
    };
    let request = SetWebhook {
        url,
        secret_token: webhook_secret(hook.bot, hook.hook),
        allowed_updates: hook.hook.allowed_updates.clone(),
        max_connections: hook.hook.max_connections,
        drop_pending_updates: hook.hook.drop_pending_updates,
        ip_address: hook.hook.ip_address,
        certificate,
    };
    match tgin_botapi::register(&hook.client, &request).await {
        Ok(()) => {
            tracing::info!(source = %name, url = ?public_url, "webhook registered with Telegram")
        }
        Err(err) => {
            tracing::error!(source = %name, error = %err, "setWebhook failed; Telegram may not deliver updates")
        }
    }
}

/// The next signal that asks to shut down. Never returns once the signal
/// stream has ended.
async fn next_terminating(signals: &mut Signals) -> SignalKind {
    loop {
        match signals.recv().await {
            Some(kind) if kind.is_terminating() => return kind,
            Some(kind) => {
                tracing::debug!(signal = ?kind, "ignoring the signal; config reload is not available yet");
            }
            None => std::future::pending().await,
        }
    }
}

/// Returns once every supervised source has failed for good, checking each
/// second; never when `enabled` is false.
async fn all_sources_failed(supervisor: &Supervisor, enabled: bool) {
    if !enabled {
        return std::future::pending().await;
    }
    let mut ticks = tokio::time::interval(SOURCES_CHECK);
    loop {
        ticks.tick().await;
        let states = supervisor.states();
        let mut sources = states
            .iter()
            .filter(|(name, _)| name.starts_with(SOURCE_PREFIX))
            .peekable();
        if sources.peek().is_some()
            && sources.all(|(_, state)| matches!(state, TaskState::Failed { .. }))
        {
            return;
        }
    }
}
