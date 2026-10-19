//! The account side of the raw-TL RPC endpoint (design §10.6, plan
//! decisions 3 and 4).
//!
//! - [`unwrap_request`] reads a downstream request against the api schema:
//!   `initConnection` and `invokeWithLayer(account layer, …)` are stripped;
//!   any other wrapper (a function with a `query:!X` field), a foreign
//!   layer, and the update-state methods are refused. What is left is one
//!   whole, strictly checked function call.
//! - [`call`] runs that call on an [`AccountRpc`] with the answer rules:
//!   one resend of a request never written, never a resend of one that may
//!   have run, one flood sleep up to the threshold, `*_MIGRATE_X` to a
//!   media session, and the pts of a `messages.affected*` answer handed to
//!   the update manager through an [`AffectedSink`].
//!
//! Nothing here logs or keeps request or answer bytes.

use std::fmt;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;

use crate::decode::BoxKind;
use crate::schema::{API, Ctor, Kind, MAX_FLAG_WORDS, Reader, Role, Ty, present};
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
    /// The media session to `dc` could not be set up (export or import of
    /// the authorization refused, or the session store failed). The
    /// request was not sent.
    #[error("media session to DC {dc}: {what}")]
    Media { dc: i32, what: &'static str },
}

/// A running account, as the RPC endpoint sees it. Task 7's account
/// supervisor implements it; [`crate::media::AccountSessions`] is the
/// session-routing part.
pub trait AccountRpc: Send + Sync {
    /// The one layer the account serves (plan decision 1).
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
    /// The update box an affected-pts answer counts in: the channel for a
    /// `channels.*` call with a `channel` field, else the common box.
    pub context: BoxKind,
}

/// Leaves out the body: it is account content.
impl fmt::Debug for Call<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Call")
            .field("method", &self.method)
            .field("body", &format_args!("<{} bytes>", self.body.len()))
            .field("context", &self.context)
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
}

const UPDATES_METHODS: [&str; 3] = [
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
            name if is_wrapper(ctor) => return Err(Refused::Wrapper(name)),
            name if UPDATES_METHODS.contains(&name) => return Err(Refused::UpdatesMethod(name)),
            method => {
                let context = check_call(&mut r, ctor)?;
                r.finish()?;
                return Ok(Call {
                    method,
                    body: rest,
                    context,
                });
            }
        }
    }
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

/// Checks a call's fields strictly; returns its update context.
fn check_call(r: &mut Reader<'_>, ctor: &'static Ctor) -> Result<BoxKind, TlError> {
    let channels = ctor.name.starts_with("channels.");
    let mut context = BoxKind::Common;
    let mut words = [0u32; MAX_FLAG_WORDS];
    for p in ctor.params {
        match &p.kind {
            Kind::Flags { slot, known } => words[usize::from(*slot)] = r.flags(ctor, p, *known)?,
            Kind::Field { ty, flag } => {
                if !present(&words, *flag) {
                    continue;
                }
                let start = r.pos;
                API.skip_value(r, ty, 1)?;
                if channels && p.name == "channel" {
                    context =
                        channel_id(&r.buf[start..r.pos]).map_or(BoxKind::None, BoxKind::Channel);
                }
            }
        }
    }
    if channels && context == BoxKind::Common {
        context = BoxKind::None;
    }
    Ok(context)
}

/// The `channel_id` of an `InputChannel` (already checked).
fn channel_id(bytes: &[u8]) -> Option<i64> {
    let mut r = Reader::new(bytes);
    let ctor = API.read_ctor(&mut r, None).ok()?;
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
                if p.role == Role::ChannelId {
                    return r.i64().ok();
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
    let wait = |prefix: &str| {
        message
            .strip_prefix(prefix)
            .and_then(|x| x.parse::<u32>().ok())
    };
    if let Some(x) = wait("FLOOD_WAIT_").or_else(|| wait("FLOOD_PREMIUM_WAIT_")) {
        return ErrorClass::FloodWait(x);
    }
    if let Some(x) = wait("SLOWMODE_WAIT_") {
        return ErrorClass::SlowMode(x);
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

/// Runs one unwrapped call (plan decision 4):
/// - [`SenderError::NotSent`]: sent once more; the sender opens a fresh
///   connection for it;
/// - [`SenderError::NoAnswer`] and every other failure: returned, never
///   resent;
/// - `FLOOD_WAIT_X` with X ≤ `flood_sleep_threshold`: slept once, then sent
///   again on the same route;
/// - `*_MIGRATE_X` from the main session: sent once to the media session
///   of DC X;
/// - a `messages.affected*` answer: its pts go to `sink` in `context`.
pub async fn call(
    account: &dyn AccountRpc,
    sink: &dyn AffectedSink,
    context: BoxKind,
    body: Bytes,
    flood_sleep_threshold: Duration,
) -> Answer {
    let mut route = Route::Main;
    let mut resent = false;
    let mut slept = false;
    loop {
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
                        tokio::time::sleep(Duration::from_secs(u64::from(x))).await;
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
    fn debug_prints_lengths_not_bytes() {
        let body = [0xab_u8; 12];
        let call = Call {
            method: "help.getConfig",
            body: &body,
            context: BoxKind::Common,
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
