//! Rate limiters that book slots ahead (shaping).
//!
//! [`KeyedLimiter`] is a keyed GCRA. Each key stores its theoretical arrival
//! time (TAT): when its next request would be on schedule. With emission
//! interval `T = per / count` and burst tolerance `τ = T × (burst − 1)`, a
//! request at `now` may start at `max(TAT − τ, now)`; booking it moves the
//! TAT one interval past `max(TAT, now)`.
//!
//! [`SlotCalendar`] is one sliding-window log: at most `burst` bookings in
//! any window of `burst × T`. A booking takes the earliest free instant at
//! or after the start asked for, so one booked far ahead never takes the
//! slots before it, as moving a single TAT would.

use std::collections::{HashMap, VecDeque};
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
        let interval = interval(count, per)?;
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

/// `per / count`, rounded up to the nanosecond: a quota is an upper bound,
/// so rounding must never raise the rate.
fn interval(count: NonZeroU32, per: Duration) -> Result<Duration, QuotaError> {
    if per.is_zero() {
        return Err(QuotaError::ZeroPeriod);
    }
    let floor = per / count.get();
    Ok(match floor.checked_mul(count.get()) {
        Some(exact) if exact == per => floor,
        _ => floor.saturating_add(Duration::from_nanos(1)),
    })
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
    /// nothing: the slot is then lost, and the quota still holds.
    ///
    /// `admit` must be what this limiter's [`KeyedLimiter::acquire`]
    /// returned for this `key`. Stamps are unique only within a shard, so
    /// another limiter's `Admit`, or one for another key, could match an
    /// unrelated booking by chance and undo it.
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

/// `count` bookings per `per`, up to `burst` of them at once, as one
/// sliding-window log: at most `burst` bookings in any window of
/// `burst × per / count`. A booking takes the earliest instant at or after
/// the one asked for where that holds, so a booking far ahead leaves the
/// free instants before it to others. Bookings that can no longer share a
/// window with a new one are dropped when booking and by
/// [`SlotCalendar::retain_recent`]: with every booking within `max_delay`
/// of its `now`, it holds about `count × (max_delay + window) / per`.
pub struct SlotCalendar {
    burst: usize,
    window: Duration,
    /// The booked instants, sorted.
    slots: Mutex<VecDeque<Instant>>,
}

/// A booked instant of a [`SlotCalendar`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Booking {
    /// How long after the caller's `now` the slot starts.
    pub delay: Duration,
    at: Instant,
}

impl SlotCalendar {
    /// `count` bookings per `per`, up to `burst` at once.
    ///
    /// # Errors
    ///
    /// `per` is zero.
    pub fn new(
        count: NonZeroU32,
        per: Duration,
        burst: NonZeroU32,
    ) -> Result<SlotCalendar, QuotaError> {
        let window = interval(count, per)?
            .checked_mul(burst.get())
            .unwrap_or(Duration::MAX);
        Ok(SlotCalendar {
            burst: usize::try_from(burst.get()).unwrap_or(usize::MAX),
            window,
            slots: Mutex::new(VecDeque::new()),
        })
    }

    /// Books the earliest free instant at or after `now + from`, if it is
    /// within `max_delay` of `now`; otherwise books nothing and tells when
    /// the earliest free instant is, from `now` (`Duration::MAX` past any
    /// representable instant).
    ///
    /// # Errors
    ///
    /// No free instant within `max_delay` of `now`.
    pub fn acquire(
        &self,
        now: Instant,
        from: Duration,
        max_delay: Duration,
    ) -> Result<Booking, Denied> {
        let never = Denied {
            retry_after: Duration::MAX,
        };
        let start = now.checked_add(from).ok_or(never)?;
        let mut slots = self.slots.lock();
        self.forget_before(&mut slots, now);
        let at = if self.fits(&slots, start) {
            start
        } else {
            slots
                .iter()
                .filter_map(|slot| slot.checked_add(self.window))
                .filter(|&after| after > start)
                .find(|&after| self.fits(&slots, after))
                .ok_or(never)?
        };
        let delay = at.saturating_duration_since(now);
        if delay > max_delay {
            return Err(Denied { retry_after: delay });
        }
        let index = slots.partition_point(|slot| *slot <= at);
        slots.insert(index, at);
        Ok(Booking { delay, at })
    }

    /// Undoes `booking`, which this calendar's [`SlotCalendar::acquire`]
    /// returned; nothing if it is already gone.
    pub fn cancel(&self, booking: Booking) {
        let mut slots = self.slots.lock();
        let index = slots.partition_point(|slot| *slot < booking.at);
        if slots.get(index) == Some(&booking.at) {
            slots.remove(index);
        }
    }

    /// Drops the bookings no booking at or after `now` can share a window
    /// with.
    pub fn retain_recent(&self, now: Instant) {
        let mut slots = self.slots.lock();
        self.forget_before(&mut slots, now);
        if slots.len() < slots.capacity() / 4 {
            slots.shrink_to_fit();
        }
    }

    pub fn len(&self) -> usize {
        self.slots.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.lock().is_empty()
    }

    fn forget_before(&self, slots: &mut VecDeque<Instant>, now: Instant) {
        while slots
            .front()
            .and_then(|slot| slot.checked_add(self.window))
            .is_some_and(|end| end <= now)
        {
            slots.pop_front();
        }
    }

    /// Whether a booking at `at` keeps every window of `slots` at `burst`
    /// bookings at most: in the sorted bookings with `at` among them, every
    /// `burst + 1` consecutive ones that include `at` span `window` or more.
    fn fits(&self, slots: &VecDeque<Instant>, at: Instant) -> bool {
        let k = slots.partition_point(|slot| *slot <= at);
        let with = |i: usize| match i.cmp(&k) {
            std::cmp::Ordering::Less => slots[i],
            std::cmp::Ordering::Equal => at,
            std::cmp::Ordering::Greater => slots[i - 1],
        };
        let len = slots.len() + 1;
        // Runs `lo ..= lo + burst` with `lo <= k <= lo + burst < len`.
        let first = k.saturating_sub(self.burst);
        let Some(last) = len.checked_sub(self.burst.saturating_add(1)) else {
            return true;
        };
        (first..=last.min(k))
            .all(|lo| with(lo + self.burst).saturating_duration_since(with(lo)) >= self.window)
    }
}

impl fmt::Debug for SlotCalendar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotCalendar")
            .field("burst", &self.burst)
            .field("window", &self.window)
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

#[cfg(test)]
mod calendar_tests {
    use std::num::NonZeroU32;
    use std::time::Duration;

    use proptest::prelude::*;
    use tokio::time::Instant;

    use super::*;

    const MS: Duration = Duration::from_millis(1);
    const SEC: Duration = Duration::from_secs(1);
    const ZERO: Duration = Duration::ZERO;

    /// 2 per second, 2 at once: at most 2 in any second.
    fn calendar(count: u32, per: Duration, burst: u32) -> SlotCalendar {
        let nz = |n| NonZeroU32::new(n).expect("non-zero");
        SlotCalendar::new(nz(count), per, nz(burst)).expect("valid")
    }

    fn delay(result: Result<Booking, Denied>) -> Result<Duration, Duration> {
        result.map(|b| b.delay).map_err(|d| d.retry_after)
    }

    #[test]
    fn burst_then_the_next_window() {
        let c = calendar(2, SEC, 2);
        let t0 = Instant::now();
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Ok(ZERO));
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Ok(ZERO));
        // A window is the burst's span: the next fits a whole second later.
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Err(SEC));
        assert_eq!(delay(c.acquire(t0, ZERO, SEC)), Ok(SEC));
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn a_booking_ahead_leaves_the_earlier_slots_free() {
        let c = calendar(2, SEC, 2);
        let t0 = Instant::now();
        // Booked 4 s ahead.
        assert_eq!(delay(c.acquire(t0, 4 * SEC, 5 * SEC)), Ok(4 * SEC));
        // Two now still fit, and a third at 1 s.
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Ok(ZERO));
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Ok(ZERO));
        assert_eq!(delay(c.acquire(t0, ZERO, 5 * SEC)), Ok(SEC));
        // 3.5 s shares a window only with 4 s: two, which fits.
        assert_eq!(delay(c.acquire(t0, 3500 * MS, 5 * SEC)), Ok(3500 * MS));
        // Now [3.5 s, 4.5 s) holds two: 3.9 s moves to 4.5 s.
        assert_eq!(delay(c.acquire(t0, 3900 * MS, 5 * SEC)), Ok(4500 * MS));
    }

    #[test]
    fn cancel_frees_the_slot_once() {
        let c = calendar(1, SEC, 1);
        let t0 = Instant::now();
        let first = c.acquire(t0, ZERO, ZERO).expect("free");
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Err(SEC));
        c.cancel(first);
        c.cancel(first);
        assert!(c.is_empty());
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Ok(ZERO));
    }

    #[test]
    fn past_bookings_are_forgotten() {
        let c = calendar(1, SEC, 1);
        let t0 = Instant::now();
        c.acquire(t0, ZERO, ZERO).expect("free");
        c.acquire(t0, 2 * SEC, 2 * SEC).expect("free");
        c.retain_recent(t0 + 999 * MS);
        assert_eq!(c.len(), 2, "still shares a window with now");
        c.retain_recent(t0 + SEC);
        assert_eq!(c.len(), 1);
        // Booking forgets too.
        c.acquire(t0 + 3 * SEC, ZERO, ZERO).expect("free");
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn unrepresentable_start_is_denied() {
        let c = calendar(1, SEC, 1);
        assert_eq!(
            delay(c.acquire(Instant::now(), Duration::MAX, Duration::MAX)),
            Err(Duration::MAX)
        );
    }

    proptest! {
        /// Whatever the requests and cancellations, the booked instants
        /// never put more than `burst` in any window.
        #[test]
        fn never_more_than_burst_in_a_window(
            requests in prop::collection::vec((0u64..300, 0u64..2_000, prop::bool::ANY), 1..120),
            burst in 1u32..5,
        ) {
            let per = 100 * MS * burst;
            let c = calendar(burst, per, burst);
            let window = per;
            let mut now = Instant::now();
            let mut booked: Vec<Booking> = Vec::new();
            for (gap, from, cancel) in requests {
                now += Duration::from_millis(gap);
                if let Ok(b) = c.acquire(now, Duration::from_millis(from), 5 * SEC) {
                    if cancel {
                        c.cancel(b);
                    } else {
                        booked.push(b);
                    }
                }
            }
            let mut at: Vec<Instant> = booked.iter().map(|b| b.at).collect();
            at.sort();
            let burst = usize::try_from(burst).expect("small");
            for pair in at.windows(burst + 1) {
                prop_assert!(pair[burst] - pair[0] >= window, "{at:?}");
            }
        }
    }
}
