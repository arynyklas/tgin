//! The admin plane and hot reload, end to end: the app in process, wiremock
//! as Telegram and as push downstreams, real loopback sockets on ports the
//! OS picks, and the `tgin admin` command line against them.

mod support;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    assert_eq!(task.error.as_deref(), Some(error.as_str()));
    assert!(source.errors_total >= 1);
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
    // As a migrated 0.2 client polls it (`/bot…` is reserved in v1).
    const ALIAS: &str = "/legacy/one/getUpdates";
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
