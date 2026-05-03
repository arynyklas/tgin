use crate::base::{Printable, RouteId, Routeable, Serverable};
use async_trait::async_trait;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

/// Default cap on the per-route buffer. Telegram already buffers ~24 h of
/// undelivered updates server-side and re-delivers them via `getUpdates` once
/// the consuming bot returns, so tgin does not need to be authoritative for
/// downstreams that go offline. The cap exists to turn a downstream outage
/// from a slow-motion OOM (see `longpoll-route-buffer-unbounded`) into a
/// loud, observable signal: oldest updates are evicted FIFO, the eviction
/// counter increments, and the operator sees the depth on `/routes`.
pub const DEFAULT_MAX_BUFFERED_UPDATES: usize = 10_000;

/// Fraction of `max_buffered_updates` at which we log a one-time warning.
/// Matches the threshold a typical operator would set in an external
/// monitor: by 80 % full, you want to know — by 100 % we are already
/// dropping. Sticky once-per-route via `high_watermark_logged`; we do not
/// want one congested route to spam the log every dispatch.
const HIGH_WATERMARK_RATIO_NUM: usize = 4;
const HIGH_WATERMARK_RATIO_DEN: usize = 5;

#[derive(Clone)]
pub struct LongPollRoute {
    /// Each entry is one update's wire JSON. We buffer raw bytes so the
    /// `getUpdates` response can be assembled without re-serializing
    /// individual updates through `serde_json::Value`.
    updates: Arc<Mutex<VecDeque<Bytes>>>,
    notify: Arc<Notify>,
    pub path: String,
    /// Hard cap on `updates.len()`. When `process` would push past this,
    /// the oldest entries are evicted FIFO until there is room for the
    /// new one and `dropped_oldest` is incremented by the number evicted.
    /// Always `>= 1` (clamped in [`set_max_buffered_updates`]).
    max_buffered_updates: usize,
    /// Cumulative count of updates evicted due to overflow. Surfaced via
    /// [`Routeable::json_struct`] so the management API `/routes` endpoint
    /// reports it. Monotonic; never decreases.
    dropped_oldest: Arc<AtomicU64>,
    /// Sticky one-shot: set the first time `updates.len()` reaches the
    /// high-watermark threshold after a push. Prevents log spam when a
    /// route stays hot.
    high_watermark_logged: Arc<AtomicBool>,
}

impl LongPollRoute {
    /// Construct a route with the default buffer cap
    /// ([`DEFAULT_MAX_BUFFERED_UPDATES`]). Override with
    /// [`set_max_buffered_updates`] before the route is mounted.
    pub fn new(path: String) -> Self {
        Self {
            updates: Arc::new(Mutex::new(VecDeque::new())),
            notify: Arc::new(Notify::new()),
            path,
            max_buffered_updates: DEFAULT_MAX_BUFFERED_UPDATES,
            dropped_oldest: Arc::new(AtomicU64::new(0)),
            high_watermark_logged: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Set the hard cap on buffer depth. Values below `1` are clamped to
    /// `1`: a cap of `0` would silently drop every dispatched update,
    /// turning the route into a black hole no operator would ask for.
    pub fn set_max_buffered_updates(&mut self, cap: usize) {
        self.max_buffered_updates = cap.max(1);
    }

    /// High-watermark threshold (`floor(cap * 4 / 5)`, with a `1` floor for
    /// tiny caps so the warning still fires). Pulled into a method so the
    /// computation is shared between `process` and tests.
    fn high_watermark(&self) -> usize {
        let raw = self
            .max_buffered_updates
            .saturating_mul(HIGH_WATERMARK_RATIO_NUM)
            / HIGH_WATERMARK_RATIO_DEN;
        raw.max(1)
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
        //
        // The buffer is bounded by `max_buffered_updates`. When the cap is
        // reached, evict the oldest entry FIFO before pushing the new one:
        // freshness wins because Telegram already buffers undelivered
        // updates server-side, so `LongPollRoute` is best-effort recovery,
        // not the system of record. Refusing to accept (the alternative
        // strategy in the recommended-fix doc) would require an LB above
        // us to react — today the LBs do not, so a refusal would just
        // bubble up to the worker pool and stall it. Eviction keeps the
        // dispatch hot path moving.
        let mut evicted: u64 = 0;
        let depth_after_push;
        {
            let mut lock = self
                .updates
                .lock()
                .expect("longpull buffer mutex poisoned");
            // Loop, not single pop: `set_max_buffered_updates` may have
            // shrunk the cap below the current depth between pushes.
            while lock.len() >= self.max_buffered_updates {
                lock.pop_front();
                evicted += 1;
            }
            lock.push_back(update);
            depth_after_push = lock.len();
        }
        if evicted > 0 {
            // `Relaxed` is sufficient: the counter is observed only via
            // `json_struct`, which has no happens-before relationship with
            // the dispatch hot path beyond "some prior process call".
            self.dropped_oldest.fetch_add(evicted, Ordering::Relaxed);
        }
        // High-watermark warning. Sticky once-per-route via
        // `compare_exchange` so a hot route cannot spam the log. The check
        // runs after the push so the threshold reflects the depth a poller
        // would actually observe.
        if depth_after_push >= self.high_watermark()
            && self
                .high_watermark_logged
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            eprintln!(
                "warning: long-poll route {} crossed buffer high-watermark ({}/{} updates buffered); downstream may be offline",
                self.path, depth_after_push, self.max_buffered_updates
            );
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
        let queue_depth = self
            .updates
            .lock()
            .map(|lock| lock.len())
            .unwrap_or(0);
        let queue_dropped_total = self.dropped_oldest.load(Ordering::Relaxed);
        json!({
            "type": "longpoll",
            "options": {
                "path": self.path,
                "max_buffered_updates": self.max_buffered_updates
            },
            "metrics": {
                "queue_depth": queue_depth,
                "queue_dropped_total": queue_dropped_total
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
        assert_eq!(
            json["options"]["max_buffered_updates"],
            DEFAULT_MAX_BUFFERED_UPDATES as u64,
            "default cap must be exposed so /routes is self-describing"
        );
        assert_eq!(json["metrics"]["queue_depth"], 0);
        assert_eq!(json["metrics"]["queue_dropped_total"], 0);
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

    /// The unbounded buffer was the OOM vector this fix exists to close.
    /// With the cap in place, pushing past it MUST evict oldest-first and
    /// MUST increment the dropped counter; the kept entries MUST be the
    /// most recent ones in FIFO order.
    #[tokio::test]
    async fn test_process_evicts_oldest_at_cap() {
        let mut route = LongPollRoute::new("/cap".to_string());
        route.set_max_buffered_updates(3);

        for i in 0..5 {
            route.process(update_bytes(json!({"update_id": i}))).await;
        }

        let json = route.json_struct().await;
        assert_eq!(json["metrics"]["queue_depth"], 3);
        assert_eq!(
            json["metrics"]["queue_dropped_total"], 2,
            "two oldest updates (id 0 and 1) must have been evicted"
        );

        let response = route.handle_request(default_params()).await;
        let body = read_body_json(response).await;
        let results = body["result"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        // Survivors are the three newest, in FIFO order.
        assert_eq!(results[0]["update_id"], 2);
        assert_eq!(results[1]["update_id"], 3);
        assert_eq!(results[2]["update_id"], 4);

        // Counters must persist after the drain -- they are cumulative,
        // not a snapshot of the current queue.
        let json = route.json_struct().await;
        assert_eq!(json["metrics"]["queue_depth"], 0);
        assert_eq!(json["metrics"]["queue_dropped_total"], 2);
    }

    /// `set_max_buffered_updates(0)` would silently turn the route into a
    /// black hole (every push immediately evicts itself). Clamp to `1`.
    #[tokio::test]
    async fn test_set_max_buffered_updates_clamps_to_one() {
        let mut route = LongPollRoute::new("/zero".to_string());
        route.set_max_buffered_updates(0);

        route.process(update_bytes(json!({"update_id": 1}))).await;
        route.process(update_bytes(json!({"update_id": 2}))).await;

        let json = route.json_struct().await;
        assert_eq!(
            json["options"]["max_buffered_updates"], 1,
            "cap of 0 must clamp to 1 -- a black-hole route is never the operator's intent"
        );
        assert_eq!(json["metrics"]["queue_depth"], 1);
        assert_eq!(json["metrics"]["queue_dropped_total"], 1);

        let response = route.handle_request(default_params()).await;
        let body = read_body_json(response).await;
        let results = body["result"].as_array().unwrap();
        // The newest survives.
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["update_id"], 2);
    }

    /// Shrinking the cap mid-flight (config reload) MUST evict down to the
    /// new cap on the very next push, not silently leave a too-deep buffer.
    #[tokio::test]
    async fn test_shrunk_cap_evicts_on_next_push() {
        let mut route = LongPollRoute::new("/shrink".to_string());
        route.set_max_buffered_updates(10);

        for i in 0..10 {
            route.process(update_bytes(json!({"update_id": i}))).await;
        }
        let json = route.json_struct().await;
        assert_eq!(json["metrics"]["queue_depth"], 10);
        assert_eq!(json["metrics"]["queue_dropped_total"], 0);

        // Shrink and push one more -- a single push must drain the queue
        // back down to the new cap, not just evict one slot.
        route.set_max_buffered_updates(3);
        route.process(update_bytes(json!({"update_id": 99}))).await;

        let json = route.json_struct().await;
        assert_eq!(json["metrics"]["queue_depth"], 3);
        assert_eq!(
            json["metrics"]["queue_dropped_total"], 8,
            "shrinking from 10 to 3 then pushing one must evict 8 (10 - 3 + 1)"
        );
    }
}
