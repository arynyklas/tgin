use crate::base::{Printable, RouteId, Routeable, Serverable};
use crate::utils::defaults::TELEGRAM_TOKEN_RE;
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use std::sync::atomic::{AtomicU64, Ordering};

/// Default per-request deadline for forwarding an update to a downstream bot.
/// A bot that does not ack within this window is treated as failed for the
/// purpose of this dispatch — the update is dropped (we never re-deliver to a
/// webhook route, by design) and the dispatcher moves on so one slow
/// downstream cannot stall the channel. Override per route via
/// `RouteConfig::WebhookRoute { request_timeout_ms }` or the management API.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct WebhookRoute {
    client: Client,
    url: String,
    request_timeout: Duration,
    dispatched: AtomicU64,
    http_success: AtomicU64,
    http_failure: AtomicU64,
    http_error: AtomicU64,
}

impl WebhookRoute {
    pub fn new(url: String, client: Client, request_timeout: Duration) -> Self {
        Self {
            client,
            url,
            request_timeout,
            dispatched: AtomicU64::new(0),
            http_success: AtomicU64::new(0),
            http_failure: AtomicU64::new(0),
            http_error: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl Routeable for WebhookRoute {
    async fn process(&self, update: Bytes) {
        // Forward the raw bytes verbatim. No `.json(&value)` round-trip:
        // re-serializing here would discard byte-for-byte fidelity (key
        // ordering, number normalisation) and add a `Value` allocation
        // and a serializer pass on every dispatch.
        self.dispatched.fetch_add(1, Ordering::Relaxed);
        match self
            .client
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .body(update)
            .timeout(self.request_timeout)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                self.http_success.fetch_add(1, Ordering::Relaxed);
            }
            Ok(resp) => {
                self.http_failure.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    route = %TELEGRAM_TOKEN_RE.replace_all(&self.url, "#####"),
                    status = resp.status().as_u16(),
                    "webhook downstream returned non-success"
                );
            }
            Err(err) => {
                self.http_error.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    route = %TELEGRAM_TOKEN_RE.replace_all(&self.url, "#####"),
                    error = %err,
                    "webhook downstream request failed"
                );
            }
        }
    }

    fn id(&self) -> Option<RouteId> {
        Some(RouteId::Url(self.url.clone()))
    }
}

impl Serverable for WebhookRoute {}

#[async_trait]
impl Printable for WebhookRoute {
    async fn print(&self) -> String {
        format!("webhook: {}", self.url)
    }

    async fn json_struct(&self) -> Value {
        json!({
            "type": "webhook",
            "options": {
                "url": TELEGRAM_TOKEN_RE.replace_all(&self.url, "#####").as_ref()
            },
            "metrics": {
                "dispatched_total": self.dispatched.load(Ordering::Relaxed),
                "http_success_total": self.http_success.load(Ordering::Relaxed),
                "http_failure_total": self.http_failure.load(Ordering::Relaxed),
                "http_error_total": self.http_error.load(Ordering::Relaxed)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_client() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn test_process_sends_correct_post_request() {
        let mock_server = MockServer::start().await;

        let payload = json!({
            "update_id": 12345,
            "message": { "text": "hello webhook" }
        });

        Mock::given(method("POST"))
            .and(body_json(&payload))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let route = WebhookRoute::new(mock_server.uri(), test_client(), DEFAULT_REQUEST_TIMEOUT);

        route.process(Bytes::from(serde_json::to_vec(&payload).unwrap())).await;
    }

    #[tokio::test]
    async fn test_process_does_not_panic_on_network_error() {
        let route = WebhookRoute::new(
            "http://localhost:9999/invalid".to_string(),
            test_client(),
            DEFAULT_REQUEST_TIMEOUT,
        );

        let payload = json!({"test": "data"});
        route.process(Bytes::from(serde_json::to_vec(&payload).unwrap())).await;
    }

    #[tokio::test]
    async fn test_printable_implementation() {
        let url = "http://my-bot.com/webhook";
        let route = WebhookRoute::new(url.to_string(), test_client(), DEFAULT_REQUEST_TIMEOUT);

        assert_eq!(route.print().await, format!("webhook: {}", url));
        let json_info = route.json_struct().await;
        assert_eq!(json_info["type"], "webhook");
        assert_eq!(json_info["options"]["url"], url);
    }

    /// A downstream URL that embeds a Telegram bot token must surface redacted
    /// in `json_struct` — the tree feeds the unauthenticated `/status` document
    /// and the management API `/routes`, so a leaked token here is a token leak.
    #[tokio::test]
    async fn test_json_struct_redacts_token_in_url() {
        let url = "https://api.telegram.org/bot12345678:ABCDEFGHIJKLMNOPQRSTUVWXYZ012345/fwd";
        let route = WebhookRoute::new(url.to_string(), test_client(), DEFAULT_REQUEST_TIMEOUT);

        let reported = route.json_struct().await["options"]["url"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!reported.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"), "token leaked: {reported}");
        assert!(reported.contains("#####"), "expected redaction marker: {reported}");
    }

    /// A downstream that holds the request open longer than the configured
    /// per-request timeout MUST cause `process` to give up and return inside the
    /// timeout window. Without this guarantee a single slow bot would park a
    /// routing slot for the full default 10 s and starve the dispatcher.
    #[tokio::test]
    async fn test_process_honors_per_request_timeout() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(Duration::from_secs(5)),
            )
            .mount(&mock_server)
            .await;

        let route = WebhookRoute::new(
            mock_server.uri(),
            test_client(),
            Duration::from_millis(100),
        );

        let start = std::time::Instant::now();
        route.process(Bytes::from(serde_json::to_vec(&json!({"update_id": 1})).unwrap())).await;
        let elapsed = start.elapsed();

        // Comfortable headroom over the 100ms cap, well under the 5s downstream delay.
        assert!(
            elapsed < Duration::from_secs(2),
            "process did not honor the per-request timeout (elapsed {elapsed:?})",
        );
    }

    /// A 2xx downstream response increments dispatched + http_success.
    #[tokio::test]
    async fn test_process_records_success_counters() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let route = WebhookRoute::new(mock_server.uri(), test_client(), DEFAULT_REQUEST_TIMEOUT);
        route
            .process(Bytes::from(serde_json::to_vec(&json!({"update_id": 1})).unwrap()))
            .await;

        let m = route.json_struct().await;
        assert_eq!(m["metrics"]["dispatched_total"], 1);
        assert_eq!(m["metrics"]["http_success_total"], 1);
        assert_eq!(m["metrics"]["http_failure_total"], 0);
        assert_eq!(m["metrics"]["http_error_total"], 0);
    }

    /// A non-success status increments http_failure (not http_error): the
    /// request completed, the downstream just refused it.
    #[tokio::test]
    async fn test_process_records_failure_on_non_success_status() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let route = WebhookRoute::new(mock_server.uri(), test_client(), DEFAULT_REQUEST_TIMEOUT);
        route
            .process(Bytes::from(serde_json::to_vec(&json!({"update_id": 1})).unwrap()))
            .await;

        let m = route.json_struct().await;
        assert_eq!(m["metrics"]["dispatched_total"], 1);
        assert_eq!(m["metrics"]["http_failure_total"], 1);
        assert_eq!(m["metrics"]["http_success_total"], 0);
    }

    /// A transport-level failure (no server listening) increments http_error.
    #[tokio::test]
    async fn test_process_records_error_on_unroutable_url() {
        let route = WebhookRoute::new(
            "http://127.0.0.1:9/never".to_string(),
            test_client(),
            Duration::from_millis(500),
        );
        route
            .process(Bytes::from(serde_json::to_vec(&json!({"update_id": 1})).unwrap()))
            .await;

        let m = route.json_struct().await;
        assert_eq!(m["metrics"]["dispatched_total"], 1);
        assert_eq!(m["metrics"]["http_error_total"], 1);
        assert_eq!(m["metrics"]["http_success_total"], 0);
    }
}
