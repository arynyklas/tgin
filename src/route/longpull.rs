use crate::base::{Printable, RouteId, Routeable, Serverable};
use crate::utils::defaults::redact;
use async_trait::async_trait;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axum::{
    body::Body,
    extract::DefaultBodyLimit,
    http::{header, HeaderMap},
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
use tokio::time::{timeout_at, Instant};

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct GetUpdatesParams {
    #[serde(default)]
    pub offset: Option<i64>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

/// Default cap on the per-route buffer. tgin confirms each long-poll batch to
/// Telegram with its next `getUpdates`, so Telegram's own retention does not
/// back this buffer: an update in it exists only here. The cap turns a
/// downstream outage from a slow-motion OOM into a loud, observable signal:
/// oldest updates are evicted FIFO and lost, the eviction counter
/// increments, and the operator sees the depth on `/api/routes` and
/// `/status`.
pub const DEFAULT_MAX_BUFFERED_UPDATES: usize = 10_000;

/// Fraction of `max_buffered_updates` at which we log a one-time warning.
/// Matches the threshold a typical operator would set in an external
/// monitor: by 80 % full, you want to know — by 100 % we are already
/// dropping. Sticky once-per-route via `high_watermark_logged`; we do not
/// want one congested route to spam the log every dispatch.
const HIGH_WATERMARK_RATIO_NUM: usize = 4;
const HIGH_WATERMARK_RATIO_DEN: usize = 5;

/// Cap on the `getUpdates` request body for both the statically-mounted
/// route and the dynamic fallback. A `getUpdates` poll carries only
/// offset/timeout/limit (plus an optional small allowed_updates array) —
/// well under 1 KiB in practice. 8 KiB leaves generous headroom while
/// closing the unbounded-body buffering vector the dynamic handler had
/// and keeping both surfaces on one limit.
pub const GET_UPDATES_BODY_LIMIT: usize = 8 * 1024;

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
    /// Cumulative count of updates served out via `getUpdates` responses.
    /// Monotonic; surfaced via [`Routeable::json_struct`].
    served: Arc<AtomicU64>,
    /// Set once by [`Routeable::begin_drain`]: no new updates will arrive,
    /// so a long poll on an empty buffer is held for at most
    /// [`DRAINING_EMPTY_POLL_HOLD`] instead of its full timeout.
    draining: Arc<AtomicBool>,
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
            served: Arc::new(AtomicU64::new(0)),
            draining: Arc::new(AtomicBool::new(false)),
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

        let mut deadline = Instant::now() + effective_timeout(params.timeout);
        let mut drain_hold_applied = false;

        loop {
            // Create the wakeup future before checking the buffer and the
            // drain flag: `notify_waiters` from `begin_drain` reaches every
            // `Notified` created before the call, even one not yet polled,
            // so a drain that lands between the checks and the `await`
            // below is not lost.
            let notified = notify.notified();

            // Sync mutex: every critical section is push_back / pop_front /
            // is_empty. None of them `.await`. Holding a `tokio::sync::Mutex`
            // here scheduled the task on every push and every poll — a heavy
            // primitive for an O(1) operation. With std::sync::Mutex the
            // uncontended path is one CAS; the contended path blocks an OS
            // thread for a few ns, never long enough for tokio to care.
            let batch_opt: Option<Vec<Bytes>> = {
                let mut lock = updates.lock().expect("longpull buffer mutex poisoned");
                if lock.is_empty() {
                    None
                } else {
                    let take = effective_limit(params.limit).min(lock.len());
                    Some(lock.drain(..take).collect())
                }
            };

            if let Some(batch) = batch_opt {
                self.served.fetch_add(batch.len() as u64, Ordering::Relaxed);
                return build_get_updates_response(&batch);
            }

            // Draining: no new updates will arrive once the backlog is
            // through, so cut the hold to `DRAINING_EMPTY_POLL_HOLD` from the
            // moment the drain is seen (a poll parked before `begin_drain`
            // is woken by it and lands here). Still waiting briefly rather
            // than answering at once keeps a re-polling consumer from
            // spinning while other routes drain.
            if !drain_hold_applied && self.draining.load(Ordering::SeqCst) {
                drain_hold_applied = true;
                deadline = deadline.min(Instant::now() + DRAINING_EMPTY_POLL_HOLD);
            }

            if Instant::now() >= deadline {
                return build_get_updates_response(&[]);
            }

            let _ = timeout_at(deadline, notified).await;
        }
    }

    /// Parse the body (JSON or urlencoded) and dispatch to [`handle_request`].
    /// Shared by the static `set_server` route and the dynamic fallback so a
    /// `getUpdates` poll behaves identically regardless of how the route was
    /// added. A malformed request body returns a Telegram-shaped 400 envelope.
    pub async fn handle_get_updates(&self, content_type: &str, body: &[u8]) -> Response {
        match parse_get_updates_params(content_type, body) {
            Ok(params) => self.handle_request(params).await,
            Err(()) => get_updates_error(400, "invalid request body"),
        }
    }
}

/// Telegram's `getUpdates` default and maximum batch size.
const GET_UPDATES_MAX_LIMIT: usize = 100;

/// Upper bound on a long-poll hold: 50 s is the Bot API server's own limit
/// on the `getUpdates` timeout. An unbounded client value would pin a
/// connection and a task indefinitely.
const GET_UPDATES_MAX_TIMEOUT: Duration = Duration::from_secs(50);

/// Longest a long poll on an empty buffer is held once the route is
/// draining: long enough that a consumer re-polling during the drain does
/// not spin, short enough that it is answered before the HTTP plane closes
/// (1 s after the backlog has drained).
const DRAINING_EMPTY_POLL_HOLD: Duration = Duration::from_millis(500);

/// Batch size for a `getUpdates` poll: Telegram's semantics — default 100,
/// clamped to `1..=100`.
fn effective_limit(requested: Option<u64>) -> usize {
    requested.map_or(GET_UPDATES_MAX_LIMIT, |limit| {
        limit.clamp(1, GET_UPDATES_MAX_LIMIT as u64) as usize
    })
}

/// Long-poll hold for a `getUpdates` poll: default 0 (short poll), capped at
/// [`GET_UPDATES_MAX_TIMEOUT`].
fn effective_timeout(requested: Option<u64>) -> Duration {
    Duration::from_secs(requested.unwrap_or(0)).min(GET_UPDATES_MAX_TIMEOUT)
}

/// Parse `getUpdates` params from a raw request body, accepting both
/// `application/json` and `application/x-www-form-urlencoded` — Telegram's
/// Bot API accepts both and the dynamic fallback always has. `Err(())` means a
/// non-empty body that is malformed for its declared content type; an empty body
/// parses to default (all-`None`) params.
fn parse_get_updates_params(content_type: &str, body: &[u8]) -> Result<GetUpdatesParams, ()> {
    if content_type.contains("application/json") {
        serde_json::from_slice(body).map_err(|_| ())
    } else if body.is_empty() {
        // A bodyless getUpdates (Content-Length: 0) is a valid poll with all
        // defaults — never an error.
        Ok(GetUpdatesParams::default())
    } else {
        serde_urlencoded::from_bytes(body).map_err(|_| ())
    }
}

/// Telegram-shaped error envelope (HTTP 200, `{"ok":false,...}` — Bot API
/// convention; the error lives in the JSON body, not the HTTP status),
/// matching `dynamic::handler::error_response`.
fn get_updates_error(error_code: u16, description: &str) -> Response {
    let body = serde_json::to_vec(&json!({
        "ok": false,
        "error_code": error_code,
        "description": description,
    }))
    .expect("static json! never fails to serialize");
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("static headers and Bytes body always build a valid Response")
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
        // reached, evict the oldest entry FIFO before pushing the new one.
        // The evicted update is lost (tgin has already confirmed it to
        // Telegram, which will not redeliver it), but freshness wins over a
        // stalled pipeline. Refusing to accept would require an LB above us
        // to react; today the LBs do not, so a refusal would just bubble up
        // to the worker pool and stall it. Eviction keeps the dispatch hot
        // path moving, and the eviction counter makes the loss visible.
        let mut evicted: u64 = 0;
        let depth_after_push;
        {
            let mut lock = self.updates.lock().expect("longpull buffer mutex poisoned");
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
            tracing::warn!(
                route = %redact(&self.path),
                depth = depth_after_push,
                cap = self.max_buffered_updates,
                "long-poll route crossed buffer high-watermark; downstream may be offline"
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

    fn pending(&self) -> usize {
        self.updates
            .lock()
            .expect("longpull buffer mutex poisoned")
            .len()
    }

    fn begin_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
        // Wake every parked poll (unlike `process`, which wakes one); each
        // re-checks the buffer and, if it is empty, waits at most
        // `DRAINING_EMPTY_POLL_HOLD` more before answering an empty batch.
        self.notify.notify_waiters();
    }
}

#[async_trait]
impl Serverable for LongPollRoute {
    async fn set_server(&self, router: Router<Sender<Bytes>>) -> Router<Sender<Bytes>> {
        let this = self.clone();
        let path = self.path.clone();

        let handler = move |headers: HeaderMap, body: Bytes| {
            let this = this.clone();
            async move {
                let content_type = headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                this.handle_get_updates(content_type, &body).await
            }
        };

        router.route(
            &path,
            post(handler).layer(DefaultBodyLimit::max(GET_UPDATES_BODY_LIMIT)),
        )
    }
}

#[async_trait]
impl Printable for LongPollRoute {
    async fn print(&self) -> String {
        format!("longpull: http://0.0.0.0{}", redact(&self.path))
    }

    async fn json_struct(&self) -> Value {
        let queue_depth = self.updates.lock().map(|lock| lock.len()).unwrap_or(0);
        let queue_dropped_total = self.dropped_oldest.load(Ordering::Relaxed);
        json!({
            "type": "longpoll",
            "options": {
                "path": redact(&self.path),
                "max_buffered_updates": self.max_buffered_updates
            },
            "metrics": {
                "queue_depth": queue_depth,
                "queue_dropped_total": queue_dropped_total,
                "served_total": self.served.load(Ordering::Relaxed)
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
    async fn test_handle_get_updates_rejects_malformed_urlencoded() {
        let route = LongPollRoute::new("/bot/updates".to_string());
        let response = route
            .handle_get_updates("application/x-www-form-urlencoded", b"offset=notanumber")
            .await;
        let body = read_body_json(response).await;
        assert_eq!(body["error_code"], 400);
    }

    #[tokio::test]
    async fn test_handle_get_updates_empty_body_defaults() {
        let route = LongPollRoute::new("/bot/updates".to_string());
        let response = route
            .handle_get_updates("application/x-www-form-urlencoded", b"")
            .await;
        let body = read_body_json(response).await;
        assert_eq!(body["result"].as_array().unwrap().len(), 0);
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

    /// `begin_drain` releases a poll parked on an empty buffer after the
    /// drain hold, instead of holding it for its full timeout. Paused clock:
    /// timers fire in deadline order, so the hold cannot race the bound.
    #[tokio::test(start_paused = true)]
    async fn test_begin_drain_wakes_parked_poll() {
        let route = LongPollRoute::new("/test".to_string());
        let parked = {
            let route = route.clone();
            tokio::spawn(async move {
                route
                    .handle_request(GetUpdatesParams {
                        timeout: Some(30),
                        ..default_params()
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;

        let drain_started = tokio::time::Instant::now();
        route.begin_drain();

        let response = tokio::time::timeout(Duration::from_secs(1), parked)
            .await
            .expect("parked poll not woken by begin_drain")
            .unwrap();
        assert_eq!(drain_started.elapsed(), DRAINING_EMPTY_POLL_HOLD);
        let body = read_body_json(response).await;
        assert_eq!(body["result"].as_array().unwrap().len(), 0);
    }

    /// While draining, buffered updates are still served (with the usual
    /// limit) and `pending` tracks what is left; a long poll on an empty
    /// buffer is then held only for [`DRAINING_EMPTY_POLL_HOLD`], so the
    /// consumer neither waits out its timeout nor spins on empty answers.
    #[tokio::test(start_paused = true)]
    async fn test_draining_serves_buffer_then_holds_empty_poll_briefly() {
        let route = LongPollRoute::new("/test".to_string());
        for i in 0..3 {
            route.process(update_bytes(json!({"id": i}))).await;
        }
        assert_eq!(route.pending(), 3);

        route.begin_drain();

        let long_poll = || GetUpdatesParams {
            timeout: Some(30),
            limit: Some(2),
            ..default_params()
        };
        let body = read_body_json(route.handle_request(long_poll()).await).await;
        assert_eq!(body["result"].as_array().unwrap().len(), 2);
        assert_eq!(route.pending(), 1);
        let body = read_body_json(route.handle_request(long_poll()).await).await;
        assert_eq!(body["result"][0]["id"], 2);
        assert_eq!(route.pending(), 0);

        let start = tokio::time::Instant::now();
        let empty = tokio::time::timeout(Duration::from_secs(2), route.handle_request(long_poll()))
            .await
            .expect("draining poll on an empty buffer must not wait out its timeout");
        assert_eq!(start.elapsed(), DRAINING_EMPTY_POLL_HOLD);
        assert_eq!(
            read_body_json(empty).await["result"]
                .as_array()
                .unwrap()
                .len(),
            0
        );

        // A short poll still answers at once.
        let start = tokio::time::Instant::now();
        let short = route.handle_request(default_params()).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(
            read_body_json(short).await["result"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    /// An update pushed while a draining poll is held is delivered to it.
    #[tokio::test(start_paused = true)]
    async fn test_draining_poll_receives_update_pushed_during_hold() {
        let route = LongPollRoute::new("/test".to_string());
        route.begin_drain();

        let held = {
            let route = route.clone();
            tokio::spawn(async move {
                route
                    .handle_request(GetUpdatesParams {
                        timeout: Some(30),
                        ..default_params()
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        route.process(update_bytes(json!({"id": 7}))).await;

        let response = tokio::time::timeout(DRAINING_EMPTY_POLL_HOLD / 2, held)
            .await
            .expect("held poll not answered")
            .unwrap();
        let body = read_body_json(response).await;
        assert_eq!(body["result"], json!([{ "id": 7 }]));
    }

    #[tokio::test]
    async fn test_default_batch_is_100() {
        let route = LongPollRoute::new("/test".to_string());
        for i in 0..150 {
            route.process(update_bytes(json!({"id": i}))).await;
        }

        let params = GetUpdatesParams {
            limit: None,
            ..default_params()
        };
        let body = read_body_json(route.handle_request(params).await).await;
        let results = body["result"].as_array().unwrap();
        assert_eq!(results.len(), 100);
        assert_eq!(results[0]["id"], 0);
        assert_eq!(results[99]["id"], 99);

        let rest = read_body_json(route.handle_request(default_params()).await).await;
        assert_eq!(rest["result"].as_array().unwrap().len(), 50);
    }

    #[tokio::test]
    async fn test_limit_above_100_clamped() {
        let route = LongPollRoute::new("/test".to_string());
        for i in 0..150 {
            route.process(update_bytes(json!({"id": i}))).await;
        }

        let params = GetUpdatesParams {
            limit: Some(1000),
            ..default_params()
        };
        let body = read_body_json(route.handle_request(params).await).await;
        assert_eq!(body["result"].as_array().unwrap().len(), 100);
    }

    #[tokio::test]
    async fn test_limit_zero_returns_one() {
        let route = LongPollRoute::new("/test".to_string());
        for i in 0..3 {
            route.process(update_bytes(json!({"id": i}))).await;
        }

        let params = GetUpdatesParams {
            limit: Some(0),
            ..default_params()
        };
        let body = read_body_json(route.handle_request(params).await).await;
        let results = body["result"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["id"], 0);
    }

    #[test]
    fn test_timeout_clamped_to_50s() {
        assert_eq!(effective_timeout(None), Duration::ZERO);
        assert_eq!(effective_timeout(Some(0)), Duration::ZERO);
        assert_eq!(effective_timeout(Some(30)), Duration::from_secs(30));
        assert_eq!(effective_timeout(Some(50)), Duration::from_secs(50));
        assert_eq!(effective_timeout(Some(51)), Duration::from_secs(50));
        assert_eq!(effective_timeout(Some(u64::MAX)), Duration::from_secs(50));
    }

    /// Wake property: once a `handle_request(timeout > 0)` is parked on
    /// the empty-queue branch, a subsequent `process()` MUST wake it
    /// promptly with the queued update.
    ///
    /// This covers the post-park notification path -- the dominant case
    /// in production, where bots issue one `getUpdates` and park for
    /// many seconds before any update arrives. It does NOT cover the
    /// race window between dropping the empty-check `Mutex` guard and
    /// registering the `Notify` future awaited by the poll: that
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
    async fn test_set_server_accepts_json_body() {
        let path = "/bot123/getUpdates";
        let route = LongPollRoute::new(path.to_string());

        route.process(update_bytes(json!({"ok": true}))).await;

        let app = Router::new();
        let app = route.set_server(app).await;

        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"timeout":0,"limit":10}"#))
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
            json["options"]["max_buffered_updates"], DEFAULT_MAX_BUFFERED_UPDATES as u64,
            "default cap must be exposed so /routes is self-describing"
        );
        assert_eq!(json["metrics"]["queue_depth"], 0);
        assert_eq!(json["metrics"]["queue_dropped_total"], 0);
        assert_eq!(json["metrics"]["served_total"], 0);
    }

    /// The route path may embed a bot token: `json_struct` (which feeds
    /// `/status` and `/api/routes`), the startup banner and the
    /// high-watermark warning redact it.
    #[tokio::test]
    async fn test_token_path_redacted() {
        const SECRET: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let mut route = LongPollRoute::new(format!("/bot123456789:{SECRET}/getUpdates"));
        route.set_max_buffered_updates(1);

        let json = route.json_struct().await;
        assert_eq!(json["options"]["path"], "/bot#####/getUpdates");

        let banner = route.print().await;
        assert_eq!(banner, "longpull: http://0.0.0.0/bot#####/getUpdates");

        let ((), logs) = crate::utils::testlog::capture_logs(
            route.process(update_bytes(json!({"update_id": 1}))),
        )
        .await;
        assert!(logs.contains("crossed buffer high-watermark"), "{logs}");
        assert!(logs.contains("/bot#####/getUpdates"), "{logs}");
        assert!(!logs.contains(SECRET), "{logs}");
    }

    /// `served_total` counts updates drained out via `getUpdates`.
    #[tokio::test]
    async fn test_served_total_counts_drained_updates() {
        let route = LongPollRoute::new("/served".to_string());
        route.process(update_bytes(json!({"update_id": 1}))).await;
        route.process(update_bytes(json!({"update_id": 2}))).await;

        let response = route.handle_request(default_params()).await;
        let body = read_body_json(response).await;
        assert_eq!(body["result"].as_array().unwrap().len(), 2);

        let json = route.json_struct().await;
        assert_eq!(json["metrics"]["served_total"], 2);
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
