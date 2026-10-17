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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
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
reload: {{ watch: false }}
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
    // Review Focus 6: on an empty queue, grammY's and Telegraf's shutdown
    // confirm (no `timeout`) still answers at once, not after a long poll.
    let started = Instant::now();
    let confirm = timeout(
        Duration::from_secs(1),
        tgin.http
            .post(tgin.url(&scoped))
            .json(&json!({ "offset": 4, "limit": 1 }))
            .send(),
    )
    .await
    .expect("the confirm answers within 1 s")
    .expect("response");
    let (status, body) = json_of(confirm).await;
    assert_eq!((status, update_ids(&body).len()), (200, 0), "{body}");
    assert!(started.elapsed() < Duration::from_secs(1));

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
        r#"tgin_gateway_requests_total{bot="hooked",method="getUpdates",outcome="local"} 6"#,
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
    // A URL without a scheme is https, as telegram-bot-api reads it.
    // Nothing is pending, so nothing is pushed there.
    assert_eq!(
        tgin.post_raw(
            &format!("/c/hooks/bot{TOKEN}/setWebhook"),
            "application/x-www-form-urlencoded",
            b"url=bot.example/hook".to_vec()
        )
        .await,
        (200, was_set.clone())
    );
    let (_, info) = tgin
        .get(&format!("/c/hooks/bot{TOKEN}/getWebhookInfo"))
        .await;
    assert_eq!(info["result"]["url"], "https://bot.example/hook", "{info}");
    assert_eq!(
        tgin.get(&format!("/c/hooks/bot{TOKEN}/deleteWebhook"))
            .await,
        (200, was_deleted.clone())
    );
    assert_eq!(
        upstream_paths(&telegram).await,
        [format!("/bot{TOKEN}/getMe")],
        "no setWebhook or deleteWebhook went upstream"
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="setWebhook",outcome="local"} 8"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="deleteWebhook",outcome="local"} 4"#,
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

/// Waits until the metric line `line` is exported.
async fn wait_metric(tgin: &Tgin, line: &str) {
    let started = Instant::now();
    while !tgin.metrics.encode().lines().any(|l| l == line) {
        assert!(
            started.elapsed() < PATIENCE,
            "missing {line:?} in\n{}",
            tgin.metrics.encode()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A token holder chooses where tgin posts a consumer's updates, but tgin
/// never acts on its own pushes: each carries `Via: 1.1 <pseudonym>`, and
/// both planes answer 508 to a request that carries this process's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_endpoint_never_reaches_tgin_itself() {
    let telegram = telegram().await;
    let down = downstream().await;
    let tgin = start(&intercept_config(&telegram.uri(), "")).await;
    let set = format!("/c/hooks/bot{TOKEN}/setWebhook");

    // The pseudonym, as a downstream sees it.
    assert_eq!(
        tgin.post_json(&set, &json!({ "url": format!("{}/in", down.uri()) }))
            .await
            .0,
        200
    );
    tgin.admit(1).await;
    delivered(&down, 1).await;
    let pushed = down.received_requests().await.expect("recording");
    let via = pushed[0]
        .headers
        .get("via")
        .expect("a Via header")
        .to_str()
        .expect("ASCII")
        .to_owned();
    let pseudonym = via.strip_prefix("1.1 tgin-").expect("1.1 tgin-<hex>");
    assert!(pseudonym.len() == 16 && pseudonym.bytes().all(|b| b.is_ascii_hexdigit()));

    // The reviewer's probe: the runtime endpoint is the token-less loopback
    // admin plane's purge of another consumer. The push is refused, counted
    // as a failure, and `keep` keeps its updates.
    let purge = format!("http://{}/v1/consumers/keep/purge", tgin.admin);
    assert_eq!(tgin.post_json(&set, &json!({ "url": purge })).await.0, 200);
    tgin.admit(2).await;
    wait_metric(
        &tgin,
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="runtime",outcome="dead_letter"} 1"#,
    )
    .await;
    tgin.wait_held("keep", 2).await;
    tgin.assert_metric(r#"tgin_consumer_dropped_total{consumer="keep",reason="purge"} 0"#);

    // The bot's own webhook ingress with its secret: the update is not
    // admitted again.
    let ingress = format!("http://{}/hook", tgin.addr);
    let request = json!({ "url": ingress, "secret_token": HOOK_SECRET });
    assert_eq!(tgin.post_json(&set, &request).await.0, 200);
    tgin.admit(3).await;
    wait_metric(
        &tgin,
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="runtime",outcome="dead_letter"} 2"#,
    )
    .await;
    tgin.wait_held("workers", 3).await;
    tgin.wait_held("keep", 3).await;
    tgin.wait_held("hooks", 0).await;

    // This process's pseudonym is refused on both planes, alone or in a
    // chain; nothing behind the check runs.
    let status = format!("http://{}/v1/status", tgin.admin);
    for value in [via.clone(), format!("1.0 fred, {via} (tgin)")] {
        let response = tgin
            .http
            .get(&status)
            .header("via", &value)
            .send()
            .await
            .expect("admin");
        assert_eq!(
            json_of(response).await,
            (508, json!({ "error": "loop detected" }))
        );
        for request in [
            tgin.http
                .post(tgin.url("/hook"))
                .header("x-telegram-bot-api-secret-token", HOOK_SECRET)
                .header("content-type", "application/json")
                .body(r#"{"update_id":9,"message":{"message_id":9,"chat":{"id":5,"type":"private"},"text":"x"}}"#),
            tgin.http.get(tgin.url(&format!("/bot{TOKEN}/getMe"))),
            tgin.http.get(tgin.url("/c/keep/getUpdates?timeout=0")),
        ] {
            let response = request.header("via", &value).send().await.expect("data");
            assert_eq!(json_of(response).await, (508, telegram_error(508, "Loop Detected")));
        }
    }
    tgin.wait_held("workers", 3).await;

    // Another tgin's pseudonym (a chained instance) passes.
    let foreign = "1.1 tgin-0000000000000001";
    let response = tgin
        .http
        .get(&status)
        .header("via", foreign)
        .send()
        .await
        .expect("admin");
    assert_eq!(response.status(), 200);
    let response = tgin
        .http
        .post(tgin.url("/hook"))
        .header("via", foreign)
        .header("x-telegram-bot-api-secret-token", HOOK_SECRET)
        .header("content-type", "application/json")
        .body(r#"{"update_id":4,"message":{"message_id":4,"chat":{"id":5,"type":"private"},"text":"x"}}"#)
        .send()
        .await
        .expect("webhook");
    assert_eq!(response.status(), 200);
    tgin.wait_held("workers", 4).await;

    // At most one warning a minute per plane, naming the bot, never a URL.
    let logs = LOGS.text();
    let refusals = |plane: &str| {
        logs.lines()
            .filter(|l| l.contains("Via loop") && l.contains(&format!("plane=\"{plane}\"")))
            .count()
    };
    assert_eq!(refusals("admin"), 1, "{logs}");
    assert_eq!(refusals("data"), 1, "{logs}");
    assert!(
        logs.lines().any(|l| l.contains("Via loop")
            && l.contains("plane=\"admin\"")
            && l.contains("hooked")),
        "the refusal names the bot"
    );
    assert!(!logs.contains("/v1/consumers/keep/purge"));
    tgin.stop().await;
}

/// The scoped intercepts reach only the consumer instance the calling bot
/// feeds: after `p1` moves from a pull consumer of `hooked` to a push
/// consumer of `other`, `other` cannot poll `hooked`'s draining updates,
/// and `hooked` still can.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_intercepts_follow_the_feeding_bot_across_reloads() {
    let telegram = telegram().await;
    let config = |hooked_to: &str, other_to: &str, p1: &str| {
        format!(
            r#"version: 1
runtime: {{ lame_duck: 0s, shutdown_grace: 2s }}
reload: {{ watch: false, drain_timeout: 30s }}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"] }}
bots:
  - name: hooked
    token: "{TOKEN}"
    api_url: "{api}"
    ingress: {{ webhook: {{ path: /hook, secret_token: "{HOOK_SECRET}" }} }}
    routes: [{{ to: [{hooked_to}] }}]
  - name: other
    token: "{OTHER_TOKEN}"
    api_url: "{api}"
    routes: [{{ to: [{other_to}] }}]
consumers:
  p1: {p1}
  keep: {{ pull: {{}} }}
"#,
            api = telegram.uri()
        )
    };
    let tgin = start(&config("p1", "keep", "{ pull: {} }")).await;
    tgin.admit(1).await;
    tgin.wait_held("p1", 1).await;
    tgin.reload(
        &config("keep", "p1", "{ push: { accept_set_webhook: true } }"),
        2,
    )
    .await;

    // `other` feeds the push consumer `p1` now: no getUpdates for it.
    let (status, body) = tgin
        .get(&format!("/c/p1/bot{OTHER_TOKEN}/getUpdates?timeout=0"))
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        body,
        telegram_error(409, "Conflict: p1 is not a pull consumer of this bot")
    );
    // Its webhook info is its push consumer's, empty: not the draining
    // pull instance's update.
    let (status, body) = tgin
        .get_raw(&format!("/c/p1/bot{OTHER_TOKEN}/getWebhookInfo"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        String::from_utf8(body).expect("UTF-8"),
        r#"{"ok":true,"result":{"url":"","has_custom_certificate":false,"pending_update_count":0}}"#
    );
    // `hooked` fed the draining pull instance and still drains it.
    let (status, body) = tgin
        .get(&format!("/c/p1/bot{TOKEN}/getUpdates?timeout=0"))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(update_ids(&body), [1]);
    let (status, body) = tgin
        .get(&format!("/c/p1/bot{TOKEN}/getUpdates?offset=2&timeout=0"))
        .await;
    assert_eq!((status, update_ids(&body).len()), (200, 0), "{body}");
    tgin.stop().await;
}

/// The token a reload rotates `TOKEN` to, under the same bot name.
const NEW_TOKEN: &str = "555555555:CCCCrotatedSecretTokenValue_-xyz";

/// Telegram, answering `getMe` for `NEW_TOKEN` too.
async fn telegram_with_rotation() -> MockServer {
    let telegram = telegram().await;
    Mock::given(path(format!("/bot{NEW_TOKEN}/getMe")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"ok":true,"result":{"id":555555555,"is_bot":true,"first_name":"Rotated","username":"hooked_bot"}}"#,
            "application/json",
        ))
        .with_priority(1)
        .mount(&telegram)
        .await;
    telegram
}

/// Waits until the gateway serves `token`: the reload applies the engine
/// before the gateway's bot table.
async fn wait_token(tgin: &Tgin, token: &str) {
    let started = Instant::now();
    while tgin.get(&format!("/bot{token}/getWebhookInfo")).await.0 != 200 {
        assert!(
            started.elapsed() < PATIENCE,
            "the gateway never served the token"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Opens `POST path` with a JSON `body` and sends only its first 4 bytes.
/// The head asks `Expect: 100-continue`, and the `100 Continue` is read
/// before any body byte: hyper writes it only once the handler polls the
/// body, which proves the gateway routed the request (and matched its
/// token) before this returns.
async fn stalled_post(tgin: &Tgin, path: &str, body: &[u8]) -> TcpStream {
    stalled_post_announcing(tgin, path, body.len(), &body[..4]).await
}

/// As [`stalled_post`], announcing `length` bytes and sending `part`.
async fn stalled_post_announcing(tgin: &Tgin, path: &str, length: usize, part: &[u8]) -> TcpStream {
    let mut stream = TcpStream::connect(tgin.addr).await.expect("connect");
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: tgin\r\nConnection: close\r\nContent-Type: application/json\r\nExpect: 100-continue\r\nContent-Length: {length}\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.expect("head");
    let read = async {
        let mut interim = Vec::new();
        while !interim.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            let n = stream.read(&mut byte).await.expect("read");
            assert!(n > 0, "connection closed before 100 Continue");
            interim.push(byte[0]);
        }
        interim
    };
    let interim = timeout(PATIENCE, read).await.expect("100 Continue in time");
    assert!(
        interim.starts_with(b"HTTP/1.1 100"),
        "{}",
        String::from_utf8_lossy(&interim)
    );
    stream.write_all(part).await.expect("part of the body");
    stream
}

/// Reads one response with a `Content-Length` JSON body off `stream`.
async fn raw_response(stream: &mut TcpStream) -> (u16, Value) {
    let read = async {
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
        let status = head[9..12].parse().expect("status");
        let length: usize = head
            .lines()
            .find_map(|l| {
                let (name, value) = l.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().expect("length"))
            })
            .unwrap_or(0);
        let mut body = buf[head_end + 4..].to_vec();
        while body.len() < length {
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await.expect("read");
            assert!(n > 0, "connection closed before the body ended");
            body.extend_from_slice(&chunk[..n]);
        }
        (status, serde_json::from_slice(&body).expect("JSON body"))
    };
    timeout(PATIENCE, read).await.expect("answered in time")
}

/// The reviewer's probe: a leaked token's holder keeps one `setWebhook`
/// open across the rotation of that token. When it completes, it is
/// refused with `401`, nothing is stored, and later updates do not reach
/// the old holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_old_token_set_webhook_changes_nothing_after_rotation() {
    let telegram = telegram_with_rotation().await;
    let attacker = downstream().await;
    let yaml = intercept_config(&telegram.uri(), "");
    let tgin = start(&yaml).await;
    let set = format!("/c/hooks/bot{TOKEN}/setWebhook");
    let capture = format!("{}/in", attacker.uri());
    assert_eq!(
        tgin.post_json(&set, &json!({ "url": capture })).await.0,
        200
    );
    tgin.admit(1).await;
    delivered(&attacker, 1).await;

    let body = serde_json::to_vec(&json!({ "url": capture })).expect("JSON");
    let mut stream = stalled_post(&tgin, &set, &body).await;
    let rotated = yaml.replace(TOKEN, NEW_TOKEN);
    tgin.reload(&rotated, 2).await;
    wait_token(&tgin, NEW_TOKEN).await;
    let info = format!("/c/hooks/bot{NEW_TOKEN}/getWebhookInfo");
    assert_eq!(
        tgin.get(&info).await.1["result"]["url"],
        "",
        "the rotation evicted it"
    );

    stream
        .write_all(&body[4..])
        .await
        .expect("the rest of the body");
    assert_eq!(
        raw_response(&mut stream).await,
        (401, telegram_error(401, "Unauthorized"))
    );
    assert_eq!(
        tgin.get(&info).await.1["result"]["url"],
        "",
        "nothing stored"
    );
    tgin.admit(2).await;
    tgin.wait_held("hooks", 1).await;
    assert_eq!(
        attacker.received_requests().await.expect("recording").len(),
        1,
        "only the update from before the rotation reached the old holder"
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="setWebhook",outcome="rejected"} 1"#,
    );
    tgin.stop().await;
}

/// The same for `deleteWebhook(drop_pending_updates=true)`: a request that
/// authenticated with the old token purges nothing after the rotation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_old_token_drop_pending_purges_nothing_after_rotation() {
    let telegram = telegram_with_rotation().await;
    let yaml = intercept_config(&telegram.uri(), "");
    let tgin = start(&yaml).await;
    tgin.admit(1).await;
    tgin.wait_held("workers", 1).await;

    let body = serde_json::to_vec(&json!({ "drop_pending_updates": true })).expect("JSON");
    let mut stream = stalled_post(
        &tgin,
        &format!("/c/workers/bot{TOKEN}/deleteWebhook"),
        &body,
    )
    .await;
    tgin.reload(&yaml.replace(TOKEN, NEW_TOKEN), 2).await;
    wait_token(&tgin, NEW_TOKEN).await;
    stream
        .write_all(&body[4..])
        .await
        .expect("the rest of the body");
    assert_eq!(
        raw_response(&mut stream).await,
        (401, telegram_error(401, "Unauthorized"))
    );
    tgin.wait_held("workers", 1).await;
    tgin.assert_metric(r#"tgin_consumer_dropped_total{consumer="workers",reason="purge"} 0"#);

    // The new token's holder can.
    assert_eq!(
        tgin.post_json(
            &format!("/c/workers/bot{NEW_TOKEN}/deleteWebhook"),
            &json!({ "drop_pending_updates": true })
        )
        .await
        .0,
        200
    );
    tgin.wait_held("workers", 0).await;
    tgin.stop().await;
}

/// The webhook reply a [`holder`] answers every push with.
const HELD_REPLY: &str = r#"{"method":"sendMessage","chat_id":5,"text":"as the bot"}"#;

/// A token holder's capture endpoint at `{url}/in`: each push it receives
/// goes to the returned channel and waits until the sender is set to
/// `true`, then is answered with [`HELD_REPLY`].
async fn holder() -> (
    String,
    mpsc::UnboundedReceiver<axum::body::Bytes>,
    watch::Sender<bool>,
) {
    let (pushes, received) = mpsc::unbounded_channel();
    let (release, released) = watch::channel(false);
    let router = axum::Router::new().route(
        "/in",
        axum::routing::post(move |body: axum::body::Bytes| {
            let _ = pushes.send(body);
            let mut released = released.clone();
            async move {
                let _ = released.wait_for(|go| *go).await;
                ([("content-type", "application/json")], HELD_REPLY)
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), received, release)
}

/// Whether Telegram was asked to send a message, with any token.
async fn sent_messages(telegram: &MockServer) -> Vec<String> {
    upstream_paths(telegram)
        .await
        .into_iter()
        .filter(|path| path.ends_with("/sendMessage"))
        .collect()
}

/// A reply on its way back from a capture when the rotation of its
/// token evicts the capture is refused: its holder never acts as the bot
/// with the new token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_from_evicted_capture_never_runs_with_new_token() {
    let telegram = telegram_with_rotation().await;
    let (capture, mut pushes, release) = holder().await;
    let yaml = intercept_config(&telegram.uri(), ", webhook_reply: execute");
    let tgin = start(&yaml).await;
    let set = format!("/c/hooks/bot{TOKEN}/setWebhook");
    let url = format!("{capture}/in");
    assert_eq!(tgin.post_json(&set, &json!({ "url": url })).await.0, 200);
    tgin.admit(1).await;
    timeout(PATIENCE, pushes.recv())
        .await
        .expect("pushed in time")
        .expect("open");

    tgin.reload(&yaml.replace(TOKEN, NEW_TOKEN), 2).await;
    wait_token(&tgin, NEW_TOKEN).await;
    let info = format!("/c/hooks/bot{NEW_TOKEN}/getWebhookInfo");
    assert_eq!(tgin.get(&info).await.1["result"]["url"], "", "evicted");
    release.send_replace(true);
    wait_metric(
        &tgin,
        r#"tgin_webhook_replies_total{consumer="hooks",outcome="refused"} 1"#,
    )
    .await;
    assert_eq!(sent_messages(&telegram).await, Vec::<String>::new());
    tgin.assert_metric(r#"tgin_webhook_replies_total{consumer="hooks",outcome="executed"} 0"#);
    tgin.stop().await;
    assert!(!LOGS.text().contains(NEW_TOKEN));
}

/// The incident response to a leaked token: a reload rotates it and
/// removes the consumer its holder captured. The retired instance stops
/// pushing to the capture, and the reply to the update it had in flight is
/// refused, so the holder never acts as the bot with the new token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_capture_never_acts_with_new_token() {
    let telegram = telegram_with_rotation().await;
    let (capture, mut pushes, release) = holder().await;
    let yaml = intercept_config(
        &telegram.uri(),
        ", webhook_reply: execute, max_in_flight: 1",
    );
    let tgin = start(&yaml).await;
    let set = format!("/c/hooks/bot{TOKEN}/setWebhook");
    let url = format!("{capture}/in");
    assert_eq!(tgin.post_json(&set, &json!({ "url": url })).await.0, 200);
    for id in 1..=4 {
        tgin.admit(id).await;
    }
    timeout(PATIENCE, pushes.recv())
        .await
        .expect("pushed in time")
        .expect("open");
    tgin.wait_held("hooks", 4).await;

    let removed: String = yaml
        .replace(TOKEN, NEW_TOKEN)
        .replace("[workers, keep, hooks]", "[workers, keep]")
        .lines()
        .filter(|line| !line.starts_with("  hooks:"))
        .map(|line| format!("{line}\n"))
        .collect();
    tgin.reload(&removed, 2).await;
    wait_token(&tgin, NEW_TOKEN).await;
    release.send_replace(true);
    wait_metric(
        &tgin,
        r#"tgin_webhook_replies_total{consumer="hooks",outcome="refused"} 1"#,
    )
    .await;
    tgin.stop().await;
    assert!(
        pushes.try_recv().is_err(),
        "the retired instance pushed to the capture after the rotation"
    );
    assert_eq!(sent_messages(&telegram).await, Vec::<String>::new());
    assert!(!LOGS.text().contains(NEW_TOKEN));
}

/// An intercepted request whose body stalls ends at the bot's
/// `upstream_idle_timeout`: `408`, counted `client_closed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_intercept_body_ends_at_idle_bound() {
    let telegram = telegram().await;
    let yaml = intercept_config(&telegram.uri(), "").replace(
        "    ingress:",
        "    gateway: { upstream_idle_timeout: 1s }\n    ingress:",
    );
    let tgin = start(&yaml).await;
    let body =
        serde_json::to_vec(&json!({ "url": "https://bot.example/x", "timeout": 0 })).expect("JSON");
    for (path, method) in [
        (format!("/c/hooks/bot{TOKEN}/setWebhook"), "setWebhook"),
        (format!("/c/workers/bot{TOKEN}/getUpdates"), "getUpdates"),
    ] {
        let started = Instant::now();
        let mut stream = stalled_post(&tgin, &path, &body).await;
        assert_eq!(
            raw_response(&mut stream).await,
            (408, telegram_error(408, "Request Timeout")),
            "{method}"
        );
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(900), "{method}: {waited:?}");
        tgin.assert_metric(&format!(
            r#"tgin_gateway_requests_total{{bot="hooked",method="{method}",outcome="client_closed"}} 1"#
        ));
    }
    assert_eq!(tgin.consumer("hooks").await["endpoints"], json!([]));
    tgin.stop().await;
}

/// A consumer-scoped `getUpdates` announcing a body over 64 KiB is refused
/// at once, as `/c/<consumer>/getUpdates` refuses it, without waiting for
/// a body that may never come.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_limit_scoped_get_updates_is_refused_at_once() {
    let telegram = telegram().await;
    let yaml = intercept_config(&telegram.uri(), "").replace(
        "    ingress:",
        "    gateway: { upstream_idle_timeout: 30s }\n    ingress:",
    );
    let tgin = start(&yaml).await;
    let mut stream = TcpStream::connect(tgin.addr).await.expect("connect");
    let head = format!(
        "POST /c/workers/bot{TOKEN}/getUpdates HTTP/1.1\r\nHost: tgin\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: 100000\r\n\r\n{{\"offset\":"
    );
    stream
        .write_all(head.as_bytes())
        .await
        .expect("head and 10 bytes");
    let started = Instant::now();
    assert_eq!(
        raw_response(&mut stream).await,
        (
            400,
            telegram_error(400, "Bad Request: the request body could not be read")
        )
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "answered without waiting: {:?}",
        started.elapsed()
    );
    tgin.stop().await;
}

/// Bot `hooked` (webhook ingress at `/hook`) feeds only the pull consumer
/// `keep`; no lame duck and a `grace` shutdown.
fn drain_config(api: &str, grace: &str) -> String {
    format!(
        r#"version: 1
runtime: {{ lame_duck: 0s, shutdown_grace: {grace} }}
reload: {{ watch: false }}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"] }}
bots:
  - name: hooked
    token: "{TOKEN}"
    api_url: "{api}"
    ingress: {{ webhook: {{ path: /hook, secret_token: "{HOOK_SECRET}" }} }}
    routes: [{{ to: [keep] }}]
consumers:
  keep: {{ pull: {{}} }}
"#
    )
}

/// Telegram for the drain tests: `sendMessage` answers after `delay`.
async fn telegram_sending(delay: Duration) -> MockServer {
    let telegram = telegram().await;
    Mock::given(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "ok": true, "result": { "message_id": 1 } }))
                .set_delay(delay),
        )
        .with_priority(1)
        .mount(&telegram)
        .await;
    telegram
}

impl Tgin {
    /// Starts the shutdown, then polls `keep` once and acknowledges what it
    /// got, as a polling bot does before its handlers run: the drain has
    /// nothing pending afterwards.
    async fn shut_down_after_the_ack(&self) {
        self.signals
            .send(SignalKind::Terminate)
            .await
            .expect("the app listens");
        let polled = format!("/c/keep/bot{TOKEN}/getUpdates?timeout=0");
        let (status, body) = self.get(&polled).await;
        assert_eq!((status, update_ids(&body)), (200, vec![1]), "{body}");
        let (status, body) = self
            .get(&format!("/c/keep/bot{TOKEN}/getUpdates?offset=2&timeout=0"))
            .await;
        assert_eq!((status, update_ids(&body).len()), (200, 0), "{body}");
    }

    /// Waits for the app to exit by itself.
    async fn stopped(self) {
        let outcome = timeout(PATIENCE, self.run)
            .await
            .expect("the app exits in time")
            .expect("the app does not panic");
        assert_eq!(outcome, Outcome::Clean);
    }
}

/// A bot answers an update after acknowledging it (aiogram and
/// python-telegram-bot poll again before their handlers run): its
/// `sendMessage` still goes through during the drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_serves_an_answer_sent_after_the_ack() {
    let telegram = telegram_sending(Duration::ZERO).await;
    let tgin = start(&drain_config(&telegram.uri(), "5s")).await;
    tgin.admit(1).await;
    tgin.wait_held("keep", 1).await;
    tgin.shut_down_after_the_ack().await;
    // The bot's handler runs for a while after the acknowledgement.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (status, body) = tgin
        .post_json(
            &format!("/c/keep/bot{TOKEN}/sendMessage"),
            &json!({ "chat_id": 5, "text": "answer" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let metrics = Arc::clone(&tgin.metrics);
    tgin.stopped().await;
    assert!(
        metrics.encode().lines().any(|l| l
            == r#"tgin_gateway_requests_total{bot="hooked",method="sendMessage",outcome="ok"} 1"#),
        "the answer reached Telegram"
    );
}

/// A call Telegram takes 3 s to answer is not cut when the queues have
/// drained: the drain waits for it, within the grace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_waits_for_an_upstream_call_in_flight() {
    let telegram = telegram_sending(Duration::from_secs(3)).await;
    let tgin = start(&drain_config(&telegram.uri(), "5s")).await;
    tgin.admit(1).await;
    tgin.wait_held("keep", 1).await;
    tgin.shut_down_after_the_ack().await;
    let started = Instant::now();
    let (status, body) = tgin
        .post_json(
            &format!("/c/keep/bot{TOKEN}/sendMessage"),
            &json!({ "chat_id": 5, "text": "slow" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(started.elapsed() >= Duration::from_secs(3));
    tgin.stopped().await;
}

/// Only calls that go to Telegram keep the drain open: a poller that polls
/// again every 500 ms, answered by tgin itself, does not stretch the
/// shutdown to the whole grace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_not_held_open_by_a_poller() {
    let telegram = telegram().await;
    let tgin = start(&drain_config(&telegram.uri(), "10s")).await;
    let poll = tgin.url(&format!("/c/keep/bot{TOKEN}/getUpdates?timeout=0"));
    let http = tgin.http.clone();
    let poller = tokio::spawn(async move {
        // Polls until the data plane closes.
        while http.get(&poll).send().await.is_ok() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    let signalled = Instant::now();
    tgin.signals
        .send(SignalKind::Terminate)
        .await
        .expect("the app listens");
    tgin.stopped().await;
    let took = signalled.elapsed();
    assert!(took < Duration::from_secs(5), "the shutdown took {took:?}");
    timeout(PATIENCE, poller)
        .await
        .expect("the poller stops with the data plane")
        .expect("the poller does not panic");
}

/// A removed push consumer's `tgin_webhook_replies_total` series stay
/// while its retired instance drains, and go when the drain ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_series_follow_the_drain_of_a_removed_consumer() {
    let telegram = telegram().await;
    let (capture, mut pushes, release) = holder().await;
    let yaml = intercept_config(&telegram.uri(), ", webhook_reply: execute").replace(
        "reload: { watch: false }",
        "reload: { watch: false, drain_timeout: 30s }",
    );
    let tgin = start(&yaml).await;
    let set = format!("/c/hooks/bot{TOKEN}/setWebhook");
    let url = format!("{capture}/in");
    assert_eq!(tgin.post_json(&set, &json!({ "url": url })).await.0, 200);
    // Telegram answers every call but getMe with 500: each reply fails.
    release.send_replace(true);
    tgin.admit(1).await;
    let failed =
        |n: u32| format!(r#"tgin_webhook_replies_total{{consumer="hooks",outcome="failed"}} {n}"#);
    wait_metric(&tgin, &failed(1)).await;
    release.send_replace(false);
    tgin.admit(2).await;
    for _ in 1..=2 {
        timeout(PATIENCE, pushes.recv())
            .await
            .expect("pushed in time")
            .expect("open");
    }

    let removed: String = yaml
        .replace("[workers, keep, hooks]", "[workers, keep]")
        .lines()
        .filter(|line| !line.starts_with("  hooks:"))
        .map(|line| format!("{line}\n"))
        .collect();
    tgin.reload(&removed, 2).await;
    wait_token(&tgin, TOKEN).await;
    tgin.assert_metric(&failed(1));
    release.send_replace(true);
    wait_metric(&tgin, &failed(2)).await;
    let started = Instant::now();
    while tgin
        .metrics
        .encode()
        .contains(r#"tgin_webhook_replies_total{consumer="hooks""#)
    {
        assert!(
            started.elapsed() < PATIENCE,
            "the series outlived the drain"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tgin.stop().await;
}
