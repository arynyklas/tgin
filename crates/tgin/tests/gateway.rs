//! The Bot API gateway end to end: the app in process on loopback sockets
//! the OS picks, wiremock (or a small axum server, where a body must
//! trickle) as Telegram, and raw TCP where a test needs exact headers.
//! Idle timeouts run in real time here: the transfers cross real sockets.
//! `upload_and_download_stream_bounded` lives in `gateway_bounded.rs`, a
//! binary of its own, because it measures the whole process's heap.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tgin::app::{App, Boot};
use tgin::exit::Outcome;
use tgin_runtime::config::MapEnv;
use tgin_runtime::lifecycle::{SignalKind, Signals};
use tgin_runtime::obs::MetricsRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Bounds every wait, so a stall fails the test instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(10);
const TOKEN: &str = "123456789:AAAAexampleSecretTokenValue_-xyz";
const OTHER_TOKEN: &str = "987654321:BBBBanotherSecretTokenValue_-xyz";
const HOOK_SECRET: &str = "hook-secret-value";

/// Everything every test logs, unredacted: the test subscriber has no
/// redactor, so a token that reaches a log line shows up here.
static LOGS: LazyLock<Logs> = LazyLock::new(|| {
    let logs = Logs::default();
    tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .try_init()
        .expect("the only global subscriber");
    logs
});

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("logs")).into_owned()
    }
}

impl io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("logs").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Bot `gw` (`TOKEN`, feeding `workers`) and bot `other` (`OTHER_TOKEN`,
/// feeding `others`), neither with ingress, so Telegram sees gateway
/// requests only; `gw` waits at most 1 s for progress.
fn gateway_config(api: &str) -> String {
    format!(
        r#"version: 1
runtime: {{ lame_duck: 0s, shutdown_grace: 2s }}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"] }}
bots:
  - name: gw
    token: "{TOKEN}"
    api_url: "{api}"
    gateway: {{ upstream_idle_timeout: 1s }}
    routes: [{{ to: [workers] }}]
  - name: other
    token: "{OTHER_TOKEN}"
    api_url: "{api}"
    routes: [{{ to: [others] }}]
consumers:
  workers: {{ pull: {{}} }}
  others: {{ pull: {{}} }}
"#
    )
}

/// The app, running on signals the test sends.
struct Tgin {
    addr: SocketAddr,
    /// The admin plane's first address, if it listens.
    admin: Option<SocketAddr>,
    file: PathBuf,
    signals: mpsc::Sender<SignalKind>,
    run: JoinHandle<Outcome>,
    metrics: Arc<MetricsRegistry>,
    http: reqwest::Client,
}

async fn start(yaml: &str) -> Tgin {
    LazyLock::force(&LOGS);
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("gateway-{:016x}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).expect("scratch directory");
    let file = dir.join("tgin.yaml");
    std::fs::write(&file, yaml).expect("the config file");
    let boot = Boot::load(&file, Arc::new(MapEnv::default()))
        .unwrap_or_else(|err| panic!("invalid test config: {err:?}"));
    let app = App::start(boot).await.expect("tgin starts");
    let addr = app.data_addrs()[0];
    let admin = app.admin_addrs().first().copied();
    let metrics = app.metrics();
    let (signals, rx) = mpsc::channel(4);
    let run = tokio::spawn(app.run(Signals::from_channel(rx)));
    Tgin {
        addr,
        admin,
        file,
        signals,
        run,
        metrics,
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
    }
}

impl Tgin {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    #[track_caller]
    fn assert_metric(&self, line: &str) {
        let out = self.metrics.encode();
        assert!(out.lines().any(|l| l == line), "missing {line:?} in\n{out}");
    }

    async fn stop(self) {
        self.signals
            .send(SignalKind::Terminate)
            .await
            .expect("the app listens");
        let outcome = timeout(PATIENCE, self.run)
            .await
            .expect("the app exits in time")
            .expect("the app does not panic");
        assert_eq!(outcome, Outcome::Clean);
    }
}

fn telegram_error(status: u16, description: &str) -> Value {
    json!({ "ok": false, "error_code": status, "description": description })
}

async fn json_of(response: reqwest::Response) -> (u16, Value) {
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("body");
    (
        status,
        serde_json::from_slice(&body).unwrap_or_else(|_| panic!("JSON: {body:?}")),
    )
}

async fn seen(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.expect("recording on")
}

/// An HTTP/1.1 response read off a raw socket.
struct Raw {
    status: u16,
    /// Lowercased names.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Raw {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Reads one response with a `Content-Length` body from `stream`.
async fn read_response(stream: &mut TcpStream) -> Raw {
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.expect("read");
        assert!(n > 0, "connection closed before the response head");
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8(buf[..head_end].to_vec()).expect("ASCII head");
    let mut lines = head.split("\r\n");
    let status = lines.next().expect("status line")[9..12]
        .parse()
        .expect("status");
    let headers: Vec<(String, String)> = lines
        .map(|line| {
            let (name, value) = line.split_once(':').expect("header line");
            (name.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .map_or(0, |(_, v)| v.parse().expect("length"));
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.expect("read");
        assert!(n > 0, "connection closed before the body ended");
        body.extend_from_slice(&chunk[..n]);
    }
    Raw {
        status,
        headers,
        body,
    }
}

/// A small HTTP server playing Telegram, for bodies wiremock cannot
/// trickle.
async fn serve(router: axum::Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxies_method_and_streams_body() {
    let telegram = MockServer::start().await;
    let answer = r#"{"ok":true,"result":{"message_id":7,"text":"h\u00e9llo"}}"#;
    Mock::given(method("POST"))
        .and(path(format!("/tg/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(answer, "application/json"))
        .mount(&telegram)
        .await;
    let refusal = r#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#;
    Mock::given(method("POST"))
        .and(path(format!("/tg/bot{TOKEN}/sendPhoto")))
        .respond_with(ResponseTemplate::new(400).set_body_raw(refusal, "application/json"))
        .mount(&telegram)
        .await;
    // `api_url` with a path: it is kept, as for tgin's own Bot API calls.
    let tgin = start(&gateway_config(&format!("{}/tg", telegram.uri()))).await;

    let body = "{\"chat_id\":42,\"text\":\"h\u{e9}llo\"}";
    let response = tgin
        .http
        .post(tgin.url(&format!("/bot{TOKEN}/sendMessage")))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "application/json",
        "{:?}",
        response.headers()
    );
    assert_eq!(
        &response.bytes().await.expect("body")[..],
        answer.as_bytes()
    );

    let response = tgin
        .http
        .post(tgin.url(&format!("/bot{TOKEN}/sendPhoto")))
        .header("content-type", "application/json")
        .body(r#"{"chat_id":1}"#)
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 400);
    assert_eq!(
        &response.bytes().await.expect("body")[..],
        refusal.as_bytes()
    );

    let requests = seen(&telegram).await;
    assert_eq!(requests.len(), 2);
    let sent = &requests[0];
    assert_eq!(sent.url.path(), format!("/tg/bot{TOKEN}/sendMessage"));
    assert_eq!(&sent.body[..], body.as_bytes());
    assert_eq!(sent.headers["content-type"], "application/json");
    assert_eq!(sent.headers["content-length"], body.len().to_string());

    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="ok"} 1"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendPhoto",outcome="rejected"} 1"#,
    );
    tgin.assert_metric(r#"tgin_gateway_upstream_seconds_count{bot="gw",method="sendMessage"} 1"#);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_token_is_401_without_upstream_call() {
    let telegram = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/getMe")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"ok":true,"result":{"id":123456789,"is_bot":true}}"#,
            "application/json",
        ))
        .mount(&telegram)
        .await;
    let tgin = start(&gateway_config(&telegram.uri())).await;

    let near_miss = format!("{}y", &TOKEN[..TOKEN.len() - 1]);
    let (id, secret) = TOKEN.split_once(':').expect("token");
    let wrong_case = format!("{id}:{}", secret.to_ascii_uppercase());
    let unknown = "111111:unknownTokenValue".to_owned();
    for token in [&unknown, &near_miss, &wrong_case] {
        let response = tgin
            .http
            .post(tgin.url(&format!("/bot{token}/getMe")))
            .send()
            .await
            .expect("response");
        let (status, body) = json_of(response).await;
        assert_eq!(status, 401, "{token}");
        assert_eq!(body, telegram_error(401, "Unauthorized"));
    }
    // Decoded once, a double-encoded token has no colon: no gateway path.
    let double = TOKEN.replace(':', "%253A");
    let response = tgin
        .http
        .get(tgin.url(&format!("/bot{double}/getMe")))
        .send()
        .await
        .expect("response");
    assert_eq!(response.headers()["content-type"], "application/json");
    let (status, body) = json_of(response).await;
    assert_eq!(status, 404);
    assert_eq!(body, telegram_error(404, "Not Found"));
    assert!(seen(&telegram).await.is_empty(), "nothing went upstream");
    tgin.assert_metric("tgin_gateway_unauthorized_total 3");

    // `%3A`, as python-telegram-bot writes it, is the configured token.
    let encoded = TOKEN.replace(':', "%3A");
    let response = tgin
        .http
        .get(tgin.url(&format!("/bot{encoded}/getMe")))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    let requests = seen(&telegram).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), format!("/bot{TOKEN}/getMe"));

    let metrics = tgin.metrics.encode();
    let logs = LOGS.text();
    for secret in [
        TOKEN,
        secret,
        OTHER_TOKEN,
        &near_miss,
        &wrong_case,
        &unknown,
        &double,
        &encoded,
    ] {
        assert!(!logs.contains(secret), "a log line holds {secret}");
        assert!(!metrics.contains(secret), "a metric holds {secret}");
    }
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_string_forwarded() {
    let telegram = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getChat")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"ok":true,"result":{"id":-100}}"#, "application/json"),
        )
        .mount(&telegram)
        .await;
    let tgin = start(&gateway_config(&telegram.uri())).await;

    let response = tgin
        .http
        .get(tgin.url(&format!(
            "/bot{TOKEN}/getChat?chat_id=%40chan&x=a+b&chat_id=2"
        )))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    let requests = seen(&telegram).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url.query(),
        Some("chat_id=%40chan&x=a+b&chat_id=2")
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headers_filtered_both_ways() {
    let telegram = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"ok":true,"result":true}"#, "application/json")
                .insert_header("connection", "keep-alive, x-hop")
                .insert_header("x-hop", "secret")
                .insert_header("keep-alive", "timeout=5")
                .insert_header("etag", "\"v1\""),
        )
        .mount(&telegram)
        .await;
    let tgin = start(&gateway_config(&telegram.uri())).await;

    let mut stream = TcpStream::connect(tgin.addr).await.expect("connect");
    let request = format!(
        "POST /bot{TOKEN}/sendMessage HTTP/1.1\r\nHost: tgin\r\nConnection: x-foo, user-agent\r\nX-Foo: bar\r\nUser-Agent: hop-listed\r\nAuthorization: Bearer downstream-secret\r\nX-Tgin-Instance: i1\r\nX-Forwarded-For: 203.0.113.9\r\nForwarded: for=203.0.113.9\r\nVia: 1.1 proxy-a\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let response = timeout(PATIENCE, read_response(&mut stream))
        .await
        .expect("a response in time");
    assert_eq!(response.status, 200);
    assert_eq!(&response.body[..], br#"{"ok":true,"result":true}"#);
    assert_eq!(response.header("etag"), Some("\"v1\""));
    assert_eq!(response.header("content-type"), Some("application/json"));
    for dropped in ["keep-alive", "x-hop"] {
        assert_eq!(
            response.header(dropped),
            None,
            "{dropped} reached the client"
        );
    }
    assert!(
        response
            .header("connection")
            .is_none_or(|v| !v.contains("x-hop")),
        "{:?}",
        response.headers
    );

    let requests = seen(&telegram).await;
    assert_eq!(requests.len(), 1);
    let headers = &requests[0].headers;
    for dropped in [
        "connection",
        "x-foo",
        "authorization",
        "x-tgin-instance",
        "x-forwarded-for",
        "forwarded",
    ] {
        assert!(!headers.contains_key(dropped), "{dropped} reached Telegram");
    }
    // Allowlisted, but named in `Connection`: hop-by-hop.
    assert!(
        headers
            .get("user-agent")
            .is_none_or(|v| v.to_str().unwrap_or_default() != "hop-listed"),
        "{headers:?}"
    );
    assert_eq!(headers["accept"], "application/json");
    assert_eq!(headers["content-type"], "application/json");
    // The intermediaries before tgin, then tgin's own pseudonym.
    let via: Vec<&str> = headers
        .get_all("via")
        .iter()
        .map(|v| v.to_str().expect("ASCII"))
        .collect();
    assert_eq!(via.len(), 2, "{via:?}");
    assert_eq!(via[0], "1.1 proxy-a");
    assert!(via[1].starts_with("1.1 tgin-"), "{via:?}");
    tgin.stop().await;
}

/// `count` chunks of 100 bytes, one every `gap`.
fn trickle(count: u32, gap: Duration) -> Body {
    let chunks = futures_util::stream::unfold(0, move |sent| async move {
        if sent == count {
            return None;
        }
        tokio::time::sleep(gap).await;
        Some((Ok::<_, Infallible>(Bytes::from(vec![b'x'; 100])), sent + 1))
    });
    Body::from_stream(chunks)
}

/// Telegram for the trickle tests: `sendDocument` answers with the number
/// of body bytes it read; `/file/…/trickle` sends 10 chunks over 2.5 s;
/// `/file/…/stall` sends one chunk, then nothing for 3 s.
async fn trickling_telegram() -> SocketAddr {
    let router = axum::Router::new()
        .route(
            &format!("/bot{TOKEN}/sendDocument"),
            post(|body: Body| async move {
                let read = axum::body::to_bytes(body, usize::MAX)
                    .await
                    .map_or(0, |b| b.len());
                format!(r#"{{"ok":true,"result":{read}}}"#)
            }),
        )
        .route(
            &format!("/file/bot{TOKEN}/trickle"),
            get(|| async { trickle(10, Duration::from_millis(250)) }),
        )
        .route(
            &format!("/file/bot{TOKEN}/stall"),
            get(|| async {
                let first = futures_util::stream::once(async {
                    Ok::<_, Infallible>(Bytes::from_static(b"first"))
                });
                let late = futures_util::stream::once(async {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    Ok::<_, Infallible>(Bytes::from_static(b"late"))
                });
                Body::from_stream(futures_util::StreamExt::chain(first, late))
            }),
        );
    serve(router).await
}

/// Sends a `sendDocument` head announcing 1 000 bytes, then `chunks` of
/// 100 bytes one every `gap`, and returns the response.
async fn upload(tgin: &Tgin, chunks: u32, gap: Duration) -> Raw {
    let mut stream = TcpStream::connect(tgin.addr).await.expect("connect");
    let head = format!(
        "POST /bot{TOKEN}/sendDocument HTTP/1.1\r\nHost: tgin\r\nContent-Type: multipart/form-data; boundary=b\r\nContent-Length: 1000\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.expect("head");
    let (mut read, mut write) = stream.into_split();
    let writer = tokio::spawn(async move {
        for _ in 0..chunks {
            tokio::time::sleep(gap).await;
            if write.write_all(&[b'x'; 100]).await.is_err() {
                return;
            }
        }
        // Keeps the socket open until the test is done with it.
        tokio::time::sleep(PATIENCE).await;
    });
    let mut joined = Vec::new();
    let response = timeout(PATIENCE, async {
        let mut buf = [0u8; 4096];
        loop {
            let n = read.read(&mut buf).await.expect("read");
            assert!(n > 0, "closed before a response");
            joined.extend_from_slice(&buf[..n]);
            if let Some(end) = joined.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&joined[..end]).to_ascii_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse().expect("length"));
                while joined.len() < end + 4 + length {
                    let n = read.read(&mut buf).await.expect("read");
                    assert!(n > 0, "closed before the body ended");
                    joined.extend_from_slice(&buf[..n]);
                }
                let status = head[9..12].parse().expect("status");
                return Raw {
                    status,
                    headers: Vec::new(),
                    body: joined[end + 4..end + 4 + length].to_vec(),
                };
            }
        }
    })
    .await
    .expect("a response in time");
    writer.abort();
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_transfer_not_cut_while_bytes_flow() {
    let telegram = trickling_telegram().await;
    let tgin = start(&gateway_config(&format!("http://{telegram}"))).await;

    // Upload: 2.5 s in all against a 1 s idle timeout, never idle 1 s.
    let started = Instant::now();
    let done = upload(&tgin, 10, Duration::from_millis(250)).await;
    assert_eq!(done.status, 200, "{}", String::from_utf8_lossy(&done.body));
    assert_eq!(&done.body[..], br#"{"ok":true,"result":1000}"#);
    assert!(started.elapsed() > Duration::from_secs(2));

    // Download: the same.
    let started = Instant::now();
    let response = tgin
        .http
        .get(tgin.url(&format!("/file/bot{TOKEN}/trickle")))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.expect("the whole body").len(), 1000);
    assert!(started.elapsed() > Duration::from_secs(2));

    // An upload idle for longer than 1 s: 502 before any header. The server
    // never got the whole request, so it cannot have executed it.
    let stalled = upload(&tgin, 1, Duration::from_millis(10)).await;
    assert_eq!(stalled.status, 502);
    assert_eq!(
        serde_json::from_slice::<Value>(&stalled.body).expect("JSON"),
        telegram_error(502, UNREACHABLE)
    );

    // A download idle for longer than 1 s: the body aborts.
    let response = tgin
        .http
        .get(tgin.url(&format!("/file/bot{TOKEN}/stall")))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    assert!(response.bytes().await.is_err(), "the body must abort");

    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendDocument",outcome="ok"} 1"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendDocument",outcome="unreachable"} 1"#,
    );
    // Counted once: a stalled upload is no client that left.
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendDocument",outcome="client_closed"} 0"#,
    );
    tgin.assert_metric(r#"tgin_gateway_requests_total{bot="gw",method="file",outcome="ok"} 1"#);
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="file",outcome="upstream_error"} 1"#,
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_unreachable_is_502_shaped() {
    // Bound but not listening, and held for the whole test: connecting is
    // refused, and no other socket can take the port meanwhile.
    let closed = tokio::net::TcpSocket::new_v4().unwrap();
    closed.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = closed.local_addr().unwrap().port();
    // A bot name of its own, for its log line; an idle timeout longer than
    // a refused connect takes (Windows retries one for about 2 s).
    let config = gateway_config(&format!("http://127.0.0.1:{port}"))
        .replace("name: gw", "name: offline")
        .replace("upstream_idle_timeout: 1s", "upstream_idle_timeout: 30s");
    let tgin = start(&config).await;

    let response = tgin
        .http
        .post(tgin.url(&format!("/bot{TOKEN}/sendMessage")))
        .header("content-type", "application/json")
        .body(r#"{"chat_id":1,"text":"x"}"#)
        .send()
        .await
        .expect("response");
    assert_eq!(response.headers()["content-type"], "application/json");
    let (status, body) = json_of(response).await;
    assert_eq!(status, 502);
    assert_eq!(body, telegram_error(502, UNREACHABLE));
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="offline",method="sendMessage",outcome="unreachable"} 1"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="offline",method="sendMessage",outcome="client_closed"} 0"#,
    );
    let logs = LOGS.text();
    assert!(
        logs.lines()
            .any(|l| l.contains("Bot API server is unreachable") && l.contains("bot=offline")),
        "the failure is logged with the bot's name"
    );
    assert!(!logs.contains(TOKEN), "the token reached a log line");
    tgin.stop().await;
}

/// A downstream that sends 100 of 1000 announced bytes and closes is not a
/// Telegram failure: counted `client_closed`, never `unreachable`, and no
/// warning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_closing_mid_upload_is_client_closed() {
    let telegram = trickling_telegram().await;
    // A bot name of its own, for its log lines; a long idle timeout, so only
    // the client's close can end the request.
    let config = gateway_config(&format!("http://{telegram}"))
        .replace("name: gw", "name: aborter")
        .replace("upstream_idle_timeout: 1s", "upstream_idle_timeout: 30s");
    let tgin = start(&config).await;

    let mut stream = TcpStream::connect(tgin.addr).await.expect("connect");
    let head = format!(
        "POST /bot{TOKEN}/sendDocument HTTP/1.1\r\nHost: tgin\r\nContent-Type: multipart/form-data; boundary=b\r\nContent-Length: 1000\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.expect("head");
    stream
        .write_all(&[b'x'; 100])
        .await
        .expect("part of the body");
    drop(stream);

    let line = r#"tgin_gateway_requests_total{bot="aborter",method="sendDocument",outcome="client_closed"} 1"#;
    let started = Instant::now();
    while !tgin.metrics.encode().lines().any(|l| l == line) {
        assert!(
            started.elapsed() < PATIENCE,
            "never counted client_closed:\n{}",
            tgin.metrics.encode()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="aborter",method="sendDocument",outcome="unreachable"} 0"#,
    );
    let logs = LOGS.text();
    assert!(
        !logs
            .lines()
            .any(|l| l.contains("WARN") && l.contains("bot=aborter")),
        "a client's close is no warning"
    );
    tgin.stop().await;
}

/// The largest `upstream_idle_timeout` the config accepts serves requests:
/// its deadlines saturate instead of overflowing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn huge_idle_timeout_serves() {
    let telegram = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"ok":true,"result":true}"#, "application/json"),
        )
        .mount(&telegram)
        .await;
    Mock::given(path(format!("/file/bot{TOKEN}/f.bin")))
        .respond_with(ResponseTemplate::new(200).set_body_raw("bytes", "application/octet-stream"))
        .mount(&telegram)
        .await;
    let config = gateway_config(&telegram.uri()).replace(
        "upstream_idle_timeout: 1s",
        "upstream_idle_timeout: 18446744073709551615s",
    );
    let tgin = start(&config).await;

    let response = tgin
        .http
        .post(tgin.url(&format!("/bot{TOKEN}/sendMessage")))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("a response, not a dropped connection");
    assert_eq!(response.status(), 200);
    assert_eq!(
        &response.bytes().await.expect("body")[..],
        br#"{"ok":true,"result":true}"#
    );
    let response = tgin
        .http
        .get(tgin.url(&format!("/file/bot{TOKEN}/f.bin")))
        .send()
        .await
        .expect("a response");
    assert_eq!(&response.bytes().await.expect("body")[..], b"bytes");
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_route_streams_with_range() {
    let telegram = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/file/bot{TOKEN}/documents/file_1.pdf")))
        .respond_with(
            ResponseTemplate::new(206)
                .set_body_raw("abcd", "application/pdf")
                .insert_header("content-range", "bytes 0-3/10")
                .insert_header("accept-ranges", "bytes")
                .insert_header("etag", "\"f1\"")
                .insert_header("last-modified", "Wed, 21 Oct 2026 07:28:00 GMT"),
        )
        .mount(&telegram)
        .await;
    let tgin = start(&gateway_config(&telegram.uri())).await;

    let encoded = TOKEN.replace(':', "%3A");
    let response = tgin
        .http
        .get(tgin.url(&format!("/file/bot{encoded}/documents/file_1.pdf")))
        .header("range", "bytes=0-3")
        .header("if-none-match", "\"f0\"")
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 206);
    let headers = response.headers().clone();
    assert_eq!(headers["content-type"], "application/pdf");
    assert_eq!(headers["content-range"], "bytes 0-3/10");
    assert_eq!(headers["accept-ranges"], "bytes");
    assert_eq!(headers["etag"], "\"f1\"");
    assert_eq!(headers["last-modified"], "Wed, 21 Oct 2026 07:28:00 GMT");
    assert_eq!(headers["content-length"], "4");
    assert_eq!(&response.bytes().await.expect("body")[..], b"abcd");

    let requests = seen(&telegram).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].headers["range"], "bytes=0-3");
    assert_eq!(requests[0].headers["if-none-match"], "\"f0\"");
    tgin.assert_metric(r#"tgin_gateway_requests_total{bot="gw",method="file",outcome="ok"} 1"#);
    tgin.stop().await;
}

/// A webhook bot `hooked` (path `/hook`) feeding `workers`, polled through
/// the alias `/bot1/getUpdates`. Reloaded by SIGHUP only: no config watch,
/// whose own reload of a rewrite would race the signal.
fn hooked_config(api: &str, extra_bot: &str) -> String {
    format!(
        r#"version: 1
runtime: {{ lame_duck: 0s, shutdown_grace: 2s }}
reload: {{ watch: false }}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"] }}
bots:
  - name: hooked
    token: "{TOKEN}"
    api_url: "{api}"
    ingress: {{ webhook: {{ path: /hook, secret_token: "{HOOK_SECRET}" }} }}
    routes: [{{ to: [workers] }}]
{extra_bot}consumers:
  workers: {{ pull: {{ aliases: ["/bot1/getUpdates"] }} }}
"#
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_routes_win() {
    let telegram = MockServer::start().await;
    for (token, m, result) in [
        (
            TOKEN,
            "getMe",
            json!({ "id": 123456789, "is_bot": true, "username": "hooked_bot" }),
        ),
        (TOKEN, "setWebhook", json!(true)),
        (TOKEN, "getChat", json!({ "id": 1 })),
        (OTHER_TOKEN, "getChat", json!({ "id": 2 })),
    ] {
        Mock::given(path(format!("/bot{token}/{m}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": result })),
            )
            .mount(&telegram)
            .await;
    }
    let tgin = start(&hooked_config(&telegram.uri(), "")).await;

    let update = r#"{"update_id":1,"message":{"message_id":1,"chat":{"id":5,"type":"private"},"text":"hi"}}"#;
    let delivered = tgin
        .http
        .post(tgin.url("/hook"))
        .header("x-telegram-bot-api-secret-token", HOOK_SECRET)
        .header("content-type", "application/json")
        .body(update)
        .send()
        .await
        .expect("webhook");
    assert_eq!(delivered.status(), 200);
    let polled = tgin
        .http
        .post(tgin.url("/bot1/getUpdates"))
        .json(&json!({ "timeout": 0 }))
        .send()
        .await
        .expect("poll");
    let (status, body) = json_of(polled).await;
    assert_eq!(status, 200);
    assert_eq!(body["result"][0]["update_id"], 1, "{body}");
    let proxied = tgin
        .http
        .get(tgin.url(&format!("/bot{TOKEN}/getChat")))
        .send()
        .await
        .expect("gateway");
    assert_eq!(json_of(proxied).await.1["result"]["id"], 1);
    let unknown = tgin
        .http
        .get(tgin.url(&format!("/bot{OTHER_TOKEN}/getChat")))
        .send()
        .await
        .expect("gateway");
    assert_eq!(unknown.status(), 401);

    // A reload that adds a bot: the gateway serves it, and the rebuilt
    // routes still win over the gateway.
    let late = format!(
        "  - name: late\n    token: \"{OTHER_TOKEN}\"\n    api_url: \"{}\"\n",
        telegram.uri()
    );
    std::fs::write(&tgin.file, hooked_config(&telegram.uri(), &late)).expect("rewrite");
    tgin.signals
        .send(SignalKind::Hangup)
        .await
        .expect("the app listens");
    let started = Instant::now();
    loop {
        let response = tgin
            .http
            .get(tgin.url(&format!("/bot{OTHER_TOKEN}/getChat")))
            .send()
            .await
            .expect("gateway");
        if response.status() == 200 {
            assert_eq!(json_of(response).await.1["result"]["id"], 2);
            break;
        }
        assert!(
            started.elapsed() < PATIENCE,
            "the reload never served `late`"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let polled = tgin
        .http
        .post(tgin.url("/bot1/getUpdates"))
        .json(&json!({ "offset": 2, "timeout": 0 }))
        .send()
        .await
        .expect("poll");
    assert_eq!(
        json_of(polled).await,
        (200, json!({ "ok": true, "result": [] }))
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consumer_scoped_route_requires_consumer_of_bot() {
    let telegram = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"ok":true,"result":true}"#, "application/json"),
        )
        .mount(&telegram)
        .await;
    Mock::given(path(format!("/file/bot{TOKEN}/photos/p.jpg")))
        .respond_with(ResponseTemplate::new(200).set_body_raw("jpeg", "image/jpeg"))
        .mount(&telegram)
        .await;
    let tgin = start(&gateway_config(&telegram.uri())).await;
    let send = |path: String| {
        tgin.http
            .post(tgin.url(&path))
            .header("content-type", "application/json")
            .body("{}")
            .send()
    };

    let ok = send(format!("/c/workers/bot{TOKEN}/sendMessage"))
        .await
        .expect("response");
    assert_eq!(ok.status(), 200);
    let file = tgin
        .http
        .get(tgin.url(&format!("/c/workers/file/bot{TOKEN}/photos/p.jpg")))
        .send()
        .await
        .expect("response");
    assert_eq!(file.status(), 200);
    assert_eq!(&file.bytes().await.expect("body")[..], b"jpeg");
    assert_eq!(seen(&telegram).await.len(), 2);

    for consumer in ["others", "nope"] {
        let response = send(format!("/c/{consumer}/bot{TOKEN}/sendMessage"))
            .await
            .expect("response");
        let (status, body) = json_of(response).await;
        assert_eq!(status, 404, "{consumer}");
        assert_eq!(body, telegram_error(404, "Not Found: consumer not found"));
    }
    // The token is checked first.
    let response = send("/c/workers/bot111111:unknownTokenValue/sendMessage".to_owned())
        .await
        .expect("response");
    assert_eq!(response.status(), 401);
    let response = tgin
        .http
        .put(tgin.url(&format!("/bot{TOKEN}/sendMessage")))
        .body("{}")
        .send()
        .await
        .expect("response");
    let (status, body) = json_of(response).await;
    assert_eq!(status, 405);
    assert_eq!(body, telegram_error(405, "Method Not Allowed"));
    let response = tgin
        .http
        .get(tgin.url("/no/such/path"))
        .send()
        .await
        .expect("response");
    let (status, body) = json_of(response).await;
    assert_eq!(status, 404);
    assert_eq!(body, telegram_error(404, "Not Found"));

    assert_eq!(seen(&telegram).await.len(), 2, "refusals never go upstream");
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="rejected"} 3"#,
    );
    tgin.stop().await;
}

// Rate limiting and 429 smoothing.

const ADMIN_TOKEN: &str = "admin-token-value-123";
const SENT: &str = r#"{"ok":true,"result":{"message_id":1}}"#;

/// Bot `gw` (`TOKEN`) with `gateway` as its gateway block, and an admin
/// plane that takes [`ADMIN_TOKEN`].
fn limited_config(api: &str, gateway: &str) -> String {
    format!(
        r#"version: 1
runtime: {{ lame_duck: 0s, shutdown_grace: 2s }}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"], token: "{ADMIN_TOKEN}" }}
bots:
  - name: gw
    token: "{TOKEN}"
    api_url: "{api}"
    gateway: {gateway}
    routes: [{{ to: [workers] }}]
consumers:
  workers: {{ pull: {{}} }}
"#
    )
}

/// `429 Too Many Requests` as Telegram writes it.
fn flood(retry_after: u64) -> String {
    format!(
        r#"{{"ok":false,"error_code":429,"description":"Too Many Requests: retry after {retry_after}","parameters":{{"retry_after":{retry_after}}}}}"#
    )
}

/// POSTs `body` as JSON to the gateway's `method` of `TOKEN`.
async fn call(
    http: &reqwest::Client,
    addr: SocketAddr,
    method: &str,
    body: &Value,
) -> reqwest::Response {
    http.post(format!("http://{addr}/bot{TOKEN}/{method}"))
        .json(body)
        .send()
        .await
        .expect("response")
}

/// The status and raw body of a gateway call.
async fn status_and_body(response: reqwest::Response) -> (u16, String) {
    let status = response.status().as_u16();
    (status, response.text().await.expect("body"))
}

/// Tokio's clock, frozen: the test's runtime starts paused, and a blocking
/// task keeps the paused clock from jumping to the next timer whenever the
/// runtime waits for a socket (tokio holds that jump back while a blocking
/// task runs), so that time moves only by [`tokio::time::advance`]. Taken
/// first thing, every instant is then a whole number of milliseconds after
/// the timer driver's start: a timer due at an instant `advance` reaches
/// fires, where a sub-millisecond offset would leave it 1 ms short.
struct Frozen(std::sync::mpsc::Sender<()>);

fn freeze() -> Frozen {
    let (hold, held) = std::sync::mpsc::channel::<()>();
    drop(tokio::task::spawn_blocking(move || {
        let _ = held.recv();
    }));
    Frozen(hold)
}

impl Frozen {
    /// Real time again, for the shutdown's own timers.
    fn thaw(self) {
        drop(self.0);
        tokio::time::resume();
    }
}

/// `fut`, bounded by [`PATIENCE`] of real time: tokio's timers do not run
/// while the clock is frozen.
async fn within<F: Future>(what: &str, fut: F) -> F::Output {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(PATIENCE);
        let _ = tx.send(());
    });
    tokio::select! {
        out = fut => out,
        _ = rx => panic!("{what} did not happen within {PATIENCE:?}"),
    }
}

/// How long [`quiet`] watches, in real time: far longer than a request
/// takes over loopback, so one that was due would show up.
const QUIET: Duration = Duration::from_millis(300);

/// Asserts that no request reaches Telegram for [`QUIET`] of real time:
/// on the frozen clock, nothing the gateway still waits for may go.
async fn quiet(what: &str, arrivals: &mut mpsc::UnboundedReceiver<Arrival>) {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(QUIET);
        let _ = tx.send(());
    });
    tokio::select! {
        arrival = arrivals.recv() => {
            let at = arrival.map(|arrival| arrival.at);
            panic!("{what}: a request reached Telegram at {at:?}");
        }
        _ = rx => {}
    }
}

/// Yields until `ready`, for at most [`PATIENCE`] of real time.
async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(started.elapsed() < PATIENCE, "{what} never happened");
        tokio::task::yield_now().await;
    }
}

impl Tgin {
    fn has_metric(&self, line: &str) -> bool {
        self.metrics.encode().lines().any(|l| l == line)
    }

    /// `POST /v1/reload`, which answers once the reload is applied.
    async fn reload(&self) {
        let admin = self.admin.expect("an admin plane");
        let response = self
            .http
            .post(format!("http://{admin}/v1/reload"))
            .bearer_auth(ADMIN_TOKEN)
            .send()
            .await
            .expect("reload");
        assert_eq!(response.status(), 200, "{:?}", response.text().await);
    }
}

/// A request as Telegram saw it, stamped with tokio's clock.
struct Arrival {
    at: tokio::time::Instant,
    body: Bytes,
}

/// Telegram as a small server in this runtime: every request is stamped
/// and sent to the returned channel, and answered with `answers` in turn,
/// the last one repeating.
async fn stamped_telegram(
    answers: Vec<(u16, String)>,
) -> (SocketAddr, mpsc::UnboundedReceiver<Arrival>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let answers = Arc::new(answers);
    let router = axum::Router::new().fallback(move |body: Bytes| {
        let at = tokio::time::Instant::now();
        let n = served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (status, answer) = answers[n.min(answers.len() - 1)].clone();
        let _ = tx.send(Arrival { at, body });
        async move {
            axum::response::Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(answer))
                .expect("response")
        }
    });
    (serve(router).await, rx)
}

#[tokio::test(start_paused = true)]
async fn per_chat_shaping_delays_fourth_message() {
    let frozen = freeze();
    let (api, mut arrivals) = stamped_telegram(vec![(200, SENT.to_owned())]).await;
    // Defaults: shape up to 5 s; per chat 1/s after a burst of 3.
    let tgin = start(&limited_config(&format!("http://{api}"), "{}")).await;
    let t0 = tokio::time::Instant::now();

    let body = json!({ "chat_id": 42, "text": "hi" });
    let sends: Vec<_> = (0..4)
        .map(|_| {
            let (http, addr, body) = (tgin.http.clone(), tgin.addr, body.clone());
            tokio::spawn(async move { call(&http, addr, "sendMessage", &body).await.status() })
        })
        .collect();
    for _ in 0..3 {
        let arrival = within("an upstream request", arrivals.recv())
            .await
            .expect("open");
        assert_eq!(arrival.at, t0);
    }
    let delayed = r#"tgin_gateway_ratelimited_total{bot="gw",scope="chat",action="delayed"} 1"#;
    until("the fourth message's delay", || tgin.has_metric(delayed)).await;
    assert!(
        arrivals.try_recv().is_err(),
        "the fourth message went at once"
    );

    // Not a millisecond early: the booked delay is the whole second.
    tokio::time::advance(Duration::from_millis(999)).await;
    quiet("the fourth message before its slot", &mut arrivals).await;
    tokio::time::advance(Duration::from_millis(1)).await;
    let fourth = within("the fourth request", arrivals.recv())
        .await
        .expect("open");
    assert_eq!(fourth.at, t0 + Duration::from_secs(1));
    assert_eq!(&fourth.body[..], body.to_string().as_bytes());
    for send in sends {
        assert_eq!(within("a response", send).await.expect("sent"), 200);
    }
    tgin.assert_metric(delayed);
    tgin.assert_metric(
        r#"tgin_gateway_ratelimited_total{bot="gw",scope="global",action="delayed"} 0"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="ok"} 4"#,
    );
    frozen.thaw();
    tgin.stop().await;
}

/// What reaches Telegram, stamped against `t0`. The frozen clock moves only
/// after the requests that are due have been seen: `due[n]` requests are
/// awaited at `t0 + n` seconds. Before each next second the clock stops 1 ms
/// short of it, and [`quiet`] shows that nothing goes early. After the last
/// second, half a second more stays quiet.
async fn arrivals_by_second(
    arrivals: &mut mpsc::UnboundedReceiver<Arrival>,
    t0: tokio::time::Instant,
    due: &[usize],
) -> Vec<(Duration, Bytes)> {
    let mut seen = Vec::new();
    for (second, &count) in due.iter().enumerate() {
        if second > 0 {
            tokio::time::advance(Duration::from_millis(999)).await;
            quiet("a request before its second", arrivals).await;
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        for _ in 0..count {
            let arrival = within("a request due now", arrivals.recv())
                .await
                .expect("open");
            seen.push((arrival.at - t0, arrival.body));
        }
    }
    tokio::time::advance(Duration::from_millis(500)).await;
    quiet("a request after the last", arrivals).await;
    seen
}

/// The e2e shaping scenario's pattern: grammY sends five messages to one
/// private chat at once (`Promise.all`), with the default limits. They
/// reach Telegram at exactly 0, 0, 0, 1 s and 2 s.
#[tokio::test(start_paused = true)]
async fn five_at_once_to_one_chat_arrive_at_the_chat_rate() {
    let frozen = freeze();
    let (api, mut arrivals) = stamped_telegram(vec![(200, SENT.to_owned())]).await;
    let tgin = start(&limited_config(&format!("http://{api}"), "{}")).await;
    let t0 = tokio::time::Instant::now();
    let sends: Vec<_> = (1..=5)
        .map(|i| {
            let (http, addr) = (tgin.http.clone(), tgin.addr);
            let body = json!({ "chat_id": 42, "text": format!("burst {i}") });
            tokio::spawn(async move { call(&http, addr, "sendMessage", &body).await.status() })
        })
        .collect();
    // The clock stays at t0 until the whole burst is booked.
    let delayed = r#"tgin_gateway_ratelimited_total{bot="gw",scope="chat",action="delayed"} 2"#;
    until("the burst booked", || tgin.has_metric(delayed)).await;
    let seen = arrivals_by_second(&mut arrivals, t0, &[3, 1, 1]).await;
    let at: Vec<Duration> = seen.iter().map(|(at, _)| *at).collect();
    let s = Duration::from_secs;
    assert_eq!(at, [s(0), s(0), s(0), s(1), s(2)]);
    for send in sends {
        assert_eq!(within("a response", send).await.expect("sent"), 200);
    }
    frozen.thaw();
    tgin.stop().await;
}

/// The same burst while another chat's call of the same bot waits out
/// Telegram's 429 (`retry_after` 1) and books the limits again: the burst
/// still arrives at 0, 0, 0, 1 s and 2 s, and the retry at 1 s.
#[tokio::test(start_paused = true)]
async fn five_at_once_beside_a_429_retry_arrive_at_the_chat_rate() {
    let frozen = freeze();
    let (api, mut arrivals) = stamped_telegram(vec![(429, flood(1)), (200, SENT.to_owned())]).await;
    let tgin = start(&limited_config(&format!("http://{api}"), "{}")).await;
    let t0 = tokio::time::Instant::now();
    let flooded = {
        let (http, addr) = (tgin.http.clone(), tgin.addr);
        let body = json!({ "chat_id": 41, "text": "flood" });
        tokio::spawn(async move { call(&http, addr, "sendMessage", &body).await.status() })
    };
    let first = within("the flooded call", arrivals.recv())
        .await
        .expect("open");
    assert_eq!(first.at, t0);
    // Its 429 is back at tgin at t0, so the retry is due at exactly 1 s.
    let answered = r#"tgin_gateway_upstream_seconds_count{bot="gw",method="sendMessage"} 1"#;
    until("the 429 at tgin", || tgin.has_metric(answered)).await;
    let sends: Vec<_> = (1..=5)
        .map(|i| {
            let (http, addr) = (tgin.http.clone(), tgin.addr);
            let body = json!({ "chat_id": 42, "text": format!("burst {i}") });
            tokio::spawn(async move { call(&http, addr, "sendMessage", &body).await.status() })
        })
        .collect();
    let delayed = r#"tgin_gateway_ratelimited_total{bot="gw",scope="chat",action="delayed"} 2"#;
    until("the burst booked", || tgin.has_metric(delayed)).await;
    let seen = arrivals_by_second(&mut arrivals, t0, &[3, 2, 1]).await;
    let of = |needle: &[u8]| -> Vec<Duration> {
        seen.iter()
            .filter(|(_, body)| body.windows(needle.len()).any(|w| w == needle))
            .map(|(at, _)| *at)
            .collect()
    };
    let s = Duration::from_secs;
    assert_eq!(of(b"burst"), [s(0), s(0), s(0), s(1), s(2)]);
    assert_eq!(of(b"flood"), [s(1)]);
    assert_eq!(within("the retry", flooded).await.expect("sent"), 200);
    for send in sends {
        assert_eq!(within("a response", send).await.expect("sent"), 200);
    }
    frozen.thaw();
    tgin.stop().await;
}

#[tokio::test(start_paused = true)]
async fn reject_mode_answers_429_shaped() {
    let frozen = freeze();
    let telegram = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SENT, "application/json"))
        .mount(&telegram)
        .await;
    let tgin = start(&limited_config(
        &telegram.uri(),
        "{ rate_limit: { mode: reject } }",
    ))
    .await;

    let to = |chat: i64| json!({ "chat_id": chat, "text": "hi" });
    let send = |chat: i64| {
        let (http, addr) = (tgin.http.clone(), tgin.addr);
        let body = to(chat);
        within("a response", async move {
            call(&http, addr, "sendMessage", &body).await
        })
    };
    for _ in 0..3 {
        assert_eq!(send(42).await.status(), 200);
    }
    let response = send(42).await;
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(response.headers()["retry-after"], "1");
    assert_eq!(
        within("the body", status_and_body(response)).await,
        (429, flood(1))
    );
    assert_eq!(
        within("the record", seen(&telegram)).await.len(),
        3,
        "a refusal never goes upstream"
    );

    // Per chat: another chat is not limited.
    assert_eq!(send(43).await.status(), 200);
    // A second later the chat has a slot again.
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(send(42).await.status(), 200);
    assert_eq!(within("the record", seen(&telegram)).await.len(), 5);

    tgin.assert_metric(
        r#"tgin_gateway_ratelimited_total{bot="gw",scope="chat",action="rejected"} 1"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_ratelimited_total{bot="gw",scope="chat",action="delayed"} 0"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="rate_limited"} 1"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="ok"} 5"#,
    );
    frozen.thaw();
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_quota_applies_to_negative_ids_and_usernames() {
    let telegram = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SENT, "application/json"))
        .mount(&telegram)
        .await;
    let tgin = start(&limited_config(
        &telegram.uri(),
        "{ rate_limit: { mode: reject, per_chat: { count: 100, per: 1s, burst: 100 }, per_group: { count: 1, per: 1h, burst: 2 } } }",
    ))
    .await;
    let send = |body: Value| {
        let (http, addr) = (tgin.http.clone(), tgin.addr);
        async move { call(&http, addr, "sendMessage", &body).await }
    };
    let form = |chat: &str| {
        tgin.http
            .post(tgin.url(&format!("/bot{TOKEN}/sendMessage")))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("chat_id={chat}&text=hi"))
            .send()
    };

    // A negative id, as a number and as a string: one group.
    let a = send(json!({ "chat_id": -1001, "text": "a" })).await;
    assert_eq!(a.status(), 200);
    let b = send(json!({ "chat_id": -1001, "text": "b" })).await;
    assert_eq!(b.status(), 200);
    let c = send(json!({ "chat_id": "-1001", "text": "c" })).await;
    let (status, body) = status_and_body(c).await;
    assert_eq!(status, 429, "{body}");
    // A username, form-encoded and as JSON, in any case: one group.
    let a = form("%40SomeChannel").await.expect("response");
    assert_eq!(a.status(), 200);
    let b = form("@somechannel").await.expect("response");
    assert_eq!(b.status(), 200);
    let c = send(json!({ "chat_id": "@SOMECHANNEL", "text": "c" })).await;
    let (status, body) = status_and_body(c).await;
    assert_eq!(status, 429, "{body}");
    // A private chat has no group quota.
    for _ in 0..3 {
        let response = send(json!({ "chat_id": 7, "text": "x" })).await;
        assert_eq!(response.status(), 200);
    }

    assert_eq!(seen(&telegram).await.len(), 7);
    tgin.assert_metric(
        r#"tgin_gateway_ratelimited_total{bot="gw",scope="group",action="rejected"} 2"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_ratelimited_total{bot="gw",scope="chat",action="rejected"} 0"#,
    );
    tgin.stop().await;
}

/// A multipart body with `chat_id` and one small file.
fn multipart(chat_id: &str) -> (&'static str, String) {
    (
        "multipart/form-data; boundary=tginboundary",
        format!(
            "--tginboundary\r\nContent-Disposition: form-data; name=\"chat_id\"\r\n\r\n{chat_id}\r\n--tginboundary\r\nContent-Disposition: form-data; name=\"document\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--tginboundary--\r\n"
        ),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multipart_uses_global_only_and_is_never_retried() {
    let telegram = MockServer::start().await;
    for method in ["sendDocument", "sendMessage"] {
        Mock::given(path(format!("/bot{TOKEN}/{method}")))
            .respond_with(ResponseTemplate::new(429).set_body_raw(flood(1), "application/json"))
            .mount(&telegram)
            .await;
    }
    // One request per chat, four in all, and a retry would be allowed.
    let tgin = start(&limited_config(
        &telegram.uri(),
        "{ rate_limit: { mode: reject, global: { count: 1, per: 1m, burst: 4 }, per_chat: { count: 1, per: 1m, burst: 1 } }, retry_429: { max_wait: 10s } }",
    ))
    .await;
    let document = || {
        let (content_type, body) = multipart("42");
        tgin.http
            .post(tgin.url(&format!("/bot{TOKEN}/sendDocument")))
            .header("content-type", content_type)
            .body(body)
            .send()
    };

    // Twice to one chat: the per-chat quota does not apply to multipart,
    // and Telegram's 429 reaches the client after one upstream call.
    for sent in 1..=2 {
        let response = document().await.expect("response");
        assert_eq!(status_and_body(response).await, (429, flood(1)));
        assert_eq!(seen(&telegram).await.len(), sent, "never retried");
    }
    // A JSON body longer than 64 KiB streams: no chat key, no retry.
    let long = json!({ "chat_id": 42, "text": "x".repeat(70 * 1024) }).to_string();
    let response = tgin
        .http
        .post(tgin.url(&format!("/bot{TOKEN}/sendMessage")))
        .header("content-type", "application/json")
        .body(long.clone())
        .send()
        .await
        .expect("response");
    assert_eq!(status_and_body(response).await, (429, flood(1)));
    let requests = seen(&telegram).await;
    assert_eq!(requests.len(), 3, "never retried");
    assert_eq!(requests[2].body, long.as_bytes());
    // The same without a length: read up to the bound, then streamed whole.
    let mut stream = TcpStream::connect(tgin.addr).await.expect("connect");
    let head = format!(
        "POST /bot{TOKEN}/sendMessage HTTP/1.1\r\nHost: tgin\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.expect("head");
    for chunk in long.as_bytes().chunks(16 * 1024) {
        let framed = format!("{:x}\r\n", chunk.len());
        stream.write_all(framed.as_bytes()).await.expect("size");
        stream.write_all(chunk).await.expect("chunk");
        stream.write_all(b"\r\n").await.expect("chunk end");
    }
    stream.write_all(b"0\r\n\r\n").await.expect("last chunk");
    let raw = timeout(PATIENCE, read_response(&mut stream))
        .await
        .expect("a response");
    assert_eq!((raw.status, &raw.body[..]), (429, flood(1).as_bytes()));
    let requests = seen(&telegram).await;
    assert_eq!(requests.len(), 4, "never retried");
    assert_eq!(requests[3].body, long.as_bytes());

    // The global quota still applies: the fifth request is tgin's own 429,
    // one interval (1 min) after the burst of 4.
    let response = document().await.expect("response");
    assert_eq!(response.status(), 429);
    let (_, body) = json_of(response).await;
    let retry_after = body["parameters"]["retry_after"]
        .as_u64()
        .expect("retry_after");
    assert!((1..=60).contains(&retry_after), "{body}");
    assert_eq!(seen(&telegram).await.len(), 4);
    tgin.assert_metric(
        r#"tgin_gateway_ratelimited_total{bot="gw",scope="global",action="rejected"} 1"#,
    );
    tgin.assert_metric(r#"tgin_gateway_retries_total{bot="gw"} 0"#);
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendDocument",outcome="rate_limited"} 3"#,
    );
    tgin.stop().await;
}

#[tokio::test(start_paused = true)]
async fn upstream_429_retried_once_when_buffered() {
    let frozen = freeze();
    let (api, mut arrivals) = stamped_telegram(vec![(429, flood(2)), (200, SENT.to_owned())]).await;
    let tgin = start(&limited_config(&format!("http://{api}"), "{}")).await;
    let t0 = tokio::time::Instant::now();

    let body = json!({ "chat_id": 42, "text": "hi" });
    let send = {
        let (http, addr, body) = (tgin.http.clone(), tgin.addr, body.clone());
        tokio::spawn(
            async move { status_and_body(call(&http, addr, "sendMessage", &body).await).await },
        )
    };
    let first = within("the first request", arrivals.recv())
        .await
        .expect("open");
    assert_eq!(first.at, t0);
    // Telegram's 429 has arrived; the retry waits its `retry_after`.
    until("the 429's arrival", || {
        tgin.has_metric(r#"tgin_gateway_upstream_seconds_count{bot="gw",method="sendMessage"} 1"#)
    })
    .await;
    // Not a millisecond early: the retry waits the whole `retry_after`.
    tokio::time::advance(Duration::from_millis(1_999)).await;
    quiet("the retry before retry_after", &mut arrivals).await;
    tokio::time::advance(Duration::from_millis(1)).await;
    let second = within("the retry", arrivals.recv()).await.expect("open");
    assert_eq!(second.at, t0 + Duration::from_secs(2));
    assert_eq!(second.body, first.body);
    assert_eq!(&first.body[..], body.to_string().as_bytes());
    assert_eq!(
        within("the response", send).await.expect("sent"),
        (200, SENT.to_owned())
    );
    assert!(arrivals.try_recv().is_err(), "at most two upstream calls");

    tgin.assert_metric(r#"tgin_gateway_retries_total{bot="gw"} 1"#);
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="ok"} 1"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="rate_limited"} 0"#,
    );
    tgin.assert_metric(r#"tgin_gateway_upstream_seconds_count{bot="gw",method="sendMessage"} 2"#);
    frozen.thaw();
    tgin.stop().await;
}

/// Telegram that never answers `getChat`, answers a body containing
/// `flood` with a 429 (`retry_after` 2) and anything else with [`SENT`].
/// Every request is stamped and sent to the returned channel.
async fn slow_telegram() -> (SocketAddr, mpsc::UnboundedReceiver<Arrival>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let router = axum::Router::new().fallback(move |uri: axum::http::Uri, body: Bytes| {
        let at = tokio::time::Instant::now();
        let flooded = body.windows(5).any(|w| w == b"flood");
        let _ = tx.send(Arrival { at, body });
        async move {
            if uri.path().ends_with("/getChat") {
                std::future::pending::<()>().await;
            }
            let (status, answer) = if flooded {
                (429, flood(2))
            } else {
                (200, SENT.to_owned())
            };
            axum::response::Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(answer))
                .expect("response")
        }
    });
    (serve(router).await, rx)
}

/// Sends a whole JSON `method` call to the gateway on a connection of its
/// own; dropping the stream leaves.
async fn leaving_client(addr: SocketAddr, method: &str, body: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "POST /bot{TOKEN}/{method} HTTP/1.1\r\nHost: tgin\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.expect("request");
    stream
}

/// A client that leaves after its request arrived whole counts
/// `client_closed`, whether tgin was waiting for Telegram's response
/// headers, out a shaping delay or out Telegram's 429; and what it left
/// never goes to Telegram later.
#[tokio::test(start_paused = true)]
async fn client_leaving_mid_request_is_client_closed() {
    let frozen = freeze();
    let (api, mut arrivals) = slow_telegram().await;
    let tgin = start(&limited_config(&format!("http://{api}"), "{}")).await;
    let closed = |method: &str, n: u32| {
        format!(
            r#"tgin_gateway_requests_total{{bot="gw",method="{method}",outcome="client_closed"}} {n}"#
        )
    };

    // Waiting for Telegram's response headers.
    let client = leaving_client(tgin.addr, "getChat", r#"{"chat_id":7}"#).await;
    let arrival = within("getChat upstream", arrivals.recv())
        .await
        .expect("open");
    assert_eq!(&arrival.body[..], br#"{"chat_id":7}"#);
    drop(client);
    until("getChat counted", || tgin.has_metric(&closed("getChat", 1))).await;

    // Waiting out a shaping delay: the chat's fourth message.
    let body = json!({ "chat_id": 42, "text": "hi" });
    for _ in 0..3 {
        let sent = status_and_body(call(&tgin.http, tgin.addr, "sendMessage", &body).await).await;
        assert_eq!(sent, (200, SENT.to_owned()));
        within("a message upstream", arrivals.recv())
            .await
            .expect("open");
    }
    let client = leaving_client(tgin.addr, "sendMessage", &body.to_string()).await;
    until("the fourth message's delay", || {
        tgin.has_metric(
            r#"tgin_gateway_ratelimited_total{bot="gw",scope="chat",action="delayed"} 1"#,
        )
    })
    .await;
    drop(client);
    until("the delayed message counted", || {
        tgin.has_metric(&closed("sendMessage", 1))
    })
    .await;

    // Waiting out Telegram's 429, once the global slot the abandoned
    // message kept at 1 s has passed.
    tokio::time::advance(Duration::from_secs(2)).await;
    let client = leaving_client(tgin.addr, "sendMessage", r#"{"chat_id":43,"text":"flood"}"#).await;
    within("the flooded message upstream", arrivals.recv())
        .await
        .expect("open");
    until("the 429's arrival", || {
        tgin.has_metric(r#"tgin_gateway_upstream_seconds_count{bot="gw",method="sendMessage"} 4"#)
    })
    .await;
    drop(client);
    until("the flooded message counted", || {
        tgin.has_metric(&closed("sendMessage", 2))
    })
    .await;

    // What the clients left never goes later.
    tokio::time::advance(Duration::from_secs(3)).await;
    quiet("an abandoned request", &mut arrivals).await;
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="ok"} 3"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="rate_limited"} 0"#,
    );
    tgin.assert_metric(r#"tgin_gateway_retries_total{bot="gw"} 0"#);
    frozen.thaw();
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_429_longer_than_max_wait_passes_through() {
    let telegram = MockServer::start().await;
    // `sendMessage`: 2 s, over `max_wait`.
    Mock::given(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(429).set_body_raw(flood(2), "application/json"))
        .mount(&telegram)
        .await;
    // `sendPhoto`: a 429 without `retry_after`.
    let bare = r#"{"ok":false,"error_code":429,"description":"Too Many Requests"}"#;
    Mock::given(path(format!("/bot{TOKEN}/sendPhoto")))
        .respond_with(ResponseTemplate::new(429).set_body_raw(bare, "application/json"))
        .mount(&telegram)
        .await;
    // `sendDice`: exactly `max_wait`, once; then success.
    Mock::given(path(format!("/bot{TOKEN}/sendDice")))
        .respond_with(ResponseTemplate::new(429).set_body_raw(flood(1), "application/json"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&telegram)
        .await;
    Mock::given(path(format!("/bot{TOKEN}/sendDice")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SENT, "application/json"))
        .mount(&telegram)
        .await;
    // `sendVoice`: a zero `retry_after` is never waited for.
    Mock::given(path(format!("/bot{TOKEN}/sendVoice")))
        .respond_with(ResponseTemplate::new(429).set_body_raw(flood(0), "application/json"))
        .mount(&telegram)
        .await;
    let tgin = start(&limited_config(
        &telegram.uri(),
        "{ retry_429: { max_wait: 1s } }",
    ))
    .await;
    let chat = json!({ "chat_id": 42 });
    let calls = |method: &'static str| {
        let telegram = &telegram;
        async move {
            seen(telegram)
                .await
                .iter()
                .filter(|r| r.url.path().ends_with(method))
                .count()
        }
    };

    let response = call(&tgin.http, tgin.addr, "sendMessage", &chat).await;
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(status_and_body(response).await, (429, flood(2)));
    assert_eq!(calls("/sendMessage").await, 1);
    let response = call(&tgin.http, tgin.addr, "sendPhoto", &chat).await;
    assert_eq!(status_and_body(response).await, (429, bare.to_owned()));
    assert_eq!(calls("/sendPhoto").await, 1);
    // The bound is inclusive.
    let response = call(&tgin.http, tgin.addr, "sendDice", &chat).await;
    assert_eq!(status_and_body(response).await, (200, SENT.to_owned()));
    assert_eq!(calls("/sendDice").await, 2);
    let response = call(&tgin.http, tgin.addr, "sendVoice", &chat).await;
    assert_eq!(status_and_body(response).await, (429, flood(0)));
    assert_eq!(calls("/sendVoice").await, 1);

    tgin.assert_metric(r#"tgin_gateway_retries_total{bot="gw"} 1"#);
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="rate_limited"} 1"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendPhoto",outcome="rate_limited"} 1"#,
    );
    tgin.stop().await;
}

/// Review Focus 3: a request goes upstream at most twice, even when
/// Telegram answers the second time with a 429 that could be retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_429_goes_upstream_at_most_twice() {
    let telegram = MockServer::start().await;
    let again = r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 1 (again)","parameters":{"retry_after":1}}"#;
    Mock::given(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(429).set_body_raw(flood(1), "application/json"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&telegram)
        .await;
    Mock::given(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(429).set_body_raw(again, "application/json"))
        .mount(&telegram)
        .await;
    let tgin = start(&limited_config(
        &telegram.uri(),
        "{ retry_429: { max_wait: 1s } }",
    ))
    .await;
    let response = call(
        &tgin.http,
        tgin.addr,
        "sendMessage",
        &json!({ "chat_id": 42 }),
    )
    .await;
    assert_eq!(status_and_body(response).await, (429, again.to_owned()));
    assert_eq!(seen(&telegram).await.len(), 2, "at most two upstream calls");
    tgin.assert_metric(r#"tgin_gateway_retries_total{bot="gw"} 1"#);
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="rate_limited"} 1"#,
    );
    tgin.stop().await;
}

/// The transparent retry does not depend on the rate-limit mode: with
/// `mode: off` a method of `rate_limit.methods` is still buffered and
/// retried once, and its retry books no quota. Off mode reads no chat, so
/// only the global quota could be in play: one request a minute, which a
/// booked resend of `sendMessage` would take, refusing `sendPhoto`'s. A
/// method outside the list streams and is not retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_is_independent_of_rate_limit_mode() {
    let telegram = MockServer::start().await;
    for method in ["sendMessage", "sendPhoto", "getChat"] {
        Mock::given(path(format!("/bot{TOKEN}/{method}")))
            .respond_with(ResponseTemplate::new(429).set_body_raw(flood(1), "application/json"))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&telegram)
            .await;
    }
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SENT, "application/json"))
        .mount(&telegram)
        .await;
    let tgin = start(&limited_config(
        &telegram.uri(),
        "{ rate_limit: { mode: off, global: { count: 1, per: 1m, burst: 1 } }, retry_429: { max_wait: 1s } }",
    ))
    .await;
    let chat = json!({ "chat_id": 42 });

    let response = call(&tgin.http, tgin.addr, "sendMessage", &chat).await;
    assert_eq!(status_and_body(response).await, (200, SENT.to_owned()));
    let requests = seen(&telegram).await;
    assert_eq!(requests.len(), 2, "retried once");
    assert_eq!(requests[1].body, requests[0].body);
    let response = call(&tgin.http, tgin.addr, "sendPhoto", &chat).await;
    assert_eq!(status_and_body(response).await, (200, SENT.to_owned()));
    assert_eq!(seen(&telegram).await.len(), 4, "retried once");
    let response = call(&tgin.http, tgin.addr, "getChat", &chat).await;
    assert_eq!(status_and_body(response).await, (429, flood(1)));
    assert_eq!(seen(&telegram).await.len(), 5, "never retried");

    tgin.assert_metric(r#"tgin_gateway_retries_total{bot="gw"} 2"#);
    for scope in ["global", "chat", "group"] {
        for action in ["delayed", "rejected"] {
            tgin.assert_metric(&format!(
                r#"tgin_gateway_ratelimited_total{{bot="gw",scope="{scope}",action="{action}"}} 0"#
            ));
        }
    }
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_429_retry_denied_by_limiter_passes_through() {
    let telegram = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(429).set_body_raw(flood(1), "application/json"))
        .mount(&telegram)
        .await;
    // The chat's only slot this minute goes to the first send.
    let tgin = start(&limited_config(
        &telegram.uri(),
        "{ rate_limit: { mode: reject, per_chat: { count: 1, per: 1m, burst: 1 } } }",
    ))
    .await;
    let response = call(
        &tgin.http,
        tgin.addr,
        "sendMessage",
        &json!({ "chat_id": 42 }),
    )
    .await;
    assert_eq!(status_and_body(response).await, (429, flood(1)));
    assert_eq!(seen(&telegram).await.len(), 1);
    tgin.assert_metric(r#"tgin_gateway_retries_total{bot="gw"} 0"#);
    tgin.assert_metric(
        r#"tgin_gateway_ratelimited_total{bot="gw",scope="chat",action="rejected"} 1"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="rate_limited"} 1"#,
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_send_methods_unlimited() {
    let telegram = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SENT, "application/json"))
        .mount(&telegram)
        .await;
    // One request a minute, for the bot and per chat.
    let tgin = start(&limited_config(
        &telegram.uri(),
        "{ rate_limit: { mode: reject, global: { count: 1, per: 1m, burst: 1 }, per_chat: { count: 1, per: 1m, burst: 1 } } }",
    ))
    .await;
    let chat = json!({ "chat_id": 42 });
    for method in [
        "getChat",
        "getChat",
        "getChat",
        "answerCallbackQuery",
        "setMyCommands",
    ] {
        let response = call(&tgin.http, tgin.addr, method, &chat).await;
        assert_eq!(response.status(), 200, "{method}");
    }
    let response = call(&tgin.http, tgin.addr, "sendMessage", &chat).await;
    assert_eq!(response.status(), 200);
    // Method names ignore case: this is `sendMessage` again.
    let other = json!({ "chat_id": 43 });
    let response = call(&tgin.http, tgin.addr, "SENDMESSAGE", &other).await;
    assert_eq!(response.status(), 429);
    let response = call(&tgin.http, tgin.addr, "copyMessages", &other).await;
    assert_eq!(response.status(), 429);
    assert_eq!(seen(&telegram).await.len(), 6);
    tgin.assert_metric(
        r#"tgin_gateway_ratelimited_total{bot="gw",scope="global",action="rejected"} 2"#,
    );
    tgin.assert_metric(r#"tgin_gateway_requests_total{bot="gw",method="getChat",outcome="ok"} 3"#);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limiter_state_survives_reload_with_same_config() {
    let telegram = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SENT, "application/json"))
        .mount(&telegram)
        .await;
    let gateway = |burst: u32, idle: u32| {
        format!(
            "{{ upstream_idle_timeout: {idle}s, rate_limit: {{ mode: reject, per_chat: {{ count: 1, per: 1m, burst: {burst} }} }} }}"
        )
    };
    let tgin = start(&limited_config(&telegram.uri(), &gateway(3, 60))).await;
    let chat = json!({ "chat_id": 42 });
    for _ in 0..3 {
        let response = call(&tgin.http, tgin.addr, "sendMessage", &chat).await;
        assert_eq!(response.status(), 200);
    }
    // The same file: the bot's entry is kept whole.
    tgin.reload().await;
    let response = call(&tgin.http, tgin.addr, "sendMessage", &chat).await;
    assert_eq!(response.status(), 429, "the reload reset the chat's quota");

    // Another idle timeout: a new entry, which shares the limiter.
    std::fs::write(&tgin.file, limited_config(&telegram.uri(), &gateway(3, 30))).expect("rewrite");
    tgin.reload().await;
    let response = call(&tgin.http, tgin.addr, "sendMessage", &chat).await;
    assert_eq!(
        response.status(),
        429,
        "the rebuilt entry reset the chat's quota"
    );

    // A changed limit is a new limiter.
    std::fs::write(&tgin.file, limited_config(&telegram.uri(), &gateway(4, 30))).expect("rewrite");
    tgin.reload().await;
    let response = call(&tgin.http, tgin.addr, "sendMessage", &chat).await;
    assert_eq!(response.status(), 200);
    assert_eq!(seen(&telegram).await.len(), 4);
    tgin.stop().await;
}

/// An `api_url` misconfigured to point at this tgin: the gateway's
/// upstream request carries the process's `Via`, so tgin refuses it with
/// `508 Loop Detected` instead of forwarding it to itself again and again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_url_pointing_at_tgin_is_refused_not_looped() {
    let tgin = start(&limited_config("http://127.0.0.1:9", "{}")).await;
    std::fs::write(
        &tgin.file,
        limited_config(&format!("http://{}", tgin.addr), "{}"),
    )
    .expect("rewrite");
    tgin.reload().await;
    for (method, body) in [
        ("getChat", json!({ "chat_id": 1 })),
        ("sendMessage", json!({ "chat_id": 1, "text": "hi" })),
    ] {
        let response = timeout(PATIENCE, call(&tgin.http, tgin.addr, method, &body))
            .await
            .unwrap_or_else(|_| panic!("{method} answered in time, without a loop"));
        let (status, body) = json_of(response).await;
        assert_eq!(status, 508, "{method}: {body}");
        assert_eq!(
            body["error_code"], 508,
            "{method}: the data plane's own refusal"
        );
        tgin.assert_metric(&format!(
            r#"tgin_gateway_requests_total{{bot="gw",method="{method}",outcome="upstream_error"}} 1"#
        ));
    }
    tgin.stop().await;
}

// Waiting for the Bot API server: the upload's idle bound, the response
// timeout, and what each 502 says.

/// The 502 once the server had the whole request and no answer came.
const NO_ANSWER: &str =
    "Bad Gateway: the Bot API server did not answer; the call may have been executed";
/// The 502 when no whole request reached the server.
const UNREACHABLE: &str = "Bad Gateway: the Bot API server is unreachable";

/// Bot `name` (`TOKEN`, feeding `workers`) on the `local` upstream `tba`
/// at `api` with `response_timeout`, and a 1 s `upstream_idle_timeout`.
fn local_config(api: &str, name: &str, response_timeout: &str) -> String {
    format!(
        r#"version: 1
runtime: {{ lame_duck: 0s, shutdown_grace: 2s }}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"] }}
upstreams:
  tba: {{ kind: local, url: "{api}", response_timeout: {response_timeout} }}
bots:
  - name: {name}
    token: "{TOKEN}"
    upstream: tba
    gateway: {{ upstream_idle_timeout: 1s }}
    routes: [{{ to: [workers] }}]
consumers:
  workers: {{ pull: {{}} }}
"#
    )
}

/// A Bot API server in this runtime that reads each request's whole body,
/// sends it to the returned channel stamped with tokio's clock, and then,
/// by the query: `mode=late` answers [`SENT`] 3 s later, anything else
/// never answers.
async fn waiting_server() -> (SocketAddr, mpsc::UnboundedReceiver<Arrival>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let router = axum::Router::new().fallback(move |uri: axum::http::Uri, body: Bytes| {
        let at = tokio::time::Instant::now();
        let late = uri.query() == Some("mode=late");
        let _ = tx.send(Arrival { at, body });
        async move {
            if late {
                tokio::time::sleep(Duration::from_secs(3)).await;
            } else {
                std::future::pending::<()>().await;
            }
            axum::response::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(SENT))
                .expect("response")
        }
    });
    (serve(router).await, rx)
}

/// A Bot API server that sends the length of each request body frame to
/// the returned channel as it arrives, and never answers.
async fn reading_server() -> (SocketAddr, mpsc::UnboundedReceiver<usize>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let router = axum::Router::new().fallback(move |body: Body| {
        let tx = tx.clone();
        async move {
            let mut frames = body.into_data_stream();
            while let Some(Ok(frame)) = futures_util::StreamExt::next(&mut frames).await {
                let _ = tx.send(frame.len());
            }
            std::future::pending::<()>().await;
        }
    });
    (serve(router).await, rx)
}

/// A Bot API server that reads each request whole (its head, then
/// `Content-Length` bytes), counts it, writes `reply` (nothing, or the
/// start of a response) and closes the connection.
async fn closing_server(reply: &'static [u8]) -> (SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&requests);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let counted = Arc::clone(&counted);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map_or(0, |v| v.trim().parse().expect("length"));
                    if buf.len() >= end + 4 + length {
                        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let _ = stream.write_all(reply).await;
                        return;
                    }
                }
            });
        }
    });
    (addr, requests)
}

/// Sends `request` from a task of its own, so the test can step the clock
/// while it waits.
fn spawn_call(request: reqwest::RequestBuilder) -> JoinHandle<(u16, Value)> {
    tokio::spawn(async move { json_of(request.send().await.expect("response")).await })
}

/// Asserts that `response` has not come after [`QUIET`] of real time: on
/// the frozen clock, the gateway still waits.
async fn unanswered<T>(what: &str, response: &JoinHandle<T>) {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(QUIET);
        let _ = tx.send(());
    });
    let _ = rx.await;
    assert!(!response.is_finished(), "{what}: answered already");
}

/// Once a local server has a large upload whole, its answer is awaited for
/// the upstream's `response_timeout` (10 s), not `upstream_idle_timeout`
/// (1 s), which bounds only the upload's stalls. An answer three idle
/// timeouts later is relayed. No answer gets the "may have been executed"
/// 502 at exactly `response_timeout` after the upload ended, and the call
/// is never sent again, a buffered one that a 429 could resend included.
#[tokio::test(start_paused = true)]
async fn response_wait_bounded_by_response_timeout_not_idle() {
    let frozen = freeze();
    let (api, mut arrivals) = waiting_server().await;
    let tgin = start(&local_config(&format!("http://{api}"), "gw", "10s")).await;
    let (content_type, upload) = multipart("7");
    let document = |mode: &str| {
        tgin.http
            .post(tgin.url(&format!("/bot{TOKEN}/sendDocument?mode={mode}")))
            .header("content-type", content_type)
            .body(upload.clone())
    };
    let millis = Duration::from_millis;
    let mut requests = 0;

    // Answered three idle timeouts after the upload ended.
    let late = spawn_call(document("late"));
    let arrival = within("the upload", arrivals.recv()).await.expect("open");
    requests += 1;
    assert_eq!(&arrival.body[..], upload.as_bytes(), "the whole upload");
    for _ in 0..2 {
        tokio::time::advance(millis(1_000)).await;
        unanswered("an upload one more idle timeout on", &late).await;
    }
    tokio::time::advance(millis(999)).await;
    unanswered("an upload before its answer", &late).await;
    tokio::time::advance(millis(1)).await;
    let (status, body) = within("the late answer", late).await.expect("sent");
    assert_eq!((status, body), (200, serde_json::from_str(SENT).unwrap()));
    assert_eq!(arrival.at.elapsed(), millis(3_000));

    // Never answered: the streamed upload, then a buffered call.
    let message = tgin
        .http
        .post(tgin.url(&format!("/bot{TOKEN}/sendMessage?mode=never")))
        .json(&json!({ "chat_id": 7, "text": "hi" }));
    for (what, request) in [
        ("sendDocument", document("never")),
        ("sendMessage", message),
    ] {
        let response = spawn_call(request);
        let arrival = within(what, arrivals.recv()).await.expect("open");
        requests += 1;
        tokio::time::advance(millis(9_999)).await;
        unanswered(what, &response).await;
        tokio::time::advance(millis(1)).await;
        let (status, body) = tokio::select! {
            biased;
            again = arrivals.recv() => {
                panic!("{what} sent again at {:?}", again.map(|again| again.at - arrival.at));
            }
            response = within("the 502", response) => response.expect("sent"),
        };
        assert_eq!(status, 502, "{what}");
        assert_eq!(body, telegram_error(502, NO_ANSWER), "{what}");
        assert_eq!(arrival.at.elapsed(), millis(10_000), "{what}");
    }

    // Never sent again, however long the clock runs on.
    tokio::time::advance(Duration::from_secs(60)).await;
    quiet("a call sent again", &mut arrivals).await;
    assert_eq!(requests, 3, "one upstream request per call");
    for line in [
        r#"tgin_gateway_requests_total{bot="gw",method="sendDocument",outcome="ok"} 1"#,
        r#"tgin_gateway_requests_total{bot="gw",method="sendDocument",outcome="unreachable"} 1"#,
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="unreachable"} 1"#,
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="client_closed"} 0"#,
        r#"tgin_gateway_retries_total{bot="gw"} 0"#,
    ] {
        tgin.assert_metric(line);
    }
    frozen.thaw();
    tgin.stop().await;
}

/// A bot on the `api_url` shorthand (what a v1 config has) keeps the v1
/// bound: after its upload, it waits its whole `upstream_idle_timeout`
/// (90 s) for the headers, not the 60 s a cloud upstream's
/// `response_timeout` defaults to, then gets the 502 in the new wording.
#[tokio::test(start_paused = true)]
async fn shorthand_response_wait_is_the_idle_bound() {
    let frozen = freeze();
    let (api, mut arrivals) = waiting_server().await;
    let tgin = start(&limited_config(
        &format!("http://{api}"),
        "{ upstream_idle_timeout: 90s }",
    ))
    .await;
    let (content_type, upload) = multipart("7");
    let response = spawn_call(
        tgin.http
            .post(tgin.url(&format!("/bot{TOKEN}/sendDocument?mode=never")))
            .header("content-type", content_type)
            .body(upload.clone()),
    );
    let arrival = within("the upload", arrivals.recv()).await.expect("open");
    assert_eq!(&arrival.body[..], upload.as_bytes(), "the whole upload");
    tokio::time::advance(Duration::from_secs(60)).await;
    unanswered("a shorthand bot at 60 s", &response).await;
    tokio::time::advance(Duration::from_millis(29_999)).await;
    unanswered("a shorthand bot before its idle bound", &response).await;
    tokio::time::advance(Duration::from_millis(1)).await;
    let (status, body) = within("the 502", response).await.expect("sent");
    assert_eq!(status, 502);
    assert_eq!(body, telegram_error(502, NO_ANSWER));
    assert_eq!(arrival.at.elapsed(), Duration::from_secs(90));
    quiet("a call sent again", &mut arrivals).await;
    frozen.thaw();
    tgin.stop().await;
}

/// While the upload runs, `upstream_idle_timeout` still bounds a stall,
/// however long `response_timeout` is: 100 of 1000 announced bytes, then
/// nothing, get the 502 exactly 1 s after the last bytes moved. The server
/// never had the whole request, so the 502 says it is unreachable, not
/// that the call may have been executed.
#[tokio::test(start_paused = true)]
async fn idle_stall_mid_upload_still_502() {
    let frozen = freeze();
    let (api, mut frames) = reading_server().await;
    let tgin = start(&local_config(&format!("http://{api}"), "staller", "10m")).await;

    let mut stream = TcpStream::connect(tgin.addr).await.expect("connect");
    let head = format!(
        "POST /bot{TOKEN}/sendDocument HTTP/1.1\r\nHost: tgin\r\nContent-Type: multipart/form-data; boundary=b\r\nContent-Length: 1000\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.expect("head");
    stream
        .write_all(&[b'x'; 100])
        .await
        .expect("part of the body");
    let mut received = 0;
    while received < 100 {
        received += within("the first bytes upstream", frames.recv())
            .await
            .expect("open");
    }
    let reader = tokio::spawn(async move {
        let raw = read_response(&mut stream).await;
        // Kept open until the response is read.
        drop(stream);
        raw
    });
    tokio::time::advance(Duration::from_millis(999)).await;
    unanswered("a stalled upload before the idle timeout", &reader).await;
    tokio::time::advance(Duration::from_millis(1)).await;
    let stalled = within("the 502", reader).await.expect("read");
    assert_eq!(stalled.status, 502);
    assert_eq!(
        serde_json::from_slice::<Value>(&stalled.body).expect("JSON"),
        telegram_error(502, UNREACHABLE)
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="staller",method="sendDocument",outcome="unreachable"} 1"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="staller",method="sendDocument",outcome="client_closed"} 0"#,
    );
    assert!(
        LOGS.text().lines().any(|l| l.contains("WARN")
            && l.contains("bot=staller")
            && l.contains("upstream_idle_timeout")),
        "the stall is logged with the bound it ran into"
    );
    frozen.thaw();
    tgin.stop().await;
}

/// A server that refuses the connection got nothing, whatever the body: a
/// JSON call read whole first, a streamed upload, and a GET without a body
/// (sent the moment it connects). Each gets the "unreachable" 502 and is
/// counted `unreachable`; the log says "Bot API server", not "Telegram".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_failure_says_unreachable() {
    // Bound but not listening, and held for the whole test: connecting is
    // refused, and no other socket can take the port meanwhile.
    let closed = tokio::net::TcpSocket::new_v4().unwrap();
    closed.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = closed.local_addr().unwrap().port();
    // An idle timeout longer than a refused connect takes (Windows retries
    // one for about 2 s).
    let config = local_config(&format!("http://127.0.0.1:{port}"), "refused", "10s")
        .replace("upstream_idle_timeout: 1s", "upstream_idle_timeout: 30s");
    let tgin = start(&config).await;
    let (content_type, upload) = multipart("7");
    let calls = [
        (
            "sendMessage",
            tgin.http
                .post(tgin.url(&format!("/bot{TOKEN}/sendMessage")))
                .json(&json!({ "chat_id": 7, "text": "hi" })),
        ),
        (
            "sendDocument",
            tgin.http
                .post(tgin.url(&format!("/bot{TOKEN}/sendDocument")))
                .header("content-type", content_type)
                .body(upload),
        ),
        (
            "getChat",
            tgin.http
                .get(tgin.url(&format!("/bot{TOKEN}/getChat?chat_id=7"))),
        ),
    ];
    for (method, request) in calls {
        let response = timeout(PATIENCE, request.send())
            .await
            .expect("answered in time")
            .expect("response");
        let (status, body) = json_of(response).await;
        assert_eq!(status, 502, "{method}");
        assert_eq!(body, telegram_error(502, UNREACHABLE), "{method}");
        tgin.assert_metric(&format!(
            r#"tgin_gateway_requests_total{{bot="refused",method="{method}",outcome="unreachable"}} 1"#
        ));
    }
    let logs = LOGS.text();
    let lines: Vec<&str> = logs.lines().filter(|l| l.contains("bot=refused")).collect();
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("WARN") && l.contains("the Bot API server is unreachable"))
            .count(),
        3,
        "one warning per call:\n{}",
        lines.join("\n")
    );
    assert!(
        !lines.iter().any(|l| l.contains("Telegram")),
        "a self-hosted server is no Telegram:\n{}",
        lines.join("\n")
    );
    assert!(!logs.contains(TOKEN), "the token reached a log line");
    tgin.stop().await;
}

/// A server that reads the whole request and closes the connection without
/// an answer may have executed the call: the 502 says so, for a buffered
/// call and a streamed upload alike, and neither goes upstream again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_lost_after_upload_may_have_been_executed() {
    let (api, requests) = closing_server(b"").await;
    let tgin = start(&local_config(&format!("http://{api}"), "dropped", "10s")).await;
    let (content_type, upload) = multipart("7");
    let calls = [
        (
            "sendMessage",
            tgin.http
                .post(tgin.url(&format!("/bot{TOKEN}/sendMessage")))
                .json(&json!({ "chat_id": 7, "text": "hi" })),
        ),
        (
            "sendDocument",
            tgin.http
                .post(tgin.url(&format!("/bot{TOKEN}/sendDocument")))
                .header("content-type", content_type)
                .body(upload),
        ),
    ];
    for (method, request) in calls {
        let response = timeout(PATIENCE, request.send())
            .await
            .expect("answered in time")
            .expect("response");
        let (status, body) = json_of(response).await;
        assert_eq!(status, 502, "{method}");
        assert_eq!(body, telegram_error(502, NO_ANSWER), "{method}");
        tgin.assert_metric(&format!(
            r#"tgin_gateway_requests_total{{bot="dropped",method="{method}",outcome="unreachable"}} 1"#
        ));
    }
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "one upstream request per call"
    );
    assert!(
        LOGS.text().lines().any(|l| l.contains("WARN")
            && l.contains("bot=dropped")
            && l.contains("the call may have been executed")),
        "the lost answer is logged with the bot's name"
    );
    tgin.stop().await;
}

/// A 429 whose body breaks off: the status already says flood control
/// refused the call, so it was not executed. The client is told the server
/// is unreachable (it may send again), never "may have been executed"; the
/// outcome is `upstream_error` (an HTTP answer came), and the buffered call
/// is not sent again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_429_body_was_not_executed() {
    let (api, requests) = closing_server(
        b"HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 200\r\n\r\n{\"ok\":false,\"error_code\":429",
    )
    .await;
    let tgin = start(&limited_config(&format!("http://{api}"), "{}")).await;
    let response = timeout(
        PATIENCE,
        call(
            &tgin.http,
            tgin.addr,
            "sendMessage",
            &json!({ "chat_id": 7, "text": "hi" }),
        ),
    )
    .await
    .expect("answered in time");
    let (status, body) = json_of(response).await;
    assert_eq!(status, 502);
    assert_eq!(body, telegram_error(502, UNREACHABLE));
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="upstream_error"} 1"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="unreachable"} 0"#,
    );
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "not sent again"
    );
    tgin.assert_metric(r#"tgin_gateway_retries_total{bot="gw"} 0"#);
    tgin.stop().await;
}

/// An explicit upstream's `response_timeout` is honoured as configured,
/// even below `upstream_idle_timeout` (no max of the two): with 2 s against
/// an idle bound of 30 s, the "may have been executed" 502 comes exactly
/// 2 s after the upload ended.
#[tokio::test(start_paused = true)]
async fn response_timeout_below_idle_is_honoured() {
    let frozen = freeze();
    let (api, mut arrivals) = waiting_server().await;
    let config = local_config(&format!("http://{api}"), "brief", "2s")
        .replace("upstream_idle_timeout: 1s", "upstream_idle_timeout: 30s");
    let tgin = start(&config).await;
    let (content_type, upload) = multipart("7");
    let response = spawn_call(
        tgin.http
            .post(tgin.url(&format!("/bot{TOKEN}/sendDocument?mode=never")))
            .header("content-type", content_type)
            .body(upload.clone()),
    );
    let arrival = within("the upload", arrivals.recv()).await.expect("open");
    assert_eq!(&arrival.body[..], upload.as_bytes(), "the whole upload");
    tokio::time::advance(Duration::from_millis(1_999)).await;
    unanswered("an upload before its response_timeout", &response).await;
    tokio::time::advance(Duration::from_millis(1)).await;
    let (status, body) = within("the 502", response).await.expect("sent");
    assert_eq!(status, 502);
    assert_eq!(body, telegram_error(502, NO_ANSWER));
    assert_eq!(arrival.at.elapsed(), Duration::from_secs(2));
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="brief",method="sendDocument",outcome="unreachable"} 1"#,
    );
    quiet("a call sent again", &mut arrivals).await;
    frozen.thaw();
    tgin.stop().await;
}

/// Whether the scrape has a series of the family `family` for `bot`.
fn has_series(text: &str, family: &str, bot: &str) -> bool {
    let prefix = format!("{family}{{bot=\"{bot}\"");
    text.lines().any(|l| l.starts_with(&prefix))
}

/// `tgin_upstream_up{bot}` appears with the bot's first upstream outcome,
/// not before; `tgin_bot_info` names each bot's upstream from the start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_up_absent_before_first_outcome() {
    let telegram = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/getChat")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SENT, "application/json"))
        .mount(&telegram)
        .await;
    let tgin = start(&gateway_config(&telegram.uri())).await;
    tgin.assert_metric(r#"tgin_bot_info{bot="gw",upstream="api_url"} 1"#);
    tgin.assert_metric(r#"tgin_bot_info{bot="other",upstream="api_url"} 1"#);
    let before = tgin.metrics.encode();
    for bot in ["gw", "other"] {
        assert!(!has_series(&before, "tgin_upstream_up", bot), "{before}");
    }

    let response = tgin
        .http
        .post(tgin.url(&format!("/bot{TOKEN}/getChat")))
        .send()
        .await
        .expect("an answer");
    assert_eq!(response.status(), 200);
    response.bytes().await.expect("the body");
    tgin.assert_metric(r#"tgin_upstream_up{bot="gw"} 1"#);
    let after = tgin.metrics.encode();
    assert!(!has_series(&after, "tgin_upstream_up", "other"), "{after}");
    assert!(!after.contains(TOKEN) && !after.contains(&telegram.uri()));
    tgin.stop().await;
}

/// Any HTTP answer, an error status too, sets `tgin_upstream_up` to 1; a
/// request the upstream leaves unanswered sets it to 0, until it answers
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_up_flips_on_failure_and_recovery() {
    let telegram = MockServer::start().await;
    let at = format!("/bot{TOKEN}/getChat");
    Mock::given(path(at.clone()))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(telegram_error(500, "Internal Server Error")),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&telegram)
        .await;
    // Longer than `gw`'s `upstream_idle_timeout` (1 s).
    Mock::given(path(at.clone()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(SENT, "application/json")
                .set_delay(Duration::from_secs(5)),
        )
        .up_to_n_times(1)
        .with_priority(2)
        .mount(&telegram)
        .await;
    Mock::given(path(at.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SENT, "application/json"))
        .with_priority(3)
        .mount(&telegram)
        .await;
    let tgin = start(&gateway_config(&telegram.uri())).await;
    let call = || async {
        let response = tgin
            .http
            .post(tgin.url(&at))
            .send()
            .await
            .expect("an answer");
        let status = response.status().as_u16();
        response.bytes().await.expect("the body");
        status
    };

    assert_eq!(call().await, 500);
    tgin.assert_metric(r#"tgin_upstream_up{bot="gw"} 1"#);
    assert_eq!(call().await, 502);
    tgin.assert_metric(r#"tgin_upstream_up{bot="gw"} 0"#);
    assert_eq!(call().await, 200);
    tgin.assert_metric(r#"tgin_upstream_up{bot="gw"} 1"#);
    tgin.stop().await;
}

/// The scrape's `tgin_upstream_up` line for `gw`, if any.
fn gw_up(tgin: &Tgin) -> Option<String> {
    tgin.metrics
        .encode()
        .lines()
        .find(|l| l.starts_with(r#"tgin_upstream_up{bot="gw"}"#))
        .map(str::to_owned)
}

/// A downstream that stops sending mid-upload is the client's stall, not
/// the server's: the same 502 and outcome as before, and the bot's
/// `tgin_upstream_up` is left as it was, absent or 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downstream_stall_leaves_upstream_up_alone() {
    let telegram = trickling_telegram().await;
    let tgin = start(&gateway_config(&format!("http://{telegram}"))).await;

    let stalled = upload(&tgin, 1, Duration::from_millis(10)).await;
    assert_eq!(stalled.status, 502);
    assert_eq!(
        serde_json::from_slice::<Value>(&stalled.body).expect("JSON"),
        telegram_error(502, UNREACHABLE)
    );
    assert_eq!(gw_up(&tgin), None);

    let done = upload(&tgin, 10, Duration::ZERO).await;
    assert_eq!(done.status, 200, "{}", String::from_utf8_lossy(&done.body));
    assert_eq!(
        gw_up(&tgin).as_deref(),
        Some(r#"tgin_upstream_up{bot="gw"} 1"#)
    );

    let stalled = upload(&tgin, 1, Duration::from_millis(10)).await;
    assert_eq!(stalled.status, 502);
    assert_eq!(
        gw_up(&tgin).as_deref(),
        Some(r#"tgin_upstream_up{bot="gw"} 1"#)
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendDocument",outcome="unreachable"} 2"#,
    );
    tgin.stop().await;
}

/// A server that accepts the connection but never reads stalls the upload
/// while the downstream still has bytes to send: that is the server's
/// stall, and `tgin_upstream_up` reads 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_that_never_reads_sets_upstream_up_to_zero() {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = socket.local_addr().unwrap();
    let listener = socket.listen(16).unwrap();
    // Accepts, holds every connection, never reads.
    let held = tokio::spawn(async move {
        let mut conns = Vec::new();
        while let Ok((conn, _)) = listener.accept().await {
            conns.push(conn);
        }
    });
    let tgin = start(&gateway_config(&format!("http://{addr}"))).await;

    // Far more than every buffer between the downstream and the server.
    const LENGTH: usize = 256 << 20;
    let mut stream = TcpStream::connect(tgin.addr).await.expect("connect");
    let head = format!(
        "POST /bot{TOKEN}/sendDocument HTTP/1.1\r\nHost: tgin\r\nContent-Type: multipart/form-data; boundary=b\r\nContent-Length: {LENGTH}\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.expect("head");
    let (mut read, mut write) = stream.into_split();
    let writer = tokio::spawn(async move {
        let chunk = vec![b'x'; 64 << 10];
        let mut sent = 0;
        while sent < LENGTH {
            if write.write_all(&chunk).await.is_err() {
                return;
            }
            sent += chunk.len();
        }
    });
    let mut head = Vec::new();
    timeout(PATIENCE, async {
        let mut buf = [0u8; 4096];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = read.read(&mut buf).await.expect("read");
            assert!(n > 0, "closed before a response");
            head.extend_from_slice(&buf[..n]);
        }
    })
    .await
    .expect("a response in time");
    writer.abort();
    let head = String::from_utf8_lossy(&head);
    assert!(head.starts_with("HTTP/1.1 502"), "{head}");
    assert_eq!(
        gw_up(&tgin).as_deref(),
        Some(r#"tgin_upstream_up{bot="gw"} 0"#)
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendDocument",outcome="unreachable"} 1"#,
    );
    held.abort();
    tgin.stop().await;
}

/// A `files.url` origin is a static file server, not the Bot API server: it
/// gets `upstream_idle_timeout` (1 s) to answer, not the upstream's
/// `response_timeout` (10 m here), and its silence is its own, never "the
/// call may have been executed".
#[tokio::test(start_paused = true)]
async fn file_origin_bounded_by_the_idle_timeout() {
    let frozen = freeze();
    let (api, mut arrivals) = waiting_server().await;
    let config = local_config(&format!("http://{api}"), "origin", "10m").replace(
        "response_timeout: 10m }",
        &format!("response_timeout: 10m, files: {{ url: \"http://{api}/files\" }} }}"),
    );
    let tgin = start(&config).await;
    let response = spawn_call(
        tgin.http
            .get(tgin.url(&format!("/file/bot{TOKEN}/documents/file_1.pdf"))),
    );
    let arrival = within("the origin request", arrivals.recv())
        .await
        .expect("open");
    tokio::time::advance(Duration::from_millis(999)).await;
    unanswered("a silent origin before the idle timeout", &response).await;
    tokio::time::advance(Duration::from_millis(1)).await;
    let (status, body) = within("the 502", response).await.expect("sent");
    assert_eq!(
        (status, body),
        (
            502,
            telegram_error(502, "Bad Gateway: the file server did not answer")
        )
    );
    assert_eq!(arrival.at.elapsed(), Duration::from_secs(1));
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="origin",method="file",outcome="unreachable"} 1"#,
    );
    let logs = LOGS.text();
    let lines: Vec<&str> = logs.lines().filter(|l| l.contains("bot=origin")).collect();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("WARN") && l.contains("the file server did not answer")),
        "{}",
        lines.join("\n")
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("Bot API server") || l.contains("may have been executed")),
        "{}",
        lines.join("\n")
    );
    frozen.thaw();
    tgin.stop().await;
}

/// A listener whose accept queue is full and stays full: connecting to it
/// does not finish (Linux drops the SYN; Windows refuses it only after
/// about 2 s of retries). The listener and the connection filling its
/// queue are held for as long as the returned values live.
async fn unaccepting() -> (SocketAddr, tokio::net::TcpListener, std::net::TcpStream) {
    let socket = tokio::net::TcpSocket::new_v4().expect("socket");
    socket.bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let listener = socket.listen(0).expect("listen");
    let addr = listener.local_addr().expect("address");
    let filler = std::net::TcpStream::connect(addr).expect("the connection that fills the queue");
    (addr, listener, filler)
}

/// A call that never left tgin, because the connection to the server was
/// still being made, was not executed: an empty-bodied call whose
/// `response_timeout` (1 s) is shorter than the connect is not answered
/// "may have been executed" at 1 s. The connect is bounded like an upload,
/// by `upstream_idle_timeout` (2 s), and answered "unreachable".
#[tokio::test(start_paused = true)]
async fn unfinished_connect_is_unreachable() {
    let frozen = freeze();
    let (api, _listener, _filler) = unaccepting().await;
    let config = local_config(&format!("http://{api}"), "connecting", "1s")
        .replace("upstream_idle_timeout: 1s", "upstream_idle_timeout: 2s");
    let tgin = start(&config).await;
    let response = spawn_call(tgin.http.post(tgin.url(&format!("/bot{TOKEN}/getMe"))));
    unanswered("a call while its connect hangs", &response).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    unanswered("a call at its response_timeout, never sent", &response).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    let (status, body) = within("the 502", response).await.expect("sent");
    assert_eq!((status, body), (502, telegram_error(502, UNREACHABLE)));
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="connecting",method="getMe",outcome="unreachable"} 1"#,
    );
    frozen.thaw();
    tgin.stop().await;
}
