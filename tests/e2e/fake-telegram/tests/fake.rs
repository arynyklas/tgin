//! The fake Bot API against real sockets: what an end-to-end test sees.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::post,
};
use bytes::Bytes;
use reqwest::{Client, multipart};
use serde_json::{Value, json};
use tgin_fake_telegram::Server;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

const TOKEN: &str = "123456:TEST-token_abc";
const OTHER: &str = "654321:other-Token";
const WAIT: Duration = Duration::from_secs(10);

struct Fake {
    server: Server,
    http: Client,
}

impl Fake {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        Self {
            server: Server::start(listener).unwrap(),
            http: Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.server.addr())
    }

    /// A JSON call; returns the status and the raw body.
    async fn call_raw(&self, token: &str, method: &str, body: Value) -> (u16, Bytes) {
        let response = self
            .http
            .post(self.url(&format!("/bot{token}/{method}")))
            .json(&body)
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.bytes().await.unwrap())
    }

    async fn call(&self, token: &str, method: &str, body: Value) -> (u16, Value) {
        let (status, body) = self.call_raw(token, method, body).await;
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// `result` of a call that must succeed.
    async fn ok(&self, token: &str, method: &str, body: Value) -> Value {
        let (status, body) = self.call(token, method, body).await;
        assert_eq!(status, 200, "{method}: {body}");
        assert_eq!(body["ok"], true, "{method}: {body}");
        body["result"].clone()
    }

    async fn update_ids(&self, token: &str, body: Value) -> Vec<i64> {
        let result = self.ok(token, "getUpdates", body).await;
        ids(&result)
    }

    async fn inject(&self, token: &str, update: Value) -> i64 {
        let response = self
            .http
            .post(self.url(&format!("/fake/updates/{token}")))
            .json(&update)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        body["update_id"].as_i64().unwrap()
    }

    async fn journal(&self, query: &str) -> Vec<Value> {
        let response = self
            .http
            .get(self.url(&format!("/fake/calls{query}")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        body.as_array().unwrap().clone()
    }

    /// Poll the journal until `ready` holds (bounded).
    async fn journal_until(&self, query: &str, ready: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + WAIT;
        loop {
            let entries = self.journal(query).await;
            if ready(&entries) {
                return entries;
            }
            assert!(
                Instant::now() < deadline,
                "journal never got ready: {entries:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait until a `getUpdates` call of `token` is parked in the fake.
    async fn wait_pending_poll(&self, token: &str, pending: usize) {
        let query = format!("?token={token}&method=getUpdates");
        self.journal_until(&query, |entries| {
            entries.iter().filter(|e| e["status"].is_null()).count() == pending
        })
        .await;
    }

    /// A `getUpdates` call in the background; yields status and raw body.
    fn spawn_poll(&self, token: &str, body: Value) -> tokio::task::JoinHandle<(u16, Bytes)> {
        let http = self.http.clone();
        let url = self.url(&format!("/bot{token}/getUpdates"));
        tokio::spawn(async move {
            let response = http.post(url).json(&body).send().await.unwrap();
            (response.status().as_u16(), response.bytes().await.unwrap())
        })
    }
}

fn ids(result: &Value) -> Vec<i64> {
    result
        .as_array()
        .unwrap()
        .iter()
        .map(|update| update["update_id"].as_i64().unwrap())
        .collect()
}

fn text_update(text: &str) -> Value {
    json!({"message": {"message_id": 1, "date": 0, "chat": {"id": 9, "type": "private"}, "text": text}})
}

/// A raw HTTP/1.1 request on a fresh connection; returns status and body.
async fn raw_request(fake: &Fake, head: &str, chunks: &[&[u8]]) -> (u16, Value) {
    let mut stream = TcpStream::connect(fake.server.addr()).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    for chunk in chunks {
        stream
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await
            .unwrap();
        stream.write_all(chunk).await.unwrap();
        stream.write_all(b"\r\n").await.unwrap();
        stream.flush().await.unwrap();
    }
    stream.write_all(b"0\r\n\r\n").await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8(response).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

#[tokio::test]
async fn offset_semantics() {
    let fake = Fake::start().await;
    for expected in 1..=5 {
        let id = fake
            .inject(TOKEN, text_update(&format!("u{expected}")))
            .await;
        assert_eq!(id, expected);
    }

    // Returned updates stay queued until a later offset confirms them.
    let all = fake.ok(TOKEN, "getUpdates", json!({})).await;
    assert_eq!(ids(&all), [1, 2, 3, 4, 5]);
    assert_eq!(all[0]["message"]["text"], "u1");
    assert_eq!(fake.update_ids(TOKEN, json!({"limit": 2})).await, [1, 2]);

    // A positive offset confirms everything below it; limit 0 clamps to 1.
    assert_eq!(
        fake.update_ids(TOKEN, json!({"offset": 3, "limit": 0}))
            .await,
        [3]
    );
    assert_eq!(fake.update_ids(TOKEN, json!({})).await, [3, 4, 5]);

    // A negative offset keeps only the tail and forgets the rest.
    assert_eq!(fake.update_ids(TOKEN, json!({"offset": -2})).await, [4, 5]);
    assert_eq!(fake.update_ids(TOKEN, json!({})).await, [4, 5]);

    // Body fields win over the query string.
    let response = fake
        .http
        .post(fake.url(&format!("/bot{TOKEN}/getUpdates?offset=1&limit=1")))
        .json(&json!({"offset": 5}))
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    assert_eq!(ids(&body["result"]), [5]);

    // Everything confirmed: an empty answer at once with timeout 0.
    assert!(
        fake.update_ids(TOKEN, json!({"offset": 6}))
            .await
            .is_empty()
    );

    // An explicit update_id is kept; the next assigned one follows it.
    assert_eq!(
        fake.inject(TOKEN, json!({"update_id": 100, "message": {"text": "x"}}))
            .await,
        100
    );
    assert_eq!(fake.inject(TOKEN, json!({})).await, 101);
    assert_eq!(
        fake.update_ids(TOKEN, json!({"offset": 6})).await,
        [100, 101]
    );

    // A long poll parks until an update arrives; other tokens are separate.
    assert!(fake.update_ids(OTHER, json!({})).await.is_empty());
    let started = Instant::now();
    let poll = {
        let http = fake.http.clone();
        let url = fake.url(&format!("/bot{TOKEN}/getUpdates"));
        tokio::spawn(async move {
            let response = http
                .post(url)
                .json(&json!({"offset": 102, "timeout": 50}))
                .send()
                .await
                .unwrap();
            response.json::<Value>().await.unwrap()
        })
    };
    fake.wait_pending_poll(TOKEN, 1).await;
    assert_eq!(fake.inject(TOKEN, text_update("late")).await, 102);
    let body = poll.await.unwrap();
    assert_eq!(ids(&body["result"]), [102]);
    assert!(started.elapsed() < WAIT);
    fake.server.stop().await.unwrap();
}

#[tokio::test]
async fn overlapping_polls_conflict() {
    let fake = Fake::start().await;
    let older = fake.spawn_poll(TOKEN, json!({"timeout": 50}));
    fake.wait_pending_poll(TOKEN, 1).await;

    // Another token's poll does not disturb it.
    assert!(fake.update_ids(OTHER, json!({})).await.is_empty());
    // Nor does a call of the same token that answers at once: only a call
    // that parks ends the older one (`Client::do_get_updates`).
    assert!(
        fake.update_ids(TOKEN, json!({"timeout": 0}))
            .await
            .is_empty()
    );
    fake.inject(TOKEN, text_update("first")).await;
    let (status, body) = older.await.unwrap();
    assert_eq!(status, 200);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(ids(&body["result"]), [1]);

    let older = fake.spawn_poll(TOKEN, json!({"offset": 2, "timeout": 50}));
    fake.wait_pending_poll(TOKEN, 1).await;
    let newer = fake.spawn_poll(TOKEN, json!({"offset": 2, "timeout": 50}));
    let (status, body) = older.await.unwrap();
    assert_eq!(status, 409);
    assert_eq!(
        body,
        &br#"{"ok":false,"error_code":409,"description":"Conflict: terminated by other getUpdates request; make sure that only one bot instance is running"}"#[..]
    );

    // The newer poll is still parked and is served normally.
    fake.wait_pending_poll(TOKEN, 1).await;
    fake.inject(TOKEN, text_update("hi")).await;
    let (status, body) = newer.await.unwrap();
    assert_eq!(status, 200);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(ids(&body["result"]), [2]);

    let statuses: Vec<Value> = fake
        .journal(&format!("?token={TOKEN}&method=getUpdates"))
        .await
        .iter()
        .map(|entry| entry["status"].clone())
        .collect();
    assert_eq!(statuses, [json!(200), json!(200), json!(409), json!(200)]);
    fake.server.stop().await.unwrap();
}

#[tokio::test]
async fn abandoned_poll_is_not_left_pending() {
    let fake = Fake::start().await;
    let head = format!(
        "POST /bot{TOKEN}/getUpdates HTTP/1.1\r\nHost: fake\r\n\
         Content-Type: application/json\r\nContent-Length: 14\r\n\r\n{{\"timeout\":50}}"
    );
    let mut stream = TcpStream::connect(fake.server.addr()).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    fake.wait_pending_poll(TOKEN, 1).await;

    // The client goes away (tgin cancels its poll on shutdown or restart).
    drop(stream);
    let query = format!("?token={TOKEN}&method=getUpdates");
    let entries = fake
        .journal_until(&query, |entries| !entries[0]["status"].is_null())
        .await;
    assert_eq!(entries[0]["status"], 499);

    // The next poll is the only pending one.
    let poll = fake.spawn_poll(TOKEN, json!({"timeout": 50}));
    fake.wait_pending_poll(TOKEN, 1).await;
    fake.inject(TOKEN, text_update("x")).await;
    assert_eq!(poll.await.unwrap().0, 200);
    fake.server.stop().await.unwrap();
}

/// A webhook endpoint that always answers 503.
async fn failing_hook() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let app = Router::new().route("/hook", post(|| async { StatusCode::SERVICE_UNAVAILABLE }));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}

#[tokio::test]
async fn webhook_set_blocks_get_updates() {
    let fake = Fake::start().await;
    let (hook, hook_task) = failing_hook().await;

    // setWebhook ends a parked poll.
    let parked = {
        let http = fake.http.clone();
        let url = fake.url(&format!("/bot{TOKEN}/getUpdates"));
        tokio::spawn(async move {
            let response = http
                .post(url)
                .json(&json!({"timeout": 50}))
                .send()
                .await
                .unwrap();
            (response.status().as_u16(), response.bytes().await.unwrap())
        })
    };
    fake.wait_pending_poll(TOKEN, 1).await;
    let (status, body) = fake
        .call(
            TOKEN,
            "setWebhook",
            json!({"url": hook, "secret_token": "s3cr3t"}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({"ok": true, "result": true, "description": "Webhook was set"})
    );
    let (status, body) = parked.await.unwrap();
    assert_eq!(status, 409);
    assert_eq!(
        body,
        &br#"{"ok":false,"error_code":409,"description":"Conflict: terminated by setWebhook request"}"#[..]
    );

    // getUpdates is refused while the webhook is set.
    let (status, body) = fake.call_raw(TOKEN, "getUpdates", json!({})).await;
    assert_eq!(status, 409);
    assert_eq!(
        body,
        &br#"{"ok":false,"error_code":409,"description":"Conflict: can't use getUpdates method while webhook is active; use deleteWebhook to delete the webhook first"}"#[..]
    );
    // Other bots are unaffected.
    assert!(fake.update_ids(OTHER, json!({})).await.is_empty());

    // Failed deliveries keep the updates pending and show up in the info.
    fake.inject(TOKEN, text_update("a")).await;
    fake.inject(TOKEN, text_update("b")).await;
    let deadline = Instant::now() + WAIT;
    let info = loop {
        let info = fake.ok(TOKEN, "getWebhookInfo", json!({})).await;
        if info.get("last_error_message").is_some() {
            break info;
        }
        assert!(Instant::now() < deadline, "no delivery error: {info}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(info["url"], hook.as_str());
    assert_eq!(info["has_custom_certificate"], false);
    assert_eq!(info["pending_update_count"], 2);
    assert_eq!(
        info["last_error_message"],
        "Wrong response from the webhook: 503 Service Unavailable"
    );
    assert!(info["last_error_date"].as_i64().unwrap() > 0);

    // deleteWebhook with drop_pending_updates forgets them.
    let (_, body) = fake
        .call(
            TOKEN,
            "deleteWebhook",
            json!({"drop_pending_updates": true}),
        )
        .await;
    assert_eq!(
        body,
        json!({"ok": true, "result": true, "description": "Webhook was deleted"})
    );
    let info = fake.ok(TOKEN, "getWebhookInfo", json!({})).await;
    assert_eq!(
        info,
        json!({"url": "", "has_custom_certificate": false, "pending_update_count": 0})
    );
    assert!(fake.update_ids(TOKEN, json!({})).await.is_empty());
    let (_, body) = fake.call(TOKEN, "deleteWebhook", json!({})).await;
    assert_eq!(body["description"], "Webhook is already deleted");

    // Without the webhook, getUpdates serves new updates again.
    let id = fake.inject(TOKEN, text_update("c")).await;
    assert_eq!(fake.update_ids(TOKEN, json!({})).await, [id]);

    // A plain deleteWebhook keeps pending updates.
    fake.ok(TOKEN, "setWebhook", json!({"url": hook})).await;
    fake.ok(TOKEN, "deleteWebhook", json!({})).await;
    assert_eq!(fake.update_ids(TOKEN, json!({})).await, [id]);

    hook_task.abort();
    fake.server.stop().await.unwrap();
}

/// Webhooks with scripted answers; each request is reported as
/// `(path, arrival)`. `/slow` answers 503 with `Retry-After: 2` once, then
/// 200; `/busy` always answers 503 with `Retry-After: 30`; `/ok` answers 200.
async fn scripted_hooks() -> (String, mpsc::UnboundedReceiver<(String, Instant)>) {
    type Seen = Arc<(mpsc::UnboundedSender<(String, Instant)>, AtomicUsize)>;
    async fn hook(State(seen): State<Seen>, uri: axum::http::Uri) -> Response {
        let path = uri.path().to_owned();
        seen.0.send((path.clone(), Instant::now())).unwrap();
        let busy = |seconds: &'static str| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, seconds)],
            )
                .into_response()
        };
        match path.as_str() {
            "/slow" if seen.1.fetch_add(1, Ordering::SeqCst) == 0 => busy("2"),
            "/busy" => busy("30"),
            _ => StatusCode::OK.into_response(),
        }
    }
    let (sender, receiver) = mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .fallback(hook)
        .with_state(Arc::new((sender, AtomicUsize::new(0))));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base, receiver)
}

#[tokio::test]
async fn webhook_retry_after_and_last_error() {
    let fake = Fake::start().await;
    let (base, mut seen) = scripted_hooks().await;
    let mut next = async || {
        tokio::time::timeout(WAIT, seen.recv())
            .await
            .expect("no delivery in time")
            .unwrap()
    };
    let deliveries = |count: usize| {
        move |entries: &[Value]| entries.iter().filter(|e| e["kind"] == "webhook").count() == count
    };
    let journal = format!("?token={TOKEN}&kind=webhook");

    // The bot's Retry-After (2 s) replaces the fake's own 1 s backoff.
    fake.ok(TOKEN, "setWebhook", json!({"url": format!("{base}/slow")}))
        .await;
    fake.inject(TOKEN, text_update("a")).await;
    let (_, failed) = next().await;
    let (_, retried) = next().await;
    assert!(
        retried - failed >= Duration::from_secs(2),
        "retried after {:?}",
        retried - failed
    );
    fake.journal_until(&journal, deliveries(2)).await;
    // The last error outlives later successes.
    let info = fake.ok(TOKEN, "getWebhookInfo", json!({})).await;
    assert_eq!(info["pending_update_count"], 0);
    assert_eq!(
        info["last_error_message"],
        "Wrong response from the webhook: 503 Service Unavailable"
    );

    // A new webhook starts without an error.
    fake.ok(TOKEN, "setWebhook", json!({"url": format!("{base}/busy")}))
        .await;
    let info = fake.ok(TOKEN, "getWebhookInfo", json!({})).await;
    assert!(info.get("last_error_message").is_none(), "{info}");
    fake.inject(TOKEN, text_update("b")).await;
    assert_eq!(next().await.0, "/busy");
    fake.journal_until(&journal, deliveries(3)).await;
    let info = fake.ok(TOKEN, "getWebhookInfo", json!({})).await;
    assert!(info.get("last_error_message").is_some(), "{info}");

    // setWebhook cuts the 30 s Retry-After short: the update goes to the
    // new webhook at once.
    fake.ok(TOKEN, "setWebhook", json!({"url": format!("{base}/ok")}))
        .await;
    assert_eq!(next().await.0, "/ok");
    fake.journal_until(&journal, deliveries(4)).await;
    let info = fake.ok(TOKEN, "getWebhookInfo", json!({})).await;
    assert_eq!(info["pending_update_count"], 0);
    assert!(info.get("last_error_message").is_none(), "{info}");
    fake.server.stop().await.unwrap();
}

struct Delivery {
    secret: Option<String>,
    content_type: Option<String>,
    body: Value,
}

/// A webhook bot: records each delivery and replies by the update's text.
async fn replying_hook() -> (String, mpsc::UnboundedReceiver<Delivery>) {
    async fn hook(
        State(seen): State<Arc<mpsc::UnboundedSender<Delivery>>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let header = |name: &str| {
            headers
                .get(name)
                .map(|value| value.to_str().unwrap().to_owned())
        };
        let update: Value = serde_json::from_slice(&body).unwrap();
        let text = update["message"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        seen.send(Delivery {
            secret: header("x-telegram-bot-api-secret-token"),
            content_type: header("content-type"),
            body: update,
        })
        .unwrap();
        if let Some(method) = text.strip_prefix("reply:") {
            let body = json!({"method": method, "chat_id": 1}).to_string();
            return ([(CONTENT_TYPE, "application/json")], body).into_response();
        }
        match text.as_str() {
            "echo" => (
                [(CONTENT_TYPE, "application/json")],
                r#"{"method":"sendMessage","chat_id":42,"text":"echo"}"#,
            )
                .into_response(),
            "doc" => {
                let body = "--XyZ\r\n\
                    Content-Disposition: form-data; name=\"method\"\r\n\r\n\
                    sendDocument\r\n\
                    --XyZ\r\n\
                    Content-Disposition: form-data; name=\"document\"; filename=\"r.txt\"\r\n\
                    Content-Type: text/plain\r\n\r\n\
                    hello\r\n\
                    --XyZ\r\n\
                    Content-Disposition: form-data; name=\"chat_id\"\r\n\r\n\
                    7\r\n\
                    --XyZ--\r\n";
                ([(CONTENT_TYPE, "multipart/form-data; boundary=XyZ")], body).into_response()
            }
            "unparsable" => (
                [(CONTENT_TYPE, "application/json")],
                r#"[{"method":"sendMessage"}]"#,
            )
                .into_response(),
            _ => StatusCode::OK.into_response(),
        }
    }
    let (sender, receiver) = mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/hook", post(hook))
        .with_state(Arc::new(sender));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, receiver)
}

#[tokio::test]
async fn webhook_delivery_and_reply_journaled() {
    let fake = Fake::start().await;
    let (hook, mut seen) = replying_hook().await;
    fake.ok(
        TOKEN,
        "setWebhook",
        json!({"url": hook, "secret_token": "s3cr3t"}),
    )
    .await;
    let info = fake.ok(TOKEN, "getWebhookInfo", json!({})).await;
    assert_eq!(info["url"], hook.as_str());

    // Telegram refuses an empty method, every `get*` method and the webhook
    // and session methods, in any case.
    let texts = [
        "echo",
        "reply:getMe",
        "doc",
        "plain",
        "reply:DeleteWebhook",
        "reply:setWebhook",
        "reply:Close",
        "reply:logout",
        "reply:",
        "reply:GETCHAT",
        "unparsable",
    ];
    for text in texts {
        fake.inject(TOKEN, text_update(text)).await;
    }
    let entries = fake
        .journal_until(&format!("?token={TOKEN}"), |entries| {
            entries.iter().filter(|e| e["kind"] == "webhook").count() == texts.len()
        })
        .await;

    for (index, text) in texts.iter().enumerate() {
        let delivery = seen.recv().await.unwrap();
        assert_eq!(delivery.secret.as_deref(), Some("s3cr3t"));
        assert_eq!(delivery.content_type.as_deref(), Some("application/json"));
        assert_eq!(delivery.body["update_id"], index + 1);
        assert_eq!(delivery.body["message"]["text"], *text);
    }

    let deliveries: Vec<Value> = entries
        .iter()
        .filter(|e| e["kind"] == "webhook")
        .map(|e| {
            json!([
                e["update_id"],
                e["status"],
                e["reply_method"],
                e["reply"],
                e["error"]
            ])
        })
        .collect();
    let refused = |id: i64, method: &str| json!([id, 200, method, "refused", null]);
    assert_eq!(
        deliveries,
        [
            json!([1, 200, "sendMessage", "executed", null]),
            refused(2, "getMe"),
            json!([3, 200, "sendDocument", "executed", null]),
            json!([4, 200, null, null, null]),
            refused(5, "DeleteWebhook"),
            refused(6, "setWebhook"),
            refused(7, "Close"),
            refused(8, "logout"),
            refused(9, ""),
            refused(10, "GETCHAT"),
            json!([11, 200, null, null, "Bad Request: can't parse JSON object"]),
        ]
    );

    // Only the executed replies became calls, after their deliveries.
    let replies: Vec<&Value> = entries
        .iter()
        .filter(|e| e["via"] == "webhook_reply")
        .collect();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["kind"], "call");
    assert_eq!(replies[0]["method"], "sendMessage");
    assert_eq!(replies[0]["args"]["chat_id"], "42");
    assert_eq!(replies[0]["args"]["text"], "echo");
    assert_eq!(replies[0]["status"], 200);
    assert_eq!(replies[1]["method"], "sendDocument");
    assert_eq!(replies[1]["args"]["chat_id"], "7");
    assert_eq!(
        replies[1]["files"],
        json!([{"name": "document", "filename": "r.txt", "size": 5}])
    );
    assert_eq!(replies[1]["status"], 200);
    // Refused replies never ran: the webhook is still set.
    let calls: Vec<&Value> = entries
        .iter()
        .filter(|e| e["kind"] == "call")
        .map(|e| &e["method"])
        .collect();
    assert_eq!(
        calls,
        [
            "setWebhook",
            "getWebhookInfo",
            "sendMessage",
            "sendDocument"
        ]
    );
    let info = fake.ok(TOKEN, "getWebhookInfo", json!({})).await;
    assert_eq!(info["url"], hook.as_str());
    assert_eq!(info["pending_update_count"], 0);
    fake.server.stop().await.unwrap();
}

#[tokio::test]
async fn fault_injection_429() {
    let fake = Fake::start().await;
    let fault = |body: Value| fake.http.post(fake.url("/fake/faults")).json(&body).send();
    let response = fault(json!({"token": TOKEN, "method": "sendMessage", "status": 429, "retry_after": 3, "count": 1}))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    // Faults are per token.
    fake.ok(OTHER, "sendMessage", json!({"chat_id": 1, "text": "x"}))
        .await;

    let message = json!({"chat_id": 42, "text": "hello"});
    let response = fake
        .http
        .post(fake.url(&format!("/bot{TOKEN}/sendMessage")))
        .json(&message)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    // Clients that read only the header see the same delay.
    assert_eq!(response.headers()["retry-after"], "3");
    assert_eq!(
        response.bytes().await.unwrap(),
        &br#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 3","parameters":{"retry_after":3}}"#[..]
    );
    let result = fake.ok(TOKEN, "sendMessage", message).await;
    assert_eq!(result["chat"]["id"], 42);
    assert_eq!(result["text"], "hello");

    let statuses: Vec<Value> = fake
        .journal(&format!("?token={TOKEN}&method=sendMessage"))
        .await
        .iter()
        .map(|entry| entry["status"].clone())
        .collect();
    assert_eq!(statuses, [json!(429), json!(200)]);

    // A delayed fault answers after the delay, once.
    fault(json!({"token": TOKEN, "method": "getMe", "status": 500, "count": 1, "delay_ms": 300}))
        .await
        .unwrap();
    let started = Instant::now();
    let (status, body) = fake.call_raw(TOKEN, "getMe", json!({})).await;
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(status, 500);
    assert_eq!(
        body,
        &br#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#[..]
    );
    fake.ok(TOKEN, "getMe", json!({})).await;

    // An impossible fault is rejected.
    let response = fault(json!({"token": TOKEN, "method": "getMe", "status": 99, "count": 1}))
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    fake.server.stop().await.unwrap();
}

#[tokio::test]
async fn upload_then_download_with_percent_encoded_token() {
    let fake = Fake::start().await;
    let form = multipart::Form::new()
        .text("chat_id", "5")
        .text("caption", "c")
        .part(
            "document",
            multipart::Part::bytes(&b"hello world"[..]).file_name("report 1.txt"),
        );
    let response = fake
        .http
        .post(fake.url(&format!("/bot{TOKEN}/sendDocument")))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let message = &body["result"];
    assert_eq!(message["chat"]["id"], 5);
    assert_eq!(message["caption"], "c");
    assert_eq!(message["document"]["file_name"], "report 1.txt");
    assert_eq!(message["document"]["file_size"], 11);
    let file_id = message["document"]["file_id"].as_str().unwrap().to_owned();

    let file = fake.ok(TOKEN, "getFile", json!({"file_id": file_id})).await;
    assert_eq!(file["file_id"], file_id.as_str());
    assert_eq!(file["file_size"], 11);
    let path = file["file_path"].as_str().unwrap().to_owned();

    // python-telegram-bot sends the token's colon as %3A.
    let encoded = TOKEN.replace(':', "%3A");
    for token in [encoded.as_str(), TOKEN] {
        let response = fake
            .http
            .get(fake.url(&format!("/file/bot{token}/{path}")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{token}");
        assert_eq!(response.bytes().await.unwrap(), &b"hello world"[..]);
    }

    // Files belong to the bot that uploaded them.
    let response = fake
        .http
        .get(fake.url(&format!("/file/bot{OTHER}/{path}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let response = fake
        .http
        .get(fake.url(&format!("/file/botnot-a-token/{path}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    // A stored file_id can be sent again.
    let again = fake
        .ok(
            TOKEN,
            "sendDocument",
            json!({"chat_id": 6, "document": file_id}),
        )
        .await;
    assert_eq!(again["document"]["file_id"], file_id.as_str());
    let (status, body) = fake
        .call(TOKEN, "getFile", json!({"file_id": "nope"}))
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["description"], "Bad Request: invalid file_id");

    let downloads = fake.journal(&format!("?token={TOKEN}&kind=file")).await;
    assert_eq!(downloads.len(), 2);
    assert_eq!(downloads[0]["status"], 200);
    assert_eq!(downloads[0]["args"]["file_path"], path.as_str());
    fake.server.stop().await.unwrap();
}

#[tokio::test]
async fn chunked_and_multipart_bodies_accepted() {
    let fake = Fake::start().await;

    // A chunked JSON body (aiogram, grammY and Telegraf stream uploads).
    let head = format!(
        "POST /bot{TOKEN}/sendMessage HTTP/1.1\r\nHost: fake\r\nConnection: close\r\n\
         Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n"
    );
    let (status, body) =
        raw_request(&fake, &head, &[b"{\"chat_id\":", b"77,\"text\":\"hi\"}"]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["chat"]["id"], 77);
    assert_eq!(body["result"]["text"], "hi");

    // grammY's chunked multipart: the file is an `attach://` part, and a text
    // field follows it.
    let multipart = "--b0und\r\n\
        Content-Disposition: form-data; name=\"chat_id\"\r\n\r\n\
        -1001\r\n\
        --b0und\r\n\
        Content-Disposition: form-data; name=\"document\"\r\n\r\n\
        attach://f1\r\n\
        --b0und\r\n\
        Content-Disposition: form-data; name=\"f1\"; filename=\"x.bin\"\r\n\
        Content-Type: application/octet-stream\r\n\r\n\
        abc\r\n\
        --b0und\r\n\
        Content-Disposition: form-data; name=\"caption\"\r\n\r\n\
        after\r\n\
        --b0und--\r\n";
    let (first, rest) = multipart.as_bytes().split_at(70);
    let head = format!(
        "POST /bot{TOKEN}/sendDocument HTTP/1.1\r\nHost: fake\r\nConnection: close\r\n\
         Content-Type: multipart/form-data; boundary=b0und\r\nTransfer-Encoding: chunked\r\n\r\n"
    );
    let (status, body) = raw_request(&fake, &head, &[first, rest]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["chat"]["id"], -1001);
    assert_eq!(body["result"]["caption"], "after");
    assert_eq!(body["result"]["document"]["file_size"], 3);
    assert_eq!(body["result"]["document"]["file_name"], "x.bin");

    // A form body wins over the query string.
    let response = fake
        .http
        .post(fake.url(&format!("/bot{TOKEN}/sendMessage?chat_id=1&text=q")))
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body("text=b+x%21")
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["result"]["chat"]["id"], 1);
    assert_eq!(body["result"]["text"], "b x!");

    // python-telegram-bot's parameterless POST: no body, no Content-Type.
    let response = fake
        .http
        .post(fake.url(&format!("/bot{TOKEN}/getMe")))
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["result"],
        json!({"id": 123456, "is_bot": true, "first_name": "Fake 123456", "username": "fake_123456_bot"})
    );
    let response = fake
        .http
        .get(fake.url(&format!("/bot{TOKEN}/getMe")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let sends = fake
        .journal(&format!("?token={TOKEN}&method=sendMessage"))
        .await;
    assert_eq!(sends[0]["transfer_encoding"], "chunked");
    assert_eq!(sends[0]["content_type"], "application/json");
    assert_eq!(sends[0]["http_method"], "POST");
    assert_eq!(sends[0]["args"], json!({"chat_id": "77", "text": "hi"}));
    assert_eq!(sends[1]["transfer_encoding"], Value::Null);
    assert_eq!(sends[1]["args"], json!({"chat_id": "1", "text": "b x!"}));
    // Empty filter values count as absent.
    let documents = fake.journal("?token=&method=sendDocument").await;
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0]["transfer_encoding"], "chunked");
    assert_eq!(
        documents[0]["args"],
        json!({"chat_id": "-1001", "document": "attach://f1", "caption": "after"})
    );
    assert_eq!(
        documents[0]["files"],
        json!([{"name": "f1", "filename": "x.bin", "size": 3}])
    );

    // DELETE clears the journal.
    let response = fake
        .http
        .delete(fake.url("/fake/calls"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(fake.journal("").await.is_empty());
    fake.server.stop().await.unwrap();
}

/// Tokens follow the e2e brief's rule, `^[0-9]+:[A-Za-z0-9_-]+$` with a
/// positive id, which is narrower than the official server's (that one
/// answers 421 for a non-integer bot id, for example); the error texts and
/// the path handling are the official server's.
#[tokio::test]
async fn token_and_method_errors_follow_the_brief_token_rule() {
    let fake = Fake::start().await;
    for token in [
        "0123",
        "123",
        "abc:def",
        "123:white%20space",
        "123:",
        "0:abc",
    ] {
        let response = fake
            .http
            .post(fake.url(&format!("/bot{token}/getMe")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{token}");
        assert_eq!(
            response.bytes().await.unwrap(),
            &br#"{"ok":false,"error_code":401,"description":"Unauthorized: invalid token specified"}"#[..],
            "{token}"
        );
    }
    let not_found = br#"{"ok":false,"error_code":404,"description":"Not Found: method not found"}"#;
    let (status, body) = fake.call_raw(TOKEN, "noSuchMethod", json!({})).await;
    assert_eq!(status, 404);
    assert_eq!(body, &not_found[..]);
    // Telegram matches method names case-insensitively.
    let me = fake.ok(TOKEN, "GETME", json!({})).await;
    assert_eq!(me["id"], 123456);

    // The whole path is percent-decoded before the token is split off, as
    // td's HttpReader does: an encoded slash separates token and method.
    let encoded = TOKEN.replace(':', "%3A");
    let response = fake
        .http
        .get(fake.url(&format!("/bot{encoded}%2FgetMe")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let response = fake
        .http
        .get(fake.url("/bot123:bad%2Fx/getMe"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(response.bytes().await.unwrap(), &not_found[..]);

    // Paths outside /bot… get the server's JSON 404.
    for path in ["/", "/nope", "/api/getMe"] {
        let response = fake.http.get(fake.url(path)).send().await.unwrap();
        assert_eq!(response.status(), 404, "{path}");
        assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
        assert_eq!(
            response.bytes().await.unwrap(),
            &br#"{"ok":false,"error_code":404,"description":"Not Found"}"#[..]
        );
    }

    let response = fake
        .http
        .get(fake.url("/fake/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    fake.server.stop().await.unwrap();
}
