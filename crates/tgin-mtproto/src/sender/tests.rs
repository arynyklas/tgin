//! The sender against a scripted transport that speaks plain MTProto
//! packets: the test plays Telegram, one message at a time.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::*;
use crate::{AuthKey, DcAddr, Opened};

/// Bounded wait for anything the test expects; the clock is paused, so it
/// passes only once the runtime is idle.
const WAIT: Duration = Duration::from_secs(5);
const ANSWER: [u8; 8] = [0x15, 0xc4, 0xb5, 0x1c, 0, 0, 0, 0];
const PING_DELAY_DISCONNECT: u32 = tl::functions::PingDelayDisconnect::CONSTRUCTOR_ID;

fn tuning(pushes: usize) -> Tuning {
    Tuning {
        pushes,
        ping_interval: Duration::from_secs(3600),
        pong_timeout: Duration::from_secs(3600),
        init_timeout: Duration::from_secs(30),
    }
}

fn dc() -> DcOption {
    DcOption {
        id: 2,
        addr: DcAddr {
            ipv4: "127.0.0.1:1".parse().unwrap(),
            ipv6: "[::1]:1".parse().unwrap(),
        },
    }
}

fn init() -> InitParams {
    let mut init = InitParams::new(12345);
    init.device_model = "test device".into();
    init.app_version = "0.0.0-test".into();
    init
}

fn get_config() -> Bytes {
    tl::functions::help::GetConfig {}.to_bytes().into()
}

fn get_nearest_dc() -> Bytes {
    tl::functions::help::GetNearestDc {}.to_bytes().into()
}

/// grammers' typed wrapper around `query`: the independent expectation for
/// the sender's byte-composed one.
fn typed_init<Q: Serializable>(init: &InitParams, query: Q) -> Vec<u8> {
    tl::functions::InvokeWithLayer {
        layer: tl::LAYER,
        query: tl::functions::InitConnection {
            api_id: init.api_id,
            device_model: init.device_model.clone(),
            system_version: init.system_version.clone(),
            app_version: init.app_version.clone(),
            system_lang_code: init.system_lang_code.clone(),
            lang_pack: init.lang_pack.clone(),
            lang_code: init.lang_code.clone(),
            proxy: None,
            params: None,
            query,
        },
    }
    .to_bytes()
}

/// A pushed update container; `n` tells them apart.
fn update(n: u8) -> Vec<u8> {
    // `updatesTooLong#e317af7e` padded with a marker word: any body that is
    // not a service message is a push.
    let mut body = tl::enums::Updates::TooLong.to_bytes();
    body.extend_from_slice(&[n, 0xee, 0xee, 0xee]);
    body
}

/// What a scripted connection's writer does with packets.
#[derive(Clone, Copy)]
enum Writes {
    /// Every packet is handed over at once.
    Deliver,
    /// Each packet waits for the test's verdict.
    Gated,
}

struct Script {
    modes: Mutex<VecDeque<Writes>>,
    conns: mpsc::UnboundedSender<Server>,
    /// Whether each `open` had a key.
    opens: Mutex<Vec<bool>>,
    /// The init call every connection must open with.
    init_call: Vec<u8>,
    /// How many of the next `open`s fail.
    failing_opens: Mutex<usize>,
}

#[derive(Clone)]
struct Scripted(Arc<Script>);

impl Transport for Scripted {
    type Reader = ScriptReader;
    type Writer = ScriptWriter;

    async fn open(
        &self,
        _dc: &DcOption,
        key: Option<&AuthKey>,
    ) -> Result<Opened<ScriptReader, ScriptWriter>, TransportError> {
        self.0.opens.lock().push(key.is_some());
        {
            let mut failing = self.0.failing_opens.lock();
            if *failing > 0 {
                *failing -= 1;
                return Err(TransportError::Closed);
            }
        }
        let mode = self.0.modes.lock().pop_front().unwrap_or(Writes::Deliver);
        let (to_client, from_server) = mpsc::unbounded_channel();
        let (to_server, from_client) = mpsc::unbounded_channel();
        let (gate, verdicts) = mpsc::unbounded_channel();
        let (attempt, attempts) = mpsc::unbounded_channel();
        let taken = Arc::new(AtomicUsize::new(0));
        let server = Server {
            to_client: Some(to_client),
            from_client,
            gate,
            attempts,
            gated: matches!(mode, Writes::Gated),
            init_call: self.0.init_call.clone(),
            taken: Arc::clone(&taken),
            session_id: None,
            next_msg_id: (1_800_000_000 << 32) | 1,
            seq: 0,
            last_msg_id: 0,
            last_seqno: -1,
            floors: Vec::new(),
        };
        self.0.conns.send(server).unwrap();
        Ok(Opened {
            reader: ScriptReader { from_server, taken },
            writer: ScriptWriter {
                to_server,
                mode,
                verdicts,
                attempt,
                queued: None,
                announced: false,
            },
            generated: None,
        })
    }
}

struct ScriptReader {
    from_server: mpsc::UnboundedReceiver<Result<Bytes, TransportError>>,
    /// Frames the sender has read.
    taken: Arc<AtomicUsize>,
}

impl PacketReader for ScriptReader {
    async fn recv(&mut self) -> Result<Bytes, TransportError> {
        match self.from_server.recv().await {
            Some(Ok(packet)) => {
                self.taken.fetch_add(1, Ordering::SeqCst);
                Ok(packet)
            }
            Some(Err(e)) => Err(e),
            None => Err(TransportError::Closed),
        }
    }
}

struct ScriptWriter {
    to_server: mpsc::UnboundedSender<Vec<u8>>,
    mode: Writes,
    verdicts: mpsc::UnboundedReceiver<bool>,
    /// Shows the test each gated packet before its verdict.
    attempt: mpsc::UnboundedSender<Vec<u8>>,
    queued: Option<Vec<u8>>,
    announced: bool,
}

impl PacketWriter for ScriptWriter {
    fn queue(&mut self, packet: &[u8]) {
        self.queued = Some(packet.to_vec());
        self.announced = false;
    }

    async fn flush(&mut self) -> Result<(), TransportError> {
        if let Writes::Gated = self.mode {
            if !self.announced {
                self.announced = true;
                let _ = self.attempt.send(self.queued.clone().unwrap_or_default());
            }
            if self.verdicts.recv().await != Some(true) {
                return Err(TransportError::Closed);
            }
        }
        let packet = self.queued.take().unwrap_or_default();
        self.to_server
            .send(packet)
            .map_err(|_| TransportError::Closed)
    }

    fn is_flushed(&self) -> bool {
        self.queued.is_none()
    }
}

/// One message of a packet.
#[derive(Debug, Clone)]
struct Msg {
    msg_id: i64,
    seqno: i32,
    body: Vec<u8>,
}

#[derive(Debug)]
struct Packet {
    salt: i64,
    session_id: i64,
    container: Option<i64>,
    container_seqno: i32,
    msgs: Vec<Msg>,
}

fn le_i64(b: &[u8]) -> i64 {
    i64::from_le_bytes(b[..8].try_into().unwrap())
}

fn le_i32(b: &[u8]) -> i32 {
    i32::from_le_bytes(b[..4].try_into().unwrap())
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

/// `(msg, next offset)`.
fn parse_msg(b: &[u8], at: usize) -> (Msg, usize) {
    let len = usize::try_from(le_i32(&b[at + 12..])).unwrap();
    let start = at + 16;
    let msg = Msg {
        msg_id: le_i64(&b[at..]),
        seqno: le_i32(&b[at + 8..]),
        body: b[start..start + len].to_vec(),
    };
    (msg, start + len)
}

impl Packet {
    fn parse(raw: &[u8]) -> Self {
        let (outer, _) = parse_msg(raw, 16);
        let container_seqno = outer.seqno;
        let (container, msgs) = if le_u32(&outer.body) == MSG_CONTAINER {
            let count = le_i32(&outer.body[4..]);
            let mut at = 8;
            let mut msgs = Vec::new();
            for _ in 0..count {
                let (msg, next) = parse_msg(&outer.body, at);
                msgs.push(msg);
                at = next;
            }
            assert!(
                msgs.iter().all(|m| m.msg_id < outer.msg_id),
                "container id is the largest"
            );
            (Some(outer.msg_id), msgs)
        } else {
            (None, vec![outer])
        };
        Self {
            salt: le_i64(raw),
            session_id: le_i64(&raw[8..]),
            container,
            container_seqno,
            msgs,
        }
    }

    /// Messages other than `msgs_ack`.
    fn requests(&self) -> Vec<Msg> {
        self.msgs
            .iter()
            .filter(|m| le_u32(&m.body) != MSGS_ACK)
            .cloned()
            .collect()
    }

    /// Every msg_id acknowledged in this packet.
    fn acks(&self) -> Vec<i64> {
        let mut ids = Vec::new();
        for m in self.msgs.iter().filter(|m| le_u32(&m.body) == MSGS_ACK) {
            assert_eq!(m.seqno % 2, 0, "msgs_ack is not content-related");
            assert_eq!(le_u32(&m.body[4..]), VECTOR);
            let n = usize::try_from(le_i32(&m.body[8..])).unwrap();
            ids.extend((0..n).map(|i| le_i64(&m.body[12 + 8 * i..])));
        }
        ids
    }

    /// What Telegram names when it ignores this packet.
    fn bad_msg_id(&self) -> i64 {
        self.container.unwrap_or(self.msgs[0].msg_id)
    }
}

/// The test's end of one scripted connection.
struct Server {
    to_client: Option<mpsc::UnboundedSender<Result<Bytes, TransportError>>>,
    from_client: mpsc::UnboundedReceiver<Vec<u8>>,
    gate: mpsc::UnboundedSender<bool>,
    attempts: mpsc::UnboundedReceiver<Vec<u8>>,
    /// Every packet waits for the test's verdict.
    gated: bool,
    /// The init call the connection must open with.
    init_call: Vec<u8>,
    taken: Arc<AtomicUsize>,
    session_id: Option<i64>,
    next_msg_id: i64,
    seq: i32,
    /// The client's last msg_id and seqno on this session.
    last_msg_id: i64,
    last_seqno: i32,
    /// `(packet's bad_msg_id, last_msg_id before it)`, to forget a packet
    /// Telegram ignored for its msg_ids (codes 16 and 17).
    floors: Vec<(i64, i64)>,
}

impl Server {
    /// Parses a packet the sender wrote and checks it the way Telegram
    /// does: one session per connection; msg_ids divisible by 4 and rising
    /// across the session, the container's above its messages; seqnos odd
    /// and rising for content messages, even and not falling for acks and
    /// containers.
    fn check(&mut self, raw: &[u8]) -> Packet {
        let packet = Packet::parse(raw);
        let session = *self.session_id.get_or_insert(packet.session_id);
        assert_eq!(packet.session_id, session, "one session per connection");
        self.floors.push((packet.bad_msg_id(), self.last_msg_id));
        let container = packet.container.map(|id| Msg {
            msg_id: id,
            seqno: packet.container_seqno,
            body: Vec::new(),
        });
        for m in packet.msgs.iter().chain(container.as_ref()) {
            assert_eq!(m.msg_id % 4, 0, "msg_id divisible by 4: {packet:?}");
            assert!(m.msg_id > self.last_msg_id, "msg_ids rise: {packet:?}");
            self.last_msg_id = m.msg_id;
            let content = !m.body.is_empty() && le_u32(&m.body) != MSGS_ACK;
            if content {
                assert_eq!(m.seqno % 2, 1, "content seqno odd: {packet:?}");
                assert!(m.seqno > self.last_seqno, "seqnos rise: {packet:?}");
            } else {
                assert_eq!(m.seqno % 2, 0, "service seqno even: {packet:?}");
                assert!(m.seqno >= self.last_seqno, "seqnos rise: {packet:?}");
            }
            self.last_seqno = m.seqno;
        }
        packet
    }

    /// The next packet the sender wrote.
    async fn packet(&mut self) -> Packet {
        let raw = timeout(WAIT, self.from_client.recv())
            .await
            .expect("the sender writes a packet")
            .expect("the connection is open");
        self.check(&raw)
    }

    /// The next packet that carries more than acks.
    async fn request(&mut self) -> Packet {
        loop {
            let packet = self.packet().await;
            if !packet.requests().is_empty() {
                return packet;
            }
        }
    }

    /// The sender writes nothing but acks within [`WAIT`].
    async fn no_request(&mut self) {
        loop {
            match timeout(WAIT, self.from_client.recv()).await {
                Err(_) | Ok(None) => return,
                Ok(Some(raw)) => {
                    let packet = self.check(&raw);
                    assert!(packet.requests().is_empty(), "unexpected {packet:?}");
                }
            }
        }
    }

    /// The next ping the sender writes within `within`, skipping other
    /// packets.
    async fn ping(&mut self, within: Duration) -> Msg {
        timeout(within, async {
            loop {
                let raw = self.from_client.recv().await.expect("connection open");
                let packet = self.check(&raw);
                if let Some(ping) = packet
                    .msgs
                    .iter()
                    .find(|m| le_u32(&m.body) == PING_DELAY_DISCONNECT)
                {
                    return ping.clone();
                }
            }
        })
        .await
        .expect("the sender pings")
    }

    /// Sends one push and returns the id of the ack-only packet that
    /// acknowledges it: a client message later than everything before.
    async fn provoke_ack(&mut self, n: u8) -> i64 {
        let id = self.send(&[(true, update(n))])[0];
        loop {
            let packet = self.packet().await;
            if packet.acks().contains(&id) {
                assert!(packet.requests().is_empty());
                return packet.bad_msg_id();
            }
        }
    }

    /// The packet the writer holds at the gate, not yet handed over.
    async fn write_attempt(&mut self) -> Packet {
        let raw = timeout(WAIT, self.attempts.recv())
            .await
            .expect("the sender tries to write")
            .expect("writer alive");
        Packet::parse(&raw)
    }

    /// Lets ack-only packets through the gate and returns the first one
    /// that carries more, still held at the gate.
    async fn held_request(&mut self) -> Packet {
        loop {
            let packet = self.write_attempt().await;
            if !packet.requests().is_empty() {
                return packet;
            }
            self.approve_write();
        }
    }

    /// The gated write goes through.
    fn approve_write(&self) {
        self.gate.send(true).unwrap();
    }

    /// The gated write fails: the connection takes none of the packet.
    fn refuse_write(&self) {
        self.gate.send(false).unwrap();
    }

    /// The connection's first packet: the sender's init call alone. Not
    /// answered.
    async fn init_packet(&mut self) -> Msg {
        if self.gated {
            self.held_request().await;
            self.approve_write();
        }
        let packet = self.request().await;
        let requests = packet.requests();
        assert_eq!(requests.len(), 1, "the init call goes alone: {packet:?}");
        assert_eq!(requests[0].body, self.init_call, "the init call");
        requests[0].clone()
    }

    /// Answers the connection's init call.
    async fn handshake(&mut self) {
        let init = self.init_packet().await;
        self.answer(init.msg_id, &ANSWER);
    }

    /// Sends one packet of messages `(content_related, body)`, in a
    /// container when there are several. Returns their msg_ids.
    fn send(&mut self, msgs: &[(bool, Vec<u8>)]) -> Vec<i64> {
        let session = self.session_id.expect("the sender wrote first");
        let mut ids = Vec::new();
        let mut inner = Vec::new();
        for (content, body) in msgs {
            let id = self.next_msg_id;
            self.next_msg_id += 4;
            let seq = if *content {
                self.seq += 1;
                self.seq * 2 - 1
            } else {
                self.seq * 2
            };
            inner.extend_from_slice(&id.to_le_bytes());
            inner.extend_from_slice(&seq.to_le_bytes());
            inner.extend_from_slice(&i32::try_from(body.len()).unwrap().to_le_bytes());
            inner.extend_from_slice(body);
            ids.push(id);
        }
        let mut packet = Vec::new();
        packet.extend_from_slice(&0x5a17_i64.to_le_bytes());
        packet.extend_from_slice(&session.to_le_bytes());
        if msgs.len() == 1 {
            packet.extend_from_slice(&inner);
        } else {
            let id = self.next_msg_id;
            self.next_msg_id += 4;
            packet.extend_from_slice(&id.to_le_bytes());
            packet.extend_from_slice(&(self.seq * 2).to_le_bytes());
            packet.extend_from_slice(&i32::try_from(inner.len() + 8).unwrap().to_le_bytes());
            packet.extend_from_slice(&MSG_CONTAINER.to_le_bytes());
            packet.extend_from_slice(&i32::try_from(msgs.len()).unwrap().to_le_bytes());
            packet.extend_from_slice(&inner);
        }
        // Telegram pads its plaintext; the sender must ignore the padding.
        packet.extend_from_slice(&[0x5f; 12]);
        self.to_client
            .as_ref()
            .expect("connection open")
            .send(Ok(packet.into()))
            .unwrap();
        ids
    }

    /// `rpc_result` for `req_msg_id`. Returns its msg_id.
    fn answer(&mut self, req_msg_id: i64, result: &[u8]) -> i64 {
        let mut body = RPC_RESULT.to_le_bytes().to_vec();
        body.extend_from_slice(&req_msg_id.to_le_bytes());
        body.extend_from_slice(result);
        self.send(&[(true, body)])[0]
    }

    /// `bad_server_salt` for the message `bad_msg_id`.
    fn bad_server_salt(&mut self, bad_msg_id: i64, new_salt: i64) {
        let body = tl::enums::BadMsgNotification::BadServerSalt(tl::types::BadServerSalt {
            bad_msg_id,
            bad_msg_seqno: 1,
            error_code: 48,
            new_server_salt: new_salt,
        })
        .to_bytes();
        self.send(&[(false, body)]);
    }

    /// `bad_msg_notification` `code` for `bad_msg_id`, sent as a message
    /// whose id carries Telegram's clock: `server_secs` seconds since the
    /// epoch.
    fn bad_msg_at(&mut self, server_secs: i64, bad_msg_id: i64, code: i32) {
        let body = tl::enums::BadMsgNotification::Notification(tl::types::BadMsgNotification {
            bad_msg_id,
            bad_msg_seqno: 1,
            error_code: code,
        })
        .to_bytes();
        let counter = self.next_msg_id;
        self.next_msg_id = (server_secs << 32) | 1;
        self.send(&[(false, body)]);
        self.next_msg_id = counter;
        // Telegram ignored the packet: its msg_ids no longer count.
        if let Some(&(_, floor)) = self.floors.iter().find(|(id, _)| *id == bad_msg_id) {
            self.last_msg_id = floor;
        }
    }

    /// `new_session_created`: Telegram made a new session at the client's
    /// message `first_msg_id`.
    fn new_session(&mut self, first_msg_id: i64, salt: i64) {
        let body = tl::enums::NewSession::Created(tl::types::NewSessionCreated {
            first_msg_id,
            unique_id: 77,
            server_salt: salt,
        })
        .to_bytes();
        self.send(&[(true, body)]);
    }

    /// The transport fails with `error` (a −404, say) and the connection
    /// ends.
    fn fail(&mut self, error: TransportError) {
        if let Some(to_client) = self.to_client.take() {
            to_client.send(Err(error)).unwrap();
        }
    }

    /// Telegram closes the connection.
    fn close(&mut self) {
        self.to_client = None;
    }

    fn frames_taken(&self) -> usize {
        self.taken.load(Ordering::SeqCst)
    }
}

struct Harness {
    sender: Arc<Sender>,
    pushes: Option<Pushes>,
    conns: mpsc::UnboundedReceiver<Server>,
    script: Arc<Script>,
}

impl Harness {
    /// A sender on its first connection; the init call is not answered.
    async fn start(init: InitParams, modes: &[Writes], pushes: usize) -> (Self, Server) {
        Self::start_with(init, modes, tuning(pushes)).await
    }

    async fn start_with(init: InitParams, modes: &[Writes], tuning: Tuning) -> (Self, Server) {
        let (conns_tx, mut conns) = mpsc::unbounded_channel();
        let script = Arc::new(Script {
            modes: Mutex::new(modes.iter().copied().collect()),
            conns: conns_tx,
            opens: Mutex::new(Vec::new()),
            init_call: init.init_call(),
            failing_opens: Mutex::new(0),
        });
        let key = Some(AuthKey([9; 256]));
        let (sender, pushes, new_key) =
            Sender::start(Scripted(Arc::clone(&script)), dc(), key, init, tuning)
                .await
                .unwrap();
        assert!(new_key.is_none(), "the sender made no key");
        let first = conns.recv().await.unwrap();
        let harness = Self {
            sender: Arc::new(sender),
            pushes: Some(pushes),
            conns,
            script,
        };
        (harness, first)
    }

    /// A sender whose first connection has answered its init call.
    async fn ready(init: InitParams, modes: &[Writes], pushes: usize) -> (Self, Server) {
        let (h, mut srv) = Self::start(init, modes, pushes).await;
        srv.handshake().await;
        (h, srv)
    }

    fn invoke(&self, body: Bytes) -> JoinHandle<Result<RpcOutcome, SenderError>> {
        let sender = Arc::clone(&self.sender);
        tokio::spawn(async move { sender.invoke(body).await })
    }

    async fn next_conn(&mut self) -> Server {
        timeout(WAIT, self.conns.recv())
            .await
            .expect("the sender opens a connection")
            .unwrap()
    }

    /// The next connection, its init call answered.
    async fn ready_conn(&mut self) -> Server {
        let mut srv = self.next_conn().await;
        srv.handshake().await;
        srv
    }

    fn opens(&self) -> Vec<bool> {
        self.script.opens.lock().clone()
    }
}

async fn outcome(
    call: JoinHandle<Result<RpcOutcome, SenderError>>,
) -> Result<RpcOutcome, SenderError> {
    timeout(WAIT, call).await.expect("the call ends").unwrap()
}

fn answered() -> RpcOutcome {
    RpcOutcome::Result(Bytes::from_static(&ANSWER))
}

/// Seconds since the epoch in a msg_id.
fn secs(msg_id: i64) -> i64 {
    msg_id >> 32
}

/// Code 17 (msg_id too high) from a server an hour behind the client, on
/// the session's first packet: the resend and everything after it carry
/// Telegram's time, not a floor left by the fast clock.
#[tokio::test(start_paused = true)]
async fn code_17_moves_msg_ids_back_to_server_time() {
    let (h, mut srv) = Harness::start(init(), &[Writes::Deliver], 8).await;
    let call = h.invoke(get_config());
    let first = srv.init_packet().await;
    let server_secs = secs(first.msg_id) - 3600;
    srv.bad_msg_at(server_secs, first.msg_id, 17);

    let again = srv.init_packet().await;
    assert!(
        (secs(again.msg_id) - server_secs).abs() <= 5,
        "resent at {} s, server at {server_secs} s",
        secs(again.msg_id)
    );
    srv.answer(again.msg_id, &ANSWER);
    let packet = srv.request().await;
    let request = packet.requests()[0].clone();
    assert!((secs(request.msg_id) - server_secs).abs() <= 5);
    srv.answer(request.msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
}

/// Code 16 (msg_id too low) from a server an hour ahead: the resend
/// carries Telegram's time.
#[tokio::test(start_paused = true)]
async fn code_16_moves_msg_ids_up_to_server_time() {
    let (h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let call = h.invoke(get_config());
    let first = srv.request().await;
    let sent = first.requests()[0].clone();
    let server_secs = secs(sent.msg_id) + 3600;
    srv.bad_msg_at(server_secs, first.bad_msg_id(), 16);

    let again = srv.request().await;
    let resent = again.requests()[0].clone();
    assert_eq!(resent.body, sent.body);
    assert!(
        (secs(resent.msg_id) - server_secs).abs() <= 5,
        "resent at {} s, server at {server_secs} s",
        secs(resent.msg_id)
    );
    srv.answer(resent.msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
}

/// Codes 32 and 33 (msg_seqno too low or too high): the named request
/// fails, and the connection ends, so the next request goes out on a fresh
/// session with seqno from 0.
#[tokio::test(start_paused = true)]
async fn seqno_codes_fail_the_request_and_end_the_connection() {
    for code in [32, 33] {
        let (mut h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
        let call = h.invoke(get_config());
        let packet = srv.request().await;
        let sent = packet.requests()[0].msg_id;
        srv.bad_msg_at(secs(sent), packet.bad_msg_id(), code);
        let result = outcome(call).await;
        assert!(
            matches!(result, Err(SenderError::BadMessage { code: c }) if c == code),
            "{code}: {result:?}"
        );
        let closed = timeout(WAIT, srv.from_client.recv()).await;
        assert!(matches!(closed, Ok(None)), "{code}: the connection ends");

        let call = h.invoke(get_config());
        let mut fresh = h.ready_conn().await;
        let packet = fresh.request().await;
        fresh.answer(packet.requests()[0].msg_id, &ANSWER);
        assert_eq!(outcome(call).await.unwrap(), answered());
    }
}

/// Answers one `help.getConfig`, so the server knows the session.
async fn first_call(h: &Harness, srv: &mut Server) {
    let call = h.invoke(get_config());
    let packet = srv.request().await;
    srv.answer(packet.requests()[0].msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
}

/// Every packet that already reached the connection carries acks only.
fn assert_no_request_reached(srv: &mut Server) {
    while let Ok(raw) = srv.from_client.try_recv() {
        let packet = srv.check(&raw);
        assert!(packet.requests().is_empty(), "reached: {packet:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn failed_write_is_not_sent() {
    let (mut h, mut srv) = Harness::start(init(), &[Writes::Gated, Writes::Deliver], 8).await;
    srv.handshake().await;
    let call = h.invoke(get_config());
    let held = srv.held_request().await;
    assert_eq!(held.requests()[0].body, get_config());
    srv.refuse_write();

    let result = outcome(call).await;
    assert!(matches!(result, Err(SenderError::NotSent)), "{result:?}");
    assert_no_request_reached(&mut srv);
    assert!(
        h.conns.try_recv().is_err(),
        "the sender does not resend on its own"
    );
}

#[tokio::test(start_paused = true)]
async fn request_written_then_drop_is_no_answer() {
    let (mut h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let call = h.invoke(get_config());
    let packet = srv.request().await;
    assert_eq!(packet.requests().len(), 1, "the request was written");
    srv.close();

    let result = outcome(call).await;
    assert!(matches!(result, Err(SenderError::NoAnswer)), "{result:?}");
    // Never resent: no new connection, nothing more on the old one.
    srv.no_request().await;
    assert!(h.conns.try_recv().is_err(), "no fresh connection");
    assert_eq!(h.opens(), [true]);
}

#[tokio::test(start_paused = true)]
async fn drop_before_write_is_not_sent_and_resend_uses_fresh_connection() {
    let (mut h, mut srv) = Harness::start(init(), &[Writes::Gated, Writes::Deliver], 8).await;
    srv.handshake().await;
    let call = h.invoke(get_config());
    // The packet waits at the gate: the connection has none of it.
    srv.held_request().await;
    srv.close();

    let result = outcome(call).await;
    assert!(matches!(result, Err(SenderError::NotSent)), "{result:?}");
    assert_no_request_reached(&mut srv);

    // The one allowed resend goes on a fresh connection, with the same key.
    let call = h.invoke(get_config());
    let mut fresh = h.ready_conn().await;
    let packet = fresh.request().await;
    let requests = packet.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body, get_config());
    fresh.answer(requests[0].msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
    assert_eq!(h.opens(), [true, true]);
}

#[tokio::test(start_paused = true)]
async fn bad_server_salt_resends_once_with_new_salt() {
    let (h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let call = h.invoke(get_config());
    let first = srv.request().await;
    assert_eq!(first.salt, 0, "an existing key starts without a salt");
    let sent = first.requests()[0].clone();

    srv.bad_server_salt(first.bad_msg_id(), 0x5a1f);
    let again = srv.request().await;
    assert_eq!(again.salt, 0x5a1f, "resent with the new salt");
    let resent = again.requests()[0].clone();
    assert_ne!(resent.msg_id, sent.msg_id, "a new msg_id");
    assert_eq!(resent.body, sent.body, "the same request");
    srv.answer(resent.msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());

    // Once: a request ignored twice fails instead of going out a third time.
    let call = h.invoke(get_nearest_dc());
    let first = srv.request().await;
    assert_eq!(first.salt, 0x5a1f, "later packets keep the salt");
    srv.bad_server_salt(first.bad_msg_id(), 0x5a20);
    let again = srv.request().await;
    assert_eq!(again.salt, 0x5a20);
    srv.bad_server_salt(again.bad_msg_id(), 0x5a21);
    let result = outcome(call).await;
    assert!(
        matches!(result, Err(SenderError::BadMessage { code: 48 })),
        "{result:?}"
    );
    srv.no_request().await;
}

/// `gzip_packed` around an answer and around a push is unwrapped; an
/// `rpc_error` keeps Telegram's whole message [C: S11 F5].
#[tokio::test(start_paused = true)]
async fn gzip_and_rpc_error_pass_through() {
    use std::io::Write as _;

    fn gzip_packed(data: &[u8]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(data).unwrap();
        let mut body = GZIP_PACKED.to_le_bytes().to_vec();
        body.extend(gz.finish().unwrap().to_bytes());
        body
    }

    let (mut h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let mut pushes = h.pushes.take().unwrap();
    let big = vec![0x42; 4096];
    let call = h.invoke(get_config());
    let packet = srv.request().await;
    srv.answer(packet.requests()[0].msg_id, &gzip_packed(&big));
    assert_eq!(
        outcome(call).await.unwrap(),
        RpcOutcome::Result(Bytes::from(big))
    );

    srv.send(&[(true, gzip_packed(&update(7)))]);
    let push = timeout(WAIT, pushes.recv()).await.unwrap().unwrap();
    assert_eq!(push, update(7));

    let call = h.invoke(get_nearest_dc());
    let packet = srv.request().await;
    let error = tl::enums::RpcError::Error(tl::types::RpcError {
        error_code: 500,
        error_message: "INTERDC_2_CALL_ERROR".into(),
    });
    srv.answer(packet.requests()[0].msg_id, &error.to_bytes());
    assert_eq!(
        outcome(call).await.unwrap(),
        RpcOutcome::Error {
            code: 500,
            message: "INTERDC_2_CALL_ERROR".into()
        }
    );
}

#[tokio::test(start_paused = true)]
async fn container_of_pushes_split_in_order() {
    let (mut h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let mut pushes = h.pushes.take().unwrap();
    first_call(&h, &mut srv).await;

    let ids = srv.send(&[(true, update(1)), (true, update(2)), (true, update(3))]);
    for n in 1..=3 {
        let push = timeout(WAIT, pushes.recv()).await.unwrap().unwrap();
        assert_eq!(
            push,
            update(n),
            "push {n}: the container's message, as sent"
        );
    }
    // Each one is acknowledged.
    let mut acked = Vec::new();
    while !ids.iter().all(|id| acked.contains(id)) {
        acked.extend(srv.packet().await.acks());
    }
    assert!(
        timeout(WAIT, pushes.recv()).await.is_err(),
        "nothing else was pushed"
    );
}

#[tokio::test(start_paused = true)]
async fn minus_404_on_logged_in_key_is_key_revoked() {
    let mut init = init();
    init.logged_in = true;
    let (mut h, mut srv) = Harness::ready(init, &[Writes::Deliver], 8).await;
    let call = h.invoke(get_config());
    srv.request().await;
    srv.fail(TransportError::Status(404));

    let result = outcome(call).await;
    assert!(matches!(result, Err(SenderError::KeyRevoked)), "{result:?}");

    // The key stays dead: no reconnect, no new key.
    let result = outcome(h.invoke(get_config())).await;
    assert!(matches!(result, Err(SenderError::KeyRevoked)), "{result:?}");
    assert!(h.conns.try_recv().is_err(), "no fresh connection");
    assert_eq!(h.opens(), [true], "never opened without the key");
}

#[tokio::test(start_paused = true)]
async fn minus_404_before_login_is_key_unknown() {
    let (mut h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let call = h.invoke(get_config());
    srv.request().await;
    srv.fail(TransportError::Status(404));
    let result = outcome(call).await;
    assert!(matches!(result, Err(SenderError::KeyUnknown)), "{result:?}");
    assert!(h.conns.try_recv().is_err(), "no fresh connection");
    assert_eq!(h.opens(), [true]);
}

/// A login on a sender made before it marks the key logged in: a later
/// −404 is a revoked key, not an unknown one.
#[tokio::test(start_paused = true)]
async fn mark_logged_in_turns_minus_404_into_key_revoked() {
    let (h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    first_call(&h, &mut srv).await;
    h.sender.mark_logged_in();
    let call = h.invoke(get_config());
    srv.request().await;
    srv.fail(TransportError::Status(404));
    let result = outcome(call).await;
    assert!(matches!(result, Err(SenderError::KeyRevoked)), "{result:?}");
}

/// The synthetic `updatesTooLong` the sender pushes when the session's
/// update stream may have a gap.
fn too_long() -> Bytes {
    Bytes::from_static(&[0x7e, 0xaf, 0x17, 0xe3])
}

/// A lost connection tells the update manager to catch up (P1); a −404
/// does not, since there is nothing to catch up with.
#[tokio::test(start_paused = true)]
async fn connection_loss_pushes_updates_too_long() {
    let (mut h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let mut pushes = h.pushes.take().unwrap();
    srv.close();
    let push = timeout(WAIT, pushes.recv()).await.unwrap();
    assert_eq!(push, Some(too_long()));

    let call = h.invoke(get_config());
    let mut fresh = h.ready_conn().await;
    fresh.request().await;
    fresh.fail(TransportError::Status(404));
    assert!(matches!(outcome(call).await, Err(SenderError::KeyUnknown)));
    assert!(
        timeout(WAIT, pushes.recv()).await.is_err(),
        "no catch-up on a dead key"
    );
}

/// `new_session_created` means Telegram may have dropped updates: the
/// sender pushes `updatesTooLong` and takes the new salt (P1).
#[tokio::test(start_paused = true)]
async fn new_session_created_pushes_updates_too_long() {
    let (mut h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let mut pushes = h.pushes.take().unwrap();
    let call = h.invoke(get_config());
    let packet = srv.request().await;
    let sent = packet.requests()[0].msg_id;
    srv.new_session(sent, 0x5e55);
    let push = timeout(WAIT, pushes.recv()).await.unwrap();
    assert_eq!(push, Some(too_long()));
    srv.answer(sent, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
    let next = srv.packet().await;
    assert_eq!(next.salt, 0x5e55, "the new session's salt");
}

/// A result whose top constructor is an `Updates` one goes to its caller
/// and, in wire order with the pushes, to the update stream (P4). Other
/// results, `messages.affectedMessages` included, go to the caller only.
#[tokio::test(start_paused = true)]
async fn update_bearing_result_is_also_pushed_in_wire_order() {
    fn rpc_result(req_msg_id: i64, result: &[u8]) -> Vec<u8> {
        let mut body = RPC_RESULT.to_le_bytes().to_vec();
        body.extend_from_slice(&req_msg_id.to_le_bytes());
        body.extend_from_slice(result);
        body
    }

    let (mut h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let mut pushes = h.pushes.take().unwrap();
    let call = h.invoke(get_config());
    let req = srv.request().await.requests()[0].msg_id;
    srv.send(&[
        (true, update(1)),
        (true, rpc_result(req, &update(9))),
        (true, update(2)),
    ]);
    assert_eq!(
        outcome(call).await.unwrap(),
        RpcOutcome::Result(update(9).into())
    );
    for n in [1, 9, 2] {
        let push = timeout(WAIT, pushes.recv()).await.unwrap().unwrap();
        assert_eq!(push, update(n));
    }

    let affected =
        tl::enums::messages::AffectedMessages::Messages(tl::types::messages::AffectedMessages {
            pts: 10,
            pts_count: 1,
        })
        .to_bytes();
    let call = h.invoke(get_nearest_dc());
    let req = srv.request().await.requests()[0].msg_id;
    srv.answer(req, &affected);
    assert_eq!(
        outcome(call).await.unwrap(),
        RpcOutcome::Result(affected.into())
    );
    assert!(
        timeout(WAIT, pushes.recv()).await.is_err(),
        "affected* results are not pushed"
    );
}

/// Requests written before `new_session_created`'s `first_msg_id` went to
/// a session whose answers are lost: they get `NoAnswer`. Later ones are
/// still answered.
#[tokio::test(start_paused = true)]
async fn new_session_created_orphans_earlier_requests() {
    let (h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let early = h.invoke(get_config());
    let first = srv.request().await.requests()[0].msg_id;
    let late = h.invoke(get_nearest_dc());
    let second = srv.request().await;
    let second_id = second.requests()[0].msg_id;
    assert!(second.bad_msg_id() > first);

    srv.new_session(second_id, 0x5e55);
    let result = outcome(early).await;
    assert!(matches!(result, Err(SenderError::NoAnswer)), "{result:?}");
    srv.answer(second_id, &ANSWER);
    assert_eq!(outcome(late).await.unwrap(), answered());
}

/// An init call orphaned by `new_session_created` is made again, and the
/// requests behind it then go out.
#[tokio::test(start_paused = true)]
async fn new_session_created_repeats_an_orphaned_init() {
    let (h, mut srv) = Harness::start(init(), &[Writes::Deliver], 8).await;
    let call = h.invoke(get_config());
    srv.init_packet().await;
    let later = srv.provoke_ack(1).await;
    srv.new_session(later, 0x5e55);

    srv.handshake().await;
    let packet = srv.request().await;
    assert_eq!(packet.requests()[0].body, get_config());
    srv.answer(packet.requests()[0].msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
}

/// A ping orphaned by `new_session_created` gets no pong; dropping it keeps
/// its pong deadline from ending a healthy connection.
#[tokio::test(start_paused = true)]
async fn new_session_created_drops_orphaned_pings() {
    let tuning = Tuning {
        ping_interval: Duration::from_secs(20),
        pong_timeout: Duration::from_secs(10),
        ..tuning(8)
    };
    let (h, mut srv) = Harness::start_with(init(), &[Writes::Deliver], tuning).await;
    srv.handshake().await;
    srv.ping(Duration::from_secs(30)).await;
    let later = srv.provoke_ack(1).await;
    srv.new_session(later, 0x5e55);
    // Past the dropped ping's pong deadline, before the next ping.
    tokio::time::sleep(Duration::from_secs(15)).await;

    let call = h.invoke(get_config());
    let packet = srv.request().await;
    srv.answer(packet.requests()[0].msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
}

/// Every connection opens with the sender's own `initConnection` call;
/// callers' requests wait for its answer and are never wrapped (P5).
#[tokio::test(start_paused = true)]
async fn init_call_opens_every_connection_and_gates_requests() {
    let init = init();
    let (mut h, mut srv) = Harness::start(init.clone(), &[Writes::Deliver], 8).await;
    let first = h.invoke(get_config());
    let call = srv.init_packet().await;
    assert_eq!(
        call.body,
        typed_init(&init, tl::functions::help::GetNearestDc {})
    );
    assert_eq!(&call.body[4..8], &tl::LAYER.to_le_bytes());
    srv.no_request().await;
    srv.answer(call.msg_id, &ANSWER);

    let packet = srv.request().await;
    let requests = packet.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body, get_config(), "not wrapped");
    srv.answer(requests[0].msg_id, &ANSWER);
    assert_eq!(outcome(first).await.unwrap(), answered());

    srv.close();
    let second = h.invoke(get_nearest_dc());
    let mut fresh = h.next_conn().await;
    let call = fresh.init_packet().await;
    fresh.answer(call.msg_id, &ANSWER);
    let packet = fresh.request().await;
    let requests = packet.requests();
    assert_eq!(requests[0].body, get_nearest_dc(), "not wrapped");
    fresh.answer(requests[0].msg_id, &ANSWER);
    assert_eq!(outcome(second).await.unwrap(), answered());
}

/// An unanswered init call ends the connection at its timeout; the
/// requests waiting behind it were never written (P5).
#[tokio::test(start_paused = true)]
async fn init_timeout_drops_connection_and_waiting_requests_are_not_sent() {
    let (mut h, mut srv) = Harness::start(init(), &[Writes::Deliver], 8).await;
    let call = h.invoke(get_config());
    srv.init_packet().await;
    let result = timeout(Duration::from_secs(60), call)
        .await
        .expect("the init timeout ends the call")
        .unwrap();
    assert!(matches!(result, Err(SenderError::NotSent)), "{result:?}");
    let closed = timeout(WAIT, srv.from_client.recv()).await;
    assert!(matches!(closed, Ok(None)), "the connection is dropped");

    let call = h.invoke(get_config());
    let mut fresh = h.ready_conn().await;
    let packet = fresh.request().await;
    fresh.answer(packet.requests()[0].msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
}

/// Telegram's `rpc_error` to the init call reaches the waiting requests.
#[tokio::test(start_paused = true)]
async fn init_refused_fails_waiting_requests() {
    let (h, mut srv) = Harness::start(init(), &[Writes::Deliver], 8).await;
    let call = h.invoke(get_config());
    let init_msg = srv.init_packet().await;
    let error = tl::enums::RpcError::Error(tl::types::RpcError {
        error_code: 400,
        error_message: "API_ID_INVALID".into(),
    });
    srv.answer(init_msg.msg_id, &error.to_bytes());
    let result = outcome(call).await;
    assert!(
        matches!(&result, Err(SenderError::InitRefused { code: 400, message }) if message == "API_ID_INVALID"),
        "{result:?}"
    );
}

/// A dropped receiver makes the sender discard pushes instead of pausing
/// its reads, so answers still arrive.
#[tokio::test(start_paused = true)]
async fn dropped_pushes_receiver_discards_pushes() {
    let (mut h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 2).await;
    drop(h.pushes.take());
    first_call(&h, &mut srv).await;
    for n in 0..10 {
        srv.send(&[(true, update(n))]);
    }
    let call = h.invoke(get_nearest_dc());
    let packet = srv.request().await;
    srv.answer(packet.requests()[0].msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
}

#[tokio::test(start_paused = true)]
async fn without_updates_wrapper_on_media_sessions() {
    let mut init = init();
    init.without_updates = true;
    let (mut h, mut srv) = Harness::start(init.clone(), &[Writes::Deliver], 8).await;
    let mut pushes = h.pushes.take().unwrap();
    let closed = timeout(WAIT, pushes.recv()).await;
    assert_eq!(closed, Ok(None), "a media session hands out no pushes");

    let call = h.invoke(get_config());
    let init_msg = srv.init_packet().await;
    let wrapped = tl::functions::InvokeWithoutUpdates {
        query: tl::functions::help::GetNearestDc {},
    };
    assert_eq!(init_msg.body, typed_init(&init, wrapped));
    srv.answer(init_msg.msg_id, &ANSWER);

    let packet = srv.request().await;
    let requests = packet.requests();
    let wrapped = tl::functions::InvokeWithoutUpdates {
        query: tl::functions::help::GetConfig {},
    };
    assert_eq!(requests[0].body, wrapped.to_bytes());
    srv.answer(requests[0].msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
}

#[tokio::test(start_paused = true)]
async fn pushes_channel_full_pauses_reads_not_acks() {
    let (mut h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 2).await;
    let mut pushes = h.pushes.take().unwrap();
    first_call(&h, &mut srv).await;
    srv.no_request().await; // the ack for the answer
    let taken = srv.frames_taken();

    // Three pushes for a channel of two, then a fourth frame.
    let first = srv.send(&[(true, update(1)), (true, update(2)), (true, update(3))]);
    let later = srv.send(&[(true, update(4))]);

    // The acks it owes go out while the channel is full ...
    let mut acked = Vec::new();
    while !first.iter().all(|id| acked.contains(id)) {
        acked.extend(srv.packet().await.acks());
    }
    // ... but the next frame stays unread.
    srv.no_request().await;
    assert_eq!(srv.frames_taken(), taken + 1, "reading paused");

    // Draining the channel resumes reading, in order.
    for n in 1..=4 {
        let push = timeout(WAIT, pushes.recv()).await.unwrap().unwrap();
        assert_eq!(push, update(n));
    }
    let mut acked = Vec::new();
    while !acked.contains(&later[0]) {
        acked.extend(srv.packet().await.acks());
    }
    assert_eq!(srv.frames_taken(), taken + 2);
}

fn pinging(pushes: usize) -> Tuning {
    Tuning {
        ping_interval: Duration::from_secs(20),
        pong_timeout: Duration::from_secs(10),
        ..tuning(pushes)
    }
}

/// `pong` for a `ping_delay_disconnect` message.
fn pong(ping: &Msg) -> Vec<u8> {
    tl::enums::Pong::Pong(tl::types::Pong {
        msg_id: ping.msg_id,
        ping_id: le_i64(&ping.body[4..]),
    })
    .to_bytes()
}

/// `msg_detailed_info` names an answer Telegram sent earlier: the sender
/// acknowledges it.
#[tokio::test(start_paused = true)]
async fn msg_detailed_info_acknowledges_the_answer() {
    let (_h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let answer_msg_id = (1_800_000_100_i64 << 32) | 5;
    let info = tl::enums::MsgDetailedInfo::Info(tl::types::MsgDetailedInfo {
        msg_id: 4,
        answer_msg_id,
        bytes: 64,
        status: 0,
    })
    .to_bytes();
    srv.send(&[(false, info)]);
    let mut acked = Vec::new();
    while !acked.contains(&answer_msg_id) {
        acked.extend(srv.packet().await.acks());
    }
}

/// A ping unanswered within the pong timeout ends the connection: the
/// written request gets `NoAnswer`, and the update manager is told.
#[tokio::test(start_paused = true)]
async fn missing_pong_ends_the_connection() {
    let (mut h, mut srv) = Harness::start_with(init(), &[Writes::Deliver], pinging(8)).await;
    srv.handshake().await;
    let mut pushes = h.pushes.take().unwrap();
    let call = h.invoke(get_config());
    srv.request().await;
    srv.ping(Duration::from_secs(30)).await;
    let result = timeout(Duration::from_secs(30), call)
        .await
        .expect("the pong timeout ends the call")
        .unwrap();
    assert!(matches!(result, Err(SenderError::NoAnswer)), "{result:?}");
    assert_eq!(timeout(WAIT, pushes.recv()).await, Ok(Some(too_long())));
}

/// Acks that went out in a packet Telegram ignored are sent again.
#[tokio::test(start_paused = true)]
async fn acks_of_an_ignored_packet_are_sent_again() {
    let (_h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let pushed = srv.send(&[(true, update(1))])[0];
    let ignored = loop {
        let packet = srv.packet().await;
        if packet.acks().contains(&pushed) {
            break packet.bad_msg_id();
        }
    };
    srv.bad_server_salt(ignored, 0x5a1f);
    let again = srv.packet().await;
    assert_eq!(again.salt, 0x5a1f);
    assert!(again.acks().contains(&pushed), "{again:?}");
}

/// While the full pushes channel pauses reads, pings still go out and the
/// unreadable pongs do not end the connection.
#[tokio::test(start_paused = true)]
async fn pings_continue_while_reads_are_paused() {
    let (mut h, mut srv) = Harness::start_with(init(), &[Writes::Deliver], pinging(2)).await;
    srv.handshake().await;
    let mut pushes = h.pushes.take().unwrap();
    let full = srv.send(&[(true, update(1)), (true, update(2)), (true, update(3))]);
    let mut acked = Vec::new();
    while !full.iter().all(|id| acked.contains(id)) {
        acked.extend(srv.packet().await.acks());
    }
    let taken = srv.frames_taken();

    // Two ping intervals, well past one pong timeout, with reads paused.
    let first = srv.ping(Duration::from_secs(30)).await;
    let second = srv.ping(Duration::from_secs(30)).await;
    srv.send(&[(false, pong(&first)), (false, pong(&second))]);
    assert_eq!(srv.frames_taken(), taken, "reads stay paused");

    for n in 1..=3 {
        let push = timeout(WAIT, pushes.recv()).await.unwrap().unwrap();
        assert_eq!(push, update(n));
    }
    let call = h.invoke(get_config());
    let packet = srv.request().await;
    srv.answer(packet.requests()[0].msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
}

/// While the full pushes channel pauses reads, the init call's answer may
/// sit unread: its deadline is extended, not fired.
#[tokio::test(start_paused = true)]
async fn init_deadline_waits_while_reads_are_paused() {
    // A paused connection never sees a close, so the first one ends on a
    // failed write once the pushes channel is full.
    let (mut h, mut srv) = Harness::start(init(), &[Writes::Gated, Writes::Deliver], 2).await;
    srv.handshake().await;
    let mut pushes = h.pushes.take().unwrap();
    srv.write_attempt().await; // the ack for the init answer, held
    srv.send(&[(true, update(1)), (true, update(2)), (true, update(3))]);
    timeout(WAIT, async {
        while srv.frames_taken() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the pushes are read");
    srv.refuse_write();

    let call = h.invoke(get_config());
    let mut fresh = h.next_conn().await;
    let init_msg = fresh.init_packet().await;
    fresh.answer(init_msg.msg_id, &ANSWER);
    // Three init timeouts with the answer unread.
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert_eq!(fresh.frames_taken(), 0, "reads stay paused");

    while let Ok(Some(_)) = timeout(WAIT, pushes.recv()).await {}
    let packet = fresh.request().await;
    fresh.answer(packet.requests()[0].msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
}

/// An `rpc_error` answer to the connection's init call.
fn refuse_init(srv: &mut Server, init_msg_id: i64, code: i32, message: &str) {
    let error = tl::enums::RpcError::Error(tl::types::RpcError {
        error_code: code,
        error_message: message.into(),
    });
    srv.answer(init_msg_id, &error.to_bytes());
}

/// Stands in for the update manager: answers every push with a request,
/// as Task 5 answers `updatesTooLong` with `getDifference`.
fn eager_consumer(h: &mut Harness) -> JoinHandle<()> {
    let mut pushes = h.pushes.take().unwrap();
    let sender = Arc::clone(&h.sender);
    tokio::spawn(async move {
        while pushes.recv().await.is_some() {
            let _ = sender.invoke(get_config()).await;
        }
    })
}

/// A permanent init refusal (a mistyped api_id) costs one connection: it
/// starts no catch-up, and later requests fail without connecting (P7).
#[tokio::test(start_paused = true)]
async fn refused_init_latches_without_a_reconnect_loop() {
    let (mut h, mut srv) = Harness::start(init(), &[Writes::Deliver], 8).await;
    let consumer = eager_consumer(&mut h);
    let call = h.invoke(get_config());
    let init_msg = srv.init_packet().await;
    refuse_init(&mut srv, init_msg.msg_id, 400, "API_ID_INVALID");
    let result = outcome(call).await;
    assert!(
        matches!(&result, Err(SenderError::InitRefused { code: 400, .. })),
        "{result:?}"
    );

    tokio::time::sleep(Duration::from_secs(600)).await;
    let result = outcome(h.invoke(get_config())).await;
    assert!(
        matches!(&result, Err(SenderError::InitRefused { code: 400, message }) if message == "API_ID_INVALID"),
        "{result:?}"
    );
    assert!(h.conns.try_recv().is_err(), "no reconnect");
    assert_eq!(h.opens(), [true], "one connection in all");
    consumer.abort();
}

/// A 420 `FLOOD_WAIT_X` answer to the init call holds for X seconds, then
/// the next request connects again.
#[tokio::test(start_paused = true)]
async fn flood_wait_on_init_latches_for_its_seconds() {
    let (mut h, mut srv) = Harness::start(init(), &[Writes::Deliver], 8).await;
    let call = h.invoke(get_config());
    let init_msg = srv.init_packet().await;
    refuse_init(&mut srv, init_msg.msg_id, 420, "FLOOD_WAIT_30");
    assert!(matches!(
        outcome(call).await,
        Err(SenderError::InitRefused { code: 420, .. })
    ));

    tokio::time::sleep(Duration::from_secs(29)).await;
    assert!(matches!(
        outcome(h.invoke(get_config())).await,
        Err(SenderError::InitRefused { code: 420, .. })
    ));
    assert!(h.conns.try_recv().is_err(), "no connection while it holds");

    tokio::time::sleep(Duration::from_secs(2)).await;
    let call = h.invoke(get_config());
    let mut fresh = h.ready_conn().await;
    let packet = fresh.request().await;
    fresh.answer(packet.requests()[0].msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
}

/// Transport −429 (too many connections from this address) delays each
/// next connection: 1 s, doubling, capped at 60 s; a completed init resets
/// it. No catch-up push is made for a connection that never initialized.
#[tokio::test(start_paused = true)]
async fn minus_429_backs_off_reconnects() {
    let (mut h, mut srv) = Harness::start(init(), &[Writes::Deliver], 8).await;
    let mut pushes = h.pushes.take().unwrap();
    // Each request opens the next connection, which the next round fails.
    let mut call = h.invoke(get_config());
    for expected in [1, 2, 4, 8, 16, 32, 60, 60] {
        srv.init_packet().await;
        // The request now waits behind the init call on this connection.
        srv.no_request().await;
        srv.fail(TransportError::Status(429));
        assert!(matches!(outcome(call).await, Err(SenderError::NotSent)));
        let failed_at = tokio::time::Instant::now();
        call = h.invoke(get_config());
        srv = timeout(Duration::from_secs(120), h.conns.recv())
            .await
            .expect("the sender reconnects")
            .unwrap();
        assert_eq!(
            failed_at.elapsed().as_secs(),
            expected,
            "the delay before the next connection"
        );
        assert!(pushes.try_recv().is_err(), "no catch-up before init");
    }
    // A completed init resets the backoff: the next −429 waits 1 s again.
    srv.handshake().await;
    let packet = srv.request().await;
    srv.answer(packet.requests()[0].msg_id, &ANSWER);
    assert_eq!(outcome(call).await.unwrap(), answered());
    srv.close();
    assert_eq!(timeout(WAIT, pushes.recv()).await, Ok(Some(too_long())));
    let started = tokio::time::Instant::now();
    let call = h.invoke(get_config());
    let mut srv = h.next_conn().await;
    assert_eq!(started.elapsed().as_secs(), 0, "no delay after a good init");
    srv.init_packet().await;
    srv.no_request().await;
    srv.fail(TransportError::Status(429));
    assert!(matches!(outcome(call).await, Err(SenderError::NotSent)));
    let failed_at = tokio::time::Instant::now();
    let _call = h.invoke(get_config());
    h.next_conn().await;
    assert_eq!(failed_at.elapsed().as_secs(), 1, "the backoff starts over");
}

/// How a connection fails before its init call completes.
#[derive(Clone, Copy, Debug)]
enum PreInit {
    Timeout,
    ServerError,
    Closed,
}

/// Fails the connection `srv` before init the way `kind` says; the request
/// waiting behind the init call ends.
async fn fail_before_init(
    srv: &mut Server,
    kind: PreInit,
    call: JoinHandle<Result<RpcOutcome, SenderError>>,
) {
    let init_msg = srv.init_packet().await;
    srv.no_request().await;
    match kind {
        PreInit::Timeout => {}
        PreInit::ServerError => refuse_init(srv, init_msg.msg_id, 500, "INTERNAL"),
        PreInit::Closed => srv.close(),
    }
    let result = timeout(Duration::from_secs(60), call)
        .await
        .expect("the call ends")
        .unwrap();
    assert!(
        matches!(
            result,
            Err(SenderError::NotSent | SenderError::InitRefused { code: 500, .. })
        ),
        "{kind:?}: {result:?}"
    );
}

/// An init timeout, a 5xx answer to the init call and a socket closed
/// before init each delay the next connection, 1 s then 2 s; none of them
/// pushes a catch-up.
#[tokio::test(start_paused = true)]
async fn every_pre_init_failure_backs_off() {
    for kind in [PreInit::Timeout, PreInit::ServerError, PreInit::Closed] {
        let (mut h, mut srv) = Harness::start(init(), &[Writes::Deliver], 8).await;
        let mut pushes = h.pushes.take().unwrap();
        let mut call = h.invoke(get_config());
        for expected in [1, 2] {
            fail_before_init(&mut srv, kind, call).await;
            let failed_at = tokio::time::Instant::now();
            call = h.invoke(get_config());
            srv = timeout(Duration::from_secs(120), h.conns.recv())
                .await
                .expect("the sender reconnects")
                .unwrap();
            assert_eq!(failed_at.elapsed().as_secs(), expected, "{kind:?}");
        }
        assert!(pushes.try_recv().is_err(), "{kind:?}: no catch-up");
    }
}

/// A failed connect backs off the next one too.
#[tokio::test(start_paused = true)]
async fn failed_connect_backs_off() {
    let (mut h, mut srv) = Harness::ready(init(), &[Writes::Deliver], 8).await;
    let mut pushes = h.pushes.take().unwrap();
    srv.close();
    // The catch-up push shows the connection has ended.
    assert_eq!(timeout(WAIT, pushes.recv()).await, Ok(Some(too_long())));
    *h.script.failing_opens.lock() = 2;
    let started = tokio::time::Instant::now();
    assert!(matches!(
        outcome(h.invoke(get_config())).await,
        Err(SenderError::NotSent)
    ));
    assert_eq!(started.elapsed().as_secs(), 0, "after a good init: at once");
    let failed_at = tokio::time::Instant::now();
    assert!(matches!(
        outcome(h.invoke(get_config())).await,
        Err(SenderError::NotSent)
    ));
    assert_eq!(failed_at.elapsed().as_secs(), 1);
    let failed_at = tokio::time::Instant::now();
    let _call = h.invoke(get_config());
    h.next_conn().await;
    assert_eq!(failed_at.elapsed().as_secs(), 2);
}

/// A FLOOD_WAIT past the clock's range keeps the refusal until the sender
/// is rebuilt, instead of overflowing.
#[tokio::test(start_paused = true)]
async fn huge_flood_wait_latches_for_good() {
    let (mut h, mut srv) = Harness::start(init(), &[Writes::Deliver], 8).await;
    let call = h.invoke(get_config());
    let init_msg = srv.init_packet().await;
    refuse_init(
        &mut srv,
        init_msg.msg_id,
        420,
        "FLOOD_WAIT_18446744073709551615",
    );
    assert!(matches!(
        outcome(call).await,
        Err(SenderError::InitRefused { code: 420, .. })
    ));
    tokio::time::sleep(Duration::from_secs(86_400)).await;
    assert!(matches!(
        outcome(h.invoke(get_config())).await,
        Err(SenderError::InitRefused { code: 420, .. })
    ));
    assert!(h.conns.try_recv().is_err(), "no reconnect");
}

/// The login over MTP frames: a [`crate::login::Dial`] whose senders run
/// on the scripted transport, so every request and answer below crosses
/// the real message layer (msg_ids, seqnos, `rpc_result`, pushes).
mod login_framing {
    use super::*;
    use crate::SessionStore;
    use crate::login::{self, Dial, LoginParams, LoginState, Method};
    use crate::testkit::{login_token, login_token_success, update_login_token};

    #[derive(Clone)]
    struct FramedDial(Arc<Script>);

    impl Dial for FramedDial {
        type Link = Sender;

        async fn dial(
            &self,
            dc: i32,
            key: Option<AuthKey>,
            logged_in: bool,
        ) -> Result<(Sender, Pushes, Option<AuthKey>), SenderError> {
            // The scripted transport makes no key: hand one over as new.
            let new = key.is_none();
            let key = key.unwrap_or(AuthKey([dc as u8; 256]));
            let mut init = init();
            init.logged_in = logged_in;
            let (sender, pushes, _) = Sender::start(
                Scripted(Arc::clone(&self.0)),
                super::dc(),
                Some(key.clone()),
                init,
                tuning(8),
            )
            .await?;
            Ok((sender, pushes, new.then_some(key)))
        }
    }

    fn later() -> i32 {
        i32::try_from(now_unix().0 + 600).unwrap()
    }

    /// QR over frames: export → token; the sender's synthetic
    /// `updatesTooLong` after a lost connection is drained and dropped;
    /// the renewal reconnects; a scan push wakes the flow, and the
    /// re-export's `loginTokenSuccess` is recorded.
    #[tokio::test(start_paused = true)]
    async fn qr_login_over_mtp_frames() {
        let (conns_tx, mut conns) = mpsc::unbounded_channel();
        let script = Arc::new(Script {
            modes: Mutex::new(VecDeque::new()),
            conns: conns_tx,
            opens: Mutex::new(Vec::new()),
            init_call: init().init_call(),
            failing_opens: Mutex::new(0),
        });
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::open(tmp.path()).unwrap();
        let params = LoginParams {
            api_id: 1,
            api_hash: "h".into(),
            phone: None,
        };
        let h = login::start(store, FramedDial(Arc::clone(&script)), params, Method::Qr)
            .await
            .unwrap();
        let export = tl::functions::auth::ExportLoginToken {
            api_id: 1,
            api_hash: "h".into(),
            except_ids: Vec::new(),
        }
        .to_bytes();

        let mut srv = timeout(WAIT, conns.recv()).await.unwrap().unwrap();
        srv.handshake().await;
        let req = srv.request().await.requests()[0].clone();
        assert_eq!(
            req.body, export,
            "the request crosses the frames as written"
        );
        srv.answer(req.msg_id, &login_token(b"one", later()));
        let first = timeout(WAIT, h.settled()).await.unwrap();
        assert!(matches!(first, LoginState::Qr { .. }), "{first:?}");

        // The connection drops: the sender pushes updatesTooLong, which
        // the login drops. The renewal (30 s on the paused clock) opens a
        // fresh connection.
        srv.close();
        let mut srv = timeout(Duration::from_secs(60), conns.recv())
            .await
            .unwrap()
            .unwrap();
        srv.handshake().await;
        let req = srv.request().await.requests()[0].clone();
        assert_eq!(req.body, export);
        srv.answer(req.msg_id, &login_token(b"two", later()));
        let second = timeout(WAIT, h.changed()).await.unwrap();
        assert!(matches!(second, LoginState::Qr { .. }));
        assert_ne!(first, second);

        // The scan: Telegram's push wakes the flow before the token expires.
        srv.send(&[(true, update_login_token())]);
        let req = srv.request().await.requests()[0].clone();
        assert_eq!(req.body, export);
        srv.answer(req.msg_id, &login_token_success(42, None));
        let end = timeout(WAIT, h.changed()).await.unwrap();
        assert_eq!(end, LoginState::LoggedIn { user_id: 42 });
        drop(h);
    }
}

/// An account on real senders over the scripted transport: a lost
/// connection and a `new_session_created` each make the sender push
/// `updatesTooLong`, the update manager asks `getDifference`, and that
/// call reconnects and brings back what was missed.
mod account_framing {
    use super::*;
    use crate::account::{Account, AccountSpec, AccountState, Connection, Network, Registry};
    use crate::rpc::BoxFuture;
    use crate::testkit::{difference, difference_empty, ids, private_message};
    use crate::{
        Admission, Batch, BatchSink, IdsError, Peer, PeerKind, SessionStore, UpdateIds, UpdateState,
    };

    struct FramedNet(Arc<Script>);

    impl Network for FramedNet {
        fn connect(
            &self,
            _dc: DcOption,
            key: Option<AuthKey>,
            media: bool,
        ) -> BoxFuture<'_, Result<Connection, SenderError>> {
            Box::pin(async move {
                let mut init = init();
                init.logged_in = true;
                init.without_updates = media;
                let key = key.unwrap_or(AuthKey([4; 256]));
                let (sender, pushes, _) = Sender::start(
                    Scripted(Arc::clone(&self.0)),
                    dc(),
                    Some(key),
                    init,
                    tuning(8),
                )
                .await?;
                Ok(Connection {
                    session: Arc::new(sender),
                    pushes,
                    new_key: None,
                })
            })
        }
    }

    /// Counts the updates it admits.
    #[derive(Default)]
    struct Count(AtomicUsize);

    impl BatchSink for Count {
        fn admit(
            &self,
            batch: &Batch,
            _ids: &mut UpdateIds,
            _store: &SessionStore,
        ) -> Result<Admission, IdsError> {
            self.0.fetch_add(batch.updates.len(), Ordering::SeqCst);
            Ok(Admission::Admitted)
        }
    }

    /// Answers the next request, which must be `updates.getDifference`.
    async fn answer_difference(srv: &mut Server, answer: &[u8]) {
        let req = srv.request().await.requests()[0].clone();
        assert_eq!(le_u32(&req.body), ids::GET_DIFFERENCE, "a getDifference");
        srv.answer(req.msg_id, answer);
    }

    async fn admitted(count: &Count, n: usize) {
        timeout(WAIT, async {
            while count.0.load(Ordering::SeqCst) < n {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the update is admitted");
    }

    #[tokio::test(start_paused = true)]
    async fn lost_connection_and_new_session_catch_up() {
        let (conns_tx, mut conns) = mpsc::unbounded_channel();
        let script = Arc::new(Script {
            modes: Mutex::new(VecDeque::new()),
            conns: conns_tx,
            opens: Mutex::new(Vec::new()),
            init_call: init().init_call(),
            failing_opens: Mutex::new(0),
        });
        let tmp = tempfile::tempdir().unwrap();
        {
            let store = SessionStore::open(tmp.path()).unwrap();
            store.set_home_key(2, &AuthKey([4; 256])).unwrap();
            store
                .record_login(Peer {
                    id: 42,
                    kind: PeerKind::User {
                        bot: Some(false),
                        is_self: Some(true),
                    },
                    access_hash: Some(1),
                })
                .unwrap();
            store
                .set_update_state(UpdateState {
                    pts: 10,
                    qts: 0,
                    seq: 1,
                    date: 1,
                })
                .unwrap();
        }
        let count = Arc::new(Count::default());
        let account = Account::start(
            AccountSpec::new("main", tmp.path()),
            Arc::clone(&count) as Arc<dyn BatchSink>,
            Arc::new(FramedNet(Arc::clone(&script))),
            Arc::new(Registry::default()),
        );
        let mut srv = timeout(WAIT, conns.recv()).await.unwrap().unwrap();
        srv.handshake().await;
        answer_difference(&mut srv, &difference_empty(1, 1)).await;
        let mut state = account.watch();
        timeout(WAIT, state.wait_for(|s| *s == AccountState::Running))
            .await
            .unwrap()
            .unwrap();

        // The connection drops while a message is sent: the catch-up
        // reconnects and fetches it.
        srv.close();
        let mut srv = timeout(WAIT, conns.recv())
            .await
            .expect("the catch-up reconnects")
            .unwrap();
        srv.handshake().await;
        answer_difference(
            &mut srv,
            &difference(&[private_message(1, 100, "a")], 11, 1, 1),
        )
        .await;
        admitted(&count, 1).await;

        // Telegram replaced the session: the same connection catches up.
        let ping = account.rpc();
        let call =
            tokio::spawn(async move { ping.invoke(crate::rpc::Route::Main, get_config()).await });
        let req = srv.request().await.requests()[0].clone();
        srv.new_session(req.msg_id, 0x5e55);
        srv.answer(req.msg_id, &ANSWER);
        assert!(call.await.unwrap().is_ok());
        answer_difference(
            &mut srv,
            &difference(&[private_message(2, 100, "b")], 12, 1, 1),
        )
        .await;
        admitted(&count, 2).await;
        assert_eq!(script.opens.lock().len(), 2, "one reconnect in all");
        account.close().await;
    }
}
