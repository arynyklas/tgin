//! tgin's own schema-driven decoder for the layer it serves
//! ([`crate::schema::API`]): only what routing and update-state handling
//! need, read straight from Telegram's bytes.
//!
//! - [`facts`]: one update's routing facts. It walks the schema, skips
//!   every field it does not need, borrows the text and never allocates.
//! - [`container`]: an `Updates` push cut into the slices of its updates
//!   and of its `users` and `chats` vectors.
//! - [`difference`]: an `updates.getDifference` or
//!   `updates.getChannelDifference` answer cut the same way, plus its state.
//! - [`state`]: an `updates.getState` answer.
//! - [`peers`]: the peers and access hashes of a `users` or `chats` vector.
//!
//! Every slice is a sub-slice of the input: Telegram's bytes, never
//! re-encoded (plan decision 7). Each checks the bytes as strictly as the
//! TL-JSON codec does.

use crate::schema::{
    API, Ctor, Kind, MAX_FLAG_WORDS, Reader, Role, Seq, TlError, Ty, deeper, present,
};
use crate::session::{ChannelKind, Peer, PeerKind};

const PEER_USER: u32 = 0x5951_1722;
const PEER_CHAT: u32 = 0x36c6_019a;
const PEER_CHANNEL: u32 = 0xa2a5_371e;
const UPDATE_SHORT: u32 = 0x78d4_dec1;
const UPDATE_SHORT_MESSAGE: u32 = 0x313b_c7f8;
const UPDATE_SHORT_CHAT_MESSAGE: u32 = 0x4d6d_eea5;
const UPDATE_SHORT_SENT_MESSAGE: u32 = 0x9015_e101;

/// A chat as Telegram's `Peer` names it, by bare id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeerId {
    User(i64),
    Chat(i64),
    Channel(i64),
}

impl PeerId {
    fn user(self) -> Option<i64> {
        match self {
            Self::User(id) => Some(id),
            _ => None,
        }
    }

    fn channel(self) -> Option<i64> {
        match self {
            Self::Channel(id) => Some(id),
            _ => None,
        }
    }
}

/// The update box whose sequence an update's `pts` or `qts` belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BoxKind {
    /// No sequence: the update is best effort.
    None,
    /// The account's common `pts` box.
    Common,
    /// The secondary `qts` box.
    Qts,
    /// A channel's own `pts` box, by bare channel id.
    Channel(i64),
}

/// The routing and update-state facts of one update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateFacts<'a> {
    /// The update's constructor, e.g. `updateNewMessage`. For
    /// `updateShort` it is the wrapped update's; the short message forms
    /// name themselves (`updateShortMessage`, …).
    pub constructor: &'static str,
    /// By the constructor's fields: a `qts` field is [`BoxKind::Qts`]; a
    /// `pts` with a `channel_id` field, or a `Channel` constructor whose
    /// message is in a channel, is that channel's; any other `pts` is
    /// [`BoxKind::Common`]. A channel update whose channel cannot be told
    /// (Telegram's `updateNewChannelMessage(messageEmpty)` without a peer)
    /// is [`BoxKind::None`], as in grammers.
    pub box_kind: BoxKind,
    pub pts: Option<i32>,
    pub pts_count: Option<i32>,
    pub qts: Option<i32>,
    /// The chat: the message's `peer_id`, else the update's `peer`, else
    /// its `channel_id`, `chat_id` or `user_id`, in that order.
    pub peer: Option<PeerId>,
    /// The sender's user id: the message's `from_id` when a user; for a
    /// message without `from_id`, its private peer unless it is outgoing.
    /// Outside a message, the update's `from_id`, else its `user_id` (for
    /// `updateShortMessage`, unless outgoing).
    pub sender: Option<i64>,
    /// The message text (or media caption), when valid UTF-8.
    pub text: Option<&'a str>,
    /// The leading `/command` of `text`, without the `/`, up to the first
    /// whitespace; a `@bot` suffix stays in (split at the first `@`).
    pub command: Option<&'a str>,
}

/// The facts of one update: an `Update` constructor, or one of the short
/// `Updates` forms (`updateShort`, `updateShortMessage`,
/// `updateShortChatMessage`, `updateShortSentMessage`), id first.
/// Never allocates.
pub fn facts(bytes: &[u8]) -> Result<UpdateFacts<'_>, TlError> {
    let mut r = Reader::new(bytes);
    let f = facts_at(&mut r, 0)?;
    r.finish()?;
    Ok(f)
}

fn facts_at<'a>(r: &mut Reader<'a>, depth: u32) -> Result<UpdateFacts<'a>, TlError> {
    let ctor = API.read_ctor(r, None)?;
    let update_ty = API.type_index("Update");
    // A short form is an `Updates`, so only the top level may be one;
    // `updateShort.update` is an `Update`.
    let is_short = depth == 0
        && matches!(
            ctor.id,
            UPDATE_SHORT
                | UPDATE_SHORT_MESSAGE
                | UPDATE_SHORT_CHAT_MESSAGE
                | UPDATE_SHORT_SENT_MESSAGE
        );
    if ctor.ty.is_none() || (ctor.ty != update_ty && !is_short) {
        return Err(TlError::WrongType {
            want: "Update",
            got: ctor.name,
        });
    }
    let mut c = Collected::default();
    let mut inner = None;
    let depth = deeper(depth)?;
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => words[usize::from(*slot)] = r.flags(ctor, p, *known)?,
            Kind::Field { ty, flag } => {
                if !present(&words, *flag) {
                    continue;
                }
                if p.role == Role::Inner {
                    inner = Some(facts_at(r, depth)?);
                } else {
                    collect(r, &mut c, p.role, ty, depth)?;
                }
            }
        }
    }
    if let Some(inner) = inner {
        return Ok(inner);
    }
    Ok(c.into_facts(ctor))
}

/// What one constructor's walk found, by role.
#[derive(Default)]
struct Collected<'a> {
    pts: Option<i32>,
    pts_count: Option<i32>,
    qts: Option<i32>,
    channel_id: Option<i64>,
    chat_id: Option<i64>,
    user_id: Option<i64>,
    peer: Option<PeerId>,
    from: Option<PeerId>,
    from_user: Option<i64>,
    text: Option<&'a str>,
    out: bool,
    message: Option<MessageFacts<'a>>,
}

#[derive(Clone, Copy)]
struct MessageFacts<'a> {
    peer: Option<PeerId>,
    from: Option<PeerId>,
    text: Option<&'a str>,
    out: bool,
}

impl<'a> Collected<'a> {
    fn into_facts(self, ctor: &'static Ctor) -> UpdateFacts<'a> {
        let m = self.message;
        let channel = self
            .channel_id
            .or_else(|| m.and_then(|m| m.peer).and_then(PeerId::channel));
        let peer = m
            .and_then(|m| m.peer)
            .or(self.peer)
            .or(self.channel_id.map(PeerId::Channel))
            .or(self.chat_id.map(PeerId::Chat))
            .or(self.user_id.map(PeerId::User));
        let sender = match m {
            Some(m) => match m.from {
                Some(from) => from.user(),
                None if !m.out => m.peer.and_then(PeerId::user),
                None => None,
            },
            None => self
                .from
                .and_then(PeerId::user)
                .or(self.from_user)
                .or(self.user_id.filter(|_| !self.out)),
        };
        let text = m.map_or(self.text, |m| m.text);
        let box_kind = match ctor.seq {
            Seq::None => BoxKind::None,
            Seq::Common => BoxKind::Common,
            Seq::Qts => BoxKind::Qts,
            Seq::Channel => channel.map_or(BoxKind::None, BoxKind::Channel),
        };
        UpdateFacts {
            constructor: ctor.name,
            box_kind,
            pts: self.pts,
            pts_count: self.pts_count,
            qts: self.qts,
            peer,
            sender,
            text,
            command: text.and_then(command),
        }
    }
}

/// Reads a field by its role into `c`, or skips it.
fn collect<'a>(
    r: &mut Reader<'a>,
    c: &mut Collected<'a>,
    role: Role,
    ty: &'static Ty,
    depth: u32,
) -> Result<(), TlError> {
    match role {
        Role::Pts => c.pts = Some(r.i32()?),
        Role::PtsCount => c.pts_count = Some(r.i32()?),
        Role::Qts => c.qts = Some(r.i32()?),
        Role::ChannelId => c.channel_id = Some(r.i64()?),
        Role::ChatId => c.chat_id = Some(r.i64()?),
        Role::UserId => c.user_id = Some(r.i64()?),
        Role::FromUser => c.from_user = Some(r.i64()?),
        Role::Peer => c.peer = Some(peer(r)?),
        Role::From => c.from = Some(peer(r)?),
        Role::Text => c.text = std::str::from_utf8(r.tl_bytes()?).ok(),
        Role::Out => c.out = true,
        Role::Message => c.message = Some(message(r, depth)?),
        Role::None | Role::Inner => API.skip_value(r, ty, depth)?,
    }
    Ok(())
}

/// A boxed `Peer`.
fn peer(r: &mut Reader<'_>) -> Result<PeerId, TlError> {
    let at = r.pos;
    let id = r.u32()?;
    let ctor = match id {
        PEER_USER => PeerId::User,
        PEER_CHAT => PeerId::Chat,
        PEER_CHANNEL => PeerId::Channel,
        _ => {
            return Err(match API.ctor_by_id(id) {
                Some(c) => TlError::WrongType {
                    want: "Peer",
                    got: c.name,
                },
                None => TlError::UnknownConstructor { id, at },
            });
        }
    };
    Ok(ctor(r.i64()?))
}

/// A boxed `Message`: its peer, sender, text and `out` flag.
fn message<'a>(r: &mut Reader<'a>, depth: u32) -> Result<MessageFacts<'a>, TlError> {
    let ctor = API.read_ctor(r, API.type_index("Message"))?;
    let depth = deeper(depth)?;
    let mut c = Collected::default();
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => words[usize::from(*slot)] = r.flags(ctor, p, *known)?,
            Kind::Field { ty, flag } => {
                if present(&words, *flag) {
                    // A message holds no message: only its own fields count.
                    let role = if p.role == Role::Message {
                        Role::None
                    } else {
                        p.role
                    };
                    collect(r, &mut c, role, ty, depth)?;
                }
            }
        }
    }
    Ok(MessageFacts {
        peer: c.peer,
        from: c.from,
        text: c.text,
        out: c.out,
    })
}

/// The leading `/token` of `text`, without the `/`.
fn command(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    Some(&rest[..rest.find(char::is_whitespace).unwrap_or(rest.len())])
}

/// An `Updates` push cut into Telegram's own bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container<'a> {
    /// `updates`, `updatesCombined`, `updateShort`, `updateShortMessage`,
    /// `updateShortChatMessage`, `updateShortSentMessage` or
    /// `updatesTooLong`.
    pub constructor: &'static str,
    /// Each update, a boxed `Update`, for `updates` and `updatesCombined`.
    /// A short form is delivered as its raw short object (plan decision
    /// 7), so for the four short forms this is the whole input, once; for
    /// `updatesTooLong`, empty.
    pub updates: Vec<&'a [u8]>,
    /// The raw `Vector<User>`, vector id and count included.
    pub users: Option<&'a [u8]>,
    /// The raw `Vector<Chat>`, vector id and count included.
    pub chats: Option<&'a [u8]>,
    pub date: Option<i32>,
    pub seq_start: Option<i32>,
    pub seq: Option<i32>,
}

/// Cuts one boxed `Updates` object into its parts.
pub fn container(bytes: &[u8]) -> Result<Container<'_>, TlError> {
    let mut r = Reader::new(bytes);
    let ctor = API.read_ctor(&mut r, API.type_index("Updates"))?;
    let short = matches!(
        ctor.id,
        UPDATE_SHORT | UPDATE_SHORT_MESSAGE | UPDATE_SHORT_CHAT_MESSAGE | UPDATE_SHORT_SENT_MESSAGE
    );
    let mut c = Container {
        constructor: ctor.name,
        updates: Vec::new(),
        users: None,
        chats: None,
        date: None,
        seq_start: None,
        seq: None,
    };
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => {
                words[usize::from(*slot)] = r.flags(ctor, p, *known)?;
            }
            Kind::Field { ty, flag } => {
                if !present(&words, *flag) {
                    continue;
                }
                match (p.name, ty) {
                    ("updates", Ty::Vector(t)) => c.updates = items(&mut r, t)?,
                    ("users", _) => c.users = Some(slice(&mut r, ty)?),
                    ("chats", _) => c.chats = Some(slice(&mut r, ty)?),
                    ("date", Ty::Int) => c.date = Some(r.i32()?),
                    ("seq", Ty::Int) => c.seq = Some(r.i32()?),
                    ("seq_start", Ty::Int) => c.seq_start = Some(r.i32()?),
                    _ => API.skip_value(&mut r, ty, 1)?,
                }
            }
        }
    }
    r.finish()?;
    if short {
        c.updates = vec![bytes];
    }
    Ok(c)
}

/// An `updates.Difference` or `updates.ChannelDifference` cut into
/// Telegram's own bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Difference<'a> {
    /// `updates.differenceEmpty`, `updates.difference`,
    /// `updates.differenceSlice`, `updates.differenceTooLong`,
    /// `updates.channelDifferenceEmpty`, `updates.channelDifferenceTooLong`
    /// or `updates.channelDifference`.
    pub constructor: &'static str,
    /// Each message, a boxed `Message`: `new_messages`, or the `messages`
    /// of `updates.channelDifferenceTooLong`.
    pub new_messages: Vec<&'a [u8]>,
    /// Each boxed `EncryptedMessage` (common difference only).
    pub new_encrypted_messages: Vec<&'a [u8]>,
    /// Each boxed `Update`.
    pub other_updates: Vec<&'a [u8]>,
    /// The raw `Vector<User>`, vector id and count included.
    pub users: Option<&'a [u8]>,
    /// The raw `Vector<Chat>`, vector id and count included.
    pub chats: Option<&'a [u8]>,
    /// The raw boxed `updates.State`: `state` of `updates.difference`,
    /// `intermediate_state` of `updates.differenceSlice`.
    pub state: Option<&'a [u8]>,
    /// The raw boxed `Dialog` of `updates.channelDifferenceTooLong`.
    pub dialog: Option<&'a [u8]>,
    /// The state to continue from: the `state`'s or `intermediate_state`'s
    /// fields; `differenceEmpty`'s `date` and `seq`; `differenceTooLong`'s
    /// and the channel forms' `pts` (`channelDifferenceTooLong`: its
    /// dialog's `pts`, when the dialog has one).
    pub pts: Option<i32>,
    pub qts: Option<i32>,
    pub date: Option<i32>,
    pub seq: Option<i32>,
    pub unread_count: Option<i32>,
    /// A channel difference's `final` flag.
    pub is_final: bool,
    /// A channel difference's `timeout`.
    pub timeout: Option<i32>,
}

/// Cuts one boxed difference into its parts.
pub fn difference(bytes: &[u8]) -> Result<Difference<'_>, TlError> {
    let mut r = Reader::new(bytes);
    let ctor = API.read_ctor(&mut r, None)?;
    let ty = ctor.ty.map(|t| API.type_def(t).name);
    if !matches!(ty, Some("updates.Difference" | "updates.ChannelDifference")) {
        return Err(TlError::WrongType {
            want: "updates.Difference",
            got: ctor.name,
        });
    }
    let mut d = Difference {
        constructor: ctor.name,
        new_messages: Vec::new(),
        new_encrypted_messages: Vec::new(),
        other_updates: Vec::new(),
        users: None,
        chats: None,
        state: None,
        dialog: None,
        pts: None,
        qts: None,
        date: None,
        seq: None,
        unread_count: None,
        is_final: false,
        timeout: None,
    };
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => {
                words[usize::from(*slot)] = r.flags(ctor, p, *known)?;
            }
            Kind::Field { ty, flag } => {
                if !present(&words, *flag) {
                    continue;
                }
                match (p.name, ty) {
                    ("new_messages" | "messages", Ty::Vector(t)) => {
                        d.new_messages = items(&mut r, t)?
                    }
                    ("new_encrypted_messages", Ty::Vector(t)) => {
                        d.new_encrypted_messages = items(&mut r, t)?;
                    }
                    ("other_updates", Ty::Vector(t)) => d.other_updates = items(&mut r, t)?,
                    ("users", _) => d.users = Some(slice(&mut r, ty)?),
                    ("chats", _) => d.chats = Some(slice(&mut r, ty)?),
                    ("state" | "intermediate_state", Ty::Boxed(_)) => {
                        let start = r.pos;
                        ints(&mut r, ty, &mut d, true)?;
                        d.state = Some(&bytes[start..r.pos]);
                    }
                    ("dialog", Ty::Boxed(_)) => {
                        let start = r.pos;
                        ints(&mut r, ty, &mut d, false)?;
                        d.dialog = Some(&bytes[start..r.pos]);
                    }
                    ("final", Ty::True) => d.is_final = true,
                    ("pts", Ty::Int) => d.pts = Some(r.i32()?),
                    ("date", Ty::Int) => d.date = Some(r.i32()?),
                    ("seq", Ty::Int) => d.seq = Some(r.i32()?),
                    ("timeout", Ty::Int) => d.timeout = Some(r.i32()?),
                    _ => API.skip_value(&mut r, ty, 1)?,
                }
            }
        }
    }
    r.finish()?;
    Ok(d)
}

/// The fields of an `updates.State`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub pts: i32,
    pub qts: i32,
    pub date: i32,
    pub seq: i32,
    pub unread_count: i32,
}

/// Reads one boxed `updates.State` (the answer to `updates.getState`).
pub fn state(bytes: &[u8]) -> Result<State, TlError> {
    let mut r = Reader::new(bytes);
    let ctor = API.read_ctor(&mut r, API.type_index("updates.State"))?;
    let mut s = State::default();
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => words[usize::from(*slot)] = r.flags(ctor, p, *known)?,
            Kind::Field { ty, flag } => {
                if !present(&words, *flag) {
                    continue;
                }
                match (p.name, ty) {
                    ("pts", Ty::Int) => s.pts = r.i32()?,
                    ("qts", Ty::Int) => s.qts = r.i32()?,
                    ("date", Ty::Int) => s.date = r.i32()?,
                    ("seq", Ty::Int) => s.seq = r.i32()?,
                    ("unread_count", Ty::Int) => s.unread_count = r.i32()?,
                    _ => API.skip_value(&mut r, ty, 1)?,
                }
            }
        }
    }
    r.finish()?;
    Ok(s)
}

/// Appends the peers a raw `Vector<User>` or `Vector<Chat>` names to `out`:
/// each user, basic group, channel or community with its kind and access
/// hash. Empty constructors (`userEmpty`, `chatEmpty`) name nothing. A
/// `min` constructor's access hash is not one tgin may cache, so it is
/// left out (Telegram: a min hash never replaces a full one).
pub fn peers(vector: &[u8], out: &mut Vec<Peer>) -> Result<(), TlError> {
    let mut r = Reader::new(vector);
    r.vector_id()?;
    let n = r.count(4)?;
    let (user_ty, chat_ty) = (API.type_index("User"), API.type_index("Chat"));
    for _ in 0..n {
        let ctor = API.read_ctor(&mut r, None)?;
        if ctor.ty.is_none() || (ctor.ty != user_ty && ctor.ty != chat_ty) {
            return Err(TlError::WrongType {
                want: "User or Chat",
                got: ctor.name,
            });
        }
        if let Some(p) = peer_entry(&mut r, ctor)? {
            out.push(p);
        }
    }
    r.finish()
}

/// One `User` or `Chat` constructor's fields (its id already read).
fn peer_entry(r: &mut Reader<'_>, ctor: &'static Ctor) -> Result<Option<Peer>, TlError> {
    let (mut id, mut hash) = (None, None);
    let (mut min, mut bot, mut is_self) = (false, false, false);
    let (mut broadcast, mut megagroup, mut gigagroup) = (false, false, false);
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => words[usize::from(*slot)] = r.flags(ctor, p, *known)?,
            Kind::Field { ty, flag } => {
                if !present(&words, *flag) {
                    continue;
                }
                match (p.name, ty) {
                    ("id", Ty::Long) => id = Some(r.i64()?),
                    ("access_hash", Ty::Long) => hash = Some(r.i64()?),
                    ("min", Ty::True) => min = true,
                    ("bot", Ty::True) => bot = true,
                    ("self", Ty::True) => is_self = true,
                    ("broadcast", Ty::True) => broadcast = true,
                    ("megagroup", Ty::True) => megagroup = true,
                    ("gigagroup", Ty::True) => gigagroup = true,
                    _ => API.skip_value(r, ty, 1)?,
                }
            }
        }
    }
    let kind = match ctor.name {
        "user" => PeerKind::User {
            bot: Some(bot),
            is_self: (!min).then_some(is_self),
        },
        "chat" | "chatForbidden" => PeerKind::Chat,
        "channel" | "channelForbidden" => PeerKind::Channel {
            kind: if gigagroup {
                Some(ChannelKind::Gigagroup)
            } else if megagroup {
                Some(ChannelKind::Megagroup)
            } else if broadcast {
                Some(ChannelKind::Broadcast)
            } else {
                None
            },
        },
        "community" | "communityForbidden" => PeerKind::Channel {
            kind: Some(ChannelKind::Community),
        },
        _ => return Ok(None),
    };
    Ok(id.map(|id| Peer {
        id,
        kind,
        access_hash: hash.filter(|_| !min),
    }))
}

/// One boxed object of type `ty`, keeping its top-level `pts`, and with
/// `state` also its `qts`, `date`, `seq` and `unread_count`; everything
/// else is skipped.
fn ints(
    r: &mut Reader<'_>,
    ty: &'static Ty,
    d: &mut Difference<'_>,
    state: bool,
) -> Result<(), TlError> {
    let Ty::Boxed(t) = ty else {
        return API.skip_value(r, ty, 1);
    };
    let ctor = API.read_ctor(r, Some(*t))?;
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => words[usize::from(*slot)] = r.flags(ctor, p, *known)?,
            Kind::Field { ty, flag } => {
                if !present(&words, *flag) {
                    continue;
                }
                match (p.name, ty) {
                    ("pts", Ty::Int) => d.pts = Some(r.i32()?),
                    ("qts", Ty::Int) if state => d.qts = Some(r.i32()?),
                    ("date", Ty::Int) if state => d.date = Some(r.i32()?),
                    ("seq", Ty::Int) if state => d.seq = Some(r.i32()?),
                    ("unread_count", Ty::Int) if state => d.unread_count = Some(r.i32()?),
                    _ => API.skip_value(r, ty, 2)?,
                }
            }
        }
    }
    Ok(())
}

/// One value of `ty`, as the bytes it took.
fn slice<'a>(r: &mut Reader<'a>, ty: &'static Ty) -> Result<&'a [u8], TlError> {
    let start = r.pos;
    API.skip_value(r, ty, 1)?;
    Ok(&r.buf[start..r.pos])
}

/// A `Vector<T>` (vector id, count, items): each item as the bytes it took.
fn items<'a>(r: &mut Reader<'a>, t: &'static Ty) -> Result<Vec<&'a [u8]>, TlError> {
    r.vector_id()?;
    let n = r.count(API.min_size(t))?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(slice(r, t)?);
    }
    Ok(v)
}

#[cfg(test)]
mod tests;
