//! `tgin-bench bot`: a fast downstream bot for the harness.
//!
//! It answers every update with `sendMessage` echoing the update's text, as
//! the aiogram bots do, but with negligible per-update work, so a scenario
//! measures the engine in front of it rather than the bot.
//!
//! - **Webhook:** an HTTP server that answers each POSTed update with 200 at
//!   once and sends the reply in the background.
//! - **Long poll:** one `getUpdates` at a time, the way frameworks poll:
//!   `offset` = last `update_id` + 1, `limit` 100, `timeout` 30 s, the
//!   parameters as a JSON body. Replies go out in the background while the
//!   next poll is already parked.

use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Router,
};
use clap::ValueEnum;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tokio::net::TcpListener;

/// Telegram's largest `getUpdates` batch.
pub const POLL_LIMIT: u32 = 100;
/// The long-poll hold the bot asks for, seconds.
pub const POLL_TIMEOUT_SECS: u64 = 30;
/// Pause after a failed poll, so a dead engine does not spin the bot.
const POLL_RETRY: Duration = Duration::from_millis(200);

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BotMode {
    /// Serve `--webhook-path` on `--port`.
    Webhook,
    /// Poll `--endpoint`.
    Longpoll,
}

#[derive(clap::Args, Clone, Debug)]
pub struct BotOptions {
    #[arg(value_enum)]
    pub mode: BotMode,
    /// The Bot API that receives `sendMessage`.
    #[arg(
        long,
        env = "TELEGRAM_API_URL",
        default_value = "http://127.0.0.1:8090"
    )]
    pub api_url: String,
    #[arg(long, env = "BOT_TOKEN", default_value = "123:test")]
    pub token: String,
    /// The webhook (webhook mode) and `/healthz` (both modes) port.
    #[arg(long, env = "PORT", default_value_t = 8080)]
    pub port: u16,
    #[arg(long, env = "WEBHOOK_PATH", default_value = "/webhook")]
    pub webhook_path: String,
    /// The `getUpdates` URL to poll (long-poll mode).
    #[arg(long, env = "TGIN_ENDPOINT")]
    pub endpoint: Option<String>,
    /// Simulated work per update before the reply, milliseconds.
    #[arg(long, env = "BOT_SLEEP_MS", default_value_t = 0)]
    pub sleep_ms: u64,
}

/// The fields of an update the bot reads.
#[derive(Deserialize, Debug)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
}

#[derive(Deserialize, Debug)]
pub struct Message {
    pub chat: Chat,
    pub text: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct Chat {
    pub id: i64,
}

#[derive(Deserialize)]
struct UpdatesResponse {
    ok: bool,
    #[serde(default)]
    result: Vec<Update>,
    #[serde(default)]
    description: Option<String>,
}

/// The offset of the next poll: one past the highest `update_id` seen, so
/// every update of the batch is confirmed; never lower than `offset`.
pub fn next_offset(offset: i64, updates: &[Update]) -> i64 {
    updates
        .iter()
        .map(|update| update.update_id + 1)
        .fold(offset, i64::max)
}

/// Sends the replies.
#[derive(Clone)]
pub struct Replier {
    client: Client,
    send_message: Arc<str>,
    sleep: Duration,
}

impl Replier {
    pub fn new(client: Client, api_url: &str, token: &str, sleep: Duration) -> Self {
        Self {
            client,
            send_message: format!("{}/bot{token}/sendMessage", api_url.trim_end_matches('/'))
                .into(),
            sleep,
        }
    }

    /// Echo the update's text to its chat in the background. Updates without
    /// a text message get no reply.
    pub fn spawn_reply(&self, update: Update) {
        let Some(Message {
            chat,
            text: Some(text),
        }) = update.message
        else {
            return;
        };
        let replier = self.clone();
        tokio::spawn(async move {
            if !replier.sleep.is_zero() {
                tokio::time::sleep(replier.sleep).await;
            }
            let body = json!({ "chat_id": chat.id, "text": text });
            if let Err(error) = replier
                .client
                .post(&*replier.send_message)
                .json(&body)
                .send()
                .await
            {
                eprintln!("sendMessage failed: {error}");
            }
        });
    }
}

/// One `getUpdates` call.
pub async fn poll(client: &Client, endpoint: &str, offset: i64) -> Result<Vec<Update>, String> {
    let response = client
        .post(endpoint)
        .json(&json!({
            "offset": offset,
            "limit": POLL_LIMIT,
            "timeout": POLL_TIMEOUT_SECS,
        }))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response.bytes().await.map_err(|error| error.to_string())?;
    let parsed: UpdatesResponse = serde_json::from_slice(&body)
        .map_err(|error| format!("{status}: unreadable getUpdates answer: {error}"))?;
    if !parsed.ok {
        return Err(format!(
            "{status}: {}",
            parsed.description.unwrap_or_default()
        ));
    }
    Ok(parsed.result)
}

/// Poll `endpoint` forever, one call at a time, replying to every update.
pub async fn poll_loop(client: Client, endpoint: String, replier: Replier) {
    let mut offset = 0;
    loop {
        match poll(&client, &endpoint, offset).await {
            Ok(updates) => {
                offset = next_offset(offset, &updates);
                for update in updates {
                    replier.spawn_reply(update);
                }
            }
            Err(error) => {
                eprintln!("getUpdates failed: {error}");
                tokio::time::sleep(POLL_RETRY).await;
            }
        }
    }
}

/// The webhook handler: 200 at once, the reply in the background. An
/// unreadable body is a 400.
async fn webhook(State(replier): State<Replier>, body: Bytes) -> StatusCode {
    match serde_json::from_slice::<Update>(&body) {
        Ok(update) => {
            replier.spawn_reply(update);
            StatusCode::OK
        }
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

/// The bot's HTTP server: `/healthz`, plus the webhook path in webhook mode.
pub fn router(mode: BotMode, webhook_path: &str, replier: Replier) -> Router {
    let router = Router::new().route("/healthz", get(|| async { "ok" }));
    match mode {
        BotMode::Webhook => router.route(webhook_path, post(webhook)),
        BotMode::Longpoll => router,
    }
    .with_state(replier)
}

/// A client for the bot: the poll's hold must not hit a request timeout.
pub fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(POLL_TIMEOUT_SECS + 30))
        .build()
        .expect("the HTTP client builds")
}

pub async fn run(options: BotOptions) -> Result<(), String> {
    let client = client();
    let replier = Replier::new(
        client.clone(),
        &options.api_url,
        &options.token,
        Duration::from_millis(options.sleep_ms),
    );
    if options.mode == BotMode::Longpoll {
        let endpoint = options
            .endpoint
            .clone()
            .ok_or("long-poll mode needs --endpoint (or TGIN_ENDPOINT)")?;
        tokio::spawn(poll_loop(client, endpoint, replier.clone()));
    }
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, options.port))
        .await
        .map_err(|error| format!("cannot listen on port {}: {error}", options.port))?;
    axum::serve(
        listener,
        router(options.mode, &options.webhook_path, replier),
    )
    .await
    .map_err(|error| format!("server failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use serde_json::Value;
    use tokio::sync::mpsc;

    fn updates(ids: &[i64]) -> Vec<Update> {
        ids.iter()
            .map(|&update_id| Update {
                update_id,
                message: None,
            })
            .collect()
    }

    #[test]
    fn an_empty_batch_keeps_the_offset() {
        assert_eq!(next_offset(0, &[]), 0);
        assert_eq!(next_offset(42, &[]), 42);
    }

    #[test]
    fn the_offset_is_one_past_the_highest_id() {
        assert_eq!(next_offset(0, &updates(&[5, 6, 7])), 8);
        assert_eq!(next_offset(8, &updates(&[9, 11, 10])), 12);
    }

    #[test]
    fn the_offset_never_goes_back() {
        // A re-served older update (an engine that ignores offsets may send
        // one) must not un-confirm newer ones.
        assert_eq!(next_offset(20, &updates(&[3, 4])), 20);
    }

    #[test]
    fn updates_parse_with_and_without_a_text_message() {
        let parsed: Vec<Update> = serde_json::from_str(
            r#"[{"update_id":1,"message":{"message_id":1,"date":0,"chat":{"id":7,"type":"private"},"text":"hi"}},
                {"update_id":2,"edited_message":{}}]"#,
        )
        .unwrap();
        assert_eq!(parsed[0].message.as_ref().unwrap().chat.id, 7);
        assert_eq!(
            parsed[0].message.as_ref().unwrap().text.as_deref(),
            Some("hi")
        );
        assert!(parsed[1].message.is_none());
    }

    /// A fake engine pull path: serves `batches` in order, records each
    /// poll's JSON body, and records the replies.
    struct FakeEngine {
        polls: Mutex<Vec<Value>>,
        batches: Mutex<Vec<Value>>,
        replies: mpsc::UnboundedSender<Value>,
    }

    async fn start_fake_engine(
        batches: Vec<Value>,
    ) -> (String, Arc<FakeEngine>, mpsc::UnboundedReceiver<Value>) {
        let (replies, received) = mpsc::unbounded_channel();
        let engine = Arc::new(FakeEngine {
            polls: Mutex::new(Vec::new()),
            batches: Mutex::new(batches.into_iter().rev().collect()),
            replies,
        });
        let app =
            Router::new()
                .route(
                    "/c/bots/getUpdates",
                    post(
                        |State(engine): State<Arc<FakeEngine>>,
                         axum::Json(body): axum::Json<Value>| async move {
                            engine.polls.lock().push(body);
                            let next = engine.batches.lock().pop();
                            match next {
                                Some(batch) => axum::Json(json!({ "ok": true, "result": batch })),
                                None => {
                                    // Hold like an empty long poll.
                                    tokio::time::sleep(Duration::from_secs(5)).await;
                                    axum::Json(json!({ "ok": true, "result": [] }))
                                }
                            }
                        },
                    ),
                )
                .route(
                    "/bot123:test/sendMessage",
                    post(
                        |State(engine): State<Arc<FakeEngine>>,
                         axum::Json(body): axum::Json<Value>| async move {
                            engine.replies.send(body).unwrap();
                            axum::Json(json!({ "ok": true, "result": {} }))
                        },
                    ),
                )
                .with_state(engine.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (base, engine, received)
    }

    fn update(id: i64) -> Value {
        json!({
            "update_id": id,
            "message": { "message_id": id, "date": 0, "chat": { "id": 1, "type": "private" }, "text": id.to_string() }
        })
    }

    #[tokio::test]
    async fn polls_carry_the_offset_limit_and_timeout_and_every_update_is_answered() {
        let (base, engine, mut replies) = start_fake_engine(vec![
            json!([update(5), update(6), update(7)]),
            json!([update(8)]),
        ])
        .await;
        let replier = Replier::new(client(), &base, "123:test", Duration::ZERO);
        let poller = tokio::spawn(poll_loop(
            client(),
            format!("{base}/c/bots/getUpdates"),
            replier,
        ));

        let mut texts = Vec::new();
        for _ in 0..4 {
            let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv())
                .await
                .expect("a reply")
                .unwrap();
            assert_eq!(reply["chat_id"], 1);
            texts.push(reply["text"].as_str().unwrap().to_owned());
        }
        texts.sort();
        assert_eq!(texts, ["5", "6", "7", "8"]);

        // The third poll is parked on the empty queue by now.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let polls = engine.polls.lock().clone();
        poller.abort();
        let offsets: Vec<i64> = polls
            .iter()
            .map(|p| p["offset"].as_i64().unwrap())
            .collect();
        assert_eq!(offsets, [0, 8, 9]);
        for poll in &polls {
            assert_eq!(poll["limit"], 100);
            assert_eq!(poll["timeout"], 30);
        }
    }

    #[tokio::test]
    async fn a_failed_poll_is_retried_with_the_same_offset() {
        let (base, engine, mut replies) = start_fake_engine(vec![
            json!([update(3)]),
            // Not an array: the answer is unreadable and the poll fails.
            json!("broken"),
            json!([update(4)]),
        ])
        .await;
        let replier = Replier::new(client(), &base, "123:test", Duration::ZERO);
        let poller = tokio::spawn(poll_loop(
            client(),
            format!("{base}/c/bots/getUpdates"),
            replier,
        ));
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(5), replies.recv())
                .await
                .expect("a reply");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let offsets: Vec<i64> = engine
            .polls
            .lock()
            .iter()
            .map(|p| p["offset"].as_i64().unwrap())
            .collect();
        poller.abort();
        assert_eq!(offsets, [0, 4, 4, 5]);
    }

    #[tokio::test]
    async fn the_webhook_answers_at_once_and_replies_in_the_background() {
        let (base, _engine, mut replies) = start_fake_engine(vec![]).await;
        // A reply delayed by 300 ms must not delay the webhook answer.
        let replier = Replier::new(client(), &base, "123:test", Duration::from_millis(300));
        let app = router(BotMode::Webhook, "/webhook", replier);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bot = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let started = std::time::Instant::now();
        let response = client()
            .post(format!("{bot}/webhook"))
            .json(&update(9))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(started.elapsed() < Duration::from_millis(200));

        let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv())
            .await
            .expect("a reply")
            .unwrap();
        assert_eq!(reply, json!({ "chat_id": 1, "text": "9" }));

        let bad = client()
            .post(format!("{bot}/webhook"))
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        let health = client().get(format!("{bot}/healthz")).send().await.unwrap();
        assert_eq!(health.status(), StatusCode::OK);
    }
}
