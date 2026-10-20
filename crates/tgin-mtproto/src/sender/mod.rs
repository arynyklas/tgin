//! tgin's MTProto sender: one connection per DC, honest outcomes.
//!
//! [`Sender`] runs tgin's own message loop over a [`Transport`]. It
//! replaces
//! grammers' `mtp::Encrypted` with its own message layer: msg_id and
//! seqno, salts, acks, containers, `bad_server_salt` and
//! `bad_msg_notification` are handled here, at the message level, and no
//! server input can panic the loop.
//!
//! # Outcomes
//!
//! [`Sender::invoke`] ends in exactly one of:
//! - `Ok(`[`RpcOutcome`]`)`: Telegram's `rpc_result` bytes, or its
//!   `rpc_error` with the full message.
//! - [`SenderError::NotSent`]: the request was never written. No whole
//!   frame holding it reached the connection. One resend is allowed; the
//!   sender sends it on a fresh connection.
//! - [`SenderError::NoAnswer`]: the request was written and the connection
//!   ended before an answer. It may have been executed. Never resend it.
//! - [`SenderError::KeyRevoked`] / [`SenderError::KeyUnknown`]: transport
//!   −404, Telegram does not know the auth key. A server that
//!   cannot find the key cannot decrypt, and so cannot execute, any request
//!   on that connection: the request was not executed.
//! - [`SenderError::BadMessage`]: Telegram ignored the request
//!   (`bad_msg_notification`), after at most one resend for the codes that
//!   allow one (16, 17, 48).
//!
//! # Connections
//!
//! One connection at a time, each a new MTProto session (fresh
//! `session_id`, seqno from 0). When one ends, the sender stays without a
//! connection until the next `invoke`, which opens a fresh one. Pushes
//! therefore stop until then. So on a main session, whenever a connection
//! that completed its init call ends (except on −404 and when the sender
//! is dropped) the sender pushes [`CONNECTION_ENDED`], and whenever
//! Telegram reports `new_session_created` a synthetic `updatesTooLong`:
//! the update manager then calls `getDifference`, which reconnects and
//! re-subscribes the session. The two differ because only the second
//! shows the server is reachable (it may cut a backed-off retry short).
//! Those are the only signals: a connection that fails before its init
//! call completes pushes nothing, so a caller whose catch-up call fails
//! must retry it itself.
//!
//! Every connection opens with the sender's own init call,
//! `invokeWithLayer(layer, initConnection(…, help.getNearestDc))`; no
//! caller's request is ever wrapped. Requests wait until Telegram answers
//! it. If no answer comes within the init timeout the connection is
//! dropped and the waiting requests get [`SenderError::NotSent`]; if
//! Telegram refuses it they get [`SenderError::InitRefused`].
//!
//! Failures before init do not turn into a reconnect loop:
//! - a 4xx refusal of the init call latches: every later request gets
//!   [`SenderError::InitRefused`] without connecting, until the sender is
//!   rebuilt; a 420 `FLOOD_WAIT_X` latches for X seconds;
//! - every other failure before init (a transport −429, an init timeout, a
//!   5xx answer to the init call, a socket closed early, a failed connect)
//!   delays the next connection by 1 s, doubling to 60 s; a completed init
//!   resets the delay.
//!
//! # Pushes
//!
//! Every message that is neither a service message nor an answer is an
//! update container: its body goes to the [`Pushes`] receiver that
//! [`Sender::connect`] returns, as received (`gzip_packed` unwrapped). The
//! channel is bounded; while it is full the loop reads no further frames,
//! RPC answers included, but still writes the acks it owes and its pings.
//! Once the receiver is dropped, pushes are discarded. A request's result
//! whose top constructor is an `Updates` one goes to its caller and also,
//! in wire order with the pushes, to the receiver, so the update state sees it;
//! `messages.affected*` results reach only the caller.
//!
//! # What the sender never does
//!
//! No flood sleep, no retry of an answered or possibly executed request,
//! no re-key: a −404 never makes a new key.

use std::collections::VecDeque;
use std::fmt;
use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use grammers_tl_types::{self as tl, Identifiable as _, Serializable};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};

use crate::retry::Backoff;
use crate::transport::{PacketReader, PacketWriter, TcpTransport, Transport, TransportError};
use crate::updates::CONNECTION_ENDED;
use crate::{AuthKey, DcOption};

mod connection;
mod init;
mod message;

#[cfg(test)]
use crate::time::now_unix;
pub use init::InitParams;
use message::Clock;

#[cfg(test)]
mod tests;

/// Telegram's limits for one container (grammers-mtproto `manual_tl`).
const MAX_CONTAINER_MESSAGES: usize = 100;
const MAX_CONTAINER_PAYLOAD: usize = 1_044_456 - 8;
/// `msg_id:long seqno:int bytes:int`.
const MESSAGE_HEADER: usize = 8 + 4 + 4;
/// `salt:long session_id:long`.
const PACKET_HEADER: usize = 8 + 8;
/// A container's message header, constructor and count.
const CONTAINER_HEADER: usize = MESSAGE_HEADER + 4 + 4;
const MAX_ACKS_PER_MESSAGE: usize = 8192;
/// Recent packets whose acks come back if Telegram ignores the packet.
const SENT_ACKS_KEPT: usize = 8;
const NO_PING_DISCONNECT: i32 = 75;
/// Defence against a corrupt `gzip_packed`.
const MAX_UNPACKED: u64 = 64 * 1024 * 1024;
const COMMANDS: usize = 256;

const RPC_RESULT: u32 = 0xf35c_6d01;
const MSG_CONTAINER: u32 = 0x73f1_f8dc;
const GZIP_PACKED: u32 = 0x3072_cfa1;
const MSG_COPY: u32 = 0xe060_46b2;
const VECTOR: u32 = 0x1cb5_c415;
const RPC_ERROR: u32 = tl::types::RpcError::CONSTRUCTOR_ID;
const MSGS_ACK: u32 = tl::types::MsgsAck::CONSTRUCTOR_ID;
const BAD_MSG_NOTIFICATION: u32 = tl::types::BadMsgNotification::CONSTRUCTOR_ID;
const BAD_SERVER_SALT: u32 = tl::types::BadServerSalt::CONSTRUCTOR_ID;
const NEW_SESSION_CREATED: u32 = tl::types::NewSessionCreated::CONSTRUCTOR_ID;
/// `updatesTooLong#e317af7e`: what the sender pushes when the session's
/// update stream may have a gap.
const UPDATES_TOO_LONG: [u8; 4] = tl::types::UpdatesTooLong::CONSTRUCTOR_ID.to_le_bytes();
const PONG: u32 = tl::types::Pong::CONSTRUCTOR_ID;
const MSG_DETAILED_INFO: u32 = tl::types::MsgDetailedInfo::CONSTRUCTOR_ID;
const MSG_NEW_DETAILED_INFO: u32 = tl::types::MsgNewDetailedInfo::CONSTRUCTOR_ID;
const FUTURE_SALTS: u32 = tl::types::FutureSalts::CONSTRUCTOR_ID;
const MSGS_STATE_REQ: u32 = tl::types::MsgsStateReq::CONSTRUCTOR_ID;
const MSGS_STATE_INFO: u32 = tl::types::MsgsStateInfo::CONSTRUCTOR_ID;
const MSGS_ALL_INFO: u32 = tl::types::MsgsAllInfo::CONSTRUCTOR_ID;
const MSG_RESEND_REQ: u32 = tl::types::MsgResendReq::CONSTRUCTOR_ID;
const MSG_RESEND_ANS_REQ: u32 = tl::types::MsgResendAnsReq::CONSTRUCTOR_ID;
const DESTROY_SESSION_OK: u32 = tl::types::DestroySessionOk::CONSTRUCTOR_ID;
const DESTROY_SESSION_NONE: u32 = tl::types::DestroySessionNone::CONSTRUCTOR_ID;
const HTTP_WAIT: u32 = tl::types::HttpWait::CONSTRUCTOR_ID;
/// The `Updates` constructors. A request's result with one of these on top
/// changes the account's update state, so it is also pushed.
const UPDATES: [u32; 7] = [
    tl::types::UpdatesTooLong::CONSTRUCTOR_ID,
    tl::types::UpdateShortMessage::CONSTRUCTOR_ID,
    tl::types::UpdateShortChatMessage::CONSTRUCTOR_ID,
    tl::types::UpdateShort::CONSTRUCTOR_ID,
    tl::types::UpdatesCombined::CONSTRUCTOR_ID,
    tl::types::Updates::CONSTRUCTOR_ID,
    tl::types::UpdateShortSentMessage::CONSTRUCTOR_ID,
];

/// Telegram's answer to a request.
#[derive(Clone, PartialEq, Eq)]
pub enum RpcOutcome {
    /// The `rpc_result`'s `result`, as Telegram sent it (`gzip_packed`
    /// unwrapped).
    Result(Bytes),
    /// The `rpc_error`, with the message as Telegram wrote it
    /// (`FLOOD_WAIT_30`, not `FLOOD_WAIT`).
    Error { code: i32, message: String },
}

/// Leaves out the result's bytes: they are account content.
impl fmt::Debug for RpcOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Result(bytes) => write!(f, "Result(<{} bytes>)", bytes.len()),
            Self::Error { code, message } => write!(f, "Error({code} {message})"),
        }
    }
}

/// Why a request has no [`RpcOutcome`].
#[derive(Debug, thiserror::Error)]
pub enum SenderError {
    /// Never written: one resend is allowed, and goes on a fresh connection.
    #[error("the request was not sent")]
    NotSent,
    /// Written, and no answer came: it may have been executed. Never resent.
    #[error("the request was sent but no answer came; it may have been executed")]
    NoAnswer,
    /// Transport −404 on a logged-in key: the authorization is gone. The
    /// sender makes no new key; the account needs a new login. The request
    /// was not executed: without the key Telegram cannot decrypt it.
    #[error("Telegram no longer knows the account's auth key (transport -404)")]
    KeyRevoked,
    /// Transport −404 on a key without a self user. Not executed, as for
    /// [`SenderError::KeyRevoked`].
    #[error("Telegram does not know this auth key (transport -404)")]
    KeyUnknown,
    /// Telegram ignored the request (`bad_msg_notification`); it was not
    /// executed.
    #[error("Telegram ignored the request (bad_msg_notification {code})")]
    BadMessage { code: i32 },
    /// The request is not whole TL words, or too large for one message.
    #[error("the request is not whole TL words or is too large")]
    BadRequest,
    /// Telegram answered the connection's `initConnection` call with this
    /// `rpc_error`. The request was not sent.
    #[error("Telegram refused initConnection: {code} {message}")]
    InitRefused { code: i32, message: String },
    /// [`Sender::connect`] could not open the first connection.
    #[error("connect: {0}")]
    Connect(#[from] TransportError),
}

/// Which −404 error a dead key gives.
#[derive(Clone, Copy, Debug)]
enum KeyGone {
    Revoked,
    Unknown,
}

impl KeyGone {
    fn error(self) -> SenderError {
        match self {
            Self::Revoked => SenderError::KeyRevoked,
            Self::Unknown => SenderError::KeyUnknown,
        }
    }
}

type Reply = oneshot::Sender<Result<RpcOutcome, SenderError>>;

struct Command {
    body: Bytes,
    reply: Reply,
}

/// The loop's timing and bounds; tests shrink them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tuning {
    /// Update containers the pushes channel holds.
    pub(crate) pushes: usize,
    pub(crate) ping_interval: Duration,
    /// A ping unanswered this long ends the connection.
    pub(crate) pong_timeout: Duration,
    /// The init call unanswered this long ends the connection.
    pub(crate) init_timeout: Duration,
}

impl Tuning {
    pub(crate) const PRODUCTION: Self = Self {
        pushes: 1024,
        ping_interval: Duration::from_secs(60),
        pong_timeout: Duration::from_secs(30),
        init_timeout: Duration::from_secs(10),
    };
}

/// One DC's sender. Share it behind an `Arc`; `invoke` takes `&self`.
#[derive(Debug)]
pub struct Sender {
    dc_id: i32,
    max_body: usize,
    commands: mpsc::Sender<Command>,
    logged_in: Arc<AtomicBool>,
}

/// The update containers a main session's connection pushes, in arrival
/// order; a media session's never yields one.
///
/// **Contract.** Drain it on a task that never awaits [`Sender::invoke`] on
/// the same sender. While this channel is full the sender reads nothing,
/// so an `invoke` awaited by the draining task waits for ever. Drop it to
/// have pushes discarded instead.
#[derive(Debug)]
pub struct Pushes(mpsc::Receiver<Bytes>);

impl Pushes {
    /// The next push; `None` once the sender is gone (or on a media
    /// session).
    pub async fn recv(&mut self) -> Option<Bytes> {
        self.0.recv().await
    }

    /// The next push if one is waiting.
    pub fn try_recv(&mut self) -> Result<Bytes, mpsc::error::TryRecvError> {
        self.0.try_recv()
    }

    /// Pushes from a test's own channel.
    #[cfg(any(test, feature = "testkit"))]
    pub(crate) fn from_receiver(rx: mpsc::Receiver<Bytes>) -> Self {
        Self(rx)
    }
}

/// What [`Sender::connect`] returns: the sender, its pushes, and the auth
/// key it made when it was given none (for the caller to store).
pub type Connected = (Sender, Pushes, Option<AuthKey>);

impl Sender {
    /// Opens the first connection to `dc` over TCP. With `key: None` it
    /// first makes a new auth key and returns it, for the caller to store.
    pub async fn connect(
        dc: DcOption,
        key: Option<AuthKey>,
        init: InitParams,
    ) -> Result<Connected, SenderError> {
        Self::connect_with(TcpTransport::new(), dc, key, init).await
    }

    /// [`connect`](Self::connect) over another [`Transport`].
    pub async fn connect_with<T: Transport>(
        transport: T,
        dc: DcOption,
        key: Option<AuthKey>,
        init: InitParams,
    ) -> Result<Connected, SenderError> {
        Self::start(transport, dc, key, init, Tuning::PRODUCTION).await
    }

    pub(crate) async fn start<T: Transport>(
        transport: T,
        dc: DcOption,
        key: Option<AuthKey>,
        init: InitParams,
        tuning: Tuning,
    ) -> Result<Connected, SenderError> {
        let opened = transport.open(&dc, key.as_ref()).await?;
        let (key, salt, time_offset, new_key) = match (opened.generated, key) {
            (Some(g), _) => (g.key.clone(), g.first_salt, g.time_offset, Some(g.key)),
            (None, Some(key)) => (key, 0, 0, None),
            (None, None) => {
                return Err(SenderError::Connect(TransportError::Handshake(
                    "the transport made no key".to_owned(),
                )));
            }
        };
        let init_call = Bytes::from(init.init_call());
        let max_body = MAX_REQUEST;
        let (commands, commands_rx) = mpsc::channel(COMMANDS);
        let (pushes_tx, pushes_rx) = mpsc::channel(tuning.pushes.max(1));
        // A media session's pushes go nowhere: its caller gets a closed
        // channel, and the loop's receiver is dropped.
        let pushes = if init.without_updates {
            mpsc::channel(1).1
        } else {
            pushes_rx
        };
        let logged_in = Arc::new(AtomicBool::new(init.logged_in));
        let actor = Actor {
            transport,
            dc,
            key,
            commands: commands_rx,
            pushes: pushes_tx,
            st: Shared {
                dc_id: dc.id,
                init_call,
                without_updates: init.without_updates,
                logged_in: Arc::clone(&logged_in),
                tuning,
                clock: Clock {
                    offset: time_offset,
                    last: 0,
                },
                salt,
                backlog: VecDeque::new(),
                dead: None,
                refused: None,
                backoff: Backoff::default(),
                retry_at: None,
            },
        };
        tokio::spawn(actor.run(opened.reader, opened.writer));
        Ok((
            Self {
                dc_id: dc.id,
                max_body,
                commands,
                logged_in,
            },
            Pushes(pushes),
            new_key,
        ))
    }

    /// The DC this sender talks to.
    #[must_use]
    pub fn dc_id(&self) -> i32 {
        self.dc_id
    }

    /// The key now has a self user (a login completed on this sender):
    /// from here on a transport −404 is [`SenderError::KeyRevoked`].
    pub fn mark_logged_in(&self) {
        self.logged_in.store(true, Ordering::Relaxed);
    }

    /// Sends one request (a serialized TL function) and waits for its
    /// outcome. See the module docs for what each outcome promises.
    pub async fn invoke(&self, body: Bytes) -> Result<RpcOutcome, SenderError> {
        if body.len() < 4 || !body.len().is_multiple_of(4) || body.len() > self.max_body {
            return Err(SenderError::BadRequest);
        }
        let (reply, answer) = oneshot::channel();
        if self.commands.send(Command { body, reply }).await.is_err() {
            return Err(SenderError::NotSent);
        }
        // The loop is gone without answering: the request may have been
        // written.
        answer.await.unwrap_or(Err(SenderError::NoAnswer))
    }
}

/// The largest request [`Sender::invoke`] takes: one message's payload,
/// less the `invokeWithoutUpdates` prefix a media session adds.
pub const MAX_REQUEST: usize = MAX_CONTAINER_PAYLOAD - MESSAGE_HEADER - WITHOUT_UPDATES.len();

/// `invokeWithoutUpdates#bfa9e4b7`, the prefix of every media request.
const WITHOUT_UPDATES: [u8; 4] =
    tl::functions::InvokeWithoutUpdates::<()>::CONSTRUCTOR_ID.to_le_bytes();

/// State that outlives a connection.
struct Shared {
    dc_id: i32,
    /// The connection's first call; see [`InitParams::init_call`].
    init_call: Bytes,
    without_updates: bool,
    logged_in: Arc<AtomicBool>,
    tuning: Tuning,
    clock: Clock,
    /// Per key, not per session, so a fresh connection keeps it.
    salt: i64,
    /// Received pushes the channel has not taken yet.
    backlog: VecDeque<Bytes>,
    dead: Option<KeyGone>,
    /// Telegram refused the init call with a 4xx: `(code, message, until)`;
    /// `until: None` holds until the sender is rebuilt.
    refused: Option<(i32, String, Option<Instant>)>,
    /// The delay after the last transport −429, and when the next
    /// connection may open.
    backoff: Backoff,
    retry_at: Option<Instant>,
}

impl Shared {
    /// The session's update stream may have a gap (`new_session_created`,
    /// on a live connection): tell the update manager, on main sessions.
    fn session_lost(&mut self) {
        self.push(Bytes::from_static(&UPDATES_TOO_LONG));
    }

    /// A connection that finished its init call ended: its pushes may be
    /// lost. Tell the update manager, on main sessions.
    fn connection_ended(&mut self) {
        self.push(Bytes::from_static(CONNECTION_ENDED));
    }

    /// One update container for the update manager, on main sessions.
    fn push(&mut self, updates: Bytes) {
        if !self.without_updates {
            self.backlog.push_back(updates);
        }
    }

    /// What goes in front of a caller's request.
    fn wrapper_len(&self) -> usize {
        if self.without_updates {
            WITHOUT_UPDATES.len()
        } else {
            0
        }
    }
}

/// Why a connection ended.
#[derive(Debug)]
enum Cause {
    Transport(TransportError),
    Protocol(&'static str),
    PongTimeout,
    InitTimeout,
    InitRefused { code: i32, message: String },
    Shutdown,
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "{e}"),
            Self::Protocol(what) => write!(f, "protocol: {what}"),
            Self::PongTimeout => f.write_str("no pong"),
            Self::InitTimeout => f.write_str("no answer to initConnection"),
            Self::InitRefused { code, message } => {
                write!(f, "initConnection refused: {code} {message}")
            }
            Self::Shutdown => f.write_str("sender dropped"),
        }
    }
}

struct Actor<T: Transport> {
    transport: T,
    dc: DcOption,
    key: AuthKey,
    commands: mpsc::Receiver<Command>,
    pushes: mpsc::Sender<Bytes>,
    st: Shared,
}

/// Where a request is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Queued,
    /// In the writer's packet, not yet wholly handed to the connection.
    InFlight {
        msg_id: i64,
        container: i64,
    },
    /// Wholly handed to the connection.
    Written {
        msg_id: i64,
        container: i64,
    },
}

impl Slot {
    fn msg_id(self) -> Option<i64> {
        match self {
            Self::Queued => None,
            Self::InFlight { msg_id, .. } | Self::Written { msg_id, .. } => Some(msg_id),
        }
    }

    /// The container the message went out in (itself when alone).
    fn container(self) -> Option<i64> {
        match self {
            Self::Queued => None,
            Self::InFlight { container, .. } | Self::Written { container, .. } => Some(container),
        }
    }

    /// `id` names this message or its container.
    fn matches(self, id: i64) -> bool {
        match self {
            Self::Queued => false,
            Self::InFlight { msg_id, container } | Self::Written { msg_id, container } => {
                msg_id == id || container == id
            }
        }
    }
}

enum What {
    Request {
        body: Bytes,
        reply: Reply,
    },
    Ping {
        body: Bytes,
    },
    /// The connection's own init call.
    Init,
}

struct Pending {
    what: What,
    slot: Slot,
    /// Already resent once after a retryable `bad_msg_notification`.
    resent: bool,
}

/// The init call on this connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Init {
    /// The next packet carries a new init call.
    Needed,
    /// Sent; requests wait for its answer until `deadline`.
    Pending {
        deadline: Instant,
    },
    Done,
}

/// One connection's MTProto session.
struct Conn {
    session_id: i64,
    /// grammers' `sequence`: twice the content-related messages sent.
    seq: i32,
    init: Init,
    /// Received content-related msg_ids not yet acknowledged.
    acks: Vec<i64>,
    /// `(ack msg_id, container msg_id, ids)` of recent packets.
    sent_acks: VecDeque<(i64, i64, Vec<i64>)>,
    pending: Vec<Pending>,
    packet: Vec<u8>,
    picks: Vec<usize>,
    next_ping: Instant,
    pong_deadline: Option<Instant>,
}
