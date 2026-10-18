//! Bounded queue whose items are leased to holders until acked.
//!
//! Producers reserve room with [`LeaseQueue::try_reserve`] and then commit or
//! cancel the [`Permit`]. Consumers lease items in seq order and ack them or
//! give them back; a lease neither acked, released nor renewed (by
//! [`LeaseQueue::held`]) within `lease_timeout` returns to its original
//! position on [`LeaseQueue::expire`].
//!
//! A permit dropped on another thread while `try_reserve` runs under
//! `OnFull::DropOldest` may not be counted as released yet, so that
//! reservation can evict more items than it needed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{self, AtomicUsize};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::sync::futures::Notified;
use tokio::time::Instant;

/// Queue limits and behaviour; [`LeaseQueue::set_config`] changes them live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfig {
    /// Most items, counting available, leased and reserved ones.
    pub capacity: usize,
    /// Most bytes, counting available, leased and reserved items.
    pub capacity_bytes: usize,
    /// How long a lease lasts before [`LeaseQueue::expire`] takes it back.
    pub lease_timeout: Duration,
    pub ordering: Ordering,
    pub on_full: OnFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ordering {
    /// Items of one key may be leased together.
    None,
    /// At most one item per key is leased at any time; items without a key
    /// are never held back. See [`LeaseQueue::set_config`] for a switch to
    /// it while same-key items are leased together.
    PerKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFull {
    /// Reject the new item.
    Backpressure,
    /// Evict the oldest unleased items to make room for the new one.
    DropOldest,
}

/// Who holds a lease.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HolderId(pub Arc<str>);

/// Room reserved for one item. Committing, cancelling or dropping it releases
/// the reservation.
#[derive(Debug)]
#[must_use = "a permit holds queue capacity until it is committed or cancelled"]
pub struct Permit {
    /// Items `OnFull::DropOldest` evicted to make room.
    pub evicted: usize,
    size: usize,
    reservations: Arc<Reservations>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let r = &self.reservations;
        r.items.fetch_sub(1, atomic::Ordering::Relaxed);
        r.bytes.fetch_sub(self.size, atomic::Ordering::Relaxed);
    }
}

/// Totals of the outstanding permits. They grow only under the queue lock; a
/// permit dropped elsewhere shrinks them without it, which at worst makes a
/// concurrent `try_reserve` see less free room than there is.
#[derive(Debug, Default)]
struct Reservations {
    items: AtomicUsize,
    bytes: AtomicUsize,
}

impl Reservations {
    fn load(&self) -> (usize, usize) {
        let items = self.items.load(atomic::Ordering::Relaxed);
        (items, self.bytes.load(atomic::Ordering::Relaxed))
    }
}

/// Why [`LeaseQueue::try_reserve`] found no room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Full {
    #[error("queue is full: item limit reached")]
    Items,
    #[error("queue is full: byte limit reached")]
    Bytes,
    /// The item is larger than `capacity_bytes`: it never fits.
    #[error("item is larger than the queue's byte limit")]
    TooLarge,
}

/// [`LeaseQueue::commit`] got a permit that another queue granted. Nothing was
/// stored; the permit still holds its reservation in the queue that granted it.
#[derive(thiserror::Error)]
#[error("permit was granted by another queue")]
pub struct ForeignPermit<T> {
    pub permit: Permit,
    pub item: T,
}

impl<T> fmt::Debug for ForeignPermit<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("ForeignPermit");
        s.field("permit", &self.permit).finish_non_exhaustive()
    }
}

/// A leased item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease<T> {
    pub seq: u64,
    pub ack_id: i64,
    pub item: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueueStats {
    pub available: usize,
    pub leased: usize,
    /// Outstanding permits.
    pub reserved: usize,
    /// Bytes of the available and leased items.
    pub bytes: usize,
}

struct Entry<T> {
    item: T,
    key: Option<u64>,
    ack_id: i64,
    size: usize,
    lease: Option<Holding>,
}

impl<T: Clone> Entry<T> {
    fn to_lease(&self, seq: u64) -> Lease<T> {
        Lease {
            seq,
            ack_id: self.ack_id,
            item: self.item.clone(),
        }
    }
}

struct Holding {
    holder: HolderId,
    /// `None` when `now + lease_timeout` overflows: the lease never expires.
    deadline: Option<Instant>,
}

struct Inner<T> {
    cfg: QueueConfig,
    entries: BTreeMap<u64, Entry<T>>,
    /// Seqs of the unleased entries.
    available: BTreeSet<u64>,
    /// Unleased seqs `Ordering::PerKey` may lease: every keyless one, and the
    /// lowest of each key that has no leased item.
    eligible: BTreeSet<u64>,
    /// Unleased seqs of each key.
    queued_per_key: HashMap<u64, BTreeSet<u64>>,
    leases: HashMap<HolderId, BTreeSet<u64>>,
    leased_per_key: HashMap<u64, u32>,
    next_seq: u64,
    /// Bytes of all entries.
    bytes: usize,
    /// Bytes of the leased entries.
    leased_bytes: usize,
}

impl<T> Inner<T> {
    fn leased(&self) -> usize {
        self.entries.len() - self.available.len()
    }

    /// Whether one more item of `size` fits next to `items` stored items of
    /// `bytes` bytes and the `reserved` (items, bytes).
    fn room(
        &self,
        reserved: (usize, usize),
        items: usize,
        bytes: usize,
        size: usize,
    ) -> Result<(), Full> {
        let (reserved_items, reserved_bytes) = reserved;
        if items.saturating_add(reserved_items) >= self.cfg.capacity {
            Err(Full::Items)
        } else if bytes.saturating_add(reserved_bytes).saturating_add(size)
            > self.cfg.capacity_bytes
        {
            Err(Full::Bytes)
        } else {
            Ok(())
        }
    }

    fn holds(&self, holder: &HolderId, seq: u64) -> bool {
        self.leases
            .get(holder)
            .is_some_and(|seqs| seqs.contains(&seq))
    }

    /// The seq of `key` that belongs in `eligible`.
    fn eligible_of(&self, key: u64) -> Option<u64> {
        if self.leased_per_key.contains_key(&key) {
            return None;
        }
        self.queued_per_key.get(&key)?.first().copied()
    }

    /// Runs `change` on an item with `key`, then brings `eligible` up to date
    /// for that key. Every change to a keyed item's availability or lease goes
    /// through here.
    fn rekeyed<R>(&mut self, key: Option<u64>, change: impl FnOnce(&mut Self) -> R) -> R {
        let Some(key) = key else {
            return change(self);
        };
        let before = self.eligible_of(key);
        let result = change(self);
        let after = self.eligible_of(key);
        if before != after {
            if let Some(seq) = before {
                self.eligible.remove(&seq);
            }
            if let Some(seq) = after {
                self.eligible.insert(seq);
            }
        }
        result
    }

    /// Marks `seq` unleased. Call it inside `rekeyed`.
    fn link(&mut self, seq: u64, key: Option<u64>) {
        self.available.insert(seq);
        match key {
            Some(key) => {
                self.queued_per_key.entry(key).or_default().insert(seq);
            }
            None => {
                self.eligible.insert(seq);
            }
        }
    }

    /// Takes `seq` out of the unleased ones, if it is there. Call it inside
    /// `rekeyed`.
    fn unlink(&mut self, seq: u64, key: Option<u64>) {
        if !self.available.remove(&seq) {
            return;
        }
        match key {
            Some(key) => {
                if let Some(seqs) = self.queued_per_key.get_mut(&key) {
                    seqs.remove(&seq);
                    if seqs.is_empty() {
                        self.queued_per_key.remove(&key);
                    }
                }
            }
            None => {
                self.eligible.remove(&seq);
            }
        }
    }

    /// Leases the unleased entry `seq` until `deadline`. The caller records
    /// it in `leases`.
    fn start_lease(&mut self, seq: u64, holder: &HolderId, deadline: Option<Instant>) {
        let Some(entry) = self.entries.get_mut(&seq) else {
            return;
        };
        let key = entry.key;
        entry.lease = Some(Holding {
            holder: holder.clone(),
            deadline,
        });
        self.leased_bytes += entry.size;
        self.rekeyed(key, |inner| {
            inner.unlink(seq, key);
            if let Some(key) = key {
                *inner.leased_per_key.entry(key).or_default() += 1;
            }
        });
    }

    /// Ends the lease on `seq`, if any, without making the entry available.
    /// Call it inside `rekeyed`.
    fn end_lease(&mut self, seq: u64) {
        let Some(entry) = self.entries.get_mut(&seq) else {
            return;
        };
        let Some(holding) = entry.lease.take() else {
            return;
        };
        self.leased_bytes -= entry.size;
        if let Some(key) = entry.key
            && let Some(count) = self.leased_per_key.get_mut(&key)
        {
            *count -= 1;
            if *count == 0 {
                self.leased_per_key.remove(&key);
            }
        }
        if let Some(seqs) = self.leases.get_mut(&holding.holder) {
            seqs.remove(&seq);
            if seqs.is_empty() {
                self.leases.remove(&holding.holder);
            }
        }
    }

    /// Puts the leased entry `seq` back among the available ones.
    fn requeue(&mut self, seq: u64) {
        let Some(key) = self.entries.get(&seq).map(|e| e.key) else {
            return;
        };
        self.rekeyed(key, |inner| {
            inner.end_lease(seq);
            inner.link(seq, key);
        });
    }

    /// Removes the entry `seq`, leased or not.
    fn remove(&mut self, seq: u64) -> Option<Entry<T>> {
        let key = self.entries.get(&seq)?.key;
        let entry = self.rekeyed(key, |inner| {
            inner.end_lease(seq);
            inner.unlink(seq, key);
            inner.entries.remove(&seq)
        })?;
        self.bytes -= entry.size;
        Some(entry)
    }
}

/// Bounded queue of items leased to holders. See the module docs.
pub struct LeaseQueue<T> {
    inner: Mutex<Inner<T>>,
    reservations: Arc<Reservations>,
    notify: Notify,
}

impl<T: Clone> LeaseQueue<T> {
    pub fn new(cfg: QueueConfig) -> Self {
        Self {
            inner: Mutex::new(Inner {
                cfg,
                entries: BTreeMap::new(),
                available: BTreeSet::new(),
                eligible: BTreeSet::new(),
                queued_per_key: HashMap::new(),
                leases: HashMap::new(),
                leased_per_key: HashMap::new(),
                next_seq: 0,
                bytes: 0,
                leased_bytes: 0,
            }),
            reservations: Arc::default(),
            notify: Notify::new(),
        }
    }

    /// Reserves room for one item of `size` bytes. An item larger than
    /// `capacity_bytes` never fits: [`Full::TooLarge`], whatever `on_full`.
    /// With `OnFull::DropOldest` it evicts the oldest unleased items when
    /// that makes room, and nothing when it cannot.
    pub fn try_reserve(&self, size: usize) -> Result<Permit, Full> {
        let mut inner = self.inner.lock();
        if size > inner.cfg.capacity_bytes {
            return Err(Full::TooLarge);
        }
        let reserved = self.reservations.load();
        let mut evicted = 0;
        if let Err(full) = inner.room(reserved, inner.entries.len(), inner.bytes, size) {
            if inner.cfg.on_full == OnFull::Backpressure {
                return Err(full);
            }
            inner.room(reserved, inner.leased(), inner.leased_bytes, size)?;
            while let Err(full) = inner.room(reserved, inner.entries.len(), inner.bytes, size) {
                let oldest = inner.available.first().copied().ok_or(full)?;
                inner.remove(oldest);
                evicted += 1;
            }
        }
        self.reservations
            .items
            .fetch_add(1, atomic::Ordering::Relaxed);
        self.reservations
            .bytes
            .fetch_add(size, atomic::Ordering::Relaxed);
        Ok(Permit {
            evicted,
            size,
            reservations: Arc::clone(&self.reservations),
        })
    }

    /// Releases the room `permit` reserved, as dropping it does, in the queue
    /// that granted it.
    pub fn cancel(&self, permit: Permit) {
        drop(permit);
    }

    /// Stores `item` in the room `permit` reserved, counting the reserved
    /// size, and returns its seq; seqs grow monotonically. A permit another
    /// queue granted stores nothing and comes back with the item.
    pub fn commit(
        &self,
        permit: Permit,
        item: T,
        key: Option<u64>,
        ack_id: i64,
    ) -> Result<u64, ForeignPermit<T>> {
        if !Arc::ptr_eq(&permit.reservations, &self.reservations) {
            return Err(ForeignPermit { permit, item });
        }
        let size = permit.size;
        let seq = {
            let mut inner = self.inner.lock();
            let seq = inner.next_seq;
            inner.next_seq += 1;
            inner.entries.insert(
                seq,
                Entry {
                    item,
                    key,
                    ack_id,
                    size,
                    lease: None,
                },
            );
            inner.bytes += size;
            inner.rekeyed(key, |inner| inner.link(seq, key));
            // The item counts now, so the reservation goes under the same lock.
            drop(permit);
            seq
        };
        self.notify.notify_waiters();
        Ok(seq)
    }

    /// Leases up to `max` available items to `holder` in seq order, until
    /// `now + lease_timeout`. With `Ordering::PerKey` it skips keys that have
    /// a leased item and takes at most one item per key.
    pub fn lease(&self, holder: &HolderId, max: usize, now: Instant) -> Vec<Lease<T>> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let candidates = match inner.cfg.ordering {
            Ordering::None => &inner.available,
            Ordering::PerKey => &inner.eligible,
        };
        let picked: Vec<u64> = candidates.iter().take(max).copied().collect();
        if picked.is_empty() {
            return Vec::new();
        }

        let deadline = now.checked_add(inner.cfg.lease_timeout);
        let mut leases = Vec::with_capacity(picked.len());
        for &seq in &picked {
            inner.start_lease(seq, holder, deadline);
            if let Some(entry) = inner.entries.get(&seq) {
                leases.push(entry.to_lease(seq));
            }
        }
        inner
            .leases
            .entry(holder.clone())
            .or_default()
            .extend(picked);
        leases
    }

    /// The leases `holder` has with `ack_id >= min_ack_id`, in seq order. It
    /// renews each of them until `now + lease_timeout`, so items handed out
    /// again do not expire while the holder processes them.
    pub fn held(&self, holder: &HolderId, min_ack_id: i64, now: Instant) -> Vec<Lease<T>> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let Some(seqs) = inner.leases.get(holder) else {
            return Vec::new();
        };
        let deadline = now.checked_add(inner.cfg.lease_timeout);
        let mut held = Vec::new();
        for &seq in seqs {
            let Some(entry) = inner.entries.get_mut(&seq) else {
                continue;
            };
            if entry.ack_id < min_ack_id {
                continue;
            }
            if let Some(holding) = &mut entry.lease {
                holding.deadline = deadline;
            }
            held.push(entry.to_lease(seq));
        }
        held
    }

    /// Removes the leases `holder` has with `ack_id` below the given one.
    pub fn ack_below(&self, holder: &HolderId, ack_id: i64) -> usize {
        self.ack_below_each(holder, ack_id, |_| {})
    }

    /// As [`LeaseQueue::ack_below`], calling `f` on each removed item in seq
    /// order after the queue lock is released.
    pub fn ack_below_each(&self, holder: &HolderId, ack_id: i64, f: impl FnMut(&T)) -> usize {
        self.ack_where(holder, |entry| entry.ack_id < ack_id, f)
    }

    /// Removes every lease `holder` has.
    pub fn ack_all(&self, holder: &HolderId) -> usize {
        self.ack_all_each(holder, |_| {})
    }

    /// As [`LeaseQueue::ack_all`], calling `f` on each removed item in seq
    /// order after the queue lock is released.
    pub fn ack_all_each(&self, holder: &HolderId, f: impl FnMut(&T)) -> usize {
        self.ack_where(holder, |_| true, f)
    }

    /// Removes the item `seq` if `holder` leases it.
    pub fn ack_seq(&self, holder: &HolderId, seq: u64) -> bool {
        let mut inner = self.inner.lock();
        if !inner.holds(holder, seq) {
            return false;
        }
        let removed = inner.remove(seq);
        let wake =
            inner.cfg.ordering == Ordering::PerKey && removed.is_some_and(|e| e.key.is_some());
        drop(inner);
        if wake {
            self.notify.notify_waiters();
        }
        true
    }

    /// Returns the item `seq`, if `holder` leases it, to its original position.
    pub fn release_seq(&self, holder: &HolderId, seq: u64) -> bool {
        let mut inner = self.inner.lock();
        if !inner.holds(holder, seq) {
            return false;
        }
        inner.requeue(seq);
        drop(inner);
        self.notify.notify_waiters();
        true
    }

    /// Returns the leases whose deadline is at or before `now` to their
    /// original positions.
    pub fn expire(&self, now: Instant) -> usize {
        let mut inner = self.inner.lock();
        let expired: Vec<u64> = (inner.leases.values().flatten())
            .copied()
            .filter(|seq| {
                let deadline = inner
                    .entries
                    .get(seq)
                    .and_then(|e| e.lease.as_ref()?.deadline);
                deadline.is_some_and(|deadline| deadline <= now)
            })
            .collect();
        for &seq in &expired {
            inner.requeue(seq);
        }
        drop(inner);
        if !expired.is_empty() {
            self.notify.notify_waiters();
        }
        expired.len()
    }

    /// Resolves after the next commit, release, expiry, config change, or ack
    /// that frees a key under `Ordering::PerKey`. Only futures that exist at
    /// that moment wake, so `enable` the future before looking at the queue.
    pub fn notified(&self) -> Notified<'_> {
        self.notify.notified()
    }

    pub fn stats(&self) -> QueueStats {
        let inner = self.inner.lock();
        QueueStats {
            available: inner.available.len(),
            leased: inner.leased(),
            reserved: self.reservations.load().0,
            bytes: inner.bytes,
        }
    }

    /// Drops every unleased item.
    pub fn purge(&self) -> usize {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let available = std::mem::take(&mut inner.available);
        inner.eligible.clear();
        inner.queued_per_key.clear();
        for seq in &available {
            if let Some(entry) = inner.entries.remove(seq) {
                inner.bytes -= entry.size;
            }
        }
        available.len()
    }

    /// How many items, leased or not, match `pred`.
    pub fn count_where(&self, pred: impl Fn(&T) -> bool) -> usize {
        let inner = self.inner.lock();
        inner.entries.values().filter(|e| pred(&e.item)).count()
    }

    /// Drops the unleased items that match `pred`.
    pub fn purge_where(&self, pred: impl Fn(&T) -> bool) -> usize {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let matched: Vec<u64> = (inner.available.iter().copied())
            .filter(|seq| inner.entries.get(seq).is_some_and(|e| pred(&e.item)))
            .collect();
        for &seq in &matched {
            inner.remove(seq);
        }
        matched.len()
    }

    /// Removes every item, leased or not, and returns them in seq order.
    /// Outstanding permits stay valid.
    pub fn drain_all(&self) -> Vec<T> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        inner.available.clear();
        inner.eligible.clear();
        inner.queued_per_key.clear();
        inner.leases.clear();
        inner.leased_per_key.clear();
        inner.bytes = 0;
        inner.leased_bytes = 0;
        std::mem::take(&mut inner.entries)
            .into_values()
            .map(|e| e.item)
            .collect()
    }

    /// Applies `cfg` from the next call on and never drops items: above a
    /// lowered capacity, reservations fail (or evict, with
    /// `OnFull::DropOldest`) until the queue is back under it. Existing leases
    /// keep their deadlines, and a switch to `Ordering::PerKey` leaves items
    /// of one key that are already leased together leased until those leases
    /// end; the key gets no new lease before then.
    pub fn set_config(&self, cfg: QueueConfig) {
        self.inner.lock().cfg = cfg;
        self.notify.notify_waiters();
    }

    /// Removes the leases of `holder` that match `pred`, then calls `f` on
    /// each removed item outside the lock.
    fn ack_where(
        &self,
        holder: &HolderId,
        pred: impl Fn(&Entry<T>) -> bool,
        mut f: impl FnMut(&T),
    ) -> usize {
        let mut inner = self.inner.lock();
        let Some(seqs) = inner.leases.get(holder) else {
            return 0;
        };
        let acked: Vec<u64> = (seqs.iter().copied())
            .filter(|seq| inner.entries.get(seq).is_some_and(&pred))
            .collect();
        let mut removed = Vec::with_capacity(acked.len());
        let mut freed_key = false;
        for &seq in &acked {
            if let Some(entry) = inner.remove(seq) {
                freed_key |= entry.key.is_some();
                removed.push(entry.item);
            }
        }
        let wake = freed_key && inner.cfg.ordering == Ordering::PerKey;
        drop(inner);
        if wake {
            self.notify.notify_waiters();
        }
        for item in &removed {
            f(item);
        }
        acked.len()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::pin::{Pin, pin};
    use std::task::{Context, Waker};

    use proptest::prelude::*;
    use proptest::sample::Index;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(30);

    fn holder(name: &str) -> HolderId {
        HolderId(Arc::from(name))
    }

    fn config(capacity: usize, capacity_bytes: usize) -> QueueConfig {
        QueueConfig {
            capacity,
            capacity_bytes,
            lease_timeout: TIMEOUT,
            ordering: Ordering::None,
            on_full: OnFull::Backpressure,
        }
    }

    fn queue(cfg: QueueConfig) -> LeaseQueue<&'static str> {
        LeaseQueue::new(cfg)
    }

    fn push<T: Clone>(
        q: &LeaseQueue<T>,
        item: T,
        key: Option<u64>,
        ack_id: i64,
        size: usize,
    ) -> u64 {
        let permit = q.try_reserve(size).expect("room for the item");
        q.commit(permit, item, key, ack_id)
            .expect("permit from this queue")
    }

    fn items<T: Clone>(leases: &[Lease<T>]) -> Vec<T> {
        leases.iter().map(|l| l.item.clone()).collect()
    }

    fn is_ready<F: Future>(fut: Pin<&mut F>) -> bool {
        fut.poll(&mut Context::from_waker(Waker::noop())).is_ready()
    }

    #[test]
    fn commit_then_lease_returns_in_seq_order() {
        let q = queue(config(10, 100));
        let now = Instant::now();
        let a = push(&q, "a", None, 7, 1);
        let b = push(&q, "b", None, 3, 1);
        let c = push(&q, "c", None, 9, 1);
        assert!(a < b && b < c);
        assert_eq!(
            q.lease(&holder("h1"), 10, now),
            vec![
                Lease {
                    seq: a,
                    ack_id: 7,
                    item: "a"
                },
                Lease {
                    seq: b,
                    ack_id: 3,
                    item: "b"
                },
                Lease {
                    seq: c,
                    ack_id: 9,
                    item: "c"
                },
            ]
        );
        assert!(q.lease(&holder("h2"), 10, now).is_empty());
        assert_eq!(
            q.stats(),
            QueueStats {
                available: 0,
                leased: 3,
                reserved: 0,
                bytes: 3
            }
        );
    }

    #[test]
    fn purge_where_drops_matching_unleased_items_only() {
        let mut cfg = config(10, 100);
        cfg.ordering = Ordering::PerKey;
        let q = queue(cfg);
        let now = Instant::now();
        push(&q, "a1", Some(1), 1, 1);
        push(&q, "b1", Some(1), 2, 2);
        push(&q, "a2", Some(2), 3, 4);
        push(&q, "b2", None, 4, 8);
        // a1 leased: it stays, though it matches.
        assert_eq!(items(&q.lease(&holder("h"), 1, now)), ["a1"]);
        assert_eq!(q.purge_where(|item| item.starts_with('a')), 1);
        assert_eq!(
            q.stats(),
            QueueStats {
                available: 2,
                leased: 1,
                reserved: 0,
                bytes: 11
            }
        );
        // Key 1 waits for a1; the rest leases in order.
        assert_eq!(items(&q.lease(&holder("h"), 10, now)), ["b2"]);
        assert_eq!(q.ack_below(&holder("h"), 2), 1);
        assert_eq!(items(&q.lease(&holder("h"), 10, now)), ["b1"]);
    }

    #[test]
    fn backpressure_full_items() {
        let q = queue(config(2, 100));
        push(&q, "a", None, 1, 1);
        let _permit = q.try_reserve(1).unwrap();
        assert_eq!(q.try_reserve(1).unwrap_err(), Full::Items);
        q.lease(&holder("h1"), 1, Instant::now());
        assert_eq!(
            q.try_reserve(1).unwrap_err(),
            Full::Items,
            "leased items count"
        );
        assert_eq!(
            q.stats(),
            QueueStats {
                available: 0,
                leased: 1,
                reserved: 1,
                bytes: 1
            }
        );
    }

    #[test]
    fn backpressure_full_bytes() {
        let q = queue(config(10, 10));
        push(&q, "a", None, 1, 6);
        assert_eq!(q.try_reserve(5).unwrap_err(), Full::Bytes);
        let permit = q.try_reserve(4).unwrap();
        assert_eq!(permit.evicted, 0);
        assert_eq!(
            q.try_reserve(1).unwrap_err(),
            Full::Bytes,
            "reserved bytes count"
        );
        assert_eq!(
            q.stats(),
            QueueStats {
                available: 1,
                leased: 0,
                reserved: 1,
                bytes: 6
            }
        );
    }

    #[test]
    fn drop_oldest_evicts_unleased_and_reports_evicted() {
        let q = queue(QueueConfig {
            on_full: OnFull::DropOldest,
            ..config(3, 100)
        });
        let now = Instant::now();
        push(&q, "a", None, 1, 1);
        push(&q, "b", None, 2, 1);
        push(&q, "c", None, 3, 1);
        assert_eq!(items(&q.lease(&holder("h1"), 1, now)), ["a"]);
        let permit = q.try_reserve(1).unwrap();
        assert_eq!(permit.evicted, 1);
        q.commit(permit, "d", None, 4).unwrap();
        assert_eq!(items(&q.lease(&holder("h2"), 10, now)), ["c", "d"]);
        assert_eq!(items(&q.peek_held(&holder("h1"))), ["a"]);

        let q = queue(QueueConfig {
            on_full: OnFull::DropOldest,
            ..config(10, 10)
        });
        push(&q, "x", None, 1, 3);
        push(&q, "y", None, 2, 3);
        push(&q, "z", None, 3, 3);
        let permit = q.try_reserve(7).unwrap();
        assert_eq!(permit.evicted, 2);
        assert_eq!(
            q.stats(),
            QueueStats {
                available: 1,
                leased: 0,
                reserved: 1,
                bytes: 3
            }
        );
        assert_eq!(items(&q.lease(&holder("h1"), 10, now)), ["z"]);
    }

    #[test]
    fn drop_oldest_with_all_leased_is_full() {
        let q = queue(QueueConfig {
            on_full: OnFull::DropOldest,
            ..config(2, 100)
        });
        push(&q, "a", None, 1, 1);
        push(&q, "b", None, 2, 1);
        q.lease(&holder("h1"), 10, Instant::now());
        assert_eq!(q.try_reserve(1).unwrap_err(), Full::Items);
        assert_eq!(
            q.stats(),
            QueueStats {
                available: 0,
                leased: 2,
                reserved: 0,
                bytes: 2
            }
        );

        // Evicting every unleased item would still leave too little room, so
        // nothing is evicted.
        let q = queue(QueueConfig {
            on_full: OnFull::DropOldest,
            ..config(10, 10)
        });
        push(&q, "a", None, 1, 4);
        q.lease(&holder("h1"), 10, Instant::now());
        push(&q, "b", None, 2, 4);
        assert_eq!(q.try_reserve(7).unwrap_err(), Full::Bytes);
        assert_eq!(
            q.stats(),
            QueueStats {
                available: 1,
                leased: 1,
                reserved: 0,
                bytes: 8
            }
        );
    }

    #[test]
    fn oversized_item_rejected() {
        for on_full in [OnFull::Backpressure, OnFull::DropOldest] {
            let q = queue(QueueConfig {
                on_full,
                ..config(10, 10)
            });
            assert_eq!(q.try_reserve(11).unwrap_err(), Full::TooLarge);
            assert_eq!(q.stats(), QueueStats::default());
            assert!(q.try_reserve(10).is_ok());
        }

        let q = queue(QueueConfig {
            on_full: OnFull::DropOldest,
            ..config(10, 10)
        });
        push(&q, "a", None, 1, 5);
        assert_eq!(q.try_reserve(11).unwrap_err(), Full::TooLarge);
        assert_eq!(q.stats().available, 1, "an oversized item evicts nothing");
    }

    #[test]
    fn cancel_releases_reservation() {
        let q = queue(config(1, 10));
        let permit = q.try_reserve(10).unwrap();
        assert_eq!(q.stats().reserved, 1);
        assert_eq!(q.try_reserve(0).unwrap_err(), Full::Items);
        q.cancel(permit);
        assert_eq!(q.stats(), QueueStats::default());
        push(&q, "a", None, 1, 10);
        assert_eq!(
            q.stats(),
            QueueStats {
                available: 1,
                leased: 0,
                reserved: 0,
                bytes: 10
            }
        );
    }

    #[test]
    fn dropped_permit_releases_reservation() {
        let q = queue(config(1, 10));
        drop(q.try_reserve(10).unwrap());
        assert_eq!(q.stats(), QueueStats::default());
        push(&q, "a", None, 1, 10);
        assert_eq!(
            q.stats(),
            QueueStats {
                available: 1,
                leased: 0,
                reserved: 0,
                bytes: 10
            }
        );
    }

    #[test]
    fn commit_rejects_foreign_permit() {
        let q1 = queue(config(1, 10));
        let q2 = queue(config(1, 10));
        let permit = q1.try_reserve(4).unwrap();
        let err = q2.commit(permit, "a", None, 1).unwrap_err();
        assert_eq!(err.item, "a");
        assert_eq!(q2.stats(), QueueStats::default(), "nothing stored");
        assert_eq!(q1.stats().reserved, 1, "the permit keeps its reservation");
        let seq = q1.commit(err.permit, err.item, None, 1).unwrap();
        assert_eq!(q1.lease(&holder("h1"), 1, Instant::now())[0].seq, seq);
        assert_eq!(
            q1.stats(),
            QueueStats {
                available: 0,
                leased: 1,
                reserved: 0,
                bytes: 4
            },
            "the item counts its reserved size"
        );
    }

    #[test]
    fn ack_below_each_visits_removed_items_in_seq_order() {
        let q = queue(config(10, 100));
        let now = Instant::now();
        let (h1, h2) = (holder("h1"), holder("h2"));
        for (item, ack_id) in [("a", 10), ("b", 11), ("c", 12), ("d", 13)] {
            push(&q, item, None, ack_id, 1);
        }
        q.lease(&h1, 3, now);
        q.lease(&h2, 1, now);
        let mut seen = Vec::new();
        assert_eq!(q.ack_below_each(&h1, 12, |item| seen.push(*item)), 2);
        assert_eq!(seen, ["a", "b"]);
        seen.clear();
        assert_eq!(q.ack_below_each(&h2, 100, |item| seen.push(*item)), 1);
        assert_eq!(seen, ["d"]);
        assert_eq!(items(&q.peek_held(&h1)), ["c"]);
    }

    #[test]
    fn ack_all_each_visits_every_removed_item() {
        let q = queue(config(10, 100));
        let now = Instant::now();
        let h = holder("h");
        for (item, ack_id) in [("a", 1), ("b", 2), ("c", 3)] {
            push(&q, item, None, ack_id, 1);
        }
        q.lease(&h, 2, now);
        let mut seen = Vec::new();
        assert_eq!(q.ack_all_each(&h, |item| seen.push(*item)), 2);
        assert_eq!(seen, ["a", "b"]);
        assert_eq!(q.ack_all_each(&holder("other"), |_| unreachable!()), 0);
        assert_eq!(q.stats().available, 1);
    }

    #[test]
    fn ack_below_removes_only_holders_leases() {
        let q = queue(config(10, 100));
        let now = Instant::now();
        let (h1, h2) = (holder("h1"), holder("h2"));
        for (item, ack_id) in [("a", 10), ("b", 11), ("c", 12), ("d", 13)] {
            push(&q, item, None, ack_id, 1);
        }
        assert_eq!(items(&q.lease(&h1, 2, now)), ["a", "b"]);
        assert_eq!(items(&q.lease(&h2, 2, now)), ["c", "d"]);
        push(&q, "e", None, 5, 1);

        assert_eq!(q.ack_below(&h1, 13), 2);
        assert_eq!(items(&q.peek_held(&h2)), ["c", "d"]);
        assert_eq!(q.ack_below(&h2, 13), 1);
        assert_eq!(items(&q.peek_held(&h2)), ["d"]);
        assert_eq!(
            q.stats(),
            QueueStats {
                available: 1,
                leased: 1,
                reserved: 0,
                bytes: 2
            },
            "the unleased item with a low ack id stays"
        );
        assert_eq!(q.ack_all(&h2), 1);
        assert_eq!(q.ack_all(&h2), 0);
        assert_eq!(
            q.stats(),
            QueueStats {
                available: 1,
                leased: 0,
                reserved: 0,
                bytes: 1
            }
        );
    }

    #[test]
    fn held_returns_redelivery_set() {
        let q = queue(config(10, 100));
        let now = Instant::now();
        let (h1, h2) = (holder("h1"), holder("h2"));
        // Ack ids need not grow with seq: Telegram may restart update ids.
        let a = push(&q, "a", None, 100, 1);
        let b = push(&q, "b", None, 5, 1);
        let c = push(&q, "c", None, 101, 1);
        q.lease(&h1, 10, now);

        assert_eq!(
            q.held(&h1, i64::MIN, now),
            vec![
                Lease {
                    seq: a,
                    ack_id: 100,
                    item: "a"
                },
                Lease {
                    seq: b,
                    ack_id: 5,
                    item: "b"
                },
                Lease {
                    seq: c,
                    ack_id: 101,
                    item: "c"
                },
            ]
        );
        assert_eq!(
            q.held(&h1, 100, now),
            vec![
                Lease {
                    seq: a,
                    ack_id: 100,
                    item: "a"
                },
                Lease {
                    seq: c,
                    ack_id: 101,
                    item: "c"
                },
            ]
        );
        assert!(q.held(&h2, i64::MIN, now).is_empty());
        assert_eq!(q.stats().leased, 3, "held leases nothing new");
        assert!(q.lease(&h1, 10, now).is_empty());
    }

    #[test]
    fn held_renews_lease_deadline() {
        let q = queue(config(10, 100));
        let t0 = Instant::now();
        let h1 = holder("h1");
        push(&q, "a", None, 1, 1);
        push(&q, "b", None, 2, 1);
        q.lease(&h1, 10, t0);

        let t1 = t0 + TIMEOUT - Duration::from_millis(1);
        assert_eq!(items(&q.held(&h1, 2, t1)), ["b"]);
        assert_eq!(
            q.expire(t0 + TIMEOUT),
            1,
            "only the unreturned lease expires"
        );
        assert_eq!(items(&q.peek_held(&h1)), ["b"]);
        assert_eq!(q.expire(t1 + TIMEOUT - Duration::from_millis(1)), 0);
        assert_eq!(q.expire(t1 + TIMEOUT), 1, "expires at the renewed deadline");
        assert!(q.peek_held(&h1).is_empty());
    }

    #[test]
    fn expire_returns_items_to_original_position() {
        let q = queue(config(10, 100));
        let t0 = Instant::now();
        let (h1, h2, h3) = (holder("h1"), holder("h2"), holder("h3"));
        let a = push(&q, "a", None, 1, 1);
        push(&q, "b", None, 2, 1);
        push(&q, "c", None, 3, 1);
        assert_eq!(items(&q.lease(&h1, 1, t0)), ["a"]);
        assert_eq!(items(&q.lease(&h2, 1, t0 + Duration::from_secs(10))), ["b"]);

        assert_eq!(q.expire(t0 + TIMEOUT - Duration::from_millis(1)), 0);
        assert_eq!(q.expire(t0 + TIMEOUT), 1);
        assert!(q.peek_held(&h1).is_empty());
        assert!(!q.ack_seq(&h1, a), "an expired lease cannot be acked");
        assert_eq!(items(&q.lease(&h3, 10, t0 + TIMEOUT)), ["a", "c"]);
        assert_eq!(items(&q.peek_held(&h2)), ["b"]);
    }

    #[test]
    fn per_key_one_lease_per_key_across_holders() {
        let q = queue(QueueConfig {
            ordering: Ordering::PerKey,
            ..config(10, 100)
        });
        let now = Instant::now();
        let (h1, h2) = (holder("h1"), holder("h2"));
        let a = push(&q, "a", Some(1), 1, 1);
        push(&q, "b", Some(1), 2, 1);
        push(&q, "c", Some(2), 3, 1);
        push(&q, "d", None, 4, 1);
        push(&q, "e", None, 5, 1);

        assert_eq!(items(&q.lease(&h1, 10, now)), ["a", "c", "d", "e"]);
        assert!(q.lease(&h2, 10, now).is_empty());
        assert!(q.ack_seq(&h1, a));
        assert_eq!(items(&q.lease(&h2, 10, now)), ["b"]);
    }

    #[test]
    fn per_key_none_allows_batches() {
        let q = queue(config(10, 100));
        let now = Instant::now();
        push(&q, "a", Some(1), 1, 1);
        push(&q, "b", Some(1), 2, 1);
        push(&q, "c", Some(1), 3, 1);
        assert_eq!(items(&q.lease(&holder("h1"), 2, now)), ["a", "b"]);
        assert_eq!(items(&q.lease(&holder("h2"), 10, now)), ["c"]);
    }

    #[test]
    fn per_key_reload_keeps_parallel_leases_until_they_end() {
        let q = queue(config(10, 100));
        let now = Instant::now();
        let (h1, h2) = (holder("h1"), holder("h2"));
        let a = push(&q, "a", Some(1), 1, 1);
        let b = push(&q, "b", Some(1), 2, 1);
        push(&q, "c", Some(1), 3, 1);
        push(&q, "d", Some(2), 4, 1);
        assert_eq!(items(&q.lease(&h1, 2, now)), ["a", "b"]);

        q.set_config(QueueConfig {
            ordering: Ordering::PerKey,
            ..config(10, 100)
        });
        assert_eq!(items(&q.peek_held(&h1)), ["a", "b"], "both stay leased");
        assert_eq!(items(&q.lease(&h2, 10, now)), ["d"]);
        assert!(q.ack_seq(&h1, a));
        assert!(q.lease(&h2, 10, now).is_empty(), "b still holds key 1");
        assert!(q.release_seq(&h1, b));
        assert_eq!(items(&q.lease(&h2, 10, now)), ["b"]);
    }

    #[test]
    fn release_seq_requeues_in_order() {
        let q = queue(config(10, 100));
        let now = Instant::now();
        let (h1, h2) = (holder("h1"), holder("h2"));
        let a = push(&q, "a", None, 1, 1);
        push(&q, "b", None, 2, 1);
        push(&q, "c", None, 3, 1);
        assert_eq!(items(&q.lease(&h1, 2, now)), ["a", "b"]);

        assert!(!q.release_seq(&h2, a), "only the holder releases");
        assert!(q.release_seq(&h1, a));
        assert!(!q.release_seq(&h1, a), "already released");
        assert!(!q.release_seq(&h1, u64::MAX));
        assert_eq!(items(&q.lease(&h2, 10, now)), ["a", "c"]);
        assert_eq!(items(&q.peek_held(&h1)), ["b"]);
    }

    #[test]
    fn set_config_shrink_keeps_items() {
        let q = queue(config(4, 100));
        let h1 = holder("h1");
        for (item, ack_id) in [("a", 1), ("b", 2), ("c", 3), ("d", 4)] {
            push(&q, item, None, ack_id, 1);
        }
        q.set_config(config(2, 100));
        assert_eq!(
            q.stats(),
            QueueStats {
                available: 4,
                leased: 0,
                reserved: 0,
                bytes: 4
            }
        );
        assert_eq!(q.try_reserve(1).unwrap_err(), Full::Items);
        assert_eq!(
            items(&q.lease(&h1, 10, Instant::now())),
            ["a", "b", "c", "d"]
        );
        assert_eq!(q.ack_below(&h1, 4), 3);
        assert!(q.try_reserve(1).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn notified_wakes_on_commit_and_expire() {
        let q = queue(config(10, 100));
        let mut on_commit = pin!(q.notified());
        on_commit.as_mut().enable();
        assert!(!is_ready(on_commit.as_mut()));
        push(&q, "a", None, 1, 1);
        assert!(is_ready(on_commit.as_mut()));

        q.lease(&holder("h1"), 1, Instant::now());
        let mut on_expire = pin!(q.notified());
        on_expire.as_mut().enable();
        tokio::time::advance(TIMEOUT - Duration::from_millis(1)).await;
        assert_eq!(q.expire(Instant::now()), 0);
        assert!(!is_ready(on_expire.as_mut()));
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(q.expire(Instant::now()), 1);
        assert!(is_ready(on_expire.as_mut()));
    }

    #[test]
    fn notified_wakes_on_release_and_per_key_ack() {
        let q = queue(QueueConfig {
            ordering: Ordering::PerKey,
            ..config(10, 100)
        });
        let now = Instant::now();
        let h1 = holder("h1");
        let a = push(&q, "a", Some(1), 1, 1);
        let b = push(&q, "b", Some(1), 2, 1);
        q.lease(&h1, 10, now);

        let mut on_release = pin!(q.notified());
        on_release.as_mut().enable();
        assert!(q.release_seq(&h1, a));
        assert!(is_ready(on_release.as_mut()));

        assert_eq!(items(&q.lease(&h1, 10, now)), ["a"]);
        let mut on_ack = pin!(q.notified());
        on_ack.as_mut().enable();
        assert!(q.ack_seq(&h1, a));
        assert!(is_ready(on_ack.as_mut()), "the ack frees key 1 for b");
        assert_eq!(q.lease(&h1, 10, now)[0].seq, b);
    }

    impl<T: Clone> LeaseQueue<T> {
        /// Whether the per-key indexes match what the entries imply.
        fn indexes_are_consistent(&self) -> bool {
            let inner = self.inner.lock();
            let mut queued: HashMap<u64, BTreeSet<u64>> = HashMap::new();
            let mut leased: HashMap<u64, u32> = HashMap::new();
            let mut eligible = BTreeSet::new();
            for (&seq, entry) in &inner.entries {
                match (entry.key, entry.lease.is_some()) {
                    (Some(key), true) => *leased.entry(key).or_default() += 1,
                    (Some(key), false) => {
                        queued.entry(key).or_default().insert(seq);
                    }
                    (None, true) => {}
                    (None, false) => {
                        eligible.insert(seq);
                    }
                }
            }
            for (key, seqs) in &queued {
                if !leased.contains_key(key) {
                    eligible.extend(seqs.first());
                }
            }
            queued == inner.queued_per_key
                && leased == inner.leased_per_key
                && eligible == inner.eligible
        }

        /// The leases of `holder` in seq order, without renewing them.
        fn peek_held(&self, holder: &HolderId) -> Vec<Lease<T>> {
            let inner = self.inner.lock();
            let Some(seqs) = inner.leases.get(holder) else {
                return Vec::new();
            };
            (seqs.iter())
                .filter_map(|&seq| Some(inner.entries.get(&seq)?.to_lease(seq)))
                .collect()
        }
    }

    const HOLDERS: [&str; 3] = ["h0", "h1", "h2"];
    const MODEL_TIMEOUT: Duration = Duration::from_secs(10);

    #[derive(Debug, Clone)]
    enum Op {
        Reserve {
            size: usize,
        },
        Commit {
            permit: Index,
            key: Option<u64>,
            ack_id: i64,
        },
        Cancel {
            permit: Index,
        },
        DropPermit {
            permit: Index,
        },
        Lease {
            holder: usize,
            max: usize,
        },
        Held {
            holder: usize,
            min_ack_id: i64,
        },
        AckBelow {
            holder: usize,
            ack_id: i64,
        },
        AckAll {
            holder: usize,
        },
        AckSeq {
            holder: usize,
            entry: Index,
        },
        Release {
            holder: usize,
            entry: Index,
        },
        Expire {
            advance_secs: u64,
        },
        Purge,
    }

    fn op_strategy(holders: usize) -> impl Strategy<Value = Op> {
        let holder = 0..holders;
        let ack_id = -3i64..20;
        prop_oneof![
            3 => (0usize..=8).prop_map(|size| Op::Reserve { size }),
            3 => (any::<Index>(), prop::option::of(0u64..4), ack_id.clone())
                .prop_map(|(permit, key, ack_id)| Op::Commit { permit, key, ack_id }),
            1 => any::<Index>().prop_map(|permit| Op::Cancel { permit }),
            1 => any::<Index>().prop_map(|permit| Op::DropPermit { permit }),
            3 => (holder.clone(), 0usize..4).prop_map(|(holder, max)| Op::Lease { holder, max }),
            1 => (holder.clone(), ack_id.clone())
                .prop_map(|(holder, min_ack_id)| Op::Held { holder, min_ack_id }),
            1 => (holder.clone(), ack_id).prop_map(|(holder, ack_id)| Op::AckBelow { holder, ack_id }),
            1 => holder.clone().prop_map(|holder| Op::AckAll { holder }),
            2 => (holder.clone(), any::<Index>()).prop_map(|(holder, entry)| Op::AckSeq { holder, entry }),
            1 => (holder, any::<Index>()).prop_map(|(holder, entry)| Op::Release { holder, entry }),
            1 => (0u64..15).prop_map(|advance_secs| Op::Expire { advance_secs }),
            1 => Just(Op::Purge),
        ]
    }

    fn case_strategy() -> impl Strategy<Value = (QueueConfig, usize, Vec<Op>)> {
        let ordering = prop_oneof![Just(Ordering::None), Just(Ordering::PerKey)];
        let on_full = prop_oneof![Just(OnFull::Backpressure), Just(OnFull::DropOldest)];
        (1usize..=5, 1usize..=16, ordering, on_full, 2usize..=3).prop_flat_map(
            |(capacity, capacity_bytes, ordering, on_full, holders)| {
                let cfg = QueueConfig {
                    capacity,
                    capacity_bytes,
                    lease_timeout: MODEL_TIMEOUT,
                    ordering,
                    on_full,
                };
                (
                    Just(cfg),
                    Just(holders),
                    prop::collection::vec(op_strategy(holders), 1..40),
                )
            },
        )
    }

    #[derive(Debug)]
    struct ModelEntry {
        seq: u64,
        item: u64,
        key: Option<u64>,
        ack_id: i64,
        size: usize,
        /// Holder index and deadline.
        lease: Option<(usize, Instant)>,
    }

    impl ModelEntry {
        fn held_by(&self, holder: usize) -> bool {
            self.lease.is_some_and(|(h, _)| h == holder)
        }

        fn to_lease(&self) -> Lease<u64> {
            Lease {
                seq: self.seq,
                ack_id: self.ack_id,
                item: self.item,
            }
        }
    }

    /// Naive reference: entries in seq order, permits by pool index.
    struct Model {
        cfg: QueueConfig,
        entries: Vec<ModelEntry>,
        reserved: Vec<usize>,
    }

    impl Model {
        fn bytes(&self, leased_only: bool) -> usize {
            let entries = self.entries.iter();
            entries
                .filter(|e| !leased_only || e.lease.is_some())
                .map(|e| e.size)
                .sum()
        }

        fn room(&self, items: usize, bytes: usize, size: usize) -> Result<(), Full> {
            let reserved_bytes: usize = self.reserved.iter().sum();
            if items + self.reserved.len() + 1 > self.cfg.capacity {
                Err(Full::Items)
            } else if bytes + reserved_bytes + size > self.cfg.capacity_bytes {
                Err(Full::Bytes)
            } else {
                Ok(())
            }
        }

        fn reserve(&mut self, size: usize) -> Result<usize, Full> {
            if size > self.cfg.capacity_bytes {
                return Err(Full::TooLarge);
            }
            let mut evicted = 0;
            if let Err(full) = self.room(self.entries.len(), self.bytes(false), size) {
                if self.cfg.on_full == OnFull::Backpressure {
                    return Err(full);
                }
                let leased = self.entries.iter().filter(|e| e.lease.is_some()).count();
                self.room(leased, self.bytes(true), size)?;
                while self
                    .room(self.entries.len(), self.bytes(false), size)
                    .is_err()
                {
                    let oldest = self.entries.iter().position(|e| e.lease.is_none());
                    self.entries.remove(oldest.expect("an unleased item"));
                    evicted += 1;
                }
            }
            self.reserved.push(size);
            Ok(evicted)
        }

        fn lease(&mut self, holder: usize, max: usize, now: Instant) -> Vec<Lease<u64>> {
            let per_key = self.cfg.ordering == Ordering::PerKey;
            let deadline = now + self.cfg.lease_timeout;
            let mut busy: HashSet<u64> = (self.entries.iter())
                .filter(|e| e.lease.is_some())
                .filter_map(|e| e.key)
                .collect();
            let mut out = Vec::new();
            for e in &mut self.entries {
                if out.len() == max {
                    break;
                }
                if e.lease.is_some() || (per_key && e.key.is_some_and(|k| busy.contains(&k))) {
                    continue;
                }
                busy.extend(e.key);
                e.lease = Some((holder, deadline));
                out.push(e.to_lease());
            }
            out
        }

        /// The leases of `holder` with `ack_id >= min_ack_id`, renewed until
        /// `now + lease_timeout`.
        fn held(&mut self, holder: usize, min_ack_id: i64, now: Instant) -> Vec<Lease<u64>> {
            let deadline = now + self.cfg.lease_timeout;
            (self.entries.iter_mut())
                .filter(|e| e.held_by(holder) && e.ack_id >= min_ack_id)
                .map(|e| {
                    e.lease = Some((holder, deadline));
                    e.to_lease()
                })
                .collect()
        }

        fn peek_held(&self, holder: usize) -> Vec<Lease<u64>> {
            (self.entries.iter())
                .filter(|e| e.held_by(holder))
                .map(ModelEntry::to_lease)
                .collect()
        }

        fn remove_where(&mut self, pred: impl Fn(&ModelEntry) -> bool) -> usize {
            let before = self.entries.len();
            self.entries.retain(|e| !pred(e));
            before - self.entries.len()
        }

        fn release(&mut self, holder: usize, seq: u64) -> bool {
            let found = (self.entries.iter_mut()).find(|e| e.seq == seq && e.held_by(holder));
            found.map(|e| e.lease = None).is_some()
        }

        fn expire(&mut self, now: Instant) -> usize {
            let mut expired = 0;
            for e in &mut self.entries {
                if e.lease.is_some_and(|(_, deadline)| deadline <= now) {
                    e.lease = None;
                    expired += 1;
                }
            }
            expired
        }

        fn stats(&self) -> QueueStats {
            let leased = self.entries.iter().filter(|e| e.lease.is_some()).count();
            QueueStats {
                available: self.entries.len() - leased,
                leased,
                reserved: self.reserved.len(),
                bytes: self.bytes(false),
            }
        }

        /// The seq of the entry `index` picks, or one no entry has.
        fn pick(&self, index: Index) -> u64 {
            match self.entries.len() {
                0 => u64::MAX,
                len => self.entries[index.index(len)].seq,
            }
        }
    }

    fn check_invariants(
        q: &LeaseQueue<u64>,
        model: &Model,
        holders: &[HolderId],
    ) -> Result<(), TestCaseError> {
        let stats = q.stats();
        prop_assert_eq!(stats, model.stats());
        let cfg = model.cfg;
        prop_assert!(stats.available + stats.leased + stats.reserved <= cfg.capacity);
        prop_assert!(stats.bytes + model.reserved.iter().sum::<usize>() <= cfg.capacity_bytes);

        let keys: HashMap<u64, Option<u64>> =
            model.entries.iter().map(|e| (e.seq, e.key)).collect();
        let mut leased = HashSet::new();
        let mut leased_per_key = HashMap::<u64, usize>::new();
        for (i, h) in holders.iter().enumerate() {
            let held = q.peek_held(h);
            prop_assert_eq!(&held, &model.peek_held(i));
            for lease in held {
                prop_assert!(leased.insert(lease.seq), "seq {} leased twice", lease.seq);
                if let Some(key) = keys[&lease.seq] {
                    *leased_per_key.entry(key).or_default() += 1;
                }
            }
        }
        prop_assert_eq!(leased.len(), stats.leased);
        prop_assert!(q.indexes_are_consistent());
        if cfg.ordering == Ordering::PerKey {
            prop_assert!(
                leased_per_key.values().all(|&n| n == 1),
                "{:?}",
                leased_per_key
            );
        }
        Ok(())
    }

    fn run_model(cfg: QueueConfig, holder_count: usize, ops: Vec<Op>) -> Result<(), TestCaseError> {
        let q = LeaseQueue::<u64>::new(cfg);
        let mut model = Model {
            cfg,
            entries: Vec::new(),
            reserved: Vec::new(),
        };
        let holders: Vec<HolderId> = HOLDERS[..holder_count].iter().map(|h| holder(h)).collect();
        // `permits[i]` reserved `model.reserved[i]` bytes.
        let mut permits: Vec<Permit> = Vec::new();
        let mut now = Instant::now();
        let mut next_item = 0;
        let mut last_seq = None;
        let (mut committed, mut removed) = (0, 0);

        for op in ops {
            match op {
                Op::Reserve { size } => {
                    let got = q.try_reserve(size).map(|permit| {
                        let evicted = permit.evicted;
                        permits.push(permit);
                        evicted
                    });
                    prop_assert_eq!(got, model.reserve(size));
                    removed += got.unwrap_or(0);
                }
                Op::Commit { .. } | Op::Cancel { .. } | Op::DropPermit { .. }
                    if permits.is_empty() => {}
                Op::Commit {
                    permit,
                    key,
                    ack_id,
                } => {
                    let i = permit.index(permits.len());
                    let size = model.reserved.swap_remove(i);
                    let item = next_item;
                    next_item += 1;
                    let seq = q
                        .commit(permits.swap_remove(i), item, key, ack_id)
                        .expect("permit from this queue");
                    prop_assert!(
                        last_seq.is_none_or(|last| seq > last),
                        "seq {} not monotonic",
                        seq
                    );
                    last_seq = Some(seq);
                    let lease = None;
                    model.entries.push(ModelEntry {
                        seq,
                        item,
                        key,
                        ack_id,
                        size,
                        lease,
                    });
                    committed += 1;
                }
                Op::Cancel { permit } => {
                    let i = permit.index(permits.len());
                    model.reserved.swap_remove(i);
                    q.cancel(permits.swap_remove(i));
                }
                Op::DropPermit { permit } => {
                    let i = permit.index(permits.len());
                    model.reserved.swap_remove(i);
                    drop(permits.swap_remove(i));
                }
                Op::Lease { holder, max } => {
                    let got = q.lease(&holders[holder], max, now);
                    prop_assert!(got.windows(2).all(|w| w[0].seq < w[1].seq));
                    prop_assert_eq!(got, model.lease(holder, max, now));
                }
                Op::Held { holder, min_ack_id } => {
                    let got = q.held(&holders[holder], min_ack_id, now);
                    prop_assert_eq!(got, model.held(holder, min_ack_id, now));
                }
                Op::AckBelow { holder, ack_id } => {
                    let n = q.ack_below(&holders[holder], ack_id);
                    prop_assert_eq!(
                        n,
                        model.remove_where(|e| e.held_by(holder) && e.ack_id < ack_id)
                    );
                    removed += n;
                }
                Op::AckAll { holder } => {
                    let n = q.ack_all(&holders[holder]);
                    prop_assert_eq!(n, model.remove_where(|e| e.held_by(holder)));
                    removed += n;
                }
                Op::AckSeq { holder, entry } => {
                    let seq = model.pick(entry);
                    let acked = q.ack_seq(&holders[holder], seq);
                    let want = model.remove_where(|e| e.seq == seq && e.held_by(holder)) == 1;
                    prop_assert_eq!(acked, want);
                    removed += usize::from(acked);
                }
                Op::Release { holder, entry } => {
                    let seq = model.pick(entry);
                    prop_assert_eq!(
                        q.release_seq(&holders[holder], seq),
                        model.release(holder, seq)
                    );
                }
                Op::Expire { advance_secs } => {
                    now += Duration::from_secs(advance_secs);
                    prop_assert_eq!(q.expire(now), model.expire(now));
                }
                Op::Purge => {
                    let n = q.purge();
                    prop_assert_eq!(n, model.remove_where(|e| e.lease.is_none()));
                    removed += n;
                }
            }
            check_invariants(&q, &model, &holders)?;
        }

        drop(permits);
        prop_assert_eq!(q.stats().reserved, 0);
        let drained = q.drain_all();
        let remaining: Vec<u64> = model.entries.iter().map(|e| e.item).collect();
        prop_assert_eq!(&drained, &remaining);
        prop_assert_eq!(committed, removed + drained.len(), "an item was lost");
        prop_assert_eq!(q.stats(), QueueStats::default());
        for h in &holders {
            prop_assert!(q.peek_held(h).is_empty());
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        #[test]
        fn queue_model_equivalence((cfg, holders, ops) in case_strategy()) {
            run_model(cfg, holders, ops)?;
        }
    }
}
