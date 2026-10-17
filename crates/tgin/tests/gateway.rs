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
    let metrics = app.metrics();
    let (signals, rx) = mpsc::channel(4);
    let run = tokio::spawn(app.run(Signals::from_channel(rx)));
    Tgin {
        addr,
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
        "POST /bot{TOKEN}/sendMessage HTTP/1.1\r\nHost: tgin\r\nConnection: x-foo, user-agent\r\nX-Foo: bar\r\nUser-Agent: hop-listed\r\nAuthorization: Bearer downstream-secret\r\nX-Tgin-Instance: i1\r\nX-Forwarded-For: 203.0.113.9\r\nForwarded: for=203.0.113.9\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
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

    // An upload idle for longer than 1 s: 502 before any header.
    let stalled = upload(&tgin, 1, Duration::from_millis(10)).await;
    assert_eq!(stalled.status, 502);
    assert_eq!(
        serde_json::from_slice::<Value>(&stalled.body).expect("JSON"),
        telegram_error(502, "Bad Gateway: Telegram is unreachable")
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
    assert_eq!(
        body,
        telegram_error(502, "Bad Gateway: Telegram is unreachable")
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="offline",method="sendMessage",outcome="unreachable"} 1"#,
    );
    let logs = LOGS.text();
    assert!(
        logs.lines()
            .any(|l| l.contains("Telegram is unreachable") && l.contains("bot=offline")),
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
