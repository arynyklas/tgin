//! End to end: the app in process, wiremock as Telegram and as push
//! downstreams, real loopback sockets on ports the OS picks.

use std::collections::{BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Value};
use tgin::app::{App, Connections};
use tgin::exit::Outcome;
use tgin_core::config;
use tgin_runtime::config::MapEnv;
use tgin_runtime::lifecycle::{SignalKind, Signals};
use tgin_runtime::obs::MetricsRegistry;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Bounds every wait, so a stall fails the test instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(10);
const TOKEN: &str = "123456789:AAAAexampleSecretTokenValue_-xyz";
const OTHER_TOKEN: &str = "987654321:BBBBanotherSecretTokenValue_-xyz";
const HOOK_SECRET: &str = "hook-secret-value";
const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";
/// Fast shutdown: no lame duck, a short grace.
const QUICK: &str = "runtime: { lame_duck: 0s, shutdown_grace: 2s }";

fn update(id: i64, text: &str) -> String {
    format!(
        r#"{{"update_id":{id},"message":{{"message_id":{id},"chat":{{"id":500,"type":"private"}},"from":{{"id":7,"is_bot":false,"first_name":"U"}},"text":"{text}"}}}}"#
    )
}

fn ok(result: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": result }))
}

/// Answers `getUpdates` with each batch in turn, then like an idle long
/// poll; reports every poll.
struct Script {
    batches: Mutex<VecDeque<String>>,
    polls: mpsc::UnboundedSender<()>,
}

impl Respond for Script {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let _ = self.polls.send(());
        match self.batches.lock().pop_front() {
            Some(batch) => ResponseTemplate::new(200).set_body_raw(
                format!(r#"{{"ok":true,"result":[{batch}]}}"#),
                "application/json",
            ),
            None => ok(json!([])).set_delay(Duration::from_secs(3600)),
        }
    }
}

/// Records each request and answers with `response`.
struct Record {
    seen: mpsc::UnboundedSender<Request>,
    response: ResponseTemplate,
}

impl Respond for Record {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let _ = self.seen.send(req.clone());
        self.response.clone()
    }
}

/// Mounts one bot on a fake Bot API: `getMe` answers `get_me`, webhook
/// calls succeed, `getUpdates` plays `batches`. Returns the poll reports.
async fn mount_bot(
    server: &MockServer,
    token: &str,
    get_me: ResponseTemplate,
    batches: Vec<String>,
) -> mpsc::UnboundedReceiver<()> {
    let at = |m: &str| format!("/bot{token}/{m}");
    Mock::given(path(at("getMe")))
        .respond_with(get_me)
        .mount(server)
        .await;
    for m in ["deleteWebhook", "setWebhook"] {
        Mock::given(path(at(m)))
            .respond_with(ok(json!(true)))
            .mount(server)
            .await;
    }
    let (polls, rx) = mpsc::unbounded_channel();
    Mock::given(path(at("getUpdates")))
        .respond_with(Script {
            batches: Mutex::new(batches.into()),
            polls,
        })
        .mount(server)
        .await;
    rx
}

fn me(username: &str) -> ResponseTemplate {
    ok(json!({ "id": 42, "is_bot": true, "first_name": "Bot", "username": username }))
}

/// A push downstream answering `response` on `/bot`; returns what it saw.
async fn downstream(response: ResponseTemplate) -> (MockServer, mpsc::UnboundedReceiver<Request>) {
    let server = MockServer::start().await;
    let (seen, rx) = mpsc::unbounded_channel();
    Mock::given(method("POST"))
        .and(path("/bot"))
        .respond_with(Record { seen, response })
        .mount(&server)
        .await;
    (server, rx)
}

async fn next<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    timeout(PATIENCE, rx.recv())
        .await
        .expect("in time")
        .expect("channel open")
}

/// The app, running.
struct Running {
    addr: SocketAddr,
    signals: mpsc::Sender<SignalKind>,
    run: JoinHandle<Outcome>,
    metrics: Arc<MetricsRegistry>,
    connections: Connections,
    http: reqwest::Client,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn start(yaml: &str) -> Running {
    let cfg =
        config::load(yaml, Path::new("tgin.yaml"), &MapEnv::default()).unwrap_or_else(|errors| {
            let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
            panic!("invalid test config:\n{}", errors.join("\n"))
        });
    let app = App::start(cfg).await.expect("tgin starts");
    let addr = app.data_addrs()[0];
    let metrics = app.metrics();
    let connections = app.connections();
    let (signals, rx) = mpsc::channel(4);
    let run = tokio::spawn(app.run(Signals::from_channel(rx)));
    Running {
        addr,
        signals,
        run,
        metrics,
        connections,
        http: client(),
    }
}

impl Running {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn signal(&self, kind: SignalKind) {
        self.signals.send(kind).await.expect("the app listens");
    }

    async fn outcome(self) -> Outcome {
        timeout(PATIENCE, self.run)
            .await
            .expect("the app exits in time")
            .expect("the app does not panic")
    }

    async fn post_update(
        &self,
        path: &str,
        secret: Option<&str>,
        body: String,
    ) -> reqwest::Response {
        let mut req = self
            .http
            .post(self.url(path))
            .header("content-type", "application/json")
            .body(body);
        if let Some(secret) = secret {
            req = req.header(SECRET_HEADER, secret);
        }
        req.send().await.expect("webhook request")
    }

    /// Posts updates to the hooked bot's webhook, each answered 200.
    async fn deliver(&self, updates: &[(i64, &str)]) {
        for (id, text) in updates {
            let response = self
                .post_update("/hook", Some(HOOK_SECRET), update(*id, text))
                .await;
            assert_eq!(response.status(), 200, "update {id}");
        }
    }

    /// GETs a pull route as `instance` (or by address) and returns the
    /// status and body.
    async fn get(&self, path_and_query: &str, instance: Option<&str>) -> (u16, String) {
        let mut req = self.http.get(self.url(path_and_query));
        if let Some(instance) = instance {
            req = req.header("x-tgin-instance", instance);
        }
        let response = req.send().await.expect("pull request");
        (
            response.status().as_u16(),
            response.text().await.expect("body"),
        )
    }

    /// The update ids a successful poll returned.
    async fn poll_ids(&self, path_and_query: &str, instance: Option<&str>) -> Vec<i64> {
        let (status, body) = self.get(path_and_query, instance).await;
        assert_eq!(status, 200, "{body}");
        ids(&body)
    }

    async fn wait_until_not_ready(&self) {
        let started = Instant::now();
        loop {
            let status = self.get("/readyz", None).await.0;
            if status == 503 {
                return;
            }
            assert!(started.elapsed() < PATIENCE, "/readyz stayed {status}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn ids(body: &str) -> Vec<i64> {
    let body: Value = serde_json::from_str(body).expect("JSON body");
    assert_eq!(body["ok"], true, "{body}");
    body["result"]
        .as_array()
        .expect("result list")
        .iter()
        .map(|u| u["update_id"].as_i64().expect("update_id"))
        .collect()
}

/// One webhook bot `hooked` (path `/hook`, secret [`HOOK_SECRET`]) and the
/// rest of the config as given.
fn hooked(telegram: &MockServer, runtime: &str, routes: &str, consumers: &str) -> String {
    format!(
        r#"version: 1
{runtime}
server: {{ listen: ["127.0.0.1:0"] }}
bots:
  - name: hooked
    token: "{TOKEN}"
    api_url: {api}
    ingress: {{ webhook: {{ path: /hook, secret_token: "{HOOK_SECRET}" }} }}
    routes: {routes}
consumers:
{consumers}
"#,
        api = telegram.uri()
    )
}

async fn telegram_with_hooked_bot() -> MockServer {
    let telegram = MockServer::start().await;
    mount_bot(&telegram, TOKEN, me("tgin_bot"), Vec::new()).await;
    telegram
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_poll_to_pull_consumer_roundtrip() {
    let telegram = MockServer::start().await;
    let (one, two) = (update(1, "one"), update(2, "two"));
    let mut polls = mount_bot(
        &telegram,
        TOKEN,
        me("tgin_bot"),
        vec![format!("{one},{two}")],
    )
    .await;
    let app = start(&format!(
        r#"version: 1
{QUICK}
server: {{ listen: ["127.0.0.1:0"] }}
bots:
  - name: poller
    token: "{TOKEN}"
    api_url: {api}
    ingress: {{ long_poll: {{ timeout: 1s, delete_webhook_on_start: false }} }}
    routes: [{{ to: [workers] }}]
consumers:
  workers: {{ pull: {{}} }}
"#,
        api = telegram.uri()
    ))
    .await;

    // The second poll comes after both updates were admitted.
    next(&mut polls).await;
    next(&mut polls).await;
    let (status, body) = app.get("/c/workers/getUpdates?timeout=5", None).await;
    assert_eq!(status, 200);
    assert_eq!(body, format!(r#"{{"ok":true,"result":[{one},{two}]}}"#));
    assert_eq!(
        app.poll_ids("/c/workers/getUpdates?offset=3", None).await,
        Vec::<i64>::new()
    );

    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_to_push_consumer_roundtrip() {
    let telegram = telegram_with_hooked_bot().await;
    let (down, mut seen) = downstream(ResponseTemplate::new(200)).await;
    let app = start(&hooked(
        &telegram,
        QUICK,
        "[{ to: [downstream] }]",
        &format!(
            r#"  downstream: {{ push: {{ endpoints: [{{ url: "{}/bot", secret_token: push-secret-value }}] }} }}"#,
            down.uri()
        ),
    ))
    .await;

    let wrong = app
        .post_update("/hook", Some("wrong-secret"), update(1, "x"))
        .await;
    assert_eq!(wrong.status(), 401);
    let body = update(2, "hello");
    app.deliver(&[(2, "hello")]).await;

    let delivered = next(&mut seen).await;
    assert_eq!(delivered.body, body.as_bytes());
    let header = |name: &str| {
        delivered
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(header(SECRET_HEADER).as_deref(), Some("push-secret-value"));
    assert_eq!(header("x-tgin-source").as_deref(), Some("hooked"));
    assert_eq!(header("x-tgin-update-type").as_deref(), Some("message"));

    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
    assert!(seen.try_recv().is_err(), "update 1 was delivered");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rules_route_commands_to_admin_consumer() {
    let telegram = telegram_with_hooked_bot().await;
    let app = start(&hooked(
        &telegram,
        QUICK,
        "\n      - { match: { commands: [admin] }, to: [admin] }\n      - { to: [workers] }",
        "  admin: { pull: {} }\n  workers: { pull: {} }",
    ))
    .await;

    // getMe named the bot `tgin_bot`, so `/admin@tgin_bot` is ours.
    app.deliver(&[
        (1, "/admin"),
        (2, "hello"),
        (3, "/admin@tgin_bot now"),
        (4, "/admin@other_bot"),
    ])
    .await;
    assert_eq!(app.poll_ids("/c/admin/getUpdates", None).await, vec![1, 3]);
    assert_eq!(
        app.poll_ids("/c/workers/getUpdates", None).await,
        vec![2, 4]
    );

    app.signal(SignalKind::Terminate).await;
    // Leased and never acked: the grace runs out, then the app exits.
    assert_eq!(app.outcome().await, Outcome::Clean);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_alias_path_serves_queue() {
    let telegram = telegram_with_hooked_bot().await;
    let app = start(&hooked(
        &telegram,
        QUICK,
        "[{ to: [workers] }]",
        r#"  workers: { pull: { aliases: ["/legacy/getUpdates", "/other/poll"] } }"#,
    ))
    .await;
    app.deliver(&[(1, "one"), (2, "two")]).await;

    let first = app
        .http
        .post(app.url("/legacy/getUpdates"))
        .json(&json!({ "limit": 1, "timeout": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(ids(&first.text().await.unwrap()), vec![1]);
    // A form body acks update 1 and leases the next one.
    let second = app
        .http
        .post(app.url("/other/poll"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("offset=2&timeout=0")
        .send()
        .await
        .unwrap();
    assert_eq!(ids(&second.text().await.unwrap()), vec![2]);
    assert_eq!(
        app.poll_ids("/c/workers/getUpdates?offset=3", None).await,
        Vec::<i64>::new()
    );

    let (status, body) = app.get("/c/nobody/getUpdates", None).await;
    assert_eq!(status, 404);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({ "ok": false, "error_code": 404, "description": "Not Found" })
    );

    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_auth_token_required() {
    let telegram = telegram_with_hooked_bot().await;
    let app = start(&hooked(
        &telegram,
        QUICK,
        "[{ to: [workers] }]",
        r#"  workers: { pull: { auth_token: pull-secret-123, aliases: [/alias] } }"#,
    ))
    .await;
    app.deliver(&[(1, "one")]).await;

    let unauthorized = json!({ "ok": false, "error_code": 401, "description": "Unauthorized" });
    for route in ["/c/workers/getUpdates?timeout=0", "/alias?timeout=0"] {
        for auth in [None, Some("Bearer pull-secret-12"), Some("Bearer wrong")] {
            let mut req = app.http.get(app.url(route));
            if let Some(auth) = auth {
                req = req.header("authorization", auth);
            }
            let response = req.send().await.unwrap();
            assert_eq!(response.status(), 401, "{route} {auth:?}");
            assert_eq!(response.json::<Value>().await.unwrap(), unauthorized);
        }
    }
    let response = app
        .http
        .get(app.url("/alias?timeout=0"))
        .header("authorization", "Bearer pull-secret-123")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(ids(&response.text().await.unwrap()), vec![1]);

    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_instances_compete() {
    let telegram = telegram_with_hooked_bot().await;
    let app = start(&hooked(
        &telegram,
        QUICK,
        "[{ to: [workers] }]",
        "  workers: { pull: {} }",
    ))
    .await;
    app.deliver(&[(1, "one"), (2, "two")]).await;

    let q = "/c/workers/getUpdates?limit=1";
    assert_eq!(app.poll_ids(q, Some("a")).await, vec![1]);
    assert_eq!(app.poll_ids(q, Some("b")).await, vec![2]);
    // Each instance gets its own lease back until it acks.
    assert_eq!(app.poll_ids(q, Some("a")).await, vec![1]);
    assert_eq!(
        app.poll_ids("/c/workers/getUpdates?offset=2", Some("a"))
            .await,
        Vec::<i64>::new()
    );
    assert_eq!(app.poll_ids(q, Some("b")).await, vec![2]);
    // A malformed instance name is refused.
    let (status, _) = app.get(q, Some("two words")).await;
    assert_eq!(status, 400);

    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_serves_buffered_then_exits_0() {
    let telegram = telegram_with_hooked_bot().await;
    let app = start(&hooked(
        &telegram,
        "runtime: { lame_duck: 300ms, shutdown_grace: 5s }",
        "[{ to: [workers] }]",
        "  workers: { pull: {} }",
    ))
    .await;
    assert_eq!(app.get("/readyz", None).await.0, 200);
    app.deliver(&[(1, "one"), (2, "two"), (3, "three")]).await;

    app.signal(SignalKind::Terminate).await;
    app.wait_until_not_ready().await;
    // Accepted during the lame duck, refused with Retry-After once ingest
    // stops.
    let mut accepted: BTreeSet<i64> = [1, 2, 3].into();
    let started = Instant::now();
    for id in 4.. {
        let response = app
            .post_update("/hook", Some(HOOK_SECRET), update(id, "late"))
            .await;
        match response.status().as_u16() {
            200 => {
                accepted.insert(id);
            }
            503 => {
                assert_eq!(
                    response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok()),
                    Some("5")
                );
                break;
            }
            status => panic!("webhook answered {status}"),
        }
        assert!(started.elapsed() < PATIENCE, "ingest never stopped");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Everything accepted is served once, then acked.
    let served = app.poll_ids("/c/workers/getUpdates", None).await;
    assert_eq!(served.iter().copied().collect::<BTreeSet<_>>(), accepted);
    assert_eq!(served.len(), accepted.len());
    let last = served.iter().max().copied().unwrap_or_default();
    assert_eq!(
        app.poll_ids(&format!("/c/workers/getUpdates?offset={}", last + 1), None)
            .await,
        Vec::<i64>::new()
    );

    let metrics = Arc::clone(&app.metrics);
    assert_eq!(app.outcome().await, Outcome::Clean);
    assert!(
        metrics
            .encode()
            .contains("tgin_shutdown_abandoned_total 0\n"),
        "{}",
        metrics.encode()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_push_endpoint_bounded_shutdown() {
    let telegram = telegram_with_hooked_bot().await;
    let (down, mut seen) =
        downstream(ResponseTemplate::new(200).set_delay(Duration::from_secs(60))).await;
    let (lame_duck, grace) = (Duration::from_millis(200), Duration::from_secs(1));
    let app = start(&hooked(
        &telegram,
        "runtime: { lame_duck: 200ms, shutdown_grace: 1s }",
        "[{ to: [stuck] }]",
        &format!(
            r#"  stuck: {{ push: {{ endpoints: [{{ url: "{}/bot", timeout: 30s }}] }} }}"#,
            down.uri()
        ),
    ))
    .await;
    app.deliver(&[(1, "one")]).await;
    next(&mut seen).await;

    let metrics = Arc::clone(&app.metrics);
    let started = Instant::now();
    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
    let took = started.elapsed();
    assert!(took >= lame_duck + grace, "exited after {took:?}");
    assert!(
        took < lame_duck + grace + Duration::from_millis(1500),
        "exited after {took:?}"
    );
    assert!(
        metrics
            .encode()
            .contains("tgin_shutdown_abandoned_total 1\n"),
        "{}",
        metrics.encode()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_signal_exits_130() {
    let telegram = telegram_with_hooked_bot().await;
    let app = start(&hooked(
        &telegram,
        "runtime: { lame_duck: 30s, shutdown_grace: 30s }",
        "[{ to: [workers] }]",
        "  workers: { pull: {} }",
    ))
    .await;
    app.deliver(&[(1, "one")]).await;

    app.signal(SignalKind::CtrlBreak).await;
    app.wait_until_not_ready().await;
    let started = Instant::now();
    app.signal(SignalKind::Interrupt).await;
    let metrics = Arc::clone(&app.metrics);
    assert_eq!(app.outcome().await, Outcome::Forced);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        metrics
            .encode()
            .contains("tgin_shutdown_abandoned_total 1\n"),
        "{}",
        metrics.encode()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_close_skips_lame_duck() {
    let telegram = telegram_with_hooked_bot().await;
    let app = start(&hooked(
        &telegram,
        "runtime: { lame_duck: 30s, shutdown_grace: 30s }",
        "[{ to: [workers] }]",
        "  workers: { pull: {} }",
    ))
    .await;

    // Nothing is pending, so only the lame duck could hold the exit.
    let started = Instant::now();
    app.signal(SignalKind::CtrlClose).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_sources_failed_exits_2() {
    let telegram = MockServer::start().await;
    let unauthorized = ResponseTemplate::new(401).set_body_json(json!({
        "ok": false, "error_code": 401, "description": "Unauthorized"
    }));
    mount_bot(&telegram, TOKEN, unauthorized.clone(), Vec::new()).await;
    mount_bot(&telegram, OTHER_TOKEN, unauthorized, Vec::new()).await;
    let app = start(&format!(
        r#"version: 1
{QUICK}
server: {{ listen: ["127.0.0.1:0"] }}
bots:
  - name: first
    token: "{TOKEN}"
    api_url: {api}
    ingress: {{ long_poll: {{}} }}
    routes: [{{ to: [first] }}]
  - name: second
    token: "{OTHER_TOKEN}"
    api_url: {api}
    ingress: {{ long_poll: {{}} }}
    routes: [{{ to: [second] }}]
consumers:
  first: {{ pull: {{}} }}
  second: {{ pull: {{}} }}
"#,
        api = telegram.uri()
    ))
    .await;
    assert_eq!(app.outcome().await, Outcome::AllSourcesFailed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_allowlist_uses_connect_info() {
    let telegram = MockServer::start().await;
    mount_bot(&telegram, TOKEN, me("open_bot"), Vec::new()).await;
    mount_bot(&telegram, OTHER_TOKEN, me("closed_bot"), Vec::new()).await;
    let app = start(&format!(
        r#"version: 1
{QUICK}
server: {{ listen: ["127.0.0.1:0"] }}
bots:
  - name: open
    token: "{TOKEN}"
    api_url: {api}
    ingress: {{ webhook: {{ path: /open, secret_token: none, ip_allowlist: [127.0.0.0/8] }} }}
    routes: [{{ to: [open] }}]
  - name: closed
    token: "{OTHER_TOKEN}"
    api_url: {api}
    ingress: {{ webhook: {{ path: /closed, secret_token: none, ip_allowlist: [10.0.0.0/8] }} }}
    routes: [{{ to: [closed] }}]
consumers:
  open: {{ pull: {{}} }}
  closed: {{ pull: {{}} }}
"#,
        api = telegram.uri()
    ))
    .await;

    assert_eq!(
        app.post_update("/open", None, update(1, "in"))
            .await
            .status(),
        200
    );
    assert_eq!(
        app.post_update("/closed", None, update(2, "out"))
            .await
            .status(),
        403
    );
    assert_eq!(app.poll_ids("/c/open/getUpdates", None).await, vec![1]);
    assert_eq!(
        app.poll_ids("/c/closed/getUpdates", None).await,
        Vec::<i64>::new()
    );

    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_poll_cancelled_by_disconnect_releases_nothing() {
    let telegram = telegram_with_hooked_bot().await;
    let app = start(&hooked(
        &telegram,
        QUICK,
        "[{ to: [workers] }]",
        "  workers: { pull: {} }",
    ))
    .await;

    // A long poll whose client gives up while it is parked.
    let ghost = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let gave_up = ghost
        .get(app.url("/c/workers/getUpdates?timeout=30"))
        .header("x-tgin-instance", "ghost")
        .send()
        .await
        .expect_err("the poll outlasts the client's timeout");
    assert!(gave_up.is_timeout(), "{gave_up}");
    drop(ghost);
    // Its handler is gone once its connection is.
    let started = Instant::now();
    while app.connections.get() > 0 {
        assert!(started.elapsed() < PATIENCE, "the connection stayed open");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    app.deliver(&[(1, "one")]).await;
    assert_eq!(
        app.poll_ids("/c/workers/getUpdates?timeout=5", Some("live"))
            .await,
        vec![1]
    );
    assert_eq!(
        app.poll_ids("/c/workers/getUpdates", Some("ghost")).await,
        Vec::<i64>::new()
    );

    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sources_built_with_engine_ids() {
    let telegram = MockServer::start().await;
    mount_bot(&telegram, TOKEN, me("poll_bot"), vec![update(10, "polled")]).await;
    mount_bot(&telegram, OTHER_TOKEN, me("hook_bot"), Vec::new()).await;
    let (down, mut seen) = downstream(ResponseTemplate::new(200)).await;
    // The bot without ingress holds source id 0, so a source numbered by
    // its position among the sources would claim its name.
    let app = start(&format!(
        r#"version: 1
{QUICK}
server: {{ listen: ["127.0.0.1:0"] }}
bots:
  - name: idle
    token: "1:idle"
  - name: polled
    token: "{TOKEN}"
    api_url: {api}
    ingress: {{ long_poll: {{ timeout: 1s, delete_webhook_on_start: false }} }}
    routes: [{{ to: [all] }}]
  - name: hooked
    token: "{OTHER_TOKEN}"
    api_url: {api}
    ingress: {{ webhook: {{ path: /hook, secret_token: none }} }}
    routes: [{{ to: [all] }}]
consumers:
  all: {{ push: {{ endpoints: [{{ url: "{down}/bot" }}] }} }}
"#,
        api = telegram.uri(),
        down = down.uri()
    ))
    .await;
    assert_eq!(
        app.post_update("/hook", None, update(20, "hooked"))
            .await
            .status(),
        200
    );

    let mut sources = BTreeSet::new();
    for _ in 0..2 {
        let delivered = next(&mut seen).await;
        let body: Value = serde_json::from_slice(&delivered.body).unwrap();
        let source = delivered.headers["x-tgin-source"]
            .to_str()
            .unwrap()
            .to_owned();
        sources.insert((body["update_id"].as_i64().unwrap(), source));
    }
    assert_eq!(
        sources,
        BTreeSet::from([(10, "polled".to_owned()), (20, "hooked".to_owned())])
    );

    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
}
