//! Long-poll ingress: `getUpdates` into a [`Sink`], holding a batch while the
//! sink pushes back.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use tgin_core::facts::Facts;
use tgin_core::ids::SourceId;
use tgin_core::metrics::{SourceErrorReason, SourceMetrics, SourceStateLabel};
use tgin_core::sink::{Admit, Sink};
use tgin_runtime::backoff::{Backoff, honor_retry_after};
use tgin_runtime::supervisor::Exit;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::client::{ApiError, BotClient, Identity, split_updates};
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
/// The longest wait after a 401 or 404 from a self-hosted Bot API server.
const REJECTED_BACKOFF_MAX: Duration = Duration::from_secs(300);
/// A webhook that is active again this soon after a successful
/// `deleteWebhook` was set again by another process (a bot framework calling
/// `setWebhook`, say); deleting it again would only fight that process.
const WEBHOOK_REDELETE_GUARD: Duration = Duration::from_secs(60);

/// Called with the bot's account after every successful `getMe`.
pub type OnIdentity = Arc<dyn Fn(SourceId, &Identity) + Send + Sync>;

/// How a [`LongPollSource`] polls.
#[derive(Clone, Debug)]
pub struct LongPollOptions {
    /// The `getUpdates` long-poll `timeout`, in whole seconds; 1 s to 50 s.
    pub timeout: Duration,
    /// Updates per `getUpdates`, 1 to 100.
    pub limit: u32,
    /// Sent with every `getUpdates`; empty asks for Telegram's default set.
    pub allowed_updates: Vec<String>,
    /// Delete any webhook before the first poll, and again when a poll finds
    /// one active, keeping pending updates. When off, an active webhook
    /// stops the source polling until someone else deletes it.
    pub delete_webhook_on_start: bool,
    /// The error backoff: `(initial, max, floor)`.
    pub backoff: (Duration, Duration, Duration),
    /// The upstream is a self-hosted Bot API server (a `local` upstream).
    /// Its 401 and 404 can come from the server itself (its api_id or
    /// api_hash rejected), so the source waits them out with the error
    /// backoff, capped at 5 min instead, rather than stopping.
    pub local_upstream: bool,
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
            allowed_updates: Vec::new(),
            delete_webhook_on_start: true,
            backoff: (
                Duration::from_millis(500),
                Duration::from_secs(30),
                Duration::from_millis(50),
            ),
            local_upstream: false,
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
    /// Why the source backs off, while it does.
    last_error: Mutex<Option<String>>,
    /// An operator's wait for the server to run dry; see
    /// [`LongPollSource::drain`].
    drain: Drain,
}

/// [`LongPollSource::drain`]'s state, shared with the poll loop.
struct Drain {
    /// True while a drain runs, and after it succeeded: polls do not wait
    /// for updates (`timeout` 0), and an empty batch holds the source. Each
    /// change wakes the poll loop: becoming true gives up its long poll in
    /// flight, becoming false lets a held source poll again.
    on: watch::Sender<bool>,
    /// Counts the empty batches polled while draining.
    empty: watch::Sender<u64>,
}

/// Calls a drain off unless disarmed: the source polls on as before.
struct Resume<'a> {
    drain: &'a Drain,
    armed: bool,
}

impl Drop for Resume<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.drain.on.send_replace(false);
        }
    }
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
    backoff: Backoffs,
    /// When `deleteWebhook` last succeeded.
    webhook_deleted: Option<Instant>,
    /// Whether the source is in [`SourceStateLabel::Running`].
    running: bool,
}

/// The source's error backoffs: one for every error a retry may fix, and
/// one, capped at [`REJECTED_BACKOFF_MAX`], for a self-hosted server's 401
/// or 404.
struct Backoffs {
    errors: Backoff,
    rejected: Backoff,
}

impl Backoffs {
    fn new((initial, max, floor): (Duration, Duration, Duration)) -> Self {
        Self {
            errors: Backoff::new(initial, max, floor),
            rejected: Backoff::new(initial, REJECTED_BACKOFF_MAX, floor),
        }
    }

    fn reset(&mut self) {
        self.errors.reset();
        self.rejected.reset();
    }

    /// The next delay after `err`, or the server's `retry_after` if longer.
    fn delay(&mut self, err: &ApiError) -> Duration {
        let backoff = match err {
            ApiError::Http {
                status: 401 | 404, ..
            } => &mut self.rejected,
            _ => &mut self.errors,
        };
        let computed = backoff.next_delay(&mut rand::rng());
        honor_retry_after(computed, err.retry_after(), RETRY_AFTER_CAP)
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
            last_error: Mutex::new(None),
            drain: Drain {
                on: watch::channel(false).0,
                empty: watch::channel(0).0,
            },
        }
    }

    /// Why the source backs off (a failed `getMe` or `getUpdates`, with the
    /// token masked), while it does.
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().clone()
    }

    /// Returns once the Bot API server holds no update of this bot, all
    /// fetched ones admitted: from now on the source polls without waiting
    /// for updates (giving up its long poll in flight, whose updates are
    /// not confirmed, so the next poll gets them again) until a poll finds
    /// none. The source then polls no more until it is stopped. Dropped
    /// before it returns, the drain is called off and the source polls on
    /// as before.
    pub async fn drain(&self) {
        let mut empty = self.drain.empty.subscribe();
        let mut resume = Resume {
            drain: &self.drain,
            armed: true,
        };
        self.drain.on.send_replace(true);
        // The sender lives as long as `self`.
        let _ = empty.changed().await;
        resume.armed = false;
    }

    /// Calls `getMe`, then polls until `ingest` is cancelled.
    ///
    /// Returns [`Exit::Done`] on cancellation and [`Exit::Fatal`] when the
    /// Bot API server rejects the token (401, 403 or 404; on a self-hosted
    /// server only 403, see [`LongPollOptions::local_upstream`]). Every
    /// other failure, of `getMe` too, is waited out here, on a sleep that
    /// cancelling `ingest` ends at once. Updates not yet admitted at
    /// cancellation are not confirmed, so the server delivers them again to
    /// the next poller.
    pub async fn run(&self, ingest: CancellationToken) -> Exit {
        self.metrics.set_state(SourceStateLabel::Starting);
        let exit = self.poll(&ingest).await;
        self.metrics.set_state(match exit {
            Exit::Done => SourceStateLabel::Stopped,
            Exit::Fatal(_) => SourceStateLabel::Failed,
            // `poll` waits out every failure a retry may fix and never
            // returns `Retry`; the arm is here only because `Exit`, the
            // supervisor's type, has the variant.
            Exit::Retry(_) => SourceStateLabel::Backoff,
        });
        exit
    }

    /// The bot's account, once `getMe` succeeds; waits out the failures a
    /// retry may fix with the error backoff (or the server's
    /// `retry_after`). `Err` is the exit: cancellation, or the token
    /// rejected.
    async fn identify(
        &self,
        ingest: &CancellationToken,
        backoff: &mut Backoffs,
    ) -> Result<Identity, Exit> {
        loop {
            let err = match ingest.run_until_cancelled(self.client.get_me()).await {
                None => return Err(Exit::Done),
                Some(Ok(identity)) => return Ok(identity),
                Some(Err(err)) => err,
            };
            self.count(&err);
            let message = format!("getMe failed: {err}");
            if is_fatal(&err, self.opts.local_upstream) {
                tracing::error!(source = %self.name, error = %err, "the Bot API server rejected the bot token; the source stops");
                return Err(Exit::Fatal(message));
            }
            let delay = backoff.delay(&err);
            tracing::warn!(source = %self.name, error = %err, ?delay, "getMe failed; trying again");
            *self.last_error.lock() = Some(message);
            self.metrics.set_state(SourceStateLabel::Backoff);
            if ingest
                .run_until_cancelled(tokio::time::sleep(delay))
                .await
                .is_none()
            {
                return Err(Exit::Done);
            }
        }
    }

    async fn poll(&self, ingest: &CancellationToken) -> Exit {
        let mut backoff = Backoffs::new(self.opts.backoff);
        let identity = match self.identify(ingest, &mut backoff).await {
            Ok(identity) => identity,
            Err(exit) => return exit,
        };
        (self.on_identity)(self.id, &identity);
        *self.last_error.lock() = None;
        self.metrics.set_state(SourceStateLabel::Running);
        tracing::info!(
            source = %self.name,
            bot_id = identity.user.id,
            username = identity.user.username.as_deref().unwrap_or_default(),
            "long poll started"
        );

        backoff.reset();
        let mut state = Poll {
            offset: 0,
            backoff,
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

        let mut drain = self.drain.on.subscribe();
        loop {
            // Marked seen here, so a drain beginning after this read wakes
            // the poll below.
            let draining = *drain.borrow_and_update();
            let timeout = if draining {
                Duration::ZERO
            } else {
                self.opts.timeout
            };
            let request = self.client.get_updates(
                state.offset,
                self.opts.limit,
                timeout,
                &self.opts.allowed_updates,
            );
            // A long poll given up confirms nothing: its updates come again.
            let polled = tokio::select! {
                () = ingest.cancelled() => return Exit::Done,
                _ = drain.changed(), if !draining => continue,
                polled = request => polled,
            };
            let step = match polled.and_then(|body| split_updates(&body)) {
                Ok(updates) => {
                    state.backoff.reset();
                    if !state.running {
                        state.running = true;
                        *self.last_error.lock() = None;
                        self.metrics.set_state(SourceStateLabel::Running);
                    }
                    if !self.admit(&updates, &mut state.offset, ingest).await {
                        return Exit::Done;
                    }
                    if draining && updates.is_empty() && !self.hold(&mut drain, ingest).await {
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

    /// After a drain's empty batch: reports it, then waits until the drain
    /// is called off (`drain` false). Returns `false` if `ingest` is
    /// cancelled first.
    async fn hold(&self, drain: &mut watch::Receiver<bool>, ingest: &CancellationToken) -> bool {
        self.drain.empty.send_modify(|n| *n += 1);
        // The sender lives as long as `self`; it never closes here.
        let resumed = drain.wait_for(|on| !on);
        ingest.run_until_cancelled(resumed).await.is_some()
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
        // One decode buffer for the batch: escaped text allocates only when
        // it outgrows what an earlier update needed.
        let mut decoded = String::new();
        for (update_id, payload) in updates {
            let facts = match extract(payload, &mut decoded) {
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

    /// Counts a failed `getUpdates`, keeps it as the reason of a backoff, and
    /// decides what to do next.
    async fn recover(&self, err: ApiError, state: &mut Poll, ingest: &CancellationToken) -> Step {
        self.count(&err);
        *self.last_error.lock() = Some(format!("getUpdates failed: {err}"));
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
                        Step::Wait(state.backoff.delay(&err))
                    }
                }
            }
            ApiError::Http {
                status: 409,
                description,
                ..
            } if description.contains("terminated by setWebhook") => {
                // The next poll finds the webhook active, and deletes it
                // (or waits) as `delete_webhook_on_start` says.
                tracing::warn!(source = %self.name, "another process set a webhook on this bot token; polling again");
                Step::Poll
            }
            ApiError::Http { status: 409, .. } => {
                tracing::warn!(source = %self.name, error = %err, "another process is polling this bot token; run one tgin per token");
                Step::Wait(self.opts.conflict_backoff)
            }
            _ if is_fatal(&err, self.opts.local_upstream) => {
                tracing::error!(source = %self.name, error = %err, "the Bot API server rejected the bot token; the source stops");
                Step::Exit(Exit::Fatal(format!("getUpdates failed: {err}")))
            }
            _ => {
                let delay = state.backoff.delay(&err);
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

/// Errors no retry can fix: the Bot API server rejected the token (401, 403,
/// 404) or the client is misconfigured. On a self-hosted server
/// (`local_upstream`) a 401 or 404 may come from the server itself (its
/// api_id or api_hash rejected) and is retried, so only a 403 is fatal.
pub fn is_fatal(err: &ApiError, local_upstream: bool) -> bool {
    match err {
        ApiError::Http {
            status: 401 | 404, ..
        } => !local_upstream,
        ApiError::Http { status: 403, .. } | ApiError::Config(_) => true,
        _ => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16) -> ApiError {
        ApiError::Http {
            status,
            description: String::new(),
            retry_after: None,
        }
    }

    /// A self-hosted server's 401 and 404 back off up to 5 min, on a
    /// backoff of their own; other errors keep the configured cap.
    #[test]
    fn rejections_back_off_up_to_5_min() {
        let defaults = LongPollOptions::default().backoff;
        let mut backoffs = Backoffs::new(defaults);
        for _ in 0..64 {
            for status in [401, 404] {
                assert!(backoffs.delay(&http(status)) <= REJECTED_BACKOFF_MAX);
            }
            assert!(backoffs.delay(&http(502)) <= defaults.1);
        }
        assert_eq!(backoffs.rejected.max(), Duration::from_secs(300));
        assert_eq!(backoffs.rejected.attempt(), 128);
        assert_eq!(backoffs.errors.attempt(), 64);
        backoffs.reset();
        assert_eq!(
            (backoffs.errors.attempt(), backoffs.rejected.attempt()),
            (0, 0)
        );
    }

    #[test]
    fn rejections_fatal_on_cloud_only() {
        for status in [401, 404] {
            assert!(is_fatal(&http(status), false), "{status}");
            assert!(!is_fatal(&http(status), true), "{status}");
        }
        for local in [false, true] {
            assert!(is_fatal(&http(403), local));
            assert!(is_fatal(&ApiError::Config("x"), local));
            assert!(!is_fatal(&http(429), local));
            assert!(!is_fatal(&http(500), local));
        }
    }

    /// Admits everything.
    struct Admits;

    impl Sink for Admits {
        fn admit(&self, _: SourceId, _: i64, _: Bytes, _: &Facts<'_>) -> Admit {
            Admit::Admitted
        }
    }

    /// The `timeout` of each `getUpdates` `server` has seen, in order.
    async fn poll_timeouts(server: &wiremock::MockServer) -> Vec<u64> {
        let requests = server.received_requests().await.unwrap_or_default();
        let polls = requests
            .iter()
            .filter(|r| r.url.path().ends_with("/getUpdates"));
        polls
            .map(|r| {
                let params: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
                params["timeout"].as_u64().unwrap()
            })
            .collect()
    }

    /// A drain called off while the source holds after its empty batch
    /// wakes it: it long-polls again with its own timeout. (One thread:
    /// once the test sees the empty batch, the source waits in `hold`.)
    #[tokio::test]
    async fn called_off_drain_resumes_long_polling() {
        use futures_util::FutureExt as _;
        use wiremock::matchers::path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const TOKEN: &str = "123456789:AAAAexampleSecretTokenValue_-xyz";
        let server = MockServer::start().await;
        Mock::given(path(format!("/bot{TOKEN}/getMe")))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"{"ok":true,"result":{"id":1,"is_bot":true,"first_name":"B"}}"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(path(format!("/bot{TOKEN}/getUpdates")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"ok":true,"result":[]}"#)
                    .set_delay(Duration::from_millis(20)),
            )
            .mount(&server)
            .await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let token = tgin_runtime::secret::Secret::new(TOKEN.to_owned());
        let client = BotClient::new(http, &server.uri().parse().unwrap(), &token).unwrap();
        let mut registry = prometheus_client::registry::Registry::default();
        let metrics = tgin_core::metrics::CoreMetrics::register(&mut registry).source("lp");
        let opts = LongPollOptions {
            timeout: Duration::from_secs(7),
            delete_webhook_on_start: false,
            ..LongPollOptions::default()
        };
        let source = Arc::new(LongPollSource::new(
            SourceId(0),
            Arc::from("lp"),
            client,
            opts,
            Arc::new(Admits),
            metrics,
            Arc::new(|_: SourceId, _: &Identity| {}),
        ));
        let ingest = CancellationToken::new();
        let run = tokio::spawn({
            let (source, ingest) = (Arc::clone(&source), ingest.clone());
            async move { source.run(ingest).await }
        });
        let wait = Duration::from_secs(10);

        let mut empty = source.drain.empty.subscribe();
        let mut drain = Box::pin(source.drain());
        assert!((&mut drain).now_or_never().is_none(), "drained at once");
        tokio::time::timeout(wait, empty.changed())
            .await
            .expect("an empty batch in time")
            .unwrap();
        let held = poll_timeouts(&server).await;
        assert_eq!(held.last(), Some(&0), "{held:?}");
        drop(drain);

        let resumed = tokio::time::timeout(wait, async {
            loop {
                let polls = poll_timeouts(&server).await;
                if polls.len() > held.len() {
                    return polls[held.len()..].to_vec();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("polling again in time");
        assert_eq!(resumed[0], 7, "{resumed:?}");
        ingest.cancel();
        assert_eq!(run.await.unwrap(), Exit::Done);
    }
}
