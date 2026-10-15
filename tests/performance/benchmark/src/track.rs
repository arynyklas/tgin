//! Matches bot replies to the updates that caused them and records latency
//! from each update's scheduled send time.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

use dashmap::DashMap;
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

/// Prefix of warm-up reply texts, `warmup-<update_id>`. Measured updates
/// carry their bare `update_id`.
const WARMUP_PREFIX: &str = "warmup-";
const REPLIES_POISONED: &str = "reply records lock poisoned";

/// The text of warm-up update `update_id`, which its reply echoes.
pub fn warmup_text(update_id: u64) -> String {
    format!("{WARMUP_PREFIX}{update_id}")
}

/// Longest latency the histogram tells apart, 1 h in microseconds; longer
/// ones are recorded as this.
const LATENCY_MAX_US: u64 = 3_600_000_000;

/// Why a send never reached the system under test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The request failed: connect error, reset, timeout.
    Transport,
    /// The system under test answered with a non-2xx status.
    Status,
}

/// Where a measured update stands.
enum Slot {
    /// Sent, no reply yet; holds the scheduled send time.
    Pending(Instant),
    Received,
    /// The send never reached the system under test.
    Failed,
}

pub struct Tracker {
    /// Every measured update of the run, by `update_id`.
    updates: DashMap<u64, Slot>,
    /// Warm-up updates by `update_id`: whether their reply came back.
    warmup: DashMap<u64, bool>,
    replies: Mutex<Replies>,
    sent: AtomicU64,
    received: AtomicU64,
    dropped_by_generator: AtomicU64,
    send_errors: AtomicU64,
    http_errors: AtomicU64,
    /// Further replies to a measured update already received.
    duplicate_replies: AtomicU64,
    /// Replies matching no update: unknown ids, replies to warm-up updates
    /// already received, or late replies to updates whose send failed.
    unmatched_replies: AtomicU64,
    warmup_sent: AtomicU64,
    warmup_received: AtomicU64,
    poll_calls: AtomicU64,
    poll_empty_calls: AtomicU64,
    poll_max_batch: AtomicU64,
    poll_total_batch: AtomicU64,
}

/// What the first reply to each measured update recorded.
struct Replies {
    latency_us: Histogram<u64>,
    last: Option<Instant>,
}

/// A snapshot of the counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub sent: u64,
    pub received: u64,
    pub dropped_by_generator: u64,
    pub send_errors: u64,
    pub http_errors: u64,
    pub duplicate_replies: u64,
    pub unmatched_replies: u64,
    pub warmup_sent: u64,
    pub warmup_received: u64,
}

impl Counts {
    /// Sent updates whose reply never came back.
    pub fn unreceived(&self) -> u64 {
        self.sent.saturating_sub(self.received)
    }
}

/// Latency percentiles in milliseconds; `None` without a single reply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Latency {
    pub min_ms: Option<f64>,
    pub mean_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

/// What long-poll consumers asked of the fake Bot API during the run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollStats {
    #[serde(rename = "get_updates_calls")]
    pub calls: u64,
    /// Calls that returned no update.
    #[serde(rename = "empty_get_updates_calls")]
    pub empty_calls: u64,
    #[serde(rename = "max_get_updates_batch")]
    pub max_batch: u64,
    #[serde(rename = "total_get_updates_batch")]
    pub total_batch: u64,
}

impl PollStats {
    /// Mean size of the non-empty batches.
    pub fn mean_batch(&self) -> Option<f64> {
        let non_empty = self.calls.saturating_sub(self.empty_calls);
        (non_empty > 0).then(|| self.total_batch as f64 / non_empty as f64)
    }
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            updates: DashMap::new(),
            warmup: DashMap::new(),
            replies: Mutex::new(Replies {
                latency_us: Histogram::new_with_bounds(1, LATENCY_MAX_US, 3)
                    .expect("valid histogram bounds"),
                last: None,
            }),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            dropped_by_generator: AtomicU64::new(0),
            send_errors: AtomicU64::new(0),
            http_errors: AtomicU64::new(0),
            duplicate_replies: AtomicU64::new(0),
            unmatched_replies: AtomicU64::new(0),
            warmup_sent: AtomicU64::new(0),
            warmup_received: AtomicU64::new(0),
            poll_calls: AtomicU64::new(0),
            poll_empty_calls: AtomicU64::new(0),
            poll_max_batch: AtomicU64::new(0),
            poll_total_batch: AtomicU64::new(0),
        }
    }

    /// A measured update due at `due` is about to go out.
    pub fn issued(&self, update_id: u64, due: Instant) {
        self.updates.insert(update_id, Slot::Pending(due));
        self.sent.fetch_add(1, Ordering::Relaxed);
    }

    /// A measured update could not go out: the in-flight budget was spent.
    pub fn dropped(&self) {
        self.dropped_by_generator.fetch_add(1, Ordering::Relaxed);
    }

    /// Warm-up update `update_id` is about to go out.
    pub fn warmup_issued(&self, update_id: u64) {
        self.warmup.insert(update_id, false);
        self.warmup_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// A measured update never reached the system under test.
    pub fn failed(&self, update_id: u64, failure: Failure) {
        let Some(mut slot) = self.updates.get_mut(&update_id) else {
            return;
        };
        // A reply that already arrived wins: the update was delivered.
        if !matches!(*slot, Slot::Pending(_)) {
            return;
        }
        *slot = Slot::Failed;
        drop(slot);
        let counter = match failure {
            Failure::Transport => &self.send_errors,
            Failure::Status => &self.http_errors,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// A bot replied with `text` through `sendMessage`.
    pub fn reply(&self, text: &str) {
        if let Some(id) = text.strip_prefix(WARMUP_PREFIX) {
            self.warmup_reply(id);
            return;
        }
        let now = Instant::now();
        let slot = text
            .parse::<u64>()
            .ok()
            .and_then(|update_id| self.updates.get_mut(&update_id));
        let Some(mut slot) = slot else {
            self.unmatched_replies.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let due = match *slot {
            Slot::Pending(due) => due,
            Slot::Received => {
                self.duplicate_replies.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Slot::Failed => {
                self.unmatched_replies.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        *slot = Slot::Received;
        drop(slot);
        let micros = u64::try_from((now - due).as_micros()).unwrap_or(u64::MAX);
        let mut replies = self.replies.lock().expect(REPLIES_POISONED);
        replies.latency_us.saturating_record(micros);
        replies.last = replies.last.max(Some(now));
        drop(replies);
        self.received.fetch_add(1, Ordering::Relaxed);
    }

    fn warmup_reply(&self, id: &str) {
        let first = id
            .parse::<u64>()
            .ok()
            .and_then(|update_id| self.warmup.get_mut(&update_id))
            .is_some_and(|mut received| !std::mem::replace(&mut *received, true));
        let counter = if first {
            &self.warmup_received
        } else {
            &self.unmatched_replies
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Every sent update was received or failed, so no reply is outstanding.
    pub fn settled(&self) -> bool {
        let counts = self.counts();
        counts.received + counts.send_errors + counts.http_errors >= counts.sent
    }

    /// Every warm-up update's reply came back.
    pub fn warmup_done(&self) -> bool {
        let counts = self.counts();
        counts.warmup_received >= counts.warmup_sent
    }

    pub fn counts(&self) -> Counts {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Counts {
            sent: load(&self.sent),
            received: load(&self.received),
            dropped_by_generator: load(&self.dropped_by_generator),
            send_errors: load(&self.send_errors),
            http_errors: load(&self.http_errors),
            duplicate_replies: load(&self.duplicate_replies),
            unmatched_replies: load(&self.unmatched_replies),
            warmup_sent: load(&self.warmup_sent),
            warmup_received: load(&self.warmup_received),
        }
    }

    /// When the latest first reply to a measured update arrived.
    pub fn last_reply(&self) -> Option<Instant> {
        self.replies.lock().expect(REPLIES_POISONED).last
    }

    pub fn latency(&self) -> Latency {
        let replies = self.replies.lock().expect(REPLIES_POISONED);
        let histogram = &replies.latency_us;
        if histogram.is_empty() {
            return Latency::default();
        }
        let ms = |micros: u64| Some(micros as f64 / 1000.0);
        Latency {
            min_ms: ms(histogram.min()),
            mean_ms: Some(histogram.mean() / 1000.0),
            p50_ms: ms(histogram.value_at_quantile(0.50)),
            p95_ms: ms(histogram.value_at_quantile(0.95)),
            p99_ms: ms(histogram.value_at_quantile(0.99)),
            max_ms: ms(histogram.max()),
        }
    }

    /// A `getUpdates` call returned `batch_len` updates.
    pub fn record_poll(&self, batch_len: usize) {
        self.poll_calls.fetch_add(1, Ordering::Relaxed);
        let batch_len = batch_len as u64;
        if batch_len == 0 {
            self.poll_empty_calls.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.poll_total_batch
            .fetch_add(batch_len, Ordering::Relaxed);
        self.poll_max_batch.fetch_max(batch_len, Ordering::Relaxed);
    }

    pub fn poll_stats(&self) -> PollStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        PollStats {
            calls: load(&self.poll_calls),
            empty_calls: load(&self.poll_empty_calls),
            max_batch: load(&self.poll_max_batch),
            total_batch: load(&self.poll_total_batch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn latency_counts_from_the_scheduled_time() {
        let tracker = Tracker::new();
        let due = Instant::now();
        // The send itself went out 1.2 ms late; the reply came 0.6 ms after
        // it. Values under 2048 µs are exact in a 3-figure histogram.
        tokio::time::advance(Duration::from_micros(1200)).await;
        tracker.issued(7, due);
        tokio::time::advance(Duration::from_micros(600)).await;

        tracker.reply("7");

        let latency = tracker.latency();
        assert_eq!(latency.max_ms, Some(1.8));
        assert_eq!(latency.p50_ms, Some(1.8));
        let counts = tracker.counts();
        assert_eq!(
            (counts.sent, counts.received, counts.unreceived()),
            (1, 1, 0)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn long_latencies_are_recorded_not_clamped() {
        let tracker = Tracker::new();
        let due = Instant::now();
        for id in 1..=3 {
            tracker.issued(id, due);
        }
        for (id, at_ms) in [(1, 5), (2, 250), (3, 5_000)] {
            tokio::time::advance(Duration::from_millis(at_ms) - due.elapsed()).await;
            tracker.reply(&id.to_string());
        }

        let latency = tracker.latency();
        let close = |got: Option<f64>, want: f64| (got.unwrap() - want).abs() <= want * 0.001;
        assert!(close(latency.min_ms, 5.0), "{latency:?}");
        assert!(close(latency.p50_ms, 250.0), "{latency:?}");
        assert!(close(latency.max_ms, 5_000.0), "{latency:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn unreceived_is_sent_minus_received_and_stays_out_of_latency() {
        let tracker = Tracker::new();
        let due = Instant::now();
        for id in 1..=4 {
            tracker.issued(id, due);
        }
        tokio::time::advance(Duration::from_millis(2)).await;
        tracker.reply("1");
        tracker.reply("2");

        let counts = tracker.counts();
        assert_eq!(counts.unreceived(), 2);
        assert!(!tracker.settled());
        assert_eq!(tracker.latency().max_ms, Some(2.0));
    }

    #[tokio::test]
    async fn failed_sends_settle_without_a_reply() {
        let tracker = Tracker::new();
        let due = Instant::now();
        for id in 1..=3 {
            tracker.issued(id, due);
        }
        tracker.reply("1");
        tracker.failed(2, Failure::Transport);
        assert!(!tracker.settled());
        tracker.failed(3, Failure::Status);
        assert!(tracker.settled());

        let counts = tracker.counts();
        assert_eq!(counts.send_errors, 1);
        assert_eq!(counts.http_errors, 1);
        assert_eq!(counts.unreceived(), 2);
    }

    #[tokio::test]
    async fn duplicate_replies_are_counted_apart_from_unmatched_ones() {
        let tracker = Tracker::new();
        tracker.issued(1, Instant::now());
        tracker.issued(2, Instant::now());
        tracker.reply("1");
        tracker.reply("1");
        tracker.reply("1");
        tracker.reply("not-an-id");
        tracker.reply("99");
        // A late reply to an update whose send failed is not a duplicate.
        tracker.failed(2, Failure::Transport);
        tracker.reply("2");

        let counts = tracker.counts();
        assert_eq!(counts.received, 1);
        assert_eq!(counts.duplicate_replies, 2);
        assert_eq!(counts.unmatched_replies, 3);
    }

    #[tokio::test]
    async fn warmup_completes_only_when_every_warmup_id_came_back() {
        let tracker = Tracker::new();
        tracker.warmup_issued(1);
        tracker.warmup_issued(2);
        tracker.reply(&warmup_text(1));
        tracker.reply(&warmup_text(1));
        assert!(
            !tracker.warmup_done(),
            "a repeated reply must not stand in for id 2"
        );
        tracker.reply(&warmup_text(7));
        assert!(
            !tracker.warmup_done(),
            "an unknown id must not stand in for id 2"
        );
        tracker.reply(&warmup_text(2));
        assert!(tracker.warmup_done());

        let counts = tracker.counts();
        assert_eq!((counts.warmup_sent, counts.warmup_received), (2, 2));
        assert_eq!((counts.sent, counts.received), (0, 0));
        assert_eq!(counts.unmatched_replies, 2);
        assert_eq!(tracker.latency(), Latency::default());
    }

    #[test]
    fn warmup_text_carries_the_id() {
        assert_eq!(warmup_text(42), "warmup-42");
    }

    #[tokio::test(start_paused = true)]
    async fn the_last_matched_reply_is_remembered() {
        let tracker = Tracker::new();
        let due = Instant::now();
        tracker.issued(1, due);
        tracker.issued(2, due);
        assert_eq!(tracker.last_reply(), None);

        tokio::time::advance(Duration::from_millis(3)).await;
        tracker.reply("2");
        tokio::time::advance(Duration::from_millis(4)).await;
        tracker.reply("1");
        tokio::time::advance(Duration::from_millis(5)).await;
        tracker.reply("1");
        tracker.reply("nope");

        assert_eq!(tracker.last_reply(), Some(due + Duration::from_millis(7)));
    }

    #[test]
    fn dropped_sends_are_not_sent() {
        let tracker = Tracker::new();
        tracker.dropped();

        let counts = tracker.counts();
        assert_eq!(counts.dropped_by_generator, 1);
        assert_eq!(counts.sent, 0);
        assert!(tracker.settled());
    }
}
