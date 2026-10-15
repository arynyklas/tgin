//! Integration tests for the hardened HTTP server and client, over real
//! loopback sockets.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use tgin_runtime::http::{
    ClientOptions, RunningServer, ServerOptions, TlsFiles, describe_error, serve,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto};

const KEEP_ALIVE_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: test\r\n\r\n";
const CLOSE_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n";
const OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\n";

fn router() -> Router {
    Router::new().route("/", get(|| async { "ok" }))
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

async fn start(opts: ServerOptions) -> RunningServer {
    serve(router(), &opts).await.expect("server starts")
}

/// Reads one `"ok"` response, or everything until the peer closes the
/// connection. A reset is reported as `Err`.
async fn read_response<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<Vec<u8>> {
    let mut response = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(response);
        }
        response.extend_from_slice(&chunk[..read]);
        if response.ends_with(b"\r\n\r\nok") {
            return Ok(response);
        }
    }
}

/// Sends `raw` and asserts that the server answers `200 OK`.
async fn assert_served<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, raw: &[u8]) {
    stream.write_all(raw).await.expect("request is written");
    let response = timeout(Duration::from_secs(5), read_response(stream))
        .await
        .expect("response within 5 s")
        .expect("response is read");
    assert!(
        response.starts_with(OK_RESPONSE),
        "unexpected response {:?}",
        String::from_utf8_lossy(&response)
    );
}

/// Waits until the server closes `stream`, either cleanly or with a reset.
/// Returns whatever the server sent before closing.
async fn closed_within(stream: &mut TcpStream, limit: Duration) -> Vec<u8> {
    let mut received = Vec::new();
    let result = timeout(limit, stream.read_to_end(&mut received)).await;
    assert!(result.is_ok(), "connection still open after {limit:?}");
    received
}

#[tokio::test]
async fn silent_client_closed_after_header_timeout() {
    let server = start(ServerOptions {
        listen: vec![loopback()],
        header_read_timeout: Duration::from_millis(300),
        ..ServerOptions::default()
    })
    .await;

    let mut stream = TcpStream::connect(server.local_addrs[0]).await.unwrap();
    let started = Instant::now();
    let received = closed_within(&mut stream, Duration::from_secs(1)).await;

    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "closed after {:?}, before the header timeout could fire",
        started.elapsed()
    );
    assert!(!received.starts_with(OK_RESPONSE));
}

#[tokio::test]
async fn partial_header_closed() {
    let server = start(ServerOptions {
        listen: vec![loopback()],
        header_read_timeout: Duration::from_millis(300),
        ..ServerOptions::default()
    })
    .await;

    let mut stream = TcpStream::connect(server.local_addrs[0]).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n")
        .await
        .unwrap();
    let started = Instant::now();
    let received = closed_within(&mut stream, Duration::from_secs(1)).await;

    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "closed after {:?}, before the header timeout could fire",
        started.elapsed()
    );
    assert!(!received.starts_with(OK_RESPONSE));
}

#[tokio::test]
async fn connection_cap_rejects_third() {
    let server = start(ServerOptions {
        listen: vec![loopback()],
        max_connections: 2,
        ..ServerOptions::default()
    })
    .await;
    let addr = server.local_addrs[0];

    // A served request proves the connection holds a slot; keep-alive keeps
    // it held.
    let mut first = TcpStream::connect(addr).await.unwrap();
    assert_served(&mut first, KEEP_ALIVE_REQUEST).await;
    let mut second = TcpStream::connect(addr).await.unwrap();
    assert_served(&mut second, KEEP_ALIVE_REQUEST).await;

    let mut third = TcpStream::connect(addr).await.unwrap();
    // The server may already have reset the socket, so a failed write is fine.
    let _ = third.write_all(KEEP_ALIVE_REQUEST).await;
    let received = closed_within(&mut third, Duration::from_secs(1)).await;
    assert!(received.is_empty(), "third connection got {received:?}");
    assert_eq!(server.connection_count(), 2);

    // Closing a held connection frees its slot.
    drop(first);
    timeout(Duration::from_secs(5), async {
        loop {
            let mut next = TcpStream::connect(addr).await.unwrap();
            let _ = next.write_all(KEEP_ALIVE_REQUEST).await;
            if let Ok(response) = read_response(&mut next).await
                && response.starts_with(OK_RESPONSE)
            {
                return;
            }
        }
    })
    .await
    .expect("a slot frees up after a held connection closes");
    assert_served(&mut second, KEEP_ALIVE_REQUEST).await;
}

#[tokio::test]
async fn two_listeners_shutdown_together() {
    let server = start(ServerOptions {
        listen: vec![loopback(), loopback()],
        ..ServerOptions::default()
    })
    .await;
    let addrs = server.local_addrs.clone();
    assert_eq!(addrs.len(), 2);
    assert_ne!(addrs[0], addrs[1]);
    for addr in &addrs {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        assert_served(&mut stream, CLOSE_REQUEST).await;
    }

    server.graceful_shutdown(Duration::from_secs(1));
    timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("both listeners stop within the grace period");

    for addr in &addrs {
        assert!(
            TcpStream::connect(addr).await.is_err(),
            "{addr} still accepts connections"
        );
    }
}

fn write_pair(dir: &Path, pair: &rcgen::CertifiedKey<rcgen::KeyPair>) -> TlsFiles {
    let files = TlsFiles {
        cert: dir.join("cert.pem"),
        key: dir.join("key.pem"),
    };
    std::fs::write(&files.cert, pair.cert.pem()).unwrap();
    std::fs::write(&files.key, pair.signing_key.serialize_pem()).unwrap();
    files
}

/// Completes a fresh TLS handshake, checks that HTTP/1.1 is served over it
/// and returns the certificate the server presented.
async fn handshake(addr: SocketAddr, roots: &RootCertStore) -> CertificateDer<'static> {
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(crypto::aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots.clone())
            .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let tcp = TcpStream::connect(addr).await.unwrap();
    let mut tls = TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .expect("TLS handshake");

    let (_, connection) = tls.get_ref();
    assert_eq!(connection.alpn_protocol(), Some(&b"http/1.1"[..]));
    let presented = connection.peer_certificates().unwrap()[0].clone();
    assert_served(&mut tls, CLOSE_REQUEST).await;
    presented
}

#[tokio::test]
async fn tls_reload_serves_new_certificate() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tls_reload_serves_new_certificate");
    std::fs::create_dir_all(&dir).unwrap();
    let first = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let second = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(first.cert.der().clone()).unwrap();
    roots.add(second.cert.der().clone()).unwrap();

    let files = write_pair(&dir, &first);
    let server = start(ServerOptions {
        listen: vec![loopback()],
        tls: Some(files),
        ..ServerOptions::default()
    })
    .await;
    let addr = server.local_addrs[0];
    assert_eq!(&handshake(addr, &roots).await, first.cert.der());

    write_pair(&dir, &second);
    server.reload_tls().await.expect("reload succeeds");
    assert_eq!(&handshake(addr, &roots).await, second.cert.der());
}

#[tokio::test]
async fn describe_error_has_no_url() {
    let closed_port = std::net::TcpListener::bind(loopback())
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{closed_port}/bot123456789:AAAAexampleSecretToken/x");

    // Built without `client()` so an HTTP_PROXY or ALL_PROXY in the test
    // environment cannot reroute the request.
    let opts = ClientOptions::default();
    let err = reqwest::Client::builder()
        .connect_timeout(opts.connect_timeout)
        .timeout(opts.request_timeout)
        .pool_idle_timeout(opts.pool_idle_timeout)
        .no_proxy()
        .build()
        .unwrap()
        .get(&url)
        .send()
        .await
        .expect_err("nothing listens on the port");
    assert!(
        format!("{err:?}").contains("AAAA"),
        "precondition: reqwest's own Debug output carries the URL"
    );

    let described = describe_error(&err);
    assert!(!described.contains("AAAA"), "token leaked: {described}");
    assert!(
        !described.contains("bot123456789"),
        "URL leaked: {described}"
    );
    assert!(described.contains("connect"), "kind missing: {described}");
}
