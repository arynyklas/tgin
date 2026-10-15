//! The fake Telegram Bot API: `getUpdates` for long-poll consumers,
//! `sendMessage` for bot replies, and the calls clients make at start.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use axum::{
    body::{to_bytes, Bytes},
    extract::{FromRequest, Multipart, Path, Request, State},
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use dashmap::DashMap;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::track::Tracker;

/// Telegram caps `getUpdates` `limit` at 100.
pub const GET_UPDATES_LIMIT_MAX: usize = 100;
/// Telegram caps `getUpdates` `timeout` at 50 s.
pub const GET_UPDATES_TIMEOUT_MAX_SECS: u64 = 50;
/// Largest `getUpdates` request body read.
const GET_UPDATES_BODY_LIMIT: usize = 64 * 1024;
const POISONED: &str = "update stream lock poisoned";
const RUN_POISONED: &str = "current run lock poisoned";

/// The update queue of one bot token. Like Telegram, it keeps every update
/// until a `getUpdates` call confirms it with a higher `offset`.
pub struct UpdateStream {
    queue: Mutex<VecDeque<Queued>>,
    notify: Notify,
}

struct Queued {
    update_id: i64,
    payload: Bytes,
}

impl UpdateStream {
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        }
    }

    /// Append an update; ids must increase.
    pub fn push(&self, update_id: i64, payload: Bytes) {
        self.queue
            .lock()
            .expect(POISONED)
            .push_back(Queued { update_id, payload });
        // Stores a permit when no poll waits, so the next poll returns at once.
        self.notify.notify_one();
    }

    #[cfg(test)]
    pub fn update_ids(&self) -> Vec<i64> {
        let queue = self.queue.lock().expect(POISONED);
        queue.iter().map(|update| update.update_id).collect()
    }

    /// Confirm updates by `offset`, then return up to `limit` of the rest.
    /// Returned updates stay queued until a later offset confirms them.
    pub(crate) fn take(&self, offset: i64, limit: usize) -> Vec<Bytes> {
        let mut queue = self.queue.lock().expect(POISONED);
        if offset > 0 {
            while queue
                .front()
                .is_some_and(|update| update.update_id < offset)
            {
                queue.pop_front();
            }
        } else if offset < 0 {
            let keep = usize::try_from(offset.unsigned_abs()).unwrap_or(usize::MAX);
            let forgotten = queue.len().saturating_sub(keep);
            queue.drain(..forgotten);
        }
        queue
            .iter()
            .take(limit)
            .map(|update| update.payload.clone())
            .collect()
    }

    /// Answer one `getUpdates` call: return what `take` finds, or wait up to
    /// `timeout` for an update to arrive.
    async fn poll(&self, params: GetUpdatesParams) -> Vec<Bytes> {
        let deadline = tokio::time::Instant::now() + params.timeout;
        loop {
            // Register for the wake-up before looking, so a push between the
            // look and the wait is not missed.
            let mut notified = std::pin::pin!(self.notify.notified());
            notified.as_mut().enable();
            let batch = self.take(params.offset, params.limit);
            if !batch.is_empty() || tokio::time::timeout_at(deadline, notified).await.is_err() {
                return batch;
            }
        }
    }
}

/// `getUpdates` fields as sent, before clamping.
#[derive(Default)]
struct RawParams {
    offset: Option<i64>,
    limit: Option<i64>,
    timeout: Option<i64>,
}

impl RawParams {
    fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let slot = match key {
            "offset" => &mut self.offset,
            "limit" => &mut self.limit,
            "timeout" => &mut self.timeout,
            _ => return Ok(()),
        };
        let parsed = value
            .trim()
            .parse()
            .map_err(|_| format!("field \"{key}\" must be an integer, got {value:?}"))?;
        *slot = Some(parsed);
        Ok(())
    }

    fn set_form(&mut self, form: &[u8]) -> Result<(), String> {
        let fields: Vec<(String, String)> =
            serde_urlencoded::from_bytes(form).map_err(|error| format!("invalid form: {error}"))?;
        fields
            .iter()
            .try_for_each(|(key, value)| self.set(key, value))
    }

    fn set_json(&mut self, body: &[u8]) -> Result<(), String> {
        let Value::Object(fields) =
            serde_json::from_slice(body).map_err(|error| format!("invalid JSON: {error}"))?
        else {
            return Err("the JSON body must be an object".to_owned());
        };
        fields.iter().try_for_each(|(key, value)| match value {
            Value::Null => Ok(()),
            Value::String(text) => self.set(key, text),
            other => self.set(key, &other.to_string()),
        })
    }

    fn clamp(self) -> GetUpdatesParams {
        let max_limit = GET_UPDATES_LIMIT_MAX as i64;
        let max_timeout = GET_UPDATES_TIMEOUT_MAX_SECS as i64;
        GetUpdatesParams {
            offset: self.offset.unwrap_or(0),
            limit: self.limit.map_or(GET_UPDATES_LIMIT_MAX, |limit| {
                limit.clamp(1, max_limit) as usize
            }),
            timeout: Duration::from_secs(
                self.timeout
                    .map_or(0, |timeout| timeout.clamp(0, max_timeout) as u64),
            ),
        }
    }
}

/// Read `getUpdates` arguments the way Telegram does: from the query string
/// and from a JSON, URL-encoded or multipart body. Body fields win.
async fn read_params(request: Request) -> Result<GetUpdatesParams, String> {
    let mut raw = RawParams::default();
    if let Some(query) = request.uri().query() {
        raw.set_form(query.as_bytes())?;
    }
    let content_type = content_type(request.headers());
    if content_type.starts_with("multipart/form-data") {
        let mut multipart = Multipart::from_request(request, &())
            .await
            .map_err(|rejection| rejection.body_text())?;
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|error| error.body_text())?
        {
            let Some(name) = field.name().map(str::to_owned) else {
                continue;
            };
            let value = field.text().await.map_err(|error| error.body_text())?;
            raw.set(&name, &value)?;
        }
    } else {
        let body = to_bytes(request.into_body(), GET_UPDATES_BODY_LIMIT)
            .await
            .map_err(|error| format!("unreadable body: {error}"))?;
        if content_type.starts_with("application/json") && !body.is_empty() {
            raw.set_json(&body)?;
        } else if content_type.starts_with("application/x-www-form-urlencoded") {
            raw.set_form(&body)?;
        }
    }
    Ok(raw.clamp())
}

fn content_type(headers: &HeaderMap) -> String {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// `getUpdates` arguments, clamped like Telegram clamps them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GetUpdatesParams {
    /// Positive: confirm updates below it. Negative: keep only the last
    /// `-offset` updates. Zero: confirm nothing.
    pub offset: i64,
    pub limit: usize,
    pub timeout: Duration,
}

impl Default for GetUpdatesParams {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: GET_UPDATES_LIMIT_MAX,
            timeout: Duration::ZERO,
        }
    }
}

/// The fake Bot API's state: one update stream per token, and the tracker
/// of the current run, which bot replies are matched against.
pub struct MockApi {
    streams: DashMap<String, Arc<UpdateStream>>,
    run: RwLock<Arc<Tracker>>,
}

impl MockApi {
    pub fn new() -> Self {
        Self {
            streams: DashMap::new(),
            run: RwLock::new(Arc::new(Tracker::new())),
        }
    }

    /// The stream of `token`, created on first use.
    pub fn stream(&self, token: &str) -> Arc<UpdateStream> {
        if let Some(stream) = self.streams.get(token) {
            return stream.clone();
        }
        self.streams
            .entry(token.to_owned())
            .or_insert_with(|| Arc::new(UpdateStream::new()))
            .clone()
    }

    /// Start counting a new run from zero.
    pub fn start_run(&self) -> Arc<Tracker> {
        let tracker = Arc::new(Tracker::new());
        *self.run.write().expect(RUN_POISONED) = tracker.clone();
        tracker
    }

    pub fn tracker(&self) -> Arc<Tracker> {
        self.run.read().expect(RUN_POISONED).clone()
    }
}

pub fn router(api: Arc<MockApi>) -> Router {
    Router::new()
        .route("/bot{token}/getUpdates", get(get_updates).post(get_updates))
        .route("/bot{token}/sendMessage", post(send_message))
        .route("/bot{token}/getMe", get(get_me).post(get_me))
        .route(
            "/bot{token}/deleteWebhook",
            get(delete_webhook).post(delete_webhook),
        )
        .with_state(api)
}

async fn get_updates(
    State(api): State<Arc<MockApi>>,
    Path(token): Path<String>,
    request: Request,
) -> Response {
    let params = match read_params(request).await {
        Ok(params) => params,
        Err(description) => return bad_request(&description),
    };
    let batch = api.stream(&token).poll(params).await;
    api.tracker().record_poll(batch.len());
    updates_response(&batch)
}

/// `{"ok":true,"result":[…]}` around the stored payloads, without parsing
/// them again.
fn updates_response(batch: &[Bytes]) -> Response {
    const HEAD: &[u8] = br#"{"ok":true,"result":["#;
    const TAIL: &[u8] = b"]}";
    let len = HEAD.len() + TAIL.len() + batch.iter().map(|update| update.len() + 1).sum::<usize>();
    let mut body = Vec::with_capacity(len);
    body.extend_from_slice(HEAD);
    for (i, update) in batch.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        body.extend_from_slice(update);
    }
    body.extend_from_slice(TAIL);
    ([(CONTENT_TYPE, "application/json")], body).into_response()
}

async fn send_message(
    State(api): State<Arc<MockApi>>,
    headers: HeaderMap,
    body: Bytes,
) -> Json<Value> {
    if let Some(text) = reply_text(&headers, &body) {
        api.tracker().reply(&text);
    }
    Json(json!({
        "ok": true,
        "result": {
            "message_id": 123,
            "date": 0,
            "chat": { "id": 1, "type": "private" },
            "text": "ok"
        }
    }))
}

/// The `text` field of a JSON or URL-encoded `sendMessage` body.
fn reply_text(headers: &HeaderMap, body: &[u8]) -> Option<String> {
    if content_type(headers).starts_with("application/json") {
        let message: Value = serde_json::from_slice(body).ok()?;
        return message.get("text")?.as_str().map(str::to_owned);
    }
    let fields: Vec<(String, String)> = serde_urlencoded::from_bytes(body).ok()?;
    fields
        .into_iter()
        .find_map(|(key, value)| (key == "text").then_some(value))
}

async fn get_me() -> Json<Value> {
    Json(json!({
        "ok": true,
        "result": {
            "id": 123456789,
            "is_bot": true,
            "first_name": "Tgin Bench Bot",
            "username": "bench_bot",
            "can_join_groups": true,
            "can_read_all_group_messages": false,
            "supports_inline_queries": false
        }
    }))
}

async fn delete_webhook() -> Json<Value> {
    Json(json!({ "ok": true, "result": true }))
}

fn bad_request(description: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "ok": false,
            "error_code": 400,
            "description": format!("Bad Request: {description}")
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::send::update_payload;
    use std::time::Instant;

    fn stream_with(ids: impl IntoIterator<Item = i64>) -> UpdateStream {
        let stream = UpdateStream::new();
        for id in ids {
            stream.push(id, update_payload(id as u64, &id.to_string()));
        }
        stream
    }

    struct Fake {
        base: String,
        api: Arc<MockApi>,
        client: reqwest::Client,
    }

    impl Fake {
        async fn start() -> Self {
            let api = Arc::new(MockApi::new());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/bot123:test", listener.local_addr().unwrap());
            let app = router(api.clone());
            tokio::spawn(async move { axum::serve(listener, app).await });
            Self {
                base,
                api,
                client: reqwest::Client::new(),
            }
        }

        fn push(&self, ids: impl IntoIterator<Item = i64>) {
            let stream = self.api.stream("123:test");
            for id in ids {
                stream.push(id, update_payload(id as u64, &id.to_string()));
            }
        }

        async fn ids(&self, request: reqwest::RequestBuilder) -> Vec<i64> {
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), 200);
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["ok"], true);
            body["result"]
                .as_array()
                .unwrap()
                .iter()
                .map(|u| u["update_id"].as_i64().unwrap())
                .collect()
        }

        fn form(&self, body: &'static str) -> reqwest::RequestBuilder {
            self.client
                .post(format!("{}/getUpdates", self.base))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(body)
        }

        fn json(&self, body: Value) -> reqwest::RequestBuilder {
            self.client
                .post(format!("{}/getUpdates", self.base))
                .json(&body)
        }

        fn query(&self, query: &str) -> reqwest::RequestBuilder {
            self.client.get(format!("{}/getUpdates?{query}", self.base))
        }
    }

    #[tokio::test]
    async fn aiogram_style_form_post_offset_confirms_updates_for_good() {
        let fake = Fake::start().await;
        fake.push(1..=5);

        assert_eq!(
            fake.ids(fake.form("offset=3&timeout=0")).await,
            vec![3, 4, 5]
        );
        // 1 and 2 are gone; 3..=5 stay until a higher offset confirms them.
        assert_eq!(fake.ids(fake.form("timeout=0")).await, vec![3, 4, 5]);
        assert_eq!(fake.ids(fake.form("offset=6")).await, Vec::<i64>::new());
        assert_eq!(fake.ids(fake.query("")).await, Vec::<i64>::new());
    }

    #[tokio::test]
    async fn json_post_offset_confirms_updates_as_number_or_string() {
        let fake = Fake::start().await;
        fake.push(1..=5);

        assert_eq!(
            fake.ids(fake.json(json!({ "offset": 2 }))).await,
            vec![2, 3, 4, 5]
        );
        assert_eq!(
            fake.ids(fake.json(json!({ "offset": "4" }))).await,
            vec![4, 5]
        );
        assert_eq!(fake.ids(fake.json(json!({}))).await, vec![4, 5]);
    }

    #[tokio::test]
    async fn multipart_post_offset_confirms_updates() {
        let fake = Fake::start().await;
        fake.push(1..=3);
        let body = "--XyZ\r\nContent-Disposition: form-data; name=\"offset\"\r\n\r\n3\r\n--XyZ\r\nContent-Disposition: form-data; name=\"limit\"\r\n\r\n10\r\n--XyZ--\r\n";

        let request = fake
            .client
            .post(format!("{}/getUpdates", fake.base))
            .header("content-type", "multipart/form-data; boundary=XyZ")
            .body(body);

        assert_eq!(fake.ids(request).await, vec![3]);
        assert_eq!(fake.ids(fake.query("")).await, vec![3]);
    }

    #[tokio::test]
    async fn query_offset_confirms_updates() {
        let fake = Fake::start().await;
        fake.push(1..=3);

        assert_eq!(fake.ids(fake.query("offset=2&timeout=0")).await, vec![2, 3]);
        assert_eq!(fake.ids(fake.query("")).await, vec![2, 3]);
    }

    #[tokio::test]
    async fn body_fields_override_query_fields() {
        let fake = Fake::start().await;
        fake.push(1..=3);
        let request = fake
            .client
            .post(format!("{}/getUpdates?offset=1", fake.base))
            .header("content-type", "application/x-www-form-urlencoded")
            .body("offset=3");

        assert_eq!(fake.ids(request).await, vec![3]);
    }

    #[tokio::test]
    async fn malformed_offset_is_a_bad_request() {
        let fake = Fake::start().await;
        fake.push(1..=3);

        let response = fake.form("offset=abc").send().await.unwrap();

        assert_eq!(response.status(), 400);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(body["error_code"], 400);
        // Nothing was confirmed.
        assert_eq!(fake.ids(fake.query("")).await, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn limit_is_capped_at_100() {
        let fake = Fake::start().await;
        fake.push(1..=150);

        let ids = fake.ids(fake.form("limit=200")).await;

        assert_eq!(ids.len(), GET_UPDATES_LIMIT_MAX);
        assert_eq!((ids[0], ids[99]), (1, 100));
        assert_eq!(
            fake.ids(fake.form("offset=101&limit=10")).await,
            (101..=110).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn empty_poll_waits_out_its_timeout() {
        let fake = Fake::start().await;

        let started = Instant::now();
        let ids = fake.ids(fake.form("timeout=1")).await;
        let elapsed = started.elapsed();

        assert!(ids.is_empty());
        assert!(
            elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(3),
            "waited {elapsed:?}"
        );
        let stats = fake.api.tracker().poll_stats();
        assert_eq!((stats.calls, stats.empty_calls), (1, 1));
    }

    #[tokio::test]
    async fn parked_poll_wakes_when_an_update_arrives() {
        let fake = Fake::start().await;
        let stream = fake.api.stream("123:test");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stream.push(42, update_payload(42, "42"));
        });

        let started = Instant::now();
        let ids = fake.ids(fake.form("timeout=5")).await;

        assert_eq!(ids, vec![42]);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn streams_are_isolated_by_token() {
        let fake = Fake::start().await;
        fake.api.stream("other").push(99, update_payload(99, "99"));
        fake.push([1]);

        assert_eq!(fake.ids(fake.query("")).await, vec![1]);
    }

    #[test]
    fn negative_offset_keeps_only_the_last_updates() {
        let stream = stream_with(1..=5);

        let batch = stream.take(-2, GET_UPDATES_LIMIT_MAX);

        assert_eq!(batch.len(), 2);
        assert_eq!(stream.update_ids(), vec![4, 5]);
    }

    #[test]
    fn zero_offset_confirms_nothing() {
        let stream = stream_with(1..=3);

        assert_eq!(stream.take(0, 2).len(), 2);
        assert_eq!(stream.update_ids(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn send_message_matches_form_and_json_replies() {
        let fake = Fake::start().await;
        let tracker = fake.api.start_run();
        tracker.issued(7, tokio::time::Instant::now());
        tracker.issued(8, tokio::time::Instant::now());

        let form = fake
            .client
            .post(format!("{}/sendMessage", fake.base))
            .header("content-type", "application/x-www-form-urlencoded")
            .body("chat_id=1&text=7")
            .send()
            .await
            .unwrap();
        let json = fake
            .client
            .post(format!("{}/sendMessage", fake.base))
            .json(&json!({ "chat_id": 1, "text": "8" }))
            .send()
            .await
            .unwrap();

        assert_eq!(form.status(), 200);
        assert_eq!(json.status(), 200);
        let reply: Value = json.json().await.unwrap();
        assert_eq!(reply["ok"], true);
        assert_eq!(tracker.counts().received, 2);
    }

    #[tokio::test]
    async fn delete_webhook_succeeds() {
        let fake = Fake::start().await;

        let response = fake
            .client
            .post(format!("{}/deleteWebhook", fake.base))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["result"], true);
    }
}
