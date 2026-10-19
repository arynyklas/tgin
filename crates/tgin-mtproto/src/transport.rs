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
pub struct TcpTransport;

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
                let generated =
                    tokio::time::timeout(HANDSHAKE_TIMEOUT, exchange_key(&mut frames, &mut out))
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

/// grammers' auth key exchange (`authentication` steps 1–3) over plain
/// messages on this connection.
async fn exchange_key<R, W>(
    frames: &mut Frames<R>,
    out: &mut FrameWriter<W>,
) -> Result<GeneratedKey, TransportError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    fn failed(e: impl std::fmt::Display) -> TransportError {
        TransportError::Handshake(e.to_string())
    }

    let (request, step) = authentication::step1().map_err(failed)?;
    let answer = plain_call(frames, out, &request.to_bytes()).await?;
    let res_pq = tl::enums::ResPq::from_bytes(&answer).map_err(failed)?;
    let (request, step) = authentication::step2(step, res_pq).map_err(failed)?;
    let answer = plain_call(frames, out, &request.to_bytes()).await?;
    let dh_params = tl::enums::ServerDhParams::from_bytes(&answer).map_err(failed)?;
    let (request, step) = authentication::step3(step, dh_params).map_err(failed)?;
    let answer = plain_call(frames, out, &request.to_bytes()).await?;
    let dh_answer = tl::enums::SetClientDhParamsAnswer::from_bytes(&answer).map_err(failed)?;
    let done = authentication::create_key(step, dh_answer).map_err(failed)?;
    Ok(GeneratedKey {
        key: AuthKey(done.auth_key),
        first_salt: done.first_salt,
        time_offset: done.time_offset,
    })
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
