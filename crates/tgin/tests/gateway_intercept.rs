//! The gateway's local answers end to end: `getUpdates`, `getMe`,
//! `setWebhook`, `deleteWebhook`, `getWebhookInfo`, `logOut` and `close`
//! never reach Telegram as a downstream sent them. The app runs in process
//! on loopback sockets the OS picks, with wiremock as Telegram and as the
//! downstream a `setWebhook` names.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tgin::app::{App, Boot};
use tgin::exit::Outcome;
use tgin_runtime::config::MapEnv;
use tgin_runtime::lifecycle::{SignalKind, Signals};
use tgin_runtime::obs::MetricsRegistry;
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
/// The `result` Telegram's `getMe` answers for `TOKEN`, spaced and ordered
/// unusually and with a field tgin does not know, so only a byte-exact
/// copy matches.
const ME: &str = r#"{"id":123456789, "is_bot":true,"first_name":"Hooked \u0411ot","username":"hooked_bot","x_future":{"a":[1,2.5,null]}}"#;
const OTHER_ME: &str =
    r#"{"id":987654321,"is_bot":true,"first_name":"Other","username":"other_bot"}"#;
const PROXY_ONLY_409: &str =
    "Conflict: tgin serves getUpdates per consumer at /c/<consumer>/bot<token>/getUpdates";

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

/// Bot `hooked` (`TOKEN`, webhook ingress at `/hook`) feeds:
/// - `workers`: pull, with an `auth_token`, honouring `drop_pending_updates`;
/// - `keep`: pull, not honouring it;
/// - `hooks`: push with no configured endpoint, taking one from
///   `setWebhook` and honouring `drop_pending_updates`.
///
/// Bot `other` (`OTHER_TOKEN`, no ingress, so tgin never calls its
/// `getMe`) feeds the pull consumer `others`. `hooks_extra` goes into the
/// `hooks` block.
fn intercept_config(api: &str, hooks_extra: &str) -> String {
    format!(
        r#"version: 1
runtime: {{ lame_duck: 0s, shutdown_grace: 2s }}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"] }}
bots:
  - name: hooked
    token: "{TOKEN}"
    api_url: "{api}"
    ingress: {{ webhook: {{ path: /hook, secret_token: "{HOOK_SECRET}" }} }}
    routes: [{{ to: [workers, keep, hooks] }}]
  - name: other
    token: "{OTHER_TOKEN}"
    api_url: "{api}"
    routes: [{{ to: [others] }}]
consumers:
  workers: {{ pull: {{ auth_token: "pull-secret", honor_drop_pending: true }} }}
  keep: {{ pull: {{}} }}
  hooks: {{ push: {{ accept_set_webhook: true, honor_drop_pending: true{hooks_extra} }} }}
  others: {{ pull: {{}} }}
"#
    )
}

/// Telegram: `getMe` for both bots; anything else answers 500, so a call
/// that should never arrive shows up as a failure too.
async fn telegram() -> MockServer {
    let telegram = MockServer::start().await;
    for (token, me) in [(TOKEN, ME), (OTHER_TOKEN, OTHER_ME)] {
        Mock::given(path(format!("/bot{token}/getMe")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!(r#"{{"ok":true,"result":{me}}}"#),
                "application/json",
            ))
            .mount(&telegram)
            .await;
    }
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&telegram)
        .await;
    telegram
}

/// The app, running on signals the test sends.
struct Tgin {
    addr: SocketAddr,
    admin: SocketAddr,
    file: PathBuf,
    signals: mpsc::Sender<SignalKind>,
    run: JoinHandle<Outcome>,
    metrics: Arc<MetricsRegistry>,
    http: reqwest::Client,
}

fn scratch_config(yaml: &str) -> PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("intercept-{:016x}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).expect("scratch directory");
    let file = dir.join("tgin.yaml");
    std::fs::write(&file, yaml).expect("the config file");
    file
}

async fn start(yaml: &str) -> Tgin {
    start_at(scratch_config(yaml)).await
}

async fn start_at(file: PathBuf) -> Tgin {
    LazyLock::force(&LOGS);
    let boot = Boot::load(&file, Arc::new(MapEnv::default()))
        .unwrap_or_else(|err| panic!("invalid test config: {err:?}"));
    let app = App::start(boot).await.expect("tgin starts");
    let addr = app.data_addrs()[0];
    let admin = app.admin_addrs()[0];
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

    /// Sends `update_id` (a private message in chat 5) to the webhook
    /// ingress.
    async fn admit(&self, update_id: i64) {
        let update = format!(
            r#"{{"update_id":{update_id},"message":{{"message_id":{update_id},"chat":{{"id":5,"type":"private"}},"text":"hi"}}}}"#
        );
        let response = self
            .http
            .post(self.url("/hook"))
            .header("x-telegram-bot-api-secret-token", HOOK_SECRET)
            .header("content-type", "application/json")
            .body(update)
            .send()
            .await
            .expect("webhook");
        assert_eq!(response.status(), 200);
    }

    /// `POST` with a JSON body.
    async fn post_json(&self, path: &str, body: &Value) -> (u16, Value) {
        let response = timeout(PATIENCE, self.http.post(self.url(path)).json(body).send())
            .await
            .expect("answered in time")
            .expect("response");
        json_of(response).await
    }

    /// `POST` with a raw body and content type.
    async fn post_raw(&self, path: &str, content_type: &str, body: Vec<u8>) -> (u16, Value) {
        let response = timeout(
            PATIENCE,
            self.http
                .post(self.url(path))
                .header("content-type", content_type)
                .body(body)
                .send(),
        )
        .await
        .expect("answered in time")
        .expect("response");
        json_of(response).await
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let response = timeout(PATIENCE, self.http.get(self.url(path)).send())
            .await
            .expect("answered in time")
            .expect("response");
        json_of(response).await
    }

    async fn get_raw(&self, path: &str) -> (u16, Vec<u8>) {
        let response = timeout(PATIENCE, self.http.get(self.url(path)).send())
            .await
            .expect("answered in time")
            .expect("response");
        let status = response.status().as_u16();
        (status, response.bytes().await.expect("body").to_vec())
    }

    /// `/v1/status`, from the loopback admin plane (no token needed).
    async fn status(&self) -> Value {
        let response = self
            .http
            .get(format!("http://{}/v1/status", self.admin))
            .send()
            .await
            .expect("status");
        assert_eq!(response.status(), 200);
        response.json().await.expect("status JSON")
    }

    /// The status entry of the consumer `name`.
    async fn consumer(&self, name: &str) -> Value {
        let status = self.status().await;
        status["consumers"]
            .as_array()
            .expect("consumers")
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("no consumer {name} in {status}"))
            .clone()
    }

    /// Updates the consumer `name` holds: queued and leased.
    async fn held(&self, name: &str) -> u64 {
        let consumer = self.consumer(name).await;
        consumer["depth"].as_u64().expect("depth") + consumer["leased"].as_u64().expect("leased")
    }

    /// Waits until the consumer `name` holds `n` updates.
    async fn wait_held(&self, name: &str, n: u64) {
        let started = Instant::now();
        loop {
            let held = self.held(name).await;
            if held == n {
                return;
            }
            assert!(
                started.elapsed() < PATIENCE,
                "{name} holds {held} updates, not {n}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Rewrites the config, reloads, and waits until the generation `generation`
    /// is in effect.
    async fn reload(&self, yaml: &str, generation: u64) {
        std::fs::write(&self.file, yaml).expect("rewrite");
        self.signals
            .send(SignalKind::Hangup)
            .await
            .expect("the app listens");
        let started = Instant::now();
        loop {
            if self.status().await["generation"] == generation {
                return;
            }
            assert!(started.elapsed() < PATIENCE, "the reload never applied");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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

fn ok_true(description: Option<&str>) -> Value {
    match description {
        Some(description) => json!({ "ok": true, "result": true, "description": description }),
        None => json!({ "ok": true, "result": true }),
    }
}

async fn json_of(response: reqwest::Response) -> (u16, Value) {
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("body");
    (
        status,
        serde_json::from_slice(&body).unwrap_or_else(|_| panic!("JSON: {body:?}")),
    )
}

/// The paths Telegram was asked for, in order.
async fn upstream_paths(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("recording on")
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect()
}

fn update_ids(body: &Value) -> Vec<i64> {
    body["result"]
        .as_array()
        .unwrap_or_else(|| panic!("a batch: {body}"))
        .iter()
        .map(|u| u["update_id"].as_i64().expect("update_id"))
        .collect()
}

/// A multipart body with the text fields `fields`, plus a file part named
/// `certificate` as `setWebhook` uploads one.
fn multipart(fields: &[(&str, &str)]) -> (String, Vec<u8>) {
    let boundary = "tgin-test-boundary";
    let mut body = String::new();
    body.push_str(&format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"certificate\"; filename=\"cert.pem\"\r\nContent-Type: application/octet-stream\r\n\r\n-----BEGIN CERTIFICATE-----\r\n"
    ));
    for (name, value) in fields {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    (
        format!("multipart/form-data; boundary={boundary}"),
        body.into_bytes(),
    )
}

/// Where updates are pushed once a `setWebhook` names it.
async fn downstream() -> MockServer {
    let down = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/in"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&down)
        .await;
    down
}

/// Waits until `down` received the update `update_id`, and returns the
/// value of its secret header.
async fn delivered(down: &MockServer, update_id: i64) -> Option<String> {
    let needle = format!(r#"{{"update_id":{update_id},"#);
    let started = Instant::now();
    loop {
        let got = down.received_requests().await.expect("recording");
        if let Some(req) = got.iter().find(|r| r.body.starts_with(needle.as_bytes())) {
            return req
                .headers
                .get("x-telegram-bot-api-secret-token")
                .map(|v| v.to_str().expect("ASCII").to_owned());
        }
        assert!(
            started.elapsed() < PATIENCE,
            "update {update_id} never reached the downstream"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_updates_consumer_scoped_serves_queue() {
    let telegram = telegram().await;
    let tgin = start(&intercept_config(&telegram.uri(), "")).await;
    for id in 1..=3 {
        tgin.admit(id).await;
    }
    let scoped = format!("/c/workers/bot{TOKEN}/getUpdates");

    // grammY: JSON `{offset, limit}` with no timeout answers at once. The
    // bot token is the authority: no `Authorization` header.
    let (status, body) = tgin
        .post_json(&scoped, &json!({ "offset": 0, "limit": 1 }))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(update_ids(&body), [1]);
    // grammY's shutdown confirm: `{offset: last + 1, limit: 1}`.
    let (status, body) = tgin
        .post_json(&scoped, &json!({ "offset": 2, "limit": 1 }))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(update_ids(&body), [2]);
    // python-telegram-bot: a form with `timeout=0`.
    let (status, body) = tgin
        .post_raw(
            &scoped,
            "application/x-www-form-urlencoded",
            b"offset=3&timeout=0".to_vec(),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(update_ids(&body), [3]);
    // A case-insensitive method in the query form confirms the last one.
    let (status, body) = tgin
        .get(&format!(
            "/c/workers/bot{TOKEN}/GETUPDATES?offset=4&timeout=0"
        ))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(update_ids(&body).is_empty());
    tgin.wait_held("workers", 0).await;

    // The consumer's own path still needs its bearer token.
    let (status, _) = tgin.get("/c/workers/getUpdates?timeout=0").await;
    assert_eq!(status, 401);
    // A multipart getUpdates is refused, like on `/c/<consumer>/getUpdates`.
    let (content_type, body) = multipart(&[("offset", "4")]);
    let (status, body) = tgin.post_raw(&scoped, &content_type, body).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        body,
        telegram_error(
            400,
            "Bad Request: send the parameters as a query string, a form or JSON"
        )
    );

    assert!(
        !upstream_paths(&telegram)
            .await
            .iter()
            .any(|p| p.ends_with("/getUpdates")),
        "getUpdates never goes upstream"
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="getUpdates",outcome="local"} 5"#,
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_updates_proxy_only_is_409() {
    let telegram = telegram().await;
    let tgin = start(&intercept_config(&telegram.uri(), "")).await;
    tgin.admit(1).await;
    for path in [
        format!("/bot{TOKEN}/getUpdates"),
        format!("/bot{TOKEN}/getupdates?offset=1"),
    ] {
        let (status, body) = tgin.get(&path).await;
        assert_eq!(status, 409, "{path}");
        assert_eq!(body, telegram_error(409, PROXY_ONLY_409));
    }
    let (status, body) = tgin
        .post_json(&format!("/bot{TOKEN}/getUpdates"), &json!({ "timeout": 0 }))
        .await;
    assert_eq!(status, 409);
    assert_eq!(body, telegram_error(409, PROXY_ONLY_409));
    // The update was not consumed by the refusals.
    tgin.wait_held("workers", 1).await;
    assert_eq!(
        upstream_paths(&telegram).await,
        [format!("/bot{TOKEN}/getMe")],
        "only tgin's own getMe went upstream"
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="getUpdates",outcome="local"} 3"#,
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_updates_on_push_consumer_is_409() {
    let telegram = telegram().await;
    let tgin = start(&intercept_config(&telegram.uri(), "")).await;
    tgin.admit(1).await;
    let (status, body) = tgin
        .post_json(
            &format!("/c/hooks/bot{TOKEN}/getUpdates"),
            &json!({ "timeout": 0 }),
        )
        .await;
    assert_eq!(status, 409);
    assert_eq!(
        body,
        telegram_error(409, "Conflict: hooks is not a pull consumer of this bot")
    );
    // Another bot's consumer is not this bot's to poll.
    let (status, body) = tgin.get(&format!("/c/others/bot{TOKEN}/getUpdates")).await;
    assert_eq!(status, 404);
    assert_eq!(body, telegram_error(404, "Not Found: consumer not found"));
    tgin.wait_held("hooks", 1).await;
    assert_eq!(
        upstream_paths(&telegram).await,
        [format!("/bot{TOKEN}/getMe")]
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_me_served_from_cache_without_upstream() {
    let telegram = telegram().await;
    let tgin = start(&intercept_config(&telegram.uri(), "")).await;
    // Starting the webhook bot called getMe once.
    assert_eq!(
        upstream_paths(&telegram).await,
        [format!("/bot{TOKEN}/getMe")]
    );
    let expected = format!(r#"{{"ok":true,"result":{ME}}}"#);
    for path in [
        format!("/bot{TOKEN}/getMe"),
        format!("/bot{TOKEN}/GETME"),
        format!("/c/workers/bot{TOKEN}/getMe"),
    ] {
        let (status, body) = tgin.get_raw(&path).await;
        assert_eq!(status, 200, "{path}");
        assert_eq!(String::from_utf8(body).expect("UTF-8"), expected, "{path}");
    }
    let (status, body) = tgin
        .post_json(&format!("/bot{TOKEN}/getMe"), &json!({}))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["username"], "hooked_bot");
    assert_eq!(
        upstream_paths(&telegram).await,
        [format!("/bot{TOKEN}/getMe")],
        "the cached identity answered"
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="getMe",outcome="local"} 4"#,
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_me_cache_filled_on_first_proxy() {
    let telegram = telegram().await;
    let tgin = start(&intercept_config(&telegram.uri(), "")).await;
    let other_me = format!("/bot{OTHER_TOKEN}/getMe");
    // `other` has no ingress: tgin never asked Telegram who it is.
    assert!(!upstream_paths(&telegram).await.contains(&other_me));
    let expected = format!(r#"{{"ok":true,"result":{OTHER_ME}}}"#);
    for _ in 0..3 {
        let (status, body) = tgin.get_raw(&other_me).await;
        assert_eq!(status, 200);
        assert_eq!(String::from_utf8(body).expect("UTF-8"), expected);
    }
    let asked = upstream_paths(&telegram)
        .await
        .iter()
        .filter(|p| **p == other_me)
        .count();
    assert_eq!(asked, 1, "proxied once, then cached");
    tgin.assert_metric(r#"tgin_gateway_requests_total{bot="other",method="getMe",outcome="ok"} 1"#);
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="other",method="getMe",outcome="local"} 2"#,
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_webhook_never_reaches_upstream() {
    let telegram = telegram().await;
    let down = downstream().await;
    let tgin = start(&intercept_config(&telegram.uri(), "")).await;
    tgin.admit(1).await;
    let was_set = ok_true(Some("Webhook was set"));
    let was_deleted = ok_true(Some("Webhook was deleted"));
    let hook = "https://bot.example/hook";

    let set = format!("/bot{TOKEN}/setWebhook");
    assert_eq!(
        tgin.post_json(&set, &json!({ "url": hook, "drop_pending_updates": true }))
            .await,
        (200, was_set.clone())
    );
    assert_eq!(
        tgin.post_raw(
            &set,
            "application/x-www-form-urlencoded",
            format!("url={hook}&secret_token=abc").into_bytes()
        )
        .await,
        (200, was_set.clone())
    );
    let (content_type, body) = multipart(&[("url", hook), ("max_connections", "40")]);
    assert_eq!(
        tgin.post_raw(&set, &content_type, body).await,
        (200, was_set.clone())
    );
    assert_eq!(
        tgin.get(&format!("/bot{TOKEN}/SETWEBHOOK?url={hook}"))
            .await,
        (200, was_set.clone())
    );
    // An empty `url` deletes, as on Telegram.
    assert_eq!(
        tgin.get(&format!("/bot{TOKEN}/setWebhook?url=")).await,
        (200, was_deleted.clone())
    );
    assert_eq!(
        tgin.post_json(
            &format!("/bot{TOKEN}/deleteWebhook"),
            &json!({ "drop_pending_updates": true })
        )
        .await,
        (200, was_deleted.clone())
    );
    // A consumer that takes no endpoint answers as the bot path does.
    assert_eq!(
        tgin.post_json(
            &format!("/c/workers/bot{TOKEN}/setWebhook"),
            &json!({ "url": hook })
        )
        .await,
        (200, was_set.clone())
    );
    assert_eq!(
        tgin.post_json(
            &format!("/c/keep/bot{TOKEN}/deleteWebhook"),
            &json!({ "drop_pending_updates": true })
        )
        .await,
        (200, was_deleted.clone())
    );
    // Captured by the consumer that accepts it: still not forwarded.
    assert_eq!(
        tgin.post_json(
            &format!("/c/hooks/bot{TOKEN}/setWebhook"),
            &json!({ "url": format!("{}/in", down.uri()) })
        )
        .await,
        (200, was_set.clone())
    );
    assert_eq!(delivered(&down, 1).await, None);
    assert_eq!(
        tgin.post_json(&format!("/c/hooks/bot{TOKEN}/deleteWebhook"), &json!({}))
            .await,
        (200, was_deleted.clone())
    );

    // Nothing was dropped: neither the bot path nor `keep` purge.
    tgin.wait_held("workers", 1).await;
    tgin.wait_held("keep", 1).await;
    tgin.wait_held("hooks", 0).await;
    assert_eq!(
        upstream_paths(&telegram).await,
        [format!("/bot{TOKEN}/getMe")],
        "no setWebhook or deleteWebhook went upstream"
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="setWebhook",outcome="local"} 7"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="deleteWebhook",outcome="local"} 3"#,
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_webhook_captures_runtime_endpoint() {
    let telegram = telegram().await;
    let down = downstream().await;
    let tgin = start(&intercept_config(&telegram.uri(), "")).await;
    // Queued before any endpoint: kept, not dead-lettered.
    tgin.admit(1).await;
    tgin.wait_held("hooks", 1).await;
    assert_eq!(tgin.consumer("hooks").await["endpoints"], json!([]));

    let set = format!("/c/hooks/bot{TOKEN}/setWebhook");
    let url = format!("{}/in", down.uri());
    let request = json!({ "url": url, "secret_token": "s3cr3t_-A" });
    assert_eq!(
        tgin.post_json(&set, &request).await,
        (200, ok_true(Some("Webhook was set")))
    );
    assert_eq!(delivered(&down, 1).await.as_deref(), Some("s3cr3t_-A"));
    tgin.admit(2).await;
    assert_eq!(delivered(&down, 2).await.as_deref(), Some("s3cr3t_-A"));
    let hooks = tgin.consumer("hooks").await;
    assert_eq!(
        hooks["endpoints"],
        json!([{ "url_redacted": down.uri(), "breaker": "closed", "runtime": true }]),
        "{hooks}"
    );
    // The same webhook again changes nothing.
    assert_eq!(
        tgin.post_json(&set, &request).await,
        (200, ok_true(Some("Webhook is already set")))
    );
    tgin.assert_metric(
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="runtime",outcome="delivered"} 2"#,
    );

    // Refused like Telegram refuses them; the endpoint stays.
    for (body, description) in [
        (
            json!({ "url": "ftp://bot.example/in" }),
            "Bad Request: invalid webhook URL specified",
        ),
        (
            json!({ "url": "not a url" }),
            "Bad Request: invalid webhook URL specified",
        ),
        (
            json!({ "url": url, "secret_token": "x".repeat(257) }),
            "Bad Request: secret token is too long",
        ),
        (
            json!({ "url": url, "secret_token": "not base64url!" }),
            "Bad Request: secret token contains illegal characters",
        ),
    ] {
        assert_eq!(
            tgin.post_json(&set, &body).await,
            (400, telegram_error(400, description)),
            "{body}"
        );
    }
    // The bot path never changes a consumer's endpoint.
    assert_eq!(
        tgin.post_json(
            &format!("/bot{TOKEN}/setWebhook"),
            &json!({ "url": "https://elsewhere.example/x" })
        )
        .await,
        (200, ok_true(Some("Webhook was set")))
    );
    tgin.admit(3).await;
    assert_eq!(delivered(&down, 3).await.as_deref(), Some("s3cr3t_-A"));

    // An empty url removes it; updates queue again.
    assert_eq!(
        tgin.post_raw(&set, "application/x-www-form-urlencoded", b"url=".to_vec())
            .await,
        (200, ok_true(Some("Webhook was deleted")))
    );
    assert_eq!(
        tgin.post_json(&format!("/c/hooks/bot{TOKEN}/deleteWebhook"), &json!({}))
            .await,
        (200, ok_true(Some("Webhook is already deleted")))
    );
    tgin.admit(4).await;
    tgin.wait_held("hooks", 1).await;
    assert_eq!(tgin.consumer("hooks").await["endpoints"], json!([]));
    assert_eq!(down.received_requests().await.expect("recording").len(), 3);

    let logs = LOGS.text();
    assert!(!logs.contains("s3cr3t_-A"), "the secret reached a log line");
    assert!(!logs.contains(TOKEN), "the token reached a log line");
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_endpoint_survives_reload_lost_on_restart() {
    let telegram = telegram().await;
    let down = downstream().await;
    let tgin = start(&intercept_config(&telegram.uri(), "")).await;
    let set = format!("/c/hooks/bot{TOKEN}/setWebhook");
    let url = format!("{}/in", down.uri());
    assert_eq!(
        tgin.post_json(&set, &json!({ "url": url, "secret_token": "keep_me" }))
            .await
            .0,
        200
    );
    tgin.admit(1).await;
    assert_eq!(delivered(&down, 1).await.as_deref(), Some("keep_me"));

    // A reload that changes the consumer's delivery settings keeps it.
    let changed = intercept_config(
        &telegram.uri(),
        ", max_in_flight: 4, retry: { max_attempts: 3 }",
    );
    tgin.reload(&changed, 2).await;
    tgin.admit(2).await;
    assert_eq!(delivered(&down, 2).await.as_deref(), Some("keep_me"));
    let info = tgin
        .get(&format!("/c/hooks/bot{TOKEN}/getWebhookInfo"))
        .await;
    assert_eq!(info.1["result"]["url"], url.as_str());

    // A restart forgets it: updates queue until the next setWebhook.
    let file = tgin.file.clone();
    tgin.stop().await;
    let tgin = start_at(file).await;
    let (status, info) = tgin
        .get(&format!("/c/hooks/bot{TOKEN}/getWebhookInfo"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(info["result"]["url"], "", "{info}");
    tgin.admit(3).await;
    tgin.wait_held("hooks", 1).await;
    assert_eq!(down.received_requests().await.expect("recording").len(), 2);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_webhook_drop_pending_purges_only_caller_with_honor_flag() {
    let telegram = telegram().await;
    let tgin = start(&intercept_config(&telegram.uri(), "")).await;
    for id in 1..=2 {
        tgin.admit(id).await;
    }
    for name in ["workers", "keep", "hooks"] {
        tgin.wait_held(name, 2).await;
    }
    let drop = json!({ "drop_pending_updates": true });
    // The bot path drops nothing: it is no consumer's.
    assert_eq!(
        tgin.post_json(&format!("/bot{TOKEN}/deleteWebhook"), &drop)
            .await
            .0,
        200
    );
    // `keep` does not honour the flag.
    assert_eq!(
        tgin.get(&format!(
            "/c/keep/bot{TOKEN}/deleteWebhook?drop_pending_updates=true"
        ))
        .await
        .0,
        200
    );
    // Not asked to drop: nothing dropped.
    assert_eq!(
        tgin.post_json(
            &format!("/c/workers/bot{TOKEN}/deleteWebhook"),
            &json!({ "drop_pending_updates": false })
        )
        .await
        .0,
        200
    );
    for name in ["workers", "keep", "hooks"] {
        tgin.wait_held(name, 2).await;
    }
    // The caller that honours it drops its own pending updates only.
    assert_eq!(
        tgin.post_raw(
            &format!("/c/workers/bot{TOKEN}/deleteWebhook"),
            "application/x-www-form-urlencoded",
            b"drop_pending_updates=True".to_vec()
        )
        .await,
        (200, ok_true(Some("Webhook was deleted")))
    );
    tgin.wait_held("workers", 0).await;
    tgin.wait_held("keep", 2).await;
    tgin.wait_held("hooks", 2).await;
    let (content_type, body) = multipart(&[("drop_pending_updates", "1")]);
    assert_eq!(
        tgin.post_raw(
            &format!("/c/hooks/bot{TOKEN}/deleteWebhook"),
            &content_type,
            body
        )
        .await,
        (200, ok_true(Some("Webhook is already deleted")))
    );
    tgin.wait_held("hooks", 0).await;
    tgin.wait_held("keep", 2).await;
    tgin.assert_metric(r#"tgin_consumer_dropped_total{consumer="workers",reason="purge"} 2"#);
    tgin.assert_metric(r#"tgin_consumer_dropped_total{consumer="hooks",reason="purge"} 2"#);
    tgin.assert_metric(r#"tgin_consumer_dropped_total{consumer="keep",reason="purge"} 0"#);
    assert_eq!(
        upstream_paths(&telegram).await,
        [format!("/bot{TOKEN}/getMe")]
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_webhook_info_synthesized() {
    let telegram = telegram().await;
    let tgin = start(&intercept_config(&telegram.uri(), "")).await;
    let (status, body) = tgin.get_raw(&format!("/bot{TOKEN}/getWebhookInfo")).await;
    assert_eq!(status, 200);
    assert_eq!(
        String::from_utf8(body).expect("UTF-8"),
        r#"{"ok":true,"result":{"url":"","has_custom_certificate":false,"pending_update_count":0}}"#
    );
    for id in 1..=2 {
        tgin.admit(id).await;
    }
    tgin.wait_held("workers", 2).await;
    let (status, body) = tgin
        .get_raw(&format!("/c/workers/bot{TOKEN}/GETWEBHOOKINFO"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        String::from_utf8(body).expect("UTF-8"),
        r#"{"ok":true,"result":{"url":"","has_custom_certificate":false,"pending_update_count":2}}"#
    );
    let (status, body) = tgin
        .get_raw(&format!("/c/hooks/bot{TOKEN}/getWebhookInfo"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        String::from_utf8(body).expect("UTF-8"),
        r#"{"ok":true,"result":{"url":"","has_custom_certificate":false,"pending_update_count":2}}"#
    );

    // A failing downstream keeps the push updates pending (retrying)
    // while the info is read.
    let failing = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&failing)
        .await;
    let url = format!("{}/in?x=1&y=two", failing.uri());
    assert_eq!(
        tgin.post_json(
            &format!("/c/hooks/bot{TOKEN}/setWebhook"),
            &json!({ "url": url })
        )
        .await
        .0,
        200
    );
    let (status, info) = tgin
        .post_json(&format!("/c/hooks/bot{TOKEN}/getWebhookInfo"), &json!({}))
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        info,
        json!({ "ok": true, "result": {
            "url": url, "has_custom_certificate": false, "pending_update_count": 2,
        }})
    );
    tgin.wait_held("hooks", 2).await;
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="getWebhookInfo",outcome="local"} 4"#,
    );
    assert_eq!(
        upstream_paths(&telegram).await,
        [format!("/bot{TOKEN}/getMe")]
    );
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_out_and_close_never_forwarded() {
    let telegram = telegram().await;
    let tgin = start(&intercept_config(&telegram.uri(), "")).await;
    for path in [
        format!("/bot{TOKEN}/logOut"),
        format!("/bot{TOKEN}/close"),
        format!("/c/workers/bot{TOKEN}/LOGOUT"),
        format!("/bot{OTHER_TOKEN}/Close"),
    ] {
        let (status, body) = tgin.post_json(&path, &json!({})).await;
        assert_eq!((status, body), (200, ok_true(None)), "{path}");
    }
    assert_eq!(
        upstream_paths(&telegram).await,
        [format!("/bot{TOKEN}/getMe")],
        "logOut and close never go upstream"
    );
    let logs = LOGS.text();
    for (bot, method) in [
        ("hooked", "logOut"),
        ("hooked", "close"),
        ("other", "close"),
    ] {
        assert!(
            logs.lines().any(|line| line.contains(" WARN ")
                && line.contains(&format!("bot={bot}"))
                && line.contains(&format!("method=\"{method}\""))),
            "no warning for {bot} {method} in\n{logs}"
        );
    }
    assert!(!logs.contains(TOKEN) && !logs.contains(OTHER_TOKEN));
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="logOut",outcome="local"} 2"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="other",method="close",outcome="local"} 1"#,
    );
    tgin.stop().await;
}
