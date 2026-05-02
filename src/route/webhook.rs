use crate::base::{Printable, RouteId, Routeable, Serverable};
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

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
}

impl WebhookRoute {
    pub fn new(url: String, client: Client, request_timeout: Duration) -> Self {
        Self { client, url, request_timeout }
    }
}

#[async_trait]
impl Routeable for WebhookRoute {
    async fn process(&self, update: Bytes) {
        // Forward the raw bytes verbatim. No `.json(&value)` round-trip:
        // re-serializing here would discard byte-for-byte fidelity (key
        // ordering, number normalisation) and add a `Value` allocation
        // and a serializer pass on every dispatch.
        let _ = self
            .client
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .body(update)
            .timeout(self.request_timeout)
            .send()
            .await;
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
                "url": self.url
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
}
