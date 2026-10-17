//! Webhook replies end to end: a push endpoint answers an update with a Bot
//! API method call in its response body, and tgin makes that call to
//! Telegram once, before the update is acknowledged, as Telegram executes a
//! webhook reply. The app runs in process on loopback sockets the OS picks;
//! wiremock plays the downstream, and a small axum server plays Telegram,
//! recording every call with its arrival time and able to hold one.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::response::Response;
use serde_json::Value;
use tgin::app::{App, Boot};
use tgin::exit::Outcome;
use tgin_runtime::config::MapEnv;
use tgin_runtime::lifecycle::{SignalKind, Signals};
use tgin_runtime::obs::MetricsRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Bounds every wait, so a stall fails the test instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(10);
const TOKEN: &str = "123456789:AAAAexampleSecretTokenValue_-xyz";
const HOOK_SECRET: &str = "hook-secret-value";
const ME: &str = r#"{"ok":true,"result":{"id":123456789,"is_bot":true,"first_name":"Hooked","username":"hooked_bot"}}"#;
const OK_TRUE: &str = r#"{"ok":true,"result":true}"#;
/// The largest reply tgin executes: 1 MiB.
const MAX_REPLY: usize = 1 << 20;

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

/// What a test adds to [`config`].
#[derive(Default)]
struct Extra<'a> {
    /// Lines in the bot's block, each indented by four spaces.
    bot: &'a str,
    /// Inside the `hooks` push block, after its own settings.
    hooks: &'a str,
    /// More consumers the bot routes to, after `hooks`: `, name`.
    to: &'a str,
    /// More consumer lines, each indented by two spaces.
    consumers: &'a str,
}

/// Bot `hooked` (`TOKEN`, webhook ingress at `/hook`) feeds the push
/// consumer `hooks`, which posts to `{down}/in` one update per chat at a
/// time and retries after 10 ms.
fn config(api: &str, down: &str, extra: &Extra<'_>) -> String {
    let Extra {
        bot,
        hooks,
        to,
        consumers,
    } = extra;
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
{bot}    ingress: {{ webhook: {{ path: /hook, secret_token: "{HOOK_SECRET}" }} }}
    routes: [{{ to: [hooks{to}] }}]
consumers:
  hooks: {{ push: {{ endpoints: [{{ url: "{down}/in" }}], ordering: per_chat, retry: {{ initial: 10ms, max: 10ms }}{hooks} }} }}
{consumers}"#
    )
}

/// One call Telegram received, stamped with tokio's clock.
#[derive(Clone, Debug)]
struct Call {
    path: String,
    content_type: Option<String>,
    body: Bytes,
    at: tokio::time::Instant,
}

/// Telegram: `getMe` answers the bot; every other call answers `answer`
/// (`{"ok":true,"result":true}` by default) and goes to `arrivals` when
/// one is taken. The first call other than `getMe` waits for `hold`'s
/// release when one is set, for at most [`PATIENCE`] of real time.
#[derive(Clone, Default)]
struct Telegram {
    calls: Arc<Mutex<Vec<Call>>>,
    hold: Arc<Mutex<Option<Hold>>>,
    answer: Arc<Mutex<Option<(u16, String)>>>,
    arrivals: Arc<Mutex<Option<mpsc::UnboundedSender<Call>>>>,
}

struct Hold {
    arrived: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

impl Telegram {
    /// Starts it; its base URL.
    async fn serve(&self) -> String {
        let router = axum::Router::new()
            .fallback(telegram_answer)
            .with_state(self.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{addr}")
    }

    /// Holds the next call other than `getMe`: returns when it arrived and
    /// the sender that releases it.
    fn hold_next(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (arrived, arrival) = oneshot::channel();
        let (release, released) = oneshot::channel();
        *self.hold.lock().expect("hold") = Some(Hold {
            arrived,
            release: released,
        });
        (arrival, release)
    }

    /// Every later call other than `getMe`, as it arrives.
    fn arrivals(&self) -> mpsc::UnboundedReceiver<Call> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.arrivals.lock().expect("arrivals") = Some(tx);
        rx
    }

    /// The calls of `method` so far.
    fn calls_to(&self, method: &str) -> Vec<Call> {
        let path = format!("/bot{TOKEN}/{method}");
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|call| call.path == path)
            .cloned()
            .collect()
    }

    /// The paths of every call so far, in order.
    fn paths(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .map(|call| call.path.clone())
            .collect()
    }

    /// Waits until `method` was called `n` times.
    async fn wait_calls(&self, method: &str, n: usize) -> Vec<Call> {
        let started = Instant::now();
        loop {
            let calls = self.calls_to(method);
            if calls.len() >= n {
                return calls;
            }
            assert!(
                started.elapsed() < PATIENCE,
                "{method} reached Telegram {} times, not {n}",
                calls.len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

async fn telegram_answer(State(telegram): State<Telegram>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    let content_type = req
        .headers()
        .get("content-type")
        .map(|v| v.to_str().expect("ASCII").to_owned());
    let body = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .expect("the request body");
    let call = Call {
        path: path.clone(),
        content_type,
        body,
        at: tokio::time::Instant::now(),
    };
    telegram.calls.lock().expect("calls").push(call.clone());
    if path.ends_with("/getMe") {
        return json_response(200, ME.to_owned());
    }
    if let Some(arrivals) = &*telegram.arrivals.lock().expect("arrivals") {
        let _ = arrivals.send(call);
    }
    let hold = telegram.hold.lock().expect("hold").take();
    if let Some(hold) = hold {
        let _ = hold.arrived.send(());
        tokio::select! {
            _ = hold.release => {}
            _ = real_time(PATIENCE) => {}
        }
    }
    let answer = telegram.answer.lock().expect("answer").clone();
    let (status, body) = answer.unwrap_or_else(|| (200, OK_TRUE.to_owned()));
    json_response(status, body)
}

/// Fires after `wait` of real time, whatever tokio's clock does.
fn real_time(wait: Duration) -> oneshot::Receiver<()> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(wait);
        let _ = tx.send(());
    });
    rx
}

/// Tokio's clock, frozen (as in `gateway.rs`): the test's runtime starts
/// paused, and a blocking task keeps the paused clock from jumping to the
/// next timer whenever the runtime waits for a socket, so time moves only
/// by [`tokio::time::advance`]. Taken first thing, every instant is a whole
/// number of milliseconds after the timer driver's start.
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

/// `fut`, bounded by [`PATIENCE`] of real time.
async fn within<F: Future>(what: &str, fut: F) -> F::Output {
    tokio::select! {
        out = fut => out,
        _ = real_time(PATIENCE) => panic!("{what} did not happen within {PATIENCE:?}"),
    }
}

/// How long [`quiet`] watches, in real time: far longer than a call takes
/// over loopback, so one that was due would show up.
const QUIET: Duration = Duration::from_millis(300);

/// Asserts that no call reaches Telegram for [`QUIET`] of real time.
async fn quiet(what: &str, arrivals: &mut mpsc::UnboundedReceiver<Call>) {
    tokio::select! {
        call = arrivals.recv() => {
            let at = call.map(|call| call.at);
            panic!("{what}: a call reached Telegram at {at:?}");
        }
        _ = real_time(QUIET) => {}
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

fn json_response(status: u16, body: String) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("a response")
}

/// The app, running on signals the test sends.
struct Tgin {
    addr: SocketAddr,
    admin: SocketAddr,
    signals: mpsc::Sender<SignalKind>,
    run: JoinHandle<Outcome>,
    metrics: Arc<MetricsRegistry>,
    http: reqwest::Client,
}

fn scratch_config(yaml: &str) -> PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("reply-{:016x}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).expect("scratch directory");
    let file = dir.join("tgin.yaml");
    std::fs::write(&file, yaml).expect("the config file");
    file
}

async fn start(yaml: &str) -> Tgin {
    LazyLock::force(&LOGS);
    let boot = Boot::load(&scratch_config(yaml), Arc::new(MapEnv::default()))
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
        signals,
        run,
        metrics,
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
    }
}

impl Tgin {
    #[track_caller]
    fn assert_metric(&self, line: &str) {
        let out = self.metrics.encode();
        assert!(out.lines().any(|l| l == line), "missing {line:?} in\n{out}");
    }

    fn has_metric(&self, line: &str) -> bool {
        self.metrics.encode().lines().any(|l| l == line)
    }

    /// Waits until the metric line `line` is exported.
    async fn wait_metric(&self, line: &str) {
        let started = Instant::now();
        while !self.metrics.encode().lines().any(|l| l == line) {
            assert!(
                started.elapsed() < PATIENCE,
                "missing {line:?} in\n{}",
                self.metrics.encode()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Sends the update `update_id`, a private message in `chat`, to the
    /// webhook ingress.
    async fn admit(&self, update_id: i64, chat: i64) {
        let update = format!(
            r#"{{"update_id":{update_id},"message":{{"message_id":{update_id},"chat":{{"id":{chat},"type":"private"}},"text":"hi"}}}}"#
        );
        let response = self
            .http
            .post(format!("http://{}/hook", self.addr))
            .header("x-telegram-bot-api-secret-token", HOOK_SECRET)
            .header("content-type", "application/json")
            .body(update)
            .send()
            .await
            .expect("webhook");
        assert_eq!(response.status(), 200);
    }

    /// The status entry of the consumer `name`, from the loopback admin
    /// plane.
    async fn consumer(&self, name: &str) -> Value {
        let response = self
            .http
            .get(format!("http://{}/v1/status", self.admin))
            .send()
            .await
            .expect("status");
        assert_eq!(response.status(), 200);
        let status: Value = response.json().await.expect("status JSON");
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

/// The downstream answers the update `update_id` with `status` and `body`
/// of type `content_type`.
async fn answer_update(
    down: &MockServer,
    update_id: i64,
    status: u16,
    content_type: &str,
    body: impl Into<Vec<u8>>,
) {
    Mock::given(method("POST"))
        .and(path("/in"))
        .and(body_string_contains(format!(r#""update_id":{update_id},"#)))
        .respond_with(ResponseTemplate::new(status).set_body_raw(body.into(), content_type))
        .mount(down)
        .await;
}

/// A multipart body of `parts` (name, optional file name, bytes).
fn multipart(boundary: &str, parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, file, value) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match file {
            Some(file) => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{file}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

#[track_caller]
fn assert_no_token_logged() {
    assert!(!LOGS.text().contains(TOKEN), "the bot token was logged");
}

/// Review Focus 5: the reply goes to Telegram exactly as the endpoint sent
/// it, once per delivery, while the update is still leased. Under
/// `ordering: per_chat` the chat's next update is posted only after the
/// reply call has completed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_reply_executed_once_before_ack() {
    let telegram = Telegram::default();
    let api = telegram.serve().await;
    let (arrived, release) = telegram.hold_next();
    let reply = r#"{"method":"sendMessage","chat_id":5,"text":"pong \u00e9","reply_parameters":{"message_id":1}}"#;
    let content_type = "Application/JSON; charset=utf-8";
    let down = MockServer::start().await;
    for id in 1..=2 {
        answer_update(&down, id, 200, content_type, reply).await;
    }
    let tgin = start(&config(&api, &down.uri(), &Extra::default())).await;
    tgin.admit(1, 5).await;
    tgin.admit(2, 5).await;

    timeout(PATIENCE, arrived)
        .await
        .expect("the first reply reaches Telegram in time")
        .expect("held");
    // The reply call is in flight: update 1 is still leased (not acked),
    // and update 2 of the same chat waits behind it.
    let hooks = tgin.consumer("hooks").await;
    assert_eq!(hooks["leased"], 1, "{hooks}");
    assert_eq!(
        tgin.held("hooks").await,
        2,
        "update 1 acked before its reply"
    );
    assert_eq!(down.received_requests().await.expect("recording").len(), 1);

    release.send(()).expect("Telegram waits");
    tgin.wait_held("hooks", 0).await;
    tgin.wait_metric(r#"tgin_webhook_replies_total{consumer="hooks",outcome="executed"} 2"#)
        .await;
    let pushed = down.received_requests().await.expect("recording");
    assert_eq!(pushed.len(), 2, "each update is posted once");
    assert!(
        String::from_utf8_lossy(&pushed[1].body).starts_with(r#"{"update_id":2,"#),
        "update 2 posted after the reply"
    );
    let replies = telegram.calls_to("sendMessage");
    assert_eq!(replies.len(), 2, "one reply call per delivery");
    for call in &replies {
        assert_eq!(&call.body[..], reply.as_bytes());
        assert_eq!(call.content_type.as_deref(), Some(content_type));
    }
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="sendMessage",outcome="ok"} 2"#,
    );
    tgin.assert_metric(
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="0",outcome="delivered"} 2"#,
    );
    assert_no_token_logged();
    tgin.stop().await;
}

/// An urlencoded reply and a multipart one, whose `method` field follows a
/// binary file part, reach Telegram byte for byte with their own
/// `Content-Type`, boundary included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_and_multipart_replies_forwarded_verbatim() {
    let telegram = Telegram::default();
    let api = telegram.serve().await;
    let form = "method=sendMessage&chat_id=5&text=a+b%26c";
    let form_type = "application/x-www-form-urlencoded";
    let boundary = "Reply-Boundary-7Xq";
    let file: &[u8] = b"\x00\x01binary\xff\xfe\r\n--Reply-Boundary-7X\r\nnot a part\r\n";
    let upload = multipart(
        boundary,
        &[
            ("document", Some("a.bin"), file),
            ("method", None, b"sendDocument"),
            ("chat_id", None, b"6"),
        ],
    );
    let upload_type = format!("multipart/form-data; boundary={boundary}");
    let down = MockServer::start().await;
    answer_update(&down, 1, 200, form_type, form).await;
    answer_update(&down, 2, 200, &upload_type, upload.clone()).await;
    let tgin = start(&config(&api, &down.uri(), &Extra::default())).await;
    tgin.admit(1, 5).await;
    tgin.admit(2, 6).await;

    let [sent_form] = &telegram.wait_calls("sendMessage", 1).await[..] else {
        panic!("one sendMessage");
    };
    assert_eq!(&sent_form.body[..], form.as_bytes());
    assert_eq!(sent_form.content_type.as_deref(), Some(form_type));
    let [sent_upload] = &telegram.wait_calls("sendDocument", 1).await[..] else {
        panic!("one sendDocument");
    };
    assert_eq!(&sent_upload.body[..], &upload[..], "byte-identical");
    assert_eq!(
        sent_upload.content_type.as_deref(),
        Some(upload_type.as_str())
    );
    tgin.wait_held("hooks", 0).await;
    tgin.wait_metric(r#"tgin_webhook_replies_total{consumer="hooks",outcome="executed"} 2"#)
        .await;
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="sendDocument",outcome="ok"} 1"#,
    );
    tgin.stop().await;
}

/// Refused as Telegram refuses them: every `get…` method (in any case),
/// `setWebhook`, `deleteWebhook`, `close`, `logOut`, an empty method and
/// none. A `sendMessage` after them is executed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_methods_not_executed() {
    let telegram = Telegram::default();
    let api = telegram.serve().await;
    let json = "application/json";
    let form = "application/x-www-form-urlencoded";
    let boundary = "refusal-boundary";
    let upload_type = format!("multipart/form-data; boundary={boundary}");
    let close = multipart(boundary, &[("method", None, b"close")]);
    let down = MockServer::start().await;
    answer_update(&down, 1, 200, json, r#"{"method":"getMe"}"#).await;
    answer_update(&down, 2, 200, json, r#"{"method":"GetChat","chat_id":5}"#).await;
    answer_update(
        &down,
        3,
        200,
        form,
        "method=setWebhook&url=https%3A%2F%2Fevil.example%2F",
    )
    .await;
    answer_update(
        &down,
        4,
        200,
        json,
        r#"{"method":"deleteWebhook","drop_pending_updates":true}"#,
    )
    .await;
    answer_update(&down, 5, 200, &upload_type, close).await;
    answer_update(&down, 6, 200, form, "method=logOut").await;
    answer_update(
        &down,
        7,
        200,
        json,
        r#"{"method":"","chat_id":5,"text":"x"}"#,
    )
    .await;
    answer_update(&down, 8, 200, json, r#"{"chat_id":5,"text":"no method"}"#).await;
    let control = r#"{"method":"sendMessage","chat_id":5,"text":"control"}"#;
    answer_update(&down, 9, 200, json, control).await;
    let tgin = start(&config(&api, &down.uri(), &Extra::default())).await;
    let before = telegram.paths();
    for id in 1..=9 {
        tgin.admit(id, 5).await;
    }

    tgin.wait_held("hooks", 0).await;
    tgin.wait_metric(r#"tgin_webhook_replies_total{consumer="hooks",outcome="executed"} 1"#)
        .await;
    tgin.assert_metric(r#"tgin_webhook_replies_total{consumer="hooks",outcome="refused"} 8"#);
    let mut after = telegram.paths();
    assert_eq!(after.drain(..before.len()).collect::<Vec<_>>(), before);
    assert_eq!(after, [format!("/bot{TOKEN}/sendMessage")]);
    assert_eq!(
        &telegram.calls_to("sendMessage")[0].body[..],
        control.as_bytes()
    );
    let out = tgin.metrics.encode();
    let methods: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with(r#"tgin_gateway_requests_total{bot="hooked""#))
        .collect();
    assert!(
        methods
            .iter()
            .all(|l| l.contains(r#"method="sendMessage""#)),
        "{methods:?}"
    );
    tgin.assert_metric(
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="0",outcome="delivered"} 9"#,
    );
    tgin.stop().await;
}

/// `{"method":"sendMessage","chat_id":5,"text":"x…x"}` of exactly `len`
/// bytes.
fn padded_reply(len: usize) -> String {
    let head = r#"{"method":"sendMessage","chat_id":5,"text":""#;
    let tail = r#""}"#;
    let mut reply = String::with_capacity(len);
    reply.push_str(head);
    reply.push_str(&"x".repeat(len - head.len() - tail.len()));
    reply.push_str(tail);
    reply
}

/// A reply over 1 MiB is counted `too_large` and not executed, and its
/// update is delivered all the same; one of exactly 1 MiB is executed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_over_limit_not_executed_delivery_acked() {
    let telegram = Telegram::default();
    let api = telegram.serve().await;
    let down = MockServer::start().await;
    let json = "application/json";
    answer_update(&down, 1, 200, json, padded_reply(MAX_REPLY + 1)).await;
    let at_limit = padded_reply(MAX_REPLY);
    answer_update(&down, 2, 200, json, at_limit.clone()).await;
    let tgin = start(&config(&api, &down.uri(), &Extra::default())).await;
    tgin.admit(1, 5).await;
    tgin.admit(2, 5).await;

    tgin.wait_held("hooks", 0).await;
    tgin.wait_metric(r#"tgin_webhook_replies_total{consumer="hooks",outcome="executed"} 1"#)
        .await;
    tgin.assert_metric(r#"tgin_webhook_replies_total{consumer="hooks",outcome="too_large"} 1"#);
    tgin.assert_metric(
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="0",outcome="delivered"} 2"#,
    );
    let [sent] = &telegram.calls_to("sendMessage")[..] else {
        panic!("only the reply within the limit goes");
    };
    assert_eq!(sent.body.len(), MAX_REPLY);
    assert!(sent.body == at_limit.as_bytes(), "the 1 MiB reply verbatim");
    tgin.stop().await;
}

/// Answers every request on `listener` with the head of a 200 JSON response
/// announcing 1000 bytes and the start of a method call, then never sends
/// the rest; the connections stay open.
async fn stalling_downstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let mut open: Vec<TcpStream> = Vec::new();
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = Vec::new();
            let complete = loop {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break false;
                }
                request.extend_from_slice(&chunk[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map_or(0, |v| v.trim().parse().unwrap_or(0));
                    if request.len() >= end + 4 + length {
                        break true;
                    }
                }
            };
            if complete {
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{\"method\":\"sendMessage\",\"chat_id\":5,\"text\":\"")
                    .await;
                open.push(stream);
            }
        }
    });
    addr
}

/// A consumer with `webhook_reply: ignore` never reads its endpoints'
/// response bodies: a body that never ends delays nothing. Beside it, a
/// consumer of the same bot that executes replies does execute its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_reply_ignore_reads_no_body() {
    let telegram = Telegram::default();
    let api = telegram.serve().await;
    let reply = r#"{"method":"sendMessage","chat_id":5,"text":"from hooks"}"#;
    let down = MockServer::start().await;
    answer_update(&down, 1, 200, "application/json", reply).await;
    let stalling = stalling_downstream().await;
    let ignored = format!(
        "  ignored: {{ push: {{ endpoints: [{{ url: \"http://{stalling}/in\", timeout: 30s }}], webhook_reply: ignore }} }}\n"
    );
    let extra = Extra {
        to: ", ignored",
        consumers: &ignored,
        ..Extra::default()
    };
    let tgin = start(&config(&api, &down.uri(), &extra)).await;
    tgin.admit(1, 5).await;

    // Reading the body would wait for the endpoint's 30 s timeout.
    tgin.wait_held("ignored", 0).await;
    tgin.assert_metric(
        r#"tgin_delivery_attempts_total{consumer="ignored",endpoint="0",outcome="delivered"} 1"#,
    );
    tgin.wait_held("hooks", 0).await;
    tgin.wait_metric(r#"tgin_webhook_replies_total{consumer="hooks",outcome="executed"} 1"#)
        .await;
    let [sent] = &telegram.calls_to("sendMessage")[..] else {
        panic!("only hooks' reply goes");
    };
    assert_eq!(&sent.body[..], reply.as_bytes());
    let out = tgin.metrics.encode();
    assert!(
        !out.contains(r#"tgin_webhook_replies_total{consumer="ignored""#),
        "{out}"
    );
    tgin.stop().await;
}

/// A delivery that fails is retried, and the method in the failed
/// answer's body is never executed; neither is one in a 4xx answer, which
/// dead-letters the update. The successful answer's reply is executed once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_delivery_never_executes_reply() {
    let telegram = Telegram::default();
    let api = telegram.serve().await;
    let json = "application/json";
    let down = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/in"))
        .and(body_string_contains(r#""update_id":1,"#))
        .and(header("x-tgin-attempt", "1"))
        .respond_with(ResponseTemplate::new(500).set_body_raw(
            r#"{"method":"sendMessage","chat_id":5,"text":"from the 500"}"#,
            json,
        ))
        .with_priority(1)
        .mount(&down)
        .await;
    let delivered = r#"{"method":"sendMessage","chat_id":5,"text":"from the 200"}"#;
    answer_update(&down, 1, 200, json, delivered).await;
    answer_update(
        &down,
        2,
        400,
        json,
        r#"{"method":"sendMessage","chat_id":6,"text":"from the 400"}"#,
    )
    .await;
    let tgin = start(&config(&api, &down.uri(), &Extra::default())).await;
    tgin.admit(1, 5).await;
    tgin.admit(2, 6).await;

    tgin.wait_held("hooks", 0).await;
    tgin.wait_metric(r#"tgin_webhook_replies_total{consumer="hooks",outcome="executed"} 1"#)
        .await;
    for line in [
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="0",outcome="retry"} 1"#,
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="0",outcome="delivered"} 1"#,
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="0",outcome="dead_letter"} 1"#,
    ] {
        tgin.assert_metric(line);
    }
    let [sent] = &telegram.calls_to("sendMessage")[..] else {
        panic!("only the delivered answer's reply goes");
    };
    assert_eq!(&sent.body[..], delivered.as_bytes());
    tgin.stop().await;
}

/// Replies go through the bot's rate limits, in `shape` mode here, on the
/// frozen clock: a reply waits exactly for the global quota's next slot,
/// and one whose chat quota has no slot within `max_delay` is not executed
/// (`rate_limited`, a warn line). Its update is delivered all the same.
#[tokio::test(start_paused = true)]
async fn reply_rate_limited_in_shape_mode() {
    let frozen = freeze();
    let telegram = Telegram::default();
    let mut arrivals = telegram.arrivals();
    let api = telegram.serve().await;
    let json = "application/json";
    let down = MockServer::start().await;
    let first = r#"{"method":"sendMessage","chat_id":5,"text":"first"}"#;
    let second = r#"{"method":"sendMessage","chat_id":6,"text":"second"}"#;
    let third = r#"{"method":"sendMessage","chat_id":5,"text":"third"}"#;
    answer_update(&down, 1, 200, json, first).await;
    answer_update(&down, 2, 200, json, second).await;
    answer_update(&down, 3, 200, json, third).await;
    let extra = Extra {
        bot: "    gateway: { rate_limit: { mode: shape, max_delay: 2s, global: { count: 1, per: 1s, burst: 1 }, per_chat: { count: 1, per: 1m, burst: 1 } } }\n",
        ..Extra::default()
    };
    let tgin = start(&config(&api, &down.uri(), &extra)).await;
    let t0 = tokio::time::Instant::now();
    let executed = |n: u32| {
        format!(r#"tgin_webhook_replies_total{{consumer="hooks",outcome="executed"}} {n}"#)
    };

    tgin.admit(1, 5).await;
    let call = within("the first reply", arrivals.recv())
        .await
        .expect("open");
    assert_eq!((call.at, &call.body[..]), (t0, first.as_bytes()));
    until("the first reply executed", || tgin.has_metric(&executed(1))).await;

    // The global quota's next slot is a second after the first reply's.
    tgin.admit(2, 6).await;
    let delayed =
        r#"tgin_gateway_ratelimited_total{bot="hooked",scope="global",action="delayed"} 1"#;
    until("the second reply's delay", || tgin.has_metric(delayed)).await;
    tokio::time::advance(Duration::from_millis(999)).await;
    quiet("the second reply before its slot", &mut arrivals).await;
    tokio::time::advance(Duration::from_millis(1)).await;
    let call = within("the second reply", arrivals.recv())
        .await
        .expect("open");
    assert_eq!(
        (call.at, &call.body[..]),
        (t0 + Duration::from_secs(1), second.as_bytes())
    );
    until("the second reply executed", || {
        tgin.has_metric(&executed(2))
    })
    .await;

    // Chat 5's next slot is a minute away, past `max_delay`.
    tgin.admit(3, 5).await;
    let refused = r#"tgin_webhook_replies_total{consumer="hooks",outcome="rate_limited"} 1"#;
    until("the third reply refused", || tgin.has_metric(refused)).await;
    let delivered =
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="0",outcome="delivered"} 3"#;
    until("every update delivered", || tgin.has_metric(delivered)).await;
    quiet("the refused reply", &mut arrivals).await;
    tgin.assert_metric(
        r#"tgin_gateway_ratelimited_total{bot="hooked",scope="chat",action="rejected"} 1"#,
    );
    tgin.assert_metric(&executed(2));
    let logs = LOGS.text();
    assert!(
        logs.lines().any(|l| l.contains("WARN")
            && l.contains("rate limits")
            && l.contains("consumer=hooks")
            && l.contains("bot=hooked")),
        "no rate-limit warning"
    );
    assert_no_token_logged();
    frozen.thaw();
    tgin.stop().await;
}

/// A reply's call to Telegram ends after 30 s in all: it counts `failed`
/// and `unreachable`, and the update is delivered.
#[tokio::test(start_paused = true)]
async fn reply_call_bounded_by_30s() {
    let frozen = freeze();
    let telegram = Telegram::default();
    let (arrived, release) = telegram.hold_next();
    let api = telegram.serve().await;
    let down = MockServer::start().await;
    let reply = r#"{"method":"sendMessage","chat_id":5,"text":"slow"}"#;
    answer_update(&down, 1, 200, "application/json", reply).await;
    let tgin = start(&config(&api, &down.uri(), &Extra::default())).await;
    let failed = r#"tgin_webhook_replies_total{consumer="hooks",outcome="failed"} 1"#;

    tgin.admit(1, 5).await;
    within("the reply reaches Telegram", arrived)
        .await
        .expect("held");
    tokio::time::advance(Duration::from_millis(29_999)).await;
    let _ = real_time(QUIET).await;
    assert!(!tgin.has_metric(failed), "the reply call ended before 30 s");
    tokio::time::advance(Duration::from_millis(1)).await;
    until("the reply call times out", || tgin.has_metric(failed)).await;
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="sendMessage",outcome="unreachable"} 1"#,
    );
    let delivered =
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="0",outcome="delivered"} 1"#;
    until("the update delivered", || tgin.has_metric(delivered)).await;
    drop(release);
    frozen.thaw();
    tgin.stop().await;
}

/// A multipart reply's `chat_id` is not read for the limits, as for a
/// streamed gateway request: only the global quota applies, so it goes
/// while its chat's quota is used up. A JSON reply to that chat is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multipart_reply_limited_by_global_quota_only() {
    let telegram = Telegram::default();
    let api = telegram.serve().await;
    let json = "application/json";
    let boundary = "limits-boundary";
    let upload = multipart(
        boundary,
        &[
            ("method", None, b"sendDocument"),
            ("chat_id", None, b"7"),
            ("document", Some("a.bin"), b"\x01\x02"),
        ],
    );
    let down = MockServer::start().await;
    let to_seven = r#"{"method":"sendMessage","chat_id":7,"text":"x"}"#;
    answer_update(&down, 1, 200, json, to_seven).await;
    answer_update(
        &down,
        2,
        200,
        &format!("multipart/form-data; boundary={boundary}"),
        upload.clone(),
    )
    .await;
    answer_update(&down, 3, 200, json, to_seven).await;
    let extra = Extra {
        bot: "    gateway: { rate_limit: { mode: reject, per_chat: { count: 1, per: 1m, burst: 1 } } }\n",
        ..Extra::default()
    };
    let tgin = start(&config(&api, &down.uri(), &extra)).await;
    for id in 1..=3 {
        tgin.admit(id, 5).await;
    }

    tgin.wait_held("hooks", 0).await;
    tgin.wait_metric(r#"tgin_webhook_replies_total{consumer="hooks",outcome="rate_limited"} 1"#)
        .await;
    tgin.assert_metric(r#"tgin_webhook_replies_total{consumer="hooks",outcome="executed"} 2"#);
    let [sent] = &telegram.calls_to("sendDocument")[..] else {
        panic!("the multipart reply goes");
    };
    assert_eq!(&sent.body[..], &upload[..]);
    assert_eq!(telegram.calls_to("sendMessage").len(), 1);
    tgin.assert_metric(
        r#"tgin_gateway_ratelimited_total{bot="hooked",scope="chat",action="rejected"} 1"#,
    );
    tgin.stop().await;
}

/// Telegram refusing a reply counts `failed`, with no second attempt; the
/// warn line carries Telegram's description with the token masked and at
/// most 256 bytes of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_reply_logged_with_redacted_description() {
    let telegram = Telegram::default();
    let api = telegram.serve().await;
    let description = format!(
        "Bad Request: chat not found near bot{TOKEN} {}",
        "y".repeat(400)
    );
    *telegram.answer.lock().expect("answer") = Some((
        400,
        serde_json::json!({ "ok": false, "error_code": 400, "description": description })
            .to_string(),
    ));
    let down = MockServer::start().await;
    let reply = r#"{"method":"sendMessage","chat_id":5,"text":"lost"}"#;
    answer_update(&down, 1, 200, "application/json", reply).await;
    let tgin = start(&config(&api, &down.uri(), &Extra::default())).await;
    tgin.admit(1, 5).await;

    tgin.wait_held("hooks", 0).await;
    tgin.wait_metric(r#"tgin_webhook_replies_total{consumer="hooks",outcome="failed"} 1"#)
        .await;
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="hooked",method="sendMessage",outcome="rejected"} 1"#,
    );
    tgin.assert_metric(
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="0",outcome="delivered"} 1"#,
    );
    assert_eq!(telegram.calls_to("sendMessage").len(), 1, "no retry");
    let logs = LOGS.text();
    let line = logs
        .lines()
        .find(|l| l.contains("WARN") && l.contains("Bad Request: chat not found near bot"))
        .unwrap_or_else(|| panic!("no warning with the description"));
    assert!(!line.contains(TOKEN), "{line}");
    // `Bad Request: chat not found near bot[redacted] ` is 47 bytes, so a
    // capped description keeps at most 209 of the 400 `y`s.
    assert!(line.contains(&"y".repeat(100)), "{line}");
    assert!(!line.contains(&"y".repeat(210)), "not capped: {line}");
    assert_no_token_logged();
    tgin.stop().await;
}
