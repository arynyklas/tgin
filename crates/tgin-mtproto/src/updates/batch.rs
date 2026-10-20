//! The batch layer: what the manager hands out (`UpdateBytes`, `Batch`)
//! and how pushes are grouped and composed into it.

use super::*;

/// One update's bytes (raw or composed) with its facts.
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
    pub(crate) fn new(bytes: Bytes) -> Result<Self, TlError> {
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
    pub(super) fn has_box(&self) -> bool {
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
    /// The channels this batch is the first to move ahead of their
    /// committed state. Persist them, with the batch's `id` as their mark
    /// ([`SessionStore::mark_channels_behind`](crate::SessionStore::mark_channels_behind)),
    /// before admitting the batch, admitted or not: after a crash they
    /// catch up at start, so a quiet channel loses nothing. A batch may
    /// carry only this (an update held for a gap, a catch-up signal).
    pub ahead: Vec<i64>,
}

/// Leaves out the bytes: they are account content.
impl fmt::Debug for Batch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Batch")
            .field("id", &self.id)
            .field("updates", &self.updates)
            .field("peers", &self.peers.len())
            .field("commit", &self.commit)
            .field("ahead", &self.ahead)
            .finish()
    }
}

impl UpdateManager {
    /// Composes a difference's message into an update:
    /// the constructor id, the raw message, then the trailing ints.
    pub(super) fn compose(&mut self, out: &mut Out, id: u32, message: &[u8], tail: &[i32]) {
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

    pub(super) fn unsequenced(&mut self, out: &mut Out, g: &Group, ub: UpdateBytes) {
        self.deliver(out, g, ub);
        out.open.unsequenced += 1;
    }

    /// Adds `ub` to the open batch from `g`. The batch then holds a
    /// sequenced update of its box: refusing it rolls that box back, even
    /// when the state it moves to is committed by a later batch (a
    /// difference split around a channel's first state).
    pub(super) fn deliver(&mut self, out: &mut Out, g: &Group, ub: UpdateBytes) {
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
    pub(super) fn switch(&mut self, out: &mut Out, g: &Group) {
        if out.open.group.origin != g.origin {
            self.flush(out);
            out.open.group = g.clone();
        }
    }

    /// Closes the open batch, if it holds anything.
    pub(super) fn flush(&mut self, out: &mut Out) {
        let group = out.open.group.clone();
        let open = std::mem::replace(&mut out.open, Open::new(group));
        if open.updates.is_empty() && open.delta.is_empty() && self.ahead_new.is_empty() {
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
            empty: open.updates.is_empty(),
            status: Status::Open,
        });
        out.batches.push(Batch {
            id,
            updates: open.updates,
            users: open.group.users,
            chats: open.group.chats,
            peers,
            commit: open.delta,
            ahead: {
                let ahead = std::mem::take(&mut self.ahead_new);
                for c in &ahead {
                    self.ahead.insert(*c, id);
                }
                ahead
            },
        });
    }

    pub(super) fn finish(&mut self, mut out: Out) -> Vec<Batch> {
        self.flush(&mut out);
        out.batches
    }

    /// A new source: its `users` and `chats` (an empty vector when absent),
    /// whose access hashes are learned at once.
    pub(super) fn group(
        &mut self,
        src: &Bytes,
        users: Option<&[u8]>,
        chats: Option<&[u8]>,
    ) -> Group {
        let slice = |v: Option<&[u8]>| {
            v.map_or_else(|| Bytes::from_static(EMPTY_VECTOR), |v| src.slice_ref(v))
        };
        let (users, chats) = (slice(users), slice(chats));
        let mut peers = Vec::new();
        if self.user_lookup.is_none() && decode::peers(&users, &mut peers).is_ok() {
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

    pub(super) fn empty_group(&mut self) -> Group {
        self.group(&Bytes::new(), None, None)
    }

    /// Counts and skips bytes the decoder refused. Logs the constructor id
    /// only, never content.
    pub(super) fn undecodable(&mut self, what: &'static str, bytes: &[u8], e: &TlError) {
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
