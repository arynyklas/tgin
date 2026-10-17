//! Keyed GCRA rate limiter that books slots ahead (shaping).
//!
//! Each key stores its theoretical arrival time (TAT): when its next request
//! would be on schedule. With emission interval `T = per / count` and burst
//! tolerance `τ = T × (burst − 1)`, a request at `now` may start at
//! `max(TAT − τ, now)`; booking it moves the TAT one interval past
//! `max(TAT, now)`.

use std::collections::HashMap;
use std::fmt;
use std::hash::{BuildHasher, Hash, RandomState};
use std::num::NonZeroU32;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;

const SHARDS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum QuotaError {
    #[error("the rate limit period must be positive")]
    ZeroPeriod,
}

/// `count` requests per `per`, up to `burst` of them at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    /// `T`: the spacing between requests at the steady rate.
    interval: Duration,
    /// `τ`: how far ahead of schedule a request may start.
    tolerance: Duration,
}

impl Quota {
    /// `count` requests per `per`, up to `burst` of them at once.
    ///
    /// `per / count` is rounded up to the nanosecond: a quota is an upper
    /// bound, so rounding must never raise the rate.
    pub fn new(count: NonZeroU32, per: Duration, burst: NonZeroU32) -> Result<Quota, QuotaError> {
        if per.is_zero() {
            return Err(QuotaError::ZeroPeriod);
        }
        let floor = per / count.get();
        let interval = match floor.checked_mul(count.get()) {
            Some(exact) if exact == per => floor,
            _ => floor.saturating_add(Duration::from_nanos(1)),
        };
        // A tolerance past `Duration::MAX` lets every request start at once,
        // which is what a burst that large means.
        let tolerance = interval
            .checked_mul(burst.get() - 1)
            .unwrap_or(Duration::MAX);
        Ok(Quota {
            interval,
            tolerance,
        })
    }

    /// One GCRA step for a key whose TAT is `stored` (`None` for a key with
    /// no state): the new TAT and the delay, or the denial.
    fn book(
        &self,
        stored: Option<Tat>,
        now: Instant,
        max_delay: Duration,
    ) -> Result<(Tat, Duration), Denied> {
        let tat = match stored {
            None => now,
            Some(Tat::At(tat)) => tat.max(now),
            Some(Tat::Never) => {
                return Err(Denied {
                    retry_after: Duration::MAX,
                });
            }
        };
        let wait = tat
            .saturating_duration_since(now)
            .saturating_sub(self.tolerance);
        if wait > max_delay {
            return Err(Denied { retry_after: wait });
        }
        let next = tat.checked_add(self.interval).map_or(Tat::Never, Tat::At);
        Ok((next, wait))
    }
}

/// A key's theoretical arrival time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tat {
    At(Instant),
    /// Past the last `Instant` the platform can represent: the key never
    /// frees again (only a `cancel` can bring it back).
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    tat: Tat,
    /// Which booking set `tat`: unique within the shard.
    stamp: u64,
}

struct Shard<K> {
    map: HashMap<K, Entry>,
    next_stamp: u64,
}

/// A booked slot.
///
/// Invariant: every booking gives its key's entry a stamp no other booking in
/// the shard has had, and `cancel` restores the entry this booking replaced
/// together with that entry's stamp. So the entry carries this admit's stamp
/// exactly when this booking is the key's latest one still in effect, and
/// undoing it restores precisely the state before it. A repeated `cancel`, or
/// one after a later booking, sees another stamp and does nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admit {
    /// How long the caller waits before using the slot (zero = now).
    pub delay: Duration,
    stamp: u64,
    /// The key's entry before this booking; `None` when it had none.
    prev: Option<Entry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Denied {
    /// How long until the key's next slot can start: a request with zero
    /// `max_delay` is admitted then. `Duration::MAX` when that slot lies past
    /// any `Instant`.
    pub retry_after: Duration,
}

/// One GCRA state per key, spread over 16 independently locked maps.
pub struct KeyedLimiter<K> {
    quota: Quota,
    hasher: RandomState,
    shards: [Mutex<Shard<K>>; SHARDS],
}

impl<K: Hash + Eq + Clone> KeyedLimiter<K> {
    pub fn new(quota: Quota) -> Self {
        Self {
            quota,
            hasher: RandomState::new(),
            shards: std::array::from_fn(|_| {
                Mutex::new(Shard {
                    map: HashMap::new(),
                    next_stamp: 0,
                })
            }),
        }
    }

    fn shard(&self, key: &K) -> &Mutex<Shard<K>> {
        // Truncating the hash keeps its low bits, all the index needs.
        &self.shards[self.hasher.hash_one(key) as usize % SHARDS]
    }

    /// Books one slot for `key` if it can start within `max_delay` of `now`:
    /// `Ok`, the slot booked (`delay` zero = now); otherwise `Err`, nothing
    /// booked.
    pub fn acquire(&self, key: &K, now: Instant, max_delay: Duration) -> Result<Admit, Denied> {
        let mut guard = self.shard(key).lock();
        let Shard { map, next_stamp } = &mut *guard;
        let stamp = *next_stamp;
        let admit = match map.get_mut(key) {
            Some(entry) => {
                let prev = *entry;
                let (tat, delay) = self.quota.book(Some(prev.tat), now, max_delay)?;
                *entry = Entry { tat, stamp };
                Admit {
                    delay,
                    stamp,
                    prev: Some(prev),
                }
            }
            None => {
                let (tat, delay) = self.quota.book(None, now, max_delay)?;
                map.insert(key.clone(), Entry { tat, stamp });
                Admit {
                    delay,
                    stamp,
                    prev: None,
                }
            }
        };
        // 2^64 bookings per shard never happen; wrapping only avoids a panic path.
        *next_stamp = stamp.wrapping_add(1);
        Ok(admit)
    }

    /// Undoes `admit` when no later booking for `key` happened; otherwise does
    /// nothing.
    pub fn cancel(&self, key: &K, admit: Admit) {
        let mut guard = self.shard(key).lock();
        let map = &mut guard.map;
        let Some(entry) = map.get_mut(key) else {
            return;
        };
        if entry.stamp != admit.stamp {
            return;
        }
        match admit.prev {
            Some(prev) => *entry = prev,
            None => {
                map.remove(key);
            }
        }
    }

    /// Forgets keys whose next slot is free at `now`: their state equals a
    /// fresh key's. A key still inside its burst allowance is kept, or
    /// forgetting it would grant a new burst.
    pub fn retain_recent(&self, now: Instant) {
        for shard in &self.shards {
            let map = &mut shard.lock().map;
            map.retain(|_, entry| match entry.tat {
                Tat::At(tat) => tat > now,
                Tat::Never => true,
            });
            // Give memory back after a spike of keys, without reallocating on
            // every sweep of a steady population.
            if map.len() < map.capacity() / 4 {
                map.shrink_to_fit();
            }
        }
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|shard| shard.lock().map.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|shard| shard.lock().map.is_empty())
    }
}

impl<K> fmt::Debug for KeyedLimiter<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedLimiter")
            .field("quota", &self.quota)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::sync::Barrier;
    use std::time::Duration;

    use tokio::time::{Instant, advance};

    use super::*;

    const SEC: Duration = Duration::from_secs(1);
    const NS: Duration = Duration::from_nanos(1);
    const ZERO: Duration = Duration::ZERO;

    fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    fn keyed(count: u32, per: Duration, burst: u32) -> KeyedLimiter<u64> {
        KeyedLimiter::new(Quota::new(nz(count), per, nz(burst)).expect("valid quota"))
    }

    fn delay(result: Result<Admit, Denied>) -> Result<Duration, Denied> {
        result.map(|admit| admit.delay)
    }

    fn denied(retry_after: Duration) -> Result<Duration, Denied> {
        Err(Denied { retry_after })
    }

    #[tokio::test(start_paused = true)]
    async fn burst_then_rate() {
        let limiter = keyed(1, SEC, 3);
        let t0 = Instant::now();
        for _ in 0..3 {
            assert_eq!(delay(limiter.acquire(&1, t0, ZERO)), Ok(ZERO));
        }
        assert_eq!(delay(limiter.acquire(&1, t0, ZERO)), denied(SEC));
        // A denial books nothing: retrying keeps the same answer.
        assert_eq!(delay(limiter.acquire(&1, t0, ZERO)), denied(SEC));

        advance(SEC - NS).await;
        assert_eq!(delay(limiter.acquire(&1, Instant::now(), ZERO)), denied(NS));
        advance(NS).await;
        assert_eq!(delay(limiter.acquire(&1, Instant::now(), ZERO)), Ok(ZERO));
        assert_eq!(
            delay(limiter.acquire(&1, Instant::now(), ZERO)),
            denied(SEC)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shaping_books_future_slots() {
        let limiter = keyed(1, SEC, 1);
        let t0 = Instant::now();
        let max_delay = 5 * SEC;
        for n in 0..=5 {
            assert_eq!(delay(limiter.acquire(&1, t0, max_delay)), Ok(n * SEC));
        }
        assert_eq!(delay(limiter.acquire(&1, t0, max_delay)), denied(6 * SEC));
        assert_eq!(delay(limiter.acquire(&1, t0, max_delay)), denied(6 * SEC));

        advance(2 * SEC).await;
        let t2 = Instant::now();
        assert_eq!(delay(limiter.acquire(&1, t2, max_delay)), Ok(4 * SEC));
        assert_eq!(delay(limiter.acquire(&1, t2, max_delay)), Ok(5 * SEC));
        assert_eq!(delay(limiter.acquire(&1, t2, max_delay)), denied(6 * SEC));
    }

    #[tokio::test(start_paused = true)]
    async fn keys_are_independent() {
        let limiter = keyed(1, SEC, 1);
        let t0 = Instant::now();
        assert_eq!(delay(limiter.acquire(&1, t0, ZERO)), Ok(ZERO));
        assert_eq!(delay(limiter.acquire(&1, t0, ZERO)), denied(SEC));
        for key in 2..100 {
            assert_eq!(delay(limiter.acquire(&key, t0, ZERO)), Ok(ZERO));
        }
        // Other keys' bookings did not push key 1 further out.
        assert_eq!(delay(limiter.acquire(&1, t0, ZERO)), denied(SEC));
        assert_eq!(limiter.len(), 99);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_returns_the_slot() {
        let limiter = keyed(1, SEC, 1);
        let t0 = Instant::now();
        let first = limiter.acquire(&1, t0, ZERO).expect("admitted");
        assert_eq!(delay(limiter.acquire(&1, t0, ZERO)), denied(SEC));
        limiter.cancel(&1, first);
        // Exact undo: the key is as if never booked.
        assert_eq!(limiter.len(), 0);
        assert!(limiter.is_empty());
        assert_eq!(delay(limiter.acquire(&1, t0, ZERO)), Ok(ZERO));

        let max_delay = 5 * SEC;
        let second = limiter.acquire(&1, t0, max_delay).expect("admitted");
        assert_eq!(second.delay, SEC);
        limiter.cancel(&1, second);
        assert_eq!(delay(limiter.acquire(&1, t0, max_delay)), Ok(SEC));
        // `second` was undone once already; a repeat cancel must not free
        // the slot that was booked again since.
        limiter.cancel(&1, second);
        assert_eq!(delay(limiter.acquire(&1, t0, max_delay)), Ok(2 * SEC));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_after_a_later_booking_does_nothing() {
        let limiter = keyed(1, SEC, 1);
        let t0 = Instant::now();
        let max_delay = 5 * SEC;
        let first = limiter.acquire(&1, t0, max_delay).expect("admitted");
        let second = limiter.acquire(&1, t0, max_delay).expect("admitted");
        assert_eq!(second.delay, SEC);
        limiter.cancel(&1, first);
        assert_eq!(delay(limiter.acquire(&1, t0, max_delay)), Ok(2 * SEC));

        // Undoing in reverse order is exact.
        let fourth = limiter.acquire(&2, t0, max_delay).expect("admitted");
        let fifth = limiter.acquire(&2, t0, max_delay).expect("admitted");
        limiter.cancel(&2, fifth);
        limiter.cancel(&2, fourth);
        assert_eq!(delay(limiter.acquire(&2, t0, max_delay)), Ok(ZERO));

        // A booking swept away and re-made is a later booking too.
        let swept = limiter.acquire(&3, t0, ZERO).expect("admitted");
        advance(SEC).await;
        let t1 = Instant::now();
        limiter.retain_recent(t1);
        assert_eq!(delay(limiter.acquire(&3, t1, ZERO)), Ok(ZERO));
        limiter.cancel(&3, swept);
        assert_eq!(delay(limiter.acquire(&3, t1, ZERO)), denied(SEC));
    }

    #[tokio::test(start_paused = true)]
    async fn retain_recent_drops_idle_keys() {
        let limiter = keyed(1, SEC, 3);
        let t0 = Instant::now();
        for key in 0..1_000 {
            assert_eq!(delay(limiter.acquire(&key, t0, ZERO)), Ok(ZERO));
        }
        let future = 1_000_000;
        for _ in 0..3 {
            assert_eq!(delay(limiter.acquire(&future, t0, ZERO)), Ok(ZERO));
        }
        // Keys inside their burst allowance still hold state: forgetting them
        // would hand out a fresh burst.
        limiter.retain_recent(t0);
        assert_eq!(limiter.len(), 1_001);
        advance(SEC - NS).await;
        limiter.retain_recent(Instant::now());
        assert_eq!(limiter.len(), 1_001);

        advance(NS).await;
        limiter.retain_recent(Instant::now());
        assert_eq!(limiter.len(), 1);
        // The survivor kept its state: one more slot now, not a fresh burst.
        let t1 = Instant::now();
        assert_eq!(delay(limiter.acquire(&future, t1, ZERO)), Ok(ZERO));
        assert_eq!(delay(limiter.acquire(&future, t1, ZERO)), denied(SEC));

        // Its next slot is t0 + 4 s.
        advance(3 * SEC - NS).await;
        limiter.retain_recent(Instant::now());
        assert_eq!(limiter.len(), 1);
        advance(NS).await;
        limiter.retain_recent(Instant::now());
        assert_eq!(limiter.len(), 0);
    }

    #[test]
    fn concurrent_acquires_never_exceed_burst() {
        const THREADS: usize = 8;
        const BURST: u32 = 50;
        let limiter = keyed(1, SEC, BURST);
        let now = Instant::now();
        let start = Barrier::new(THREADS);
        let admitted: usize = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..THREADS)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        (0..1_000)
                            .filter(|_| limiter.acquire(&7, now, ZERO).is_ok())
                            .count()
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("worker panicked"))
                .sum()
        });
        assert_eq!(admitted, BURST as usize);
    }

    #[test]
    fn quota_needs_a_positive_period() {
        assert_eq!(Quota::new(nz(1), ZERO, nz(1)), Err(QuotaError::ZeroPeriod));
        // Shorter than a nanosecond per request rounds the interval up, so
        // the rate never exceeds the quota.
        let limiter = KeyedLimiter::new(Quota::new(nz(3), NS, nz(1)).expect("valid"));
        let now = Instant::now();
        assert_eq!(delay(limiter.acquire(&1, now, ZERO)), Ok(ZERO));
        assert_eq!(delay(limiter.acquire(&1, now, ZERO)), denied(NS));
    }

    #[test]
    fn interval_rounds_up() {
        // 30/s: 33_333_333.3 ns rounds up to 33_333_334 ns.
        let limiter = keyed(30, SEC, 30);
        let now = Instant::now();
        for _ in 0..30 {
            assert_eq!(delay(limiter.acquire(&1, now, ZERO)), Ok(ZERO));
        }
        assert_eq!(
            delay(limiter.acquire(&1, now, ZERO)),
            denied(Duration::from_nanos(33_333_334))
        );
    }

    /// The latest `Instant` this platform can represent.
    fn end_of_time() -> Instant {
        let mut end = Instant::now();
        for shift in (0..64).rev() {
            if let Some(later) = end.checked_add(Duration::from_secs(1 << shift)) {
                end = later;
            }
        }
        for shift in (0..30).rev() {
            if let Some(later) = end.checked_add(Duration::from_nanos(1 << shift)) {
                end = later;
            }
        }
        end
    }

    #[test]
    fn huge_values_never_panic() {
        let max = nz(u32::MAX);
        for quota in [
            Quota::new(nz(1), Duration::MAX, nz(1)),
            Quota::new(nz(1), Duration::MAX, max),
            Quota::new(max, Duration::MAX, max),
            Quota::new(max, NS, max),
            Quota::new(nz(1), SEC, max),
        ] {
            let limiter = KeyedLimiter::new(quota.expect("valid quota"));
            let now = Instant::now();
            for max_delay in [ZERO, SEC, Duration::MAX] {
                for _ in 0..3 {
                    let _ = limiter.acquire(&1, now, max_delay);
                }
            }
            limiter.retain_recent(now);
            limiter.retain_recent(end_of_time());
        }

        // One request per `Duration::MAX`: the first is admitted, its next
        // slot lies past any `Instant`, so the key never frees again.
        let limiter = keyed(1, Duration::MAX, 1);
        let now = Instant::now();
        let first = limiter.acquire(&1, now, ZERO).expect("admitted");
        assert_eq!(first.delay, ZERO);
        assert_eq!(
            delay(limiter.acquire(&1, now, Duration::MAX)),
            denied(Duration::MAX)
        );
        limiter.retain_recent(end_of_time());
        assert_eq!(limiter.len(), 1);
        limiter.cancel(&1, first);
        assert_eq!(delay(limiter.acquire(&1, now, ZERO)), Ok(ZERO));

        // A burst so large its tolerance saturates admits at once.
        let limiter = keyed(1, Duration::MAX / 2, u32::MAX);
        assert_eq!(delay(limiter.acquire(&1, now, ZERO)), Ok(ZERO));
    }

    #[test]
    fn now_far_in_the_future() {
        let end = end_of_time();
        let limiter = keyed(1, SEC, 1);

        // The slot after `end - 1s` is `end` itself, still representable.
        let before = end - SEC;
        assert_eq!(delay(limiter.acquire(&1, before, Duration::MAX)), Ok(ZERO));
        assert_eq!(delay(limiter.acquire(&1, before, Duration::MAX)), Ok(SEC));
        assert_eq!(
            delay(limiter.acquire(&1, before, Duration::MAX)),
            denied(Duration::MAX)
        );

        // At the last instant, the next slot cannot be represented.
        assert_eq!(delay(limiter.acquire(&2, end, Duration::MAX)), Ok(ZERO));
        assert_eq!(
            delay(limiter.acquire(&2, end, Duration::MAX)),
            denied(Duration::MAX)
        );

        let limiter = keyed(1, SEC, 1);
        limiter.acquire(&1, before, ZERO).expect("admitted");
        limiter.acquire(&2, end, ZERO).expect("admitted");
        limiter.retain_recent(end);
        // Key 1's next slot is `end`: free. Key 2's never is.
        assert_eq!(limiter.len(), 1);
        assert_eq!(delay(limiter.acquire(&1, end, ZERO)), Ok(ZERO));
    }
}
