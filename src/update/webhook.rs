use crate::base::{Printable, Serverable};
use crate::update::base::Updater;

use crate::utils::defaults::TELEGRAM_TOKEN_REGEX;

use async_trait::async_trait;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Router,
};
use bytes::Bytes;
use serde_json::json;
use serde_json::value::RawValue;
use std::time::Duration;

use subtle::ConstantTimeEq;

use reqwest::Client;

use tokio::sync::mpsc::Sender;

use regex::Regex;

pub struct RegistrationWebhookConfig {
    public_ip: String,
    client: Client,
    set_webhook_url: String,

    token_regex: Regex,
}

impl RegistrationWebhookConfig {
    pub fn new(token: String, public_ip: String, client: Client) -> Self {
        Self {
            public_ip,
            client,
            set_webhook_url: format!("https://api.telegram.org/bot{}/setWebhook", token),
            token_regex: Regex::new(TELEGRAM_TOKEN_REGEX).unwrap(),
        }
    }

    pub fn set_webhook_url(&mut self, set_webhook_url: String) {
        self.set_webhook_url = set_webhook_url;
    }
}

pub struct WebhookUpdate {
    path: String,
    secret_token: Option<String>,
    registration: Option<RegistrationWebhookConfig>,
}

impl WebhookUpdate {
    pub fn new(path: String) -> Self {
        Self {
            path,
            secret_token: None,
            registration: None,
        }
    }

    pub fn set_secret_token(&mut self, token: String) {
        self.secret_token = Some(token);
    }

    pub fn set_registration(&mut self, config: RegistrationWebhookConfig) {
        self.registration = Some(config);
    }

    pub async fn register_webhook(&self, config: &RegistrationWebhookConfig) {
        let full_url = format!("{}{}", config.public_ip.trim_end_matches('/'), self.path);

        let params = json!({ "url": full_url });

        match config
            .client
            .post(&config.set_webhook_url)
            .json(&params)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status().is_success() {
                    println!("Webhook set successfully for path: {}", self.path);
                } else {
                    eprintln!("Failed to set webhook. Status: {}", resp.status());
                }
            }
            Err(e) => eprintln!("Network error setting webhook: {}", e),
        }
    }
}

#[async_trait]
impl Updater for WebhookUpdate {
    async fn start(&self, _tx: Sender<Bytes>) {
        if let Some(config) = &self.registration {
            self.register_webhook(config).await;
        } else {
            println!(
                "Webhook started in passive mode (no auto-registration) for {}",
                self.path
            );
        }
    }
}

#[async_trait]
impl Serverable for WebhookUpdate {
    async fn set_server(&self, router: Router<Sender<Bytes>>) -> Router<Sender<Bytes>> {
        let path = self.path.clone();
        let secret = self.secret_token.clone();

        // Capture the request body as raw `Bytes` instead of parsing
        // `Json<Value>`: Telegram already produced valid JSON; the
        // router's job is to forward those bytes verbatim. Saves one
        // deep parse on every webhook hit. Downstreams that expect
        // structured JSON pay one parse on their side, the same as
        // they always have.
        //
        // We still gate on JSON *shape* before pushing into the channel:
        // a single `from_slice::<&RawValue>` confirms the bytes are a
        // syntactically valid JSON value without allocating a `Value`
        // tree or reformatting the wire bytes. The routing tree assumes
        // every `process(update)` call carries one valid JSON value
        // (see `Routeable::process`); without this gate, a misbehaving
        // caller posting non-JSON corrupts the next `getUpdates`
        // response on any `LongPollRoute` downstream and the consuming
        // bot's client library aborts the whole batch.
        let handler_func = move |State(tx): State<Sender<Bytes>>,
                                 headers: HeaderMap,
                                 body: Bytes| {
            let secret = secret.clone();
            async move {
                if let Some(sec) = secret {
                    let provided = headers
                        .get("x-telegram-bot-api-secret-token")
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("");
                    let provided_bytes = provided.as_bytes();
                    let expected_bytes = sec.as_bytes();
                    let len_eq = provided_bytes.len() == expected_bytes.len();
                    // Compare same-length buffers (zero-padded to expected len) so that
                    // a length mismatch still goes through ct_eq and doesn't short-circuit
                    // by length alone. The final `&` with `len_eq` rejects length mismatches.
                    let mut padded = vec![0u8; expected_bytes.len()];
                    let copy_len = provided_bytes.len().min(expected_bytes.len());
                    padded[..copy_len].copy_from_slice(&provided_bytes[..copy_len]);
                    let bytes_eq: bool = padded.ct_eq(expected_bytes).into();
                    if !(len_eq && bytes_eq) {
                        return StatusCode::UNAUTHORIZED;
                    }
                }
                // Validate JSON shape *after* the secret check so
                // unauthenticated callers can't probe body parsing
                // behaviour. `&RawValue` borrows from `body` and is
                // dropped at the end of this `if` -- the bytes are
                // forwarded unchanged below.
                if serde_json::from_slice::<&RawValue>(&body).is_err() {
                    return StatusCode::BAD_REQUEST;
                }
                match tx.send(body).await {
                    Ok(()) => StatusCode::OK,
                    // Channel closed: the dispatcher is gone (shutdown). Telling Telegram
                    // 200 here would silently drop the update because Telegram never re-
                    // delivers an acknowledged update. 503 makes it retry.
                    Err(_) => StatusCode::SERVICE_UNAVAILABLE,
                }
            }
        };

        router.route(&path, post(handler_func))
    }
}

#[async_trait]
impl Printable for WebhookUpdate {
    async fn print(&self) -> String {
        let reg_text = match &self.registration {
            Some(reg) => format!(
                "REGISTRATED ON {}",
                &reg.token_regex.replace_all(&reg.set_webhook_url, "#####")
            ),
            None => "".to_string(),
        };
        format!("webhook: 0.0.0.0{} {}", self.path, reg_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tokio::sync::mpsc;
    use tower::ServiceExt;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_webhook_registers_correctly() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": true})),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let my_ip = "https://my-server.com";
        let token = "TOKEN123";

        let client = Client::builder().no_proxy().build().unwrap();
        let mut reg_config =
            RegistrationWebhookConfig::new(token.to_string(), my_ip.to_string(), client);

        reg_config.set_webhook_url(format!("{}/setWebhook", mock_server.uri()));

        let mut updater = WebhookUpdate::new("/webhook".to_string());
        updater.set_registration(reg_config);

        let (tx, _) = mpsc::channel(1);

        updater.start(tx).await;
    }

    #[tokio::test]
    async fn test_webhook_handler_receives_json_and_sends_to_channel() {
        let updater = WebhookUpdate::new("/bot/update".to_string());

        let (tx, mut rx) = mpsc::channel(10);

        let app = Router::new();
        let app = updater.set_server(app).await.with_state(tx);

        let incoming_payload = json!({
            "update_id": 999,
            "message": { "text": "Hello via Webhook" }
        });

        let request = Request::builder()
            .method("POST")
            .uri("/bot/update")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&incoming_payload).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let received = rx.recv().await.expect("Channel should receive update");
        let received: serde_json::Value = serde_json::from_slice(&received).unwrap();

        assert_eq!(received["update_id"], 999);
        assert_eq!(received["message"]["text"], "Hello via Webhook");
    }

    /// When the dispatcher channel is closed, returning 200 to Telegram would silently
    /// drop the update -- Telegram never re-delivers an acknowledged update. The handler
    /// MUST surface that as 503 so Telegram retries.
    #[tokio::test]
    async fn test_handler_returns_503_on_closed_channel() {
        let updater = WebhookUpdate::new("/bot/update".to_string());

        let (tx, rx) = mpsc::channel(10);
        // Drop the receiver before any request arrives -- channel is now closed for
        // sends. The handler captured `tx` via Router::with_state.
        drop(rx);

        let app = updater.set_server(Router::new()).await.with_state(tx);

        let request = Request::builder()
            .method("POST")
            .uri("/bot/update")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&json!({"update_id": 1})).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Secret-token check: missing / wrong header -> 401, correct header -> 200.
    /// Covers the constant-time comparison swap; previously this path was uncovered.
    #[tokio::test]
    async fn test_handler_secret_token_check() {
        let secret = "shared-secret".to_string();
        let mut updater = WebhookUpdate::new("/bot/update".to_string());
        updater.set_secret_token(secret.clone());
        let updater = updater;

        let send = |header: Option<&'static str>| {
            let updater = &updater;
            async move {
                let (tx, mut rx) = mpsc::channel(10);
                let app = updater.set_server(Router::new()).await.with_state(tx);

                let mut builder = Request::builder()
                    .method("POST")
                    .uri("/bot/update")
                    .header("content-type", "application/json");
                if let Some(h) = header {
                    builder = builder.header("x-telegram-bot-api-secret-token", h);
                }
                let request = builder
                    .body(Body::from(serde_json::to_vec(&json!({"update_id": 1})).unwrap()))
                    .unwrap();

                let status = app.oneshot(request).await.unwrap().status();
                // Drain so the channel doesn't accumulate across calls.
                let _ = rx.try_recv();
                status
            }
        };

        assert_eq!(send(None).await, StatusCode::UNAUTHORIZED, "missing header");
        assert_eq!(send(Some("wrong")).await, StatusCode::UNAUTHORIZED, "wrong secret");
        // Length-mismatch case (shorter than expected) -- exercises the padded ct_eq path.
        assert_eq!(send(Some("shared")).await, StatusCode::UNAUTHORIZED, "too short");
        // Length-mismatch case (longer than expected) -- exercises the padded ct_eq path.
        assert_eq!(
            send(Some("shared-secret-extra")).await,
            StatusCode::UNAUTHORIZED,
            "too long"
        );
        assert_eq!(send(Some("shared-secret")).await, StatusCode::OK, "correct secret");
    }

    /// `Routeable::process` requires a syntactically valid JSON value;
    /// the routing tree concatenates the bytes verbatim into a
    /// `getUpdates` envelope, so non-JSON ingress would corrupt the
    /// response and silently drop every update batched alongside it.
    /// The webhook handler MUST gate on shape and return 400 *before*
    /// the bytes reach the channel.
    #[tokio::test]
    async fn test_handler_rejects_non_json_body() {
        let updater = WebhookUpdate::new("/bot/update".to_string());

        let cases: &[(&str, &[u8])] = &[
            ("binary garbage", b"garbage\x00not-json"),
            ("empty body", b""),
            ("trailing junk after a JSON value", br#"{"update_id":1} trailing"#),
            ("unterminated object", br#"{"update_id":1"#),
            ("bare identifier", b"undefined"),
        ];

        for (label, body) in cases {
            let (tx, mut rx) = mpsc::channel(10);
            let app = updater.set_server(Router::new()).await.with_state(tx);

            let request = Request::builder()
                .method("POST")
                .uri("/bot/update")
                .header("content-type", "application/json")
                .body(Body::from(body.to_vec()))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{label}: non-JSON body must be rejected with 400"
            );

            // The malformed body MUST NOT have been forwarded onto the
            // channel -- otherwise the next LongPollRoute downstream
            // would emit a corrupted `getUpdates` envelope.
            assert!(
                rx.try_recv().is_err(),
                "{label}: malformed body leaked into the channel"
            );
        }
    }

    /// The shape gate runs *after* the secret-token check, so an
    /// unauthenticated caller posting non-JSON sees 401 (auth failure
    /// remains the louder signal) rather than 400. Guards against
    /// a refactor that swaps the two checks and lets unauthenticated
    /// callers probe body-parsing behaviour.
    #[tokio::test]
    async fn test_handler_secret_check_runs_before_json_shape_check() {
        let mut updater = WebhookUpdate::new("/bot/update".to_string());
        updater.set_secret_token("shared-secret".to_string());

        let (tx, mut rx) = mpsc::channel(10);
        let app = updater.set_server(Router::new()).await.with_state(tx);

        let request = Request::builder()
            .method("POST")
            .uri("/bot/update")
            .header("content-type", "application/json")
            // Wrong secret AND non-JSON body -- auth must win.
            .header("x-telegram-bot-api-secret-token", "wrong")
            .body(Body::from(b"garbage\x00not-json".to_vec()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err());
    }
}
