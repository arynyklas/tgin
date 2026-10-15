//! The bots' sources: a supervised long-poll task per long-poll bot, and
//! for each webhook bot its identity and its registration with Telegram.
//! Sources start with the app and as reloads add them, and each one stops
//! on its own token, a child of the process ingest token.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tgin_botapi::{ApiError, BotClient, BotUser, LongPollOptions, LongPollSource, SetWebhook};
use tgin_core::config::{Bot, Ingress, LongPoll, Webhook};
use tgin_core::engine::Engine;
use tgin_core::ids::SourceId;
use tgin_core::metrics::{SourceMetrics, SourceStateLabel};
use tgin_runtime::backoff::{Backoff, honor_retry_after};
use tgin_runtime::obs::{Health, Readiness};
use tgin_runtime::supervisor::{Exit, Supervisor, TaskState};
use tokio_util::sync::CancellationToken;

use crate::app::{StartError, webhook_secret};

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
/// How long a removed source's task may take to end before its entry is
/// left to the shutdown.
const REAP_WAIT: Duration = Duration::from_secs(60);

/// The supervisor's name of a bot's source, also its readiness component.
pub(crate) fn source_task(bot: &str) -> String {
    format!("{SOURCE_PREFIX}{bot}")
}

fn identity_task(bot: &str) -> String {
    format!("{IDENTITY_PREFIX}{bot}")
}

/// How a bot receives its updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    LongPoll,
    Webhook,
}

/// The bots' sources, by bot name.
pub(crate) struct Sources {
    engine: Arc<Engine>,
    supervisor: Arc<Supervisor>,
    health: Arc<Health>,
    client: reqwest::Client,
    ingest: CancellationToken,
    force: CancellationToken,
    registry: Arc<Mutex<Registry>>,
}

/// The sources that run, and the bots whose source is on its way up.
#[derive(Default)]
struct Registry {
    started: BTreeMap<String, Started>,
    /// Bots between their stop (a restart) or their preparation and the end
    /// of their start: neither running nor failed.
    starting: BTreeSet<String>,
}

/// A source that runs.
struct Started {
    kind: Kind,
    /// Stops this source and its background `getMe`; a child of ingest.
    stop: CancellationToken,
    metrics: SourceMetrics,
}

/// A bot whose source is about to start.
pub(crate) struct Prepared {
    id: SourceId,
    name: Arc<str>,
    bot: Bot,
    client: BotClient,
    metrics: SourceMetrics,
    stop: CancellationToken,
}

impl Prepared {
    pub(crate) fn kind(&self) -> Option<Kind> {
        match self.bot.ingress {
            Some(Ingress::LongPoll(_)) => Some(Kind::LongPoll),
            Some(Ingress::Webhook(_)) => Some(Kind::Webhook),
            None => None,
        }
    }

    fn webhook(&self) -> Option<&Webhook> {
        match &self.bot.ingress {
            Some(Ingress::Webhook(hook)) => Some(hook),
            _ => None,
        }
    }
}

impl Sources {
    /// Sources fed to `engine`, supervised by `supervisor`, calling the
    /// Bot API with `client`. Every source stops when `ingest` is cancelled;
    /// `force` aborts their tasks.
    pub(crate) fn new(
        engine: Arc<Engine>,
        supervisor: Arc<Supervisor>,
        health: Arc<Health>,
        client: reqwest::Client,
        ingest: CancellationToken,
        force: CancellationToken,
    ) -> Self {
        Self {
            engine,
            supervisor,
            health,
            client,
            ingest,
            force,
            registry: Arc::default(),
        }
    }

    /// Readies `bot`'s source to start, with the engine's id of the bot,
    /// its API client and a stop token of its own, and reports it not ready
    /// (and starting, never failed) until it starts.
    ///
    /// # Errors
    ///
    /// The engine has no source for the bot, or its API client cannot be
    /// set up; the bot is no longer starting then.
    pub(crate) fn prepare(&self, bot: &Bot) -> Result<Prepared, StartError> {
        self.registry.lock().starting.insert(bot.name.clone());
        self.health.set(
            &source_task(&bot.name),
            Readiness::NotReady("starting".to_owned()),
        );
        let prepared = self.prepared(bot);
        if prepared.is_err() {
            self.registry.lock().starting.remove(&bot.name);
        }
        prepared
    }

    fn prepared(&self, bot: &Bot) -> Result<Prepared, StartError> {
        let id = self
            .engine
            .source_id(&bot.name)
            .ok_or_else(|| StartError::NoSource(bot.name.clone()))?;
        let metrics = {
            let state = self.engine.state();
            let info = state
                .sources
                .get(usize::from(id.0))
                .and_then(Option::as_ref)
                .ok_or_else(|| StartError::NoSource(bot.name.clone()))?;
            info.metrics.clone()
        };
        let client = BotClient::new(self.client.clone(), bot.api_url.expose(), &bot.token)
            .map_err(|source| StartError::Bot {
                bot: bot.name.clone(),
                source,
            })?;
        Ok(Prepared {
            id,
            name: Arc::from(bot.name.as_str()),
            bot: bot.clone(),
            client,
            metrics,
            stop: self.ingest.child_token(),
        })
    }

    /// Asks Telegram for each webhook bot's username, all at once (each
    /// call may wait out the client's timeout), so that `/cmd@bot` rules
    /// match from the first update. A bot whose call fails keeps trying in
    /// the background. Long-poll sources learn theirs as they start.
    pub(crate) async fn identify(&self, bots: &[Prepared]) {
        let webhooks: Vec<&Prepared> = bots
            .iter()
            .filter(|bot| bot.kind() == Some(Kind::Webhook))
            .collect();
        let calls = webhooks
            .iter()
            .map(|bot| bot.stop.run_until_cancelled(bot.client.get_me()));
        let identities = futures_util::future::join_all(calls).await;
        for (bot, identity) in webhooks.into_iter().zip(identities) {
            match identity {
                None => {}
                Some(Ok(user)) => self.engine.set_username(bot.id, user.username),
                Some(Err(err)) => {
                    tracing::warn!(
                        source = %bot.name,
                        error = %err,
                        "getMe failed; commands addressed to this bot by @username do not match until it succeeds"
                    );
                    self.learn_username(bot, &err);
                }
            }
        }
    }

    /// Starts the webhook bots of `bots`, whose routes the data plane
    /// serves by now: each is registered with Telegram when its config asks
    /// (all at once), then reported ready.
    pub(crate) async fn start_webhooks(&self, bots: Vec<Prepared>) {
        let webhooks: Vec<Prepared> = bots
            .into_iter()
            .filter(|bot| bot.kind() == Some(Kind::Webhook))
            .collect();
        for bot in &webhooks {
            bot.metrics.set_state(SourceStateLabel::Running);
            let mut registry = self.registry.lock();
            registry.started.insert(
                bot.name.to_string(),
                Started {
                    kind: Kind::Webhook,
                    stop: bot.stop.clone(),
                    metrics: bot.metrics.clone(),
                },
            );
            registry.starting.remove(&*bot.name);
        }
        let registrations = webhooks.iter().filter_map(|bot| {
            let hook = bot.webhook()?;
            if !hook.register {
                tracing::info!(source = %bot.name, "webhook served; not registered with Telegram (register: false)");
                return None;
            }
            Some(bot.stop.run_until_cancelled(register(bot, hook)))
        });
        futures_util::future::join_all(registrations).await;
        for bot in &webhooks {
            self.health.set(&source_task(&bot.name), Readiness::Ready);
        }
    }

    /// Starts `bot`'s long poll as the supervised task `source:<bot>`; it
    /// polls until its stop token is cancelled.
    pub(crate) fn start_long_poll(&self, bot: Prepared) {
        let Some(Ingress::LongPoll(poll)) = &bot.bot.ingress else {
            return;
        };
        let Prepared {
            id,
            name,
            client,
            metrics,
            stop,
            ..
        } = &bot;
        let identity_engine = Arc::clone(&self.engine);
        let source = Arc::new(LongPollSource::new(
            *id,
            Arc::clone(name),
            client.clone(),
            long_poll_options(poll),
            Arc::clone(&self.engine),
            metrics.clone(),
            Arc::new(move |id: SourceId, user: &BotUser| {
                identity_engine.set_username(id, user.username.clone());
            }),
        ));
        self.registry.lock().started.insert(
            name.to_string(),
            Started {
                kind: Kind::LongPoll,
                stop: stop.clone(),
                metrics: metrics.clone(),
            },
        );
        let task_stop = stop.clone();
        // The supervisor's token aborts; the source stops on its own token
        // by itself.
        self.supervisor.spawn(
            Arc::from(source_task(name)),
            Backoff::new(
                Duration::from_secs(1),
                Duration::from_secs(60),
                Duration::ZERO,
            ),
            self.force.clone(),
            move || {
                let source = Arc::clone(&source);
                let stop = task_stop.clone();
                async move { source.run(stop).await }
            },
        );
        // Only now: until the new task is supervised, the supervisor may
        // still list the instance this one replaces, failed.
        self.registry.lock().starting.remove(&**name);
        self.health.set(&source_task(name), Readiness::Ready);
    }

    /// Stops the source of the bot `name`, if it runs. A bot that is not
    /// `removed` restarts: it counts as starting until it has started
    /// again. A `removed` bot is forgotten too: its readiness component
    /// goes. The tasks of the stopped source leave the supervisor's list
    /// once they have ended, unless the bot polls again by then.
    pub(crate) fn stop(&self, name: &str, removed: bool) {
        let started = {
            let mut registry = self.registry.lock();
            if !removed {
                registry.starting.insert(name.to_owned());
            }
            registry.started.remove(name)
        };
        if removed {
            self.health.remove(&source_task(name));
        }
        let Some(started) = started else {
            return;
        };
        started.stop.cancel();
        if started.kind == Kind::Webhook {
            started.metrics.set_state(SourceStateLabel::Stopped);
        }
        // An ended background `getMe` is of no use, also for a bot that
        // starts again.
        self.reap(identity_task(name), None);
        if started.kind == Kind::LongPoll {
            self.reap(source_task(name), Some(name.to_owned()));
        }
    }

    /// Forgets the supervised task `task` once its current instance has
    /// ended, unless the bot `unless_polling` polls again by then: its new
    /// long poll then owns the name. (An entry removed while the bot is
    /// still starting is simply created again by its new long poll.)
    fn reap(&self, task: String, unless_polling: Option<String>) {
        let Some(mut state) = self.supervisor.subscribe(&task) else {
            return;
        };
        let supervisor = Arc::clone(&self.supervisor);
        let registry = Arc::clone(&self.registry);
        tokio::spawn(async move {
            let ended = state
                .wait_for(|state| matches!(state, TaskState::Stopped | TaskState::Failed { .. }));
            if tokio::time::timeout(REAP_WAIT, ended).await.is_err() {
                return;
            }
            let polling = |bot: &String| {
                registry
                    .lock()
                    .started
                    .get(bot)
                    .is_some_and(|s| s.kind == Kind::LongPoll)
            };
            if !unless_polling.as_ref().is_some_and(polling) {
                supervisor.remove(&task);
            }
        });
    }

    /// How the bot `name` receives updates, if its source runs.
    pub(crate) fn kind(&self, name: &str) -> Option<Kind> {
        self.registry
            .lock()
            .started
            .get(name)
            .map(|started| started.kind)
    }

    /// Whether the source of the bot `name` is on its way up.
    pub(crate) fn is_starting(&self, name: &str) -> bool {
        self.registry.lock().starting.contains(name)
    }

    /// Whether ingest has stopped, so that no source runs any more.
    pub(crate) fn ingest_stopped(&self) -> bool {
        self.ingest.is_cancelled()
    }

    /// Reports the webhook sources stopped, once ingest has stopped.
    pub(crate) fn webhooks_stopped(&self) {
        for started in self.registry.lock().started.values() {
            if started.kind == Kind::Webhook {
                started.metrics.set_state(SourceStateLabel::Stopped);
            }
        }
    }

    /// Whether every source has failed for good. Only long-poll sources
    /// fail, so this holds only when every source is one, at least one
    /// runs, no webhook could still deliver updates and no source is on its
    /// way up (a reload restarting or adding one).
    pub(crate) fn all_failed(&self) -> bool {
        // Held while the states are read, so a restart cannot slip between.
        let registry = self.registry.lock();
        if !registry.starting.is_empty()
            || registry.started.is_empty()
            || registry.started.values().any(|s| s.kind == Kind::Webhook)
        {
            return false;
        }
        let states = self.supervisor.states();
        registry.started.keys().all(|bot| {
            let task = source_task(bot);
            states
                .iter()
                .any(|(name, state)| **name == *task && matches!(state, TaskState::Failed { .. }))
        })
    }

    /// After the first `getMe` of the webhook bot `bot` failed with
    /// `first`, keeps calling it in the background, backing off from 1 s to
    /// 5 min, until it succeeds (the username then goes to the engine),
    /// Telegram rejects the token, or the bot stops.
    fn learn_username(&self, bot: &Prepared, first: &ApiError) {
        if tgin_botapi::is_fatal(first) {
            return;
        }
        let (id, name) = (bot.id, Arc::clone(&bot.name));
        let (engine, client, stop) = (
            Arc::clone(&self.engine),
            bot.client.clone(),
            bot.stop.clone(),
        );
        let first_retry_after = retry_after(first);
        self.supervisor.spawn(
            Arc::from(identity_task(&name)),
            Backoff::new(
                Duration::from_secs(1),
                Duration::from_secs(60),
                Duration::ZERO,
            ),
            self.force.clone(),
            move || {
                let (engine, client, stop, name) = (
                    Arc::clone(&engine),
                    client.clone(),
                    stop.clone(),
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
                        if stop.run_until_cancelled(sleep).await.is_none() {
                            return Exit::Done;
                        }
                        match stop.run_until_cancelled(client.get_me()).await {
                            None => return Exit::Done,
                            Some(Ok(user)) => {
                                tracing::info!(source = %name, "getMe succeeded; commands addressed to this bot by @username match");
                                engine.set_username(id, user.username);
                                return Exit::Done;
                            }
                            Some(Err(err)) if tgin_botapi::is_fatal(&err) => {
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
}

fn long_poll_options(poll: &LongPoll) -> LongPollOptions {
    LongPollOptions {
        timeout: poll.timeout,
        limit: poll.limit,
        allowed_updates: poll.allowed_updates.clone(),
        delete_webhook_on_start: poll.delete_webhook_on_start,
        ..LongPollOptions::default()
    }
}

/// The server's `retry_after`, if it sent one.
fn retry_after(err: &ApiError) -> Option<Duration> {
    match err {
        ApiError::Http { retry_after, .. } => *retry_after,
        _ => None,
    }
}

/// Calls `setWebhook` for `bot`. A failure is logged, not fatal: Telegram
/// may still have the webhook from an earlier run.
async fn register(bot: &Prepared, hook: &Webhook) {
    let name = &bot.name;
    let Some(public_url) = &hook.public_url else {
        return;
    };
    let joined = format!(
        "{}{}",
        public_url.expose().as_str().trim_end_matches('/'),
        hook.path.expose()
    );
    let Ok(url) = url::Url::parse(&joined) else {
        tracing::error!(source = %name, "the webhook URL (public_url + path) is not a valid URL; not registering");
        return;
    };
    let certificate = match &hook.certificate_file {
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
        secret_token: webhook_secret(&bot.bot, hook),
        allowed_updates: hook.allowed_updates.clone(),
        max_connections: hook.max_connections,
        drop_pending_updates: hook.drop_pending_updates,
        ip_address: hook.ip_address,
        certificate,
    };
    match tgin_botapi::register(&bot.client, &request).await {
        Ok(()) => {
            tracing::info!(source = %name, url = ?public_url, "webhook registered with Telegram")
        }
        Err(err) => {
            tracing::error!(source = %name, error = %err, "setWebhook failed; Telegram may not deliver updates")
        }
    }
}
