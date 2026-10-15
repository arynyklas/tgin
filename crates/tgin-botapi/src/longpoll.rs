//! Long-poll ingress: `getUpdates` into a [`Sink`], holding a batch while the
//! sink pushes back.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tgin_core::facts::Facts;
use tgin_core::ids::SourceId;
use tgin_core::metrics::{SourceErrorReason, SourceMetrics, SourceStateLabel};
use tgin_core::sink::{Admit, Sink};
use tgin_runtime::backoff::{Backoff, honor_retry_after};
use tgin_runtime::supervisor::Exit;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::client::{ApiError, BotClient, BotUser, split_updates};
use crate::facts::extract;

/// Telegram wants a positive `getUpdates` `timeout`; 0 would turn the long
/// poll into a tight short-poll loop.
const MIN_POLL_TIMEOUT: Duration = Duration::from_secs(1);
/// Telegram caps the `getUpdates` `timeout` at 50 s.
const MAX_POLL_TIMEOUT: Duration = Duration::from_secs(50);
/// Telegram caps the `getUpdates` `limit` at 100.
const MAX_LIMIT: u32 = 100;
/// The longest server-requested `retry_after` honoured.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(3600);
/// A webhook that is active again this soon after a successful
/// `deleteWebhook` was set again by another process (a bot framework calling
/// `setWebhook`, say); deleting it again would only fight that process.
const WEBHOOK_REDELETE_GUARD: Duration = Duration::from_secs(60);

/// Called with the bot's account after every successful `getMe`.
pub type OnIdentity = Arc<dyn Fn(SourceId, &BotUser) + Send + Sync>;

/// How a [`LongPollSource`] polls.
#[derive(Clone, Debug)]
pub struct LongPollOptions {
    /// The `getUpdates` long-poll `timeout`, in whole seconds; 1 s to 50 s.
    pub timeout: Duration,
    /// Updates per `getUpdates`, 1 to 100.
    pub limit: u32,
    pub allowed_updates: Option<Vec<String>>,
    /// Delete any webhook before the first poll, and again when a poll finds
    /// one active, keeping pending updates. When off, an active webhook
    /// stops the source polling until someone else deletes it.
    pub delete_webhook_on_start: bool,
    /// The error backoff: `(initial, max, floor)`.
    pub backoff: (Duration, Duration, Duration),
    /// The wait after a 409 that polling again cannot fix.
    pub conflict_backoff: Duration,
    /// The wait before offering again an update the sink refused with
    /// [`Admit::Backpressure`].
    pub admit_retry: Duration,
}

impl Default for LongPollOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            limit: MAX_LIMIT,
            allowed_updates: None,
            delete_webhook_on_start: true,
            backoff: (
                Duration::from_millis(500),
                Duration::from_secs(30),
                Duration::from_millis(50),
            ),
            conflict_backoff: Duration::from_secs(5),
            admit_retry: Duration::from_millis(250),
        }
    }
}

/// Pulls one bot's updates with `getUpdates` and offers them to a [`Sink`].
pub struct LongPollSource<S: Sink> {
    id: SourceId,
    /// For logs.
    name: Arc<str>,
    client: BotClient,
    opts: LongPollOptions,
    sink: Arc<S>,
    metrics: SourceMetrics,
    on_identity: OnIdentity,
}

/// What the poll loop does after a failed `getUpdates`.
enum Step {
    Poll,
    Wait(Duration),
    Exit(Exit),
}

/// Bookkeeping for one [`LongPollSource::run`].
struct Poll {
    /// The first update not yet confirmed: the last admitted id + 1.
    offset: i64,
    backoff: Backoff,
    /// When `deleteWebhook` last succeeded.
    webhook_deleted: Option<Instant>,
    /// Whether the source is in [`SourceStateLabel::Running`].
    running: bool,
}

impl Poll {
    /// The error backoff's next delay, or the server's `retry_after` if
    /// longer.
    fn error_delay(&mut self, err: &ApiError) -> Duration {
        let retry_after = match err {
            ApiError::Http { retry_after, .. } => *retry_after,
            _ => None,
        };
        let computed = self.backoff.next_delay(&mut rand::rng());
        honor_retry_after(computed, retry_after, RETRY_AFTER_CAP)
    }
}

impl<S: Sink> LongPollSource<S> {
    /// `opts.timeout` is clamped to 1 s..=50 s and `opts.limit` to 1..=100,
    /// what Telegram accepts. `on_identity` is called with the bot's account
    /// after every successful `getMe`.
    pub fn new(
        id: SourceId,
        name: Arc<str>,
        client: BotClient,
        mut opts: LongPollOptions,
        sink: Arc<S>,
        metrics: SourceMetrics,
        on_identity: OnIdentity,
    ) -> Self {
        opts.timeout = opts.timeout.clamp(MIN_POLL_TIMEOUT, MAX_POLL_TIMEOUT);
        opts.limit = opts.limit.clamp(1, MAX_LIMIT);
        Self {
            id,
            name,
            client,
            opts,
            sink,
            metrics,
            on_identity,
        }
    }

    /// Calls `getMe`, then polls until `ingest` is cancelled.
    ///
    /// Returns [`Exit::Done`] on cancellation, [`Exit::Fatal`] when Telegram
    /// rejects the token (401, 403 or 404), and [`Exit::Retry`] when `getMe`
    /// fails otherwise (after the server's `retry_after`, if it sent one).
    /// Every other poll failure is waited out here. Updates not yet admitted
    /// at cancellation are not confirmed, so Telegram delivers them again to
    /// the next poller.
    pub async fn run(&self, ingest: CancellationToken) -> Exit {
        self.metrics.set_state(SourceStateLabel::Starting);
        let exit = self.poll(&ingest).await;
        self.metrics.set_state(match exit {
            Exit::Done => SourceStateLabel::Stopped,
            Exit::Retry(_) => SourceStateLabel::Backoff,
            Exit::Fatal(_) => SourceStateLabel::Failed,
        });
        exit
    }

    async fn poll(&self, ingest: &CancellationToken) -> Exit {
        let user = match ingest.run_until_cancelled(self.client.get_me()).await {
            None => return Exit::Done,
            Some(Ok(user)) => user,
            Some(Err(err)) => {
                self.count(&err);
                let message = format!("getMe failed: {err}");
                return if is_fatal(&err) {
                    tracing::error!(source = %self.name, error = %err, "Telegram rejected the bot token; the source stops");
                    Exit::Fatal(message)
                } else {
                    tracing::warn!(source = %self.name, error = %err, "getMe failed");
                    if let ApiError::Http {
                        retry_after: Some(retry_after),
                        ..
                    } = err
                    {
                        self.metrics.set_state(SourceStateLabel::Backoff);
                        let wait =
                            honor_retry_after(Duration::ZERO, Some(retry_after), RETRY_AFTER_CAP);
                        if ingest
                            .run_until_cancelled(tokio::time::sleep(wait))
                            .await
                            .is_none()
                        {
                            return Exit::Done;
                        }
                    }
                    Exit::Retry(message)
                };
            }
        };
        (self.on_identity)(self.id, &user);
        self.metrics.set_state(SourceStateLabel::Running);
        tracing::info!(
            source = %self.name,
            bot_id = user.id,
            username = user.username.as_deref().unwrap_or_default(),
            "long poll started"
        );

        let (initial, max, floor) = self.opts.backoff;
        let mut state = Poll {
            offset: 0,
            backoff: Backoff::new(initial, max, floor),
            webhook_deleted: None,
            running: true,
        };
        if self.opts.delete_webhook_on_start {
            match ingest
                .run_until_cancelled(self.client.delete_webhook(false))
                .await
            {
                None => return Exit::Done,
                Some(Ok(())) => state.webhook_deleted = Some(Instant::now()),
                Some(Err(err)) => {
                    self.count(&err);
                    tracing::warn!(source = %self.name, error = %err, "deleteWebhook failed; polling anyway");
                }
            }
        }

        loop {
            let polled = ingest
                .run_until_cancelled(self.client.get_updates(
                    state.offset,
                    self.opts.limit,
                    self.opts.timeout,
                    self.opts.allowed_updates.as_deref(),
                ))
                .await;
            let Some(polled) = polled else {
                return Exit::Done;
            };
            let step = match polled.and_then(|body| split_updates(&body)) {
                Ok(updates) => {
                    state.backoff.reset();
                    if !state.running {
                        state.running = true;
                        self.metrics.set_state(SourceStateLabel::Running);
                    }
                    if !self.admit(&updates, &mut state.offset, ingest).await {
                        return Exit::Done;
                    }
                    Step::Poll
                }
                Err(err) => self.recover(err, &mut state, ingest).await,
            };
            match step {
                Step::Poll => {}
                Step::Wait(delay) => {
                    if state.running {
                        state.running = false;
                        self.metrics.set_state(SourceStateLabel::Backoff);
                    }
                    let sleep = tokio::time::sleep(delay);
                    if ingest.run_until_cancelled(sleep).await.is_none() {
                        return Exit::Done;
                    }
                }
                Step::Exit(exit) => return exit,
            }
        }
    }

    /// Offers each update in order and moves `offset` past each one the sink
    /// takes. While the sink pushes back, the rest of the batch waits here
    /// and nothing is polled. Returns `false` if `ingest` is cancelled first.
    async fn admit(
        &self,
        updates: &[(i64, Bytes)],
        offset: &mut i64,
        ingest: &CancellationToken,
    ) -> bool {
        for (update_id, payload) in updates {
            let facts = match extract(payload) {
                Ok((_, facts)) => facts,
                Err(err) => {
                    tracing::warn!(source = %self.name, update_id, error = %err, "unreadable update; routing it without facts");
                    Facts::default()
                }
            };
            let mut held = false;
            loop {
                match self
                    .sink
                    .admit(self.id, *update_id, payload.clone(), &facts)
                {
                    Admit::Admitted | Admit::Unroutable => break,
                    Admit::Backpressure => {
                        if !held {
                            held = true;
                            tracing::debug!(source = %self.name, update_id, "sink is full; holding the batch");
                        }
                        let sleep = tokio::time::sleep(self.opts.admit_retry);
                        if ingest.run_until_cancelled(sleep).await.is_none() {
                            return false;
                        }
                    }
                }
            }
            self.metrics.update();
            *offset = update_id.saturating_add(1);
        }
        true
    }

    /// Counts a failed `getUpdates` and decides what to do next.
    async fn recover(&self, err: ApiError, state: &mut Poll, ingest: &CancellationToken) -> Step {
        self.count(&err);
        match &err {
            ApiError::Http {
                status: 409,
                description,
                ..
            } if description.contains("webhook is active") => {
                if !self.opts.delete_webhook_on_start {
                    tracing::error!(source = %self.name, "a webhook is set on this bot token and delete_webhook_on_start is off; delete the webhook or enable the option");
                    return Step::Wait(self.opts.conflict_backoff);
                }
                if state
                    .webhook_deleted
                    .is_some_and(|at| at.elapsed() < WEBHOOK_REDELETE_GUARD)
                {
                    tracing::error!(source = %self.name, "a webhook is active again within 60 s of deleting it: another process (a bot framework calling setWebhook, say) keeps setting it; not deleting it again yet");
                    return Step::Wait(self.opts.conflict_backoff);
                }
                tracing::warn!(source = %self.name, "a webhook is active on this bot token; deleting it (pending updates kept) and polling again");
                match ingest
                    .run_until_cancelled(self.client.delete_webhook(false))
                    .await
                {
                    None => Step::Exit(Exit::Done),
                    Some(Ok(())) => {
                        state.webhook_deleted = Some(Instant::now());
                        Step::Poll
                    }
                    Some(Err(err)) => {
                        self.count(&err);
                        tracing::warn!(source = %self.name, error = %err, "deleteWebhook failed");
                        Step::Wait(state.error_delay(&err))
                    }
                }
            }
            ApiError::Http { status: 409, .. } => {
                tracing::warn!(source = %self.name, error = %err, "another process is polling this bot token; run one tgin per token");
                Step::Wait(self.opts.conflict_backoff)
            }
            _ if is_fatal(&err) => {
                tracing::error!(source = %self.name, error = %err, "Telegram rejected the bot token; the source stops");
                Step::Exit(Exit::Fatal(format!("getUpdates failed: {err}")))
            }
            _ => {
                let delay = state.error_delay(&err);
                tracing::warn!(source = %self.name, error = %err, ?delay, "getUpdates failed; backing off");
                Step::Wait(delay)
            }
        }
    }

    fn count(&self, err: &ApiError) {
        if let Some(reason) = reason(err) {
            self.metrics.error(reason);
        }
    }
}

/// Errors no retry can fix: the token is rejected (401, 403, 404) or the
/// client is misconfigured.
fn is_fatal(err: &ApiError) -> bool {
    matches!(
        err,
        ApiError::Http {
            status: 401 | 403 | 404,
            ..
        } | ApiError::Config(_)
    )
}

/// The metric reason of `err`; a client setup error is not a source error.
fn reason(err: &ApiError) -> Option<SourceErrorReason> {
    Some(match err {
        ApiError::Network(_) => SourceErrorReason::Network,
        ApiError::Decode(_) => SourceErrorReason::Decode,
        ApiError::Http {
            status: 401 | 403 | 404,
            ..
        } => SourceErrorReason::Unauthorized,
        ApiError::Http { status: 429, .. } => SourceErrorReason::RateLimited,
        ApiError::Http { status: 409, .. } => SourceErrorReason::Conflict,
        ApiError::Http { .. } => SourceErrorReason::Http,
        ApiError::Config(_) => return None,
    })
}
