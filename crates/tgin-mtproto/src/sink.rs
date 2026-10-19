//! Where an account's ingest hands its update batches over, and the ids the
//! updates get.
//!
//! The caller is the account's one ingest task, which owns the account's
//! [`SessionStore`] and [`UpdateManager`](crate::UpdateManager). For each
//! [`Batch`] the manager returns, in the order returned (that is, batch
//! order), it:
//! 1. persists [`Batch::ahead`] with
//!    [`SessionStore::mark_channels_behind`] (the marks of all batches of
//!    one manager call in one write), before admitting any of them;
//! 2. calls [`BatchSink::admit`] with the account's [`UpdateIds`] and the
//!    same store;
//!    An `Err` ([`IdsError`]) is a storage failure: the id mark could not
//!    be persisted, nothing was admitted, and the batch stays open (neither
//!    committed nor refused). The account fails with a storage error.
//! 3. on [`Admission::Admitted`], persists what
//!    [`committed(id)`](crate::UpdateManager::committed) returns with
//!    [`SessionStore::commit_state`]; on [`Admission::Refused`], calls
//!    [`refused(id)`](crate::UpdateManager::refused) and persists what it
//!    returns the same way. A refused batch is gone from memory; gap
//!    recovery fetches its sequenced updates again, once the box's backoff
//!    is over (1 s, doubling per refusal up to 60 s, reset when the box
//!    next commits a batch). The caller keeps running the manager's fetches
//!    by [`deadline`](crate::UpdateManager::deadline) and
//!    [`next_action`](crate::UpdateManager::next_action), which honour it.
//!
//! The sink never opens the store, never blocks on the network and never
//! awaits; it may write the store (see [`UpdateIds`]).

use crate::error::SessionError;
use crate::session::SessionStore;
use crate::updates::Batch;

/// How many ids [`UpdateIds`] reserves with one write.
pub const ID_BLOCK: i64 = 1024;

/// What became of a whole batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Every update is queued for each consumer its rules route it to (an
    /// update no rule matches counts as handled).
    Admitted,
    /// Nothing of the batch was queued: a consumer queue had no room.
    Refused,
}

/// Admits an account's batches, all of a batch or none of it.
pub trait BatchSink: Send + Sync + 'static {
    /// Offers `batch`, taking its update ids from `ids` (persisted through
    /// `store`). Call it in batch order, from one task per account; see the
    /// [module docs](self) for what to do with the answer.
    ///
    /// # Errors
    ///
    /// The ids could not be handed out ([`UpdateIds::take`]): a storage
    /// failure, never backpressure. Nothing was admitted.
    fn admit(
        &self,
        batch: &Batch,
        ids: &mut UpdateIds,
        store: &SessionStore,
    ) -> Result<Admission, IdsError>;
}

/// Hands out an account's `update_id`s: strictly increasing, starting at 1,
/// and never repeated across restarts. The high-water mark is persisted
/// ahead of use, [`ID_BLOCK`] ids at a time, so ids a run reserved but did
/// not hand out are skipped after a restart.
#[derive(Debug)]
pub struct UpdateIds {
    /// The next id to hand out.
    next: i64,
    /// The persisted high-water mark: every id up to it may be handed out.
    reserved: i64,
}

impl UpdateIds {
    /// Starts after the high-water mark `store` holds.
    ///
    /// # Errors
    ///
    /// The store could not be read.
    pub fn load(store: &SessionStore) -> Result<Self, SessionError> {
        let reserved = store.update_id()?.unwrap_or(0).max(0);
        Ok(Self {
            next: reserved.saturating_add(1),
            reserved,
        })
    }

    /// `n` consecutive ids, as the first one; persists a higher high-water
    /// mark through `store` first when the reserved block runs out. Ids
    /// taken by a batch that is then refused are not handed out again.
    ///
    /// # Errors
    ///
    /// The store could not be written, or the ids ran out (after 2^63):
    /// nothing is handed out.
    pub fn take(&mut self, n: usize, store: &SessionStore) -> Result<i64, IdsError> {
        let n = i64::try_from(n).map_err(|_| IdsError::Exhausted)?;
        let last = self
            .next
            .checked_add(n)
            .and_then(|end| end.checked_sub(1))
            .ok_or(IdsError::Exhausted)?;
        if last > self.reserved {
            let blocks = last.div_euclid(ID_BLOCK) + 1;
            let mark = blocks.checked_mul(ID_BLOCK).unwrap_or(i64::MAX);
            store.set_update_id(mark)?;
            self.reserved = mark;
        }
        let first = self.next;
        self.next = last.saturating_add(1);
        Ok(first)
    }
}

/// Why [`UpdateIds::take`] handed nothing out.
#[derive(Debug, thiserror::Error)]
pub enum IdsError {
    #[error("the update_id high-water mark could not be persisted: {0}")]
    Store(#[from] SessionError),
    #[error("update ids are exhausted")]
    Exhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_id_monotonic_across_restart_skips_allowed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("acct");
        let mut seen = Vec::new();
        {
            let store = SessionStore::open(&dir).expect("open");
            let mut ids = UpdateIds::load(&store).expect("load");
            assert_eq!(ids.take(1, &store).expect("ids"), 1, "ids start at 1");
            assert_eq!(
                store.update_id().expect("read"),
                Some(ID_BLOCK),
                "a block reserved before use"
            );
            seen.push(1);
            // A batch of 3, then one that crosses the block's end.
            let first = ids.take(3, &store).expect("ids");
            seen.extend(first..first + 3);
            assert_eq!(first, 2);
            ids.next = ID_BLOCK - 1;
            let first = ids.take(4, &store).expect("ids");
            assert_eq!(first, ID_BLOCK - 1);
            seen.extend(first..first + 4);
            assert_eq!(
                store.update_id().expect("read"),
                Some(2 * ID_BLOCK),
                "the mark covers the whole batch"
            );
        }
        // A restart: the ids up to the mark were reserved and may have
        // gone out, so the next run starts past it.
        let store = SessionStore::open(&dir).expect("reopen");
        let mut ids = UpdateIds::load(&store).expect("load");
        let first = ids.take(2, &store).expect("ids");
        assert_eq!(first, 2 * ID_BLOCK + 1, "skips, never repeats");
        seen.extend(first..first + 2);
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
        assert_eq!(store.update_id().expect("read"), Some(3 * ID_BLOCK));
    }

    #[test]
    fn one_write_per_block() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::open(&tmp.path().join("acct")).expect("open");
        let mut ids = UpdateIds::load(&store).expect("load");
        for _ in 0..ID_BLOCK {
            ids.take(1, &store).expect("ids");
        }
        assert_eq!(store.update_id().expect("read"), Some(ID_BLOCK));
        assert_eq!(ids.take(1, &store).expect("ids"), ID_BLOCK + 1);
        assert_eq!(store.update_id().expect("read"), Some(2 * ID_BLOCK));
    }
}
