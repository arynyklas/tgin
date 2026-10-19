//! The sender's seam: a connection that moves whole MTProto packets.
//!
//! A packet, as the [`Sender`](crate::Sender) sees it, is the plaintext of
//! an encrypted MTProto message: `salt:long session_id:long`, then one
//! message (`msg_id:long seqno:int bytes:int body`, possibly a
//! `msg_container`), then, on incoming packets, the padding. A
//! [`Transport`] owns everything below that: the MTProto 2.0 cipher with
//! the auth key, the framing, the socket. The production transport is
//! [`TcpTransport`]; the sender's tests use a scripted one that speaks
//! plain packets.
//!
//! Both halves are cancel safe: the sender drops a pending `recv` or
//! `flush` future whenever another event wins, and the next call carries
//! on where the last one stopped.

use std::io;
use std::mem;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use grammers_crypto::{DequeBuffer, decrypt_data_v2, encrypt_data_v2};
use grammers_mtproto::authentication;
use grammers_mtproto::mtp::{Deserialization, Mtp, Plain};
use grammers_mtproto::transport::{Intermediate, Transport as _};
use grammers_tl_types::{self as tl, Deserializable, Serializable};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{AuthKey, DcOption};

/// The TCP connect deadline.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The deadline for the whole auth key exchange.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest frame accepted from Telegram. Its largest answers
/// (`upload.getFile`, 1 MiB parts) stay well below.
const MAX_FRAME: usize = 2 * 1024 * 1024;
/// Bytes put in front of a packet: `auth_key_id`, `msg_key`, the
/// intermediate length and its one-time tag.
const FRONT: usize = 8 + 16 + 4 + 4;
/// `auth_key_id` and `msg_key`, in front of the ciphertext.
const CIPHER_HEADER: usize = 8 + 16;

/// Why a connection failed. Every variant ends the connection.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("connection: {0}")]
    Io(#[from] io::Error),
    #[error("connection closed")]
    Closed,
    #[error("connect timed out")]
    Timeout,
    /// A transport error: Telegram's negative length. 404 means it does not
    /// know the auth key; 429, too many connections from this address.
    #[error("transport error -{0}")]
    Status(u32),
    /// A frame that is not a valid MTProto packet.
    #[error("bad frame: {0}")]
    Frame(&'static str),
    /// The auth key exchange failed.
    #[error("auth key exchange: {0}")]
    Handshake(String),
}

/// A key the transport made while opening a connection.
pub struct GeneratedKey {
    pub key: AuthKey,
    /// The first server salt, from the key exchange.
    pub first_salt: i64,
    /// Telegram's clock minus ours, in seconds.
    pub time_offset: i32,
}

/// Leaves out the salt; the key's own `Debug` hides it.
impl std::fmt::Debug for GeneratedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedKey")
            .field("key", &self.key)
            .field("time_offset", &self.time_offset)
            .finish_non_exhaustive()
    }
}

/// An open connection.
pub struct Opened<R, W> {
    pub reader: R,
    pub writer: W,
    /// The key made on this connection, when [`Transport::open`] had none.
    pub generated: Option<GeneratedKey>,
}

/// Opens connections to a DC. One value serves every connection a
/// [`Sender`](crate::Sender) makes.
pub trait Transport: Send + Sync + 'static {
    type Reader: PacketReader;
    type Writer: PacketWriter;

    /// A new connection to `dc`, encrypted with `key`. With `key: None` the
    /// transport first makes a new auth key on the connection and returns
    /// it in [`Opened::generated`].
    fn open(
        &self,
        dc: &DcOption,
        key: Option<&AuthKey>,
    ) -> impl Future<Output = Result<Opened<Self::Reader, Self::Writer>, TransportError>> + Send;
}

/// The incoming half of a connection.
pub trait PacketReader: Send + 'static {
    /// The next incoming packet's plaintext. Cancel safe.
    fn recv(&mut self) -> impl Future<Output = Result<Bytes, TransportError>> + Send;
}

/// The outgoing half of a connection. It holds at most one packet.
pub trait PacketWriter: Send + 'static {
    /// Takes the next packet's plaintext. Called only when
    /// [`is_flushed`](Self::is_flushed).
    fn queue(&mut self, packet: &[u8]);

    /// Hands the queued packet's remaining bytes to the connection. `Ok`
    /// means every byte of it was handed over. Cancel safe.
    fn flush(&mut self) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// Nothing queued, or every byte of the queued packet handed over.
    /// While this is false, the peer cannot have the whole packet.
    fn is_flushed(&self) -> bool;
}

/// The production transport: MTProto's intermediate framing over TCP to
/// the DC's IPv4 address, with the MTProto 2.0 cipher.
///
/// A sender owns one. It runs one auth key exchange at a time: a second
/// waits until the first one's CPU-bound steps have finished, even ones a
/// timeout abandoned, so forged answers cannot pile up blocking threads.
#[derive(Clone)]
pub struct TcpTransport {
    exchanges: Arc<Semaphore>,
}

impl TcpTransport {
    #[must_use]
    pub fn new() -> Self {
        Self {
            exchanges: Arc::new(Semaphore::new(1)),
        }
    }
}

impl Default for TcpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for TcpTransport {
    type Reader = TcpReader;
    type Writer = TcpWriter;

    async fn open(
        &self,
        dc: &DcOption,
        key: Option<&AuthKey>,
    ) -> Result<Opened<TcpReader, TcpWriter>, TransportError> {
        let addr = SocketAddr::V4(dc.addr.ipv4);
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| TransportError::Timeout)??;
        stream.set_nodelay(true)?;
        let (read, write) = stream.into_split();
        let mut frames = Frames::new(read);
        let mut out = FrameWriter::new(write);
        let (key, generated) = match key {
            Some(key) => (key.clone(), None),
            None => {
                let generated = tokio::time::timeout(
                    HANDSHAKE_TIMEOUT,
                    guarded_exchange(&self.exchanges, &mut frames, &mut out),
                )
                .await
                .map_err(|_| TransportError::Timeout)??;
                (generated.key.clone(), Some(generated))
            }
        };
        let cipher = grammers_crypto::AuthKey::from_bytes(key.0);
        Ok(Opened {
            reader: TcpReader {
                frames,
                key: cipher.clone(),
            },
            writer: TcpWriter { out, key: cipher },
            generated,
        })
    }
}

/// [`exchange_key`] once `exchanges` lets it run: one at a time, and the
/// permit is held by every step, the detached ones included.
async fn guarded_exchange<R, W>(
    exchanges: &Arc<Semaphore>,
    frames: &mut Frames<R>,
    out: &mut FrameWriter<W>,
) -> Result<GeneratedKey, TransportError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let permit = Arc::clone(exchanges)
        .acquire_owned()
        .await
        .map_err(|_| failed("the key exchange guard is closed"))?;
    exchange_key(frames, out, Arc::new(permit)).await
}

/// [`TcpTransport`]'s incoming half: decrypts each frame in place.
pub struct TcpReader {
    frames: Frames<OwnedReadHalf>,
    key: grammers_crypto::AuthKey,
}

impl PacketReader for TcpReader {
    async fn recv(&mut self) -> Result<Bytes, TransportError> {
        let mut frame = self.frames.next().await?;
        if frame.len() >= 8 && frame[..8] == [0; 8] {
            return Err(TransportError::Frame(
                "unencrypted message on an encrypted connection",
            ));
        }
        decrypt_data_v2(&mut frame, &self.key).map_err(|e| match e {
            grammers_crypto::Error::InvalidBuffer => TransportError::Frame("ciphertext length"),
            grammers_crypto::Error::AuthKeyMismatch => TransportError::Frame("another auth key"),
            grammers_crypto::Error::MessageKeyMismatch => TransportError::Frame("msg_key mismatch"),
        })?;
        Ok(frame.freeze().slice(CIPHER_HEADER..))
    }
}

/// [`TcpTransport`]'s outgoing half: encrypts and frames each packet.
pub struct TcpWriter {
    out: FrameWriter<OwnedWriteHalf>,
    key: grammers_crypto::AuthKey,
}

impl PacketWriter for TcpWriter {
    fn queue(&mut self, packet: &[u8]) {
        let buf = self.out.start();
        buf.extend(packet);
        encrypt_data_v2(buf, &self.key);
        self.out.finish();
    }

    async fn flush(&mut self) -> Result<(), TransportError> {
        self.out.flush().await
    }

    fn is_flushed(&self) -> bool {
        self.out.is_flushed()
    }
}

/// Intermediate frames in: `len:int` then `len` bytes. A 4-byte frame is a
/// transport error code.
struct Frames<R> {
    read: R,
    head: [u8; 4],
    head_len: usize,
    body: BytesMut,
    body_len: usize,
}

impl<R: AsyncRead + Unpin> Frames<R> {
    fn new(read: R) -> Self {
        Self {
            read,
            head: [0; 4],
            head_len: 0,
            body: BytesMut::new(),
            body_len: 0,
        }
    }

    /// The next frame's payload. Cancel safe: progress lives in `self`.
    async fn next(&mut self) -> Result<BytesMut, TransportError> {
        while self.head_len < 4 {
            let n = self.read.read(&mut self.head[self.head_len..]).await?;
            if n == 0 {
                return Err(TransportError::Closed);
            }
            self.head_len += n;
        }
        let len = usize::try_from(i32::from_le_bytes(self.head))
            .ok()
            .filter(|len| (4..=MAX_FRAME).contains(len) && len.is_multiple_of(4))
            .ok_or(TransportError::Frame("frame length"))?;
        if self.body.len() != len {
            self.body = BytesMut::zeroed(len);
            self.body_len = 0;
        }
        while self.body_len < len {
            let n = self.read.read(&mut self.body[self.body_len..]).await?;
            if n == 0 {
                return Err(TransportError::Closed);
            }
            self.body_len += n;
        }
        self.head_len = 0;
        self.body_len = 0;
        let body = mem::take(&mut self.body);
        if len == 4 {
            let code = i32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            return Err(TransportError::Status(code.unsigned_abs()));
        }
        Ok(body)
    }
}

/// Intermediate frames out, one at a time.
struct FrameWriter<W> {
    write: W,
    framing: Intermediate,
    buf: DequeBuffer<u8>,
    head: usize,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    fn new(write: W) -> Self {
        Self {
            write,
            framing: Intermediate::new(),
            buf: DequeBuffer::with_capacity(0, FRONT),
            head: 0,
        }
    }

    /// The cleared buffer for the next frame's payload.
    fn start(&mut self) -> &mut DequeBuffer<u8> {
        self.buf.clear();
        self.head = 0;
        &mut self.buf
    }

    /// Frames the payload written since [`start`](Self::start).
    fn finish(&mut self) {
        self.framing.pack(&mut self.buf);
    }

    fn is_flushed(&self) -> bool {
        self.head >= self.buf.len()
    }

    /// Cancel safe: `head` only moves past bytes the socket took.
    async fn flush(&mut self) -> Result<(), TransportError> {
        while self.head < self.buf.len() {
            let n = self.write.write(&self.buf[self.head..]).await?;
            if n == 0 {
                return Err(TransportError::Closed);
            }
            self.head += n;
        }
        Ok(())
    }
}

/// The smallest `server_DH_inner_data`: constructor, nonce, server_nonce,
/// g, a 2048-bit dh_prime and g_a (each a 4-byte TL length and 256 bytes),
/// server_time.
const MIN_DH_INNER: usize = 4 + 16 + 16 + 4 + (4 + 256) + (4 + 256) + 4;
/// `server_DH_params_ok`'s `encrypted_answer` starts with a SHA-1.
const MIN_ENCRYPTED_ANSWER: usize = 20 + MIN_DH_INNER;
/// Telegram's pq is a 64-bit product of two 32-bit primes; anything at or
/// below this cannot be one.
const MIN_PQ: u64 = 1 << 32;

/// grammers' auth key exchange (`authentication` steps 1–3) over plain
/// messages on this connection.
///
/// The answers are unencrypted, so an on-path attacker can forge them.
/// grammers then panics on a short `encrypted_answer`, loops for ever
/// factorizing a tiny pq and runs for a very long time on a prime one. So
/// the answers are checked first, and each CPU-bound step runs in
/// `spawn_blocking` under `catch_unwind`, holding `permit`; the caller
/// bounds the whole exchange with a timeout.
async fn exchange_key<R, W>(
    frames: &mut Frames<R>,
    out: &mut FrameWriter<W>,
    permit: Arc<OwnedSemaphorePermit>,
) -> Result<GeneratedKey, TransportError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (request, step) = authentication::step1().map_err(failed)?;
    let answer = plain_call(frames, out, &request.to_bytes()).await?;
    let res_pq = tl::enums::ResPq::from_bytes(&answer).map_err(failed)?;
    check_pq(&res_pq)?;
    let (request, step) = blocking(&permit, move || authentication::step2(step, res_pq)).await?;
    let answer = plain_call(frames, out, &request.to_bytes()).await?;
    let dh_params = tl::enums::ServerDhParams::from_bytes(&answer).map_err(failed)?;
    check_dh_params(&dh_params)?;
    let (request, step) = blocking(&permit, move || authentication::step3(step, dh_params)).await?;
    let answer = plain_call(frames, out, &request.to_bytes()).await?;
    let dh_answer = tl::enums::SetClientDhParamsAnswer::from_bytes(&answer).map_err(failed)?;
    let done = blocking(&permit, move || authentication::create_key(step, dh_answer)).await?;
    Ok(GeneratedKey {
        key: AuthKey(done.auth_key),
        first_salt: done.first_salt,
        time_offset: done.time_offset,
    })
}

fn failed(e: impl std::fmt::Display) -> TransportError {
    TransportError::Handshake(e.to_string())
}

/// `res_pq`'s pq must be an odd composite 64-bit number above 2^32. A
/// composite one has a prime factor below 2^32, which grammers'
/// Pollard–Brent finds in milliseconds; a prime one would keep it busy for
/// a very long time.
fn check_pq(res_pq: &tl::enums::ResPq) -> Result<(), TransportError> {
    let tl::enums::ResPq::Pq(res_pq) = res_pq;
    let pq = <[u8; 8]>::try_from(res_pq.pq.as_slice())
        .map(u64::from_be_bytes)
        .map_err(|_| TransportError::Handshake("pq is not 8 bytes".to_owned()))?;
    if pq <= MIN_PQ || pq.is_multiple_of(2) || is_prime(pq) {
        return Err(TransportError::Handshake(
            "pq is not a product of two large primes".to_owned(),
        ));
    }
    Ok(())
}

/// Deterministic Miller–Rabin for 64-bit numbers: the bases 2 to 37 decide
/// every `n < 3.3 * 10^24`.
fn is_prime(n: u64) -> bool {
    const BASES: [u64; 12] = [2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37];
    fn mul(a: u64, b: u64, m: u64) -> u64 {
        let product = u128::from(a) * u128::from(b) % u128::from(m);
        u64::try_from(product).unwrap_or(0)
    }
    fn pow(mut base: u64, mut exp: u64, m: u64) -> u64 {
        let mut acc = 1 % m;
        base %= m;
        while exp > 0 {
            if exp & 1 == 1 {
                acc = mul(acc, base, m);
            }
            base = mul(base, base, m);
            exp >>= 1;
        }
        acc
    }
    if n < 2 {
        return false;
    }
    for p in BASES {
        if n.is_multiple_of(p) {
            return n == p;
        }
    }
    let shift = (n - 1).trailing_zeros();
    let odd = (n - 1) >> shift;
    'bases: for a in BASES {
        let mut x = pow(a, odd, n);
        if x == 1 || x == n - 1 {
            continue;
        }
        for _ in 1..shift {
            x = mul(x, x, n);
            if x == n - 1 {
                continue 'bases;
            }
        }
        return false;
    }
    true
}

/// `server_DH_params_ok`'s `encrypted_answer` must hold at least the
/// smallest inner data.
fn check_dh_params(params: &tl::enums::ServerDhParams) -> Result<(), TransportError> {
    match params {
        tl::enums::ServerDhParams::Ok(ok) if ok.encrypted_answer.len() < MIN_ENCRYPTED_ANSWER => {
            Err(TransportError::Handshake(format!(
                "encrypted_answer of {} bytes is too short",
                ok.encrypted_answer.len()
            )))
        }
        _ => Ok(()),
    }
}

/// One CPU-bound key exchange step, off the async threads, with a panic
/// turned into an error. The thread holds `permit` until it finishes.
async fn blocking<T, F>(permit: &Arc<OwnedSemaphorePermit>, step: F) -> Result<T, TransportError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, authentication::Error> + Send + 'static,
{
    let permit = Arc::clone(permit);
    let caught = tokio::task::spawn_blocking(move || {
        let caught = std::panic::catch_unwind(AssertUnwindSafe(step));
        drop(permit);
        caught
    })
    .await
    .map_err(|_| TransportError::Handshake("the key exchange step was cancelled".to_owned()))?;
    match caught {
        Ok(result) => result.map_err(failed),
        Err(_) => Err(TransportError::Handshake(
            "the server's answer made the key exchange panic".to_owned(),
        )),
    }
}

/// One unencrypted request and its answer's body.
async fn plain_call<R, W>(
    frames: &mut Frames<R>,
    out: &mut FrameWriter<W>,
    request: &[u8],
) -> Result<Vec<u8>, TransportError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut plain = Plain::new();
    let buf = out.start();
    plain
        .push(buf, request)
        .ok_or(TransportError::Frame("plain request"))?;
    plain.finalize(buf);
    out.finish();
    out.flush().await?;
    let mut frame = frames.next().await?;
    match plain
        .deserialize(&mut frame)
        .map_err(|e| TransportError::Handshake(e.to_string()))?
        .pop()
    {
        Some(Deserialization::RpcResult(answer)) => Ok(answer.body),
        _ => Err(TransportError::Frame("plain answer")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the forged Telegram answers to each plain request.
    enum Forged {
        /// `resPQ` with this pq and Telegram's production key fingerprint.
        ResPq(u64),
        /// `server_DH_params_ok` with this `encrypted_answer`.
        DhOk(Vec<u8>),
    }

    /// Runs `exchange_key` against an on-path forger that answers each
    /// plain request in turn. A spin or a hang fails at 5 s.
    async fn forged_exchange(answers: Vec<Forged>) -> Result<GeneratedKey, TransportError> {
        let (client, server) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(client);
        let mut frames = Frames::new(read);
        let mut out = FrameWriter::new(write);
        let forger = tokio::spawn(async move {
            let (mut read, mut write) = tokio::io::split(server);
            let mut tag = [0; 4];
            read.read_exact(&mut tag).await.ok()?;
            for answer in answers {
                let mut len = [0; 4];
                read.read_exact(&mut len).await.ok()?;
                let mut payload = vec![0; usize::try_from(i32::from_le_bytes(len)).ok()?];
                read.read_exact(&mut payload).await.ok()?;
                // auth_key_id, msg_id, length, then the request.
                let request = &payload[20..];
                let nonce: [u8; 16] = request[4..20].try_into().ok()?;
                let body = match answer {
                    Forged::ResPq(pq) => tl::enums::ResPq::Pq(tl::types::ResPq {
                        nonce,
                        server_nonce: [7; 16],
                        pq: pq.to_be_bytes().to_vec(),
                        server_public_key_fingerprints: vec![-3_414_540_481_677_951_611],
                    })
                    .to_bytes(),
                    Forged::DhOk(encrypted_answer) => {
                        tl::enums::ServerDhParams::Ok(tl::types::ServerDhParamsOk {
                            nonce,
                            server_nonce: request[20..36].try_into().ok()?,
                            encrypted_answer,
                        })
                        .to_bytes()
                    }
                };
                let mut message = 0i64.to_le_bytes().to_vec();
                message.extend_from_slice(&((1_800_000_000i64 << 32) | 1).to_le_bytes());
                message.extend_from_slice(&i32::try_from(body.len()).ok()?.to_le_bytes());
                message.extend_from_slice(&body);
                write
                    .write_all(&i32::try_from(message.len()).ok()?.to_le_bytes())
                    .await
                    .ok()?;
                write.write_all(&message).await.ok()?;
            }
            Some(())
        });
        let exchanges = Arc::new(Semaphore::new(1));
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            guarded_exchange(&exchanges, &mut frames, &mut out),
        )
        .await
        .expect("a forged answer ends the exchange at once");
        forger.abort();
        result
    }

    fn handshake_error(result: Result<GeneratedKey, TransportError>) -> String {
        match result {
            Err(TransportError::Handshake(why)) => why,
            other => panic!("expected a handshake error, got {other:?}"),
        }
    }

    /// A tiny odd pq made grammers' `factorize` loop for ever; an even one
    /// is no product of two primes either.
    #[tokio::test]
    async fn forged_small_or_even_pq_is_refused_without_spinning() {
        for pq in [7, 1 << 40] {
            let why = handshake_error(forged_exchange(vec![Forged::ResPq(pq)]).await);
            assert!(why.contains("pq"), "{pq}: {why}");
        }
    }

    /// An empty `encrypted_answer` made grammers' step 3 panic.
    #[tokio::test]
    async fn forged_empty_encrypted_answer_is_refused() {
        // Telegram's documented sample pq: 1229739323 × 1402015859.
        let answers = vec![
            Forged::ResPq(0x17ED_4894_1A08_F981),
            Forged::DhOk(Vec::new()),
        ];
        let why = handshake_error(forged_exchange(answers).await);
        assert!(why.contains("too short"), "{why}");
    }

    /// A permit for one key exchange step.
    fn permit(exchanges: &Arc<Semaphore>) -> Arc<OwnedSemaphorePermit> {
        Arc::new(Arc::clone(exchanges).try_acquire_owned().unwrap())
    }

    /// A panic in a key exchange step becomes an error, not a crash.
    #[tokio::test]
    async fn a_panicking_step_is_an_error() {
        let exchanges = Arc::new(Semaphore::new(1));
        let result = blocking(
            &permit(&exchanges),
            || -> Result<(), authentication::Error> {
                panic!("a forged answer");
            },
        )
        .await;
        let why = match result {
            Err(TransportError::Handshake(why)) => why,
            other => panic!("{other:?}"),
        };
        assert!(why.contains("panic"), "{why}");
    }

    /// A forged prime pq kept grammers' Pollard–Brent busy for a very long
    /// time; it is refused before grammers sees it.
    #[tokio::test]
    async fn forged_prime_pq_is_refused_in_milliseconds() {
        // 2^64 − 59 and 2^63 − 25, both prime.
        for pq in [18_446_744_073_709_551_557, 9_223_372_036_854_775_783] {
            let started = std::time::Instant::now();
            let why = handshake_error(forged_exchange(vec![Forged::ResPq(pq)]).await);
            assert!(why.contains("pq"), "{pq}: {why}");
            assert!(started.elapsed() < Duration::from_secs(1), "{pq}");
        }
    }

    #[test]
    fn miller_rabin_is_exact_on_hard_cases() {
        for p in [
            2,
            3,
            37,
            1_000_000_007,
            4_294_967_291,
            18_446_744_073_709_551_557,
        ] {
            assert!(is_prime(p), "{p}");
        }
        // Strong pseudoprimes to the first bases, Carmichael numbers, and
        // Telegram's sample pq (1229739323 × 1402015859).
        for c in [
            0,
            1,
            561,
            3_215_031_751,
            2_152_302_898_747,
            3_825_123_056_546_413_051,
            0x17ED_4894_1A08_F981,
            4_294_967_291 * 4_294_967_279,
        ] {
            assert!(!is_prime(c), "{c}");
        }
    }

    /// A detached step (its caller timed out) keeps the permit until its
    /// thread finishes, so no second exchange starts beside it.
    #[tokio::test]
    async fn an_abandoned_step_holds_the_exchange_permit() {
        let exchanges = Arc::new(Semaphore::new(1));
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let held = permit(&exchanges);
        let step = blocking(&held, move || {
            let _ = wait.recv();
            Ok::<_, authentication::Error>(())
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), step)
                .await
                .is_err()
        );
        // The caller gave up: only the detached thread holds the permit now.
        drop(held);
        assert!(
            Arc::clone(&exchanges).try_acquire_owned().is_err(),
            "the detached step still holds the permit"
        );
        release.send(()).unwrap();
        let freed = tokio::time::timeout(
            Duration::from_secs(5),
            Arc::clone(&exchanges).acquire_owned(),
        )
        .await;
        assert!(matches!(freed, Ok(Ok(_))), "released when the step ends");
    }

    /// A second exchange through the same guard waits: it writes nothing
    /// while the first one runs, and starts once it is gone.
    #[tokio::test(start_paused = true)]
    async fn one_key_exchange_at_a_time() {
        let exchanges = Arc::new(Semaphore::new(1));
        let (first_client, _first_server) = tokio::io::duplex(1 << 16);
        let first = {
            let exchanges = Arc::clone(&exchanges);
            tokio::spawn(async move {
                let (read, write) = tokio::io::split(first_client);
                let (mut frames, mut out) = (Frames::new(read), FrameWriter::new(write));
                guarded_exchange(&exchanges, &mut frames, &mut out).await
            })
        };
        tokio::task::yield_now().await;

        let (second_client, mut second_server) = tokio::io::duplex(1 << 16);
        let second = tokio::spawn(async move {
            let (read, write) = tokio::io::split(second_client);
            let (mut frames, mut out) = (Frames::new(read), FrameWriter::new(write));
            guarded_exchange(&exchanges, &mut frames, &mut out).await
        });
        let mut tag = [0; 4];
        let waited =
            tokio::time::timeout(Duration::from_secs(60), second_server.read_exact(&mut tag)).await;
        assert!(
            waited.is_err(),
            "the second exchange wrote while the first ran"
        );

        first.abort();
        let started =
            tokio::time::timeout(Duration::from_secs(5), second_server.read_exact(&mut tag)).await;
        assert!(
            matches!(started, Ok(Ok(_))),
            "it starts once the first is gone"
        );
        second.abort();
    }

    #[test]
    fn generated_key_debug_hides_the_salt() {
        let generated = GeneratedKey {
            key: AuthKey([3; 256]),
            first_salt: 0x0123_4567_89ab_cdef,
            time_offset: -7,
        };
        let shown = format!("{generated:?}");
        assert!(shown.contains("time_offset: -7"), "{shown}");
        for salt in [
            0x0123_4567_89ab_cdef_i64.to_string(),
            "123456789abcdef".to_owned(),
        ] {
            assert!(!shown.to_lowercase().contains(&salt), "{shown}");
        }
        assert!(!shown.contains("first_salt"), "{shown}");
    }

    /// A frame arriving one byte at a time, with the read cancelled after
    /// every byte, still comes out whole.
    #[tokio::test]
    async fn frames_survive_cancelled_reads() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut frames = Frames::new(server);
        let mut wire = 8i32.to_le_bytes().to_vec();
        wire.extend_from_slice(b"abcdefgh");
        let mut got = None;
        for byte in &wire {
            client.write_all(&[*byte]).await.ok();
            // Polls once; a pending read is dropped mid-frame.
            if let Ok(frame) = tokio::time::timeout(Duration::ZERO, frames.next()).await {
                got = Some(frame.map(|f| f.to_vec()).map_err(|e| e.to_string()));
            }
        }
        assert_eq!(got, Some(Ok(b"abcdefgh".to_vec())));
    }

    #[tokio::test]
    async fn status_frame_is_a_transport_error() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut frames = Frames::new(server);
        let mut wire = 4i32.to_le_bytes().to_vec();
        wire.extend_from_slice(&(-404i32).to_le_bytes());
        client.write_all(&wire).await.ok();
        assert!(matches!(
            frames.next().await,
            Err(TransportError::Status(404))
        ));
    }

    /// `TcpWriter`'s framing: the intermediate tag, the length, then the
    /// ciphertext under the key's id.
    #[tokio::test]
    async fn writer_frames_and_encrypts() {
        let (client, server) = tokio::io::duplex(4096);
        let key = grammers_crypto::AuthKey::from_bytes([7; 256]);
        let mut writer = FrameWriter::new(client);
        let packet = [1u8; 48];
        let buf = writer.start();
        buf.extend(&packet[..]);
        encrypt_data_v2(buf, &key);
        writer.finish();
        assert!(!writer.is_flushed());
        writer.flush().await.ok();
        assert!(writer.is_flushed());
        drop(writer);

        let mut server = server;
        let mut wire = Vec::new();
        server.read_to_end(&mut wire).await.ok();
        assert_eq!(&wire[..4], &[0xee; 4], "intermediate tag");
        let len = usize::try_from(i32::from_le_bytes([wire[4], wire[5], wire[6], wire[7]]));
        assert_eq!(len.ok(), Some(wire.len() - 8));
        // Client-side ciphertext: same key id in front.
        assert_eq!(&wire[8..16], &key_id(&key));
    }

    fn key_id(key: &grammers_crypto::AuthKey) -> [u8; 8] {
        // `auth_key_id` is the low 8 bytes of SHA-1(key); encrypt an empty
        // buffer to read it back.
        let mut buf = DequeBuffer::with_capacity(0, CIPHER_HEADER);
        encrypt_data_v2(&mut buf, key);
        let mut id = [0; 8];
        id.copy_from_slice(&buf[..8]);
        id
    }
}
