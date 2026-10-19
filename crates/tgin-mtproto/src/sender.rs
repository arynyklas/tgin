//! tgin's MTProto sender: one connection per DC, honest outcomes.
//!
//! [`Sender`] runs tgin's own message loop over a [`Transport`]. It started
//! from the spike's `raw_sender.rs` (spike/m3 `74ad0e5`) and replaces
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
//!   sender sends it on a fresh connection [C: S11].
//! - [`SenderError::NoAnswer`]: the request was written and the connection
//!   ended before an answer. It may have been executed. Never resend it.
//! - [`SenderError::KeyRevoked`] / [`SenderError::KeyUnknown`]: transport
//!   −404, Telegram does not know the auth key [C: S4 F8].
//! - [`SenderError::BadMessage`]: Telegram ignored the request
//!   (`bad_msg_notification`), after at most one resend for the codes that
//!   allow one (16, 17, 48).
//!
//! # Connections
//!
//! One connection at a time, each a new MTProto session (fresh
//! `session_id`, seqno from 0). When one ends, the sender stays without a
//! connection until the next `invoke`, which opens a fresh one. Pushes
//! therefore stop until then; after a reconnect the caller re-subscribes
//! the session with its next update call. The first request on each
//! connection is wrapped in `invokeWithLayer(layer, initConnection(…))`,
//! and later requests wait until Telegram has answered it.
//!
//! # Pushes
//!
//! Every message that is neither a service message nor an answer is an
//! update container: its body goes to the [`Pushes`] receiver that
//! [`Sender::connect`] returns, as received (`gzip_packed` unwrapped). The
//! channel is bounded; while it is full the loop reads no further frames,
//! RPC answers included, but still writes the acks it owes and its pings.
//! Once the receiver is dropped, pushes are discarded. Updates that come
//! back as a request's `rpc_result` go to that request's caller only.
//!
//! # What the sender never does
//!
//! No flood sleep, no retry of an answered or possibly executed request,
//! no re-key: a −404 never makes a new key.

use std::collections::VecDeque;
use std::fmt;
use std::io::Read as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use grammers_tl_types::{self as tl, Identifiable as _, Serializable};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};

use crate::transport::{PacketReader, PacketWriter, TcpTransport, Transport, TransportError};
use crate::{AuthKey, DcOption};

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

/// What `initConnection` tells Telegram, and how the connection is used.
#[derive(Clone)]
pub struct InitParams {
    pub api_id: i32,
    pub device_model: String,
    pub system_version: String,
    pub app_version: String,
    pub system_lang_code: String,
    pub lang_pack: String,
    pub lang_code: String,
    /// A media session: every request is wrapped in `invokeWithoutUpdates`,
    /// and pushes are dropped ([`Sender::pushes`] gives `None`).
    pub without_updates: bool,
    /// The key has a self user: a transport −404 is
    /// [`SenderError::KeyRevoked`], else [`SenderError::KeyUnknown`].
    pub logged_in: bool,
    layer: i32,
}

impl InitParams {
    /// tgin's defaults, in grammers' compiled layer. An account serves
    /// exactly that layer (plan decision 1).
    #[must_use]
    pub fn new(api_id: i32) -> Self {
        Self {
            api_id,
            device_model: "tgin".to_owned(),
            system_version: std::env::consts::OS.to_owned(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            system_lang_code: "en".to_owned(),
            lang_pack: String::new(),
            lang_code: "en".to_owned(),
            without_updates: false,
            logged_in: false,
            layer: tl::LAYER,
        }
    }

    /// The layer `invokeWithLayer` names: `grammers_tl_types::LAYER`.
    #[must_use]
    pub fn layer(&self) -> i32 {
        self.layer
    }

    /// `invokeWithLayer(layer, initConnection(…, ` up to the query, which
    /// is `initConnection`'s last field.
    fn init_prefix(&self) -> Vec<u8> {
        struct Nothing;
        impl Serializable for Nothing {
            fn serialize(&self, _: &mut impl Extend<u8>) {}
        }
        tl::functions::InvokeWithLayer {
            layer: self.layer,
            query: tl::functions::InitConnection {
                api_id: self.api_id,
                device_model: self.device_model.clone(),
                system_version: self.system_version.clone(),
                app_version: self.app_version.clone(),
                system_lang_code: self.system_lang_code.clone(),
                lang_pack: self.lang_pack.clone(),
                lang_code: self.lang_code.clone(),
                proxy: None,
                params: None,
                query: Nothing,
            },
        }
        .to_bytes()
    }
}

/// `api_id` is left out: it identifies tgin's Telegram application.
impl fmt::Debug for InitParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InitParams")
            .field("device_model", &self.device_model)
            .field("app_version", &self.app_version)
            .field("layer", &self.layer)
            .field("without_updates", &self.without_updates)
            .field("logged_in", &self.logged_in)
            .finish_non_exhaustive()
    }
}

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
    /// sender makes no new key; the account needs a new login.
    #[error("Telegram no longer knows the account's auth key (transport -404)")]
    KeyRevoked,
    /// Transport −404 on a key without a self user.
    #[error("Telegram does not know this auth key (transport -404)")]
    KeyUnknown,
    /// Telegram ignored the request (`bad_msg_notification`); it was not
    /// executed.
    #[error("Telegram ignored the request (bad_msg_notification {code})")]
    BadMessage { code: i32 },
    /// The request is not whole TL words, or too large for one message.
    #[error("the request is not whole TL words or is too large")]
    BadRequest,
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
}

impl Tuning {
    pub(crate) const PRODUCTION: Self = Self {
        pushes: 1024,
        ping_interval: Duration::from_secs(60),
        pong_timeout: Duration::from_secs(30),
    };
}

/// One DC's sender. Share it behind an `Arc`; `invoke` takes `&self`.
#[derive(Debug)]
pub struct Sender {
    dc_id: i32,
    max_body: usize,
    commands: mpsc::Sender<Command>,
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
        Self::connect_with(TcpTransport, dc, key, init).await
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
        let init_prefix = init.init_prefix();
        let max_body =
            MAX_CONTAINER_PAYLOAD - MESSAGE_HEADER - init_prefix.len() - WITHOUT_UPDATES.len();
        let (commands, commands_rx) = mpsc::channel(COMMANDS);
        let (pushes_tx, pushes_rx) = mpsc::channel(tuning.pushes.max(1));
        // A media session's pushes go nowhere: its caller gets a closed
        // channel, and the loop's receiver is dropped.
        let pushes = if init.without_updates {
            mpsc::channel(1).1
        } else {
            pushes_rx
        };
        let actor = Actor {
            transport,
            dc,
            key,
            commands: commands_rx,
            pushes: pushes_tx,
            st: Shared {
                dc_id: dc.id,
                init_prefix,
                without_updates: init.without_updates,
                logged_in: init.logged_in,
                tuning,
                clock: Clock {
                    offset: time_offset,
                    last: 0,
                },
                salt,
                backlog: VecDeque::new(),
                dead: None,
            },
        };
        tokio::spawn(actor.run(opened.reader, opened.writer));
        Ok((
            Self {
                dc_id: dc.id,
                max_body,
                commands,
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

/// `invokeWithoutUpdates#bfa9e4b7`, the prefix of every media request.
const WITHOUT_UPDATES: [u8; 4] =
    tl::functions::InvokeWithoutUpdates::<()>::CONSTRUCTOR_ID.to_le_bytes();

/// msg_ids from the local clock plus Telegram's offset, strictly rising.
struct Clock {
    offset: i32,
    last: i64,
}

fn now_unix() -> (i64, u32) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (
        i64::try_from(now.as_secs()).unwrap_or(0),
        now.subsec_nanos(),
    )
}

impl Clock {
    fn next(&mut self) -> i64 {
        let (secs, nanos) = now_unix();
        let mut id = ((secs + i64::from(self.offset)) << 32) | (i64::from(nanos) << 2);
        if id <= self.last {
            id = self.last + 4;
        }
        self.last = id;
        id
    }

    /// Takes Telegram's time from one of its msg_ids. A new offset also
    /// drops the floor: ids made with a wrong offset must not hold the next
    /// ones in the future (Telethon's `update_time_offset`).
    fn correct(&mut self, server_msg_id: i64) {
        let (secs, _) = now_unix();
        let offset = i32::try_from((server_msg_id >> 32) - secs).unwrap_or(self.offset);
        if offset != self.offset {
            self.offset = offset;
            self.last = 0;
        }
    }
}

/// State that outlives a connection.
struct Shared {
    dc_id: i32,
    init_prefix: Vec<u8>,
    without_updates: bool,
    logged_in: bool,
    tuning: Tuning,
    clock: Clock,
    /// Per key, not per session, so a fresh connection keeps it.
    salt: i64,
    /// Received pushes the channel has not taken yet.
    backlog: VecDeque<Bytes>,
    dead: Option<KeyGone>,
}

impl Shared {
    fn wrapper_len(&self, init: bool) -> usize {
        let mut len = 0;
        if init {
            len += self.init_prefix.len();
        }
        if self.without_updates {
            len += WITHOUT_UPDATES.len();
        }
        len
    }
}

/// Why a connection ended.
#[derive(Debug)]
enum Cause {
    Transport(TransportError),
    Protocol(&'static str),
    PongTimeout,
    Shutdown,
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "{e}"),
            Self::Protocol(what) => write!(f, "protocol: {what}"),
            Self::PongTimeout => f.write_str("no pong"),
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
    Request { body: Bytes, reply: Reply },
    Ping { body: Bytes },
}

struct Pending {
    id: u64,
    what: What,
    slot: Slot,
    /// Already resent once after a retryable `bad_msg_notification`.
    resent: bool,
}

/// `initConnection` on this connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Init {
    /// The next request is wrapped.
    Needed,
    /// This request carries it; other requests wait for its answer.
    Pending(u64),
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
    next_id: u64,
    packet: Vec<u8>,
    picks: Vec<usize>,
    next_ping: Instant,
    pong_deadline: Option<Instant>,
}

impl<T: Transport> Actor<T> {
    async fn run(mut self, reader: T::Reader, writer: T::Writer) {
        let mut conn = Some((reader, writer, None));
        loop {
            let (reader, writer, first) = match conn.take() {
                Some(c) => c,
                None => match self.idle().await {
                    Some(c) => c,
                    None => return,
                },
            };
            if self.serve(reader, writer, first).await {
                return;
            }
        }
    }

    /// Without a connection: hands out pushes, and opens a fresh connection
    /// for the next request. `None` once the [`Sender`] is dropped.
    async fn idle(&mut self) -> Option<(T::Reader, T::Writer, Option<Command>)> {
        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    let command = command?;
                    if let Some(gone) = self.st.dead {
                        let _ = command.reply.send(Err(gone.error()));
                        continue;
                    }
                    if command.reply.is_closed() {
                        continue;
                    }
                    match self.transport.open(&self.dc, Some(&self.key)).await {
                        Ok(opened) => return Some((opened.reader, opened.writer, Some(command))),
                        Err(e) => {
                            tracing::debug!(dc = self.st.dc_id, error = %e, "mtproto connect failed");
                            let _ = command.reply.send(Err(SenderError::NotSent));
                            while let Ok(queued) = self.commands.try_recv() {
                                let _ = queued.reply.send(Err(SenderError::NotSent));
                            }
                        }
                    }
                }
                permit = self.pushes.reserve(), if !self.st.backlog.is_empty() => {
                    hand_out(permit, &mut self.st.backlog);
                }
            }
        }
    }

    /// Runs one connection until it ends. `true` once the [`Sender`] is
    /// dropped.
    async fn serve(
        &mut self,
        mut reader: T::Reader,
        mut writer: T::Writer,
        first: Option<Command>,
    ) -> bool {
        let st = &mut self.st;
        let mut conn = Conn::new(st.tuning.ping_interval);
        if let Some(command) = first {
            conn.enqueue_request(command);
        }
        let cause = loop {
            if writer.is_flushed() && conn.build(st) {
                writer.queue(&conn.packet);
            }
            let reading = st.backlog.is_empty();
            let timer = conn
                .pong_deadline
                .map_or(conn.next_ping, |d| d.min(conn.next_ping));
            tokio::select! {
                packet = reader.recv(), if reading => match packet {
                    Ok(packet) => {
                        if let Err(cause) = conn.on_packet(st, packet) {
                            break cause;
                        }
                    }
                    Err(e) => break Cause::Transport(e),
                },
                flushed = writer.flush(), if !writer.is_flushed() => match flushed {
                    Ok(()) => conn.mark_written(),
                    Err(e) => break Cause::Transport(e),
                },
                permit = self.pushes.reserve(), if !reading => {
                    hand_out(permit, &mut st.backlog);
                }
                command = self.commands.recv() => match command {
                    Some(command) => conn.enqueue_request(command),
                    None => break Cause::Shutdown,
                },
                () = sleep_until(timer) => {
                    if let Err(cause) = conn.on_timer(st, Instant::now()) {
                        break cause;
                    }
                }
            }
        };
        let gone = match &cause {
            Cause::Transport(TransportError::Status(404)) => Some(if st.logged_in {
                KeyGone::Revoked
            } else {
                KeyGone::Unknown
            }),
            _ => None,
        };
        match gone {
            Some(_) => tracing::warn!(
                dc = st.dc_id,
                "mtproto transport -404: the auth key is unknown"
            ),
            None => tracing::debug!(dc = st.dc_id, cause = %cause, "mtproto connection ended"),
        }
        st.dead = st.dead.or(gone);
        conn.end(writer.is_flushed(), gone);
        matches!(cause, Cause::Shutdown)
    }
}

/// Moves one backlog entry into the pushes channel; drops the backlog when
/// nobody receives pushes.
fn hand_out(
    permit: Result<mpsc::Permit<'_, Bytes>, mpsc::error::SendError<()>>,
    backlog: &mut VecDeque<Bytes>,
) {
    match permit {
        Ok(permit) => {
            if let Some(push) = backlog.pop_front() {
                permit.send(push);
            }
        }
        Err(_) => backlog.clear(),
    }
}

impl Conn {
    fn new(ping_interval: Duration) -> Self {
        Self {
            session_id: rand::random(),
            seq: 0,
            init: Init::Needed,
            acks: Vec::new(),
            sent_acks: VecDeque::new(),
            pending: Vec::new(),
            next_id: 0,
            packet: Vec::new(),
            picks: Vec::new(),
            next_ping: Instant::now() + ping_interval,
            pong_deadline: None,
        }
    }

    fn push_pending(&mut self, what: What) {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.push(Pending {
            id,
            what,
            slot: Slot::Queued,
            resent: false,
        });
    }

    fn enqueue_request(&mut self, command: Command) {
        self.push_pending(What::Request {
            body: command.body,
            reply: command.reply,
        });
    }

    /// grammers `Encrypted::get_seq_no`.
    fn seq_no(&mut self, content_related: bool) -> i32 {
        if content_related {
            self.seq += 2;
            self.seq - 1
        } else {
            self.seq
        }
    }

    /// Builds the next packet into `self.packet`: owed acks, then queued
    /// pings and requests in order. `false` when there is nothing to send.
    fn build(&mut self, st: &mut Shared) -> bool {
        // A caller that gave up before its request was written never has
        // it written.
        self.pending.retain(|p| {
            !matches!((&p.slot, &p.what), (Slot::Queued, What::Request { reply, .. }) if reply.is_closed())
        });
        let ack_count = self.acks.len().min(MAX_ACKS_PER_MESSAGE);
        let ack_len = 4 + 4 + 4 + 8 * ack_count;
        let mut count = usize::from(ack_count > 0);
        let mut size = if ack_count > 0 {
            MESSAGE_HEADER + ack_len
        } else {
            0
        };
        let mut init = self.init;
        let mut init_pick = None;
        self.picks.clear();
        for (i, p) in self.pending.iter().enumerate() {
            if p.slot != Slot::Queued {
                continue;
            }
            let len = match &p.what {
                What::Ping { body } => body.len(),
                What::Request { body, .. } => match init {
                    Init::Pending(_) => continue,
                    Init::Needed => st.wrapper_len(true) + body.len(),
                    Init::Done => st.wrapper_len(false) + body.len(),
                },
            };
            if count == MAX_CONTAINER_MESSAGES
                || size + MESSAGE_HEADER + len > MAX_CONTAINER_PAYLOAD
            {
                break;
            }
            if matches!(p.what, What::Request { .. }) && init == Init::Needed {
                init = Init::Pending(p.id);
                init_pick = Some(i);
            }
            count += 1;
            size += MESSAGE_HEADER + len;
            self.picks.push(i);
        }
        if count == 0 {
            return false;
        }
        self.init = init;

        let mut packet = std::mem::take(&mut self.packet);
        packet.clear();
        packet.extend_from_slice(&st.salt.to_le_bytes());
        packet.extend_from_slice(&self.session_id.to_le_bytes());
        let container_at = packet.len();
        if count > 1 {
            packet.extend_from_slice(&[0; CONTAINER_HEADER]);
        }
        let mut acked = None;
        if ack_count > 0 {
            let msg_id = st.clock.next();
            let seq = self.seq_no(false);
            let ids: Vec<i64> = self.acks.drain(..ack_count).collect();
            write_header(&mut packet, msg_id, seq, ack_len);
            packet.extend_from_slice(&MSGS_ACK.to_le_bytes());
            packet.extend_from_slice(&VECTOR.to_le_bytes());
            packet.extend_from_slice(&len_i32(ack_count).to_le_bytes());
            for id in &ids {
                packet.extend_from_slice(&id.to_le_bytes());
            }
            acked = Some((msg_id, ids));
        }
        let picks = std::mem::take(&mut self.picks);
        for &i in &picks {
            let msg_id = st.clock.next();
            let seq = self.seq_no(true);
            let p = &mut self.pending[i];
            match &p.what {
                What::Ping { body } => {
                    write_header(&mut packet, msg_id, seq, body.len());
                    packet.extend_from_slice(body);
                }
                What::Request { body, .. } => {
                    let init = init_pick == Some(i);
                    write_header(&mut packet, msg_id, seq, st.wrapper_len(init) + body.len());
                    if init {
                        packet.extend_from_slice(&st.init_prefix);
                    }
                    if st.without_updates {
                        packet.extend_from_slice(&WITHOUT_UPDATES);
                    }
                    packet.extend_from_slice(body);
                }
            }
            p.slot = Slot::InFlight {
                msg_id,
                container: msg_id,
            };
        }
        let mut container = acked.as_ref().map_or(0, |(id, _)| *id);
        if count > 1 {
            container = st.clock.next();
            let seq = self.seq_no(false);
            let inner = packet.len() - container_at - MESSAGE_HEADER;
            let header = &mut packet[container_at..container_at + CONTAINER_HEADER];
            header[..8].copy_from_slice(&container.to_le_bytes());
            header[8..12].copy_from_slice(&seq.to_le_bytes());
            header[12..16].copy_from_slice(&len_i32(inner).to_le_bytes());
            header[16..20].copy_from_slice(&MSG_CONTAINER.to_le_bytes());
            header[20..].copy_from_slice(&len_i32(count).to_le_bytes());
            for &i in &picks {
                if let Slot::InFlight { msg_id, .. } = self.pending[i].slot {
                    self.pending[i].slot = Slot::InFlight { msg_id, container };
                }
            }
        }
        if let Some((msg_id, ids)) = acked {
            if self.sent_acks.len() == SENT_ACKS_KEPT {
                self.sent_acks.pop_front();
            }
            self.sent_acks.push_back((msg_id, container, ids));
        }
        self.packet = packet;
        self.picks = picks;
        true
    }

    fn mark_written(&mut self) {
        for p in &mut self.pending {
            if let Slot::InFlight { msg_id, container } = p.slot {
                p.slot = Slot::Written { msg_id, container };
            }
        }
    }

    /// An incoming packet's plaintext.
    fn on_packet(&mut self, st: &mut Shared, packet: Bytes) -> Result<(), Cause> {
        let session_id = i64_at(&packet, 8).ok_or(Cause::Protocol("short packet"))?;
        if session_id != self.session_id {
            return Err(Cause::Protocol("another session's packet"));
        }
        let (msg_id, seqno, body, _) =
            message_at(&packet, PACKET_HEADER).ok_or(Cause::Protocol("message header"))?;
        self.on_message(st, msg_id, seqno, body, 0)
    }

    /// One message; `depth` counts the containers and `gzip_packed` around
    /// it (MTProto nests neither, so more than two is refused).
    fn on_message(
        &mut self,
        st: &mut Shared,
        msg_id: i64,
        seqno: i32,
        body: Bytes,
        depth: u8,
    ) -> Result<(), Cause> {
        if depth > 2 {
            return Err(Cause::Protocol("nested containers"));
        }
        if seqno & 1 == 1 {
            self.acks.push(msg_id);
        }
        let bad = || Cause::Protocol("service message");
        match u32_at(&body, 0).ok_or(bad())? {
            RPC_RESULT => {
                let req_msg_id = i64_at(&body, 4).ok_or(bad())?;
                let outcome = rpc_outcome(body.slice(12..), true);
                self.resolve(req_msg_id, outcome);
            }
            MSG_CONTAINER => {
                let count = u32_at(&body, 4).ok_or(bad())?;
                let mut at = 8;
                for _ in 0..count {
                    let (id, seq, inner, next) = message_at(&body, at).ok_or(bad())?;
                    self.on_message(st, id, seq, inner, depth + 1)?;
                    at = next;
                }
            }
            GZIP_PACKED => {
                let inner = gunzip(&body[4..]).ok_or(Cause::Protocol("gzip_packed"))?;
                // Already acknowledged above, as the outer message.
                self.on_message(st, msg_id, 0, inner, depth + 1)?;
            }
            BAD_SERVER_SALT => {
                let bad_msg_id = i64_at(&body, 4).ok_or(bad())?;
                let code = i32_at(&body, 16).ok_or(bad())?;
                let salt = i64_at(&body, 20).ok_or(bad())?;
                st.salt = salt;
                self.on_bad_msg(st, msg_id, bad_msg_id, code);
            }
            BAD_MSG_NOTIFICATION => {
                let bad_msg_id = i64_at(&body, 4).ok_or(bad())?;
                let code = i32_at(&body, 16).ok_or(bad())?;
                self.on_bad_msg(st, msg_id, bad_msg_id, code);
            }
            NEW_SESSION_CREATED => st.salt = i64_at(&body, 20).ok_or(bad())?,
            PONG => {
                let ping_msg_id = i64_at(&body, 4).ok_or(bad())?;
                self.resolve(ping_msg_id, Err(SenderError::NoAnswer));
            }
            // grammers `handle_detailed_info`: acknowledge the answer.
            MSG_DETAILED_INFO => self.acks.push(i64_at(&body, 12).ok_or(bad())?),
            MSG_NEW_DETAILED_INFO => self.acks.push(i64_at(&body, 4).ok_or(bad())?),
            MSGS_ACK | FUTURE_SALTS | MSGS_STATE_REQ | MSGS_STATE_INFO | MSGS_ALL_INFO
            | MSG_RESEND_REQ | MSG_RESEND_ANS_REQ | DESTROY_SESSION_OK | DESTROY_SESSION_NONE
            | HTTP_WAIT | MSG_COPY => {}
            _ => st.backlog.push_back(body),
        }
        Ok(())
    }

    /// An answer for the message `msg_id`.
    fn resolve(&mut self, msg_id: i64, outcome: Result<RpcOutcome, SenderError>) {
        let Some(i) = self
            .pending
            .iter()
            .position(|p| p.slot.msg_id() == Some(msg_id))
        else {
            return;
        };
        let p = self.pending.remove(i);
        match p.what {
            What::Ping { .. } => self.pong_deadline = None,
            What::Request { reply, .. } => {
                if self.init == Init::Pending(p.id) {
                    self.init = match outcome {
                        Ok(RpcOutcome::Result(_)) => Init::Done,
                        _ => Init::Needed,
                    };
                }
                let _ = reply.send(outcome);
            }
        }
    }

    /// Telegram ignored the message `bad_msg_id` (a request or a container).
    fn on_bad_msg(&mut self, st: &mut Shared, server_msg_id: i64, bad_msg_id: i64, code: i32) {
        // grammers `handle_bad_notification`.
        match code {
            16 | 17 => st.clock.correct(server_msg_id),
            32 => self.seq += 64,
            33 => self.seq -= 16,
            _ => {}
        }
        let retry = matches!(code, 16 | 17 | 48);
        let mut kept = VecDeque::with_capacity(self.sent_acks.len());
        for (ack_id, container, ids) in self.sent_acks.drain(..) {
            if ack_id == bad_msg_id || container == bad_msg_id {
                self.acks.extend(ids);
            } else {
                kept.push_back((ack_id, container, ids));
            }
        }
        self.sent_acks = kept;
        let mut i = 0;
        while i < self.pending.len() {
            let p = &mut self.pending[i];
            if !p.slot.matches(bad_msg_id) {
                i += 1;
                continue;
            }
            if self.init == Init::Pending(p.id) {
                self.init = Init::Needed;
            }
            if retry && !p.resent {
                p.slot = Slot::Queued;
                p.resent = true;
                i += 1;
                continue;
            }
            let p = self.pending.remove(i);
            match p.what {
                What::Ping { .. } => self.pong_deadline = None,
                What::Request { reply, .. } => {
                    let _ = reply.send(Err(SenderError::BadMessage { code }));
                }
            }
        }
    }

    fn on_timer(&mut self, st: &Shared, now: Instant) -> Result<(), Cause> {
        if let Some(deadline) = self.pong_deadline
            && now >= deadline
        {
            // The pong may sit behind frames the full pushes channel keeps
            // unread.
            if st.backlog.is_empty() {
                return Err(Cause::PongTimeout);
            }
            self.pong_deadline = Some(now + st.tuning.pong_timeout);
        }
        if now >= self.next_ping {
            let ping = tl::functions::PingDelayDisconnect {
                ping_id: rand::random(),
                disconnect_delay: NO_PING_DISCONNECT,
            };
            self.push_pending(What::Ping {
                body: ping.to_bytes().into(),
            });
            self.next_ping = now + st.tuning.ping_interval;
            self.pong_deadline
                .get_or_insert(now + st.tuning.pong_timeout);
        }
        Ok(())
    }

    /// The connection is gone: every request still here gets its outcome.
    fn end(self, flushed: bool, gone: Option<KeyGone>) {
        for p in self.pending {
            let What::Request { reply, .. } = p.what else {
                continue;
            };
            let error = match (gone, p.slot) {
                (Some(gone), _) => gone.error(),
                (None, Slot::Queued) => SenderError::NotSent,
                // Telegram cannot have a frame the connection never took whole.
                (None, Slot::InFlight { .. }) if !flushed => SenderError::NotSent,
                (None, Slot::InFlight { .. } | Slot::Written { .. }) => SenderError::NoAnswer,
            };
            let _ = reply.send(Err(error));
        }
    }
}

/// An `rpc_result`'s `result`: Telegram's error, or its bytes.
fn rpc_outcome(result: Bytes, unpack: bool) -> Result<RpcOutcome, SenderError> {
    match u32_at(&result, 0) {
        Some(RPC_ERROR) => {
            let code = i32_at(&result, 4).ok_or(SenderError::NoAnswer)?;
            let message = tl_bytes(&result[8..]).ok_or(SenderError::NoAnswer)?;
            Ok(RpcOutcome::Error {
                code,
                message: String::from_utf8_lossy(message).into_owned(),
            })
        }
        Some(GZIP_PACKED) if unpack => {
            let inner = gunzip(&result[4..]).ok_or(SenderError::NoAnswer)?;
            rpc_outcome(inner, false)
        }
        _ => Ok(RpcOutcome::Result(result)),
    }
}

fn write_header(packet: &mut Vec<u8>, msg_id: i64, seqno: i32, len: usize) {
    packet.extend_from_slice(&msg_id.to_le_bytes());
    packet.extend_from_slice(&seqno.to_le_bytes());
    packet.extend_from_slice(&len_i32(len).to_le_bytes());
}

/// Lengths here are bounded by [`MAX_CONTAINER_PAYLOAD`].
fn len_i32(len: usize) -> i32 {
    i32::try_from(len).unwrap_or(i32::MAX)
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn i64_at(b: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The message at `at`: `(msg_id, seqno, body, end)`.
fn message_at(b: &Bytes, at: usize) -> Option<(i64, i32, Bytes, usize)> {
    let msg_id = i64_at(b, at)?;
    let seqno = i32_at(b, at + 8)?;
    let len = usize::try_from(i32_at(b, at + 12)?).ok()?;
    let start = at + MESSAGE_HEADER;
    let end = start.checked_add(len).filter(|end| *end <= b.len())?;
    Some((msg_id, seqno, b.slice(start..end), end))
}

/// A TL `bytes`/`string` value's content.
fn tl_bytes(b: &[u8]) -> Option<&[u8]> {
    let (len, start) = match *b.first()? {
        254 => (
            usize::from(*b.get(1)?) | usize::from(*b.get(2)?) << 8 | usize::from(*b.get(3)?) << 16,
            4,
        ),
        255 => return None,
        short => (usize::from(short), 1),
    };
    b.get(start..start + len)
}

/// `gzip_packed`'s `packed_data`, unpacked.
fn gunzip(packed: &[u8]) -> Option<Bytes> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(tl_bytes(packed)?)
        .take(MAX_UNPACKED)
        .read_to_end(&mut out)
        .ok()?;
    (u64::try_from(out.len()).ok()? < MAX_UNPACKED).then(|| out.into())
}
