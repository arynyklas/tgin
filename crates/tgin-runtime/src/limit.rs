//! Rate limiters that book slots ahead (shaping).
//!
//! [`KeyedLimiter`] is a keyed GCRA. Each key stores its theoretical arrival
//! time (TAT): when its next request would be on schedule. With emission
//! interval `T = per / count` and burst tolerance `τ = T × (burst − 1)`, a
//! request at `now` may start at `max(TAT − τ, now)`; booking it moves the
//! TAT one interval past `max(TAT, now)`.
//!
//! [`SlotCalendar`] keeps one GCRA quota as a calendar of booked instants:
//! it admits exactly the schedules the GCRA admits, and a booking takes the
//! earliest instant at or after the start asked for that keeps the
//! calendar conforming, so one booked far ahead never takes the capacity
//! before it, as moving a single TAT would.

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
        if per.is_zero() {
            return Err(QuotaError::ZeroPeriod);
        }
        Ok(Quota::with_interval(interval(count, per), burst))
    }

    /// `count` requests per second, up to `burst` of them at once: a
    /// [`Quota::new`] that cannot fail.
    #[must_use]
    pub fn per_second(count: NonZeroU32, burst: NonZeroU32) -> Quota {
        Quota::with_interval(interval(count, Duration::from_secs(1)), burst)
    }

    fn with_interval(interval: Duration, burst: NonZeroU32) -> Quota {
        // A tolerance past `Duration::MAX` lets every request start at once,
        // which is what a burst that large means.
        let tolerance = interval
            .checked_mul(burst.get() - 1)
            .unwrap_or(Duration::MAX);
        Quota {
            interval,
            tolerance,
        }
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
/// so rounding must never raise the rate. `per` is positive.
fn interval(count: NonZeroU32, per: Duration) -> Duration {
    let floor = per / count.get();
    match floor.checked_mul(count.get()) {
        Some(exact) if exact == per => floor,
        _ => floor.saturating_add(Duration::from_nanos(1)),
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

/// One GCRA quota (`count` per `per`, `burst` at once) kept as a calendar
/// of booked start instants rather than one theoretical arrival time. It
/// admits exactly the schedules the GCRA admits: with `T = per / count` and
/// `τ = (burst − 1) × T`, sorted starts `t_1 ≤ … ≤ t_n` conform if and
/// only if `t_j − t_i ≥ (j − i + 1 − burst) × T` for every `i < j`. A
/// booking takes the earliest instant at or after the one asked for that
/// keeps the calendar conformant, so a booking far ahead leaves the free
/// capacity before it to others, and booked in time order it starts when
/// the GCRA would start it. Bookings before the latest `now` are folded into
/// a floor (the GCRA's theoretical arrival time after them), which keeps
/// every answer the same; with every booking within `max_delay` of its
/// `now`, it holds at most about `burst + count × max_delay / per`
/// bookings.
pub struct SlotCalendar {
    quota: Quota,
    state: Mutex<Calendar>,
}

/// The bookings of a [`SlotCalendar`].
#[derive(Default)]
struct Calendar {
    /// The GCRA's theoretical arrival time after the bookings folded in:
    /// every booking left must keep its distance to them through it.
    floor: Option<Instant>,
    /// The booked instants not folded in, sorted.
    slots: VecDeque<Instant>,
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
        Ok(SlotCalendar {
            quota: Quota::new(count, per, burst)?,
            state: Mutex::new(Calendar::default()),
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
        let mut state = self.state.lock();
        self.fold_before(&mut state, now);
        let at = self.earliest(&state, start).ok_or(never)?;
        let delay = at.saturating_duration_since(now);
        if delay > max_delay {
            return Err(Denied { retry_after: delay });
        }
        let index = state.slots.partition_point(|slot| *slot <= at);
        state.slots.insert(index, at);
        Ok(Booking { delay, at })
    }

    /// Undoes `booking`, which this calendar's [`SlotCalendar::acquire`]
    /// returned; nothing if it is already gone. A booking that started
    /// before a later `now` is folded into the floor and stays charged, as
    /// a [`KeyedLimiter::cancel`] after a later booking does: that only
    /// errs on the strict side.
    pub fn cancel(&self, booking: Booking) {
        let slots = &mut self.state.lock().slots;
        let index = slots.partition_point(|slot| *slot < booking.at);
        if slots.get(index) == Some(&booking.at) {
            slots.remove(index);
        }
    }

    /// Folds the bookings before `now` into the floor.
    pub fn retain_recent(&self, now: Instant) {
        let mut state = self.state.lock();
        self.fold_before(&mut state, now);
        if state.slots.len() < state.slots.capacity() / 4 {
            state.slots.shrink_to_fit();
        }
    }

    /// The bookings not yet folded into the floor.
    pub fn len(&self) -> usize {
        self.state.lock().slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state.lock().slots.is_empty()
    }

    /// Folds every booking before `now` into the floor: the GCRA's state
    /// after them, `floor = max(floor, t) + T` for each in order. No new
    /// booking starts before `now`, so the bookings left keep their
    /// distance to the folded ones exactly when they keep it to the floor.
    fn fold_before(&self, state: &mut Calendar, now: Instant) {
        let before = state.slots.partition_point(|slot| *slot < now);
        let mut folded = 0;
        for &slot in state.slots.iter().take(before) {
            let Some(next) = state
                .floor
                .map_or(slot, |floor| floor.max(slot))
                .checked_add(self.quota.interval)
            else {
                break;
            };
            state.floor = Some(next);
            folded += 1;
        }
        state.slots.drain(..folded);
    }

    /// The earliest instant at or after `start` whose booking keeps the
    /// calendar conformant. With `s_k = t_k − k × T`, the sorted starts
    /// conform when `s_i − s_j ≤ τ` for every `i < j`, and the floor asks
    /// `s_j ≥ floor − τ` of every booking left, as an `s` of `floor` before
    /// them. Inserting at position `p` shifts the later `s` down by `T`, so
    /// the booking fits when it keeps that with the largest `s` before it,
    /// the smallest shifted `s` after it, and those two with each other.
    fn earliest(&self, state: &Calendar, start: Instant) -> Option<Instant> {
        let slots = &state.slots;
        let nanos = |d: Duration| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX);
        let interval = nanos(self.quota.interval);
        let tolerance = nanos(self.quota.tolerance);
        let origin = [slots.front().copied(), state.floor]
            .into_iter()
            .flatten()
            .fold(start, Instant::min);
        let offset = |at: Instant| nanos(at.saturating_duration_since(origin));
        let index = |k: usize| i128::try_from(k).unwrap_or(i128::MAX);
        let s: Vec<i128> = slots
            .iter()
            .enumerate()
            .map(|(k, slot)| offset(*slot).saturating_sub(index(k).saturating_mul(interval)))
            .collect();
        let mut suffix_min: Vec<Option<i128>> = vec![None; s.len() + 1];
        for k in (0..s.len()).rev() {
            suffix_min[k] = Some(suffix_min[k + 1].map_or(s[k], |m| m.min(s[k])));
        }
        let first = slots.partition_point(|slot| *slot <= start);
        let mut prefix_max = s[..first]
            .iter()
            .copied()
            .chain(state.floor.map(offset))
            .max();
        let start = offset(start);
        for p in first..=s.len() {
            let low = if p == first {
                start
            } else {
                offset(slots[p - 1])
            };
            let shift = index(p).saturating_mul(interval);
            // The shifted later bookings' smallest `s`.
            let after = suffix_min[p].map(|m| m.saturating_sub(interval));
            let spans = match (prefix_max, after) {
                (Some(before), Some(after)) => before.saturating_sub(after) <= tolerance,
                _ => true,
            };
            if spans {
                let lower = prefix_max.map_or(low, |before| {
                    low.max(before.saturating_sub(tolerance).saturating_add(shift))
                });
                let upper =
                    after.map(|after| after.saturating_add(tolerance).saturating_add(shift));
                let below_next = slots.get(p).is_none_or(|next| lower < offset(*next));
                if below_next && upper.is_none_or(|upper| lower <= upper) {
                    let lower = u128::try_from(lower).ok()?;
                    let secs = u64::try_from(lower / 1_000_000_000).ok()?;
                    let subsec = u32::try_from(lower % 1_000_000_000).ok()?;
                    return origin.checked_add(Duration::new(secs, subsec));
                }
            }
            if let Some(&here) = s.get(p) {
                prefix_max = Some(prefix_max.map_or(here, |m| m.max(here)));
            }
        }
        None
    }
}

impl fmt::Debug for SlotCalendar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotCalendar")
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
    fn per_second_is_new_over_one_second() {
        for (count, burst) in [(1, 1), (3, 2), (20, 20)] {
            assert_eq!(
                Ok(Quota::per_second(nz(count), nz(burst))),
                Quota::new(nz(count), Duration::from_secs(1), nz(burst))
            );
        }
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

    fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    fn calendar(count: u32, per: Duration, burst: u32) -> SlotCalendar {
        SlotCalendar::new(nz(count), per, nz(burst)).expect("valid")
    }

    fn delay(result: Result<Booking, Denied>) -> Result<Duration, Duration> {
        result.map(|b| b.delay).map_err(|d| d.retry_after)
    }

    #[test]
    fn burst_then_the_rate() {
        // T = 500 ms, two at once.
        let c = calendar(2, SEC, 2);
        let t0 = Instant::now();
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Ok(ZERO));
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Ok(ZERO));
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Err(500 * MS));
        assert_eq!(delay(c.acquire(t0, ZERO, SEC)), Ok(500 * MS));
        assert_eq!(c.len(), 3);
    }

    /// A burst larger than the rate waits one interval, not the burst's
    /// span: 1 per minute, 4 at once, the fifth waits a minute.
    #[test]
    fn large_burst_waits_one_interval() {
        let c = calendar(1, 60 * SEC, 4);
        let t0 = Instant::now();
        for _ in 0..4 {
            assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Ok(ZERO));
        }
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Err(60 * SEC));
    }

    #[test]
    fn a_booking_ahead_leaves_the_earlier_capacity_free() {
        let c = calendar(2, SEC, 2);
        let t0 = Instant::now();
        // Booked 4 s ahead.
        assert_eq!(delay(c.acquire(t0, 4 * SEC, 5 * SEC)), Ok(4 * SEC));
        // The burst now is still free, and the rate after it.
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Ok(ZERO));
        assert_eq!(delay(c.acquire(t0, ZERO, ZERO)), Ok(ZERO));
        assert_eq!(delay(c.acquire(t0, ZERO, 5 * SEC)), Ok(500 * MS));
        // Around the booking at 4 s: 3.5 s and 3.9 s fit (3.5, 3.9, 4 keep
        // 4 − 3.5 ≥ 500 ms), a third there moves to 4.5 s.
        assert_eq!(delay(c.acquire(t0, 3500 * MS, 5 * SEC)), Ok(3500 * MS));
        assert_eq!(delay(c.acquire(t0, 3900 * MS, 5 * SEC)), Ok(3900 * MS));
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
    fn past_bookings_fold_into_the_floor() {
        let c = calendar(1, SEC, 1);
        let t0 = Instant::now();
        c.acquire(t0, ZERO, ZERO).expect("free");
        c.acquire(t0, 2 * SEC, 2 * SEC).expect("free");
        c.retain_recent(t0 + 999 * MS);
        assert_eq!(c.len(), 1, "the booking at t0 is folded");
        // The floor still holds it: the next free instant is t0 + 1 s.
        assert_eq!(delay(c.acquire(t0 + 999 * MS, ZERO, ZERO)), Err(MS));
        c.retain_recent(t0 + SEC);
        assert_eq!(c.len(), 1);
        // Booking folds too.
        c.acquire(t0 + 3 * SEC, ZERO, ZERO).expect("free");
        assert_eq!(c.len(), 1);
    }

    /// A backlog above the rate never lets the calendar grow: 31/s against
    /// the defaults (30/s, burst 30, max delay 5 s) for 15 minutes keeps at
    /// most `burst + count × max_delay / per` bookings, and admits and
    /// refuses exactly what the GCRA does.
    #[test]
    fn a_sustained_backlog_stays_bounded() {
        let c = calendar(30, SEC, 30);
        let gcra = KeyedLimiter::new(Quota::new(nz(30), SEC, nz(30)).expect("valid"));
        let max_delay = 5 * SEC;
        let t0 = Instant::now();
        let (mut admitted, mut refused, mut most) = (0u32, 0u32, 0usize);
        for k in 0..31 * 900u32 {
            let now = t0 + SEC * k / 31;
            let got = delay(c.acquire(now, ZERO, max_delay));
            let expected = gcra
                .acquire(&0u8, now, max_delay)
                .map(|a| a.delay)
                .map_err(|d| d.retry_after);
            assert_eq!(got, expected, "request {k}");
            match got {
                Ok(_) => admitted += 1,
                Err(_) => refused += 1,
            }
            most = most.max(c.len());
        }
        assert!(most <= 30 + 30 * 5 + 2, "{most} bookings kept");
        assert!(
            admitted > 0 && refused > 0,
            "{admitted} admitted, {refused} refused"
        );
    }

    #[test]
    fn unrepresentable_start_is_denied() {
        let c = calendar(1, SEC, 1);
        assert_eq!(
            delay(c.acquire(Instant::now(), Duration::MAX, Duration::MAX)),
            Err(Duration::MAX)
        );
    }

    /// The pairwise GCRA condition on sorted starts.
    fn conforms(starts: &[Instant], interval: Duration, burst: u32) -> bool {
        (0..starts.len()).all(|i| {
            (i + 1..starts.len()).all(|j| {
                let n = u32::try_from(j - i).unwrap_or(u32::MAX) + 1;
                n <= burst || starts[j] - starts[i] >= interval * (n - burst)
            })
        })
    }

    proptest! {
        /// Booked in time order, the calendar starts each request exactly
        /// when a GCRA does, and refuses the same ones.
        #[test]
        fn in_order_it_is_the_gcra(
            requests in prop::collection::vec((0u64..400, 0u64..3_000), 1..150),
            count in 1u32..5,
            burst in 1u32..6,
        ) {
            let per = SEC;
            let c = calendar(count, per, burst);
            let gcra = KeyedLimiter::new(
                Quota::new(NonZeroU32::new(count).unwrap(), per, NonZeroU32::new(burst).unwrap())
                    .unwrap(),
            );
            let mut now = Instant::now();
            for (gap, max_delay) in requests {
                now += Duration::from_millis(gap);
                let max_delay = Duration::from_millis(max_delay);
                let expected = gcra.acquire(&0u8, now, max_delay).map(|a| a.delay).map_err(|d| d.retry_after);
                prop_assert_eq!(delay(c.acquire(now, ZERO, max_delay)), expected);
            }
        }

        /// Booked out of order and cancelled at random, the booked instants
        /// always conform.
        #[test]
        fn out_of_order_it_conforms(
            requests in prop::collection::vec((0u64..300, 0u64..2_000, prop::bool::ANY), 1..120),
            count in 1u32..5,
            burst in 1u32..5,
        ) {
            let per = SEC;
            let c = calendar(count, per, burst);
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
            prop_assert!(conforms(&at, c.quota.interval, burst), "{at:?}");
        }

        /// Folding the past into the floor changes no answer: the same
        /// requests against the whole unfolded history book the same
        /// instants and are refused alike.
        #[test]
        fn folding_changes_no_answer(
            requests in prop::collection::vec((0u64..300, 0u64..2_000), 1..150),
            count in 1u32..5,
            burst in 1u32..5,
        ) {
            let c = calendar(count, SEC, burst);
            let mut history = Calendar::default();
            let mut now = Instant::now();
            for (gap, from) in requests {
                now += Duration::from_millis(gap);
                let start = now + Duration::from_millis(from);
                let expected = c
                    .earliest(&history, start)
                    .map(|at| at.saturating_duration_since(now))
                    .filter(|d| *d <= 2 * SEC);
                let got = c.acquire(now, Duration::from_millis(from), 2 * SEC).ok();
                prop_assert_eq!(got.map(|b| b.delay), expected);
                if let Some(b) = got {
                    let index = history.slots.partition_point(|slot| *slot <= b.at);
                    history.slots.insert(index, b.at);
                }
            }
        }
    }
}
