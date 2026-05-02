use crate::base::{Printable, RouteId, Routeable, Serverable};
use async_trait::async_trait;

use std::collections::VecDeque;

use axum::{
    body::Body,
    extract::Form,
    http::header,
    response::Response,
    routing::post,
    Router,
};
use bytes::{Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::sync::Notify;
use tokio::time::timeout as tokio_timeout;

#[derive(Serialize, Deserialize, Debug)]
pub struct GetUpdatesParams {
    #[serde(default)]
    pub offset: Option<i64>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Clone)]
pub struct LongPollRoute {
    /// Each entry is one update's wire JSON. We buffer raw bytes so the
    /// `getUpdates` response can be assembled without re-serializing
    /// individual updates through `serde_json::Value`.
    updates: Arc<Mutex<VecDeque<Bytes>>>,
    notify: Arc<Notify>,
    pub path: String,
}

impl LongPollRoute {
    pub fn new(path: String) -> Self {
        Self {
            updates: Arc::new(Mutex::new(VecDeque::new())),
            notify: Arc::new(Notify::new()),
            path,
        }
    }

    pub async fn handle_request(&self, params: GetUpdatesParams) -> Response {
        let updates = self.updates.clone();
        let notify = self.notify.clone();

        let timeout_sec = params.timeout.unwrap_or(0);
        let start_time = tokio::time::Instant::now();
        let duration = Duration::from_secs(timeout_sec);

        loop {
            // Sync mutex: every critical section is push_back / pop_front /
            // is_empty. None of them `.await`. Holding a `tokio::sync::Mutex`
            // here scheduled the task on every push and every poll — a heavy
            // primitive for an O(1) operation. With std::sync::Mutex the
            // uncontended path is one CAS; the contended path blocks an OS
            // thread for a few ns, never long enough for tokio to care.
            let batch_opt: Option<Vec<Bytes>> = {
                let mut lock = updates
                    .lock()
                    .expect("longpull buffer mutex poisoned");
                if lock.is_empty() {
                    None
                } else {
                    let limit = params.limit.unwrap_or(1000) as usize;
                    let take = limit.min(lock.len());
                    Some(lock.drain(..take).collect())
                }
            };

            if let Some(batch) = batch_opt {
                return build_get_updates_response(&batch);
            }

            if timeout_sec == 0 || start_time.elapsed() >= duration {
                return build_get_updates_response(&[]);
            }

            let remaining = duration.saturating_sub(start_time.elapsed());
            let _ = tokio_timeout(remaining, notify.notified()).await;
        }
    }
}

/// Hand-build `{"ok":true,"result":[<raw1>,<raw2>,...]}` directly from the
/// buffered update bytes. Avoids both an intermediate `Value` tree and a
/// `serde_json::to_vec` pass over per-update JSON we already have on hand.
fn build_get_updates_response(batch: &[Bytes]) -> Response {
    const PREFIX: &[u8] = b"{\"ok\":true,\"result\":[";
    const SUFFIX: &[u8] = b"]}";

    let mut len = PREFIX.len() + SUFFIX.len();
    for (i, b) in batch.iter().enumerate() {
        if i > 0 {
            len += 1; // comma
        }
        len += b.len();
    }

    let mut buf = BytesMut::with_capacity(len);
    buf.extend_from_slice(PREFIX);
    for (i, b) in batch.iter().enumerate() {
        if i > 0 {
            buf.extend_from_slice(b",");
        }
        buf.extend_from_slice(b);
    }
    buf.extend_from_slice(SUFFIX);

    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(buf.freeze()))
        .expect("static headers and Bytes body always build a valid Response")
}

#[async_trait]
impl Routeable for LongPollRoute {
    async fn process(&self, update: Bytes) {
        // Push and notify must NOT both run while the buffer mutex is held.
        // The poller wakes from `notify.notified()` and immediately tries to
        // lock the same mutex; if `notify_one` fires before we drop the
        // guard, the poller is contended for the duration of this critical
        // section. Drop the lock first, then notify.
        {
            let mut lock = self
                .updates
                .lock()
                .expect("longpull buffer mutex poisoned");
            lock.push_back(update);
        }
        // `notify_one` instead of `notify_waiters` to close the lost-wakeup
        // window in `handle_request`: between dropping the empty-check lock
        // and registering the `Notify` future, a `notify_waiters` call from a
        // concurrent `process` would have no waiter to wake and the signal
        // would be silently dropped, parking the consumer for up to its full
        // `timeout=...` deadline. `notify_one` stores a permit when no
        // waiter is registered, so the next `notified().await` consumes it
        // immediately. Coalescing of multiple notifications into a single
        // permit is fine because the consumer always re-checks `is_empty`
        // after wake.
        //
        // Caveat: with multiple concurrent pollers on the same route
        // (atypical — aiogram opens one in-flight `getUpdates` per bot),
        // only one wakes per push. Unwoken pollers see the next push
        // instead. No update is lost; the queue is drained FIFO regardless
        // of which poller wakes.
        self.notify.notify_one();
    }

    fn id(&self) -> Option<RouteId> {
        Some(RouteId::Path(self.path.clone()))
    }
}

#[async_trait]
impl Serverable for LongPollRoute {
    async fn set_server(&self, router: Router<Sender<Bytes>>) -> Router<Sender<Bytes>> {
        let this = self.clone();
        let path = self.path.clone();

        let handler = move |Form(params): Form<GetUpdatesParams>| {
            let this = this.clone();

            async move { this.handle_request(params).await }
        };

        router.route(&path, post(handler))
    }
}

#[async_trait]
impl Printable for LongPollRoute {
    async fn print(&self) -> String {
        format!("longpull: http://0.0.0.0{}", self.path)
    }

    async fn json_struct(&self) -> Value {
        json!({
            "type": "longpoll",
            "options": {
                "path": self.path
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    fn default_params() -> GetUpdatesParams {
        GetUpdatesParams {
            offset: None,
            timeout: Some(0),
            limit: Some(100),
        }
    }

    fn update_bytes(value: Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&value).unwrap())
    }

    async fn read_body_json(response: Response) -> Value {
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body_bytes).unwrap()
    }

    #[tokio::test]
    async fn test_basic_process_and_retrieve() {
        let route = LongPollRoute::new("/bot/updates".to_string());

        route.process(update_bytes(json!({"update_id": 1}))).await;
        route.process(update_bytes(json!({"update_id": 2}))).await;

        let response = route.handle_request(default_params()).await;
        let body = read_body_json(response).await;
        let results = body.get("result").unwrap().as_array().unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["update_id"], 1);
        assert_eq!(results[1]["update_id"], 2);
    }

    #[tokio::test]
    async fn test_limit_batching() {
        let route = LongPollRoute::new("/test".to_string());

        for i in 0..10 {
            route.process(update_bytes(json!({"id": i}))).await;
        }

        let params = GetUpdatesParams {
            limit: Some(4),
            ..default_params()
        };

        let response = route.handle_request(params).await;
        let body = read_body_json(response).await;
        let results = body.get("result").unwrap().as_array().unwrap();

        assert_eq!(results.len(), 4);
        assert_eq!(results[0]["id"], 0);
        assert_eq!(results[3]["id"], 3);

        let remaining = route.handle_request(default_params()).await;
        let rem_body = read_body_json(remaining).await;
        assert_eq!(rem_body["result"].as_array().unwrap().len(), 6);
    }

    #[tokio::test]
    async fn test_timeout_empty_queue() {
        let route = LongPollRoute::new("/test".to_string());

        let start = tokio::time::Instant::now();

        let params = GetUpdatesParams {
            timeout: Some(1),
            ..default_params()
        };

        let response = route.handle_request(params).await;
        let duration = start.elapsed();

        let body = read_body_json(response).await;
        let results = body.get("result").unwrap().as_array().unwrap();

        assert_eq!(results.len(), 0);
        assert!(duration.as_millis() >= 1000);
    }

    /// Wake property: once a `handle_request(timeout > 0)` is parked on
    /// the empty-queue branch, a subsequent `process()` MUST wake it
    /// promptly with the queued update.
    ///
    /// This covers the post-park notification path -- the dominant case
    /// in production, where bots issue one `getUpdates` and park for
    /// many seconds before any update arrives. It does NOT cover the
    /// race window between dropping the empty-check `Mutex` guard and
    /// registering the `Notify` future inside `tokio_timeout`: that
    /// window is ~50 ns of synchronous Rust and is too narrow to hit by
    /// scheduling alone, even with thousands of trial iterations on a
    /// multi-thread runtime. The fix for that race is the choice of
    /// `notify_one` over `notify_waiters` in `process` (see the comment
    /// there); a deterministic test would require production-code
    /// instrumentation we are not willing to add for a one-line fix.
    #[tokio::test]
    async fn test_longpoll_notification() {
        let route = Arc::new(LongPollRoute::new("/test".to_string()));
        let route_clone = route.clone();

        let handle = tokio::spawn(async move {
            let params = GetUpdatesParams {
                timeout: Some(5),
                ..default_params()
            };
            route_clone.handle_request(params).await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        route.process(update_bytes(json!({"msg": "hello"}))).await;
        let response = handle.await.unwrap();
        let body = read_body_json(response).await;
        let results = body.get("result").unwrap().as_array().unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["msg"], "hello");
    }

    #[tokio::test]
    async fn test_axum_server_integration() {
        let path = "/bot123/getUpdates";
        let route = LongPollRoute::new(path.to_string());

        route.process(update_bytes(json!({"ok": true}))).await;

        let app = Router::new();
        let app = route.set_server(app).await;

        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("timeout=0&limit=10"))
            .unwrap();

        let (tx, _rx) = tokio::sync::mpsc::channel::<Bytes>(1);
        let app = app.with_state(tx);

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: Value = serde_json::from_slice(&body_bytes).unwrap();

        assert_eq!(body_json["result"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_printable_json_struct() {
        let route = LongPollRoute::new("/my/path".to_string());
        let json = route.json_struct().await;

        assert_eq!(json["type"], "longpoll");
        assert_eq!(json["options"]["path"], "/my/path");
    }

    /// `build_get_updates_response` must always emit a syntactically valid
    /// JSON envelope, even with zero updates and with multiple updates that
    /// must be comma-separated.
    #[tokio::test]
    async fn test_response_envelope_is_valid_json() {
        let empty = build_get_updates_response(&[]);
        let body = read_body_json(empty).await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["result"].as_array().unwrap().len(), 0);

        let two = build_get_updates_response(&[
            Bytes::from_static(br#"{"update_id":1}"#),
            Bytes::from_static(br#"{"update_id":2}"#),
        ]);
        let body = read_body_json(two).await;
        let arr = body["result"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["update_id"], 1);
        assert_eq!(arr[1]["update_id"], 2);
    }
}
