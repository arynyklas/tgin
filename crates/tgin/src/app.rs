//! The running process: the engine, the sources that feed it, the data
//! plane, and the shutdown sequence.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use tgin_botapi::{ApiError, BotClient, BotUser, LongPollOptions, LongPollSource, SetWebhook};
use tgin_core::config::{Bot, Config, ConsumerKind, Ingress, Webhook, WebhookSecret};
use tgin_core::engine::{BuildError, Engine, EngineRuntime};
use tgin_core::ids::SourceId;
use tgin_core::metrics::{CoreMetrics, SourceMetrics, SourceStateLabel};
use tgin_runtime::backoff::Backoff;
use tgin_runtime::http::{self, ClientOptions, RunningServer, ServerOptions, TlsFiles};
use tgin_runtime::lifecycle::{Lifecycle, ShutdownTiming, SignalKind, Signals};
use tgin_runtime::obs::{Health, MetricsRegistry, Readiness};
use tgin_runtime::secret::Secret;
use tgin_runtime::supervisor::{Supervisor, TaskState};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::cli::{COMMIT, VERSION};
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

/// tgin, started: sources poll, the data plane serves, consumers deliver.
pub struct App {
    timing: ShutdownTiming,
    lifecycle: Arc<Lifecycle>,
    health: Arc<Health>,
    metrics: Arc<MetricsRegistry>,
    core: CoreMetrics,
    engine: Arc<Engine>,
    supervisor: Arc<Supervisor>,
    /// Stops the engine's consumer tasks gracefully.
    stop: CancellationToken,
    /// Taken by the shutdown, which waits for the listeners to stop.
    server: Arc<Mutex<Option<RunningServer>>>,
    data_addrs: Vec<SocketAddr>,
    /// The webhook sources' metrics, whose state follows the data plane.
    webhook_metrics: Vec<SourceMetrics>,
    /// Whether every source failing ends the process: only with long-poll
    /// sources and no webhook, which could still deliver.
    exit_when_sources_fail: bool,
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
        let lifecycle = Arc::new(Lifecycle::new());
        let stop = CancellationToken::new();
        let started = Self::assemble(cfg, Arc::clone(&lifecycle), stop.clone()).await;
        if started.is_err() {
            lifecycle.ingest_token().cancel();
            lifecycle.force_token().cancel();
            stop.cancel();
        }
        started
    }

    async fn assemble(
        cfg: Config,
        lifecycle: Arc<Lifecycle>,
        stop: CancellationToken,
    ) -> Result<Self, StartError> {
        let health = Arc::new(Health::default());
        let starting = || Readiness::NotReady("starting".to_owned());
        health.set(ENGINE, starting());
        if cfg.server.is_some() {
            health.set(DATA_PLANE, starting());
        }
        for bot in cfg.bots.iter().filter(|bot| bot.ingress.is_some()) {
            health.set(&source_task(&bot.name), starting());
        }

        let mut registry = MetricsRegistry::new(VERSION, COMMIT.unwrap_or("unknown"));
        let core = CoreMetrics::register(registry.registry_mut());
        let metrics = Arc::new(registry);
        let client = http::client(&ClientOptions::default());
        let supervisor = Arc::new(Supervisor::new());
        let force = lifecycle.force_token();
        let engine = Arc::new(Engine::new(
            &cfg,
            core.clone(),
            client.clone(),
            EngineRuntime {
                supervisor: Arc::clone(&supervisor),
                stop: stop.clone(),
                force: force.clone(),
            },
        )?);
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

        // Usernames first, so `/cmd@bot` rules match from the first update.
        for hook in &webhooks {
            match hook.client.get_me().await {
                Ok(user) => engine.set_username(hook.id, user.username),
                Err(err) => tracing::warn!(
                    source = %hook.bot.name,
                    error = %err,
                    "getMe failed; commands addressed to this bot by @username do not match"
                ),
            }
        }

        let ingest = lifecycle.ingest_token();
        let probes = Probes {
            lifecycle: Arc::clone(&lifecycle),
            health: Arc::clone(&health),
        };
        let server = match &cfg.server {
            Some(server) => {
                let routes = data_server::router(&cfg, server, &engine, &ingest, probes);
                let routes = Arc::new(ArcSwap::from_pointee(routes));
                let opts = ServerOptions {
                    listen: server.listen.clone(),
                    tls: server.tls.as_ref().map(|tls| TlsFiles {
                        cert: tls.cert_file.expose().clone(),
                        key: tls.key_file.expose().clone(),
                    }),
                    max_connections: server.max_connections,
                    header_read_timeout: server.header_read_timeout,
                };
                let running = http::serve(data_server::delegating(routes), &opts)
                    .await
                    .map_err(StartError::Serve)?;
                tracing::info!(listen = ?running.local_addrs, "data plane listening");
                health.set(DATA_PLANE, Readiness::Ready);
                Some(running)
            }
            None => None,
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
        let data_addrs = server
            .as_ref()
            .map(|s| s.local_addrs.clone())
            .unwrap_or_default();
        let server = Arc::new(Mutex::new(server));
        lifecycle.set_running();
        tracing::info!(version = VERSION, "tgin started");
        Ok(Self {
            timing: ShutdownTiming {
                lame_duck: cfg.runtime.lame_duck,
                grace: cfg.runtime.shutdown_grace,
            },
            exit_when_sources_fail: long_poll_sources > 0 && webhooks.is_empty(),
            webhook_metrics: webhooks.into_iter().map(|hook| hook.metrics).collect(),
            lifecycle,
            health,
            metrics,
            core,
            engine,
            supervisor,
            stop,
            server,
            data_addrs,
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
        Connections(Arc::clone(&self.server))
    }

    /// Runs until a terminating signal (or every source failing), then
    /// drains and shuts down. A second terminating signal forces the exit.
    pub async fn run(self, mut signals: Signals) -> Outcome {
        let cause = tokio::select! {
            kind = next_terminating(&mut signals) => Cause::Signal(kind),
            () = all_sources_failed(&self.supervisor, self.exit_when_sources_fail) => Cause::AllSourcesFailed,
        };
        let second = tokio::select! {
            outcome = self.shutdown(cause) => return outcome,
            kind = next_terminating(&mut signals) => kind,
        };
        self.abort(second).await;
        Outcome::Forced
    }

    /// The drain: readiness off, lame duck, ingest off, queues drained (or
    /// the grace spent), listeners and tasks closed.
    async fn shutdown(&self, cause: Cause) -> Outcome {
        let (timing, outcome) = match cause {
            Cause::Signal(kind) => (self.timing.for_signal(kind), Outcome::Clean),
            Cause::AllSourcesFailed => (self.timing, Outcome::AllSourcesFailed),
        };
        let serving = !self.data_addrs.is_empty();
        self.lifecycle.begin_drain();
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
        self.lifecycle.ingest_token().cancel();
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

        self.stop.cancel();
        let deadline = Instant::now() + CLOSE_GRACE;
        let server = self.server.lock().take();
        if let Some(server) = server {
            server.graceful_shutdown(CLOSE_GRACE);
            if tokio::time::timeout_at(deadline, server.wait())
                .await
                .is_err()
            {
                tracing::warn!("the data plane did not close within its grace");
            }
        }
        let aborted = self.supervisor.shutdown(deadline).await;
        self.lifecycle.force_token().cancel();
        if aborted > 0 {
            tracing::warn!(
                aborted,
                "tasks still running at the shutdown deadline were aborted"
            );
        }
        self.finish();
        outcome
    }

    /// The second signal: stop everything now.
    async fn abort(&self, kind: SignalKind) {
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
        let abandoned = self.engine.pending();
        self.core
            .shutdown_abandoned(u64::try_from(abandoned).unwrap_or(u64::MAX));
        if abandoned > 0 {
            tracing::warn!(abandoned, "exiting with updates not delivered");
        } else {
            tracing::info!(abandoned, "shutdown complete");
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
        if let Some(Ingress::Webhook(hook)) = &bot.ingress {
            if let Some(secret) = webhook_secret(bot, hook) {
                secrets.push(secret.expose().clone());
            }
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
