use crate::base::{Printable, Serverable};
use crate::update::base::Updater;
use crate::utils::defaults::TELEGRAM_TOKEN_REGEX;

use async_trait::async_trait;
use bytes::Bytes;
use rand::Rng;
use regex::Regex;
use reqwest::header::RETRY_AFTER;
use reqwest::{header::HeaderMap, Client, StatusCode};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::time::{sleep, Duration};

/// Telegram caps `getUpdates?timeout=` at 50 seconds.
const TELEGRAM_LONG_POLL_TIMEOUT_MAX: u64 = 50;
/// Telegram caps `getUpdates?limit=` at 100.
const TELEGRAM_LONG_POLL_LIMIT_MAX: u64 = 100;
/// Slack added to the long-poll server hold so the per-request HTTP deadline
/// always exceeds Telegram's reply window.
const REQUEST_TIMEOUT_SLACK: Duration = Duration::from_secs(10);
/// Upper bound on the backoff sleep when a flapping upstream keeps failing.
/// Bounded at 1 s for two reasons:
///   * Recovery latency. With `jittered(cap)`, the average sleep at cap is
///     `cap/2`. A 5 s cap meant up to 5 s of contaminated tail latency after
///     any transient blip (idle keep-alive close, brief listener pause,
///     mock-server startup race). 1 s caps the worst-case recovery window
///     to ~1 s and the average to ~500 ms while still rate-limiting a
///     hard outage to a sustainable ~1 RPS.
///   * Long-poll already throttles itself. Each attempt holds for up to
///     `long_poll_timeout` seconds anyway, so we don't need a long backoff
///     to keep retry traffic low — we need a short one to resume polling
///     promptly the moment upstream comes back.
const BACKOFF_CAP_MS: u64 = 1_000;
/// Floor for the backoff base — `error_timeout_sleep=0` would otherwise wedge
/// the loop in a tight retry spin.
const BACKOFF_FLOOR_MS: u64 = 50;

/// One-shot "prolonged outage" warning fires after this many consecutive
/// failed attempts. With `BACKOFF_CAP_MS` of 1 s the average inter-attempt
/// gap settles around 500 ms, so 60 attempts is roughly half a minute of
/// sustained transient failure — long enough that a real Telegram
/// incident has registered, short enough that the operator hears about it
/// before the failure mode propagates. We do *not* terminate on transient
/// failures: Telegram has multi-minute outages we want to ride out, not
/// abandon.
const PROLONGED_OUTAGE_THRESHOLD: u32 = 60;

pub struct LongPollUpdate {
    client: Client,
    url: String,
    /// Sleep between successful poll cycles (ms).
    default_timeout_sleep: u64,
    /// Base backoff after a failed poll cycle (ms). Doubles up to `BACKOFF_CAP_MS`.
    error_timeout_sleep: u64,
    /// `getUpdates?timeout=` value. Capped at `TELEGRAM_LONG_POLL_TIMEOUT_MAX`.
    long_poll_timeout: u64,
    /// `getUpdates?limit=` value. Capped at `TELEGRAM_LONG_POLL_LIMIT_MAX`.
    long_poll_limit: u64,
    token_regex: Regex,
    /// Shared permanent-failure counter. Incremented from `start` when the
    /// upstream returns a status that means "this updater will never
    /// succeed without operator intervention" (401 / 403 / 404 — see
    /// [`is_permanent_status`]). The binary reads this counter on
    /// shutdown to decide its exit code.
    health_counter: Arc<AtomicUsize>,
}

impl LongPollUpdate {
    pub fn new(token: String, client: Client) -> Self {
        Self {
            client,
            url: format!("https://api.telegram.org/bot{}/getUpdates", token),
            default_timeout_sleep: 0,
            error_timeout_sleep: 100,
            long_poll_timeout: 30,
            long_poll_limit: TELEGRAM_LONG_POLL_LIMIT_MAX,
            token_regex: Regex::new(TELEGRAM_TOKEN_REGEX).unwrap(),
            health_counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn set_url(&mut self, url: String) {
        self.url = url;
    }

    pub fn set_timeouts(&mut self, default_timeout_sleep: u64, error_timeout_sleep: u64) {
        self.default_timeout_sleep = default_timeout_sleep;
        self.error_timeout_sleep = error_timeout_sleep;
    }

    /// Set Telegram-side polling parameters. Values are clamped to Telegram's
    /// documented maxima (`timeout<=50`, `limit<=100`) — passing larger values
    /// is a config error that we silently correct rather than letting Telegram
    /// reject every request.
    pub fn set_long_poll_params(&mut self, timeout: u64, limit: u64) {
        self.long_poll_timeout = timeout.min(TELEGRAM_LONG_POLL_TIMEOUT_MAX);
        self.long_poll_limit = limit.clamp(1, TELEGRAM_LONG_POLL_LIMIT_MAX);
    }
    /// Override the per-updater health counter so the binary can observe
    /// permanent failures across all updaters and exit non-zero. The
    /// default counter is local to this `LongPollUpdate`; in production
    /// `Tgin` swaps it for a process-wide `Arc<AtomicUsize>` (plumbed via
    /// `build_updates`).
    pub fn set_health_counter(&mut self, counter: Arc<AtomicUsize>) {
        self.health_counter = counter;
    }

    /// Per-request HTTP deadline: long-poll server hold + slack.
    fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.long_poll_timeout) + REQUEST_TIMEOUT_SLACK
    }
}

#[async_trait]
impl Updater for LongPollUpdate {
    async fn start(&self, tx: Sender<Bytes>) {
        let mut offset: i64 = 0;
        let backoff_base_ms = self.error_timeout_sleep.max(BACKOFF_FLOOR_MS);
        let mut backoff_ms = backoff_base_ms;
        let request_timeout = self.request_timeout();
        let redacted_url = self.token_regex.replace_all(&self.url, "#####").to_string();

        // Outage bookkeeping. `consecutive_failures` is reset on every
        // successful 2xx parse; it tracks transient failures only (network
        // error, body read error, JSON parse error, non-success status that
        // is *not* permanent). `prolonged_outage_logged` is sticky for the
        // duration of one outage so a hot retry loop cannot spam stderr,
        // and is cleared on recovery so the *next* outage gets its own
        // announcement. 429 (rate-limit) is intentionally excluded from the
        // counter -- Telegram is healthy and explicitly telling us to
        // wait, which is not the same kind of incident as a flapping
        // upstream.
        let mut consecutive_failures: u32 = 0;
        let mut prolonged_outage_logged: bool = false;

        loop {
            let params = [
                ("offset", offset.to_string()),
                ("timeout", self.long_poll_timeout.to_string()),
                ("limit", self.long_poll_limit.to_string()),
            ];

            let response = self
                .client
                .get(&self.url)
                .query(&params)
                .timeout(request_timeout)
                .send()
                .await;

            let response = match response {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("longpull network error ({}): {:?}", redacted_url, err);
                    note_consecutive_failure(
                        &mut consecutive_failures,
                        &mut prolonged_outage_logged,
                        &redacted_url,
                    );
                    sleep(jittered(backoff_ms)).await;
                    backoff_ms = next_backoff(backoff_ms, BACKOFF_CAP_MS);
                    continue;
                }
            };

            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = parse_retry_after(response.headers());
                let body = response.text().await.unwrap_or_default();
                eprintln!(
                    "longpull rate-limited ({}) status={} body={:?}",
                    redacted_url, status, body
                );
                let wait = retry_after.unwrap_or_else(|| jittered(backoff_ms));
                sleep(wait).await;
                backoff_ms = next_backoff(backoff_ms, BACKOFF_CAP_MS);
                continue;
            }

            if is_permanent_status(status) {
                // 401 (token wrong / revoked), 403 (bot blocked, token
                // valid but route forbidden), 404 (URL malformed: no bot
                // exists at this token). Retrying never fixes any of
                // these -- only operator intervention does. Logging once
                // and returning stops the ~1 RPS of authenticated-but-
                // rejected traffic and surfaces the misconfiguration via
                // the shared health counter so the binary can exit
                // non-zero on shutdown.
                let body = response.text().await.unwrap_or_default();
                eprintln!(
                    "longpull permanent failure ({}) status={} body={:?} -- updater stopping; check token/URL configuration",
                    redacted_url, status, body
                );
                self.health_counter.fetch_add(1, Ordering::Relaxed);
                return;
            }

            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                eprintln!(
                    "longpull HTTP error ({}) status={} body={:?}",
                    redacted_url, status, body
                );
                note_consecutive_failure(
                    &mut consecutive_failures,
                    &mut prolonged_outage_logged,
                    &redacted_url,
                );
                sleep(jittered(backoff_ms)).await;
                backoff_ms = next_backoff(backoff_ms, BACKOFF_CAP_MS);
                continue;
            }

            // Read the response body as raw bytes; do NOT build a
            // `serde_json::Value` tree. The router's job is to forward
            // each update's wire JSON unchanged, so we keep the bytes and
            // only parse enough to walk the `result` array.
            let body = match response.bytes().await {
                Ok(b) => b,
                Err(err) => {
                    eprintln!("longpull body read error ({}): {:?}", redacted_url, err);
                    note_consecutive_failure(
                        &mut consecutive_failures,
                        &mut prolonged_outage_logged,
                        &redacted_url,
                    );
                    sleep(jittered(backoff_ms)).await;
                    backoff_ms = next_backoff(backoff_ms, BACKOFF_CAP_MS);
                    continue;
                }
            };

            let parsed: PollResponse = match serde_json::from_slice(&body) {
                Ok(p) => p,
                Err(err) => {
                    eprintln!("longpull JSON parse error ({}): {:?}", redacted_url, err);
                    note_consecutive_failure(
                        &mut consecutive_failures,
                        &mut prolonged_outage_logged,
                        &redacted_url,
                    );
                    sleep(jittered(backoff_ms)).await;
                    backoff_ms = next_backoff(backoff_ms, BACKOFF_CAP_MS);
                    continue;
                }
            };

            // Successful 2xx parse -> reset backoff and outage counters.
            // Clearing `prolonged_outage_logged` here means a fresh outage
            // (after recovery) gets its own one-shot announcement instead
            // of being suppressed by a previous incident's flag.
            backoff_ms = backoff_base_ms;
            consecutive_failures = 0;
            prolonged_outage_logged = false;

            for raw in parsed.result {
                // The raw text is the original wire bytes for this update.
                // Extract `update_id` (one tiny parse) for offset bookkeeping,
                // then forward the bytes downstream untouched. Per-update
                // cost: one `Bytes` allocation + memcpy of the update size.
                let raw_str = raw.get();
                let id = match serde_json::from_str::<UpdateIdOnly>(raw_str) {
                    Ok(x) => x.update_id,
                    Err(err) => {
                        eprintln!(
                            "longpull update missing update_id ({}): {:?}",
                            redacted_url, err
                        );
                        continue;
                    }
                };
                offset = id + 1;
                let update_bytes = Bytes::copy_from_slice(raw_str.as_bytes());
                // `tx.send(...).await` is the natural backpressure: if
                // the dispatcher is gone (channel closed), exit.
                if tx.send(update_bytes).await.is_err() {
                    return;
                }
            }

            if self.default_timeout_sleep > 0 {
                sleep(Duration::from_millis(self.default_timeout_sleep)).await;
            }
        }
    }
}

/// Minimal shape for the Bot API `getUpdates` response. We borrow each
/// element of `result` as a `Box<RawValue>` so its original wire bytes
/// are preserved verbatim and forwarded to downstream routes without
/// re-serialization.
#[derive(Deserialize)]
struct PollResponse {
    #[serde(default)]
    result: Vec<Box<RawValue>>,
}

/// One-field projection used to read `update_id` out of an already-
/// serialized update. Exists only for offset bookkeeping; the rest of
/// the update is forwarded as opaque bytes.
#[derive(Deserialize)]
struct UpdateIdOnly {
    update_id: i64,
}

/// HTTP statuses we treat as terminal for a long-poll updater.
///
/// * `401 Unauthorized` -- token rejected outright. Either revoked or never
///   valid; no amount of retry recovers without a new token.
/// * `403 Forbidden` -- the bot is banned, blocked, or its scope is wrong.
///   Telegram returns 403 for tokens it once accepted but no longer does
///   (admin-deleted bots, restricted accounts).
/// * `404 Not Found` -- `api.telegram.org/bot<token>/getUpdates` returns 404
///   when `<token>` is malformed (typo, truncation). Retrying a 404 here
///   is just typing the wrong number louder.
///
/// 5xx is *not* in this set: Telegram has multi-minute incidents we
/// expect to ride out. 429 has its own dedicated path with `Retry-After`.
fn is_permanent_status(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED
        || status == StatusCode::FORBIDDEN
        || status == StatusCode::NOT_FOUND
}

/// Bookkeeping for one transient failure. Increments the consecutive-
/// failure counter and, on first crossing of [`PROLONGED_OUTAGE_THRESHOLD`],
/// emits a single louder log line so the operator notices a sustained
/// problem without us spamming stderr every retry.
fn note_consecutive_failure(
    consecutive_failures: &mut u32,
    prolonged_outage_logged: &mut bool,
    redacted_url: &str,
) {
    *consecutive_failures = consecutive_failures.saturating_add(1);
    if !*prolonged_outage_logged && *consecutive_failures >= PROLONGED_OUTAGE_THRESHOLD {
        *prolonged_outage_logged = true;
        eprintln!(
            "longpull prolonged outage ({}): {} consecutive failed attempts -- downstream may be wedged",
            redacted_url, *consecutive_failures
        );
    }
}

/// Double `current_ms` and clamp at `cap_ms`.
fn next_backoff(current_ms: u64, cap_ms: u64) -> u64 {
    current_ms.saturating_mul(2).min(cap_ms)
}

/// Full-jitter delay: uniformly pick in `[0, current_ms]`. Spreads retries so
/// many concurrent updaters don't synchronize their wakeups.
fn jittered(current_ms: u64) -> Duration {
    let max = current_ms.max(1);
    let n = rand::rng().random_range(0..=max);
    Duration::from_millis(n)
}

/// Parse the `Retry-After` HTTP header as a `delta-seconds` value. We do not
/// support `HTTP-date` form — Telegram emits seconds.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

impl Serverable for LongPollUpdate {}

#[async_trait]
impl Printable for LongPollUpdate {
    async fn print(&self) -> String {
        let token = self.token_regex.replace_all(&self.url, "#####");
        format!(
            "longpull: {} timeout={} limit={} sleep={}/{}",
            token,
            self.long_poll_timeout,
            self.long_poll_limit,
            self.default_timeout_sleep,
            self.error_timeout_sleep
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use wiremock::matchers::{any, path};
    use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

    fn test_client() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    fn build_updater(server: &MockServer) -> LongPollUpdate {
        let mut updater = LongPollUpdate::new("MYTOKEN".to_string(), test_client());
        updater.set_url(format!("{}/botMYTOKEN/getUpdates", server.uri()));
        // Tight loop + tiny error sleep so the test exercises both branches quickly.
        updater.set_timeouts(0, 50);
        // Long-poll hold of 0 keeps the mock from blocking the test thread.
        updater.set_long_poll_params(0, 100);
        updater
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_longpoll_fetches_updates_and_sends_to_channel() {
        let mock_server = MockServer::start().await;

        let response_body = serde_json::json!({
            "ok": true,
            "result": [
                { "update_id": 100, "message": { "text": "test1" } },
                { "update_id": 101, "message": { "text": "test2" } }
            ]
        });

        Mock::given(any())
            .and(path("/botMYTOKEN/getUpdates"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(response_body),
            )
            .expect(1..)
            .mount(&mock_server)
            .await;

        let updater = build_updater(&mock_server);

        let (tx, mut rx) = mpsc::channel(10);

        let handle = tokio::spawn(async move {
            updater.start(tx).await;
        });

        let update1 = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("Timed out waiting for update 1")
            .expect("Channel closed unexpectedly");
        let update1: Value = serde_json::from_slice(&update1).unwrap();
        assert_eq!(update1["update_id"], 100);
        assert_eq!(update1["message"]["text"], "test1");

        let update2 = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("Timed out waiting for update 2")
            .expect("Channel closed unexpectedly");
        let update2: Value = serde_json::from_slice(&update2).unwrap();
        assert_eq!(update2["update_id"], 101);

        handle.abort();
    }

    /// `Respond` impl that returns 500 for the first N calls and 200 with a
    /// canned body afterwards. Used to verify the loop survives transient 5xx.
    struct FlakyResponder {
        fail_count: usize,
        calls: Arc<AtomicUsize>,
        success_body: Value,
    }

    impl Respond for FlakyResponder {
        fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_count {
                ResponseTemplate::new(500).set_body_string("upstream broken")
            } else {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(self.success_body.clone())
            }
        }
    }

    /// 5xx responses MUST not be parsed as success and MUST trigger backoff.
    /// After the upstream recovers, the loop MUST resume publishing updates.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_longpoll_retries_on_5xx_then_recovers() {
        let mock_server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));

        let success_body = serde_json::json!({
            "ok": true,
            "result": [{ "update_id": 7, "message": { "text": "after-5xx" } }]
        });

        Mock::given(any())
            .and(path("/botMYTOKEN/getUpdates"))
            .respond_with(FlakyResponder {
                fail_count: 2,
                calls: calls.clone(),
                success_body,
            })
            .expect(3..)
            .mount(&mock_server)
            .await;

        let updater = build_updater(&mock_server);

        let (tx, mut rx) = mpsc::channel(10);
        let handle = tokio::spawn(async move {
            updater.start(tx).await;
        });

        let update = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Timed out waiting for post-5xx update")
            .expect("Channel closed unexpectedly");
        let update: Value = serde_json::from_slice(&update).unwrap();
        assert_eq!(update["update_id"], 7);
        assert!(
            calls.load(Ordering::SeqCst) >= 3,
            "expected at least 3 polls (2 failing + 1 success), saw {}",
            calls.load(Ordering::SeqCst)
        );

        handle.abort();
    }

    /// 429 with a `Retry-After` header MUST cause the loop to wait at least
    /// that many seconds before issuing the next request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_longpoll_honours_retry_after_on_429() {
        let mock_server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));

        struct RateLimited {
            calls: Arc<AtomicUsize>,
            success_body: Value,
        }
        impl Respond for RateLimited {
            fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    ResponseTemplate::new(429)
                        .insert_header("retry-after", "1")
                        .set_body_string("slow down")
                } else {
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "application/json")
                        .set_body_json(self.success_body.clone())
                }
            }
        }

        let success_body = serde_json::json!({
            "ok": true,
            "result": [{ "update_id": 9, "message": { "text": "after-429" } }]
        });

        Mock::given(any())
            .and(path("/botMYTOKEN/getUpdates"))
            .respond_with(RateLimited {
                calls: calls.clone(),
                success_body,
            })
            .expect(2..)
            .mount(&mock_server)
            .await;

        let updater = build_updater(&mock_server);

        let (tx, mut rx) = mpsc::channel(10);
        let started = tokio::time::Instant::now();
        let handle = tokio::spawn(async move {
            updater.start(tx).await;
        });

        let update = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Timed out waiting for post-429 update")
            .expect("Channel closed unexpectedly");
        let update: Value = serde_json::from_slice(&update).unwrap();
        assert_eq!(update["update_id"], 9);

        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(900),
            "expected to wait ~1s for Retry-After, slept {:?}",
            elapsed
        );

        handle.abort();
    }

    #[test]
    fn test_set_long_poll_params_clamps_to_telegram_maxima() {
        let mut up = LongPollUpdate::new("T".to_string(), test_client());
        up.set_long_poll_params(999, 999);
        assert_eq!(up.long_poll_timeout, TELEGRAM_LONG_POLL_TIMEOUT_MAX);
        assert_eq!(up.long_poll_limit, TELEGRAM_LONG_POLL_LIMIT_MAX);

        up.set_long_poll_params(0, 0);
        assert_eq!(up.long_poll_timeout, 0);
        // limit floors to 1 — `getUpdates?limit=0` would be wasteful.
        assert_eq!(up.long_poll_limit, 1);
    }

    #[test]
    fn test_next_backoff_doubles_and_caps() {
        assert_eq!(next_backoff(100, 30_000), 200);
        assert_eq!(next_backoff(20_000, 30_000), 30_000);
        assert_eq!(next_backoff(30_000, 30_000), 30_000);
    }

    #[test]
    fn test_parse_retry_after() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, "5".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(5)));

        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, "  7 ".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(7)));

        // HTTP-date form is unsupported on purpose — Telegram doesn't emit it.
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap());
        assert_eq!(parse_retry_after(&h), None);

        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    /// 401 / 403 / 404 are misconfiguration, not transient. The updater MUST
    /// stop retrying (return from `start`), bump the shared health counter
    /// exactly once, and emit a distinctive `permanent failure` log line.
    /// Without this, a revoked token churns ~1 RPS of authenticated-but-
    /// rejected `getUpdates` against Telegram forever and the operator has
    /// no signal beyond a flood of generic HTTP-error log lines.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_longpoll_terminates_on_permanent_status() {
        for status in [401u16, 403, 404] {
            let mock_server = MockServer::start().await;
            let calls = Arc::new(AtomicUsize::new(0));

            struct CountingPermanent {
                status: u16,
                calls: Arc<AtomicUsize>,
            }
            impl Respond for CountingPermanent {
                fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(self.status).set_body_string("nope")
                }
            }

            Mock::given(any())
                .and(path("/botMYTOKEN/getUpdates"))
                .respond_with(CountingPermanent {
                    status,
                    calls: calls.clone(),
                })
                .mount(&mock_server)
                .await;

            let mut updater = build_updater(&mock_server);
            let health = Arc::new(AtomicUsize::new(0));
            updater.set_health_counter(health.clone());

            let (tx, _rx) = mpsc::channel(10);

            // `start` MUST return on its own -- not via `abort` -- within
            // a tight window. Without the permanent-status handling, the
            // task would loop forever and this `timeout` would fire.
            timeout(Duration::from_secs(2), updater.start(tx))
                .await
                .unwrap_or_else(|_| {
                    panic!("updater never returned for status {status}")
                });

            assert_eq!(
                health.load(Ordering::Relaxed),
                1,
                "status {status}: health counter must increment exactly once"
            );
            assert!(
                calls.load(Ordering::SeqCst) >= 1,
                "status {status}: expected at least one upstream call"
            );
        }
    }

    /// Counterpart to the permanent-status test: 5xx is *not* permanent.
    /// The updater MUST keep retrying through any number of 5xx responses
    /// without touching the health counter. Pairs with
    /// `test_longpoll_retries_on_5xx_then_recovers` (which proves recovery)
    /// to lock in the classification: 5xx is transient, 401/403/404 is not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_longpoll_does_not_terminate_on_5xx() {
        let mock_server = MockServer::start().await;

        Mock::given(any())
            .and(path("/botMYTOKEN/getUpdates"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream broken"))
            .mount(&mock_server)
            .await;

        let mut updater = build_updater(&mock_server);
        let health = Arc::new(AtomicUsize::new(0));
        updater.set_health_counter(health.clone());

        let (tx, _rx) = mpsc::channel(10);

        // `start` must NOT return inside this window: 5xx is transient.
        // If we ever regress and treat 5xx as permanent, this `timeout`
        // resolves with `Ok(())` and the assertion below fires.
        let outcome = timeout(Duration::from_millis(500), updater.start(tx)).await;
        assert!(
            outcome.is_err(),
            "start returned on 5xx; permanent-failure classification leaked into transient errors"
        );
        assert_eq!(
            health.load(Ordering::Relaxed),
            0,
            "5xx must not bump the permanent-failure counter"
        );
    }

    /// `is_permanent_status` is the single source of truth for the
    /// classification. Pin its behaviour: a regression here would silently
    /// re-introduce the retry-forever bug or, worse, turn a transient outage
    /// into a permanent termination.
    #[test]
    fn test_is_permanent_status_classification() {
        for s in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
        ] {
            assert!(is_permanent_status(s), "{s} must be permanent");
        }
        for s in [
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::BAD_REQUEST,
        ] {
            assert!(!is_permanent_status(s), "{s} must NOT be permanent");
        }
    }

    /// `note_consecutive_failure` MUST emit the prolonged-outage log line
    /// exactly once per outage even under sustained failure. The flag stays
    /// sticky until the caller resets it on recovery; that reset path is
    /// covered indirectly by `test_longpoll_retries_on_5xx_then_recovers`,
    /// which observes the loop publishing again after upstream returns 200.
    #[test]
    fn test_note_consecutive_failure_one_shot() {
        let mut consecutive: u32 = PROLONGED_OUTAGE_THRESHOLD - 2;
        let mut logged = false;

        // Below threshold: counter increments, log stays silent.
        note_consecutive_failure(&mut consecutive, &mut logged, "test");
        assert_eq!(consecutive, PROLONGED_OUTAGE_THRESHOLD - 1);
        assert!(!logged);

        // Crossing threshold: log fires, flag flips.
        note_consecutive_failure(&mut consecutive, &mut logged, "test");
        assert_eq!(consecutive, PROLONGED_OUTAGE_THRESHOLD);
        assert!(logged);

        // Past threshold while flag is set: counter still tracks but the
        // log MUST NOT fire again -- same reason `eprintln!` is sticky:
        // we want one signal per outage, not one per retry.
        let logged_before = logged;
        for _ in 0..10 {
            note_consecutive_failure(&mut consecutive, &mut logged, "test");
        }
        assert_eq!(consecutive, PROLONGED_OUTAGE_THRESHOLD + 10);
        assert_eq!(logged, logged_before);
    }

}
