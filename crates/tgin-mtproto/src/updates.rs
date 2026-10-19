//! The update manager: tgin's own update-state handling and gap recovery,
//! by Telegram's rules (<https://core.telegram.org/api/updates>).
//!
//! It is a pure state machine over bytes. It does no I/O: pushes and fetch
//! answers go in, batches and fetch actions come out. The caller
//! - feeds every push ([`feed_push`](UpdateManager::feed_push)), including
//!   the `Updates` answers the sender copies into the push stream and the
//!   synthetic `updatesTooLong` it adds when a connection ends;
//! - runs each [`Fetch`] from [`next_action`](UpdateManager::next_action)
//!   on the ingest session and feeds the answer back (or reports
//!   [`fetch_failed`](UpdateManager::fetch_failed)); it calls
//!   `next_action` again by [`deadline`](UpdateManager::deadline);
//! - offers each [`Batch`] to admission, in id order, then reports
//!   [`committed`](UpdateManager::committed) or
//!   [`refused`](UpdateManager::refused), and persists the [`StateDelta`]
//!   either returns ([`SessionStore::commit_state`](crate::SessionStore::commit_state)).
//!
//! # Delivery (plan decision 5)
//!
//! Sequenced updates (`pts`, `qts`, a channel's `pts`; 0 is no sequence)
//! are at least once: a box's state is persisted only after every batch
//! before and including the one that moved it is admitted. A refused batch
//! rolls its boxes back to the last committed state and refetches them, so
//! its sequenced updates come back; its unsequenced ones are counted in
//! [`Dropped::full`]. There is no dedup: replays may repeat updates.
//!
//! # Catch-up (plan decision 6)
//!
//! The common box (with `qts` and `seq`) catches up with
//! `updates.getDifference`, at start and on a gap still open after
//! [`GAP_HOLD`]. A channel catches up only on a gap in that channel or on
//! `updateChannelTooLong`, never at start. Pushes for any box that is not
//! catching up are delivered as they come.
//!
//! # Bytes (plan decision 7)
//!
//! A pushed update is its raw TL, cut from the container; a short form is
//! the raw short object. A difference's messages become `updateNewMessage`,
//! `updateNewChannelMessage` or `updateNewEncryptedMessage`, composed by
//! bytes (constructor id, the raw message, then the difference's final
//! `pts` and a `pts_count` of 0, or its final `qts`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::ops::Range;

use bytes::Bytes;
use tokio::time::{Duration, Instant};

use crate::TlError;
use crate::decode::{self, BoxKind, PeerId, UpdateFacts};
use crate::session::{Peer, PeerKind, UpdateState};

/// How long a gap may stay open before the box fetches its difference;
/// the missing updates may only be reordered (Telegram's 0.5 s).
pub const GAP_HOLD: Duration = Duration::from_millis(500);
/// With no push for this long, the common box fetches its difference.
pub const IDLE_DIFFERENCE: Duration = Duration::from_secs(15 * 60);
/// At most this many `updates.getChannelDifference` calls in flight.
pub const MAX_CHANNEL_FETCHES: usize = 4;
/// Held (out-of-order) updates per box. Beyond it, a sequenced update is
/// dropped from memory; the box's state has not moved past it, so the
/// difference brings it back.
const MAX_HELD: usize = 4096;

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
    /// The users whose access hash is cached.
    pub known_users: Vec<i64>,
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
}

impl StateDelta {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pts.is_none()
            && self.qts.is_none()
            && self.seq.is_none()
            && self.date.is_none()
            && self.channels.is_empty()
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
    }
}

/// A call the ingest session runs for the manager. Only the ingest session
/// calls `updates.getState` or a difference [C: S14 (b)].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fetch {
    /// `updates.getState`; feed the answer to
    /// [`feed_state`](UpdateManager::feed_state).
    GetState,
    /// `updates.getDifference`; feed the answer to
    /// [`feed_difference`](UpdateManager::feed_difference).
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

/// One update's bytes (raw or composed, plan decision 7) with its facts.
#[derive(Clone)]
pub struct UpdateBytes {
    bytes: Bytes,
    /// With `text` and `command` left out; [`facts`](Self::facts) adds
    /// them back from `bytes`.
    facts: UpdateFacts<'static>,
    text: Option<Range<usize>>,
    command: Option<Range<usize>>,
}

impl UpdateBytes {
    fn new(bytes: Bytes) -> Result<Self, TlError> {
        let f = decode::facts(&bytes)?;
        let base = bytes.as_ptr().addr();
        let range = |s: &str| {
            let start = s.as_ptr().addr() - base;
            start..start + s.len()
        };
        let (text, command) = (f.text.map(range), f.command.map(range));
        let facts = UpdateFacts {
            constructor: f.constructor,
            box_kind: f.box_kind,
            pts: f.pts,
            pts_count: f.pts_count,
            qts: f.qts,
            peer: f.peer,
            sender: f.sender,
            text: None,
            command: None,
        };
        Ok(Self {
            bytes,
            facts,
            text,
            command,
        })
    }

    /// The update's TL bytes.
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    #[must_use]
    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }

    /// The box whose sequence the update is in, unless unsequenced.
    fn sequenced_box(&self) -> Option<BoxKind> {
        let f = &self.facts;
        let seq = match f.box_kind {
            BoxKind::Common | BoxKind::Channel(_) => f.pts,
            BoxKind::Qts => f.qts,
            BoxKind::None => None,
        };
        seq.filter(|&s| s != 0).map(|_| f.box_kind)
    }

    /// Sequenced, or `updateChannelTooLong` (a channel's catch-up signal,
    /// with or without a pts): handled by its box's rules, not the seq's.
    fn has_box(&self) -> bool {
        self.sequenced_box().is_some() || self.facts.constructor == "updateChannelTooLong"
    }

    /// The update's facts ([`decode::facts`] of [`bytes`](Self::bytes)).
    #[must_use]
    pub fn facts(&self) -> UpdateFacts<'_> {
        let text = |r: &Option<Range<usize>>| {
            r.clone()
                .and_then(|r| self.bytes.get(r))
                .and_then(|b| std::str::from_utf8(b).ok())
        };
        UpdateFacts {
            text: text(&self.text),
            command: text(&self.command),
            ..self.facts
        }
    }
}

/// Names the constructor and size only: the bytes are account content.
impl fmt::Debug for UpdateBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}(<{} bytes>)",
            self.facts.constructor,
            self.bytes.len()
        )
    }
}

/// Updates to admit together, then report by `id`.
#[derive(Clone)]
pub struct Batch {
    /// Rising; report batches in this order.
    pub id: u64,
    pub updates: Vec<UpdateBytes>,
    /// The raw `Vector<User>` of the push or difference the updates came
    /// from (an empty vector when it had none).
    pub users: Bytes,
    /// The raw `Vector<Chat>`, likewise.
    pub chats: Bytes,
    /// The peers `users` and `chats` name, for
    /// [`SessionStore::put_peers`](crate::SessionStore::put_peers).
    pub peers: Vec<Peer>,
    /// The state this batch moves to. Persist what
    /// [`committed`](UpdateManager::committed) returns, not this: a batch
    /// after a refused one must not persist past the refused updates.
    pub commit: StateDelta,
}

/// Leaves out the bytes: they are account content.
impl fmt::Debug for Batch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Batch")
            .field("id", &self.id)
            .field("updates", &self.updates)
            .field("peers", &self.peers.len())
            .field("commit", &self.commit)
            .finish()
    }
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
        }
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
    /// Channels with a gap deadline.
    gapped: HashSet<i64>,
    known_users: HashSet<i64>,
    committed: UpdateState,
    committed_channels: HashMap<i64, i32>,
    pending: VecDeque<Pending>,
    next_id: u64,
    next_origin: u64,
    dropped: Dropped,
}

impl UpdateManager {
    /// Starts from the persisted state: with a common state, the first
    /// action is `updates.getDifference`; without one, `updates.getState`.
    /// No channel catches up at start.
    #[must_use]
    pub fn new(state: StoredState) -> Self {
        let common = state.common;
        let fetch = if common.pts == 0 {
            CommonFetch::WantState
        } else {
            CommonFetch::WantDifference
        };
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
            channels: state
                .channels
                .iter()
                .map(|&(c, p)| (c, Channel::new(Some(p))))
                .collect(),
            queue: VecDeque::new(),
            in_flight: 0,
            gapped: HashSet::new(),
            known_users: state.known_users.into_iter().collect(),
            committed: common,
            committed_channels: state.channels.into_iter().collect(),
            pending: VecDeque::new(),
            next_id: 0,
            next_origin: 0,
            dropped: Dropped::default(),
        }
    }

    /// What was dropped so far.
    #[must_use]
    pub fn dropped(&self) -> Dropped {
        self.dropped
    }

    /// One push: a boxed `Updates` object.
    ///
    /// `updatesTooLong` (Telegram's, or the sender's synthetic one after a
    /// dropped connection or `new_session_created`) starts a common
    /// `updates.getDifference`, and is never lost:
    /// - while one is pending (not yet asked), more join it, so a
    ///   reconnect's two signals run one chain;
    /// - while a call is in flight, exactly one more difference follows
    ///   the chain's end, since that call may predate the signal.
    ///
    /// A call lost with the connection comes back through
    /// [`fetch_failed`](Self::fetch_failed).
    pub fn feed_push(&mut self, container: Bytes) -> Vec<Batch> {
        self.idle_at = Instant::now() + IDLE_DIFFERENCE;
        let c = match decode::container(&container) {
            Ok(c) => c,
            Err(e) => {
                self.undecodable("push", &container, &e);
                return Vec::new();
            }
        };
        if c.constructor == "updatesTooLong" {
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
    /// `affectedFoundMessages` answer, in the request's box: the common box,
    /// or the channel for `channels.*` calls.
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

    /// The next call to run, if one is due. Call again after each answer
    /// and by [`deadline`](Self::deadline).
    pub fn next_action(&mut self) -> Option<Fetch> {
        let now = Instant::now();
        if self.fetch == CommonFetch::Idle
            && (self.gap_deadline.is_some_and(|d| d <= now) || self.idle_at <= now)
        {
            self.fetch = CommonFetch::WantDifference;
            self.gap_deadline = None;
        }
        if self.idle_at <= now {
            self.idle_at = now + IDLE_DIFFERENCE;
        }
        match self.fetch {
            CommonFetch::WantState => {
                self.fetch = CommonFetch::State { epoch: self.epoch };
                self.again = false;
                return Some(Fetch::GetState);
            }
            CommonFetch::WantDifference => {
                self.fetch = CommonFetch::Difference { epoch: self.epoch };
                // This call postdates every signal so far.
                self.again = false;
                return Some(Fetch::GetDifference {
                    pts: self.pts,
                    qts: self.qts,
                    date: self.date,
                });
            }
            _ => {}
        }
        if !self.gapped.is_empty() {
            let due: Vec<i64> = self
                .gapped
                .iter()
                .copied()
                .filter(|c| {
                    self.channels
                        .get(c)
                        .and_then(|ch| ch.deadline)
                        .is_none_or(|d| d <= now)
                })
                .collect();
            for c in due {
                self.gapped.remove(&c);
                if let Some(ch) = self.channels.get_mut(&c) {
                    ch.deadline = None;
                    if ch.fetch == ChannelFetch::Idle && ch.pts.is_some() {
                        ch.fetch = ChannelFetch::Wanted;
                        self.queue.push_back(c);
                    }
                }
            }
        }
        while self.in_flight < MAX_CHANNEL_FETCHES {
            let c = self.queue.pop_front()?;
            let Some(ch) = self.channels.get_mut(&c) else {
                continue;
            };
            if ch.fetch != ChannelFetch::Wanted {
                continue;
            }
            let Some(pts) = ch.pts else {
                ch.fetch = ChannelFetch::Idle;
                continue;
            };
            ch.fetch = ChannelFetch::InFlight { epoch: ch.epoch };
            self.in_flight += 1;
            return Some(Fetch::GetChannelDifference { channel: c, pts });
        }
        None
    }

    /// When [`next_action`](Self::next_action) may next have something
    /// that is not due now: a gap's hold, or the idle difference.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        let mut at = self.idle_at;
        if self.fetch == CommonFetch::Idle
            && let Some(d) = self.gap_deadline
        {
            at = at.min(d);
        }
        for c in &self.gapped {
            if let Some(d) = self.channels.get(c).and_then(|ch| ch.deadline) {
                at = at.min(d);
            }
        }
        at
    }

    /// A fetch from [`next_action`](Self::next_action) got no answer (a
    /// connection error or an RPC error). It is asked again.
    pub fn fetch_failed(&mut self, fetch: Fetch) {
        match fetch {
            Fetch::GetState => {
                if matches!(self.fetch, CommonFetch::State { .. }) {
                    self.fetch = CommonFetch::WantState;
                }
            }
            Fetch::GetDifference { .. } => {
                if matches!(self.fetch, CommonFetch::Difference { .. }) {
                    self.fetch = CommonFetch::WantDifference;
                }
            }
            Fetch::GetChannelDifference { channel, .. } => {
                if let Some(ch) = self.channels.get_mut(&channel)
                    && matches!(ch.fetch, ChannelFetch::InFlight { .. })
                {
                    ch.fetch = ChannelFetch::Wanted;
                    self.queue.push_back(channel);
                    self.in_flight = self.in_flight.saturating_sub(1);
                }
            }
        }
    }

    /// Stops tracking a channel the account can no longer read (e.g.
    /// `CHANNEL_PRIVATE`): its state and held updates are dropped.
    pub fn forget_channel(&mut self, channel: i64) {
        if let Some(ch) = self.channels.remove(&channel)
            && matches!(ch.fetch, ChannelFetch::InFlight { .. })
        {
            self.in_flight = self.in_flight.saturating_sub(1);
        }
        self.gapped.remove(&channel);
        self.committed_channels.remove(&channel);
    }

    /// The answer to [`Fetch::GetState`]. Held common and qts updates are
    /// delivered as they are, then the state is taken whole.
    pub fn feed_state(&mut self, bytes: Bytes) -> Vec<Batch> {
        let CommonFetch::State { epoch } = self.fetch else {
            return Vec::new();
        };
        if epoch != self.epoch {
            self.fetch = CommonFetch::WantDifference;
            return Vec::new();
        }
        let s = match decode::state(&bytes) {
            Ok(s) => s,
            Err(e) => {
                // Never retried in a loop: the idle difference comes next.
                self.undecodable("state", &bytes, &e);
                self.fetch = CommonFetch::Idle;
                self.chain_end();
                return Vec::new();
            }
        };
        let mut out = Out::new(self.empty_group());
        let mut held = std::mem::take(&mut self.held_pts);
        held.append(&mut self.held_qts);
        held.sort_by_key(|h| h.group.origin);
        for h in held {
            if let Item::Update(ub) = h.item {
                self.deliver(&mut out, &h.group, ub);
            }
        }
        self.pts = s.pts;
        self.qts = s.qts;
        self.seq = s.seq;
        self.date = s.date;
        self.seq_seen.clear();
        self.gap_deadline = None;
        self.fetch = CommonFetch::Idle;
        self.idle_at = Instant::now() + IDLE_DIFFERENCE;
        self.touch_common_all(&mut out);
        self.chain_end();
        self.finish(out)
    }

    /// The answer to [`Fetch::GetDifference`].
    pub fn feed_difference(&mut self, bytes: Bytes) -> Vec<Batch> {
        let CommonFetch::Difference { epoch } = self.fetch else {
            return Vec::new();
        };
        if epoch != self.epoch {
            // Asked before a refused batch rolled the state back.
            self.fetch = CommonFetch::WantDifference;
            return Vec::new();
        }
        self.idle_at = Instant::now() + IDLE_DIFFERENCE;
        let d = match decode::difference(&bytes) {
            Ok(d) if d.constructor.starts_with("updates.difference") => d,
            Ok(_) => {
                self.undecodable(
                    "difference",
                    &bytes,
                    &TlError::WrongType {
                        want: "updates.Difference",
                        got: "updates.ChannelDifference",
                    },
                );
                self.fetch = CommonFetch::WantState;
                return Vec::new();
            }
            Err(e) => {
                // Skip past it: resync from `updates.getState`, so the same
                // difference is never fetched again.
                self.undecodable("difference", &bytes, &e);
                self.fetch = CommonFetch::WantState;
                return Vec::new();
            }
        };
        let home = self.group(&bytes, d.users, d.chats);
        let mut out = Out::new(home.clone());
        match d.constructor {
            "updates.differenceEmpty" => {
                self.date = d.date.unwrap_or(self.date);
                self.seq = d.seq.unwrap_or(self.seq);
                out.open.delta.date = Some(self.date);
                out.open.delta.seq = Some(self.seq);
                out.open.common = true;
                self.fetch = CommonFetch::Idle;
            }
            "updates.differenceTooLong" => match d.pts {
                Some(p) if p > self.pts => {
                    self.pts = p;
                    out.open.delta.pts = Some(p);
                    out.open.common = true;
                    self.fetch = CommonFetch::WantDifference;
                }
                _ => self.fetch = CommonFetch::WantState,
            },
            _ => {
                let slice = d.constructor == "updates.differenceSlice";
                let pts = d.pts.unwrap_or(self.pts);
                let qts = d.qts.unwrap_or(self.qts);
                for m in &d.new_messages {
                    self.compose(&mut out, UPDATE_NEW_MESSAGE, m, &[pts, 0]);
                }
                for m in &d.new_encrypted_messages {
                    self.compose(&mut out, UPDATE_NEW_ENCRYPTED_MESSAGE, m, &[qts]);
                }
                for u in &d.other_updates {
                    match UpdateBytes::new(bytes.slice_ref(u)) {
                        Ok(ub) => self.offer(ub, Mode::Difference, &mut out),
                        Err(e) => self.undecodable("update", u, &e),
                    }
                }
                let progressed = (pts, qts) != (self.pts, self.qts);
                self.pts = pts;
                self.qts = qts;
                self.seq = d.seq.unwrap_or(self.seq);
                self.date = d.date.unwrap_or(self.date);
                // The whole difference is one commit [C: S5]: its state goes
                // on its last batch.
                self.switch(&mut out, &home);
                self.touch_common_all(&mut out);
                self.fetch = if slice && progressed {
                    CommonFetch::WantDifference
                } else {
                    CommonFetch::Idle
                };
            }
        }
        if self.fetch == CommonFetch::Idle {
            self.seq_seen.clear();
            // With a follow-up chain due, an uncached peer's short update
            // waits for it; otherwise it goes as it is.
            let check = self.again;
            self.release_common(Common::Pts, check, &mut out);
            self.release_common(Common::Qts, check, &mut out);
        }
        self.chain_end();
        self.rearm_gap();
        self.finish(out)
    }

    /// The answer to [`Fetch::GetChannelDifference`] for `channel`.
    pub fn feed_channel_difference(&mut self, channel: i64, bytes: Bytes) -> Vec<Batch> {
        let Some(ch) = self.channels.get_mut(&channel) else {
            return Vec::new();
        };
        let ChannelFetch::InFlight { epoch } = ch.fetch else {
            return Vec::new();
        };
        self.in_flight = self.in_flight.saturating_sub(1);
        if epoch != ch.epoch {
            ch.fetch = ChannelFetch::Wanted;
            self.queue.push_back(channel);
            return Vec::new();
        }
        let old = ch.pts;
        let d = match decode::difference(&bytes) {
            Ok(d) if d.constructor.starts_with("updates.channelDifference") => Ok(d),
            Ok(_) => Err(TlError::WrongType {
                want: "updates.ChannelDifference",
                got: "updates.Difference",
            }),
            Err(e) => Err(e),
        };
        let d = match d {
            Ok(d) => d,
            Err(e) => {
                // Skip past it: the channel's state is dropped and its next
                // update starts it again, so this difference is never
                // fetched again.
                ch.pts = None;
                ch.fetch = ChannelFetch::Idle;
                self.undecodable("channel difference", &bytes, &e);
                let mut out = Out::new(self.empty_group());
                self.release_channel(channel, &mut out);
                return self.finish(out);
            }
        };
        let home = self.group(&bytes, d.users, d.chats);
        let mut out = Out::new(home.clone());
        let pts = d.pts.or(old).unwrap_or(0);
        for m in &d.new_messages {
            self.compose(&mut out, UPDATE_NEW_CHANNEL_MESSAGE, m, &[pts, 0]);
        }
        for u in &d.other_updates {
            match UpdateBytes::new(bytes.slice_ref(u)) {
                Ok(ub) => self.offer(ub, Mode::ChannelDifference(channel), &mut out),
                Err(e) => self.undecodable("update", u, &e),
            }
        }
        self.switch(&mut out, &home);
        let more = !d.is_final && d.pts.is_some_and(|p| old.is_none_or(|o| p > o));
        if let Some(ch) = self.channels.get_mut(&channel) {
            ch.pts = d.pts;
            if let Some(p) = d.pts {
                out.open.touch_channel(channel, p);
            }
            if more {
                ch.fetch = ChannelFetch::Wanted;
                self.queue.push_back(channel);
            } else {
                ch.fetch = ChannelFetch::Idle;
            }
        }
        self.release_channel(channel, &mut out);
        self.finish(out)
    }

    /// Batch `id` was admitted. Returns the state to persist now, if any:
    /// batches are settled in id order, so a batch behind an unreported
    /// one waits for it.
    pub fn committed(&mut self, id: u64) -> Option<StateDelta> {
        self.resolve(id, Status::Committed)
    }

    /// Batch `id` was not admitted. Its boxes roll back to the committed
    /// state and fetch their difference; its unsequenced updates are
    /// counted as dropped. Returns the state to persist now, if any.
    pub fn refused(&mut self, id: u64) -> Option<StateDelta> {
        self.resolve(id, Status::Refused)
    }

    fn resolve(&mut self, id: u64, status: Status) -> Option<StateDelta> {
        let i = self.pending.binary_search_by_key(&id, |p| p.id).ok()?;
        let p = &mut self.pending[i];
        if p.status != Status::Open {
            return None;
        }
        p.status = status;
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
        if p.common_epoch == Some(self.epoch) {
            self.epoch += 1;
            self.pts = self.committed.pts;
            self.qts = self.committed.qts;
            self.seq = self.committed.seq;
            self.date = self.committed.date;
            self.seq_seen.clear();
            self.gap_deadline = None;
            // A fetch in flight is now stale; its answer is dropped and
            // asked again from the rolled-back state.
            if matches!(self.fetch, CommonFetch::Idle) {
                self.fetch = CommonFetch::WantDifference;
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
            self.gapped.remove(&c);
            if ch.fetch == ChannelFetch::Idle && ch.pts.is_some() {
                ch.fetch = ChannelFetch::Wanted;
                self.queue.push_back(c);
            }
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

    /// A short update naming a user whose access hash is not cached
    /// [C: S9].
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
        [f.sender, peer]
            .into_iter()
            .flatten()
            .any(|u| !self.known_users.contains(&u))
    }

    fn offer_common(&mut self, which: Common, h: Held, out: &mut Out) {
        if self.fetch.busy() {
            if h.check_peer {
                // Its covering difference may be one that started earlier.
                self.want_difference();
            }
            self.hold_common(which, h);
            return;
        }
        let local = i64::from(self.local(which));
        match h.before().cmp(&local) {
            std::cmp::Ordering::Equal if h.check_peer => {
                // Fetch the difference that covers it: it brings the users
                // and chats. The update stays held until then.
                self.hold_common(which, h);
                self.want_difference();
            }
            std::cmp::Ordering::Equal => {
                self.apply_common(which, h, out);
                self.release_common(which, true, out);
            }
            std::cmp::Ordering::Less => {} // already applied
            std::cmp::Ordering::Greater => {
                self.hold_common(which, h);
                self.arm_gap();
            }
        }
    }

    fn local(&self, which: Common) -> i32 {
        match which {
            Common::Pts => self.pts,
            Common::Qts => self.qts,
        }
    }

    fn held_mut(&mut self, which: Common) -> &mut Vec<Held> {
        match which {
            Common::Pts => &mut self.held_pts,
            Common::Qts => &mut self.held_qts,
        }
    }

    fn hold_common(&mut self, which: Common, h: Held) {
        let held = self.held_mut(which);
        if held.len() < MAX_HELD {
            held.push(h);
        }
    }

    fn apply_common(&mut self, which: Common, h: Held, out: &mut Out) {
        if let Item::Update(ub) = h.item {
            self.deliver(out, &h.group, ub);
        }
        match which {
            Common::Pts => {
                self.pts = h.pts;
                out.open.delta.pts = Some(h.pts);
            }
            Common::Qts => {
                self.qts = h.pts;
                out.open.delta.qts = Some(h.pts);
            }
        }
        out.open.common = true;
    }

    /// Applies the held updates that now follow on. With `check`, a live
    /// short update from an uncached peer still waits for a difference;
    /// after a difference it goes as it is, so the check never loops.
    fn release_common(&mut self, which: Common, check: bool, out: &mut Out) {
        if self.fetch.busy() || self.held_mut(which).is_empty() {
            return;
        }
        let mut held = std::mem::take(self.held_mut(which));
        held.sort_by_key(Held::before);
        let mut keep = Vec::new();
        for h in held {
            let local = i64::from(self.local(which));
            if !keep.is_empty() || h.before() > local {
                keep.push(h);
            } else if h.before() == local {
                if check && h.check_peer && self.unknown_peer(&facts_of(&h)) {
                    keep.push(h);
                    self.want_difference();
                } else {
                    self.apply_common(which, h, out);
                }
            }
        }
        *self.held_mut(which) = keep;
    }

    fn offer_channel(&mut self, c: i64, h: Held, out: &mut Out) {
        let ch = self.channels.entry(c).or_insert_with(|| Channel::new(None));
        if ch.fetch != ChannelFetch::Idle {
            if ch.held.len() < MAX_HELD {
                ch.held.push(h);
            }
            return;
        }
        let Some(local) = ch.pts else {
            self.adopt(c, &h, out);
            self.apply_channel(c, h, out);
            self.release_channel(c, out);
            return;
        };
        match h.before().cmp(&i64::from(local)) {
            std::cmp::Ordering::Equal => {
                self.apply_channel(c, h, out);
                self.release_channel(c, out);
            }
            std::cmp::Ordering::Less => {} // already applied
            std::cmp::Ordering::Greater => {
                if ch.held.len() < MAX_HELD {
                    ch.held.push(h);
                }
                ch.deadline.get_or_insert_with(|| Instant::now() + GAP_HOLD);
                self.gapped.insert(c);
            }
        }
    }

    /// An unknown channel's state starts just before its first update, in
    /// a batch of its own: once that batch is committed, a crash before
    /// the update's own commit still leaves a state to catch up from.
    fn adopt(&mut self, c: i64, h: &Held, out: &mut Out) {
        let base = i32::try_from(h.before()).unwrap_or(h.pts);
        self.set_baseline(c, base, out);
    }

    fn set_baseline(&mut self, c: i64, pts: i32, out: &mut Out) {
        let group = out.open.group.clone();
        self.flush(out);
        if let Some(ch) = self.channels.get_mut(&c) {
            ch.pts = Some(pts);
        }
        out.open.touch_channel(c, pts);
        self.flush(out);
        out.open.group = group;
    }

    fn apply_channel(&mut self, c: i64, h: Held, out: &mut Out) {
        if let Item::Update(ub) = h.item {
            self.deliver(out, &h.group, ub);
        }
        if let Some(ch) = self.channels.get_mut(&c) {
            ch.pts = Some(h.pts);
        }
        out.open.touch_channel(c, h.pts);
    }

    fn release_channel(&mut self, c: i64, out: &mut Out) {
        let Some(ch) = self.channels.get_mut(&c) else {
            return;
        };
        if ch.fetch != ChannelFetch::Idle || ch.held.is_empty() {
            if ch.held.is_empty() {
                ch.deadline = None;
                self.gapped.remove(&c);
            }
            return;
        }
        let mut held = std::mem::take(&mut ch.held);
        held.sort_by_key(Held::before);
        let mut keep = Vec::new();
        for h in held {
            let local = self.channels.get(&c).and_then(|ch| ch.pts);
            match local {
                None => {
                    self.adopt(c, &h, out);
                    self.apply_channel(c, h, out);
                }
                Some(_) if !keep.is_empty() => keep.push(h),
                Some(l) => match h.before().cmp(&i64::from(l)) {
                    std::cmp::Ordering::Equal => self.apply_channel(c, h, out),
                    std::cmp::Ordering::Less => {}
                    std::cmp::Ordering::Greater => keep.push(h),
                },
            }
        }
        if let Some(ch) = self.channels.get_mut(&c) {
            if keep.is_empty() {
                ch.deadline = None;
                self.gapped.remove(&c);
            } else {
                ch.deadline.get_or_insert_with(|| Instant::now() + GAP_HOLD);
                self.gapped.insert(c);
            }
            ch.held = keep;
        }
    }

    /// `updateChannelTooLong`: a catch-up signal for the channel, with or
    /// without a pts. An unknown channel has nothing to catch up from; its
    /// pts, when given, becomes its state.
    fn channel_too_long(&mut self, c: i64, pts: Option<i32>, out: &mut Out) {
        let ch = self.channels.entry(c).or_insert_with(|| Channel::new(None));
        match ch.pts {
            None => {
                if let Some(p) = pts.filter(|&p| p != 0)
                    && ch.fetch == ChannelFetch::Idle
                {
                    self.set_baseline(c, p, out);
                    self.release_channel(c, out);
                }
            }
            Some(_) => {
                if ch.fetch == ChannelFetch::Idle {
                    ch.fetch = ChannelFetch::Wanted;
                    ch.deadline = None;
                    self.gapped.remove(&c);
                    self.queue.push_back(c);
                }
            }
        }
    }

    /// `updates.getDifference` as soon as possible (a gap's hold is over,
    /// `updatesTooLong`, or an uncached peer). While one is only pending,
    /// the signal joins it. While a call is in flight, that call may
    /// predate the signal, so exactly one more difference follows the
    /// chain's end: the signal is never lost, and a burst runs one chain.
    fn want_difference(&mut self) {
        match self.fetch {
            CommonFetch::Idle => self.fetch = CommonFetch::WantDifference,
            CommonFetch::State { .. } | CommonFetch::Difference { .. } => self.again = true,
            CommonFetch::WantState | CommonFetch::WantDifference => {}
        }
        self.gap_deadline = None;
    }

    /// The common chain is over: one more difference if a signal came
    /// while a call of it was in flight.
    fn chain_end(&mut self) {
        if self.fetch == CommonFetch::Idle && std::mem::take(&mut self.again) {
            self.fetch = CommonFetch::WantDifference;
        }
    }

    fn arm_gap(&mut self) {
        self.gap_deadline
            .get_or_insert_with(|| Instant::now() + GAP_HOLD);
    }

    /// Keeps the common gap deadline only while a gap is open.
    fn rearm_gap(&mut self) {
        let open =
            !(self.held_pts.is_empty() && self.held_qts.is_empty() && self.seq_seen.is_empty());
        if open && !self.fetch.busy() {
            self.arm_gap();
        } else {
            self.gap_deadline = None;
        }
    }

    /// After a seq is applied, the containers already handled that now
    /// follow on close the seq gap.
    fn advance_seq(&mut self, out: &mut Out) {
        loop {
            let next = i64::from(self.seq) + 1;
            self.seq_seen.retain(|&(s, _)| i64::from(s) >= next);
            let Some(i) = self
                .seq_seen
                .iter()
                .position(|&(s, _)| i64::from(s) == next)
            else {
                break;
            };
            let (_, end) = self.seq_seen.swap_remove(i);
            self.seq = end;
            out.open.delta.seq = Some(end);
        }
    }

    fn touch_common_all(&self, out: &mut Out) {
        out.open.delta.pts = Some(self.pts);
        out.open.delta.qts = Some(self.qts);
        out.open.delta.seq = Some(self.seq);
        out.open.delta.date = Some(self.date);
        out.open.common = true;
    }

    /// Composes a difference's message into an update (plan decision 7):
    /// the constructor id, the raw message, then the trailing ints.
    fn compose(&mut self, out: &mut Out, id: u32, message: &[u8], tail: &[i32]) {
        let mut b = Vec::with_capacity(4 + message.len() + 4 * tail.len());
        b.extend_from_slice(&id.to_le_bytes());
        b.extend_from_slice(message);
        for v in tail {
            b.extend_from_slice(&v.to_le_bytes());
        }
        match UpdateBytes::new(Bytes::from(b)) {
            Ok(ub) => {
                let home = out.home.clone();
                self.deliver(out, &home, ub);
            }
            Err(e) => self.undecodable("message", message, &e),
        }
    }

    fn unsequenced(&mut self, out: &mut Out, g: &Group, ub: UpdateBytes) {
        self.deliver(out, g, ub);
        out.open.unsequenced += 1;
    }

    /// Adds `ub` to the open batch from `g`. The batch then holds a
    /// sequenced update of its box: refusing it rolls that box back, even
    /// when the state it moves to is committed by a later batch (a
    /// difference split around a channel's first state).
    fn deliver(&mut self, out: &mut Out, g: &Group, ub: UpdateBytes) {
        self.switch(out, g);
        match ub.sequenced_box() {
            Some(BoxKind::Common | BoxKind::Qts) => out.open.common = true,
            Some(BoxKind::Channel(c)) if !out.open.channels.contains(&c) => {
                out.open.channels.push(c);
            }
            _ => {}
        }
        out.open.updates.push(ub);
    }

    /// Makes `g` the open batch's source, closing the batch of another.
    fn switch(&mut self, out: &mut Out, g: &Group) {
        if out.open.group.origin != g.origin {
            self.flush(out);
            out.open.group = g.clone();
        }
    }

    /// Closes the open batch, if it holds anything.
    fn flush(&mut self, out: &mut Out) {
        let group = out.open.group.clone();
        let open = std::mem::replace(&mut out.open, Open::new(group));
        if open.updates.is_empty() && open.delta.is_empty() {
            return;
        }
        let mut peers = Vec::new();
        for v in [&open.group.users, &open.group.chats] {
            if decode::peers(v, &mut peers).is_err() {
                peers.clear();
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.pending.push_back(Pending {
            id,
            delta: open.delta.clone(),
            common_epoch: open.common.then_some(self.epoch),
            channel_epochs: open
                .channels
                .iter()
                .filter_map(|c| self.channels.get(c).map(|ch| (*c, ch.epoch)))
                .collect(),
            unsequenced: open.unsequenced,
            status: Status::Open,
        });
        out.batches.push(Batch {
            id,
            updates: open.updates,
            users: open.group.users,
            chats: open.group.chats,
            peers,
            commit: open.delta,
        });
    }

    fn finish(&mut self, mut out: Out) -> Vec<Batch> {
        self.flush(&mut out);
        out.batches
    }

    /// A new source: its `users` and `chats` (an empty vector when absent),
    /// whose access hashes are learned at once.
    fn group(&mut self, src: &Bytes, users: Option<&[u8]>, chats: Option<&[u8]>) -> Group {
        let slice = |v: Option<&[u8]>| {
            v.map_or_else(|| Bytes::from_static(EMPTY_VECTOR), |v| src.slice_ref(v))
        };
        let (users, chats) = (slice(users), slice(chats));
        let mut peers = Vec::new();
        if decode::peers(&users, &mut peers).is_ok() {
            for p in peers {
                if matches!(p.kind, PeerKind::User { .. }) && p.access_hash.is_some() {
                    self.known_users.insert(p.id);
                }
            }
        }
        let origin = self.next_origin;
        self.next_origin += 1;
        Group {
            origin,
            users,
            chats,
        }
    }

    fn empty_group(&mut self) -> Group {
        self.group(&Bytes::new(), None, None)
    }

    /// Counts and skips bytes the decoder refused. Logs the constructor id
    /// only, never content.
    fn undecodable(&mut self, what: &'static str, bytes: &[u8], e: &TlError) {
        self.dropped.undecodable += 1;
        let id = bytes
            .first_chunk::<4>()
            .map_or(0, |b| u32::from_le_bytes(*b));
        tracing::warn!(
            constructor = %format_args!("{id:#010x}"),
            error = %e,
            "update manager: undecodable {what} skipped"
        );
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
