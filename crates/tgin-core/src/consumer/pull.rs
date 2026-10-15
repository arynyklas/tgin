//! A consumer that downstream bots poll, `getUpdates`-style.
//!
//! Each poll acks by offset, gets back what its instance still holds, and
//! otherwise leases new updates, waiting for them up to its timeout. Leases
//! an instance neither acks nor renews return to the queue on expiry.

use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tgin_runtime::queue::{HolderId, Lease, LeaseQueue, QueueConfig};
use tokio::sync::Notify;
use tokio::time::{Instant, MissedTickBehavior, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::envelope::Envelope;
use crate::metrics::ConsumerMetrics;

/// Most updates one poll may ask for.
const MAX_LIMIT: usize = 100;
/// Longest wait one poll may ask for.
const MAX_TIMEOUT: Duration = Duration::from_secs(50);
/// Longest an empty poll waits once the consumer drains.
const DRAIN_HOLD: Duration = Duration::from_millis(500);
/// How often expired leases return to the queue.
const EXPIRY_PERIOD: Duration = Duration::from_secs(1);

/// One poll from a downstream bot instance. The HTTP layer clamps `limit`
/// to 1..=100 and `timeout` to at most 50 s.
#[derive(Debug, Clone)]
pub struct PollRequest {
    pub instance: HolderId,
    /// Acks the instance's updates below it when positive, all of them when
    /// negative.
    pub offset: i64,
    pub limit: usize,
    pub timeout: Duration,
}

/// The payloads handed to the instance, in queue order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PollResponse {
    pub items: Vec<Bytes>,
}

impl PollResponse {
    fn from_leases(leases: Vec<Lease<Envelope>>) -> Self {
        Self {
            items: leases.into_iter().map(|l| l.item.payload).collect(),
        }
    }
}

pub struct PullConsumer {
    name: Arc<str>,
    queue: LeaseQueue<Envelope>,
    metrics: ConsumerMetrics,
    draining: AtomicBool,
    /// Wakes parked polls when the drain begins.
    drain_started: Notify,
}

impl PullConsumer {
    pub fn new(name: Arc<str>, cfg: QueueConfig, metrics: ConsumerMetrics) -> Self {
        Self {
            name,
            queue: LeaseQueue::new(cfg),
            metrics,
            draining: AtomicBool::new(false),
            drain_started: Notify::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Acks by `offset`, then returns the oldest `limit` updates the instance
    /// still holds from `offset` on, else leases up to `limit` new updates,
    /// waiting for them until the timeout, `cancel`, or the drain hold ends
    /// the wait.
    pub async fn poll(&self, req: PollRequest, cancel: &CancellationToken) -> PollResponse {
        debug_assert!(
            (1..=MAX_LIMIT).contains(&req.limit),
            "poll limit {} outside 1..={MAX_LIMIT}",
            req.limit
        );
        debug_assert!(
            req.timeout <= MAX_TIMEOUT,
            "poll timeout {:?} above {MAX_TIMEOUT:?}",
            req.timeout
        );
        let start = Instant::now();
        self.ack(&req.instance, req.offset, start);
        let mut held = self.queue.held(&req.instance, req.offset, start);
        // The rest stay leased and come back on a later poll.
        held.truncate(req.limit);
        if !held.is_empty() {
            self.update_depth();
            return PollResponse::from_leases(held);
        }

        let deadline = start.checked_add(req.timeout).unwrap_or(start);
        let mut drain_deadline = None;
        loop {
            // Enabled before looking at the queue, so no wake-up is lost.
            let mut changed = pin!(self.queue.notified());
            changed.as_mut().enable();
            let mut drain_started = pin!(self.drain_started.notified());
            drain_started.as_mut().enable();

            let now = Instant::now();
            let leased = self.queue.lease(&req.instance, req.limit, now);
            if !leased.is_empty() {
                self.update_depth();
                return PollResponse::from_leases(leased);
            }
            let mut until = deadline;
            if self.draining.load(Ordering::SeqCst) {
                let hold = *drain_deadline.get_or_insert_with(|| now + DRAIN_HOLD);
                until = until.min(hold);
            }
            if now >= until {
                break;
            }
            tokio::select! {
                () = &mut changed => {}
                () = &mut drain_started => {}
                () = sleep_until(until) => break,
                () = cancel.cancelled() => break,
            }
        }
        self.update_depth();
        PollResponse::default()
    }

    pub fn queue(&self) -> &LeaseQueue<Envelope> {
        &self.queue
    }

    /// From now on a poll on an empty queue answers within 500 ms; parked
    /// polls re-evaluate their wait with that cap.
    pub fn begin_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
        self.drain_started.notify_waiters();
    }

    /// Returns expired leases to the queue every second until `cancel`.
    pub fn spawn_expiry(self: &Arc<Self>, cancel: CancellationToken) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval_at(Instant::now() + EXPIRY_PERIOD, EXPIRY_PERIOD);
            ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = ticks.tick() => {
                        this.queue.expire(Instant::now());
                        this.update_depth();
                    }
                }
            }
        });
    }

    /// Acks the instance's leases by `offset`, recording each one's latency
    /// since it was received.
    fn ack(&self, instance: &HolderId, offset: i64, now: Instant) {
        let record = |envelope: &Envelope| {
            let latency = now.saturating_duration_since(envelope.received_at);
            self.metrics.latency(latency.as_secs_f64());
        };
        let acked = match offset {
            0 => 0,
            o if o > 0 => self.queue.ack_below_each(instance, o, record),
            _ => self.queue.ack_all_each(instance, record),
        };
        if acked > 0 {
            self.update_depth();
        }
    }

    fn update_depth(&self) {
        let stats = self.queue.stats();
        let gauge = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        self.metrics
            .set_depth(gauge(stats.available), gauge(stats.leased), 0);
    }
}

#[cfg(test)]
mod tests {
    use prometheus_client::encoding::text::encode;
    use prometheus_client::registry::Registry;
    use tgin_runtime::queue::{OnFull, Ordering};
    use tokio::time::{Instant, sleep};

    use super::*;
    use crate::ids::SourceId;
    use crate::metrics::CoreMetrics;

    fn config(lease_timeout: Duration, ordering: Ordering) -> QueueConfig {
        QueueConfig {
            capacity: 1000,
            capacity_bytes: 1 << 20,
            lease_timeout,
            ordering,
            on_full: OnFull::Backpressure,
        }
    }

    fn consumer_with(cfg: QueueConfig) -> (Registry, Arc<PullConsumer>) {
        let mut reg = Registry::with_prefix("tgin");
        let metrics = CoreMetrics::register(&mut reg).consumer("q");
        (
            reg,
            Arc::new(PullConsumer::new(Arc::from("q"), cfg, metrics)),
        )
    }

    fn consumer() -> (Registry, Arc<PullConsumer>) {
        consumer_with(config(Duration::from_secs(60), Ordering::None))
    }

    fn payload(update_id: i64) -> Bytes {
        Bytes::from(format!("{{\"update_id\":{update_id}}}"))
    }

    fn admit(c: &PullConsumer, update_id: i64, key: Option<u64>) {
        let payload = payload(update_id);
        let permit = c.queue().try_reserve(payload.len()).expect("room");
        let envelope = Envelope {
            source: SourceId(0),
            update_id,
            update_type: Bytes::from_static(b"message"),
            key,
            payload,
            received_at: Instant::now(),
        };
        c.queue()
            .commit(permit, envelope, key, update_id)
            .expect("own permit");
    }

    fn req(instance: &str, offset: i64, limit: usize, timeout: Duration) -> PollRequest {
        PollRequest {
            instance: HolderId(Arc::from(instance)),
            offset,
            limit,
            timeout,
        }
    }

    fn ids(resp: &PollResponse) -> Vec<i64> {
        resp.items
            .iter()
            .map(|p| {
                let v: serde_json::Value = serde_json::from_slice(p).expect("json");
                v["update_id"].as_i64().expect("update_id")
            })
            .collect()
    }

    fn text(reg: &Registry) -> String {
        let mut s = String::new();
        encode(&mut s, reg).expect("encode");
        s
    }

    async fn poll(c: &PullConsumer, r: PollRequest) -> Vec<i64> {
        ids(&c.poll(r, &CancellationToken::new()).await)
    }

    const NOW: Duration = Duration::ZERO;

    #[tokio::test(start_paused = true)]
    async fn poll_returns_admitted_updates() {
        let (_reg, c) = consumer();
        admit(&c, 1, None);
        admit(&c, 2, None);
        assert_eq!(poll(&c, req("a", 0, 100, NOW)).await, [1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn poll_ack_by_offset_removes_only_acked() {
        let (reg, c) = consumer();
        for id in 1..=3 {
            admit(&c, id, None);
        }
        assert_eq!(poll(&c, req("a", 0, 100, NOW)).await, [1, 2, 3]);
        sleep(Duration::from_millis(3)).await;
        // Offset 3 acks 1 and 2; 3 is still held, so it comes back.
        assert_eq!(poll(&c, req("a", 3, 100, NOW)).await, [3]);
        let stats = c.queue().stats();
        assert_eq!((stats.available, stats.leased), (0, 1));

        let out = text(&reg);
        for want in [
            r#"tgin_consumer_queue_depth{consumer="q"} 0"#,
            r#"tgin_consumer_leased{consumer="q"} 1"#,
            r#"tgin_consumer_in_flight{consumer="q"} 0"#,
            r#"tgin_delivery_latency_seconds_count{consumer="q"} 2"#,
            r#"tgin_delivery_latency_seconds_bucket{le="0.002",consumer="q"} 0"#,
            r#"tgin_delivery_latency_seconds_bucket{le="0.004",consumer="q"} 2"#,
        ] {
            assert!(out.lines().any(|l| l == want), "missing {want:?} in\n{out}");
        }
        // Pollers are not endpoints: a pull consumer counts no attempts.
        assert!(!out.contains("tgin_delivery_attempts_total{"), "{out}");
    }

    #[tokio::test(start_paused = true)]
    async fn poll_same_offset_redelivers_held() {
        let (_reg, c) = consumer();
        admit(&c, 1, None);
        admit(&c, 2, None);
        assert_eq!(poll(&c, req("a", 0, 100, NOW)).await, [1, 2]);
        admit(&c, 3, None);
        // The response was lost: the same offset gets the same updates.
        assert_eq!(poll(&c, req("a", 0, 100, NOW)).await, [1, 2]);
        assert_eq!(poll(&c, req("a", 2, 100, NOW)).await, [2]);
        assert_eq!(poll(&c, req("a", 3, 100, NOW)).await, [3]);
    }

    #[tokio::test(start_paused = true)]
    async fn poll_redelivery_capped_at_limit() {
        let (_reg, c) = consumer();
        for id in 1..=3 {
            admit(&c, id, None);
        }
        assert_eq!(poll(&c, req("a", 0, 100, NOW)).await, [1, 2, 3]);
        // Held updates come back oldest first, at most `limit` of them; the
        // rest stay leased to the instance.
        assert_eq!(poll(&c, req("a", 0, 2, NOW)).await, [1, 2]);
        let stats = c.queue().stats();
        assert_eq!((stats.available, stats.leased), (0, 3));
        assert_eq!(poll(&c, req("a", 2, 1, NOW)).await, [2]);
        assert_eq!(poll(&c, req("a", 3, 1, NOW)).await, [3]);
        assert!(poll(&c, req("b", 0, 100, NOW)).await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn poll_negative_offset_acks_all() {
        let (reg, c) = consumer();
        admit(&c, 1, None);
        admit(&c, 2, None);
        assert_eq!(poll(&c, req("a", 0, 100, NOW)).await, [1, 2]);
        assert!(poll(&c, req("a", -1, 100, NOW)).await.is_empty());
        let stats = c.queue().stats();
        assert_eq!((stats.available, stats.leased), (0, 0));
        let out = text(&reg);
        assert!(
            out.lines()
                .any(|l| l == r#"tgin_delivery_latency_seconds_count{consumer="q"} 2"#),
            "{out}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn poll_two_instances_never_share_an_update() {
        let (_reg, c) = consumer();
        for id in 1..=4 {
            admit(&c, id, None);
        }
        assert_eq!(poll(&c, req("a", 0, 2, NOW)).await, [1, 2]);
        assert_eq!(poll(&c, req("b", 0, 100, NOW)).await, [3, 4]);
        assert_eq!(poll(&c, req("b", 0, 100, NOW)).await, [3, 4]);
        // An offset acks only the poller's own leases.
        assert_eq!(poll(&c, req("b", 100, 100, NOW)).await, Vec::<i64>::new());
        assert_eq!(poll(&c, req("a", 0, 100, NOW)).await, [1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn poll_lease_expiry_redelivers_to_other_instance() {
        let (reg, c) = consumer_with(config(Duration::from_secs(2), Ordering::None));
        let cancel = CancellationToken::new();
        c.spawn_expiry(cancel.clone());
        admit(&c, 1, None);
        assert_eq!(poll(&c, req("a", 0, 100, NOW)).await, [1]);
        assert!(poll(&c, req("b", 0, 100, NOW)).await.is_empty());
        sleep(Duration::from_secs(3)).await;
        let out = text(&reg);
        assert!(
            out.lines()
                .any(|l| l == r#"tgin_consumer_queue_depth{consumer="q"} 1"#),
            "the expiry tick updates the gauges:\n{out}"
        );
        assert_eq!(poll(&c, req("b", 0, 100, NOW)).await, [1]);
        // `a` acking by offset no longer touches the update `b` holds.
        assert!(poll(&c, req("a", 2, 100, NOW)).await.is_empty());
        assert_eq!(poll(&c, req("b", 0, 100, NOW)).await, [1]);
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_task_stops_on_cancel() {
        let (_reg, c) = consumer();
        let cancel = CancellationToken::new();
        c.spawn_expiry(cancel.clone());
        assert_eq!(Arc::strong_count(&c), 2);
        cancel.cancel();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(Arc::strong_count(&c), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn poll_waits_until_timeout_then_empty() {
        let (_reg, c) = consumer();
        let start = Instant::now();
        assert!(
            poll(&c, req("a", 0, 100, Duration::from_secs(5)))
                .await
                .is_empty()
        );
        assert_eq!(start.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_wakes_on_commit() {
        let (_reg, c) = consumer();
        let start = Instant::now();
        let (got, ()) = tokio::join!(poll(&c, req("a", 0, 100, Duration::from_secs(30))), async {
            sleep(Duration::from_secs(1)).await;
            admit(&c, 7, None);
        });
        assert_eq!(got, [7]);
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_cancel_answers_empty() {
        let (_reg, c) = consumer();
        let cancel = CancellationToken::new();
        let start = Instant::now();
        let (got, ()) = tokio::join!(
            c.poll(req("a", 0, 100, Duration::from_secs(30)), &cancel),
            async {
                sleep(Duration::from_secs(2)).await;
                cancel.cancel();
            }
        );
        assert!(got.items.is_empty());
        assert_eq!(start.elapsed(), Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_per_chat_serializes_chat() {
        let (_reg, c) = consumer_with(config(Duration::from_secs(60), Ordering::PerKey));
        admit(&c, 1, Some(7));
        admit(&c, 2, Some(7));
        admit(&c, 3, Some(8));
        assert_eq!(poll(&c, req("a", 0, 100, NOW)).await, [1, 3]);
        assert!(poll(&c, req("b", 0, 100, NOW)).await.is_empty());
        // Acking 1 frees chat 7; `a` still holds 3 and gets it back.
        assert_eq!(poll(&c, req("a", 2, 100, NOW)).await, [3]);
        assert_eq!(poll(&c, req("b", 0, 100, NOW)).await, [2]);
    }

    #[tokio::test(start_paused = true)]
    async fn poll_wakes_when_ack_frees_chat() {
        let (_reg, c) = consumer_with(config(Duration::from_secs(60), Ordering::PerKey));
        admit(&c, 1, Some(7));
        admit(&c, 2, Some(7));
        admit(&c, 3, Some(8));
        assert_eq!(poll(&c, req("a", 0, 100, NOW)).await, [1, 3]);
        let start = Instant::now();
        let (got, ()) = tokio::join!(poll(&c, req("b", 0, 100, Duration::from_secs(30))), async {
            sleep(Duration::from_secs(1)).await;
            // `a` still holds 3, so its poll acks 1 and returns 3 without
            // leasing 2 itself; the ack wakes `b`.
            assert_eq!(poll(&c, req("a", 2, 100, NOW)).await, [3]);
        });
        assert_eq!(got, [2]);
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_limit_and_timeout_clamped() {
        // The HTTP layer clamps; `poll` honours the bounds it receives.
        let (_reg, c) = consumer();
        for id in 1..=150 {
            admit(&c, id, None);
        }
        assert_eq!(poll(&c, req("a", 0, 1, NOW)).await, [1]);
        let start = Instant::now();
        let got = poll(&c, req("b", 0, 100, Duration::from_secs(50))).await;
        assert_eq!(got, (2..=101).collect::<Vec<_>>());
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[cfg(debug_assertions)]
    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "limit")]
    async fn poll_rejects_unclamped_limit() {
        let (_reg, c) = consumer();
        poll(&c, req("a", 0, 0, NOW)).await;
    }

    #[cfg(debug_assertions)]
    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "timeout")]
    async fn poll_rejects_unclamped_timeout() {
        let (_reg, c) = consumer();
        poll(&c, req("a", 0, 1, Duration::from_secs(51))).await;
    }

    #[tokio::test(start_paused = true)]
    async fn drain_answers_parked_poll() {
        let (_reg, c) = consumer();
        let start = Instant::now();
        let (got, ()) = tokio::join!(poll(&c, req("a", 0, 100, Duration::from_secs(30))), async {
            sleep(Duration::from_secs(1)).await;
            c.begin_drain();
        });
        assert!(got.is_empty());
        assert_eq!(start.elapsed(), Duration::from_millis(1500));

        // While draining, an empty queue answers within 500 ms, or the
        // poll's own shorter timeout.
        let start = Instant::now();
        assert!(
            poll(&c, req("a", 0, 100, Duration::from_secs(30)))
                .await
                .is_empty()
        );
        assert_eq!(start.elapsed(), Duration::from_millis(500));
        let start = Instant::now();
        assert!(
            poll(&c, req("a", 0, 100, Duration::from_millis(200)))
                .await
                .is_empty()
        );
        assert_eq!(start.elapsed(), Duration::from_millis(200));

        // A non-empty queue keeps serving, and a commit inside the hold
        // still reaches a parked poll.
        admit(&c, 1, None);
        assert_eq!(
            poll(&c, req("a", 0, 100, Duration::from_secs(30))).await,
            [1]
        );
        let (got, ()) = tokio::join!(poll(&c, req("b", 0, 100, Duration::from_secs(30))), async {
            sleep(Duration::from_millis(100)).await;
            admit(&c, 2, None);
        });
        assert_eq!(got, [2]);
    }
}
