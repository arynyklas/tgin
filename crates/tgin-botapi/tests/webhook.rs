//! The webhook ingress on a real in-process server with a recording sink.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use prometheus_client::encoding::text::encode;
use prometheus_client::registry::Registry;
use reqwest::StatusCode;
use serde_json::json;
use tgin_botapi::{
    ApiError, BotClient, DEFAULT_WEBHOOK_BODY_LIMIT, IpAllowlist, IpNet, Registration, SetWebhook,
    WebhookOptions, register, retry_registration, webhook_router, webhook_secret,
};
use tgin_core::facts::Facts;
use tgin_core::ids::SourceId;
use tgin_core::metrics::CoreMetrics;
use tgin_core::sink::{Admit, Sink};
use tgin_runtime::secret::Secret;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "123456789:AAAAexampleSecretTokenValue_-xyz";
const SECRET: &str = "s3cret-webhook-token_value";
const PATH: &str = "/hook/bot";
const SOURCE: SourceId = SourceId(3);
const SECRET_HEADER: &str = "X-Telegram-Bot-Api-Secret-Token";
/// Bounds every wait, so a server that stalls fails the test instead of
/// hanging it.
const PATIENCE: Duration = Duration::from_secs(10);

/// One update with a private chat; its exact bytes are what the sink must see.
fn update(id: i64) -> String {
    format!(
        r#"{{"update_id":{id},"message":{{"message_id":{id},"chat":{{"id":500,"type":"private"}},"text":"hi {id}"}}}}"#
    )
}

/// A valid update exactly `len` bytes long.
fn update_of_len(len: usize) -> String {
    let head = r#"{"update_id":1,"message":{"text":""#;
    let tail = r#""}}"#;
    let pad = "a".repeat(len - head.len() - tail.len());
    let body = format!("{head}{pad}{tail}");
    assert_eq!(body.len(), len);
    body
}

fn net(s: &str) -> IpNet {
    s.parse().unwrap()
}

fn options() -> WebhookOptions {
    WebhookOptions {
        path: PATH.to_owned(),
        secret: Some(Secret::new(SECRET.to_owned())),
        ip_allowlist: IpAllowlist::None,
        trusted_proxies: Vec::new(),
        body_limit: DEFAULT_WEBHOOK_BODY_LIMIT,
    }
}

#[derive(Clone, Debug)]
struct Call {
    source: SourceId,
    update_id: i64,
    payload: Bytes,
    update_type: String,
    chat_id: Option<i64>,
}

/// A sink that answers from `policy` and records every offer.
struct Recorder {
    policy: Box<dyn Fn(i64) -> Admit + Send + Sync>,
    calls: Mutex<Vec<Call>>,
}

impl Recorder {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().clone()
    }

    fn ids(&self) -> Vec<i64> {
        self.calls().iter().map(|c| c.update_id).collect()
    }
}

impl Sink for Recorder {
    fn admit(&self, source: SourceId, update_id: i64, payload: Bytes, facts: &Facts<'_>) -> Admit {
        self.calls.lock().push(Call {
            source,
            update_id,
            payload,
            update_type: facts.update_type.to_owned(),
            chat_id: facts.chat_id,
        });
        (self.policy)(update_id)
    }
}

/// A webhook router served on `127.0.0.1:0`.
struct Hook {
    addr: SocketAddr,
    sink: Arc<Recorder>,
    ingest: CancellationToken,
    registry: Registry,
    http: reqwest::Client,
}

impl Hook {
    /// Served with `ConnectInfo<SocketAddr>`, as the app does.
    async fn start(
        opts: WebhookOptions,
        policy: impl Fn(i64) -> Admit + Send + Sync + 'static,
    ) -> Self {
        Self::serve(opts, policy, true, CancellationToken::new()).await
    }

    async fn serve(
        opts: WebhookOptions,
        policy: impl Fn(i64) -> Admit + Send + Sync + 'static,
        connect_info: bool,
        ingest: CancellationToken,
    ) -> Self {
        let mut registry = Registry::with_prefix("tgin");
        let metrics = CoreMetrics::register(&mut registry).source("wh");
        let sink = Arc::new(Recorder {
            policy: Box::new(policy),
            calls: Mutex::default(),
        });
        let app = webhook_router(
            SOURCE,
            Arc::from("wh"),
            opts,
            Arc::clone(&sink),
            ingest.clone(),
            metrics,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        if connect_info {
            let app = app.into_make_service_with_connect_info::<SocketAddr>();
            tokio::spawn(async move { axum::serve(listener, app).await });
        } else {
            tokio::spawn(async move { axum::serve(listener, app).await });
        }
        // `no_proxy`: proxy variables in the test environment must not
        // reroute requests meant for the local server.
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        Self {
            addr,
            sink,
            ingest,
            registry,
            http,
        }
    }

    fn url(&self) -> String {
        format!("http://{}{PATH}", self.addr)
    }

    /// POSTs `body` with the given secret and `X-Forwarded-For` headers.
    async fn post(
        &self,
        secret: Option<&str>,
        forwarded: Option<&str>,
        body: impl Into<reqwest::Body>,
    ) -> reqwest::Response {
        let mut req = self.http.post(self.url()).body(body);
        if let Some(secret) = secret {
            req = req.header(SECRET_HEADER, secret);
        }
        if let Some(forwarded) = forwarded {
            req = req.header("X-Forwarded-For", forwarded);
        }
        timeout(PATIENCE, req.send())
            .await
            .expect("timed out waiting for a response")
            .unwrap()
    }

    async fn send(&self, body: impl Into<reqwest::Body>) -> reqwest::Response {
        self.post(Some(SECRET), None, body).await
    }

    /// Writes `request` on a fresh connection, holding the connection open
    /// so the server sees no more bytes than given, and returns the status
    /// and head of the response.
    async fn raw(&self, request: &[u8]) -> (u16, String) {
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        stream.write_all(request).await.unwrap();
        let mut buf = Vec::new();
        let read = timeout(PATIENCE, async {
            let mut chunk = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "the server closed the connection without a response");
                buf.extend_from_slice(&chunk[..n]);
            }
        })
        .await;
        assert!(read.is_ok(), "timed out waiting for a response");
        let head = String::from_utf8(buf).unwrap();
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("a status line");
        (status, head.to_ascii_lowercase())
    }

    /// A request head for `PATH`.
    fn head(&self, secret: &str, extra: &str) -> String {
        format!(
            "POST {PATH} HTTP/1.1\r\nHost: {}\r\n{SECRET_HEADER}: {secret}\r\nContent-Type: application/json\r\n{extra}\r\n",
            self.addr
        )
    }

    /// Asserts each `name{labels} value` sample is exported as given.
    fn assert_metrics(&self, want: &[&str]) {
        let mut text = String::new();
        encode(&mut text, &self.registry).unwrap();
        for line in want {
            assert!(
                text.lines().any(|l| l == *line),
                "missing {line:?} in\n{text}"
            );
        }
    }
}

fn admitted(_: i64) -> Admit {
    Admit::Admitted
}

async fn assert_status(resp: reqwest::Response, status: StatusCode) {
    assert_eq!(resp.status(), status);
    let retry_after = resp.headers().get("retry-after").cloned();
    let body = resp.bytes().await.unwrap();
    assert!(body.is_empty(), "{body:?}");
    if status == StatusCode::SERVICE_UNAVAILABLE {
        assert_eq!(retry_after.unwrap(), "5");
    } else {
        assert_eq!(retry_after, None);
    }
}

#[tokio::test]
async fn accepts_valid_secret() {
    let hook = Hook::start(options(), |id| {
        if id == 2 {
            Admit::Unroutable
        } else {
            Admit::Admitted
        }
    })
    .await;

    assert_status(hook.send(update(1)).await, StatusCode::OK).await;
    // An update no rule matches is acknowledged too: redelivering it would
    // not route it.
    assert_status(hook.send(update(2)).await, StatusCode::OK).await;

    let calls = hook.sink.calls();
    assert_eq!(hook.sink.ids(), [1, 2]);
    assert_eq!(calls[0].source, SOURCE);
    assert_eq!(calls[0].payload, update(1).as_bytes());
    assert_eq!(calls[0].update_type, "message");
    assert_eq!(calls[0].chat_id, Some(500));
    hook.assert_metrics(&[
        r#"tgin_source_updates_total{source="wh"} 2"#,
        r#"tgin_source_errors_total{source="wh",reason="unauthorized"} 0"#,
        r#"tgin_routes_unmatched_total{source="wh"} 0"#,
    ]);

    // Without a configured secret, no header is needed.
    let open = Hook::start(
        WebhookOptions {
            secret: None,
            ..options()
        },
        admitted,
    )
    .await;
    assert_status(open.post(None, None, update(3)).await, StatusCode::OK).await;
    assert_eq!(open.sink.ids(), [3]);
}

#[tokio::test]
async fn non_post_405_and_other_paths_404() {
    let hook = Hook::start(options(), admitted).await;
    for verb in [reqwest::Method::GET, reqwest::Method::PUT] {
        let resp = hook
            .http
            .request(verb, hook.url())
            .header(SECRET_HEADER, SECRET)
            .body(update(1))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
    let resp = hook
        .http
        .post(format!("http://{}/elsewhere", hook.addr))
        .header(SECRET_HEADER, SECRET)
        .body(update(1))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(hook.sink.calls().is_empty());
}

#[tokio::test]
async fn rejects_missing_or_wrong_secret_without_reading_body() {
    let hook = Hook::start(options(), admitted).await;

    assert_status(
        hook.post(None, None, update(1)).await,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    assert_status(
        hook.post(Some("wrong"), None, update(1)).await,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    // A prefix or an extension of the secret is still wrong.
    assert_status(
        hook.post(Some(&SECRET[..SECRET.len() - 1]), None, update(1))
            .await,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    assert_status(
        hook.post(Some(&format!("{SECRET}x")), None, update(1))
            .await,
        StatusCode::UNAUTHORIZED,
    )
    .await;

    // The server answers on the headers alone: the announced body is never
    // sent, and an oversized one is refused as unauthorized, not too large.
    let (status, _) = hook
        .raw(hook.head("wrong", "Content-Length: 100\r\n").as_bytes())
        .await;
    assert_eq!(status, 401);
    let oversized = format!("Content-Length: {}\r\n", 2 * DEFAULT_WEBHOOK_BODY_LIMIT);
    let (status, _) = hook.raw(hook.head("wrong", &oversized).as_bytes()).await;
    assert_eq!(status, 401);

    assert!(hook.sink.calls().is_empty());
    hook.assert_metrics(&[
        r#"tgin_source_errors_total{source="wh",reason="unauthorized"} 6"#,
        r#"tgin_source_updates_total{source="wh"} 0"#,
    ]);

    // The right secret on the same raw path is read and admitted.
    let body = update(9);
    let request = format!(
        "{}{body}",
        hook.head(SECRET, &format!("Content-Length: {}\r\n", body.len()))
    );
    let (status, _) = hook.raw(request.as_bytes()).await;
    assert_eq!(status, 200);
    assert_eq!(hook.sink.ids(), [9]);
}

#[tokio::test]
async fn body_limit_413() {
    assert_eq!(DEFAULT_WEBHOOK_BODY_LIMIT, 1024 * 1024);
    let hook = Hook::start(
        WebhookOptions {
            body_limit: 64,
            ..options()
        },
        admitted,
    )
    .await;

    // At the limit is fine; one byte more is not.
    assert_status(hook.send(update_of_len(64)).await, StatusCode::OK).await;
    assert_status(
        hook.send(update_of_len(65)).await,
        StatusCode::PAYLOAD_TOO_LARGE,
    )
    .await;

    // A Content-Length above the limit is refused before any body arrives.
    let (status, _) = hook
        .raw(hook.head(SECRET, "Content-Length: 100000\r\n").as_bytes())
        .await;
    assert_eq!(status, 413);

    // Without a Content-Length the limit holds while reading: one chunk over
    // it (the body never ends), and two chunks each under it.
    let chunked = "Transfer-Encoding: chunked\r\n";
    let one = format!("{}41\r\n{}\r\n", hook.head(SECRET, chunked), "a".repeat(65));
    let (status, _) = hook.raw(one.as_bytes()).await;
    assert_eq!(status, 413);
    let two = format!(
        "{}28\r\n{a}\r\n28\r\n{a}\r\n",
        hook.head(SECRET, chunked),
        a = "a".repeat(40)
    );
    let (status, _) = hook.raw(two.as_bytes()).await;
    assert_eq!(status, 413);

    // A chunked body within the limit is read whole, across chunks.
    let body = r#"{"update_id":5,"edited_message":{"text":"x"}}"#;
    let ok = format!(
        "{}{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
        hook.head(SECRET, chunked),
        10,
        &body[..10],
        body.len() - 10,
        &body[10..]
    );
    let (status, _) = hook.raw(ok.as_bytes()).await;
    assert_eq!(status, 200);
    assert_eq!(hook.sink.calls()[1].payload, body.as_bytes());

    // The default limit is 1 MiB.
    let default = Hook::start(options(), admitted).await;
    let over = format!("Content-Length: {}\r\n", DEFAULT_WEBHOOK_BODY_LIMIT + 1);
    let (status, _) = default.raw(default.head(SECRET, &over).as_bytes()).await;
    assert_eq!(status, 413);
    assert_status(
        default
            .send(update_of_len(DEFAULT_WEBHOOK_BODY_LIMIT))
            .await,
        StatusCode::OK,
    )
    .await;

    assert_eq!(hook.sink.ids(), [1, 5]);
    // Each refusal for size counts as a body that is not an update.
    hook.assert_metrics(&[
        r#"tgin_source_updates_total{source="wh"} 2"#,
        r#"tgin_source_errors_total{source="wh",reason="decode"} 4"#,
    ]);
}

/// A body that arrived in one frame is copied to an allocation of its own
/// size: queued, it must not keep the connection's whole read buffer.
#[tokio::test]
async fn single_frame_body_owns_exactly_its_bytes() {
    let hook = Hook::start(options(), admitted).await;
    assert_status(hook.send(update(1)).await, StatusCode::OK).await;
    let payload = hook.sink.calls.lock().pop().expect("admitted").payload;
    assert_eq!(payload, update(1).as_bytes());
    // The connection stays open in the client's pool, so a slice of the
    // server's read buffer would not be unique.
    match payload.try_into_mut() {
        Ok(owned) => assert_eq!(owned.capacity(), owned.len()),
        Err(shared) => panic!("the payload shares its memory ({} bytes)", shared.len()),
    }
}

#[tokio::test]
async fn allowlist_blocks_and_trusted_proxy_header_used() {
    let telegram = WebhookOptions {
        ip_allowlist: IpAllowlist::Telegram,
        ..options()
    };

    // The test client connects from 127.0.0.1, outside Telegram's ranges; a
    // forwarded address from an untrusted peer is ignored.
    let hook = Hook::start(telegram.clone(), admitted).await;
    assert_status(hook.send(update(1)).await, StatusCode::FORBIDDEN).await;
    assert_status(
        hook.post(Some(SECRET), Some("149.154.160.5"), update(2))
            .await,
        StatusCode::FORBIDDEN,
    )
    .await;
    // The address is checked before the secret.
    assert_status(
        hook.post(Some("wrong"), None, update(3)).await,
        StatusCode::FORBIDDEN,
    )
    .await;
    assert!(hook.sink.calls().is_empty());
    hook.assert_metrics(&[r#"tgin_source_errors_total{source="wh",reason="unauthorized"} 3"#]);

    // Behind a trusted proxy, the right-most untrusted forwarded address is
    // the client.
    let proxied = Hook::start(
        WebhookOptions {
            trusted_proxies: vec![net("127.0.0.0/8"), net("10.1.0.0/16")],
            ..telegram.clone()
        },
        admitted,
    )
    .await;
    for (forwarded, status) in [
        ("149.154.160.5", StatusCode::OK),
        ("91.108.7.255", StatusCode::OK),
        ("1.2.3.4, 149.154.175.1, 10.1.2.3", StatusCode::OK),
        ("149.154.160.5, 10.0.0.9", StatusCode::FORBIDDEN),
        ("149.154.160.5, 1.2.3.4", StatusCode::FORBIDDEN),
        ("149.154.176.0", StatusCode::FORBIDDEN),
        ("not-an-ip", StatusCode::FORBIDDEN),
        ("149.154.160.5, garbage", StatusCode::FORBIDDEN),
    ] {
        let resp = proxied.post(Some(SECRET), Some(forwarded), update(4)).await;
        assert_eq!(resp.status(), status, "{forwarded}");
    }
    // Without the header, the trusted proxy itself is the client.
    assert_status(proxied.send(update(5)).await, StatusCode::FORBIDDEN).await;
    // Several header lines form one list, in order.
    let resp = proxied
        .http
        .post(proxied.url())
        .header(SECRET_HEADER, SECRET)
        .header("X-Forwarded-For", "149.154.160.5")
        .header("X-Forwarded-For", "10.1.0.1")
        .body(update(6))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(proxied.sink.ids(), [4, 4, 4, 6]);

    // A CIDR list admits the loopback peer directly.
    let local = Hook::start(
        WebhookOptions {
            ip_allowlist: IpAllowlist::Cidrs(vec![net("127.0.0.1/32")]),
            ..options()
        },
        admitted,
    )
    .await;
    assert_status(local.send(update(7)).await, StatusCode::OK).await;

    // Served without connection info, an allowlist fails closed; without an
    // allowlist the address is not needed.
    let blind = Hook::serve(
        WebhookOptions {
            ip_allowlist: IpAllowlist::Cidrs(vec![net("0.0.0.0/0"), net("::/0")]),
            ..options()
        },
        admitted,
        false,
        CancellationToken::new(),
    )
    .await;
    assert_status(blind.send(update(8)).await, StatusCode::FORBIDDEN).await;
    let open = Hook::serve(options(), admitted, false, CancellationToken::new()).await;
    assert_status(open.send(update(8)).await, StatusCode::OK).await;
}

#[tokio::test]
async fn backpressure_503_retry_after() {
    let hook = Hook::start(options(), |id| {
        if id == 1 {
            Admit::Backpressure
        } else {
            Admit::Admitted
        }
    })
    .await;

    assert_status(hook.send(update(1)).await, StatusCode::SERVICE_UNAVAILABLE).await;
    assert_status(hook.send(update(2)).await, StatusCode::OK).await;

    // The refused update was offered once and is not counted as received.
    assert_eq!(hook.sink.ids(), [1, 2]);
    hook.assert_metrics(&[r#"tgin_source_updates_total{source="wh"} 1"#]);
}

#[tokio::test]
async fn draining_503_retry_after() {
    let hook = Hook::start(options(), admitted).await;
    hook.ingest.cancel();

    assert_status(hook.send(update(1)).await, StatusCode::SERVICE_UNAVAILABLE).await;
    // Refused on the headers alone: the announced body is never sent.
    let (status, head) = hook
        .raw(hook.head(SECRET, "Content-Length: 100\r\n").as_bytes())
        .await;
    assert_eq!(status, 503);
    assert!(head.contains("\r\nretry-after: 5\r\n"), "{head}");
    // Authentication still comes first.
    assert_status(
        hook.post(Some("wrong"), None, update(2)).await,
        StatusCode::UNAUTHORIZED,
    )
    .await;

    assert!(hook.sink.calls().is_empty());
    hook.assert_metrics(&[r#"tgin_source_updates_total{source="wh"} 0"#]);
}

#[tokio::test]
async fn cancelled_during_admission_503_retry_after() {
    // Ingest stops while the sink commits the update: the drain may already
    // have found nothing pending, so the update is refused for Telegram to
    // deliver again; the committed copy may still go out (at least once).
    let ingest = CancellationToken::new();
    let cancel = ingest.clone();
    let hook = Hook::serve(
        options(),
        move |_| {
            cancel.cancel();
            Admit::Admitted
        },
        true,
        ingest,
    )
    .await;
    assert_status(hook.send(update(1)).await, StatusCode::SERVICE_UNAVAILABLE).await;
    assert_eq!(hook.sink.ids(), [1]);
    hook.assert_metrics(&[r#"tgin_source_updates_total{source="wh"} 1"#]);

    // An unroutable update has nowhere to go on any instance: acknowledged.
    let ingest = CancellationToken::new();
    let cancel = ingest.clone();
    let hook = Hook::serve(
        options(),
        move |_| {
            cancel.cancel();
            Admit::Unroutable
        },
        true,
        ingest,
    )
    .await;
    assert_status(hook.send(update(2)).await, StatusCode::OK).await;
    assert_eq!(hook.sink.ids(), [2]);
}

#[tokio::test]
async fn broken_body_400_counts_network() {
    let hook = Hook::start(options(), admitted).await;
    let broken = format!(
        "{}zz\r\nabc\r\n",
        hook.head(SECRET, "Transfer-Encoding: chunked\r\n")
    );
    let (status, _) = hook.raw(broken.as_bytes()).await;
    assert_eq!(status, 400);
    assert!(hook.sink.calls().is_empty());
    hook.assert_metrics(&[
        r#"tgin_source_errors_total{source="wh",reason="network"} 1"#,
        r#"tgin_source_errors_total{source="wh",reason="decode"} 0"#,
    ]);
}

#[tokio::test]
async fn bad_json_400() {
    let hook = Hook::start(options(), admitted).await;
    for body in [
        "",
        "not json",
        "[]",
        r#"{"message":{}}"#,
        r#"{"update_id":"1","message":{}}"#,
        r#"{"update_id":1.5}"#,
        r#"{"update_id":1,"message":{"text":"\x"}}"#,
        r#"{"update_id":1}trailing"#,
    ] {
        let resp = hook.send(body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    assert!(hook.sink.calls().is_empty());
    hook.assert_metrics(&[
        r#"tgin_source_errors_total{source="wh",reason="decode"} 8"#,
        r#"tgin_source_updates_total{source="wh"} 0"#,
    ]);
}

#[tokio::test]
async fn register_sends_derived_secret() {
    let token = Secret::new(TOKEN.to_owned());
    let secret = webhook_secret(&token);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/setWebhook")))
        .and(body_partial_json(json!({
            "url": "https://tgin.example/hook/bot",
            "secret_token": secret,
            "drop_pending_updates": false,
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": true })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let client = BotClient::new(http, &server.uri().parse().unwrap(), &token).unwrap();
    let req = SetWebhook {
        url: "https://tgin.example/hook/bot".parse().unwrap(),
        secret_token: Some(Secret::new(secret.clone())),
        allowed_updates: Vec::new(),
        max_connections: None,
        drop_pending_updates: false,
        ip_address: None,
        certificate: None,
    };
    register(&client, &req).await.unwrap();
    let again = req.clone();

    // The ingress registered with that secret accepts what Telegram sends.
    let hook = Hook::start(
        WebhookOptions {
            secret: req.secret_token.clone(),
            ..options()
        },
        admitted,
    )
    .await;
    assert_status(
        hook.post(Some(&secret), None, update(1)).await,
        StatusCode::OK,
    )
    .await;

    // A rejected registration is an error.
    let failing = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ok": false, "error_code": 400, "description": "Bad Request: bad webhook"
        })))
        .mount(&failing)
        .await;
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let client = BotClient::new(http, &failing.uri().parse().unwrap(), &token).unwrap();
    let err = register(&client, &again).await.unwrap_err();
    assert!(err.to_string().contains("bad webhook"), "{err}");
}

/// A 429 asking to wait `secs` seconds, as the Bot API server sends it.
fn too_many_requests(secs: u64) -> ApiError {
    ApiError::Http {
        status: 429,
        description: format!("Too Many Requests: retry after {secs}"),
        retry_after: Some(Duration::from_secs(secs)),
    }
}

/// On a frozen clock: a `setWebhook` answered 429 with `retry_after` 3 is
/// made again exactly 3 s later (the backoff's first delay is at most 1 s),
/// and the next 200 registers the webhook. A failed attempt made before
/// the retries start is waited out the same way.
#[tokio::test(start_paused = true)]
async fn registration_retries_honour_retry_after() {
    let started = tokio::time::Instant::now();
    let calls = Mutex::new(Vec::new());
    // Popped from the end: the 429 first, then the 200.
    let mut replies = vec![Ok(()), Err(too_many_requests(3))];
    let registered = retry_registration("wh", &CancellationToken::new(), false, None, || {
        calls.lock().push(started.elapsed());
        std::future::ready(replies.pop().expect("two attempts"))
    })
    .await;
    assert_eq!(registered, Registration::Registered);
    assert_eq!(*calls.lock(), [Duration::ZERO, Duration::from_secs(3)]);

    let started = tokio::time::Instant::now();
    let calls = Mutex::new(Vec::new());
    let registered = retry_registration(
        "wh",
        &CancellationToken::new(),
        false,
        Some(too_many_requests(3)),
        || {
            calls.lock().push(started.elapsed());
            std::future::ready(Ok(()))
        },
    )
    .await;
    assert_eq!(registered, Registration::Registered);
    assert_eq!(*calls.lock(), [Duration::from_secs(3)]);
}

/// On a frozen clock: cancelling the bot's ingest ends the retries at once,
/// both while a retry is waited for and while an attempt is in flight.
#[tokio::test(start_paused = true)]
async fn registration_stops_with_ingest() {
    let started = tokio::time::Instant::now();
    let stop = CancellationToken::new();
    let attempts = Mutex::new(0);
    let retries = retry_registration("wh", &stop, false, None, || {
        *attempts.lock() += 1;
        std::future::ready(Err(too_many_requests(3600)))
    });
    let cancel = async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        stop.cancel();
    };
    let (registered, ()) = tokio::join!(retries, cancel);
    assert_eq!(registered, Registration::Stopped);
    assert_eq!(*attempts.lock(), 1);
    assert_eq!(started.elapsed(), Duration::from_secs(10));

    let started = tokio::time::Instant::now();
    let stop = CancellationToken::new();
    let hanging = retry_registration("wh", &stop, false, None, std::future::pending);
    let cancel = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        stop.cancel();
    };
    let (registered, ()) = tokio::join!(hanging, cancel);
    assert_eq!(registered, Registration::Stopped);
    assert_eq!(started.elapsed(), Duration::from_secs(1));
}

fn http_error(status: u16) -> ApiError {
    ApiError::Http {
        status,
        description: format!("error {status}"),
        retry_after: None,
    }
}

/// On a frozen clock: a cloud server's 401, 403 or 404 rejects the token
/// for good, so registration ends after that one attempt, also when it is
/// the error handed over from an attempt made before. A self-hosted
/// server's 401 or 404 may be about its own api_id or api_hash: it is
/// retried; its 403 is final too.
#[tokio::test(start_paused = true)]
async fn registration_rejected_for_good_on_cloud_only() {
    for status in [401, 403, 404] {
        // A retry fails the test at once rather than waiting on the paused
        // clock forever.
        let attempts = Mutex::new(0);
        let outcome = retry_registration("wh", &CancellationToken::new(), false, None, || {
            let mut n = attempts.lock();
            *n += 1;
            assert_eq!(*n, 1, "{status} retried after a rejection");
            std::future::ready(Err(http_error(status)))
        })
        .await;
        assert_eq!(outcome, Registration::Rejected(http_error(status)));
        assert_eq!(*attempts.lock(), 1, "{status}");

        let outcome = retry_registration(
            "wh",
            &CancellationToken::new(),
            false,
            Some(http_error(status)),
            || -> std::future::Ready<Result<(), ApiError>> {
                panic!("no attempt after a rejection")
            },
        )
        .await;
        assert_eq!(outcome, Registration::Rejected(http_error(status)));
    }

    for status in [401, 404] {
        let attempts = Mutex::new(0);
        let outcome = retry_registration("wh", &CancellationToken::new(), true, None, || {
            let mut attempts = attempts.lock();
            *attempts += 1;
            std::future::ready(if *attempts < 3 {
                Err(http_error(status))
            } else {
                Ok(())
            })
        })
        .await;
        assert_eq!(outcome, Registration::Registered, "{status}");
        assert_eq!(*attempts.lock(), 3, "{status}");
    }
    let attempts = Mutex::new(0);
    let outcome = retry_registration("wh", &CancellationToken::new(), true, None, || {
        let mut n = attempts.lock();
        *n += 1;
        assert_eq!(*n, 1, "local 403 retried after a rejection");
        std::future::ready(Err(http_error(403)))
    })
    .await;
    assert_eq!(outcome, Registration::Rejected(http_error(403)));
}
