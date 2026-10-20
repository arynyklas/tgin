//! The account side of the raw-TL RPC endpoint (contract in DOCS.md, User
//! accounts).
//!
//! - [`unwrap_request`] reads a downstream request against the api schema:
//!   `initConnection` and `invokeWithLayer(account layer, …)` are stripped;
//!   any other wrapper (a function with a `query:!X` field), a foreign
//!   layer, the update-state methods and the `auth.*` methods that mint or
//!   move an authorization ([`AUTHORIZATION_METHODS`]) are refused. What is left is one
//!   whole, strictly checked function call.
//! - [`call`] runs that call on an [`AccountRpc`] with the answer rules:
//!   one resend of a request never written, never a resend of one that may
//!   have run, one flood sleep up to the threshold, `*_MIGRATE_X` to a
//!   media session, and the pts of a `messages.affected*` answer handed to
//!   the update manager through an [`AffectedSink`], in the box of the
//!   channel the call targets or the common box.
//!
//! Nothing here logs or keeps request or answer bytes.

use std::fmt;
use std::pin::Pin;
use std::time::Duration;

use tokio::time::{Instant, sleep_until};

use bytes::Bytes;

use crate::decode::{BoxKind, PeerId};
use crate::retry::{Wait, WaitKind};
use crate::schema::{API, Ctor, Kind, MAX_FLAG_WORDS, Reader, Role, Ty, deeper, present};
use crate::{RpcOutcome, SenderError, TlError};

/// A boxed, sendable future: the trait methods below stay object safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Which session of the account a request goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// The main session on the home DC.
    Main,
    /// The media session to this DC (after a `*_MIGRATE_X`).
    Media(i32),
}

/// Why an account could not give a request an [`RpcOutcome`].
#[derive(Debug, thiserror::Error)]
pub enum InvokeError {
    #[error(transparent)]
    Sender(#[from] SenderError),
    /// The media session to `dc` could not be set up or used. The request
    /// was not executed.
    #[error("media session to DC {dc}: {failure}")]
    Media { dc: i32, failure: MediaFailure },
    /// The account's request budget (`max_rps`) refused a further request
    /// to Telegram: a resend, a migrate hop or a media setup. Not executed.
    #[error("the account's request budget is spent for now")]
    RateLimited { retry_after: Duration },
}

/// The account's request budget: one [`Charge::charge`] per request
/// written to Telegram.
pub trait Charge: Send + Sync {
    /// Takes one request from the budget, or says how long until one is
    /// free.
    fn charge(&self) -> Result<(), Duration>;
}

impl<C: Charge + ?Sized> Charge for std::sync::Arc<C> {
    fn charge(&self) -> Result<(), Duration> {
        C::charge(self)
    }
}

/// Charges `charge` for one more request.
pub(crate) fn charged(charge: &dyn Charge) -> Result<(), InvokeError> {
    charge
        .charge()
        .map_err(|retry_after| InvokeError::RateLimited { retry_after })
}

/// Why a media session could not take a request. Either way the request
/// was not executed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MediaFailure {
    /// Telegram asked tgin to wait this many seconds (`FLOOD_WAIT_X` on
    /// `auth.exportAuthorization`, `auth.importAuthorization` or the media
    /// connection's init call).
    #[error("{what}: wait {seconds} s")]
    FloodWait { what: &'static str, seconds: u32 },
    /// Anything else: connecting, the authorization or the session store
    /// failed.
    #[error("{what}")]
    Failed { what: &'static str },
}

/// A running account, as the RPC endpoint sees it. The account
/// supervisor implements it; [`crate::media::AccountSessions`] is the
/// session-routing part.
pub trait AccountRpc: Send + Sync {
    /// The one layer the account serves.
    fn layer(&self) -> i32;
    /// Sends one unwrapped request on `route`.
    fn invoke(&self, route: Route, body: Bytes) -> BoxFuture<'_, Result<RpcOutcome, InvokeError>>;
}

/// Where the pts of a `messages.affected*` answer go: the account's update
/// manager ([`crate::UpdateManager::feed_affected`]).
pub trait AffectedSink: Send + Sync {
    fn feed_affected(&self, box_kind: BoxKind, pts: i32, pts_count: i32);
}

/// An unwrapped request.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Call<'a> {
    /// The function's full name, e.g. `messages.sendMessage`.
    pub method: &'static str,
    /// The function's bytes, wrappers stripped; a suffix of the request.
    pub body: &'a [u8],
    /// The update box an affected-pts answer counts in: the channel the
    /// call targets (its `channel`, `parent_peer` or `peer`), else the
    /// common box.
    pub context: BoxKind,
    /// The user, chat or channel the call targets (its `channel`,
    /// `parent_peer` or `peer`), if it names one.
    pub peer: Option<PeerId>,
    /// The call spends the account's Stars or TON: it sets
    /// `allow_paid_stars` (it agrees to pay the recipient's price) or
    /// `allow_paid_floodskip` (to pay to skip a flood wait), offers a
    /// `suggestedPost` with a `price`, or holds an `inputMediaStakeDice`.
    /// Read from the schema, on every function and anywhere in the call.
    pub pays_stars: bool,
    /// The call writes the account's quick replies: a send that files its
    /// message under a `quick_reply_shortcut`, or an edit of one by
    /// `quick_reply_shortcut_id`. Quick replies hold the greeting and away
    /// messages every new chat receives.
    pub writes_quick_replies: bool,
}

/// Leaves out the body: it is account content.
impl fmt::Debug for Call<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Call")
            .field("method", &self.method)
            .field("body", &format_args!("<{} bytes>", self.body.len()))
            .field("context", &self.context)
            .field("peer", &self.peer)
            .field("pays_stars", &self.pays_stars)
            .field("writes_quick_replies", &self.writes_quick_replies)
            .finish()
    }
}

/// Why a request is refused before it reaches Telegram.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// Not whole, valid TL of the account's layer.
    #[error("the request is not valid TL: {0}")]
    Malformed(#[from] TlError),
    /// A constructor, not a function.
    #[error("`{0}` is not a function")]
    NotAFunction(&'static str),
    /// `invokeWithLayer` naming another layer.
    #[error("the request is in layer {got}; the account serves layer {serves}")]
    ForeignLayer { got: i32, serves: i32 },
    /// An `invoke*` wrapper other than `invokeWithLayer`.
    #[error("`{0}` is not forwarded")]
    Wrapper(&'static str),
    /// `updates.getState`, `getDifference` or `getChannelDifference`:
    /// updates come from consumers.
    #[error("`{0}` is answered only to tgin's own session; take updates from a consumer")]
    UpdatesMethod(&'static str),
    /// An `auth.*` method that mints or moves an authorization of the
    /// account: tgin owns them.
    #[error("`{0}` mints or moves an authorization of the account; tgin owns those")]
    AuthorizationMethod(&'static str),
}

/// Refused whatever a client's allowlist says (the owner may
/// overrule it): each mints or moves an authorization of the account, and
/// tgin owns the account's authorizations. `bots.exportBotToken` hands
/// out, or with `revoke` rotates, the token of a bot the account manages,
/// and `phone.getGroupCallStreamRtmpUrl` the RTMP stream key of a channel
/// the account streams to: credential exports of the same kind.
pub const AUTHORIZATION_METHODS: [&str; 10] = [
    "auth.exportAuthorization",
    "auth.importAuthorization",
    "auth.acceptLoginToken",
    "auth.exportLoginToken",
    "auth.importLoginToken",
    "auth.bindTempAuthKey",
    "auth.importBotAuthorization",
    "auth.importWebTokenAuthorization",
    "bots.exportBotToken",
    "phone.getGroupCallStreamRtmpUrl",
];

/// Refused whatever a client's allowlist says: updates reach a client
/// through a consumer, and only tgin's own session catches up.
pub const UPDATES_METHODS: [&str; 3] = [
    "updates.getState",
    "updates.getDifference",
    "updates.getChannelDifference",
];

/// Unwraps a downstream request for an account serving `layer` (plan
/// decision 3).
pub fn unwrap_request(body: &[u8], layer: i32) -> Result<Call<'_>, Refused> {
    let mut rest = body;
    loop {
        let mut r = Reader::new(rest);
        let ctor = API.read_ctor(&mut r, None)?;
        if ctor.ty.is_some() {
            return Err(Refused::NotAFunction(ctor.name));
        }
        match ctor.name {
            "invokeWithLayer" => {
                let got = r.i32()?;
                if got != layer {
                    return Err(Refused::ForeignLayer { got, serves: layer });
                }
                rest = &rest[r.pos..];
            }
            "initConnection" => rest = &rest[query_start(&mut r, ctor)?..],
            name if always_refused(name) => {
                return Err(if is_wrapper(ctor) {
                    Refused::Wrapper(name)
                } else if UPDATES_METHODS.contains(&name) {
                    Refused::UpdatesMethod(name)
                } else {
                    Refused::AuthorizationMethod(name)
                });
            }
            method => {
                let checked = check_call(&mut r, ctor)?;
                r.finish()?;
                return Ok(Call {
                    method,
                    body: rest,
                    context: checked.context,
                    peer: checked.peer,
                    pays_stars: checked.pays_stars,
                    writes_quick_replies: checked.writes_quick_replies,
                });
            }
        }
    }
}

/// Whether [`unwrap_request`] refuses a call to `method` whatever a
/// client's allowlist says: [`AUTHORIZATION_METHODS`], [`UPDATES_METHODS`]
/// and every `invoke*` wrapper but the two it strips (`initConnection`,
/// and `invokeWithLayer` in the account's layer). It refuses such a call
/// as soon as it has read the function's id.
#[must_use]
pub fn always_refused(method: &str) -> bool {
    if AUTHORIZATION_METHODS.contains(&method) || UPDATES_METHODS.contains(&method) {
        return true;
    }
    match API.ctor_by_name(method) {
        Some(ctor) if ctor.ty.is_none() => {
            is_wrapper(ctor) && !matches!(method, "initConnection" | "invokeWithLayer")
        }
        _ => false,
    }
}

/// The constructor id of the api schema's function `method`; `None` for a
/// name that is no function.
#[must_use]
pub fn function_id(method: &str) -> Option<u32> {
    API.ctor_by_name(method)
        .filter(|ctor| ctor.ty.is_none())
        .map(|ctor| ctor.id)
}

/// A function whose result is its `query:!X`'s.
fn is_wrapper(ctor: &'static Ctor) -> bool {
    ctor.params
        .iter()
        .any(|p| matches!(p.kind, Kind::Field { ty: Ty::Any, .. }))
}

/// Skips a wrapper's fields up to its `query`; the offset where it starts.
fn query_start(r: &mut Reader<'_>, ctor: &'static Ctor) -> Result<usize, TlError> {
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => words[usize::from(*slot)] = r.flags(ctor, p, *known)?,
            Kind::Field { ty: Ty::Any, .. } => return Ok(r.pos),
            Kind::Field { ty, flag } => {
                if present(&words, *flag) {
                    API.skip_value(r, ty, 1)?;
                }
            }
        }
    }
    Err(TlError::Eof { at: r.pos })
}

/// Checks a call's fields strictly; returns its update context: the channel
/// its target field ([`target_field`]) names, else the common box (none
/// for a `channels.*` call without one). With a `parent_peer` field
/// (`messages.deleteSavedHistory`) the target is that, as `peer` is then a
/// saved dialog of the account's own Saved Messages.
///
/// Also returns the peer that field names, and whether the call sets
/// `allow_paid_stars`.
fn check_call(r: &mut Reader<'_>, ctor: &'static Ctor) -> Result<Checked, TlError> {
    let channels = ctor.name.starts_with("channels.");
    let target = target_field(ctor, channels);
    let mut peer = None;
    let mut pays_stars = false;
    let mut writes_quick_replies = false;
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => words[usize::from(*slot)] = r.flags(ctor, p, *known)?,
            Kind::Field { ty, flag } => {
                if !present(&words, *flag) {
                    continue;
                }
                pays_stars |= PAID_STARS_FIELDS.contains(&p.name);
                writes_quick_replies |= QUICK_REPLY_FIELDS.contains(&p.name);
                let start = r.pos;
                pays_stars |= skip_spending(r, ty, 1)?;
                if Some(p.name) == target {
                    peer = peer_id(&r.buf[start..r.pos]);
                }
            }
        }
    }
    let context = match peer {
        Some(PeerId::Channel(id)) => BoxKind::Channel(id),
        _ if channels => BoxKind::None,
        _ => BoxKind::Common,
    };
    Ok(Checked {
        context,
        peer,
        pays_stars,
        writes_quick_replies,
    })
}

/// Calls whose target chat is a field the general rule in
/// [`target_field`] would not pick.
const TARGET_FIELDS: [(&str, &str); 1] = [("messages.forwardMessages", "to_peer")];

/// The field that names the chat a call targets: `channel` on a
/// `channels.*` call, a listed field ([`TARGET_FIELDS`]), `parent_peer`
/// when there is one, else `peer`, else the first `InputPeer` field whose
/// name ends in `peer` and is no pagination cursor (`offset_peer`). `None`
/// when the call names no chat.
fn target_field(ctor: &'static Ctor, channels: bool) -> Option<&'static str> {
    if channels {
        return Some("channel");
    }
    if let Some(&(_, field)) = TARGET_FIELDS.iter().find(|(m, _)| *m == ctor.name) {
        return Some(field);
    }
    let named = |name: &str| ctor.params.iter().any(|p| p.name == name);
    if named("parent_peer") {
        return Some("parent_peer");
    }
    if named("peer") {
        return Some("peer");
    }
    ctor.params
        .iter()
        .find(|p| {
            p.name.ends_with("peer")
                && !p.name.starts_with("offset_")
                && matches!(&p.kind, Kind::Field { ty: Ty::Boxed(t), .. }
                    if API.type_def(*t).name == "InputPeer")
        })
        .map(|p| p.name)
}

/// The fields by which a call agrees to pay Stars: a recipient's price
/// (`allow_paid_stars`), or to skip a flood wait (`allow_paid_floodskip`).
const PAID_STARS_FIELDS: [&str; 2] = ["allow_paid_stars", "allow_paid_floodskip"];

/// Skips one value of `ty`, as [`Schema::skip_value`](crate::schema)
/// does, and says whether it spends the account's Stars or TON anywhere
/// inside: a `suggestedPost` that offers a `price` (charged when the
/// channel accepts the post), or an `inputMediaStakeDice` (a TON bet).
fn skip_spending(r: &mut Reader<'_>, ty: &'static Ty, depth: u32) -> Result<bool, TlError> {
    match ty {
        Ty::Boxed(t) => spending_object(r, Some(*t), depth),
        Ty::Any => spending_object(r, None, depth),
        Ty::Bare(c) => spending_fields(r, API.ctor(*c), depth),
        Ty::Vector(t) => {
            r.vector_id()?;
            spending_items(r, t, depth)
        }
        Ty::BareVector(t) => spending_items(r, t, depth),
        _ => API.skip_value(r, ty, depth).map(|()| false),
    }
}

fn spending_object(r: &mut Reader<'_>, want: Option<u16>, depth: u32) -> Result<bool, TlError> {
    let ctor = API.read_ctor(r, want)?;
    let stake = ctor.name == "inputMediaStakeDice";
    Ok(spending_fields(r, ctor, depth)? || stake)
}

fn spending_fields(r: &mut Reader<'_>, ctor: &'static Ctor, depth: u32) -> Result<bool, TlError> {
    let depth = deeper(depth)?;
    let offer = ctor.name == "suggestedPost";
    let mut spends = false;
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => words[usize::from(*slot)] = r.flags(ctor, p, *known)?,
            Kind::Field { ty, flag } => {
                if present(&words, *flag) {
                    spends |= offer && p.name == "price";
                    spends |= skip_spending(r, ty, depth)?;
                }
            }
        }
    }
    Ok(spends)
}

fn spending_items(r: &mut Reader<'_>, t: &'static Ty, depth: u32) -> Result<bool, TlError> {
    let depth = deeper(depth)?;
    let n = r.count(API.min_size(t))?;
    let mut spends = false;
    for _ in 0..n {
        spends |= skip_spending(r, t, depth)?;
    }
    Ok(spends)
}

/// The fields by which a call writes the account's quick replies.
const QUICK_REPLY_FIELDS: [&str; 2] = ["quick_reply_shortcut", "quick_reply_shortcut_id"];

/// What [`check_call`] reads from a call's fields.
struct Checked {
    context: BoxKind,
    peer: Option<PeerId>,
    pays_stars: bool,
    writes_quick_replies: bool,
}

/// The peer an `InputPeer` or `InputChannel` names (already checked):
/// `inputChannel`, `inputChannelFromMessage`, `inputPeerChannel` and
/// `inputPeerChannelFromMessage` a channel, `inputPeerChat` a chat,
/// `inputPeerUser` and `inputPeerUserFromMessage` a user; `None` for any
/// other (`inputPeerSelf`, `inputPeerEmpty`).
fn peer_id(bytes: &[u8]) -> Option<PeerId> {
    let mut r = Reader::new(bytes);
    let ctor = API.read_ctor(&mut r, None).ok()?;
    let (role, make): (Role, fn(i64) -> PeerId) = match ctor.name {
        "inputChannel"
        | "inputChannelFromMessage"
        | "inputPeerChannel"
        | "inputPeerChannelFromMessage" => (Role::ChannelId, PeerId::Channel),
        "inputPeerChat" => (Role::ChatId, PeerId::Chat),
        "inputPeerUser" | "inputPeerUserFromMessage" => (Role::UserId, PeerId::User),
        _ => return None,
    };
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => {
                words[usize::from(*slot)] = r.flags(ctor, p, *known).ok()?
            }
            Kind::Field { ty, flag } => {
                if !present(&words, *flag) {
                    continue;
                }
                if p.role == role {
                    return r.i64().ok().map(make);
                }
                API.skip_value(&mut r, ty, 1).ok()?;
            }
        }
    }
    None
}

/// `(pts, pts_count)` of a `messages.affectedMessages`, `affectedHistory`
/// or `affectedFoundMessages` answer; `None` for any other answer.
#[must_use]
pub fn affected_pts(result: &[u8]) -> Option<(i32, i32)> {
    let mut r = Reader::new(result);
    let ctor = API.read_ctor(&mut r, None).ok()?;
    if !ctor.name.starts_with("messages.affected") {
        return None;
    }
    let (mut pts, mut count) = (None, None);
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => {
                words[usize::from(*slot)] = r.flags(ctor, p, *known).ok()?
            }
            Kind::Field { ty, flag } => {
                if !present(&words, *flag) {
                    continue;
                }
                match p.role {
                    Role::Pts => pts = Some(r.i32().ok()?),
                    Role::PtsCount => count = Some(r.i32().ok()?),
                    _ => API.skip_value(&mut r, ty, 1).ok()?,
                }
                if let (Some(pts), Some(count)) = (pts, count) {
                    return Some((pts, count));
                }
            }
        }
    }
    None
}

/// The classes of `rpc_error` the endpoint acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// `FLOOD_WAIT_X` or `FLOOD_PREMIUM_WAIT_X`: wait X seconds.
    FloodWait(u32),
    /// `SLOWMODE_WAIT_X`.
    SlowMode(u32),
    /// `FROZEN_*`: the account is frozen.
    Frozen,
    /// A 303 `*_MIGRATE_X`: the call belongs on DC X.
    Migrate(i32),
    Other,
}

/// Classifies an `rpc_error` by its code and message.
#[must_use]
pub fn classify(code: i32, message: &str) -> ErrorClass {
    if let Some(wait) = Wait::parse(message)
        && let Ok(x) = u32::try_from(wait.seconds)
    {
        match wait.kind {
            WaitKind::Flood | WaitKind::PremiumFlood => return ErrorClass::FloodWait(x),
            WaitKind::SlowMode => return ErrorClass::SlowMode(x),
            WaitKind::Other => {}
        }
    }
    if message.starts_with("FROZEN_") {
        return ErrorClass::Frozen;
    }
    if code == 303
        && let Some((head, dc)) = message.rsplit_once('_')
        && head.ends_with("_MIGRATE")
        && let Ok(dc) = dc.parse::<i32>()
    {
        return ErrorClass::Migrate(dc);
    }
    ErrorClass::Other
}

/// How [`call`] ended.
pub enum Answer {
    /// Telegram's `rpc_result`, raw.
    Result(Bytes),
    /// Telegram's `rpc_error`, for the endpoint to pass on raw. `class`
    /// tells the endpoint whether to map it to 429 or 403.
    Error {
        code: i32,
        message: String,
        class: ErrorClass,
    },
    /// No outcome; see [`InvokeError`].
    Failed(InvokeError),
}

/// Leaves out the result's bytes: they are account content.
impl fmt::Debug for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Result(bytes) => write!(f, "Result(<{} bytes>)", bytes.len()),
            Self::Error {
                code,
                message,
                class,
            } => write!(f, "Error({code} {message}, {class:?})"),
            Self::Failed(e) => write!(f, "Failed({e:?})"),
        }
    }
}

/// Runs one unwrapped call:
/// - [`SenderError::NotSent`]: sent once more; the sender opens a fresh
///   connection for it;
/// - [`SenderError::NoAnswer`] and every other failure: returned, never
///   resent;
/// - `FLOOD_WAIT_X` with X ≤ `flood_sleep_threshold`: slept once, then sent
///   again on the same route; if `drain` completes during the sleep, the
///   flood wait is answered with what is left of it;
/// - `*_MIGRATE_X` from the main session: sent once to the media session
///   of DC X;
/// - a `messages.affected*` answer: its pts go to `sink` in `context`;
/// - each send after the first is charged to `charge`; a refusal ends the
///   call with [`InvokeError::RateLimited`].
pub async fn call(
    account: &dyn AccountRpc,
    sink: &dyn AffectedSink,
    context: BoxKind,
    body: Bytes,
    flood_sleep_threshold: Duration,
    charge: &dyn Charge,
    drain: impl Future<Output = ()>,
) -> Answer {
    let mut drain = std::pin::pin!(drain);
    let mut route = Route::Main;
    let mut resent = false;
    let mut slept = false;
    let mut first = true;
    loop {
        // The first send is the caller's to charge; every further one is
        // charged here.
        if !first && let Err(e) = charged(charge) {
            return Answer::Failed(e);
        }
        first = false;
        match account.invoke(route, body.clone()).await {
            Err(InvokeError::Sender(SenderError::NotSent)) if !resent => resent = true,
            Err(e) => return Answer::Failed(e),
            Ok(RpcOutcome::Result(bytes)) => {
                if let Some((pts, pts_count)) = affected_pts(&bytes) {
                    sink.feed_affected(context, pts, pts_count);
                }
                return Answer::Result(bytes);
            }
            Ok(RpcOutcome::Error { code, message }) => {
                let class = classify(code, &message);
                match class {
                    ErrorClass::FloodWait(x)
                        if !slept && Duration::from_secs(u64::from(x)) <= flood_sleep_threshold =>
                    {
                        slept = true;
                        let until = Instant::now() + Duration::from_secs(u64::from(x));
                        tokio::select! {
                            () = sleep_until(until) => {}
                            () = &mut drain => {
                                // Not executed: the client retries after
                                // what is left of the wait.
                                let left = until.saturating_duration_since(Instant::now());
                                let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
                                return Answer::Error {
                                    code,
                                    message,
                                    class: ErrorClass::FloodWait(u32::try_from(secs).unwrap_or(x)),
                                };
                            }
                        }
                    }
                    ErrorClass::Migrate(dc) if route == Route::Main => route = Route::Media(dc),
                    _ => {
                        return Answer::Error {
                            code,
                            message,
                            class,
                        };
                    }
                }
            }
        }
    }
}

/// `rpc_error#2144ca19 error_code:int error_message:string`: the bytes of
/// an error Telegram sent, composed again for the client.
#[must_use]
pub fn rpc_error_bytes(code: i32, message: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + message.len());
    out.extend_from_slice(&0x2144_ca19_u32.to_le_bytes());
    out.extend_from_slice(&code.to_le_bytes());
    // Telegram's messages are short ASCII; a longer one is cut to TL's bound.
    let msg = &message.as_bytes()[..message.len().min((1 << 24) - 1)];
    let _ = crate::schema::write_tl_bytes(&mut out, msg);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_field_names_the_chat_a_call_targets() {
        let target = |name: &str| {
            let ctor = API.ctor_by_name(name).expect("a function");
            target_field(ctor, name.starts_with("channels."))
        };
        assert_eq!(target("channels.deleteMessages"), Some("channel"));
        assert_eq!(target("messages.forwardMessages"), Some("to_peer"));
        assert_eq!(target("messages.deleteSavedHistory"), Some("parent_peer"));
        assert_eq!(target("messages.sendMessage"), Some("peer"));
        assert_eq!(
            target("payments.getPremiumGiftCodeOptions"),
            Some("boost_peer")
        );
        // A pagination cursor is no target.
        assert_eq!(target("messages.getDialogs"), None);
        assert_eq!(target("help.getConfig"), None);
    }

    #[test]
    fn debug_prints_lengths_not_bytes() {
        let body = [0xab_u8; 12];
        let call = Call {
            method: "help.getConfig",
            body: &body,
            context: BoxKind::Common,
            peer: None,
            pays_stars: false,
            writes_quick_replies: false,
        };
        let call = format!("{call:?}");
        assert!(
            call.contains("<12 bytes>") && !call.contains("171"),
            "{call}"
        );
        let answer = format!("{:?}", Answer::Result(Bytes::from_static(&[0xab; 8])));
        assert_eq!(answer, "Result(<8 bytes>)");
    }

    #[test]
    fn every_function_has_a_result_type() {
        use crate::schema::Ty;
        for schema in [&API, &crate::schema::MTPROTO] {
            for ctor in schema.ctors {
                match (ctor.ty, &ctor.result) {
                    (Some(_), None) => {}
                    (None, Some(Ty::Any)) => {
                        assert!(is_wrapper(ctor), "{} has no known result type", ctor.name)
                    }
                    (None, Some(_)) => {}
                    _ => panic!("{} result {:?}", ctor.name, ctor.result),
                }
            }
        }
        let users = API
            .ctor_by_name("users.getUsers")
            .and_then(|c| c.result.as_ref());
        assert!(matches!(users, Some(Ty::Vector(Ty::Boxed(_)))));
    }

    #[test]
    fn classify_names_each_class() {
        assert_eq!(classify(420, "FLOOD_WAIT_30"), ErrorClass::FloodWait(30));
        assert_eq!(
            classify(420, "FLOOD_PREMIUM_WAIT_5"),
            ErrorClass::FloodWait(5)
        );
        assert_eq!(classify(420, "SLOWMODE_WAIT_9"), ErrorClass::SlowMode(9));
        assert_eq!(classify(420, "FROZEN_METHOD_INVALID"), ErrorClass::Frozen);
        assert_eq!(classify(303, "FILE_MIGRATE_4"), ErrorClass::Migrate(4));
        assert_eq!(classify(303, "NETWORK_MIGRATE_2"), ErrorClass::Migrate(2));
        assert_eq!(classify(400, "FILE_MIGRATE_4"), ErrorClass::Other);
        assert_eq!(classify(400, "PEER_ID_INVALID"), ErrorClass::Other);
    }

    #[test]
    fn rpc_error_bytes_round_trip_through_the_mtproto_schema() {
        let mut json = String::new();
        crate::schema::MTPROTO
            .to_json(&rpc_error_bytes(400, "PEER_ID_INVALID"), &mut json)
            .unwrap();
        assert_eq!(
            json,
            r#"{"_":"rpc_error","error_code":400,"error_message":"PEER_ID_INVALID"}"#
        );
    }
}
