//! End to end: the app in process, wiremock as Telegram and as push
//! downstreams, real loopback sockets on ports the OS picks.

#[cfg(unix)]
mod support;

use std::collections::{BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tgin::app::{App, Boot, Connections};
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
    admin: SocketAddr,
    signals: mpsc::Sender<SignalKind>,
    run: JoinHandle<Outcome>,
    metrics: Arc<MetricsRegistry>,
    connections: Connections,
    http: reqwest::Client,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// `yaml` with the admin plane on a port the OS picks, unless it places
/// the admin plane itself.
fn with_admin(yaml: &str) -> String {
    if yaml.contains("\nadmin:") {
        yaml.to_owned()
    } else {
        format!("{yaml}\nadmin: {{ listen: [\"127.0.0.1:0\"] }}\n")
    }
}

fn load(yaml: &str) -> config::Config {
    config::load(
        &with_admin(yaml),
        Path::new("tgin.yaml"),
        &MapEnv::default(),
    )
    .unwrap_or_else(|errors| {
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        panic!("invalid test config:\n{}", errors.join("\n"))
    })
}

/// `yaml`, written to a config file of its own and loaded from there.
fn boot(yaml: &str) -> Boot {
    let file = scratch(&format!("boot-{:016x}", rand::random::<u64>())).join("tgin.yaml");
    std::fs::write(&file, with_admin(yaml)).expect("the config file");
    Boot::load(&file, Arc::new(MapEnv::default()))
        .unwrap_or_else(|err| panic!("invalid test config: {err:?}"))
}

async fn start(yaml: &str) -> Running {
    running(App::start(boot(yaml)).await.expect("tgin starts"))
}

/// Runs `app` on signals the test sends.
fn running(app: App) -> Running {
    let addr = app.data_addrs()[0];
    let admin = app.admin_addrs()[0];
    let metrics = app.metrics();
    let connections = app.connections();
    let (signals, rx) = mpsc::channel(4);
    let run = tokio::spawn(app.run(Signals::from_channel(rx)));
    Running {
        addr,
        admin,
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

    /// The status `/readyz` on the admin plane answers.
    async fn readiness(&self) -> u16 {
        self.http
            .get(format!("http://{}/readyz", self.admin))
            .send()
            .await
            .expect("readiness probe")
            .status()
            .as_u16()
    }

    async fn wait_until_not_ready(&self) {
        let started = Instant::now();
        loop {
            let status = self.readiness().await;
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
    assert_eq!(app.readiness().await, 200);
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

/// Two long-poll bots whose tokens Telegram rejects.
async fn failing_sources(runtime: &str) -> (MockServer, Running) {
    let telegram = MockServer::start().await;
    let unauthorized = ResponseTemplate::new(401).set_body_json(json!({
        "ok": false, "error_code": 401, "description": "Unauthorized"
    }));
    mount_bot(&telegram, TOKEN, unauthorized.clone(), Vec::new()).await;
    mount_bot(&telegram, OTHER_TOKEN, unauthorized, Vec::new()).await;
    let app = start(&format!(
        r#"version: 1
{runtime}
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
    (telegram, app)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_sources_failed_exits_2() {
    let (_telegram, app) = failing_sources(QUICK).await;
    assert_eq!(app.outcome().await, Outcome::AllSourcesFailed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_while_sources_failed_keeps_draining() {
    let lame_duck = Duration::from_millis(1500);
    let (_telegram, app) =
        failing_sources("runtime: { lame_duck: 1500ms, shutdown_grace: 2s }").await;
    app.wait_until_not_ready().await;
    let draining = Instant::now();
    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::AllSourcesFailed);
    // The signal did not cut the lame duck short.
    assert!(
        draining.elapsed() >= lame_duck - Duration::from_millis(100),
        "exited after {:?}",
        draining.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_signals_while_sources_failed_exit_130() {
    let (_telegram, app) =
        failing_sources("runtime: { lame_duck: 30s, shutdown_grace: 30s }").await;
    app.wait_until_not_ready().await;
    let started = Instant::now();
    app.signal(SignalKind::Terminate).await;
    app.signal(SignalKind::Interrupt).await;
    assert_eq!(app.outcome().await, Outcome::Forced);
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// A webhook bot whose `getMe` answers only after a minute, with a push
/// consumer, so the start is still waiting when the test signals.
async fn slow_start() -> (MockServer, Boot) {
    let telegram = MockServer::start().await;
    mount_bot(
        &telegram,
        TOKEN,
        me("tgin_bot").set_delay(Duration::from_secs(60)),
        Vec::new(),
    )
    .await;
    let (down, _seen) = downstream(ResponseTemplate::new(200)).await;
    let cfg = boot(&hooked(
        &telegram,
        QUICK,
        "[{ to: [downstream] }]",
        &format!(
            r#"  downstream: {{ push: {{ endpoints: [{{ url: "{}/bot" }}] }} }}"#,
            down.uri()
        ),
    ));
    (telegram, cfg)
}

/// Launches `cfg` and waits until it asked Telegram for the bot's identity.
async fn launch_until_get_me(
    telegram: &MockServer,
    cfg: Boot,
) -> (
    mpsc::Sender<SignalKind>,
    JoinHandle<Result<Outcome, tgin::app::StartError>>,
) {
    let (signals, rx) = mpsc::channel(4);
    let launched = tokio::spawn(tgin::app::launch(cfg, Signals::from_channel(rx)));
    let started = Instant::now();
    while telegram
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty()
    {
        assert!(started.elapsed() < PATIENCE, "getMe was never called");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (signals, launched)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_during_start_stops_cleanly() {
    let (telegram, cfg) = slow_start().await;
    let (signals, launched) = launch_until_get_me(&telegram, cfg).await;
    let started = Instant::now();
    signals.send(SignalKind::Terminate).await.unwrap();
    let outcome = timeout(PATIENCE, launched).await.expect("in time").unwrap();
    assert_eq!(outcome.expect("no start error"), Outcome::Clean);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_signal_during_start_exits_130() {
    let (telegram, cfg) = slow_start().await;
    let (signals, launched) = launch_until_get_me(&telegram, cfg).await;
    let started = Instant::now();
    signals.send(SignalKind::Terminate).await.unwrap();
    signals.send(SignalKind::Interrupt).await.unwrap();
    let outcome = timeout(PATIENCE, launched).await.expect("in time").unwrap();
    assert_eq!(outcome.expect("no start error"), Outcome::Forced);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_routes_serves_the_next_request() {
    let telegram = telegram_with_hooked_bot().await;
    let config = |alias: &str| {
        hooked(
            &telegram,
            QUICK,
            "[{ to: [workers] }]",
            &format!("  workers: {{ pull: {{ aliases: [{alias}] }} }}"),
        )
    };
    let app = App::start(boot(&config("/old")))
        .await
        .expect("tgin starts");
    let addr = app.data_addrs()[0];
    let http = client();
    let status = |path: &'static str| {
        let http = http.clone();
        async move {
            http.get(format!("http://{addr}{path}"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    assert_eq!(status("/old").await, 200);
    assert_eq!(status("/new").await, 404);

    assert!(app.set_routes(&load(&config("/new"))));
    assert_eq!(status("/old").await, 404);
    assert_eq!(status("/new").await, 200);
    assert_eq!(status("/c/workers/getUpdates").await, 200);

    let app = running(app);
    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
}

/// Fails the first `failures` calls with a 500, then names the bot.
struct FlakyGetMe {
    calls: std::sync::atomic::AtomicUsize,
    failures: usize,
}

impl Respond for FlakyGetMe {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call < self.failures {
            ResponseTemplate::new(500).set_body_json(json!({
                "ok": false, "error_code": 500, "description": "Internal Server Error"
            }))
        } else {
            me("tgin_bot")
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_get_me_retried_until_username_known() {
    let telegram = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/getMe")))
        .respond_with(FlakyGetMe {
            calls: std::sync::atomic::AtomicUsize::new(0),
            failures: 2,
        })
        .mount(&telegram)
        .await;
    let app = start(&hooked(
        &telegram,
        QUICK,
        "\n      - { match: { commands: [admin] }, to: [admin] }\n      - { to: [workers] }",
        "  admin: { pull: {} }\n  workers: { pull: {} }",
    ))
    .await;
    // Ready after the first, failed getMe.
    assert_eq!(app.readiness().await, 200);

    // Until the username is known, `/admin@tgin_bot` is someone else's.
    let started = Instant::now();
    let mut id = 1;
    loop {
        app.deliver(&[(id, "/admin@tgin_bot")]).await;
        let admin = app
            .poll_ids(&format!("/c/admin/getUpdates?offset={id}"), None)
            .await;
        if admin == vec![id] {
            break;
        }
        assert_eq!(admin, Vec::<i64>::new());
        assert_eq!(
            app.poll_ids(&format!("/c/workers/getUpdates?offset={id}"), None)
                .await,
            vec![id]
        );
        assert!(
            started.elapsed() < PATIENCE,
            "the username was never learnt"
        );
        id += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let get_me = format!("/bot{TOKEN}/getMe");
    let calls = telegram
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == get_me)
        .count();
    assert_eq!(calls, 3);

    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
}

/// Two webhook bots whose `getMe` each answers after `GET_ME_DELAY`: the
/// start asks for both identities at once, so it takes one delay, not two.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_get_me_calls_run_concurrently() {
    const GET_ME_DELAY: Duration = Duration::from_millis(1500);
    let telegram = MockServer::start().await;
    mount_bot(
        &telegram,
        TOKEN,
        me("first_bot").set_delay(GET_ME_DELAY),
        Vec::new(),
    )
    .await;
    mount_bot(
        &telegram,
        OTHER_TOKEN,
        me("second_bot").set_delay(GET_ME_DELAY),
        Vec::new(),
    )
    .await;
    let started = Instant::now();
    let app = start(&format!(
        r#"version: 1
{QUICK}
server: {{ listen: ["127.0.0.1:0"] }}
bots:
  - name: first
    token: "{TOKEN}"
    api_url: {api}
    ingress: {{ webhook: {{ path: /first, secret_token: none }} }}
    routes: [{{ to: [first] }}]
  - name: second
    token: "{OTHER_TOKEN}"
    api_url: {api}
    ingress: {{ webhook: {{ path: /second, secret_token: none }} }}
    routes: [{{ to: [second] }}]
consumers:
  first: {{ pull: {{}} }}
  second: {{ pull: {{}} }}
"#,
        api = telegram.uri()
    ))
    .await;
    let took = started.elapsed();
    assert!(took >= GET_ME_DELAY, "started after {took:?}");
    assert!(took < GET_ME_DELAY * 2, "started after {took:?}");

    app.signal(SignalKind::Terminate).await;
    assert_eq!(app.outcome().await, Outcome::Clean);
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

/// The `tgin` binary with `args`, `TGIN_CONFIG` unset unless given.
fn tgin(args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_tgin"));
    command.args(args).env_remove("TGIN_CONFIG");
    command
}

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn output(command: &mut std::process::Command) -> Output {
    let out = command.output().expect("tgin runs");
    Output {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A scratch directory of its own for `test`.
fn scratch(test: &str) -> std::path::PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

#[test]
fn legacy_file_flag_hint_exits_1() {
    for args in [["-f", "tgin.ron"].as_slice(), &["--file=tgin.ron"]] {
        let out = output(&mut tgin(args));
        assert_eq!(out.code, Some(1), "{args:?}: {}", out.stderr);
        assert!(
            out.stderr.contains(
                "tgin 1.0 reads YAML; convert with: tgin config migrate tgin.ron -o tgin.yaml"
            ),
            "{}",
            out.stderr
        );
    }
}

#[test]
fn usage_error_exits_64() {
    for args in [
        ["--bogus"].as_slice(),
        &["frobnicate"],
        &["config"],
        &["-c", "a.yaml", "config", "schema"],
    ] {
        let out = output(&mut tgin(args));
        assert_eq!(out.code, Some(64), "{args:?}: {}", out.stderr);
        assert!(!out.stderr.is_empty(), "{args:?} printed no usage");
    }
    let help = output(&mut tgin(&["--help"]));
    assert_eq!(help.code, Some(0));
    assert!(help.stdout.contains("config"), "{}", help.stdout);
}

#[test]
fn config_validate_lists_all_errors_exit_1() {
    let dir = scratch("config_validate_lists_all_errors_exit_1");
    let bad = dir.join("bad.yaml");
    std::fs::write(
        &bad,
        "version: 1\nreload: { debounce: 9ms }\nbots:\n  - name: a\n    token: t\n    routes: [{ to: [nope] }]\n  - name: a\n    token: u\n",
    )
    .unwrap();
    let out = output(&mut tgin(&["config", "validate", bad.to_str().unwrap()]));
    assert_eq!(out.code, Some(1), "{}", out.stderr);
    for expected in [
        "reload.debounce",
        "minimum of 10ms",
        "unknown consumer `nope`",
        "bot name `a` is already used by bots[0]",
    ] {
        assert!(
            out.stderr.contains(expected),
            "missing {expected:?} in:\n{}",
            out.stderr
        );
    }
    assert_eq!(out.stderr.matches("error: ").count(), 3, "{}", out.stderr);

    let good = dir.join("good.yaml");
    std::fs::write(&good, "version: 1\n").unwrap();
    let out = output(&mut tgin(&["config", "validate", good.to_str().unwrap()]));
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert!(out.stdout.contains("valid"), "{}", out.stdout);
}

#[test]
fn config_print_redacts_and_schema_prints() {
    let dir = scratch("config_print_redacts_and_schema_prints");
    let file = dir.join("tgin.yaml");
    std::fs::write(
        &file,
        "version: 1\nbots:\n  - name: a\n    token: ${env:PRINT_TEST_TOKEN}\n",
    )
    .unwrap();
    let out =
        output(tgin(&["config", "print", file.to_str().unwrap()]).env("PRINT_TEST_TOKEN", TOKEN));
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    let printed: Value = serde_json::from_str(&out.stdout).expect("JSON");
    assert_eq!(printed["bots"][0]["token"], "[redacted]");
    assert!(!out.stdout.contains("AAAAexample"), "{}", out.stdout);

    let schema = output(&mut tgin(&["config", "schema"]));
    assert_eq!(schema.code, Some(0));
    let schema: Value = serde_json::from_str(&schema.stdout).expect("JSON schema");
    assert_eq!(schema["title"], "tgin configuration, version 1");

    let version = output(&mut tgin(&["version"]));
    assert_eq!(version.code, Some(0));
    assert!(
        version
            .stdout
            .starts_with(&format!("tgin {}", env!("CARGO_PKG_VERSION"))),
        "{}",
        version.stdout
    );
}

#[test]
fn config_path_from_flag_then_env_then_default() {
    let dir = scratch("config_path_from_flag_then_env_then_default");
    let cases: [(&[&str], Option<&str>, &str); 4] = [
        (
            &["-c", "from-flag.yaml"],
            Some("from-env.yaml"),
            "from-flag.yaml",
        ),
        (
            &["run", "--config", "from-flag.yaml"],
            Some("from-env.yaml"),
            "from-flag.yaml",
        ),
        (&[], Some("from-env.yaml"), "from-env.yaml"),
        (&["run"], None, "tgin.yaml"),
    ];
    for (args, env, expected) in cases {
        let mut command = tgin(args);
        command.current_dir(&dir);
        if let Some(env) = env {
            command.env("TGIN_CONFIG", env);
        }
        let out = output(&mut command);
        assert_eq!(out.code, Some(1), "{args:?}: {}", out.stderr);
        assert!(
            out.stderr
                .contains(&format!("cannot read the config file {expected}")),
            "{args:?} {env:?}: {}",
            out.stderr
        );
    }
}

#[test]
fn shipped_config_is_valid() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tgin.yaml");
    let text = std::fs::read_to_string(&file).expect("the root tgin.yaml");
    let mut env = MapEnv::default();
    env.vars.insert("TOKEN".to_owned(), TOKEN.to_owned());
    let cfg = config::load(&text, &file, &env).unwrap_or_else(|errors| {
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        panic!("{}", errors.join("\n"))
    });
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

/// The `tgin` binary serving a webhook bot backed by `telegram`, its
/// listeners on ports the OS picks, with `runtime` timings; waits until
/// ready. Returns the child and the admin plane's base URL.
#[cfg(unix)]
async fn spawn_tgin(
    test: &str,
    telegram: &MockServer,
    runtime: &str,
) -> (std::process::Child, String) {
    let file = scratch(test).join("tgin.yaml");
    std::fs::write(
        &file,
        format!(
            r#"version: 1
{runtime}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"] }}
bots:
  - name: hooked
    token: "{TOKEN}"
    api_url: {api}
    ingress: {{ webhook: {{ path: /hook, secret_token: none }} }}
    routes: [{{ to: [workers] }}]
consumers:
  workers: {{ pull: {{}} }}
"#,
            api = telegram.uri()
        ),
    )
    .unwrap();
    let (mut child, admin) = support::spawn_listening(
        &mut tgin(&["-c", file.to_str().unwrap()]),
        &file.with_file_name("tgin.log"),
        PATIENCE,
    )
    .await;
    let base = format!("http://{admin}");
    let http = client();
    let started = Instant::now();
    while started.elapsed() < PATIENCE {
        if let Ok(response) = http.get(format!("{base}/readyz")).send().await
            && response.status() == 200
        {
            return (child, base);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("tgin never became ready");
}

#[cfg(unix)]
fn kill(child: &std::process::Child, signal: &str) {
    let status = std::process::Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success(), "kill {signal} failed");
}

/// Waits for `child` to exit, within [`PATIENCE`].
#[cfg(unix)]
async fn exit_code(mut child: std::process::Child) -> Option<i32> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("waitable") {
            return status.code();
        }
        if started.elapsed() > PATIENCE {
            let _ = child.kill();
            let _ = child.wait();
            panic!("tgin did not exit in time");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn os_sigterm_exits_0() {
    let telegram = telegram_with_hooked_bot().await;
    let (child, _base) = spawn_tgin("os_sigterm_exits_0", &telegram, QUICK).await;
    kill(&child, "-TERM");
    assert_eq!(exit_code(child).await, Some(0));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn os_sigterm_then_sigint_exits_130() {
    let telegram = telegram_with_hooked_bot().await;
    let (mut child, base) = spawn_tgin(
        "os_sigterm_then_sigint_exits_130",
        &telegram,
        "runtime: { lame_duck: 30s, shutdown_grace: 30s }",
    )
    .await;
    kill(&child, "-TERM");
    let http = client();
    let started = Instant::now();
    while http
        .get(format!("{base}/readyz"))
        .send()
        .await
        .map(|r| r.status())
        .ok()
        != Some(reqwest::StatusCode::SERVICE_UNAVAILABLE)
    {
        if started.elapsed() > PATIENCE {
            let _ = child.kill();
            let _ = child.wait();
            panic!("/readyz never turned 503");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    kill(&child, "-INT");
    assert_eq!(exit_code(child).await, Some(130));
}
