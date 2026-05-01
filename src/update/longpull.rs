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
/// Bounded at 5 s so a transient outage (mock server that hasn't bound yet,
/// brief network blip, single 5xx burst) does not leave the poller asleep
/// past the recovery moment. Sustained outages still rate-limit themselves
/// because the loop hits this cap after ~6 consecutive failures.
const BACKOFF_CAP_MS: u64 = 5_000;
/// Floor for the backoff base — `error_timeout_sleep=0` would otherwise wedge
/// the loop in a tight retry spin.
const BACKOFF_FLOOR_MS: u64 = 50;

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

            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                eprintln!(
                    "longpull HTTP error ({}) status={} body={:?}",
                    redacted_url, status, body
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
                    sleep(jittered(backoff_ms)).await;
                    backoff_ms = next_backoff(backoff_ms, BACKOFF_CAP_MS);
                    continue;
                }
            };

            let parsed: PollResponse = match serde_json::from_slice(&body) {
                Ok(p) => p,
                Err(err) => {
                    eprintln!("longpull JSON parse error ({}): {:?}", redacted_url, err);
                    sleep(jittered(backoff_ms)).await;
                    backoff_ms = next_backoff(backoff_ms, BACKOFF_CAP_MS);
                    continue;
                }
            };

            // Successful 2xx parse \u2192 reset backoff.
            backoff_ms = backoff_base_ms;

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
}
