//! Lock-free circuit breaker.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Consecutive failures that open a closed breaker.
    pub failure_threshold: u32,
    /// How long an open breaker rejects calls before admitting one probe.
    pub open_for: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_for: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

/// How far before construction `Breaker::offset` can still tell instants apart.
const OFFSET_SHIFT: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// The low two bits of the state word hold the state; the rest hold
/// `opened_at`.
const STATE_BITS: u32 = 2;
const STATE_MASK: u64 = (1 << STATE_BITS) - 1;
/// Largest `opened_at` the state word can hold (about 146 years).
const MAX_OFFSET: u64 = u64::MAX >> STATE_BITS;

const CLOSED: u64 = 0;
const OPEN: u64 = 1;
const HALF_OPEN: u64 = 2;

fn pack(state: u64, opened_at: u64) -> u64 {
    (opened_at << STATE_BITS) | state
}

/// Closed → Open after `failure_threshold` consecutive failures; Open →
/// HalfOpen once `open_for` has elapsed, admitting exactly one probe whose
/// outcome closes or reopens it.
#[derive(Debug)]
pub struct Breaker {
    cfg: BreakerConfig,
    /// Reference point for `opened_at`; see [`Breaker::offset`].
    base: Instant,
    /// State and `opened_at` in one word (see [`pack`]), so every transition
    /// is a single compare-exchange against the exact word it was decided on.
    word: AtomicU64,
    failures: AtomicU32,
}

impl Breaker {
    pub fn new(cfg: BreakerConfig) -> Self {
        Self {
            cfg,
            base: Instant::now(),
            word: AtomicU64::new(pack(CLOSED, 0)),
            failures: AtomicU32::new(0),
        }
    }

    pub fn allow(&self, now: Instant) -> bool {
        let word = self.word.load(Ordering::Acquire);
        match word & STATE_MASK {
            CLOSED => true,
            OPEN => {
                let opened_at = word >> STATE_BITS;
                if self.offset(now) < self.reopens_at(opened_at) {
                    return false;
                }
                self.word
                    .compare_exchange(
                        word,
                        pack(HALF_OPEN, opened_at),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            }
            _ => false,
        }
    }

    pub fn on_success(&self) {
        self.failures.store(0, Ordering::Release);
        self.word.store(pack(CLOSED, 0), Ordering::Release);
    }

    pub fn on_failure(&self, now: Instant) {
        let word = self.word.load(Ordering::Acquire);
        match word & STATE_MASK {
            HALF_OPEN => self.open(word, now),
            CLOSED => {
                let failures = self
                    .failures
                    .fetch_add(1, Ordering::AcqRel)
                    .saturating_add(1);
                if failures >= self.cfg.failure_threshold {
                    self.open(word, now);
                }
            }
            _ => {}
        }
    }

    pub fn state(&self) -> BreakerState {
        match self.word.load(Ordering::Acquire) & STATE_MASK {
            CLOSED => BreakerState::Closed,
            OPEN => BreakerState::Open,
            _ => BreakerState::HalfOpen,
        }
    }

    /// How long until [`Breaker::allow`] may admit a call, without changing
    /// the state: zero while closed or once an open breaker's `open_for` has
    /// elapsed, the rest of `open_for` while open, and `None` while half-open,
    /// when only the probe's outcome changes it.
    pub fn ready_in(&self, now: Instant) -> Option<Duration> {
        let word = self.word.load(Ordering::Acquire);
        match word & STATE_MASK {
            CLOSED => Some(Duration::ZERO),
            OPEN => {
                let reopens_at = self.reopens_at(word >> STATE_BITS);
                let left = reopens_at.saturating_sub(self.offset(now));
                Some(Duration::from_nanos(left))
            }
            _ => None,
        }
    }

    /// Opens the breaker at `now` unless another caller already moved it
    /// away from `from`; a lost race changes nothing.
    fn open(&self, from: u64, now: Instant) {
        let _ = self.word.compare_exchange(
            from,
            pack(OPEN, self.offset(now)),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// The offset at which a breaker opened at `opened_at` admits a probe.
    fn reopens_at(&self, opened_at: u64) -> u64 {
        let open_for = u64::try_from(self.cfg.open_for.as_nanos()).unwrap_or(u64::MAX);
        opened_at.saturating_add(open_for)
    }

    /// Nanoseconds from `base - OFFSET_SHIFT` to `t`, capped at
    /// [`MAX_OFFSET`]. The shift lets callers pass instants taken shortly
    /// before construction, and keeps `t` far from `base` so the subtraction
    /// is exact (Windows rounds differences within one clock tick to zero).
    fn offset(&self, t: Instant) -> u64 {
        let shifted = t.checked_add(OFFSET_SHIFT).unwrap_or(t);
        let elapsed = shifted.saturating_duration_since(self.base);
        u64::try_from(elapsed.as_nanos())
            .unwrap_or(u64::MAX)
            .min(MAX_OFFSET)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BreakerConfig {
        BreakerConfig {
            failure_threshold: 3,
            open_for: Duration::from_secs(30),
        }
    }

    fn opened(t0: Instant) -> Breaker {
        let b = Breaker::new(cfg());
        for _ in 0..3 {
            b.on_failure(t0);
        }
        b
    }

    #[test]
    fn default_config() {
        let d = BreakerConfig::default();
        assert_eq!(d.failure_threshold, 5);
        assert_eq!(d.open_for, Duration::from_secs(30));
    }

    #[test]
    fn breaker_opens_after_threshold() {
        let t0 = Instant::now();
        let b = Breaker::new(cfg());
        b.on_failure(t0);
        b.on_failure(t0);
        assert_eq!(b.state(), BreakerState::Closed);
        assert!(b.allow(t0));
        b.on_failure(t0);
        assert_eq!(b.state(), BreakerState::Open);
        assert!(!b.allow(t0 + Duration::from_secs(29)));
    }

    #[test]
    fn breaker_half_open_allows_one_probe() {
        let t0 = Instant::now();
        let b = opened(t0);
        let t1 = t0 + Duration::from_secs(30);
        assert!(b.allow(t1));
        assert_eq!(b.state(), BreakerState::HalfOpen);
        assert!(!b.allow(t1));
        assert!(!b.allow(t1 + Duration::from_secs(100)));
    }

    #[test]
    fn breaker_probe_failure_reopens() {
        let t0 = Instant::now();
        let b = opened(t0);
        let t1 = t0 + Duration::from_secs(30);
        assert!(b.allow(t1));
        b.on_failure(t1);
        assert_eq!(b.state(), BreakerState::Open);
        assert!(!b.allow(t1 + Duration::from_secs(29)));
        assert!(b.allow(t1 + Duration::from_secs(30)));
    }

    #[test]
    fn breaker_probe_success_closes() {
        let t0 = Instant::now();
        let b = opened(t0);
        assert!(b.allow(t0 + Duration::from_secs(30)));
        b.on_success();
        assert_eq!(b.state(), BreakerState::Closed);
        assert!(b.allow(t0 + Duration::from_secs(30)));
        b.on_failure(t0);
        b.on_failure(t0);
        assert_eq!(b.state(), BreakerState::Closed, "failure count was reset");
    }

    #[test]
    fn breaker_half_open_admits_exactly_one_concurrent_probe() {
        const THREADS: usize = 16;
        let t0 = Instant::now();
        let b = opened(t0);
        let t1 = t0 + Duration::from_secs(30);
        let barrier = std::sync::Barrier::new(THREADS);
        let admitted = std::thread::scope(|s| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        b.allow(t1)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|&ok| ok)
                .count()
        });
        assert_eq!(admitted, 1);
        assert_eq!(b.state(), BreakerState::HalfOpen);
    }

    #[test]
    fn breaker_ready_in_tracks_state() {
        let t0 = Instant::now();
        let b = Breaker::new(cfg());
        assert_eq!(b.ready_in(t0), Some(Duration::ZERO));
        for _ in 0..3 {
            b.on_failure(t0);
        }
        assert_eq!(
            b.ready_in(t0 + Duration::from_secs(10)),
            Some(Duration::from_secs(20))
        );
        let t1 = t0 + Duration::from_secs(31);
        assert_eq!(b.ready_in(t1), Some(Duration::ZERO));
        assert_eq!(b.state(), BreakerState::Open, "ready_in admits nothing");
        assert!(b.allow(t1));
        assert_eq!(b.ready_in(t1), None, "the probe's outcome decides");
        b.on_success();
        assert_eq!(b.ready_in(t1), Some(Duration::ZERO));
    }
}
