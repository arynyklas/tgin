//! Hot reload: one task serializes every trigger (SIGHUP, `POST /v1/reload`
//! and the config watcher), re-reads the config file, applies it to the
//! engine and starts, stops and restarts the sources the change touches.
//! The same task moves bots off their Bot API server
//! (`POST /v1/bots/{name}/logout|close`), so a reload never restarts a bot
//! halfway through its move.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tgin_botapi::ApiError;
use tgin_core::config::{self, Bot, Config};
use tgin_core::metrics::ReloadOutcome;
use tgin_core::reload::ReloadReport;
use tgin_runtime::config::{ConfigError, ConfigWatcher, Env, watch_config};
use tgin_runtime::http::{ServerTls, TlsFiles};
use tgin_runtime::lifecycle::{Lifecycle, Phase};
use tgin_runtime::obs::LogHandle;
use tgin_runtime::secret::Redactor;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::admin::{BotMove, Moved};
use crate::app::{self, Shared};
use crate::cli;
use crate::data_server::DataRoutes;
use crate::sources::Kind;

/// Reload requests that may wait while one runs; a SIGHUP finding the queue
/// full is covered by a request already waiting.
const QUEUE: usize = 8;
/// How often the watcher re-reads the config file, for file systems that
/// deliver no change events.
const WATCH_POLL: Duration = Duration::from_secs(2);
/// How often a move looks whether tgin still holds updates of the bot.
const MOVE_CHECK: Duration = Duration::from_millis(50);

/// What asked for a reload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    Signal,
    Api,
    Watch,
}

/// One job for the reload task, with where its result goes.
pub(crate) enum Request {
    Reload {
        trigger: Trigger,
        reply: Option<oneshot::Sender<Result<ReloadReport, ReloadError>>>,
    },
    /// Moves the bot `bot` off its Bot API server; see
    /// [`Reloader::move_bot`].
    Move {
        bot: String,
        how: BotMove,
        timeout: Option<Duration>,
        reply: oneshot::Sender<Result<Moved, MoveError>>,
    },
}

/// Why a reload changed nothing.
#[derive(Debug)]
pub(crate) enum ReloadError {
    /// tgin is shutting down.
    Draining,
    /// The config file cannot be read, is invalid or cannot be applied.
    Rejected(Vec<ConfigError>),
}

/// Why a bot was not moved.
#[derive(Debug)]
pub(crate) enum MoveError {
    /// tgin is shutting down.
    Draining,
    /// No bot of the running config has the name.
    UnknownBot,
    /// The bot is parked already.
    Parked,
    /// The Bot API server still held updates of the bot at the deadline;
    /// the bot runs on unchanged.
    Undrained,
    /// The Bot API server refused or failed the call (token masked); the
    /// bot runs on.
    Upstream(ApiError),
}

/// Where reload requests go.
#[derive(Clone)]
pub(crate) struct ReloadQueue {
    tx: mpsc::Sender<Request>,
    lifecycle: Arc<Lifecycle>,
}

impl ReloadQueue {
    /// A queue and the receiver [`Reloader::run`] takes requests from.
    pub(crate) fn new(lifecycle: Arc<Lifecycle>) -> (Self, mpsc::Receiver<Request>) {
        let (tx, rx) = mpsc::channel(QUEUE);
        (Self { tx, lifecycle }, rx)
    }

    /// SIGHUP: queues a reload, unless tgin is shutting down.
    pub(crate) fn signal(&self) {
        if self.lifecycle.phase() >= Phase::Draining {
            tracing::debug!("SIGHUP ignored: shutting down");
            return;
        }
        let request = Request::Reload {
            trigger: Trigger::Signal,
            reply: None,
        };
        match self.tx.try_send(request) {
            Ok(()) => tracing::info!("SIGHUP: reloading the config"),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!("SIGHUP: a reload is already waiting");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!("SIGHUP ignored: reloads have stopped");
            }
        }
    }

    /// `POST /v1/reload`: reloads and returns the result, after the reloads
    /// already waiting.
    pub(crate) async fn request(&self) -> Result<ReloadReport, ReloadError> {
        let (reply, result) = oneshot::channel();
        let request = Request::Reload {
            trigger: Trigger::Api,
            reply: Some(reply),
        };
        if self.tx.send(request).await.is_err() {
            return Err(ReloadError::Draining);
        }
        // Dropped unanswered when the shutdown stopped the reloads.
        result.await.unwrap_or(Err(ReloadError::Draining))
    }

    /// `POST /v1/bots/{bot}/logout|close`: moves the bot (see
    /// [`Reloader::move_bot`]) and returns the result, after the reloads
    /// already waiting.
    pub(crate) async fn move_bot(
        &self,
        bot: String,
        how: BotMove,
        timeout: Option<Duration>,
    ) -> Result<Moved, MoveError> {
        let (reply, result) = oneshot::channel();
        let request = Request::Move {
            bot,
            how,
            timeout,
            reply,
        };
        if self.tx.send(request).await.is_err() {
            return Err(MoveError::Draining);
        }
        result.await.unwrap_or(Err(MoveError::Draining))
    }
}

/// The config file and what reloading it touches.
pub(crate) struct Reloader {
    pub(crate) shared: Arc<Shared>,
    /// The data plane's routes; `None` without a data plane.
    pub(crate) routes: Option<Arc<DataRoutes>>,
    /// The data plane's certificate, with the bytes of the certificate and
    /// key files it was loaded from; `None` without TLS.
    pub(crate) tls: Option<(ServerTls, TlsBytes)>,
    pub(crate) path: PathBuf,
    pub(crate) env: Arc<dyn Env>,
    /// The bytes the running config was loaded from.
    pub(crate) loaded: Vec<u8>,
    /// Every secret of every config loaded so far: retired components may
    /// still log with old secrets while they drain, so none is forgotten.
    pub(crate) secrets: BTreeSet<String>,
    /// Sets the log level of the process's subscriber; `None` when the app
    /// runs without one (in tests).
    pub(crate) log: Option<LogHandle>,
}

/// The bytes of a certificate file and its key file.
pub(crate) type TlsBytes = (Vec<u8>, Vec<u8>);

/// A watch on the config file.
struct Watch {
    _watcher: ConfigWatcher,
    changes: mpsc::Receiver<()>,
    debounce: Duration,
}

impl Reloader {
    /// Runs the reloads and moves asked through `requests`, and the
    /// reloads the config watcher asks for while the config enables it, one
    /// at a time, until `stop` is cancelled.
    pub(crate) async fn run(
        mut self,
        mut requests: mpsc::Receiver<Request>,
        stop: CancellationToken,
    ) {
        let mut watch = None;
        self.settle(&mut watch).await;
        loop {
            let request = tokio::select! {
                biased;
                () = stop.cancelled() => return,
                request = requests.recv() => match request {
                    Some(request) => request,
                    None => return,
                },
                changed = next_change(&mut watch) => {
                    if !changed {
                        tracing::warn!("the config watcher stopped; reload with SIGHUP or POST /v1/reload");
                        watch = None;
                        continue;
                    }
                    Request::Reload { trigger: Trigger::Watch, reply: None }
                }
            };
            match request {
                Request::Reload { trigger, reply } => {
                    let result = self.reload(trigger).await;
                    let applied = result.is_ok();
                    if let Some(reply) = reply {
                        let _ = reply.send(result);
                    }
                    if applied {
                        self.settle(&mut watch).await;
                    }
                }
                Request::Move {
                    bot,
                    how,
                    timeout,
                    reply,
                } => {
                    let _ = reply.send(self.move_bot(&bot, how, timeout).await);
                }
            }
        }
    }

    /// Watches the config file as the running config asks, and reloads it
    /// when it changed before the watch began.
    async fn settle(&mut self, watch: &mut Option<Watch>) {
        while self.rewatch(watch).await {
            if self.reload(Trigger::Watch).await.is_err() {
                return;
            }
        }
    }

    /// Starts, stops or restarts the watcher to match the running config.
    /// Returns whether a new watch began on a file whose bytes differ from
    /// those the running config was loaded from: a change made before the
    /// watch began, which it does not report.
    async fn rewatch(&self, watch: &mut Option<Watch>) -> bool {
        let reload = &self.shared.config.load().reload;
        let wanted = reload.watch.then_some(reload.debounce);
        if watch.as_ref().map(|w| w.debounce) == wanted {
            return false;
        }
        *watch = None;
        let Some(debounce) = wanted else {
            return false;
        };
        match watch_config(&self.path, debounce, WATCH_POLL) {
            Ok((watcher, changes, events)) => {
                *watch = Some(Watch {
                    _watcher: watcher,
                    changes,
                    debounce,
                });
                match events {
                    None => tracing::info!("watching the config file for changes"),
                    Some(err) => tracing::warn!(
                        "file events unavailable ({err}); polling every {} s",
                        WATCH_POLL.as_secs()
                    ),
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "the config file cannot be watched; reload with SIGHUP or POST /v1/reload");
                return false;
            }
        }
        tokio::fs::read(&self.path)
            .await
            .is_ok_and(|bytes| bytes != self.loaded)
    }

    /// Re-reads the config file and applies it: secrets, engine, sources,
    /// routes, certificate. A config that cannot be read, loaded or
    /// applied changes nothing.
    async fn reload(&mut self, trigger: Trigger) -> Result<ReloadReport, ReloadError> {
        if self.shared.lifecycle.phase() >= Phase::Draining {
            tracing::debug!(?trigger, "reload ignored: shutting down");
            return Err(ReloadError::Draining);
        }
        let text = match tokio::fs::read(&self.path).await {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|_| ConfigError::new("the config file is not valid UTF-8")),
            Err(err) => Err(ConfigError::new(format!(
                "cannot read the config file {}: {err}",
                self.path.display()
            ))),
        };
        let cfg = match text {
            Ok(text) => config::load(&text, &self.path, &*self.env).map(|cfg| (cfg, text)),
            Err(err) => Err(vec![err]),
        };
        let (cfg, text) = match cfg {
            Ok(loaded) => loaded,
            Err(errors) => return Err(self.reject(trigger, errors)),
        };
        // Before the engine changes: what starts now may log its secrets.
        self.learn_secrets(&cfg);
        let report = match self.shared.engine.apply(&cfg) {
            Ok(report) => report,
            Err(err) => {
                let error = ConfigError::new(format!("the config cannot be applied: {err}"));
                return Err(self.reject(trigger, vec![error]));
            }
        };
        // First, so the rest of this reload logs at the new level.
        self.set_level(&cfg);
        for warning in &cfg.warnings {
            tracing::warn!("{warning}");
        }
        let cfg = Arc::new(cfg);
        let previous = self.shared.config.swap(Arc::clone(&cfg));
        if self.shared.admin_exposed && previous.admin.token.is_some() && cfg.admin.token.is_none()
        {
            tracing::warn!(
                "admin.token removed while the admin plane listens on a non-loopback address; protected endpoints refuse every request until restart"
            );
        }
        self.loaded = text.into_bytes();

        // Before the sources restart: their `getMe` records identities in
        // the gateway's new entries.
        self.shared.gateway.set_bots(
            app::gateway_bots(&cfg, &self.shared.engine),
            app::trusted_proxies(&cfg),
        );
        self.restart_sources(&cfg, &report).await;
        self.reload_tls(&cfg).await;
        if !report.restart_required.is_empty() {
            tracing::warn!(
                settings = ?report.restart_required,
                "these changed settings take effect only after a restart"
            );
        }
        tracing::debug!(?report, "reload report");
        tracing::info!(
            ?trigger,
            generation = report.generation,
            sources_added = report.sources_added.len(),
            sources_removed = report.sources_removed.len(),
            sources_restarted = report.sources_restarted.len(),
            "config reloaded"
        );
        Ok(report)
    }

    /// Applies `cfg`'s log level when it differs from the running one.
    fn set_level(&self, cfg: &Config) {
        let Some(log) = &self.log else {
            return;
        };
        if cfg.log.level == self.shared.config.load().log.level {
            return;
        }
        if let Err(err) = log.set_level(cli::level_name(cfg.log.level)) {
            tracing::warn!(error = %err, "the new log level could not be applied");
        }
    }

    /// Counts and logs a rejected reload, in one line.
    fn reject(&self, trigger: Trigger, errors: Vec<ConfigError>) -> ReloadError {
        self.shared.core.reload(ReloadOutcome::Rejected);
        let listed: Vec<String> = errors.iter().map(one_line).collect();
        tracing::warn!(
            ?trigger,
            errors = errors.len(),
            details = %listed.join("; "),
            "config reload rejected; the running config is unchanged"
        );
        ReloadError::Rejected(errors)
    }

    /// Adds `cfg`'s secrets to those the logs mask.
    fn learn_secrets(&mut self, cfg: &Config) {
        let before = self.secrets.len();
        self.secrets.extend(app::secrets(cfg));
        if self.secrets.len() != before {
            let redactor = Redactor::new(self.secrets.iter().cloned());
            self.shared.redactor.store(Arc::new(redactor));
        }
    }

    /// Stops the removed sources, restarts the changed ones, starts the new
    /// ones, and serves the new routes. A parked bot stays parked, with a
    /// warning, unless the reload changed its upstream: it then starts on
    /// the new one.
    async fn restart_sources(&self, cfg: &Config, report: &ReloadReport) {
        let sources = &self.shared.sources;
        for name in &report.sources_removed {
            sources.stop(name, true);
        }
        let mut starting = Vec::new();
        for name in &report.sources_restarted {
            let bot = cfg.bots.iter().find(|bot| &bot.name == name);
            // A bot whose ingress went away is gone as a source.
            let gone = bot.is_none_or(|bot| bot.ingress.is_none());
            let moved = report.upstream_changed.contains(name);
            if !gone && !moved && sources.is_parked(name) {
                continue;
            }
            if moved {
                sources.unpark(name);
            }
            sources.stop(name, gone);
            if let Some(bot) = bot.filter(|_| !gone) {
                starting.push(bot);
            }
        }
        starting.extend(
            report
                .sources_added
                .iter()
                .filter_map(|name| cfg.bots.iter().find(|bot| &bot.name == name))
                .filter(|bot| bot.ingress.is_some()),
        );
        self.start_sources(cfg, starting).await;
        for name in sources.parked() {
            tracing::warn!(bot = %name, "the bot stays parked: it was logged out of its Bot API server, and this reload left its upstream unchanged; a reload that changes its upstream starts it there");
        }
    }

    /// Starts the sources of `bots`, which have ingress, and serves the
    /// routes of `cfg`.
    async fn start_sources(&self, cfg: &Config, bots: Vec<&Bot>) {
        let shared = &self.shared;
        let sources = &shared.sources;
        let mut prepared = Vec::new();
        for bot in bots {
            match sources.prepare(bot) {
                Ok(bot) => prepared.push(bot),
                Err(err) => {
                    tracing::error!(source = %bot.name, error = %err, "the source cannot start");
                }
            }
        }
        // The routes change at once, so a removed webhook stops accepting
        // while new bots' `getMe` calls may wait on Telegram. Telegram sends
        // to a registered webhook only after `setWebhook`, which comes after
        // the username is known.
        if let (Some(routes), Some(server)) = (&self.routes, &cfg.server) {
            routes.rebuild(cfg, server, &shared.engine, &|bot| sources.ingest_of(bot));
        }
        sources.identify(&prepared).await;
        let (long_polls, webhooks): (Vec<_>, Vec<_>) = prepared
            .into_iter()
            .partition(|bot| bot.kind() == Some(Kind::LongPoll));
        sources.start_webhooks(webhooks).await;
        for bot in long_polls {
            sources.start_long_poll(bot);
        }
    }

    /// Moves the bot `name` off its Bot API server, `how` says with which
    /// call, in these steps; `timeout` (`reload.drain_timeout` when `None`)
    /// bounds steps 1 to 3 together:
    /// 1. the server runs dry of the bot's updates, its ingest running on
    ///    ([`crate::sources::Sources::drain_upstream`]); not done by the
    ///    deadline, the move fails ([`MoveError::Undrained`]) and changes
    ///    nothing;
    /// 2. the bot's ingest stops, and it is parked;
    /// 3. tgin delivers what it holds of the bot, until the deadline;
    /// 4. the server is called: `logOut`, or `deleteWebhook` then `close`.
    ///    When that fails, the bot is unparked and starts again.
    ///
    /// During the shutdown nothing is called on the server.
    async fn move_bot(
        &self,
        name: &str,
        how: BotMove,
        timeout: Option<Duration>,
    ) -> Result<Moved, MoveError> {
        let shared = &self.shared;
        if shared.lifecycle.phase() >= Phase::Draining {
            return Err(MoveError::Draining);
        }
        let cfg = shared.config.load_full();
        let bot = (cfg.bots.iter().find(|bot| bot.name == name)).ok_or(MoveError::UnknownBot)?;
        let sources = &shared.sources;
        if sources.is_parked(name) {
            return Err(MoveError::Parked);
        }
        let id = shared.engine.source_id(name).ok_or(MoveError::UnknownBot)?;
        let client = sources.client(bot).map_err(MoveError::Upstream)?;
        let timeout = timeout.unwrap_or(cfg.reload.drain_timeout);
        let started = Instant::now();
        let ingest = shared.lifecycle.ingest_token();

        let drain = tokio::time::timeout(timeout, sources.drain_upstream(bot, &client));
        match ingest.run_until_cancelled(drain).await {
            None => return Err(MoveError::Draining),
            Some(Err(_)) => {
                tracing::warn!(bot = %name, ?timeout, "the Bot API server still held updates of the bot at the deadline; the bot was not moved");
                return Err(MoveError::Undrained);
            }
            Some(Ok(())) => {}
        }
        sources.park(name);
        let engine = &shared.engine;
        let delivered = tokio::time::timeout(timeout.saturating_sub(started.elapsed()), async {
            while engine.pending_from(id) > 0 {
                tokio::time::sleep(MOVE_CHECK).await;
            }
        });
        if ingest.run_until_cancelled(delivered).await.is_none() {
            return Err(MoveError::Draining);
        }
        let left = engine.pending_from(id);

        let called = match how {
            BotMove::LogOut => client.log_out().await,
            BotMove::Close => match client.delete_webhook(false).await {
                Ok(()) => client.close().await,
                Err(err) => Err(err),
            },
        };
        match called {
            Ok(result) => {
                tracing::info!(bot = %name, method = how.method(), upstream = %bot.upstream.name, left, "the bot was logged out of its Bot API server and stays parked; a reload that changes its upstream starts it there");
                Ok(Moved {
                    bot: name.to_owned(),
                    left,
                    upstream: bot.upstream.name.to_string(),
                    result,
                })
            }
            Err(err) => {
                tracing::warn!(bot = %name, method = how.method(), error = %err, "the Bot API server did not log the bot out; it starts again");
                sources.unpark(name);
                let restart = bot.ingress.is_some().then_some(bot);
                self.start_sources(&cfg, restart.into_iter().collect())
                    .await;
                Err(MoveError::Upstream(err))
            }
        }
    }

    /// Reloads the data plane's certificate when the bytes of its
    /// certificate or key file changed; a certificate that does not load
    /// leaves the old one in use. TLS turned on or off is in the report's
    /// `restart_required`.
    async fn reload_tls(&mut self, cfg: &Config) {
        let configured = cfg.server.as_ref().and_then(|server| server.tls.as_ref());
        let (Some((tls, loaded)), Some(files)) = (&mut self.tls, configured) else {
            return;
        };
        let files = TlsFiles {
            cert: files.cert_file.expose().clone(),
            key: files.key_file.expose().clone(),
        };
        let bytes = match read_tls(&files).await {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(error = %err, "the TLS certificate or key cannot be read; keeping the certificate in use");
                return;
            }
        };
        if bytes == *loaded {
            return;
        }
        match tls.reload(&files).await {
            Ok(()) => {
                *loaded = bytes;
                tracing::info!("TLS certificate reloaded");
            }
            Err(err) => {
                tracing::warn!(error = %err, "the new TLS certificate or key does not load; keeping the certificate in use");
            }
        }
    }
}

/// The next change the watcher reports: `true`, or `false` once it has
/// stopped; never without a watch.
async fn next_change(watch: &mut Option<Watch>) -> bool {
    match watch {
        Some(watch) => watch.changes.recv().await.is_some(),
        None => std::future::pending().await,
    }
}

/// The bytes of `files`.
pub(crate) async fn read_tls(files: &TlsFiles) -> std::io::Result<TlsBytes> {
    Ok((
        tokio::fs::read(&files.cert).await?,
        tokio::fs::read(&files.key).await?,
    ))
}

/// `err` on one line: `path (line L, column C): message (hint: …)`.
fn one_line(err: &ConfigError) -> String {
    let mut line = match &err.at {
        Some(at) => format!("{} (line {}, column {}): ", at.path, at.line, at.column),
        None => String::new(),
    };
    line.push_str(&err.message);
    if let Some(hint) = &err.hint {
        line.push_str(" (hint: ");
        line.push_str(hint);
        line.push(')');
    }
    line
}
