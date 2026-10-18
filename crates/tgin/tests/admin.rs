//! The admin plane and hot reload, end to end: the app in process, wiremock
//! as Telegram and as push downstreams, real loopback sockets on ports the
//! OS picks, and the `tgin admin` command line against them.

mod support;

use std::collections::BTreeSet;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use serde_json::{Value, json};
use tgin::admin::Status;
use tgin::app::{App, Boot, StartError};
use tgin::exit::Outcome;
use tgin_runtime::config::MapEnv;
use tgin_runtime::lifecycle::{SignalKind, Signals};
use tgin_runtime::obs::{METRICS_CONTENT_TYPE, MetricsRegistry};
use tgin_runtime::secret::Redactor;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Bounds every wait, so a stall fails the test instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(10);
const TOKEN: &str = "123456789:AAAAexampleSecretTokenValue_-xyz";
const OTHER_TOKEN: &str = "987654321:BBBBanotherSecretTokenValue_-xyz";
const THIRD_TOKEN: &str = "555555555:CCCCthirdSecretTokenValue_-xyzab";
const FOURTH_TOKEN: &str = "444444444:DDDDfourthSecretTokenValue_-xyza";
const HOOK_SECRET: &str = "hook-secret-value";
const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";
const ADMIN_TOKEN: &str = "admin-token-value-123";
/// Fast shutdown: no lame duck, a short grace.
const QUICK: &str = "runtime: { lame_duck: 0s, shutdown_grace: 2s }";

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

fn update(id: i64, text: &str) -> String {
    format!(
        r#"{{"update_id":{id},"message":{{"message_id":{id},"chat":{{"id":500,"type":"private"}},"from":{{"id":7,"is_bot":false,"first_name":"U"}},"text":"{text}"}}}}"#
    )
}

fn ok(result: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": result }))
}

fn me(username: &str) -> ResponseTemplate {
    ok(json!({ "id": 42, "is_bot": true, "first_name": "Bot", "username": username }))
}

/// Answers `getUpdates` with an empty batch after `idle`, and reports
/// every poll.
struct Polls {
    idle: Duration,
    polls: mpsc::UnboundedSender<()>,
}

impl Respond for Polls {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let _ = self.polls.send(());
        ok(json!([])).set_delay(self.idle)
    }
}

/// Mounts one bot on a fake Bot API: `getMe` answers `get_me`, webhook
/// calls succeed, `getUpdates` answers empty batches after `idle`. Returns
/// the poll reports.
async fn mount_bot(
    server: &MockServer,
    token: &str,
    get_me: ResponseTemplate,
    idle: Duration,
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
        .respond_with(Polls { idle, polls })
        .mount(server)
        .await;
    rx
}

/// Telegram with one bot, `TOKEN`, named `tgin_bot`.
async fn telegram() -> MockServer {
    let telegram = MockServer::start().await;
    mount_bot(&telegram, TOKEN, me("tgin_bot"), Duration::from_secs(3600)).await;
    telegram
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

/// A push downstream answering 200 on `/bot`; returns what it saw.
async fn downstream() -> (MockServer, mpsc::UnboundedReceiver<Request>) {
    let server = MockServer::start().await;
    let (seen, rx) = mpsc::unbounded_channel();
    Mock::given(method("POST"))
        .and(path("/bot"))
        .respond_with(Record {
            seen,
            response: ResponseTemplate::new(200),
        })
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

/// A config with the data plane and the admin plane (with
/// [`ADMIN_TOKEN`]) on ports the OS picks, quick shutdown timings, and
/// `rest`. The config watch is off unless `rest` sets `reload` itself: a
/// rewrite would otherwise reload on its own.
fn config(rest: &str) -> String {
    let reload = if rest.contains("reload:") {
        ""
    } else {
        "reload: { watch: false }\n"
    };
    format!(
        "version: 1\n{QUICK}\nserver: {{ listen: [\"127.0.0.1:0\"] }}\nadmin: {{ listen: [\"127.0.0.1:0\"], token: {ADMIN_TOKEN} }}\n{reload}{rest}"
    )
}

/// The webhook bot `name` with `token`, served on `path` with
/// [`HOOK_SECRET`], routing every update to `to`.
fn hook_bot(telegram: &MockServer, name: &str, token: &str, path: &str, to: &str) -> String {
    format!(
        "  - name: {name}\n    token: \"{token}\"\n    api_url: {api}\n    ingress: {{ webhook: {{ path: {path}, secret_token: {HOOK_SECRET} }} }}\n    routes: [{{ to: [{to}] }}]\n",
        api = telegram.uri()
    )
}

/// The long-poll bot `name` with `token`, routing every update to `to`.
fn poll_bot(telegram: &MockServer, name: &str, token: &str, to: &str) -> String {
    format!(
        "  - name: {name}\n    token: \"{token}\"\n    api_url: {api}\n    ingress: {{ long_poll: {{ timeout: 1s, delete_webhook_on_start: false }} }}\n    routes: [{{ to: [{to}] }}]\n",
        api = telegram.uri()
    )
}

/// The bot `hooked` (on `/hook`, [`TOKEN`]) feeding the pull consumer
/// `workers`, plus `extra` config.
fn hooked(telegram: &MockServer, extra: &str) -> String {
    config(&format!(
        "{extra}bots:\n{}consumers:\n  workers: {{ pull: {{}} }}\n",
        hook_bot(telegram, "hooked", TOKEN, "/hook", "workers")
    ))
}

/// A scratch directory of its own for `test`.
fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("admin")
        .join(test);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// The app, running from a config file the test can rewrite.
struct Tgin {
    file: PathBuf,
    data: SocketAddr,
    admin: SocketAddr,
    signals: mpsc::Sender<SignalKind>,
    run: JoinHandle<Outcome>,
    metrics: Arc<MetricsRegistry>,
    redactor: Arc<ArcSwap<Redactor>>,
    http: reqwest::Client,
}

fn write_config(test: &str, yaml: &str) -> PathBuf {
    let file = scratch(test).join("tgin.yaml");
    std::fs::write(&file, yaml).expect("the config file");
    file
}

fn boot(file: &Path) -> Boot {
    Boot::load(file, Arc::new(MapEnv::default()))
        .unwrap_or_else(|err| panic!("invalid test config: {err:?}"))
}

async fn start(test: &str, yaml: &str) -> Tgin {
    LazyLock::force(&LOGS);
    let file = write_config(test, yaml);
    let app = App::start(boot(&file)).await.expect("tgin starts");
    running(app, file)
}

fn running(app: App, file: PathBuf) -> Tgin {
    let (signals, rx) = mpsc::channel(4);
    let data = app.data_addrs()[0];
    let mut admin = app.admin_addrs()[0];
    // A listener on every interface is reached through loopback.
    if admin.ip().is_unspecified() {
        admin.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
    }
    let metrics = app.metrics();
    let redactor = app.redactor();
    let run = tokio::spawn(app.run(Signals::from_channel(rx)));
    Tgin {
        file,
        data,
        admin,
        signals,
        run,
        metrics,
        redactor,
        http: client(),
    }
}

/// An answer: its status and body.
struct Answer {
    status: u16,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|err| panic!("{err}: {} {}", self.status, self.body))
    }
}

async fn answer(request: reqwest::RequestBuilder) -> Answer {
    let response = request.send().await.expect("the request is answered");
    Answer {
        status: response.status().as_u16(),
        body: response.text().await.expect("the body"),
    }
}

impl Tgin {
    fn data_url(&self, path: &str) -> String {
        format!("http://{}{path}", self.data)
    }

    fn admin_url(&self, path: &str) -> String {
        format!("http://{}{path}", self.admin)
    }

    /// GETs `path` from the admin plane, with `token` if given.
    async fn admin_get(&self, path: &str, token: Option<&str>) -> Answer {
        let mut request = self.http.get(self.admin_url(path));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        answer(request).await
    }

    /// POSTs to `path` on the admin plane, with `token` if given.
    async fn admin_post(&self, path: &str, token: Option<&str>) -> Answer {
        let mut request = self.http.post(self.admin_url(path));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        answer(request).await
    }

    async fn status(&self) -> Status {
        let answer = self.admin_get("/v1/status", Some(ADMIN_TOKEN)).await;
        assert_eq!(answer.status, 200, "{}", answer.body);
        serde_json::from_str(&answer.body).expect("a status")
    }

    /// Rewrites the config file.
    fn rewrite(&self, yaml: &str) {
        std::fs::write(&self.file, yaml).expect("the config file");
    }

    /// `POST /v1/reload`.
    async fn reload(&self) -> Answer {
        self.admin_post("/v1/reload", Some(ADMIN_TOKEN)).await
    }

    /// Waits until the status shows `generation`.
    async fn wait_for_generation(&self, generation: u64) -> Status {
        let started = Instant::now();
        loop {
            let status = self.status().await;
            if status.generation == generation {
                return status;
            }
            assert!(
                started.elapsed() < PATIENCE,
                "generation {} never became {generation}",
                status.generation
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until the status satisfies `ok`; `what` names it on failure.
    async fn wait_status(&self, what: &str, ok: impl Fn(&Status) -> bool) -> Status {
        let started = Instant::now();
        loop {
            let status = self.status().await;
            if ok(&status) {
                return status;
            }
            assert!(
                started.elapsed() < PATIENCE,
                "{what} never held: {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn readiness(&self) -> u16 {
        self.admin_get("/readyz", None).await.status
    }

    async fn wait_until_not_ready(&self) {
        let started = Instant::now();
        while self.readiness().await != 503 {
            assert!(started.elapsed() < PATIENCE, "/readyz stayed ready");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn post_update(&self, path: &str, body: String) -> u16 {
        answer(
            self.http
                .post(self.data_url(path))
                .header("content-type", "application/json")
                .header(SECRET_HEADER, HOOK_SECRET)
                .body(body),
        )
        .await
        .status
    }

    /// Posts updates to `path`, each answered 200.
    async fn deliver(&self, path: &str, ids: &[i64]) {
        for id in ids {
            assert_eq!(
                self.post_update(path, update(*id, "text")).await,
                200,
                "update {id}"
            );
        }
    }

    /// GETs a pull route with `auth` as the `Authorization` header.
    async fn pull(&self, path_and_query: &str, auth: Option<&str>) -> Answer {
        let mut request = self.http.get(self.data_url(path_and_query));
        if let Some(auth) = auth {
            request = request.header("authorization", auth);
        }
        answer(request).await
    }

    /// The update ids a successful poll returned.
    async fn poll_ids(&self, path_and_query: &str) -> Vec<i64> {
        let answer = self.pull(path_and_query, None).await;
        assert_eq!(answer.status, 200, "{}", answer.body);
        ids(&answer.body)
    }

    fn metrics_text(&self) -> String {
        self.metrics.encode()
    }

    async fn signal(&self, kind: SignalKind) {
        self.signals.send(kind).await.expect("the app listens");
    }

    async fn stop(self) {
        self.signal(SignalKind::Terminate).await;
        let outcome = timeout(PATIENCE, self.run)
            .await
            .expect("the app exits in time")
            .expect("the app does not panic");
        assert_eq!(outcome, Outcome::Clean);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probes_unauthenticated() {
    let telegram = telegram().await;
    let tgin = start("probes_unauthenticated", &hooked(&telegram, "")).await;

    let livez = tgin.admin_get("/livez", None).await;
    assert_eq!((livez.status, livez.body.as_str()), (200, "ok"));
    let readyz = tgin.admin_get("/readyz", None).await;
    assert_eq!(readyz.status, 200, "{}", readyz.body);
    assert_eq!(
        readyz.json(),
        json!({
            "ready": true,
            "phase": "running",
            "components": {
                "admin_plane": "ready",
                "data_plane": "ready",
                "engine": "ready",
                "source:hooked": "ready",
            },
        })
    );
    // The probes left the data plane.
    for probe in ["/livez", "/readyz"] {
        assert_eq!(
            answer(tgin.http.get(tgin.data_url(probe))).await.status,
            404,
            "{probe}"
        );
    }
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_requires_token() {
    let telegram = telegram().await;
    let tgin = start("status_requires_token", &hooked(&telegram, "")).await;
    tgin.deliver("/hook", &[1, 2]).await;

    let unauthorized = json!({ "error": "unauthorized" });
    for token in [
        None,
        Some("wrong"),
        Some(&ADMIN_TOKEN[..ADMIN_TOKEN.len() - 1]),
    ] {
        for path in ["/v1/status", "/v1/config"] {
            let answer = tgin.admin_get(path, token).await;
            assert_eq!(answer.status, 401, "{path} {token:?}");
            assert_eq!(answer.json(), unauthorized);
        }
        for path in [
            "/v1/reload",
            "/v1/consumers/workers/pause",
            "/v1/consumers/workers/purge",
        ] {
            let answer = tgin.admin_post(path, token).await;
            assert_eq!(answer.status, 401, "{path} {token:?}");
        }
    }

    let status = tgin.status().await;
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
    assert_eq!((status.generation, status.phase.as_str()), (1, "running"));
    let source = &status.sources[0];
    assert_eq!(
        (
            source.name.as_str(),
            source.kind.as_str(),
            source.ingress.as_deref(),
            source.state.as_str(),
            source.updates_total,
            source.errors_total,
        ),
        ("hooked", "bot", Some("webhook"), "running", 2, 0)
    );
    let consumer = &status.consumers[0];
    assert_eq!(
        (
            consumer.name.as_str(),
            consumer.mode.as_str(),
            consumer.depth,
            consumer.paused
        ),
        ("workers", "pull", 2, false),
        "the unauthorized requests changed nothing"
    );

    let config = tgin.admin_get("/v1/config", Some(ADMIN_TOKEN)).await;
    assert_eq!(config.status, 200);
    let shown = config.json();
    assert_eq!(shown["bots"][0]["token"], "[redacted]");
    assert_eq!(shown["admin"]["token"], "[redacted]");
    for secret in [TOKEN, ADMIN_TOKEN, HOOK_SECRET] {
        assert!(!config.body.contains(secret), "{}", config.body);
    }
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_public_flag() {
    let telegram = telegram().await;
    let tgin = start("metrics_public_flag", &hooked(&telegram, "")).await;

    let closed = tgin.admin_get("/metrics", None).await;
    assert_eq!(closed.status, 401);
    let response = tgin
        .http
        .get(tgin.admin_url("/metrics"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"].to_str().unwrap(),
        METRICS_CONTENT_TYPE
    );
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("tgin_config_generation 1\n")
    );

    // The flag, like the token, is read from the running config.
    tgin.rewrite(&hooked(&telegram, "").replace(
        &format!("token: {ADMIN_TOKEN} }}"),
        &format!("token: {ADMIN_TOKEN}, public_metrics: true }}"),
    ));
    assert_eq!(tgin.reload().await.status, 200);
    let open = tgin.admin_get("/metrics", None).await;
    assert_eq!(open.status, 200);
    assert!(open.body.contains("tgin_config_generation 2\n"));
    assert_eq!(tgin.admin_get("/v1/status", None).await.status, 401);
    tgin.stop().await;
}

/// The bot `hooked` feeding both `workers` and `extra`, plus `extra_config`.
fn with_extra_consumer(telegram: &MockServer, extra_config: &str) -> String {
    config(&format!(
        "{extra_config}bots:\n{}consumers:\n  workers: {{ pull: {{}} }}\n  extra: {{ pull: {{}} }}\n",
        hook_bot(telegram, "hooked", TOKEN, "/hook", "workers, extra")
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_applies_and_reports() {
    let telegram = telegram().await;
    let tgin = start("reload_applies_and_reports", &hooked(&telegram, "")).await;
    assert_eq!(tgin.pull("/c/extra/getUpdates", None).await.status, 404);

    tgin.rewrite(&with_extra_consumer(&telegram, ""));
    let reload = tgin.reload().await;
    assert_eq!(reload.status, 200, "{}", reload.body);
    assert_eq!(
        reload.json(),
        json!({
            "generation": 2,
            "added": ["extra"],
            "removed": [],
            "changed": [],
            "sources_added": [],
            "sources_removed": [],
            "sources_restarted": [],
            "upstream_changed": [],
            "restart_required": [],
        })
    );

    tgin.deliver("/hook", &[1]).await;
    assert_eq!(tgin.poll_ids("/c/extra/getUpdates").await, vec![1]);
    assert_eq!(tgin.poll_ids("/c/workers/getUpdates").await, vec![1]);
    let status = tgin.status().await;
    assert_eq!(status.generation, 2);
    let names: Vec<&str> = status.consumers.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["workers", "extra"]);
    let metrics = tgin.metrics_text();
    assert!(
        metrics.contains("tgin_config_reloads_total{outcome=\"applied\"} 1\n"),
        "{metrics}"
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_invalid_returns_422_and_keeps_running_config() {
    let telegram = telegram().await;
    let tgin = start(
        "reload_invalid_returns_422_and_keeps_running_config",
        &hooked(&telegram, ""),
    )
    .await;
    let before = tgin.admin_get("/v1/config", Some(ADMIN_TOKEN)).await.body;

    let broken = with_extra_consumer(&telegram, "").replace("workers, extra", "workers, nope");
    tgin.rewrite(&broken);
    let reload = tgin.reload().await;
    assert_eq!(reload.status, 422, "{}", reload.body);
    let errors = reload.json()["errors"].as_array().expect("errors").clone();
    assert_eq!(errors.len(), 1, "{errors:?}");
    let error = &errors[0];
    let line = broken
        .lines()
        .position(|line| line.contains("nope"))
        .expect("the bad line")
        + 1;
    assert_eq!(error["path"], "bots[0].routes[0].to[1]");
    assert_eq!(error["line"], line);
    assert!(error["column"].as_u64().is_some(), "{error}");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("unknown consumer `nope`"),
        "{error}"
    );
    assert!(error.get("hint").is_some(), "{error}");

    // A file that is not even YAML is refused the same way, and its text
    // is not echoed.
    tgin.rewrite(&format!("version: 1\nbots: [{{ token: \"{TOKEN}\"\n"));
    let reload = tgin.reload().await;
    assert_eq!(reload.status, 422, "{}", reload.body);
    assert!(!reload.body.contains(TOKEN), "{}", reload.body);

    assert_eq!(tgin.status().await.generation, 1);
    assert_eq!(
        tgin.admin_get("/v1/config", Some(ADMIN_TOKEN)).await.body,
        before
    );
    tgin.deliver("/hook", &[1]).await;
    assert_eq!(tgin.poll_ids("/c/workers/getUpdates").await, vec![1]);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_reload_counted() {
    let telegram = telegram().await;
    let tgin = start("rejected_reload_counted", &hooked(&telegram, "")).await;

    tgin.rewrite("version: 1\nconsumers: { a: { pull: {} }, a: { pull: {} } }\n");
    assert_eq!(tgin.reload().await.status, 422);
    tgin.rewrite("version: 2\n");
    assert_eq!(tgin.reload().await.status, 422);
    let metrics = tgin.metrics_text();
    assert!(
        metrics.contains("tgin_config_reloads_total{outcome=\"rejected\"} 2\n"),
        "{metrics}"
    );
    assert!(
        metrics.contains("tgin_config_reloads_total{outcome=\"applied\"} 0\n"),
        "{metrics}"
    );
    assert!(metrics.contains("tgin_config_generation 1\n"), "{metrics}");
    tgin.stop().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sighup_triggers_reload() {
    let telegram = telegram().await;
    let tgin = start("sighup_triggers_reload", &hooked(&telegram, "")).await;

    tgin.rewrite(&with_extra_consumer(&telegram, ""));
    tgin.signal(SignalKind::Hangup).await;
    let status = tgin.wait_for_generation(2).await;
    assert_eq!(status.consumers.len(), 2);
    // An explicit trigger reloads also when the file is unchanged: its
    // references may resolve differently.
    tgin.signal(SignalKind::Hangup).await;
    tgin.wait_for_generation(3).await;
    assert_eq!(tgin.readiness().await, 200);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_triggers_reload() {
    let telegram = telegram().await;
    let watched = "reload: { watch: true, debounce: 50ms }\n";
    let tgin = start("watch_triggers_reload", &hooked(&telegram, watched)).await;
    assert_eq!(tgin.status().await.generation, 1);

    tgin.rewrite(&with_extra_consumer(&telegram, watched));
    let status = tgin.wait_for_generation(2).await;
    assert_eq!(status.consumers.len(), 2);
    tgin.stop().await;
}

/// The file changed between the load and the start of the watch: the app
/// compares the file with what it loaded once the watch runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_rereads_after_start() {
    let telegram = telegram().await;
    let watched = "reload: { watch: true, debounce: 50ms }\n";
    let file = write_config("watch_rereads_after_start", &hooked(&telegram, watched));
    let mut loaded = boot(&file);
    std::fs::write(&file, with_extra_consumer(&telegram, watched)).unwrap();
    loaded.loaded = hooked(&telegram, watched).into_bytes();
    let tgin = running(App::start(loaded).await.expect("tgin starts"), file);

    let status = tgin.wait_for_generation(2).await;
    assert_eq!(status.consumers.len(), 2);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_stops_delivery_resume_restarts() {
    let telegram = telegram().await;
    let (down, mut seen) = downstream().await;
    // `probe` gets every update too, never paused: its delivery shows that
    // the paused consumer's dispatcher had its chance.
    let tgin = start(
        "pause_stops_delivery_resume_restarts",
        &config(&format!(
            "bots:\n{}consumers:\n  down: {{ push: {{ endpoints: [{{ url: \"{url}/bot\" }}] }} }}\n  probe: {{ push: {{ endpoints: [{{ url: \"{url}/bot?probe\" }}] }} }}\n",
            hook_bot(&telegram, "hooked", TOKEN, "/hook", "down, probe"),
            url = down.uri()
        )),
    )
    .await;
    let is_probe = |req: &Request| req.url.query() == Some("probe");

    let paused = tgin
        .admin_post("/v1/consumers/down/pause", Some(ADMIN_TOKEN))
        .await;
    assert_eq!((paused.status, paused.body.as_str()), (204, ""));
    tgin.deliver("/hook", &[1]).await;
    let first = next(&mut seen).await;
    assert!(is_probe(&first), "delivered while paused: {}", first.url);
    let consumer = tgin.status().await.consumers[0].clone();
    assert_eq!(
        (
            consumer.name.as_str(),
            consumer.paused,
            consumer.depth,
            consumer.in_flight
        ),
        ("down", true, 1, 0)
    );
    assert!(seen.try_recv().is_err(), "delivered while paused");

    let resumed = tgin
        .admin_post("/v1/consumers/down/resume", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(resumed.status, 204);
    let delivered = next(&mut seen).await;
    assert!(!is_probe(&delivered));
    assert_eq!(delivered.body, update(1, "text").as_bytes());
    assert!(!tgin.status().await.consumers[0].paused);

    for action in ["pause", "resume", "drain", "purge"] {
        let unknown = tgin
            .admin_post(&format!("/v1/consumers/nobody/{action}"), Some(ADMIN_TOKEN))
            .await;
        assert_eq!(unknown.status, 404, "{action}");
        assert_eq!(unknown.json(), json!({ "error": "unknown consumer" }));
    }
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_counts() {
    let telegram = telegram().await;
    let tgin = start("purge_counts", &hooked(&telegram, "")).await;
    tgin.deliver("/hook", &[1, 2, 3]).await;
    // A leased update is not purged.
    assert_eq!(
        tgin.poll_ids("/c/workers/getUpdates?limit=1").await,
        vec![1]
    );

    let purged = tgin
        .admin_post("/v1/consumers/workers/purge", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(purged.status, 200);
    assert_eq!(purged.json(), json!({ "purged": 2 }));
    assert_eq!(tgin.status().await.consumers[0].depth, 0);
    assert_eq!(
        tgin.poll_ids("/c/workers/getUpdates?offset=2").await,
        Vec::<i64>::new()
    );
    let metrics = tgin.metrics_text();
    assert!(
        metrics.contains("tgin_consumer_dropped_total{consumer=\"workers\",reason=\"purge\"} 2\n"),
        "{metrics}"
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_ids_survive_bot_insertion() {
    let telegram = telegram().await;
    mount_bot(
        &telegram,
        OTHER_TOKEN,
        me("inserted_bot"),
        Duration::from_secs(3600),
    )
    .await;
    let tgin = start(
        "source_ids_survive_bot_insertion",
        &config(&format!(
            "bots:\n{}consumers:\n  second: {{ pull: {{}} }}\n",
            hook_bot(&telegram, "hooked", TOKEN, "/hook", "second")
        )),
    )
    .await;

    // A bot inserted before `hooked`.
    tgin.rewrite(&config(&format!(
        "bots:\n{}{}consumers:\n  first: {{ pull: {{}} }}\n  second: {{ pull: {{}} }}\n",
        hook_bot(&telegram, "inserted", OTHER_TOKEN, "/first", "first"),
        hook_bot(&telegram, "hooked", TOKEN, "/hook", "second")
    )));
    let reload = tgin.reload().await;
    assert_eq!(reload.status, 200, "{}", reload.body);
    assert_eq!(reload.json()["sources_added"], json!(["inserted"]));

    tgin.deliver("/hook", &[1]).await;
    tgin.deliver("/first", &[2]).await;
    assert_eq!(tgin.poll_ids("/c/second/getUpdates").await, vec![1]);
    assert_eq!(tgin.poll_ids("/c/first/getUpdates").await, vec![2]);
    tgin.stop().await;
}

/// The `getUpdates` calls Telegram has seen for `token`.
async fn polls_of(telegram: &MockServer, token: &str) -> usize {
    let at = format!("/bot{token}/getUpdates");
    telegram
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == at)
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_starts_added_and_stops_removed_sources() {
    let quick_polls = Duration::from_millis(50);
    let telegram = MockServer::start().await;
    let mut old_polls = mount_bot(&telegram, TOKEN, me("old_poll"), quick_polls).await;
    mount_bot(&telegram, OTHER_TOKEN, me("old_hook"), quick_polls).await;
    let mut new_polls = mount_bot(&telegram, THIRD_TOKEN, me("new_poll"), quick_polls).await;
    mount_bot(&telegram, FOURTH_TOKEN, me("new_hook"), quick_polls).await;
    let tgin = start(
        "reload_starts_added_and_stops_removed_sources",
        &config(&format!(
            "bots:\n{}{}consumers:\n  polled: {{ pull: {{}} }}\n  hooked: {{ pull: {{}} }}\n",
            poll_bot(&telegram, "polled", TOKEN, "polled"),
            hook_bot(&telegram, "hooked", OTHER_TOKEN, "/hook", "hooked"),
        )),
    )
    .await;
    next(&mut old_polls).await;
    tgin.deliver("/hook", &[1]).await;

    let registered_hook = hook_bot(&telegram, "hooked2", FOURTH_TOKEN, "/hook2", "hooked2")
        .replace(
            "secret_token:",
            "public_url: \"https://tgin.example\", register: true, secret_token:",
        );
    tgin.rewrite(&config(&format!(
        "bots:\n{}{}consumers:\n  polled2: {{ pull: {{}} }}\n  hooked2: {{ pull: {{}} }}\n",
        poll_bot(&telegram, "polled2", THIRD_TOKEN, "polled2"),
        registered_hook,
    )));
    let reload = tgin.reload().await;
    assert_eq!(reload.status, 200, "{}", reload.body);
    let report = reload.json();
    assert_eq!(report["sources_added"], json!(["hooked2", "polled2"]));
    assert_eq!(report["sources_removed"], json!(["hooked", "polled"]));

    // The new long poll runs; the new webhook is served and registered.
    next(&mut new_polls).await;
    tgin.deliver("/hook2", &[2]).await;
    assert_eq!(tgin.poll_ids("/c/hooked2/getUpdates").await, vec![2]);
    let set_webhook = format!("/bot{FOURTH_TOKEN}/setWebhook");
    let registered = telegram
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.url.path() == set_webhook)
        .expect("setWebhook for the new webhook bot");
    let request: Value = serde_json::from_slice(&registered.body).unwrap_or_default();
    let body = String::from_utf8_lossy(&registered.body);
    assert!(
        request["url"] == "https://tgin.example/hook2" || body.contains("tgin.example%2Fhook2"),
        "{body}"
    );

    // The removed webhook is gone from the data plane.
    assert_eq!(tgin.post_update("/hook", update(3, "late")).await, 404);
    // The removed long poll stopped polling and left the task list.
    let started = Instant::now();
    loop {
        let status = tgin.status().await;
        let tasks: BTreeSet<&str> = status.tasks.iter().map(|t| t.name.as_str()).collect();
        if !tasks.contains("source:polled") {
            assert!(tasks.contains("source:polled2"), "{tasks:?}");
            break;
        }
        assert!(started.elapsed() < PATIENCE, "source:polled stayed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Its task has ended, so it polls no more: three polls of the new
    // source later, the old count has not moved.
    let polled = polls_of(&telegram, TOKEN).await;
    while new_polls.try_recv().is_ok() {}
    for _ in 0..3 {
        next(&mut new_polls).await;
    }
    assert_eq!(
        polls_of(&telegram, TOKEN).await,
        polled,
        "polled still polls"
    );

    let ready = tgin.admin_get("/readyz", None).await.json();
    let components: BTreeSet<String> = ready["components"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        components,
        [
            "admin_plane",
            "data_plane",
            "engine",
            "source:hooked2",
            "source:polled2"
        ]
        .map(str::to_owned)
        .into()
    );
    assert_eq!(ready["ready"], true);
    tgin.stop().await;
}

/// The readiness components `/readyz` lists.
async fn components(tgin: &Tgin) -> BTreeSet<String> {
    let ready = tgin.admin_get("/readyz", None).await.json();
    assert_eq!(ready["ready"], true, "{ready}");
    ready["components"]
        .as_object()
        .expect("components")
        .keys()
        .cloned()
        .collect()
}

/// The state of the supervised task `name`, if listed.
fn task_state(status: &Status, name: &str) -> Option<String> {
    status
        .tasks
        .iter()
        .find(|t| t.name == name)
        .map(|t| t.state.clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_restarts_changed_sources() {
    let quick_polls = Duration::from_millis(50);
    let telegram = MockServer::start().await;
    let mut rot_old = mount_bot(&telegram, TOKEN, me("rot_bot"), quick_polls).await;
    let mut rot_new = mount_bot(&telegram, THIRD_TOKEN, me("rot_bot"), quick_polls).await;
    let mut sw_polls = mount_bot(&telegram, OTHER_TOKEN, me("sw_bot"), quick_polls).await;
    let consumers = "consumers:\n  rot: { pull: {} }\n  sw: { pull: {} }\n";
    let tgin = start(
        "reload_restarts_changed_sources",
        &config(&format!(
            "bots:\n{}{}{consumers}",
            poll_bot(&telegram, "rot", TOKEN, "rot"),
            poll_bot(&telegram, "sw", OTHER_TOKEN, "sw"),
        )),
    )
    .await;
    next(&mut rot_old).await;
    next(&mut sw_polls).await;
    let expected: BTreeSet<String> = [
        "admin_plane",
        "data_plane",
        "engine",
        "source:rot",
        "source:sw",
    ]
    .map(str::to_owned)
    .into();

    // `rot` gets a new token, `sw` becomes a webhook.
    tgin.rewrite(&config(&format!(
        "bots:\n{}{}{consumers}",
        poll_bot(&telegram, "rot", THIRD_TOKEN, "rot"),
        hook_bot(&telegram, "sw", OTHER_TOKEN, "/sw", "sw"),
    )));
    let reload = tgin.reload().await;
    assert_eq!(reload.json()["sources_restarted"], json!(["rot", "sw"]));
    next(&mut rot_new).await;
    // `sw`'s long poll leaves the task list; `rot`'s new one runs.
    let status = tgin
        .wait_status("source:sw gone, source:rot running", |s| {
            task_state(s, "source:sw").is_none()
                && task_state(s, "source:rot").as_deref() == Some("running")
        })
        .await;
    let sw = status.sources.iter().find(|s| s.name == "sw").unwrap();
    assert_eq!(
        (sw.ingress.as_deref(), sw.state.as_str()),
        (Some("webhook"), "running")
    );
    assert_eq!(components(&tgin).await, expected);
    // The old token and the old long poll stopped: three polls of the new
    // token later, their counts have not moved.
    let (old_rot, old_sw) = (
        polls_of(&telegram, TOKEN).await,
        polls_of(&telegram, OTHER_TOKEN).await,
    );
    while rot_new.try_recv().is_ok() {}
    for _ in 0..3 {
        next(&mut rot_new).await;
    }
    assert_eq!(
        polls_of(&telegram, TOKEN).await,
        old_rot,
        "the old token polls"
    );
    assert_eq!(
        polls_of(&telegram, OTHER_TOKEN).await,
        old_sw,
        "sw still polls"
    );
    tgin.deliver("/sw", &[1]).await;
    assert_eq!(tgin.poll_ids("/c/sw/getUpdates").await, vec![1]);

    // And back: `sw` polls again, its webhook is gone.
    tgin.rewrite(&config(&format!(
        "bots:\n{}{}{consumers}",
        poll_bot(&telegram, "rot", THIRD_TOKEN, "rot"),
        poll_bot(&telegram, "sw", OTHER_TOKEN, "sw"),
    )));
    let reload = tgin.reload().await;
    assert_eq!(reload.json()["sources_restarted"], json!(["sw"]));
    while sw_polls.try_recv().is_ok() {}
    next(&mut sw_polls).await;
    tgin.wait_status("source:sw running", |s| {
        task_state(s, "source:sw").as_deref() == Some("running")
    })
    .await;
    assert_eq!(tgin.post_update("/sw", update(2, "late")).await, 404);
    assert_eq!(components(&tgin).await, expected);
    tgin.stop().await;
}

/// A reload that restarts the only healthy source, while the other has
/// failed for good, does not shut tgin down: the restarted source counts as
/// starting until it has started again (here: its getMe takes 2 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_does_not_count_as_failed() {
    let telegram = MockServer::start().await;
    let rejected = ResponseTemplate::new(401).set_body_json(json!({
        "ok": false, "error_code": 401, "description": "Unauthorized"
    }));
    mount_bot(&telegram, TOKEN, rejected, Duration::from_secs(3600)).await;
    mount_bot(
        &telegram,
        OTHER_TOKEN,
        me("slow_bot").set_delay(Duration::from_secs(2)),
        Duration::from_secs(3600),
    )
    .await;
    let consumers = "consumers:\n  dead: { pull: {} }\n  slow: { pull: {} }\n";
    let tgin = start(
        "restart_does_not_count_as_failed",
        &config(&format!(
            "bots:\n{}{}{consumers}",
            poll_bot(&telegram, "dead", TOKEN, "dead"),
            poll_bot(&telegram, "slow", OTHER_TOKEN, "slow"),
        )),
    )
    .await;
    tgin.wait_status("source:dead failed", |s| {
        task_state(s, "source:dead").as_deref() == Some("failed")
    })
    .await;

    tgin.rewrite(&config(&format!(
        "bots:\n{}{}{consumers}",
        poll_bot(&telegram, "dead", TOKEN, "dead"),
        hook_bot(&telegram, "slow", OTHER_TOKEN, "/slow", "slow"),
    )));
    let reload = tgin.reload().await;
    assert_eq!(reload.status, 200, "{}", reload.body);
    // The reload waited the 2 s getMe out, past a 1 s failure check.
    assert!(!tgin.run.is_finished(), "tgin shut down");
    assert_eq!(tgin.status().await.phase, "running");
    assert_eq!(tgin.readiness().await, 200);
    tgin.stop().await;
}

/// A `local` upstream named `tba` at `server`, in local mode if `local_mode`.
fn local_upstream(server: &MockServer, local_mode: bool) -> String {
    format!(
        "upstreams:\n  tba: {{ kind: local, url: \"{}\", local_mode: {local_mode} }}\n",
        server.uri()
    )
}

/// Records the arrival time and body of each request, answering `response`.
struct Arrivals {
    seen: mpsc::UnboundedSender<(Instant, Request)>,
    response: ResponseTemplate,
}

impl Respond for Arrivals {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let _ = self.seen.send((Instant::now(), req.clone()));
        self.response.clone()
    }
}

/// A webhook bot on a local-mode server registers over plain `http`, on a
/// port the cloud would refuse, with its `max_connections` (up to 100 000
/// there) on every attempt: the first answer, at start, is a 429 asking
/// for 2 s, and the supervised `register:<bot>` retry waits that out
/// before it registers the bot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_connections_forwarded() {
    let server = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/getMe")))
        .respond_with(me("tgin_bot"))
        .mount(&server)
        .await;
    let (seen, mut registrations) = mpsc::unbounded_channel();
    let set_webhook = format!("/bot{TOKEN}/setWebhook");
    Mock::given(path(set_webhook.as_str()))
        .respond_with(Arrivals {
            seen: seen.clone(),
            response: ResponseTemplate::new(429).set_body_json(json!({
                "ok": false, "error_code": 429,
                "description": "Too Many Requests: retry after 2",
                "parameters": { "retry_after": 2 }
            })),
        })
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path(set_webhook.as_str()))
        .respond_with(Arrivals {
            seen,
            response: ok(json!(true)),
        })
        .mount(&server)
        .await;
    let tgin = start(
        "max_connections_forwarded",
        &config(&format!(
            "{}bots:\n  - name: hooked\n    token: \"{TOKEN}\"\n    upstream: tba\n    ingress: {{ webhook: {{ path: /hook, secret_token: {HOOK_SECRET}, public_url: \"http://tgin.internal:8080\", register: true, max_connections: 100000 }} }}\n    routes: [{{ to: [workers] }}]\nconsumers:\n  workers: {{ pull: {{}} }}\n",
            local_upstream(&server, true)
        )),
    )
    .await;

    let mut arrivals = Vec::new();
    for attempt in 1..=2 {
        let (at, request) = next(&mut registrations).await;
        let body: Value = serde_json::from_slice(&request.body).expect("a JSON setWebhook");
        assert_eq!(body["url"], "http://tgin.internal:8080/hook", "{attempt}");
        assert_eq!(body["max_connections"], 100_000, "{attempt}");
        arrivals.push(at);
    }
    // The start's 429 reaches the retry task, which waits its `retry_after`.
    let gap = arrivals[1] - arrivals[0];
    assert!(gap >= Duration::from_millis(1500), "{gap:?}");
    let status = tgin
        .wait_status("register:hooked stopped", |s| {
            task_state(s, "register:hooked").as_deref() == Some("stopped")
        })
        .await;
    assert_eq!(status.sources[0].state, "running");
    assert_eq!(tgin.readiness().await, 200);
    tgin.stop().await;
}

/// A long poll whose local Bot API server answers 401 (its api_id or
/// api_hash rejected) backs off with the reason in the status, its task
/// still running: it never counts as failed, so tgin keeps running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_upstream_rejection_backs_off() {
    let server = MockServer::start().await;
    let rejected = ResponseTemplate::new(401).set_body_json(json!({
        "ok": false, "error_code": 401, "description": "Unauthorized: invalid api-id/api-hash"
    }));
    mount_bot(&server, TOKEN, rejected, Duration::from_secs(3600)).await;
    let tgin = start(
        "local_upstream_rejection_backs_off",
        &config(&format!(
            "{}bots:\n  - name: poller\n    token: \"{TOKEN}\"\n    upstream: tba\n    ingress: {{ long_poll: {{ timeout: 1s, delete_webhook_on_start: false }} }}\n    routes: [{{ to: [workers] }}]\nconsumers:\n  workers: {{ pull: {{}} }}\n",
            local_upstream(&server, false)
        )),
    )
    .await;

    let status = tgin
        .wait_status("poller backs off", |s| s.sources[0].state == "backoff")
        .await;
    let error = status.sources[0].error.clone().expect("the reason");
    assert!(error.contains("401"), "{error}");
    assert!(error.contains("invalid api-id/api-hash"), "{error}");
    assert!(!error.contains(TOKEN), "{error}");
    assert_eq!(
        task_state(&status, "source:poller").as_deref(),
        Some("running")
    );
    assert_eq!(status.phase, "running");
    assert_eq!(tgin.readiness().await, 200);
    assert!(!tgin.run.is_finished(), "tgin shut down");
    tgin.stop().await;
}

/// R40: once a reload drops `admin.token` while a non-loopback admin
/// listener stays bound (`admin.listen` changes need a restart), every
/// protected endpoint refuses every request; the probes stay open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_auth_fails_closed_after_token_removal() {
    let telegram = telegram().await;
    let loopback_admin = "admin: { listen: [\"127.0.0.1:0\"]";
    let tokened = hooked(&telegram, "");
    let tgin = start(
        "admin_auth_fails_closed_after_token_removal",
        &tokened.replace(loopback_admin, "admin: { listen: [\"0.0.0.0:0\"]"),
    )
    .await;
    assert_eq!(tgin.admin_get("/v1/status", None).await.status, 401);

    tgin.rewrite(&tokened.replace(
        &format!("{loopback_admin}, token: {ADMIN_TOKEN} }}"),
        "admin: { listen: [\"127.0.0.1:0\"] }",
    ));
    let reload = tgin.reload().await;
    assert_eq!(reload.status, 200, "{}", reload.body);
    assert_eq!(reload.json()["restart_required"], json!(["admin.listen"]));

    let unauthorized = json!({ "error": "unauthorized" });
    // Neither without a token nor with the removed one.
    for token in [None, Some(ADMIN_TOKEN)] {
        for path in ["/v1/status", "/v1/config", "/metrics"] {
            let answer = tgin.admin_get(path, token).await;
            assert_eq!(answer.status, 401, "{path} {token:?}");
        }
        for path in ["/v1/consumers/workers/purge", "/v1/reload"] {
            let answer = tgin.admin_post(path, token).await;
            assert_eq!(answer.status, 401, "{path} {token:?}");
            assert_eq!(answer.json(), unauthorized);
        }
    }
    assert_eq!(tgin.admin_get("/livez", None).await.status, 200);
    assert_eq!(tgin.readiness().await, 200);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_pull_consumer_keeps_auth_while_draining() {
    const OLD_SECRET: &str = "old-pull-secret-value";
    let telegram = telegram().await;
    let drain = "reload: { watch: false, drain_timeout: 30s }\n";
    let tgin = start(
        "retired_pull_consumer_keeps_auth_while_draining",
        &config(&format!(
            "{drain}bots:\n{}consumers:\n  old: {{ pull: {{ auth_token: {OLD_SECRET} }} }}\n",
            hook_bot(&telegram, "hooked", TOKEN, "/hook", "old")
        )),
    )
    .await;
    tgin.deliver("/hook", &[1, 2]).await;

    tgin.rewrite(&config(&format!(
        "{drain}bots:\n{}consumers:\n  new: {{ pull: {{}} }}\n",
        hook_bot(&telegram, "hooked", TOKEN, "/hook", "new")
    )));
    let reload = tgin.reload().await;
    assert_eq!(reload.json()["removed"], json!(["old"]));

    let bearer = format!("Bearer {OLD_SECRET}");
    let route = "/c/old/getUpdates?timeout=0";
    assert_eq!(tgin.pull(route, None).await.status, 401);
    assert_eq!(
        tgin.pull(route, Some("Bearer wrong-secret")).await.status,
        401
    );
    let drained = tgin.pull(route, Some(&bearer)).await;
    assert_eq!(drained.status, 200, "{}", drained.body);
    assert_eq!(ids(&drained.body), vec![1, 2]);
    let acked = tgin
        .pull("/c/old/getUpdates?offset=3&timeout=0", Some(&bearer))
        .await;
    assert_eq!(ids(&acked.body), Vec::<i64>::new());
    // Empty, the retired consumer goes, and with it its route.
    let started = Instant::now();
    while tgin.pull(route, Some(&bearer)).await.status != 404 {
        assert!(started.elapsed() < PATIENCE, "the drained consumer stayed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_redacts_task_errors() {
    const PULL_SECRET: &str = "pull-secret-in-an-error";
    let telegram = MockServer::start().await;
    // Telegram's error text carries a configured secret.
    let failing = ResponseTemplate::new(500).set_body_json(json!({
        "ok": false,
        "error_code": 500,
        "description": format!("Internal Server Error: {PULL_SECRET}"),
    }));
    mount_bot(&telegram, TOKEN, failing, Duration::from_secs(3600)).await;
    let tgin = start(
        "status_redacts_task_errors",
        &config(&format!(
            "bots:\n{}consumers:\n  workers: {{ pull: {{ auth_token: {PULL_SECRET} }} }}\n",
            poll_bot(&telegram, "flaky", TOKEN, "workers")
        )),
    )
    .await;

    let started = Instant::now();
    let (source, task) = loop {
        let answer = tgin.admin_get("/v1/status", Some(ADMIN_TOKEN)).await;
        assert!(!answer.body.contains(PULL_SECRET), "{}", answer.body);
        let status: Status = serde_json::from_str(&answer.body).unwrap();
        let source = status.sources[0].clone();
        if source.state == "backoff" {
            let task = status
                .tasks
                .iter()
                .find(|t| t.name == "source:flaky")
                .cloned()
                .expect("the source's task");
            break (source, task);
        }
        assert!(started.elapsed() < PATIENCE, "the source never backed off");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let error = source.error.expect("the source's error");
    assert!(error.contains("getMe failed"), "{error}");
    assert!(error.contains("#####"), "{error}");
    // The source waits out the failure itself: its task keeps running.
    assert_eq!(task.state, "running");
    assert_eq!(task.error, None);
    assert!(source.errors_total >= 1);
    tgin.stop().await;
}

/// Rotating a running bot to a token the Bot API client refuses (here a
/// space in its token file) is rejected like any invalid config: the bot
/// keeps polling with its old token and tgin stays ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_refuses_an_unusable_token() {
    const TEST: &str = "reload_refuses_an_unusable_token";
    let telegram = MockServer::start().await;
    let mut polls = mount_bot(&telegram, TOKEN, me("tgin_bot"), Duration::from_secs(3600)).await;
    let token_file = scratch(TEST).join("token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    // The real file system: the reload reads the token file again.
    let file = write_config(
        TEST,
        &config(&format!(
            "bots:\n{}consumers:\n  workers: {{ pull: {{}} }}\n",
            poll_bot(&telegram, "main", "${file:token}", "workers")
        )),
    );
    let boot = Boot::load(&file, Arc::new(tgin_runtime::config::OsEnv)).expect("valid config");
    let tgin = running(App::start(boot).await.expect("tgin starts"), file);
    next(&mut polls).await;

    std::fs::write(&token_file, "123456:abc def\n").unwrap();
    let reload = tgin.reload().await;
    assert_eq!(reload.status, 422, "{}", reload.body);
    assert!(reload.body.contains("bots[0].token"), "{}", reload.body);
    assert!(!reload.body.contains("abc def"), "{}", reload.body);

    let ready = tgin.admin_get("/readyz", None).await;
    assert_eq!(ready.status, 200, "{}", ready.body);
    let answer = tgin.admin_get("/v1/status", Some(ADMIN_TOKEN)).await;
    let status: Status = serde_json::from_str(&answer.body).unwrap();
    assert_eq!(status.generation, 1);
    assert_eq!(status.sources[0].state, "running", "{}", answer.body);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redactor_keeps_retired_secrets() {
    // Not shaped like Telegram tokens, so only the configured secrets mask
    // them.
    const OLD_TOKEN: &str = "plain-old-token-value";
    const NEW_TOKEN: &str = "plain-new-token-value";
    let telegram = MockServer::start().await;
    mount_bot(
        &telegram,
        OLD_TOKEN,
        me("old_bot"),
        Duration::from_secs(3600),
    )
    .await;
    mount_bot(
        &telegram,
        NEW_TOKEN,
        me("new_bot"),
        Duration::from_secs(3600),
    )
    .await;
    let tgin = start(
        "redactor_keeps_retired_secrets",
        &config(&format!(
            "bots:\n{}consumers:\n  workers: {{ pull: {{}} }}\n",
            hook_bot(&telegram, "gone", OLD_TOKEN, "/hook", "workers")
        )),
    )
    .await;
    let line = format!("{OLD_TOKEN} {HOOK_SECRET} {NEW_TOKEN} {ADMIN_TOKEN}");
    assert!(tgin.redactor.load().redact(&line).contains(NEW_TOKEN));

    tgin.rewrite(&config(&format!(
        "bots:\n{}consumers:\n  workers: {{ pull: {{}} }}\n",
        hook_bot(&telegram, "fresh", NEW_TOKEN, "/fresh", "workers").replace(
            &format!("secret_token: {HOOK_SECRET}"),
            "secret_token: none"
        )
    )));
    let reload = tgin.reload().await;
    assert_eq!(reload.json()["sources_removed"], json!(["gone"]));

    let masked = tgin.redactor.load().redact(&line).into_owned();
    for secret in [OLD_TOKEN, HOOK_SECRET, NEW_TOKEN, ADMIN_TOKEN] {
        assert!(!masked.contains(secret), "{secret} in {masked}");
    }
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_during_drain_refused() {
    let telegram = telegram().await;
    let tgin = start(
        "reload_during_drain_refused",
        &hooked(&telegram, "").replace(QUICK, "runtime: { lame_duck: 30s, shutdown_grace: 30s }"),
    )
    .await;

    tgin.signal(SignalKind::Terminate).await;
    tgin.wait_until_not_ready().await;
    let refused = tgin.reload().await;
    assert_eq!(refused.status, 503);
    assert_eq!(refused.json(), json!({ "error": "draining" }));
    // So is an admin resume, which would end the consumers' drain.
    let resume = tgin
        .admin_post("/v1/consumers/workers/resume", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(resume.status, 409);
    assert_eq!(resume.json(), json!({ "error": "draining" }));
    // The status stays readable during the drain.
    assert_eq!(tgin.status().await.phase, "draining");

    tgin.signal(SignalKind::Interrupt).await;
    let outcome = timeout(PATIENCE, tgin.run).await.unwrap().unwrap();
    assert_eq!(outcome, Outcome::Forced);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_ends_a_drain() {
    let telegram = telegram().await;
    let tgin = start("resume_ends_a_drain", &hooked(&telegram, "")).await;
    // A long poll on the empty queue: its answer time tells whether the
    // consumer drains (500 ms hold) or waits the poll's 3 s timeout out.
    let route = tgin.data_url("/c/workers/getUpdates?timeout=3");
    let poll = || async {
        let started = Instant::now();
        let body = answer(tgin.http.get(&route)).await.body;
        (ids(&body), started.elapsed())
    };

    let drained = tgin
        .admin_post("/v1/consumers/workers/drain", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(drained.status, 204);
    let (got, took) = poll().await;
    assert_eq!(got, Vec::<i64>::new());
    assert!(
        took < Duration::from_millis(1500),
        "drained poll took {took:?}"
    );

    let resumed = tgin
        .admin_post("/v1/consumers/workers/resume", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(resumed.status, 204);
    let (got, took) = poll().await;
    assert_eq!(got, Vec::<i64>::new());
    assert!(took >= Duration::from_secs(3), "resumed poll took {took:?}");
    tgin.deliver("/hook", &[1]).await;
    assert_eq!(
        tgin.poll_ids("/c/workers/getUpdates?timeout=0").await,
        vec![1]
    );
    tgin.stop().await;
}

/// A pull consumer `name` (with `extra` settings) fed by `hooked`.
fn pull_only(telegram: &MockServer, name: &str, extra: &str) -> String {
    config(&format!(
        "bots:\n{}consumers:\n  {name}: {{ pull: {{ {extra} }} }}\n",
        hook_bot(telegram, "hooked", TOKEN, "/hook", name)
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_pull_aliases_drain_until_reused() {
    const OLD_SECRET: &str = "old-alias-secret-value";
    // An alias path a client can poll (`/bot<token>` paths are reserved).
    const ALIAS: &str = "/pollers/one/getUpdates";
    let telegram = telegram().await;
    let tgin = start(
        "retired_pull_aliases_drain_until_reused",
        &pull_only(
            &telegram,
            "old",
            &format!("auth_token: {OLD_SECRET}, aliases: [\"{ALIAS}\"]"),
        ),
    )
    .await;
    tgin.deliver("/hook", &[1, 2]).await;

    tgin.rewrite(&pull_only(&telegram, "new", ""));
    assert_eq!(tgin.reload().await.json()["removed"], json!(["old"]));
    // The retired consumer's alias still serves its drain, with its token.
    let route = format!("{ALIAS}?timeout=0");
    assert_eq!(tgin.pull(&route, None).await.status, 401);
    let drained = tgin
        .pull(&route, Some(&format!("Bearer {OLD_SECRET}")))
        .await;
    assert_eq!(drained.status, 200, "{}", drained.body);
    assert_eq!(ids(&drained.body), vec![1, 2]);

    // A config that gives the path to another consumer wins it.
    tgin.rewrite(&pull_only(
        &telegram,
        "other",
        &format!("aliases: [\"{ALIAS}\"]"),
    ));
    assert_eq!(tgin.reload().await.json()["added"], json!(["other"]));
    tgin.deliver("/hook", &[3]).await;
    assert_eq!(tgin.poll_ids(&route).await, vec![3]);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_reports_restart_fields() {
    let telegram = telegram().await;
    let tgin = start("reload_reports_restart_fields", &hooked(&telegram, "")).await;

    // `log.level` applies live; the rest needs a restart.
    let changed = hooked(&telegram, "log: { level: debug, format: json }\n")
        .replace(
            QUICK,
            "runtime: { lame_duck: 0s, shutdown_grace: 2s, worker_threads: 3 }",
        )
        .replace(
            "server: { listen: [\"127.0.0.1:0\"] }",
            "server: { listen: [\"127.0.0.1:0\"], max_connections: 77, header_read_timeout: 3s, tls: { cert_file: tls/cert.pem, key_file: tls/key.pem } }",
        );
    tgin.rewrite(&changed);
    let reload = tgin.reload().await;
    assert_eq!(reload.status, 200, "{}", reload.body);
    assert_eq!(
        reload.json()["restart_required"],
        json!([
            "log.format",
            "runtime.worker_threads",
            "server.header_read_timeout",
            "server.max_connections",
            "server.tls",
        ])
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_bind_failure_is_a_start_error() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let telegram = telegram().await;
    let yaml = hooked(&telegram, "").replace(
        "admin: { listen: [\"127.0.0.1:0\"]",
        &format!("admin: {{ listen: [\"127.0.0.1:{port}\"]"),
    );
    let file = write_config("admin_bind_failure_is_a_start_error", &yaml);
    match App::start(boot(&file)).await {
        Err(StartError::Admin(_)) => {}
        Err(err) => panic!("another start error: {err}"),
        Ok(_) => panic!("started without its admin plane"),
    }
}

/// The `tgin` binary with `args`, without the admin environment variables.
fn binary(args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_tgin"));
    command
        .args(args)
        .env_remove("TGIN_CONFIG")
        .env_remove("TGIN_ADMIN_URL")
        .env_remove("TGIN_ADMIN_TOKEN_FILE");
    command
}

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    took: Duration,
}

/// Runs `command` off the async runtime.
async fn output(mut command: std::process::Command) -> Output {
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let out = command.output().expect("tgin runs");
        Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            took: started.elapsed(),
        }
    })
    .await
    .expect("the command ran")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_cli_status_table_and_json() {
    let telegram = telegram().await;
    let tgin = start("admin_cli_status_table_and_json", &hooked(&telegram, "")).await;
    tgin.deliver("/hook", &[1]).await;
    let token_file = scratch("admin_cli_status_table_and_json").join("admin-token");
    std::fs::write(&token_file, format!("{ADMIN_TOKEN}\n")).unwrap();
    let url = format!("http://{}", tgin.admin);
    let admin = |args: &[&str]| {
        let mut command = binary(&["admin", "--url", &url]);
        command
            .args(["--token-file", token_file.to_str().unwrap()])
            .args(args);
        command
    };

    let table = output(admin(&["status"])).await;
    assert_eq!(table.code, Some(0), "{}", table.stderr);
    let lines: Vec<&str> = table.stdout.lines().collect();
    assert_eq!(
        lines[0],
        format!("tgin {}, generation 1, running", env!("CARGO_PKG_VERSION"))
    );
    let row = |first: &str| {
        lines
            .iter()
            .find(|line| line.starts_with(first))
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .unwrap_or_else(|| panic!("no {first} row in:\n{}", table.stdout))
    };
    assert_eq!(
        row("SOURCE"),
        [
            "SOURCE", "INGRESS", "STATE", "UPDATES", "ERRORS", "LAST", "ERROR"
        ]
    );
    assert_eq!(row("hooked"), ["hooked", "webhook", "running", "1", "0"]);
    assert_eq!(row("workers"), ["workers", "pull", "1", "0", "0", "no"]);

    let json = output(admin(&["status", "--json"])).await;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    let parsed: Status = serde_json::from_str(&json.stdout).expect("the status JSON");
    assert_eq!(parsed.generation, 1);
    assert_eq!(parsed.consumers[0].name, "workers");

    let consumers = output(admin(&["--json", "consumers"])).await;
    let listed: Value = serde_json::from_str(&consumers.stdout).expect("JSON");
    assert_eq!(listed[0]["name"], "workers");

    let paused = output(admin(&["pause", "workers"])).await;
    assert_eq!(
        (paused.code, paused.stdout.as_str()),
        (Some(0), "paused workers\n")
    );
    assert!(tgin.status().await.consumers[0].paused);
    let purged = output(admin(&["purge", "workers"])).await;
    assert_eq!(purged.stdout, "purged 1 updates from workers\n");
    let unknown = output(admin(&["resume", "nobody"])).await;
    assert_eq!(unknown.code, Some(1));
    assert!(
        unknown.stderr.contains("404 Not Found: unknown consumer"),
        "{}",
        unknown.stderr
    );

    tgin.rewrite(&with_extra_consumer(&telegram, ""));
    let reload = output(admin(&["reload"])).await;
    assert_eq!(reload.code, Some(0), "{}", reload.stderr);
    assert_eq!(
        reload.stdout,
        "reloaded: generation 2\n  consumers added: extra\n"
    );
    tgin.rewrite("version: 1\nbots: [{ name: a }]\n");
    let rejected = output(admin(&["reload"])).await;
    assert_eq!(rejected.code, Some(1));
    assert!(
        rejected
            .stderr
            .contains("422 Unprocessable Entity: the config was rejected")
            && rejected.stderr.contains("error: bots[0]"),
        "{}",
        rejected.stderr
    );

    // The token file can come from the environment; without a token the
    // API refuses.
    let mut from_env = binary(&["admin", "--url", &url, "status", "--json"]);
    from_env.env("TGIN_ADMIN_TOKEN_FILE", &token_file);
    assert_eq!(output(from_env).await.code, Some(0));
    let refused = output(binary(&["admin", "--url", &url, "status"])).await;
    assert_eq!(refused.code, Some(1));
    assert!(
        refused.stderr.contains("401 Unauthorized: unauthorized"),
        "{}",
        refused.stderr
    );
    for out in [
        &table, &json, &paused, &purged, &unknown, &reload, &rejected,
    ] {
        assert!(!out.stdout.contains(ADMIN_TOKEN) && !out.stderr.contains(ADMIN_TOKEN));
    }

    let usage = output(admin(&["pause"])).await;
    assert_eq!(usage.code, Some(64), "{}", usage.stderr);
    let bad_url = output(binary(&["admin", "--url", "ftp://host", "status"])).await;
    assert_eq!(bad_url.code, Some(64), "{}", bad_url.stderr);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_cli_health_exit_codes() {
    let telegram = telegram().await;
    let tgin = start(
        "admin_cli_health_exit_codes",
        &hooked(&telegram, "").replace(QUICK, "runtime: { lame_duck: 30s, shutdown_grace: 30s }"),
    )
    .await;
    let url = format!("http://{}", tgin.admin);

    // No token needed.
    let ready = output(binary(&["admin", "--url", &url, "health"])).await;
    assert_eq!(
        (ready.code, ready.stdout.as_str()),
        (Some(0), "ready\n"),
        "{}",
        ready.stderr
    );
    let mut from_env = binary(&["admin", "health", "--json"]);
    from_env.env("TGIN_ADMIN_URL", &url);
    let json = output(from_env).await;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    assert_eq!(
        serde_json::from_str::<Value>(&json.stdout).unwrap()["ready"],
        true
    );
    // Nor a readable token file: health never reads it.
    let missing = scratch("admin_cli_health_exit_codes").join("no-such-token");
    let mut flag = binary(&["admin", "--url", &url, "--token-file"]);
    flag.arg(&missing).arg("health");
    let mut env = binary(&["admin", "--url", &url, "health"]);
    env.env("TGIN_ADMIN_TOKEN_FILE", &missing);
    for command in [flag, env] {
        let out = output(command).await;
        assert_eq!(out.code, Some(0), "{}", out.stderr);
    }

    tgin.signal(SignalKind::Terminate).await;
    tgin.wait_until_not_ready().await;
    let draining = output(binary(&["admin", "--url", &url, "health"])).await;
    assert_eq!(draining.code, Some(1));
    assert!(
        draining.stdout.contains("phase draining"),
        "{}",
        draining.stdout
    );
    tgin.signal(SignalKind::Interrupt).await;
    let outcome = timeout(PATIENCE, tgin.run).await.unwrap().unwrap();
    assert_eq!(outcome, Outcome::Forced);

    // Nothing listens: a socket bound but not listening, held until the
    // end, so no other test's listener can take its port meanwhile.
    let dead = tokio::net::TcpSocket::new_v4().unwrap();
    dead.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let dead_url = format!("http://{}", dead.local_addr().unwrap());
    let gone = output(binary(&["admin", "--url", &dead_url, "health"])).await;
    assert_eq!(gone.code, Some(1), "{}", gone.stdout);
    assert!(gone.took < Duration::from_secs(4), "took {:?}", gone.took);
    drop(dead);
}

#[test]
fn admin_non_loopback_requires_token() {
    let file = scratch("admin_non_loopback_requires_token").join("tgin.yaml");
    std::fs::write(
        &file,
        "version: 1\nadmin: { listen: [\"0.0.0.0:0\"] }\nbots:\n  - name: a\n    token: \"1:x\"\n",
    )
    .unwrap();
    let out = binary(&["-c", file.to_str().unwrap()])
        .output()
        .expect("tgin runs");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("admin.listen[0]") && stderr.contains("`admin.token` is required"),
        "{stderr}"
    );
}

/// Real SIGHUP to the binary: the reload shows in the generation.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn os_sighup_reloads() {
    let telegram = telegram().await;
    // No watch: the rewrite alone would reload.
    let yaml = |consumers: &str| {
        format!(
            "version: 1\n{QUICK}\nreload: {{ watch: false }}\nserver: {{ listen: [\"127.0.0.1:0\"] }}\nadmin: {{ listen: [\"127.0.0.1:0\"] }}\nbots:\n{}consumers:\n{consumers}",
            hook_bot(&telegram, "hooked", TOKEN, "/hook", "workers")
        )
    };
    let file = write_config("os_sighup_reloads", &yaml("  workers: { pull: {} }\n"));
    let log = file.with_file_name("tgin.log");
    let (mut child, admin) =
        support::spawn_listening(&mut binary(&["-c", file.to_str().unwrap()]), &log, PATIENCE)
            .await;
    let http = client();
    let base = format!("http://{admin}");
    let generation = || {
        let http = http.clone();
        let url = format!("{base}/v1/status");
        async move {
            let status: Status = http.get(url).send().await.ok()?.json().await.ok()?;
            Some(status.generation)
        }
    };
    let wait_for = |want: u64| async move {
        let started = Instant::now();
        while generation().await != Some(want) {
            if started.elapsed() > PATIENCE {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    };
    let kill = |signal: &str| {
        let status = std::process::Command::new("kill")
            .args([signal, &child.id().to_string()])
            .status()
            .expect("kill runs");
        assert!(status.success(), "kill {signal} failed");
    };

    let mut reloaded = wait_for(1).await;
    if reloaded {
        std::fs::write(
            &file,
            yaml("  workers: { pull: {} }\n  extra: { pull: {} }\n"),
        )
        .unwrap();
        kill("-HUP");
        reloaded = wait_for(2).await;
    }
    if reloaded {
        kill("-TERM");
    }
    let started = Instant::now();
    let code = loop {
        if let Some(status) = child.try_wait().expect("waitable") {
            break status.code();
        }
        if !reloaded || started.elapsed() > PATIENCE {
            let _ = child.kill();
            let _ = child.wait();
            let log = std::fs::read_to_string(&log).unwrap_or_default();
            panic!("tgin did not reload or exit in time; its log:\n{log}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(code, Some(0));
}

/// `log.level` follows reloads in the binary, whose logs go to stdout: a
/// debug line appears once the level is `debug` and not after it is back
/// to `info`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_level_follows_reloads() {
    let telegram = telegram().await;
    let yaml = |level: &str| {
        format!(
            "version: 1\n{QUICK}\nlog: {{ level: {level} }}\nreload: {{ watch: false }}\nserver: {{ listen: [\"127.0.0.1:0\"] }}\nadmin: {{ listen: [\"127.0.0.1:0\"] }}\nbots:\n{}consumers:\n  workers: {{ pull: {{}} }}\n",
            hook_bot(&telegram, "hooked", TOKEN, "/hook", "workers")
        )
    };
    let file = write_config("log_level_follows_reloads", &yaml("info"));
    let log = file.with_file_name("tgin.log");
    let (mut child, admin) =
        support::spawn_listening(&mut binary(&["-c", file.to_str().unwrap()]), &log, PATIENCE)
            .await;
    let http = client();
    let admin = format!("http://{admin}");
    let logged = || std::fs::read_to_string(&log).unwrap_or_default();
    // Waits for the `n`th reload's info line, written after its debug one.
    let reloaded = |n: usize| async move {
        let started = Instant::now();
        while logged().matches("config reloaded").count() < n {
            if started.elapsed() > PATIENCE {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    };

    let mut outcome = Err("tgin never became ready".to_owned());
    let started = Instant::now();
    while started.elapsed() < PATIENCE {
        let ready = http.get(format!("{admin}/readyz")).send().await;
        if ready.is_ok_and(|r| r.status() == 200) {
            outcome = Ok(());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut debug_lines = Vec::new();
    for (n, level) in [(1, "debug"), (2, "info")] {
        if outcome.is_err() {
            break;
        }
        std::fs::write(&file, yaml(level)).unwrap();
        let status = http
            .post(format!("{admin}/v1/reload"))
            .send()
            .await
            .map(|r| r.status().as_u16());
        outcome = match status {
            Ok(200) if reloaded(n).await => Ok(()),
            other => Err(format!("reload {n} ({level}): {other:?}")),
        };
        debug_lines.push(logged().matches("reload report").count());
    }
    let _ = child.kill();
    let _ = child.wait();
    if let Err(err) = outcome {
        panic!("{err}; the log:\n{}", logged());
    }
    assert_eq!(debug_lines, [1, 1], "{}", logged());
}

/// A Bot API server's `getUpdates` for one bot, holding `updates` as
/// Telegram holds pending updates: a poll gets those from its `offset` on
/// at once (those below are confirmed and forgotten), else an empty batch,
/// after `idle` unless its `timeout` is 0. With `flood`, a poll with
/// `timeout` 0 that finds none gets one new update instead, numbered from
/// it, after 50 ms: draining never ends. Reports every poll.
struct Pending {
    updates: Arc<Mutex<Vec<i64>>>,
    idle: Duration,
    flood: Option<AtomicI64>,
    polls: mpsc::UnboundedSender<()>,
}

impl Respond for Pending {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let params: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let offset = params["offset"].as_i64().unwrap_or(0);
        let batch = |ids: &[i64]| {
            let updates = ids
                .iter()
                .map(|id| serde_json::from_str(&update(*id, "pending")).expect("an update"));
            ok(Value::Array(updates.collect()))
        };
        let response = {
            let mut updates = self.updates.lock().expect("updates");
            updates.retain(|id| *id >= offset);
            let waits = params["timeout"].as_u64() != Some(0);
            match &self.flood {
                _ if !updates.is_empty() => batch(&updates),
                _ if waits => ok(json!([])).set_delay(self.idle),
                Some(next) => batch(&[next.fetch_add(1, Ordering::SeqCst)])
                    .set_delay(Duration::from_millis(50)),
                None => ok(json!([])),
            }
        };
        // Only now: a reported poll has its answer decided.
        let _ = self.polls.send(());
        response
    }
}

/// Mounts the bot `token` on `server` with [`Pending`] updates, none yet;
/// `getMe` names it `polled_bot`. Returns the updates and the polls.
async fn mount_pending(
    server: &MockServer,
    token: &str,
    idle: Duration,
) -> (Arc<Mutex<Vec<i64>>>, mpsc::UnboundedReceiver<()>) {
    mount_polls(server, token, idle, None).await
}

/// [`mount_pending`], flooding from `flood` when given.
async fn mount_polls(
    server: &MockServer,
    token: &str,
    idle: Duration,
    flood: Option<i64>,
) -> (Arc<Mutex<Vec<i64>>>, mpsc::UnboundedReceiver<()>) {
    Mock::given(path(format!("/bot{token}/getMe")))
        .respond_with(me("polled_bot"))
        .mount(server)
        .await;
    let updates = Arc::default();
    let (polls, rx) = mpsc::unbounded_channel();
    Mock::given(path(format!("/bot{token}/getUpdates")))
        .respond_with(Pending {
            updates: Arc::clone(&updates),
            idle,
            flood: flood.map(AtomicI64::new),
            polls,
        })
        .mount(server)
        .await;
    (updates, rx)
}

/// The `timeout` of each `getUpdates` `server` was asked for with `token`,
/// in order.
async fn poll_timeouts(server: &MockServer, token: &str) -> Vec<u64> {
    let at = format!("/bot{token}/getUpdates");
    let requests = server.received_requests().await.unwrap_or_default();
    requests
        .iter()
        .filter(|r| r.url.path() == at)
        .map(|r| {
            let params: Value = serde_json::from_slice(&r.body).expect("JSON");
            params["timeout"].as_u64().expect("a timeout")
        })
        .collect()
}

/// Waits until `server` gets a `getUpdates` for `token` with `timeout` 1,
/// the `polled` bot's long poll, after the first `after` polls.
async fn wait_long_poll(server: &MockServer, token: &str, after: usize) {
    let started = Instant::now();
    loop {
        let timeouts = poll_timeouts(server, token).await;
        if timeouts.iter().skip(after).any(|t| *t == 1) {
            return;
        }
        assert!(
            started.elapsed() < PATIENCE,
            "no long poll after poll {after}: {timeouts:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Pulls `workers` until `id` comes, acknowledging all before it.
async fn pull_until(tgin: &Tgin, id: i64) {
    let started = Instant::now();
    let mut offset = 0;
    loop {
        let polled = format!("/c/workers/getUpdates?offset={offset}&timeout=1");
        let ids = tgin.poll_ids(&polled).await;
        if ids.contains(&id) {
            return;
        }
        offset = ids.last().map_or(offset, |last| last + 1);
        assert!(started.elapsed() < PATIENCE, "update {id} never came");
    }
}

/// Mounts the bot `token`'s `getWebhookInfo` on `server`, reporting
/// `pending` updates.
async fn mount_webhook_info(server: &MockServer, token: &str, pending: u64) {
    Mock::given(path(format!("/bot{token}/getWebhookInfo")))
        .respond_with(ok(json!({
            "url": "", "has_custom_certificate": false, "pending_update_count": pending,
        })))
        .mount(server)
        .await;
}

/// Mounts what moving the bot `token` calls besides `deleteWebhook`:
/// `getWebhookInfo`, reporting `pending` updates, and `logOut` and
/// `close`, whose descriptions echo the token.
async fn mount_moves(server: &MockServer, token: &str, pending: u64) {
    mount_webhook_info(server, token, pending).await;
    for (m, done) in [("logOut", "logged out"), ("close", "closed")] {
        Mock::given(path(format!("/bot{token}/{m}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": true, "description": format!("bot {token} {done}"),
            })))
            .mount(server)
            .await;
    }
}

/// The methods `server` was asked for with `token`, in order.
async fn calls(server: &MockServer, token: &str) -> Vec<String> {
    let prefix = format!("/bot{token}/");
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.url.path().strip_prefix(&prefix).map(str::to_owned))
        .collect()
}

/// How often `server` was asked for `method` with `token`.
async fn count_calls(server: &MockServer, token: &str, method: &str) -> usize {
    calls(server, token)
        .await
        .iter()
        .filter(|m| *m == method)
        .count()
}

/// The state `/v1/status` shows for the source `name`.
async fn source_state(tgin: &Tgin, name: &str) -> String {
    let status = tgin.status().await;
    status
        .sources
        .iter()
        .find(|s| s.name == name)
        .map(|s| s.state.clone())
        .unwrap_or_else(|| panic!("no source {name} in {status:?}"))
}

/// The long-poll bot `polled` ([`TOKEN`], 1 s polls) at `upstream` (an
/// `api_url: …` or `upstream: …` line), feeding `workers`.
fn polled(upstream: &str, timeout: &str) -> String {
    format!(
        "  - name: polled\n    token: \"{TOKEN}\"\n    {upstream}\n    ingress: {{ long_poll: {{ timeout: {timeout}, delete_webhook_on_start: false }} }}\n    routes: [{{ to: [workers] }}]\n"
    )
}

/// `logout`: the pending updates are fetched from the server and delivered
/// before `logOut`, which is called exactly once, after the source stopped;
/// the description comes back masked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bot_logout_drains_stops_and_calls_upstream_once() {
    let telegram = MockServer::start().await;
    let (pending, mut polls) = mount_pending(&telegram, TOKEN, Duration::from_secs(3600)).await;
    mount_moves(&telegram, TOKEN, 0).await;
    let api = format!("api_url: {}", telegram.uri());
    let tgin = start(
        "bot_logout_drains_stops_and_calls_upstream_once",
        &config(&format!(
            "bots:\n{}consumers:\n  workers: {{ pull: {{}} }}\n",
            polled(&api, "1s")
        )),
    )
    .await;
    // The source's long poll waits on the server, which then gets two
    // updates: only polling again fetches them.
    next(&mut polls).await;
    pending.lock().expect("updates").extend([1, 2]);

    let consume = async {
        let started = Instant::now();
        let mut got = Vec::new();
        while got.len() < 2 {
            assert!(started.elapsed() < PATIENCE, "only {got:?} delivered");
            let offset = got.last().map_or(0, |id| id + 1);
            let polled = format!("/c/workers/getUpdates?offset={offset}&timeout=1");
            got.extend(tgin.poll_ids(&polled).await);
        }
        // Acknowledged: tgin holds nothing of the bot any more.
        let acked = tgin.poll_ids("/c/workers/getUpdates?offset=3&timeout=0");
        assert_eq!(acked.await, Vec::<i64>::new());
        got
    };
    let logout = tgin.admin_post("/v1/bots/polled/logout", Some(ADMIN_TOKEN));
    let (answer, delivered) = timeout(PATIENCE, async { tokio::join!(logout, consume) })
        .await
        .expect("the logout ends in time");
    assert_eq!(delivered, [1, 2]);
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(
        answer.json(),
        json!({
            "bot": "polled",
            "left": 0,
            "upstream": "api_url",
            "result": "bot [redacted] logged out",
        })
    );

    let calls = calls(&telegram, TOKEN).await;
    assert_eq!(
        calls.iter().filter(|m| *m == "logOut").count(),
        1,
        "{calls:?}"
    );
    assert_eq!(
        calls.last().map(String::as_str),
        Some("logOut"),
        "{calls:?}"
    );
    // The last poll before it confirmed both updates and found none left.
    let at = format!("/bot{TOKEN}/getUpdates");
    let last_poll: Value = telegram
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .rfind(|r| r.url.path() == at)
        .map(|r| serde_json::from_slice(&r.body).expect("JSON"))
        .expect("polled");
    assert_eq!(
        (&last_poll["offset"], &last_poll["timeout"]),
        (&json!(3), &json!(0))
    );
    assert_eq!(source_state(&tgin, "polled").await, "stopped");
    assert!(!LOGS.text().contains(TOKEN), "the token reached a log line");
    tgin.stop().await;
}

/// `close` on a webhook bot: `deleteWebhook`, then `close`, each once.
/// tgin's own queue waits out the timeout, which tgin reports as `left`;
/// the parked bot's route then answers 503, and the update tgin holds is
/// still delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bot_close_deletes_webhook_then_closes() {
    let telegram = telegram().await;
    mount_moves(&telegram, TOKEN, 0).await;
    let tgin = start(
        "bot_close_deletes_webhook_then_closes",
        &hooked(&telegram, ""),
    )
    .await;
    tgin.deliver("/hook", &[1]).await;

    let answer = tgin
        .admin_post("/v1/bots/hooked/close?timeout=500ms", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(
        answer.json(),
        json!({
            "bot": "hooked",
            "left": 1,
            "upstream": "api_url",
            "result": "bot [redacted] closed",
        })
    );
    let calls = calls(&telegram, TOKEN).await;
    let moves: Vec<&str> = calls
        .iter()
        .map(String::as_str)
        .filter(|m| *m != "getMe")
        .collect();
    let (drain, rest) = moves.split_at(moves.len().saturating_sub(2));
    assert!(
        !drain.is_empty() && drain.iter().all(|m| *m == "getWebhookInfo"),
        "{calls:?}"
    );
    assert_eq!(rest, ["deleteWebhook", "close"], "{calls:?}");

    assert_eq!(tgin.post_update("/hook", update(2, "late")).await, 503);
    assert_eq!(source_state(&tgin, "hooked").await, "stopped");
    assert_eq!(tgin.poll_ids("/c/workers/getUpdates").await, vec![1]);
    tgin.stop().await;
}

/// A parked bot stays parked across reloads that leave its upstream
/// alone, an unchanged config or one that would restart its source, each
/// with a warning; meanwhile it never calls its server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_bot_not_restarted_by_unchanged_reload() {
    let quick_polls = Duration::from_millis(50);
    let telegram = MockServer::start().await;
    let (_pending, mut polls) = mount_pending(&telegram, TOKEN, quick_polls).await;
    mount_moves(&telegram, TOKEN, 0).await;
    // Its polls are the test's clock.
    let mut clock = mount_bot(&telegram, OTHER_TOKEN, me("clock_bot"), quick_polls).await;
    let api = format!("api_url: {}", telegram.uri());
    let yaml = |timeout: &str| {
        config(&format!(
            "bots:\n{}{}consumers:\n  workers: {{ pull: {{}} }}\n  clocked: {{ pull: {{}} }}\n",
            polled(&api, timeout),
            poll_bot(&telegram, "clock", OTHER_TOKEN, "clocked"),
        ))
    };
    let tgin = start("parked_bot_not_restarted_by_unchanged_reload", &yaml("1s")).await;
    next(&mut polls).await;
    let answer = tgin
        .admin_post("/v1/bots/polled/logout", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    let parked_at = calls(&telegram, TOKEN).await.len();

    let unchanged = tgin.reload().await;
    assert_eq!(unchanged.status, 200, "{}", unchanged.body);
    tgin.rewrite(&yaml("2s"));
    let restarting = tgin.reload().await;
    assert_eq!(restarting.status, 200, "{}", restarting.body);
    assert_eq!(restarting.json()["sources_restarted"], json!(["polled"]));

    while clock.try_recv().is_ok() {}
    for _ in 0..3 {
        next(&mut clock).await;
    }
    assert_eq!(
        calls(&telegram, TOKEN).await.len(),
        parked_at,
        "the parked bot called its server: {:?}",
        calls(&telegram, TOKEN).await
    );
    assert_eq!(source_state(&tgin, "polled").await, "stopped");
    let logs = LOGS.text();
    let warnings = logs
        .lines()
        .filter(|l| l.contains(" WARN ") && l.contains("bot=polled") && l.contains("stays parked"))
        .count();
    assert_eq!(warnings, 2, "{logs}");
    tgin.stop().await;
}

/// A reload that moves a parked bot to another upstream starts it there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_upstream_unparks() {
    let quick_polls = Duration::from_millis(50);
    let telegram = MockServer::start().await;
    let (_pending, mut polls) = mount_pending(&telegram, TOKEN, quick_polls).await;
    mount_moves(&telegram, TOKEN, 0).await;
    let local = MockServer::start().await;
    let mut local_polls = mount_bot(&local, TOKEN, me("polled_bot"), quick_polls).await;
    let consumers = "consumers:\n  workers: { pull: {} }\n";
    let api = format!("api_url: {}", telegram.uri());
    let tgin = start(
        "changed_upstream_unparks",
        &config(&format!("bots:\n{}{consumers}", polled(&api, "1s"))),
    )
    .await;
    next(&mut polls).await;
    let answer = tgin
        .admin_post("/v1/bots/polled/logout", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(answer.status, 200, "{}", answer.body);

    tgin.rewrite(&config(&format!(
        "{}bots:\n{}{consumers}",
        local_upstream(&local, false),
        polled("upstream: tba", "1s")
    )));
    let reload = tgin.reload().await;
    assert_eq!(reload.status, 200, "{}", reload.body);
    assert_eq!(reload.json()["upstream_changed"], json!(["polled"]));
    next(&mut local_polls).await;
    tgin.wait_status("polled running", |s| {
        task_state(s, "source:polled").as_deref() == Some("running")
    })
    .await;
    assert_eq!(count_calls(&local, TOKEN, "getMe").await, 1);
    // No longer parked: it moves again, now off the new server.
    mount_moves(&local, TOKEN, 0).await;
    let again = tgin
        .admin_post("/v1/bots/polled/logout", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.json()["upstream"], "tba");
    assert_eq!(count_calls(&local, TOKEN, "logOut").await, 1);
    assert_eq!(count_calls(&telegram, TOKEN, "logOut").await, 1);
    tgin.stop().await;
}

/// `tgin admin bot logout|close` does nothing without `--yes`: it says what
/// would happen and exits 64. With `--yes` it moves the bot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_requires_yes() {
    let telegram = telegram().await;
    mount_moves(&telegram, TOKEN, 0).await;
    let tgin = start("cli_requires_yes", &hooked(&telegram, "")).await;
    let token_file = scratch("cli_requires_yes").join("admin-token");
    std::fs::write(&token_file, format!("{ADMIN_TOKEN}\n")).unwrap();
    let url = format!("http://{}", tgin.admin);
    let admin = |args: &[&str]| {
        let mut command = binary(&["admin", "--url", &url]);
        command
            .args(["--token-file", token_file.to_str().unwrap()])
            .args(args);
        command
    };

    let dry = output(admin(&["bot", "logout", "hooked"])).await;
    assert_eq!(dry.code, Some(64), "{}", dry.stderr);
    assert!(
        dry.stderr.contains("hooked")
            && dry.stderr.contains("logOut")
            && dry.stderr.contains("--yes"),
        "{}",
        dry.stderr
    );
    let dry_close = output(admin(&["bot", "close", "hooked", "--timeout", "5s"])).await;
    assert_eq!(dry_close.code, Some(64), "{}", dry_close.stderr);
    assert!(
        dry_close.stderr.contains("deleteWebhook") && dry_close.stderr.contains("5s"),
        "{}",
        dry_close.stderr
    );
    let bad = output(admin(&[
        "bot",
        "logout",
        "hooked",
        "--yes",
        "--timeout",
        "5M",
    ]))
    .await;
    assert_eq!(bad.code, Some(64), "{}", bad.stderr);
    assert_eq!(count_calls(&telegram, TOKEN, "logOut").await, 0);
    assert_eq!(source_state(&tgin, "hooked").await, "running");

    let done = output(admin(&[
        "bot",
        "logout",
        "hooked",
        "--yes",
        "--timeout",
        "5s",
    ]))
    .await;
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert!(
        done.stdout.contains("hooked") && done.stdout.contains("logged out"),
        "{}",
        done.stdout
    );
    assert_eq!(count_calls(&telegram, TOKEN, "logOut").await, 1);
    let again = output(admin(&["--json", "bot", "logout", "hooked", "--yes"])).await;
    assert_eq!(again.code, Some(1), "{}", again.stderr);
    assert!(
        again.stderr.contains("409 Conflict: parked"),
        "{}",
        again.stderr
    );
    for out in [&dry, &dry_close, &bad, &done, &again] {
        assert!(!out.stdout.contains(TOKEN) && !out.stderr.contains(TOKEN));
        assert!(!out.stdout.contains(ADMIN_TOKEN) && !out.stderr.contains(ADMIN_TOKEN));
    }
    tgin.stop().await;
}

/// Moves need the admin token, name a configured bot and a valid timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_bot_404() {
    let telegram = telegram().await;
    mount_moves(&telegram, TOKEN, 0).await;
    let tgin = start("unknown_bot_404", &hooked(&telegram, "")).await;
    assert_eq!(
        tgin.admin_post("/v1/bots/hooked/logout", None).await.status,
        401
    );
    for op in ["logout", "close"] {
        let answer = tgin
            .admin_post(&format!("/v1/bots/nobody/{op}"), Some(ADMIN_TOKEN))
            .await;
        assert_eq!(answer.status, 404, "{}", answer.body);
        assert_eq!(answer.json(), json!({ "error": "unknown bot" }));
    }
    for bad in ["5", "5M", "soon"] {
        let answer = tgin
            .admin_post(
                &format!("/v1/bots/hooked/logout?timeout={bad}"),
                Some(ADMIN_TOKEN),
            )
            .await;
        assert_eq!(answer.status, 400, "{bad}: {}", answer.body);
        assert_eq!(answer.json(), json!({ "error": "invalid timeout" }));
    }
    for method in ["getWebhookInfo", "logOut", "close"] {
        assert_eq!(count_calls(&telegram, TOKEN, method).await, 0, "{method}");
    }
    assert_eq!(source_state(&tgin, "hooked").await, "running");
    tgin.stop().await;
}

/// No move during the shutdown: the bot is never logged out then.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_during_shutdown_409() {
    let telegram = telegram().await;
    mount_moves(&telegram, TOKEN, 0).await;
    let tgin = start(
        "logout_during_shutdown_409",
        &hooked(&telegram, "").replace(QUICK, "runtime: { lame_duck: 30s, shutdown_grace: 30s }"),
    )
    .await;
    tgin.signal(SignalKind::Terminate).await;
    tgin.wait_until_not_ready().await;
    for op in ["logout", "close"] {
        let answer = tgin
            .admin_post(&format!("/v1/bots/hooked/{op}"), Some(ADMIN_TOKEN))
            .await;
        assert_eq!(answer.status, 409, "{}", answer.body);
        assert_eq!(answer.json(), json!({ "error": "draining" }));
    }
    for method in ["getWebhookInfo", "deleteWebhook", "logOut", "close"] {
        assert_eq!(count_calls(&telegram, TOKEN, method).await, 0, "{method}");
    }
    tgin.signal(SignalKind::Interrupt).await;
    let outcome = timeout(PATIENCE, tgin.run).await.unwrap().unwrap();
    assert_eq!(outcome, Outcome::Forced);
}

/// While the server still holds updates at the deadline, the move fails
/// with 504 and changes nothing: the bot's ingest never stopped and its
/// server was not asked to log it out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_504_when_upstream_not_drained() {
    let telegram = telegram().await;
    mount_moves(&telegram, TOKEN, 3).await;
    let tgin = start(
        "logout_504_when_upstream_not_drained",
        &hooked(&telegram, ""),
    )
    .await;
    let answer = tgin
        .admin_post("/v1/bots/hooked/logout?timeout=300ms", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(answer.status, 504, "{}", answer.body);
    assert_eq!(answer.json(), json!({ "error": "upstream not drained" }));
    assert!(count_calls(&telegram, TOKEN, "getWebhookInfo").await >= 1);
    assert_eq!(count_calls(&telegram, TOKEN, "logOut").await, 0);
    assert_eq!(source_state(&tgin, "hooked").await, "running");
    tgin.deliver("/hook", &[1]).await;
    assert_eq!(tgin.poll_ids("/c/workers/getUpdates").await, vec![1]);
    tgin.stop().await;
}

/// A parked bot is not moved again: 409 `parked`, nothing called.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_of_parked_bot_409() {
    let telegram = telegram().await;
    mount_moves(&telegram, TOKEN, 0).await;
    let tgin = start("logout_of_parked_bot_409", &hooked(&telegram, "")).await;
    let first = tgin
        .admin_post("/v1/bots/hooked/logout", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(first.status, 200, "{}", first.body);
    let before = calls(&telegram, TOKEN).await;
    for op in ["logout", "close"] {
        let answer = tgin
            .admin_post(&format!("/v1/bots/hooked/{op}"), Some(ADMIN_TOKEN))
            .await;
        assert_eq!(answer.status, 409, "{}", answer.body);
        assert_eq!(answer.json(), json!({ "error": "parked" }));
    }
    assert_eq!(calls(&telegram, TOKEN).await, before);
    tgin.stop().await;
}

/// The server refusing the call answers 502 with its description, masked;
/// the bot then runs on, so the move can be tried again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bot_logout_upstream_error_502() {
    let telegram = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/logOut")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ok": false, "error_code": 400, "description": format!("Bad Request: not now, {TOKEN}"),
        })))
        .mount(&telegram)
        .await;
    mount_bot(&telegram, TOKEN, me("tgin_bot"), Duration::from_secs(3600)).await;
    mount_webhook_info(&telegram, TOKEN, 0).await;
    let tgin = start("bot_logout_upstream_error_502", &hooked(&telegram, "")).await;
    for attempt in 1..=2 {
        let answer = tgin
            .admin_post("/v1/bots/hooked/logout?timeout=500ms", Some(ADMIN_TOKEN))
            .await;
        assert_eq!(answer.status, 502, "{}", answer.body);
        assert_eq!(
            answer.json(),
            json!({ "error": "Bad Request: not now, [redacted]" })
        );
        assert_eq!(count_calls(&telegram, TOKEN, "logOut").await, attempt);
        assert_eq!(source_state(&tgin, "hooked").await, "running");
        tgin.deliver("/hook", &[attempt.try_into().unwrap()]).await;
    }
    assert_eq!(tgin.poll_ids("/c/workers/getUpdates").await, vec![1, 2]);
    assert!(!LOGS.text().contains(TOKEN), "the token reached a log line");
    tgin.stop().await;
}

/// Parking the only long poll that has not failed does not count as every
/// source failing: tgin keeps running, so a reload can start the bot on
/// its new upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_bot_keeps_tgin_running() {
    let telegram = MockServer::start().await;
    let rejected = ResponseTemplate::new(401).set_body_json(json!({
        "ok": false, "error_code": 401, "description": "Unauthorized"
    }));
    mount_bot(&telegram, OTHER_TOKEN, rejected, Duration::from_secs(3600)).await;
    let (_pending, mut polls) = mount_pending(&telegram, TOKEN, Duration::from_millis(50)).await;
    mount_moves(&telegram, TOKEN, 0).await;
    let api = format!("api_url: {}", telegram.uri());
    let tgin = start(
        "parked_bot_keeps_tgin_running",
        &config(&format!(
            "bots:\n{}{}consumers:\n  workers: {{ pull: {{}} }}\n  dead: {{ pull: {{}} }}\n",
            polled(&api, "1s"),
            poll_bot(&telegram, "dead", OTHER_TOKEN, "dead"),
        )),
    )
    .await;
    tgin.wait_status("source:dead failed", |s| {
        task_state(s, "source:dead").as_deref() == Some("failed")
    })
    .await;
    next(&mut polls).await;
    let answer = tgin
        .admin_post("/v1/bots/polled/logout", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    // Past two of the app's once-a-second checks for every source failing.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(!tgin.run.is_finished(), "tgin shut down");
    assert_eq!(tgin.status().await.phase, "running");
    tgin.stop().await;
}

/// The long-poll bot `polled` of [`polled`], renamed `name`.
fn polled_as(name: &str, upstream: &str, timeout: &str) -> String {
    polled(upstream, timeout).replace("name: polled", &format!("name: {name}"))
}

/// A parked bot whose ingress a reload removes stays parked: when a later
/// reload gives it ingress again on the same upstream, it is not started,
/// nothing is called on its server, its webhook route answers 503 and each
/// reload warns that it stays parked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_bot_stays_parked_when_its_ingress_comes_back() {
    let quick_polls = Duration::from_millis(50);
    let telegram = MockServer::start().await;
    let (_pending, mut polls) = mount_pending(&telegram, TOKEN, quick_polls).await;
    mount_moves(&telegram, TOKEN, 0).await;
    let mut clock = mount_bot(&telegram, OTHER_TOKEN, me("clock_bot"), quick_polls).await;
    let api = format!("api_url: {}", telegram.uri());
    let yaml = |kept: &str| {
        config(&format!(
            "bots:\n{kept}{}consumers:\n  workers: {{ pull: {{}} }}\n  clocked: {{ pull: {{}} }}\n",
            poll_bot(&telegram, "clock", OTHER_TOKEN, "clocked"),
        ))
    };
    let tgin = start(
        "parked_bot_stays_parked_when_its_ingress_comes_back",
        &yaml(&polled_as("kept", &api, "1s")),
    )
    .await;
    next(&mut polls).await;
    let answer = tgin
        .admin_post("/v1/bots/kept/logout", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    let parked_at = calls(&telegram, TOKEN).await.len();

    // No ingress, then a webhook on the same upstream.
    let bare = format!(
        "  - name: kept\n    token: \"{TOKEN}\"\n    {api}\n    routes: [{{ to: [workers] }}]\n"
    );
    tgin.rewrite(&yaml(&bare));
    let removed = tgin.reload().await;
    assert_eq!(removed.status, 200, "{}", removed.body);
    assert_eq!(removed.json()["sources_restarted"], json!(["kept"]));
    tgin.rewrite(&yaml(&hook_bot(
        &telegram, "kept", TOKEN, "/kept", "workers",
    )));
    let restored = tgin.reload().await;
    assert_eq!(restored.status, 200, "{}", restored.body);
    assert_eq!(restored.json()["sources_restarted"], json!(["kept"]));
    assert_eq!(restored.json()["upstream_changed"], json!([]));

    while clock.try_recv().is_ok() {}
    for _ in 0..3 {
        next(&mut clock).await;
    }
    assert_eq!(
        calls(&telegram, TOKEN).await.len(),
        parked_at,
        "the parked bot called its server: {:?}",
        calls(&telegram, TOKEN).await
    );
    assert_eq!(tgin.post_update("/kept", update(1, "late")).await, 503);
    assert_eq!(source_state(&tgin, "kept").await, "stopped");
    let again = tgin
        .admin_post("/v1/bots/kept/logout", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(again.json(), json!({ "error": "parked" }));
    let logs = LOGS.text();
    let warnings = logs
        .lines()
        .filter(|l| l.contains(" WARN ") && l.contains("bot=kept") && l.contains("stays parked"))
        .count();
    assert_eq!(warnings, 2, "{logs}");
    tgin.stop().await;
}

/// A long poll whose server never runs dry: the move fails with 504 and
/// changes nothing, so the source long-polls with its own timeout again
/// and delivers what comes next; its server was not asked to log it out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_poll_504_resumes_long_polling() {
    let telegram = MockServer::start().await;
    let (pending, mut polls) =
        mount_polls(&telegram, TOKEN, Duration::from_millis(200), Some(1000)).await;
    mount_moves(&telegram, TOKEN, 0).await;
    let api = format!("api_url: {}", telegram.uri());
    let tgin = start(
        "long_poll_504_resumes_long_polling",
        &config(&format!(
            "bots:\n{}consumers:\n  workers: {{ pull: {{}} }}\n",
            polled_as("flooded", &api, "1s")
        )),
    )
    .await;
    next(&mut polls).await;
    let answer = tgin
        .admin_post("/v1/bots/flooded/logout?timeout=500ms", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(answer.status, 504, "{}", answer.body);
    assert_eq!(answer.json(), json!({ "error": "upstream not drained" }));
    let drained = poll_timeouts(&telegram, TOKEN).await;
    assert!(drained.contains(&0), "never drained: {drained:?}");
    assert_eq!(count_calls(&telegram, TOKEN, "logOut").await, 0);

    wait_long_poll(&telegram, TOKEN, drained.len()).await;
    assert_eq!(source_state(&tgin, "flooded").await, "running");
    pending.lock().expect("updates").push(100_000);
    pull_until(&tgin, 100_000).await;
    assert_eq!(count_calls(&telegram, TOKEN, "logOut").await, 0);
    tgin.stop().await;
}

/// A long poll whose server refuses `logOut`: 502, and the bot is started
/// again on that server, long-polling and delivering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_poll_502_restarts_the_source() {
    let telegram = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/logOut")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ok": false, "error_code": 400, "description": format!("Bad Request: not now, {TOKEN}"),
        })))
        .mount(&telegram)
        .await;
    let (pending, mut polls) = mount_pending(&telegram, TOKEN, Duration::from_millis(50)).await;
    let api = format!("api_url: {}", telegram.uri());
    let tgin = start(
        "long_poll_502_restarts_the_source",
        &config(&format!(
            "bots:\n{}consumers:\n  workers: {{ pull: {{}} }}\n",
            polled_as("refused", &api, "1s")
        )),
    )
    .await;
    next(&mut polls).await;
    let answer = tgin
        .admin_post("/v1/bots/refused/logout", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(answer.status, 502, "{}", answer.body);
    assert_eq!(
        answer.json(),
        json!({ "error": "Bad Request: not now, [redacted]" })
    );
    let refused_at = poll_timeouts(&telegram, TOKEN).await.len();

    wait_long_poll(&telegram, TOKEN, refused_at).await;
    assert_eq!(count_calls(&telegram, TOKEN, "getMe").await, 2);
    tgin.wait_status("source:refused running", |s| {
        task_state(s, "source:refused").as_deref() == Some("running")
    })
    .await;
    pending.lock().expect("updates").push(5);
    pull_until(&tgin, 5).await;
    tgin.stop().await;
}

/// `getWebhookInfo` reporting the updates `pending` holds, read at each
/// call: a webhook bot's move drains until the test sets it to 0.
struct GatedInfo {
    pending: Arc<AtomicU64>,
}

impl Respond for GatedInfo {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        ok(json!({
            "url": "", "has_custom_certificate": false,
            "pending_update_count": self.pending.load(Ordering::SeqCst),
        }))
    }
}

/// How many captured log lines contain every one of `parts`.
fn logged(parts: &[&str]) -> usize {
    let logs = LOGS.text();
    logs.lines()
        .filter(|l| parts.iter().all(|p| l.contains(p)))
        .count()
}

/// Waits until more than `before` captured log lines contain every one of
/// `parts`.
async fn wait_logged(parts: &[&str], before: usize) {
    let started = Instant::now();
    while logged(parts) <= before {
        assert!(started.elapsed() < PATIENCE, "never logged: {parts:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// R17: a move asked for before a reload runs before it. While one move
/// runs (its server reports pending updates until the test clears them)
/// and another waits, a SIGHUP reload moving the waiting bot to another
/// upstream waits too: the waiting bot is logged out of the server it
/// was on, and only then started on the new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_move_runs_before_a_later_reload() {
    let telegram = MockServer::start().await;
    mount_bot(
        &telegram,
        TOKEN,
        me("runner_bot"),
        Duration::from_secs(3600),
    )
    .await;
    let gate = Arc::new(AtomicU64::new(1));
    Mock::given(path(format!("/bot{TOKEN}/getWebhookInfo")))
        .respond_with(GatedInfo {
            pending: Arc::clone(&gate),
        })
        .mount(&telegram)
        .await;
    mount_moves(&telegram, TOKEN, 0).await;
    mount_bot(
        &telegram,
        OTHER_TOKEN,
        me("waiter_bot"),
        Duration::from_secs(3600),
    )
    .await;
    mount_moves(&telegram, OTHER_TOKEN, 0).await;
    let local = MockServer::start().await;
    mount_bot(
        &local,
        OTHER_TOKEN,
        me("waiter_bot"),
        Duration::from_secs(3600),
    )
    .await;
    mount_moves(&local, OTHER_TOKEN, 0).await;
    let runner = hook_bot(&telegram, "runner", TOKEN, "/runner", "workers");
    let waiter = hook_bot(&telegram, "waiter", OTHER_TOKEN, "/waiter", "waiters");
    let consumers = "consumers:\n  workers: { pull: {} }\n  waiters: { pull: {} }\n";
    let tgin = start(
        "queued_move_runs_before_a_later_reload",
        &config(&format!("bots:\n{runner}{waiter}{consumers}")),
    )
    .await;
    let spawn_move = |bot: &str| {
        let request = tgin
            .http
            .post(tgin.admin_url(&format!("/v1/bots/{bot}/logout?timeout=30s")))
            .bearer_auth(ADMIN_TOKEN);
        tokio::spawn(async move { answer(request).await })
    };

    let running = spawn_move("runner");
    let started = Instant::now();
    while count_calls(&telegram, TOKEN, "getWebhookInfo").await == 0 {
        assert!(started.elapsed() < PATIENCE, "the first move never ran");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let waiting = spawn_move("waiter");
    wait_logged(&["bot move queued", "bot=waiter"], 0).await;
    let moved = waiter.replace(&format!("api_url: {}", telegram.uri()), "upstream: tba");
    tgin.rewrite(&config(&format!(
        "{}bots:\n{runner}{moved}{consumers}",
        local_upstream(&local, false)
    )));
    // The app takes the SIGHUP off its signal channel and queues the
    // reload in the same step; the log line says it was queued, not
    // dropped (another test's SIGHUP line could only come on top).
    let sighup = ["SIGHUP: reloading the config"];
    let before = logged(&sighup);
    tgin.signal(SignalKind::Hangup).await;
    let started = Instant::now();
    while tgin.signals.capacity() < tgin.signals.max_capacity() {
        assert!(started.elapsed() < PATIENCE, "the SIGHUP was never taken");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    wait_logged(&sighup, before).await;
    // Both wait now; the running move may end.
    gate.store(0, Ordering::SeqCst);

    let ran = timeout(PATIENCE, running)
        .await
        .expect("in time")
        .expect("ran");
    assert_eq!(ran.status, 200, "{}", ran.body);
    let waited = timeout(PATIENCE, waiting)
        .await
        .expect("in time")
        .expect("ran");
    assert_eq!(waited.status, 200, "{}", waited.body);
    assert_eq!(waited.json()["upstream"], "api_url");
    let status = tgin.wait_for_generation(2).await;
    assert_eq!(count_calls(&telegram, OTHER_TOKEN, "logOut").await, 1);
    assert_eq!(count_calls(&local, OTHER_TOKEN, "logOut").await, 0);
    // Unparked by the reload, it runs on the new upstream.
    assert_eq!(count_calls(&local, OTHER_TOKEN, "getMe").await, 1);
    let waiter_state = status.sources.iter().find(|s| s.name == "waiter");
    assert_eq!(waiter_state.map(|s| s.state.as_str()), Some("running"));
    tgin.stop().await;
}

/// When the server cannot be asked whether it still holds updates, the
/// 504's warning says why (masked), not that updates were held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_504_names_the_drain_failure() {
    let telegram = MockServer::start().await;
    Mock::given(path(format!("/bot{TOKEN}/getWebhookInfo")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "ok": false, "error_code": 401, "description": format!("Unauthorized: {TOKEN} revoked"),
        })))
        .mount(&telegram)
        .await;
    mount_bot(&telegram, TOKEN, me("tgin_bot"), Duration::from_secs(3600)).await;
    let tgin = start(
        "logout_504_names_the_drain_failure",
        &config(&format!(
            "bots:\n{}consumers:\n  workers: {{ pull: {{}} }}\n",
            hook_bot(&telegram, "unasked", TOKEN, "/hook", "workers")
        )),
    )
    .await;
    let answer = tgin
        .admin_post("/v1/bots/unasked/logout?timeout=300ms", Some(ADMIN_TOKEN))
        .await;
    assert_eq!(answer.status, 504, "{}", answer.body);
    assert_eq!(answer.json(), json!({ "error": "upstream not drained" }));
    let logs = LOGS.text();
    let warning = logs
        .lines()
        .find(|l| l.contains(" WARN ") && l.contains("bot=unasked") && l.contains("not moved"))
        .unwrap_or_else(|| panic!("no warning in\n{logs}"));
    assert!(
        warning.contains("could not be asked")
            && warning.contains(
                "getWebhookInfo failed: Bot API error 401: Unauthorized: [redacted] revoked"
            ),
        "{warning}"
    );
    assert!(!logs.contains(TOKEN), "the token reached a log line");
    tgin.stop().await;
}
