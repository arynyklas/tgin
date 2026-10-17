//! The gateway streams: 4 MiB and 200 MiB uploads and downloads pass
//! through keeping `Content-Length` both ways, and what tgin buffers stays
//! under one fixed bound ([`BOUND`], 4 MiB) whatever the body size.
//!
//! "Buffered" is measured as the growth of the process's live heap over a
//! transfer, through the shared counting global allocator
//! (`tgin-botapi/tests/support/counting_alloc.rs`): whatever tgin holds of a
//! body is on the heap, hyper's per-connection read buffers (up to 408 KiB
//! each) and queued frames included. The downstream client and the fake
//! Telegram are raw TCP over stack buffers, so they add no heap of their
//! own while bytes flow, and this binary holds this one test, so nothing
//! else allocates meanwhile.

#[path = "../../tgin-botapi/tests/support/counting_alloc.rs"]
mod counting_alloc;

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tgin::app::{App, Boot};
use tgin_runtime::config::MapEnv;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use counting_alloc::Counting;

#[global_allocator]
static GLOBAL: Counting = Counting;

const TOKEN: &str = "123456789:AAAAexampleSecretTokenValue_-xyz";
/// The large transfer, each way.
const SIZE: u64 = 200 * 1024 * 1024;
/// The small transfer, each way: 50× smaller than [`SIZE`].
const SMALL: u64 = 4 * 1024 * 1024;
/// The most any transfer, small or large, may grow the heap by. Measured
/// growth comes in steps of about 418 KB, one hyper read buffer (8 KiB +
/// 400 KiB) pinned per queued frame: 1.26, 1.68, 2.10 and 2.52 MB on
/// Windows, at most 1.26 MB on Linux, the same steps at 4 MiB and at
/// 200 MiB. Four MiB leaves room for three more steps above the largest
/// growth measured (a fourth reaches the bound). One bound for both sizes
/// is the size-independence check: the 50× larger body stays under it,
/// and it is under 2 % of that body.
const BOUND: isize = 4 * 1024 * 1024;
/// Bounds each transfer.
const PATIENCE: Duration = Duration::from_secs(300);
const CHUNK: usize = 16 * 1024;

/// What the fake Telegram saw of uploads.
#[derive(Default)]
struct Seen {
    /// The body bytes of the last upload.
    uploaded: AtomicU64,
    /// Whether the last upload announced its exact length, unchunked.
    framed_by_length: AtomicBool,
}

/// Reads a request or response head into `buf`, returning its length and
/// how many bytes after it `buf` already holds.
async fn read_head(stream: &mut TcpStream, buf: &mut [u8]) -> Option<(usize, usize)> {
    let mut filled = 0;
    loop {
        if let Some(end) = buf[..filled].windows(4).position(|w| w == b"\r\n\r\n") {
            return Some((end, filled - end - 4));
        }
        let n = stream.read(&mut buf[filled..]).await.ok()?;
        if n == 0 {
            return None;
        }
        filled += n;
    }
}

/// The value of the header `name` (lowercase) in `head`.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n").skip(1).find_map(|line| {
        let (n, v) = line.split_once(':')?;
        n.eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// Discards `remaining` bytes of `stream`.
async fn drain(stream: &mut TcpStream, mut remaining: u64, buf: &mut [u8]) -> u64 {
    let mut read = 0;
    while remaining > 0 {
        let want = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let n = stream.read(&mut buf[..want]).await.expect("read");
        assert!(n > 0, "closed early");
        read += n as u64;
        remaining -= n as u64;
    }
    read
}

/// Writes `size` zero bytes to `stream` from a stack buffer.
async fn send_zeros(stream: &mut TcpStream, size: u64) {
    let zeros = [0u8; CHUNK];
    let mut left = size;
    while left > 0 {
        let n = usize::try_from(left).unwrap_or(CHUNK).min(CHUNK);
        stream.write_all(&zeros[..n]).await.expect("write");
        left -= n as u64;
    }
}

/// Fake Telegram: `POST …/sendDocument` reads the body and answers
/// `{"ok":true,"result":true}`; `GET …/file/…/<n>` sends `n` zero bytes
/// with a `Content-Length`. Connections are kept alive.
async fn telegram(seen: Arc<Seen>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let mut buf = [0u8; 16 * 1024];
                while let Some((end, extra)) = read_head(&mut stream, &mut buf).await {
                    let head = std::str::from_utf8(&buf[..end]).expect("ASCII").to_owned();
                    if head.starts_with("POST") {
                        let length: u64 = header(&head, "content-length")
                            .map_or(0, |v| v.parse().expect("length"));
                        seen.framed_by_length.store(
                            header(&head, "content-length").is_some()
                                && header(&head, "transfer-encoding").is_none(),
                            Ordering::SeqCst,
                        );
                        let mut scratch = [0u8; CHUNK];
                        let rest = drain(&mut stream, length - extra as u64, &mut scratch).await;
                        seen.uploaded.store(extra as u64 + rest, Ordering::SeqCst);
                        let body = br#"{"ok":true,"result":true}"#;
                        let reply = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        );
                        stream.write_all(reply.as_bytes()).await.expect("reply");
                        stream.write_all(body).await.expect("reply");
                    } else {
                        let size: u64 = head
                            .split(' ')
                            .nth(1)
                            .and_then(|path| path.rsplit('/').next())
                            .and_then(|n| n.parse().ok())
                            .expect("a size");
                        let reply = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {size}\r\n\r\n"
                        );
                        stream.write_all(reply.as_bytes()).await.expect("reply");
                        send_zeros(&mut stream, size).await;
                    }
                }
            });
        }
    });
    addr
}

/// Uploads `size` bytes through tgin; returns the response head.
async fn upload(tgin: SocketAddr, size: u64) -> String {
    let mut stream = TcpStream::connect(tgin).await.expect("connect");
    let head = format!(
        "POST /bot{TOKEN}/sendDocument HTTP/1.1\r\nHost: tgin\r\nContent-Type: multipart/form-data; boundary=b\r\nContent-Length: {size}\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.expect("head");
    send_zeros(&mut stream, size).await;
    let mut buf = [0u8; 4096];
    let (end, _) = read_head(&mut stream, &mut buf).await.expect("a response");
    std::str::from_utf8(&buf[..end]).expect("ASCII").to_owned()
}

/// Downloads `size` bytes through tgin; returns the response head and the
/// body bytes read.
async fn download(tgin: SocketAddr, size: u64) -> (String, u64) {
    let mut stream = TcpStream::connect(tgin).await.expect("connect");
    let head = format!("GET /file/bot{TOKEN}/documents/{size} HTTP/1.1\r\nHost: tgin\r\n\r\n");
    stream.write_all(head.as_bytes()).await.expect("head");
    let mut buf = [0u8; CHUNK];
    let (end, extra) = read_head(&mut stream, &mut buf).await.expect("a response");
    let head = std::str::from_utf8(&buf[..end]).expect("ASCII").to_owned();
    let length: u64 = header(&head, "content-length").map_or(0, |v| v.parse().expect("length"));
    let read = extra as u64 + drain(&mut stream, length - extra as u64, &mut buf).await;
    (head, read)
}

/// Runs `transfer` and returns the heap's growth over it, in bytes. The
/// caller boxes `transfer` first, so neither its allocation nor its size
/// (its buffers live in it) counts or burdens the test thread's stack.
async fn growth<F: Future>(transfer: Pin<Box<F>>) -> (isize, F::Output) {
    let base = counting_alloc::reset_peak();
    let out = timeout(PATIENCE, transfer)
        .await
        .expect("the transfer ends in time");
    (counting_alloc::peak() - base, out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_and_download_stream_bounded() {
    let seen = Arc::new(Seen::default());
    let api = telegram(Arc::clone(&seen)).await;
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("gateway-bounded");
    std::fs::create_dir_all(&dir).expect("scratch directory");
    let file = dir.join("tgin.yaml");
    std::fs::write(
        &file,
        format!(
            "version: 1\nserver: {{ listen: [\"127.0.0.1:0\"] }}\nadmin: {{ listen: [\"127.0.0.1:0\"] }}\nbots:\n  - name: gw\n    token: \"{TOKEN}\"\n    api_url: \"http://{api}\"\n"
        ),
    )
    .expect("the config file");
    let boot = Boot::load(&file, Arc::new(MapEnv::default())).expect("valid config");
    let app = App::start(boot).await.expect("tgin starts");
    let tgin = app.data_addrs()[0];

    // Warm: connections pooled, metric handles resolved, buffers grown.
    for _ in 0..2 {
        assert!(upload(tgin, SMALL).await.starts_with("HTTP/1.1 200"));
        let (head, read) = download(tgin, SMALL).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(read, SMALL);
    }

    // The reference: the largest growth over as many 4 MiB transfers as
    // make 200 MiB, so it has as many chances to meet hyper's worst
    // buffer timing as the 200 MiB transfer has.
    let (mut small_up, mut small_down) = (0, 0);
    for _ in 0..SIZE / SMALL {
        let (up, head) = growth(Box::pin(upload(tgin, SMALL))).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let (down, (_, read)) = growth(Box::pin(download(tgin, SMALL))).await;
        assert_eq!(read, SMALL);
        small_up = small_up.max(up);
        small_down = small_down.max(down);
    }

    let (up, head) = growth(Box::pin(upload(tgin, SIZE))).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(seen.uploaded.load(Ordering::SeqCst), SIZE);
    assert!(
        seen.framed_by_length.load(Ordering::SeqCst),
        "Telegram must get the upload's Content-Length, unchunked"
    );

    let (down, (head, read)) = growth(Box::pin(download(tgin, SIZE))).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(
        header(&head, "content-length"),
        Some(SIZE.to_string().as_str())
    );
    assert_eq!(header(&head, "transfer-encoding"), None);
    assert_eq!(read, SIZE);

    println!(
        "heap growth: upload {up} B at {SIZE} B, {small_up} B at {SMALL} B; download {down} B at {SIZE} B, {small_down} B at {SMALL} B"
    );
    for (what, growth) in [
        ("200 MiB upload", up),
        ("4 MiB upload", small_up),
        ("200 MiB download", down),
        ("4 MiB download", small_down),
    ] {
        assert!(growth <= BOUND, "the {what} grew the heap by {growth} B");
    }
}
