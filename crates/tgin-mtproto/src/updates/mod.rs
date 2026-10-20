//! The update manager: tgin's own update-state handling and gap recovery,
//! by Telegram's rules (<https://core.telegram.org/api/updates>).
//!
//! It is a pure state machine over bytes. It does no I/O: pushes and fetch
//! answers go in, batches and fetch actions come out. The caller
//! - feeds every push ([`feed_push`](UpdateManager::feed_push)), including
//!   the `Updates` answers the sender copies into the push stream, its
//!   synthetic `updatesTooLong` on `new_session_created`, and its
//!   [`CONNECTION_ENDED`] signal when a connection ends;
//! - runs each [`Fetch`] from [`next_action`](UpdateManager::next_action)
//!   on the ingest session and feeds the answer back, or reports
//!   [`fetch_failed`](UpdateManager::fetch_failed) (asked again after a
//!   backoff) or
//!   [`fetch_failed_permanently`](UpdateManager::fetch_failed_permanently);
//!   it calls `next_action` again by [`deadline`](UpdateManager::deadline);
//! - persists the marks of every [`Batch`] a call returned (its
//!   [`ahead`](Batch::ahead) channels with the batch's id as their mark,
//!   and an unknown channel's baseline:
//!   [`SessionStore::mark_channels_behind`](crate::SessionStore::mark_channels_behind))
//!   before it offers the first of them to admission; then offers each
//!   batch to admission, in id order, and reports
//!   [`committed`](UpdateManager::committed) or
//!   [`refused`](UpdateManager::refused), and persists the [`StateDelta`]
//!   either returns ([`SessionStore::commit_state`](crate::SessionStore::commit_state)).
//!   Marks and deltas may be written in any order and by different writers:
//!   a delta clears a channel only up to the mark it names, so a later
//!   batch's mark is never erased by an earlier caught-up report;
//! - before it drops the manager (shutdown, reload), persists the marks of
//!   every batch it was given; unadmitted batches may then be dropped.
//!
//! # Delivery
//!
//! Sequenced updates (`pts`, `qts`, a channel's `pts`; 0 is no sequence)
//! are at least once: a box's state is persisted only after every batch
//! before and including the one that moved it is admitted. A refused batch
//! rolls its boxes back to the last committed state and refetches them, so
//! its sequenced updates come back; its unsequenced ones are counted in
//! [`Dropped::full`]. There is no dedup: replays may repeat updates.
//!
//! The one crash window for a channel: from the moment its push is
//! received (still in the sender's buffers or the pushes channel counts)
//! until the marks of the call that fed it are durable. Telegram treats the
//! push as delivered and only the client knew the channel moved, so a quiet
//! channel (no later post, no signal from Telegram) does not bring it back.
//! Every other crash point replays it (the channel catches up at start).
//!
//! By design, the manager does not owe:
//! - an unknown channel's updates before its first one (its state starts
//!   just before it);
//! - a channel's updates before its fetch failed for good or its answer
//!   could not be decoded (its state is dropped and starts again);
//! - what a `differenceTooLong` or `channelDifferenceTooLong` skips
//!   (Telegram cannot send it any more);
//! - a post lost with a connection whose session Telegram replaced: it
//!   comes back only if Telegram's getDifference signals that channel,
//!   which tgin cannot check.
//!
//! # Catch-up
//!
//! The common box (with `qts` and `seq`) catches up with
//! `updates.getDifference`, at start and on a gap still open after
//! [`GAP_HOLD`]. A channel catches up on a gap in that channel, on
//! `updateChannelTooLong`, or at start when the last run left it behind
//! ([`StoredState::behind`]: a batch moved it past its committed pts, and
//! it never caught up); no other channel catches up at start. Pushes for
//! any box that is not catching up are delivered as they come.
//!
//! # Bytes
//!
//! A pushed update is its raw TL, cut from the container; a short form is
//! the raw short object. A difference's messages become `updateNewMessage`,
//! `updateNewChannelMessage` or `updateNewEncryptedMessage`, composed by
//! bytes (constructor id, the raw message, then the difference's final
//! `pts` and a `pts_count` of 0, or its final `qts`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::ops::Range;

use bytes::Bytes;
use tokio::time::{Duration, Instant};

use crate::TlError;
use crate::decode::{self, BoxKind, PeerId, UpdateFacts};
use crate::retry::Backoff;
use crate::session::{Peer, PeerKind, UpdateState};

mod batch;
mod channels;
mod common;
mod schedule;

pub use batch::{Batch, UpdateBytes};
use schedule::{later, wait_for};

/// How long a gap may stay open before the box fetches its difference;
/// the missing updates may only be reordered (Telegram's 0.5 s).
pub const GAP_HOLD: Duration = Duration::from_millis(500);
/// With no push for this long, the common box fetches its difference.
pub const IDLE_DIFFERENCE: Duration = Duration::from_secs(15 * 60);
/// At most this many `updates.getChannelDifference` calls in flight.
pub const MAX_CHANNEL_FETCHES: usize = 4;
/// A failed fetch, or the refetch of a refused batch, is asked again after
/// this long, doubling per failure or refusal up to [`RETRY_MAX`] (as TDLib
/// backs off getDifference).
pub const RETRY_MIN: Duration = Backoff::FIRST;
pub const RETRY_MAX: Duration = Backoff::MAX;
/// What the sender pushes when a connection that finished its init call
/// ends: the session's update stream may have a gap. Not TL (no schema
/// constructor has this id), so it is never mistaken for Telegram's
/// `updatesTooLong`, which comes on a live session.
pub const CONNECTION_ENDED: &[u8] = b"\0end";
/// Held (out-of-order) updates per box. Beyond it, a sequenced update is
/// dropped from memory; the box's state has not moved past it, so the
/// difference brings it back.
const MAX_HELD: usize = 4096;
/// A channel's mark before the batch that names it is closed.
const MARK_PENDING: u64 = u64::MAX;

const UPDATE_NEW_MESSAGE: u32 = 0x1f2b_0afd;
const UPDATE_NEW_CHANNEL_MESSAGE: u32 = 0x62ba_04d9;
const UPDATE_NEW_ENCRYPTED_MESSAGE: u32 = 0x12bc_bd9a;
/// `Vector<T>` with no items: the `users`/`chats` of a batch whose source
/// has none.
const EMPTY_VECTOR: &[u8] = &[0x15, 0xc4, 0xb5, 0x1c, 0, 0, 0, 0];

/// What the manager starts from, as persisted
/// ([`SessionStore::stored_state`](crate::SessionStore::stored_state)).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoredState {
    /// The common state; `pts` 0 means none was ever fetched.
    pub common: UpdateState,
    /// Each channel's pts, by bare channel id.
    pub channels: Vec<(i64, i32)>,
    /// Users whose access hash is cached, known up front. Leave it empty and
    /// give a [`with_user_lookup`](UpdateManager::with_user_lookup) instead
    /// to keep no set in memory.
    pub known_users: Vec<i64>,
    /// Channels a batch moved ahead of their stored pts that have not
    /// caught up since ([`Batch::ahead`]), each with the id of the batch
    /// that marked it last: each fetches its difference after the
    /// start-up difference, so a crash before their batches were committed
    /// loses none of a quiet channel's updates.
    pub behind: Vec<(i64, u64)>,
}

/// State to persist: each `Some` field and each channel row replaces the
/// stored one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateDelta {
    pub pts: Option<i32>,
    pub qts: Option<i32>,
    pub seq: Option<i32>,
    pub date: Option<i32>,
    /// `(bare channel id, pts)`, one row per channel.
    pub channels: Vec<(i64, i32)>,
    /// Channels caught up with their committed pts, each with the mark it
    /// clears (the id of the batch that marked it): a row marked again
    /// since, by a later batch, stays.
    pub caught_up: Vec<(i64, u64)>,
    /// Channels whose state was dropped (no longer readable, or abandoned
    /// after a fetch failed for good), each with the first batch id after
    /// the drop: delete their stored pts and behind row, unless a batch at
    /// or after that id has marked the channel since (it started again).
    pub forgotten: Vec<(i64, u64)>,
}

impl StateDelta {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pts.is_none()
            && self.qts.is_none()
            && self.seq.is_none()
            && self.date.is_none()
            && self.channels.is_empty()
            && self.caught_up.is_empty()
            && self.forgotten.is_empty()
    }

    fn set_channel(&mut self, channel: i64, pts: i32) {
        match self.channels.iter_mut().find(|(c, _)| *c == channel) {
            Some(row) => row.1 = pts,
            None => self.channels.push((channel, pts)),
        }
    }

    /// `later` over `self`.
    fn merge(&mut self, later: StateDelta) {
        self.pts = later.pts.or(self.pts);
        self.qts = later.qts.or(self.qts);
        self.seq = later.seq.or(self.seq);
        self.date = later.date.or(self.date);
        for (c, p) in later.channels {
            self.set_channel(c, p);
        }
        for (c, g) in later.caught_up {
            match self.caught_up.iter_mut().find(|(k, _)| *k == c) {
                Some(row) => row.1 = row.1.max(g),
                None => self.caught_up.push((c, g)),
            }
        }
        for (c, g) in later.forgotten {
            match self.forgotten.iter_mut().find(|(k, _)| *k == c) {
                Some(row) => row.1 = row.1.max(g),
                None => self.forgotten.push((c, g)),
            }
        }
    }
}

/// A call the ingest session runs for the manager. Only the ingest session
/// calls `updates.getState` or a difference, so two sessions never race on
/// the state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fetch {
    /// `updates.getState`; feed the answer to
    /// [`feed_state`](UpdateManager::feed_state).
    GetState,
    /// `updates.getDifference`; feed the answer to
    /// [`feed_difference`](UpdateManager::feed_difference). Never pass
    /// `pts_total_limit`: its `differenceTooLong` skips updates no
    /// difference brings back.
    GetDifference { pts: i32, qts: i32, date: i32 },
    /// `updates.getChannelDifference` for the channel (the caller supplies
    /// its access hash); feed the answer to
    /// [`feed_channel_difference`](UpdateManager::feed_channel_difference).
    GetChannelDifference { channel: i64, pts: i32 },
}

/// What the manager dropped, for
/// `tgin_mtproto_updates_dropped_total{account,reason}`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    /// `reason="undecodable"`: a push, an update or a fetch answer the
    /// decoder refused (e.g. an unknown flag bit), skipped.
    pub undecodable: u64,
    /// `reason="full"`: unsequenced updates of a refused batch.
    pub full: u64,
}

/// The boxes catching up: the common box (with `qts`) while it waits for
/// or runs `updates.getState` or `getDifference`, and the channels waiting
/// for or running `updates.getChannelDifference` (those left behind by the
/// last run included, until they are queued).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatchingUp {
    pub common: bool,
    pub channels: usize,
}

/// Where updates came from: one push or one fetch answer.
#[derive(Clone)]
struct Group {
    origin: u64,
    users: Bytes,
    chats: Bytes,
}

enum Item {
    Update(UpdateBytes),
    /// A `messages.affected*` answer: state only.
    Affected,
}

/// A sequenced update waiting for its box.
struct Held {
    item: Item,
    pts: i32,
    count: i32,
    /// A live short update: check its users' access hashes when applied.
    check_peer: bool,
    group: Group,
}

impl Held {
    /// The box's state before this update.
    fn before(&self) -> i64 {
        i64::from(self.pts) - i64::from(self.count)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommonFetch {
    Idle,
    WantState,
    State { epoch: u32 },
    WantDifference,
    Difference { epoch: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChannelFetch {
    Idle,
    Wanted,
    InFlight { epoch: u32 },
}

struct Channel {
    /// `None`: unknown; the next update or `updateChannelTooLong` with a
    /// pts sets it.
    pts: Option<i32>,
    held: Vec<Held>,
    deadline: Option<Instant>,
    fetch: ChannelFetch,
    /// An `updateChannelTooLong` came while a call was in flight: one more
    /// fetch follows it.
    again: bool,
    /// A failed fetch is asked again then (with `fetch` Wanted).
    retry_at: Option<Instant>,
    backoff: Backoff,
    /// The backoff of the refetch after a refusal: grows per refusal, and
    /// only a committed batch of the channel resets it.
    refusals: Backoff,
    /// A refused batch's refetch waits until then. Apart from `retry_at`:
    /// a signal that the server is reachable cuts a failed fetch's retry
    /// short, never a refusal's wait for a full consumer.
    refused_until: Option<Instant>,
    /// Bumped when a refused batch rolls the channel back.
    epoch: u32,
}

impl Channel {
    fn new(pts: Option<i32>) -> Self {
        Self {
            pts,
            held: Vec::new(),
            deadline: None,
            fetch: ChannelFetch::Idle,
            epoch: 0,
            again: false,
            retry_at: None,
            backoff: Backoff::default(),
            refusals: Backoff::default(),
            refused_until: None,
        }
    }

    /// When a fetch of the channel may be asked: after both a failed
    /// fetch's retry and a refusal's wait.
    fn ready_at(&self) -> Option<Instant> {
        wait_for(self.retry_at, self.refused_until)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Open,
    Committed,
    Refused,
}

struct Pending {
    id: u64,
    delta: StateDelta,
    /// The common epoch when the batch moved the common state.
    common_epoch: Option<u32>,
    channel_epochs: Vec<(i64, u32)>,
    unsequenced: u64,
    /// It carries no update: nothing to refuse.
    empty: bool,
    status: Status,
}

/// The batch being built, and those done.
struct Out {
    home: Group,
    open: Open,
    batches: Vec<Batch>,
}

struct Open {
    group: Group,
    updates: Vec<UpdateBytes>,
    delta: StateDelta,
    common: bool,
    channels: Vec<i64>,
    unsequenced: u64,
}

impl Open {
    fn new(group: Group) -> Self {
        Self {
            group,
            updates: Vec::new(),
            delta: StateDelta::default(),
            common: false,
            channels: Vec::new(),
            unsequenced: 0,
        }
    }

    fn touch_channel(&mut self, channel: i64, pts: i32) {
        self.delta.set_channel(channel, pts);
        if !self.channels.contains(&channel) {
            self.channels.push(channel);
        }
    }
}

impl Out {
    fn new(home: Group) -> Self {
        Self {
            open: Open::new(home.clone()),
            home,
            batches: Vec::new(),
        }
    }
}

/// How an update reached the manager.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Live,
    /// In a common difference: common and qts updates are covered by its
    /// state.
    Difference,
    /// In this channel's difference: its updates are covered by its pts.
    ChannelDifference(i64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Common {
    Pts,
    Qts,
}

/// Whether a user's access hash is cached: see
/// [`UpdateManager::with_user_lookup`].
type UserLookup = Box<dyn Fn(i64) -> bool + Send + Sync>;

/// One account's update state and gap recovery. See the module docs.
pub struct UpdateManager {
    pts: i32,
    qts: i32,
    seq: i32,
    date: i32,
    held_pts: Vec<Held>,
    held_qts: Vec<Held>,
    /// `(seq_start, seq)` of containers handled while a seq gap is open.
    seq_seen: Vec<(i32, i32)>,
    gap_deadline: Option<Instant>,
    idle_at: Instant,
    fetch: CommonFetch,
    /// A difference was asked for while a call was in flight: one more
    /// follows the chain.
    again: bool,
    /// Bumped when a refused batch rolls the common state back.
    epoch: u32,
    channels: HashMap<i64, Channel>,
    /// Channels waiting for a fetch slot, in arrival order.
    queue: VecDeque<i64>,
    in_flight: usize,
    /// Channels with a gap deadline. Ordered sets here and below: what the
    /// manager does never depends on hash order, so a run replays exactly.
    gapped: BTreeSet<i64>,
    /// Channels waiting for a failed fetch's retry.
    retrying: BTreeSet<i64>,
    /// The common box's failed fetch is asked again then.
    retry_at: Option<Instant>,
    backoff: Backoff,
    /// The backoff of the refetch after a refusal: grows per refusal, and
    /// only a committed batch of the common box resets it.
    refusals: Backoff,
    /// The common box's refetch after a refusal waits until then; apart
    /// from `retry_at`, which `updatesTooLong` cuts short.
    refused_until: Option<Instant>,
    /// Telegram's `FLOOD_WAIT_X` on `updates.getState` or `getDifference`:
    /// no common fetch before then, whatever signal comes.
    common_flood: Option<Instant>,
    /// The same for `updates.getChannelDifference`, for every channel:
    /// Telegram's limit is per method.
    channel_flood: Option<Instant>,
    /// Users known to have a cached access hash: those of the stored state
    /// and, without a [`user_lookup`](Self::with_user_lookup), those seen
    /// since.
    known_users: HashSet<i64>,
    /// Asks the session's cache whether a user's access hash is known; then
    /// nothing is held in memory per user.
    user_lookup: Option<UserLookup>,
    committed: UpdateState,
    committed_channels: HashMap<i64, i32>,
    pending: VecDeque<Pending>,
    next_id: u64,
    next_origin: u64,
    dropped: Dropped,
    /// Channels persisted as behind ([`Batch::ahead`]), not yet caught up.
    /// The value is the mark: the id of the batch that named the channel
    /// last ([`MARK_PENDING`] until that batch is closed).
    ahead: BTreeMap<i64, u64>,
    /// Newly ahead, for the next batch's `ahead`.
    ahead_new: Vec<i64>,
    /// Channels behind at start, queued once the start-up difference ends.
    startup: Vec<i64>,
    /// Channels caught up, with their marks, for the next committed delta.
    caught_up: Vec<(i64, u64)>,
    /// Channels whose state was dropped, for the next committed delta.
    forgotten: Vec<(i64, u64)>,
}

impl UpdateManager {
    /// Starts from the persisted state: with a common state, the first
    /// action is `updates.getDifference`; without one, `updates.getState`.
    /// The channels in [`StoredState::behind`] catch up after that; no
    /// other channel catches up at start.
    #[must_use]
    pub fn new(state: StoredState) -> Self {
        let common = state.common;
        let fetch = if common.pts == 0 {
            CommonFetch::WantState
        } else {
            CommonFetch::WantDifference
        };
        let channels: HashMap<i64, Channel> = state
            .channels
            .iter()
            .map(|&(c, p)| (c, Channel::new(Some(p))))
            .collect();
        // A channel behind without a stored pts has nothing to catch up
        // from (its first state was never committed).
        let (startup, caught_up): (Vec<_>, Vec<_>) = state
            .behind
            .into_iter()
            .partition(|(c, _)| channels.contains_key(c));
        // Batch ids go on from the marks, so a later mark is always higher.
        let next_id = startup
            .iter()
            .chain(&caught_up)
            .map(|&(_, g)| g.saturating_add(1))
            .max()
            .unwrap_or(0);
        Self {
            pts: common.pts,
            qts: common.qts,
            seq: common.seq,
            date: common.date,
            held_pts: Vec::new(),
            held_qts: Vec::new(),
            seq_seen: Vec::new(),
            gap_deadline: None,
            idle_at: Instant::now() + IDLE_DIFFERENCE,
            fetch,
            epoch: 0,
            again: false,
            channels,
            queue: VecDeque::new(),
            in_flight: 0,
            gapped: BTreeSet::new(),
            retrying: BTreeSet::new(),
            retry_at: None,
            backoff: Backoff::default(),
            refusals: Backoff::default(),
            refused_until: None,
            common_flood: None,
            channel_flood: None,
            ahead: startup.iter().copied().collect(),
            startup: startup.into_iter().map(|(c, _)| c).collect(),
            ahead_new: Vec::new(),
            caught_up,
            forgotten: Vec::new(),
            known_users: state.known_users.into_iter().collect(),
            user_lookup: None,
            committed: common,
            committed_channels: state.channels.into_iter().collect(),
            pending: VecDeque::new(),
            next_id,
            next_origin: 0,
            dropped: Dropped::default(),
        }
    }

    /// Asks `known` whether a user's access hash is cached (the session's
    /// `peers` table), when a short update needs to know, instead of holding
    /// a set of every known user in memory.
    #[must_use]
    pub fn with_user_lookup(mut self, known: impl Fn(i64) -> bool + Send + Sync + 'static) -> Self {
        self.user_lookup = Some(Box::new(known));
        self
    }

    /// What was dropped so far.
    #[must_use]
    pub fn dropped(&self) -> Dropped {
        self.dropped
    }

    /// The boxes catching up now.
    #[must_use]
    pub fn catching_up(&self) -> CatchingUp {
        let fetching = |ch: &Channel| ch.fetch != ChannelFetch::Idle;
        let waiting = self
            .startup
            .iter()
            .filter(|c| !self.channels.get(c).is_some_and(fetching))
            .count();
        CatchingUp {
            common: self.fetch.busy(),
            channels: self.channels.values().filter(|ch| fetching(ch)).count() + waiting,
        }
    }

    /// One push: a boxed `Updates` object, or the sender's
    /// [`CONNECTION_ENDED`] signal.
    ///
    /// `updatesTooLong` (Telegram's, or the sender's after
    /// `new_session_created`) and [`CONNECTION_ENDED`] start a common
    /// `updates.getDifference`, and are never lost:
    /// - while one is pending (not yet asked), more join it, so a
    ///   reconnect's signals run one chain;
    /// - while a call is in flight, exactly one more difference follows
    ///   the chain's end, since that call may predate the signal.
    ///
    /// `updatesTooLong` also cuts a pending retry short: it came on a live
    /// session, so the server is reachable. [`CONNECTION_ENDED`] does not:
    /// the connection that sent it is gone, and on a path that drops every
    /// connection after its init call, cutting the retry short would ask
    /// once a second forever instead of backing off.
    ///
    /// A call lost with the connection comes back through
    /// [`fetch_failed`](Self::fetch_failed).
    pub fn feed_push(&mut self, container: Bytes) -> Vec<Batch> {
        self.idle_at = Instant::now() + IDLE_DIFFERENCE;
        if container.as_ref() == CONNECTION_ENDED {
            self.want_difference();
            return Vec::new();
        }
        let c = match decode::container(&container) {
            Ok(c) => c,
            Err(e) => {
                self.undecodable("push", &container, &e);
                return Vec::new();
            }
        };
        if c.constructor == "updatesTooLong" {
            // A pending retry is cut short: the signal came on a live
            // session, so the server is reachable again. The backoff stays
            // (an answer resets it): Telegram sends `new_session_created`
            // on every new connection, so on a path that drops each one
            // after its init call, resetting it would ask once a second.
            self.retry_at = None;
            self.want_difference();
            return Vec::new();
        }
        let home = self.group(&container, c.users, c.chats);
        let mut out = Out::new(home);

        // The seq rules apply to `updates` and `updatesCombined` only. A
        // seq_start of 0 is unordered: applied, and the seq state is kept
        // (grammers' NO_SEQ). Box updates are handled first, by their own
        // box's rules, whatever the seq says; the seq decides only for the
        // rest (Telegram's rules, as TDLib does): an already applied
        // container still carries a channel update the common difference
        // that moved seq past it does not cover.
        let mut seq_step = None;
        let mut stale = false;
        if let Some(end) = c.seq
            && !self.fetch.busy()
        {
            let start = c.seq_start.unwrap_or(end);
            let next = i64::from(self.seq) + 1;
            if start != 0 {
                match i64::from(start).cmp(&next) {
                    std::cmp::Ordering::Equal => {
                        seq_step = Some((end, c.date.unwrap_or(self.date)));
                    }
                    // Already applied.
                    std::cmp::Ordering::Less => stale = true,
                    std::cmp::Ordering::Greater => {
                        self.seq_seen.push((start, end));
                        self.arm_gap();
                    }
                }
            }
        }

        for u in &c.updates {
            match UpdateBytes::new(container.slice_ref(u)) {
                Ok(ub) if stale && !ub.has_box() => {}
                Ok(ub) => self.offer(ub, Mode::Live, &mut out),
                Err(e) => self.undecodable("update", u, &e),
            }
        }

        if let Some((seq, date)) = seq_step {
            self.seq = seq;
            self.date = date;
            out.open.delta.seq = Some(seq);
            out.open.delta.date = Some(date);
            out.open.common = true;
            self.advance_seq(&mut out);
        }
        self.rearm_gap();
        self.finish(out)
    }

    /// The pts of a `messages.affectedMessages`, `affectedHistory` or
    /// `affectedFoundMessages` answer, in the request's box: the channel the
    /// call targets (a `channels.*` call's `channel`, or a channel `peer` /
    /// `parent_peer`), else the common box.
    pub fn feed_affected(&mut self, box_kind: BoxKind, pts: i32, pts_count: i32) -> Vec<Batch> {
        if pts == 0 {
            return Vec::new();
        }
        let home = self.empty_group();
        let mut out = Out::new(home.clone());
        let held = Held {
            item: Item::Affected,
            pts,
            count: pts_count,
            check_peer: false,
            group: home,
        };
        match box_kind {
            BoxKind::Common => self.offer_common(Common::Pts, held, &mut out),
            BoxKind::Qts => self.offer_common(Common::Qts, Held { count: 1, ..held }, &mut out),
            BoxKind::Channel(id) => self.offer_channel(id, held, &mut out),
            BoxKind::None => {}
        }
        self.rearm_gap();
        self.finish(out)
    }

    /// Batch `id` was admitted. Returns the state to persist now, if any:
    /// batches are settled in id order, so a batch behind an unreported
    /// one waits for it.
    pub fn committed(&mut self, id: u64) -> Option<StateDelta> {
        self.resolve(id, Status::Committed)
    }

    /// Batch `id` was not admitted. Its boxes roll back to the committed
    /// state and fetch their difference after a backoff, per box:
    /// [`RETRY_MIN`], doubling per refusal up to [`RETRY_MAX`], reset when
    /// the box next commits a batch. A full queue is thus refetched at a
    /// pace Telegram accepts, not at round-trip rate. Its unsequenced
    /// updates are counted as dropped. Returns the state to persist now, if
    /// any.
    ///
    /// A batch with no updates had nothing to admit: refusing it counts as
    /// [`committed`](Self::committed). Its state (an adopted channel's
    /// baseline, a getState, a difference's last state) stays, so the
    /// updates the state was set for are not dropped below a new baseline.
    pub fn refused(&mut self, id: u64) -> Option<StateDelta> {
        self.resolve(id, Status::Refused)
    }

    fn resolve(&mut self, id: u64, status: Status) -> Option<StateDelta> {
        let i = self.pending.binary_search_by_key(&id, |p| p.id).ok()?;
        let p = &mut self.pending[i];
        if p.status != Status::Open {
            return None;
        }
        p.status = if p.empty { Status::Committed } else { status };
        let mut delta = StateDelta::default();
        while self
            .pending
            .front()
            .is_some_and(|p| p.status != Status::Open)
        {
            let Some(p) = self.pending.pop_front() else {
                break;
            };
            if p.status == Status::Committed {
                let d = self.still_current(p);
                // The box takes batches again: its next refusal starts the
                // backoff over.
                if d.pts.is_some() || d.qts.is_some() || d.seq.is_some() || d.date.is_some() {
                    self.refusals.reset();
                }
                for (c, _) in &d.channels {
                    if let Some(ch) = self.channels.get_mut(c) {
                        ch.refusals.reset();
                    }
                }
                self.committed.pts = d.pts.unwrap_or(self.committed.pts);
                self.committed.qts = d.qts.unwrap_or(self.committed.qts);
                self.committed.seq = d.seq.unwrap_or(self.committed.seq);
                self.committed.date = d.date.unwrap_or(self.committed.date);
                for &(c, pts) in &d.channels {
                    self.committed_channels.insert(c, pts);
                }
                delta.merge(d);
            } else {
                self.roll_back(&p);
            }
        }
        // Channels back where their committed state says are no longer
        // behind.
        let mut caught_up = std::mem::take(&mut self.caught_up);
        caught_up.extend(
            self.ahead
                .iter()
                .map(|(&c, &g)| (c, g))
                .filter(|&(c, g)| g != MARK_PENDING && self.caught_up_with_commit(c)),
        );
        for (c, _) in &caught_up {
            self.ahead.remove(c);
        }
        delta.caught_up = caught_up;
        delta.forgotten = std::mem::take(&mut self.forgotten);
        (!delta.is_empty()).then_some(delta)
    }

    /// The part of a committed batch's delta no refusal has rolled back.
    fn still_current(&self, p: Pending) -> StateDelta {
        let mut d = StateDelta::default();
        if p.common_epoch == Some(self.epoch) {
            d.pts = p.delta.pts;
            d.qts = p.delta.qts;
            d.seq = p.delta.seq;
            d.date = p.delta.date;
        }
        for (c, pts) in p.delta.channels {
            let current = p
                .channel_epochs
                .iter()
                .find(|(pc, _)| *pc == c)
                .is_some_and(|&(_, e)| self.channels.get(&c).is_some_and(|ch| ch.epoch == e));
            if current {
                d.set_channel(c, pts);
            }
        }
        d
    }

    fn roll_back(&mut self, p: &Pending) {
        self.dropped.full += p.unsequenced;
        let now = Instant::now();
        if p.common_epoch == Some(self.epoch) {
            self.epoch += 1;
            let wait = self.refusals.next();
            self.refused_until = Some(later(self.refused_until, now + wait));
            self.pts = self.committed.pts;
            self.qts = self.committed.qts;
            self.seq = self.committed.seq;
            self.date = self.committed.date;
            self.seq_seen.clear();
            self.gap_deadline = None;
            // A fetch in flight is now stale: its answer is dropped and
            // asked again (`resync`). A pending one starts from the
            // rolled-back state: a pending getState would take the server
            // state whole, past the refused updates.
            if matches!(
                self.fetch,
                CommonFetch::Idle | CommonFetch::WantState | CommonFetch::WantDifference
            ) {
                self.fetch = self.resync();
            }
        }
        for &(c, e) in &p.channel_epochs {
            let Some(ch) = self.channels.get_mut(&c) else {
                continue;
            };
            if ch.epoch != e {
                continue;
            }
            ch.epoch += 1;
            ch.pts = self.committed_channels.get(&c).copied();
            ch.deadline = None;
            let wait = ch.refusals.next();
            ch.refused_until = Some(later(ch.refused_until, now + wait));
            self.gapped.remove(&c);
            if ch.fetch == ChannelFetch::Idle && ch.pts.is_some() {
                ch.fetch = ChannelFetch::Wanted;
            }
            // Queued once the backoff is over (a call in flight is stale:
            // its end queues it, and the queue honours the backoff too).
            self.retrying.insert(c);
        }
    }

    /// Routes one update to its box.
    fn offer(&mut self, ub: UpdateBytes, mode: Mode, out: &mut Out) {
        let f = ub.facts();
        let (box_kind, pts, count, qts) = (f.box_kind, f.pts, f.pts_count, f.qts);
        // pts or qts 0 is no sequence (grammers' NO_PTS).
        let sequenced = match box_kind {
            BoxKind::Common | BoxKind::Channel(_) => pts.is_some_and(|p| p != 0),
            BoxKind::Qts => qts.is_some_and(|q| q != 0),
            BoxKind::None => false,
        };
        let check_peer = mode == Mode::Live && self.unknown_peer(&f);
        let home = out.home.clone();
        if f.constructor == "updateChannelTooLong" {
            // A signal for the manager, never delivered.
            if let BoxKind::Channel(c) = box_kind
                && mode != Mode::ChannelDifference(c)
            {
                self.channel_too_long(c, pts, out);
            }
            return;
        }
        let held = |item, pts, count| Held {
            item,
            pts,
            count,
            check_peer,
            group: home.clone(),
        };
        match box_kind {
            BoxKind::Common | BoxKind::Qts if mode == Mode::Difference && sequenced => {
                self.deliver(out, &home, ub);
            }
            BoxKind::Channel(c) if mode == Mode::ChannelDifference(c) && sequenced => {
                self.deliver(out, &home, ub);
            }
            BoxKind::Common => match pts.filter(|&p| p != 0) {
                Some(p) => {
                    let h = held(Item::Update(ub), p, count.unwrap_or(0));
                    self.offer_common(Common::Pts, h, out);
                }
                None => self.unsequenced(out, &home, ub),
            },
            BoxKind::Qts => match qts.filter(|&q| q != 0) {
                Some(q) => self.offer_common(Common::Qts, held(Item::Update(ub), q, 1), out),
                None => self.unsequenced(out, &home, ub),
            },
            BoxKind::Channel(c) => match pts.filter(|&p| p != 0) {
                Some(p) => {
                    let h = held(Item::Update(ub), p, count.unwrap_or(0));
                    self.offer_channel(c, h, out);
                }
                None => self.unsequenced(out, &home, ub),
            },
            BoxKind::None => self.unsequenced(out, &home, ub),
        }
    }

    /// A short update naming a user whose access hash is not cached,
    /// so it cannot be resolved without a difference.
    fn unknown_peer(&self, f: &UpdateFacts<'_>) -> bool {
        if !matches!(
            f.constructor,
            "updateShortMessage" | "updateShortChatMessage"
        ) {
            return false;
        }
        let peer = match f.peer {
            Some(PeerId::User(u)) => Some(u),
            _ => None,
        };
        [f.sender, peer].into_iter().flatten().any(|u| {
            !self.known_users.contains(&u)
                && !self.user_lookup.as_ref().is_some_and(|known| known(u))
        })
    }
}

impl CommonFetch {
    /// The common box is catching up: its updates wait.
    fn busy(self) -> bool {
        self != CommonFetch::Idle
    }
}

fn facts_of(h: &Held) -> UpdateFacts<'_> {
    match &h.item {
        Item::Update(ub) => ub.facts(),
        Item::Affected => UpdateFacts {
            constructor: "",
            box_kind: BoxKind::None,
            pts: None,
            pts_count: None,
            qts: None,
            peer: None,
            sender: None,
            text: None,
            command: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::CONNECTION_ENDED;
    use crate::schema::{API, MTPROTO};

    /// The connection-end signal is no TL object of either schema, so no
    /// push from Telegram can be mistaken for it, nor it for one.
    #[test]
    fn connection_ended_is_not_tl() {
        let id = CONNECTION_ENDED
            .first_chunk::<4>()
            .map(|b| u32::from_le_bytes(*b))
            .expect("four bytes");
        assert!(API.ctor_by_id(id).is_none());
        assert!(MTPROTO.ctor_by_id(id).is_none());
        assert!(crate::decode::container(CONNECTION_ENDED).is_err());
    }
}
