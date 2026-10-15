//! Lock-free circuit breaker.

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
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

const CLOSED: u8 = 0;
const OPEN: u8 = 1;
const HALF_OPEN: u8 = 2;

/// Closed → Open after `failure_threshold` consecutive failures; Open →
/// HalfOpen once `open_for` has elapsed, admitting exactly one probe whose
/// outcome closes or reopens it.
#[derive(Debug)]
pub struct Breaker {
    cfg: BreakerConfig,
    /// Reference point for `opened_at`, which is stored as nanoseconds from
    /// it; see [`Breaker::offset`].
    base: Instant,
    state: AtomicU8,
    failures: AtomicU32,
    opened_at: AtomicU64,
}

impl Breaker {
    pub fn new(cfg: BreakerConfig) -> Self {
        Self {
            cfg,
            base: Instant::now(),
            state: AtomicU8::new(CLOSED),
            failures: AtomicU32::new(0),
            opened_at: AtomicU64::new(0),
        }
    }

    pub fn allow(&self, now: Instant) -> bool {
        match self.state.load(Ordering::Acquire) {
            CLOSED => true,
            OPEN => {
                let opened_at = self.opened_at.load(Ordering::Acquire);
                let open_for = u64::try_from(self.cfg.open_for.as_nanos()).unwrap_or(u64::MAX);
                if self.offset(now) < opened_at.saturating_add(open_for) {
                    return false;
                }
                self.state
                    .compare_exchange(OPEN, HALF_OPEN, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            }
            _ => false,
        }
    }

    pub fn on_success(&self) {
        self.failures.store(0, Ordering::Release);
        self.state.store(CLOSED, Ordering::Release);
    }

    pub fn on_failure(&self, now: Instant) {
        match self.state.load(Ordering::Acquire) {
            HALF_OPEN => self.open(HALF_OPEN, now),
            CLOSED => {
                let failures = self
                    .failures
                    .fetch_add(1, Ordering::AcqRel)
                    .saturating_add(1);
                if failures >= self.cfg.failure_threshold {
                    self.open(CLOSED, now);
                }
            }
            _ => {}
        }
    }

    pub fn state(&self) -> BreakerState {
        match self.state.load(Ordering::Acquire) {
            CLOSED => BreakerState::Closed,
            OPEN => BreakerState::Open,
            _ => BreakerState::HalfOpen,
        }
    }

    fn open(&self, from: u8, now: Instant) {
        self.opened_at.store(self.offset(now), Ordering::Release);
        let _ = self
            .state
            .compare_exchange(from, OPEN, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Nanoseconds from `base - OFFSET_SHIFT` to `t`. The shift lets callers
    /// pass instants taken shortly before construction, and keeps `t` far
    /// from `base` so the subtraction is exact (Windows rounds differences
    /// within one clock tick to zero).
    fn offset(&self, t: Instant) -> u64 {
        let shifted = t.checked_add(OFFSET_SHIFT).unwrap_or(t);
        let elapsed = shifted.saturating_duration_since(self.base);
        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
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
}
