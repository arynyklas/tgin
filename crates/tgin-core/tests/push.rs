//! The push consumer against wiremock endpoints, in real time with
//! millisecond-scale backoffs: wiremock needs real I/O.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
use std::time::Duration;

use bytes::Bytes;
use prometheus_client::encoding::text::encode;
use prometheus_client::registry::Registry;
use tgin_core::consumer::push::{
    Balance, Endpoint, LEASE_TIMEOUT, PushConsumer, PushOptions, RetryPolicy, RuntimeEndpoint,
};
use tgin_core::envelope::Envelope;
use tgin_core::ids::SourceId;
use tgin_core::metrics::CoreMetrics;
use tgin_core::reply::{MAX_REPLY_BYTES, Reply, ReplyExecutor, ReplyHandle};
use tgin_runtime::breaker::{Breaker, BreakerConfig, BreakerState};
use tgin_runtime::http::Via;
use tgin_runtime::queue::{OnFull, Ordering, QueueConfig};
use tgin_runtime::secret::Secret;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Bound on every wait for a condition.
const WAIT: Duration = Duration::from_secs(10);
/// Marker inside every payload; it must never reach the logs.
const PAYLOAD_MARKER: &str = "payload-marker";

/// Everything every test logs. A global subscriber, since a scoped one can
/// miss events while tests on other threads register callsites.
static LOGS: LazyLock<Logs> = LazyLock::new(|| {
    let logs = Logs::default();
    tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .try_init()
        .expect("the only global subscriber");
    logs
});

fn retry(initial_ms: u64, max_ms: u64, max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        initial: Duration::from_millis(initial_ms),
        max: Duration::from_millis(max_ms),
        max_attempts,
        max_age: Duration::from_secs(3600),
    }
}

fn options(balance: Balance, max_in_flight: usize, retry: RetryPolicy) -> PushOptions {
    PushOptions {
        max_in_flight,
        balance,
        retry,
        source_names: Arc::from([Arc::<str>::from("bot1")]),
        breaker: BreakerConfig {
            failure_threshold: 1000,
            open_for: Duration::from_secs(60),
        },
        runtime_timeout: Duration::from_secs(5),
        reply: None,
    }
}

/// The endpoint a `setWebhook` of bot 0 names.
fn runtime(server: &MockServer, route: &str, secret: Option<&str>) -> RuntimeEndpoint {
    RuntimeEndpoint {
        url: format!("{}{route}", server.uri()).parse().expect("url"),
        secret_token: secret.map(|s| Secret::new(s.to_owned())),
        source: SourceId(0),
        token_fingerprint: 7,
    }
}

fn breaker(failure_threshold: u32, open_for: Duration) -> Breaker {
    Breaker::new(BreakerConfig {
        failure_threshold,
        open_for,
    })
}

fn endpoint_with(server: &MockServer, route: &str, breaker: Breaker) -> Endpoint {
    Endpoint {
        url: format!("{}{route}", server.uri()).parse().expect("url"),
        origin: Arc::from(server.uri()),
        secret_token: None,
        timeout: Duration::from_secs(5),
        breaker,
    }
}

fn endpoint(server: &MockServer, route: &str) -> Endpoint {
    endpoint_with(server, route, breaker(1000, Duration::from_secs(60)))
}

fn queue(ordering: Ordering) -> QueueConfig {
    QueueConfig {
        capacity: 1000,
        capacity_bytes: 1 << 20,
        lease_timeout: LEASE_TIMEOUT,
        ordering,
        on_full: OnFull::Backpressure,
    }
}

fn client() -> reqwest::Client {
    // As `tgin_runtime::http::client` builds it: no redirects. No proxy
    // either, so proxy variables in the environment cannot reroute tests.
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("client")
}

struct Harness {
    reg: Registry,
    consumer: Arc<PushConsumer>,
    via: Via,
    cancel: CancellationToken,
    force: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl Harness {
    fn new(cfg: QueueConfig, endpoints: Vec<Endpoint>, opts: PushOptions) -> Self {
        LazyLock::force(&LOGS);
        let mut reg = Registry::with_prefix("tgin");
        let metrics = CoreMetrics::register(&mut reg).consumer("hooks");
        let via = Via::generate();
        let consumer = Arc::new(PushConsumer::new(
            Arc::from("hooks"),
            cfg,
            endpoints,
            opts,
            client(),
            &via,
            metrics,
        ));
        let mut h = Self {
            reg,
            consumer,
            via,
            cancel: CancellationToken::new(),
            force: CancellationToken::new(),
            task: None,
        };
        h.spawn();
        h
    }

    fn spawn(&mut self) {
        self.cancel = CancellationToken::new();
        self.force = CancellationToken::new();
        self.task = Some(self.consumer.spawn(self.cancel.clone(), self.force.clone()));
    }

    async fn join(&mut self) {
        let task = self.task.take().expect("running");
        timeout(WAIT, task)
            .await
            .expect("dispatcher stops")
            .expect("dispatcher does not panic");
    }

    fn admit_at(&self, update_id: i64, key: Option<u64>, received_at: Instant) {
        let key_json = key.map_or_else(|| "null".to_owned(), |k| k.to_string());
        let payload = Bytes::from(format!(
            r#"{{"update_id":{update_id},"key":{key_json},"text":"{PAYLOAD_MARKER}"}}"#
        ));
        let q = self.consumer.queue();
        let permit = q.try_reserve(payload.len()).expect("room");
        let envelope = Envelope {
            source: SourceId(0),
            update_id,
            update_type: Bytes::from_static(b"message"),
            key,
            payload,
            received_at,
        };
        q.commit(permit, envelope, key, update_id)
            .expect("own permit");
    }

    fn admit(&self, update_id: i64, key: Option<u64>) {
        self.admit_at(update_id, key, Instant::now());
    }

    async fn settled(&self) {
        let c = &self.consumer;
        eventually("every update settles", || {
            c.pending() == 0 && c.in_flight() == 0
        })
        .await;
    }

    fn metrics(&self) -> String {
        let mut s = String::new();
        encode(&mut s, &self.reg).expect("encode");
        s
    }

    fn assert_metric(&self, line: &str) {
        let out = self.metrics();
        assert!(out.lines().any(|l| l == line), "missing {line:?} in\n{out}");
    }

    /// The value of the series `series`, which must exist.
    fn value(&self, series: &str) -> u64 {
        let out = self.metrics();
        let prefix = format!("{series} ");
        out.lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("missing {series:?} in\n{out}"))
            .parse()
            .expect("integer")
    }

    fn attempts(&self, endpoint: usize, outcome: &str) -> String {
        format!(
            r#"tgin_delivery_attempts_total{{consumer="hooks",endpoint="{endpoint}",outcome="{outcome}"}}"#
        )
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.force.cancel();
    }
}

async fn eventually(what: &str, cond: impl Fn() -> bool) {
    timeout(WAIT, async {
        while !cond() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.expect("recording on")
}

async fn wait_requests(server: &MockServer, n: usize) -> Vec<Request> {
    timeout(WAIT, async {
        loop {
            let got = requests(server).await;
            if got.len() >= n {
                return got;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {n} requests"))
}

fn json(req: &Request) -> serde_json::Value {
    serde_json::from_slice(&req.body).expect("json body")
}

fn update_id(req: &Request) -> i64 {
    json(req)["update_id"].as_i64().expect("update_id")
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers.get(name).map(|v| v.to_str().expect("ascii"))
}

fn attempt_header(req: &Request) -> u32 {
    header(req, "x-tgin-attempt")
        .expect("attempt header")
        .parse()
        .expect("number")
}

fn on(req: &Request, route: &str) -> bool {
    req.url.path() == route
}

async fn ok(server: &MockServer) {
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("logs").clone()).expect("utf-8")
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

    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

#[tokio::test]
async fn delivers_with_headers() {
    let server = MockServer::start().await;
    ok(&server).await;
    let mut with_secret = endpoint(&server, "/a");
    with_secret.secret_token = Some(Secret::new("s3cret-token".to_owned()));
    let h = Harness::new(
        queue(Ordering::None),
        vec![with_secret, endpoint(&server, "/b")],
        options(Balance::RoundRobin, 1, retry(10, 50, 5)),
    );
    h.admit(1, None);
    h.admit(2, Some(42));
    h.settled().await;

    let got = requests(&server).await;
    assert_eq!(got.len(), 2);
    for req in &got {
        assert_eq!(req.method.as_str(), "POST");
        assert_eq!(header(req, "content-type"), Some("application/json"));
        assert_eq!(header(req, "x-tgin-source"), Some("bot1"));
        assert_eq!(header(req, "x-tgin-update-type"), Some("message"));
        assert_eq!(header(req, "x-tgin-attempt"), Some("1"));
        let via = format!("1.1 {}", h.via.pseudonym());
        assert_eq!(header(req, "via"), Some(via.as_str()));
        let id = update_id(req);
        let key = if id == 2 { "42" } else { "null" };
        assert_eq!(
            req.body,
            format!(r#"{{"update_id":{id},"key":{key},"text":"{PAYLOAD_MARKER}"}}"#).as_bytes()
        );
        let secret = header(req, "x-telegram-bot-api-secret-token");
        if on(req, "/a") {
            assert_eq!(secret, Some("s3cret-token"));
        } else {
            assert!(on(req, "/b"));
            assert_eq!(secret, None, "no secret configured, no header");
        }
    }
    assert!(got.iter().any(|r| on(r, "/a")) && got.iter().any(|r| on(r, "/b")));

    h.assert_metric(&format!("{} 1", h.attempts(0, "delivered")));
    h.assert_metric(&format!("{} 1", h.attempts(1, "delivered")));
    h.assert_metric(&format!("{} 0", h.attempts(0, "retry")));
    h.assert_metric(r#"tgin_delivery_latency_seconds_count{consumer="hooks"} 2"#);
    h.assert_metric(r#"tgin_consumer_queue_depth{consumer="hooks"} 0"#);
    h.assert_metric(r#"tgin_consumer_leased{consumer="hooks"} 0"#);
    h.assert_metric(r#"tgin_consumer_in_flight{consumer="hooks"} 0"#);
}

#[tokio::test]
async fn retries_5xx_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    ok(&server).await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/a")],
        options(Balance::RoundRobin, 4, retry(5, 20, 5)),
    );
    h.admit(1, None);
    h.settled().await;

    let got = requests(&server).await;
    assert_eq!(
        got.iter().map(attempt_header).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(got.iter().all(|r| update_id(r) == 1));
    h.assert_metric(&format!("{} 2", h.attempts(0, "retry")));
    h.assert_metric(&format!("{} 1", h.attempts(0, "delivered")));
    h.assert_metric(&format!("{} 0", h.attempts(0, "dead_letter")));
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="hooks",reason="dead_letter"} 0"#);
}

#[tokio::test]
async fn honors_retry_after_on_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/a"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    // Retry-After above `retry.max` is capped at it.
    Mock::given(method("POST"))
        .and(path("/b"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3600"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    ok(&server).await;

    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/a")],
        options(Balance::RoundRobin, 4, retry(1, 2000, 5)),
    );
    let start = Instant::now();
    h.admit(1, None);
    h.settled().await;
    assert!(
        start.elapsed() >= Duration::from_secs(1),
        "retried after {:?}",
        start.elapsed()
    );
    assert_eq!(requests(&server).await.len(), 2);

    h.consumer.set_endpoints(
        vec![endpoint(&server, "/b")],
        options(Balance::RoundRobin, 4, retry(1, 50, 5)),
    );
    let start = Instant::now();
    h.admit(2, None);
    h.settled().await;
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "capped at retry.max, took {:?}",
        start.elapsed()
    );
    let on_b: Vec<u32> = requests(&server)
        .await
        .iter()
        .filter(|r| on(r, "/b"))
        .map(attempt_header)
        .collect();
    assert_eq!(on_b, [1, 2]);
}

#[tokio::test]
async fn permanent_4xx_dead_letters() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/hook/SECRETPATH")],
        options(Balance::RoundRobin, 4, retry(1, 5, 5)),
    );
    h.admit(4040, None);
    h.settled().await;

    assert_eq!(requests(&server).await.len(), 1, "a 4xx is not retried");
    h.assert_metric(&format!("{} 1", h.attempts(0, "dead_letter")));
    h.assert_metric(&format!("{} 0", h.attempts(0, "retry")));
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="hooks",reason="dead_letter"} 1"#);
    let stats = h.consumer.queue().stats();
    assert_eq!((stats.available, stats.leased), (0, 0));

    let logs = LOGS.text();
    let line = logs
        .lines()
        .find(|l| l.contains("update_id=4040"))
        .unwrap_or_else(|| panic!("no dead letter logged in {logs}"));
    for want in [
        "WARN",
        "consumer=hooks",
        "source=bot1",
        "update_type=message",
        "update_id=4040",
        "endpoint=0",
        "400",
    ] {
        assert!(line.contains(want), "{want:?} missing in {line}");
    }
    // Nothing any test logged carries a payload or an endpoint URL.
    for leak in [PAYLOAD_MARKER, "SECRETPATH", "/hook", "127.0.0.1"] {
        assert!(!logs.contains(leak), "{leak:?} leaked in {logs}");
    }
}

#[tokio::test]
async fn max_attempts_dead_letters() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/a")],
        options(Balance::RoundRobin, 4, retry(1, 5, 3)),
    );
    h.admit(1, None);
    h.settled().await;

    let got = requests(&server).await;
    assert_eq!(
        got.iter().map(attempt_header).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    h.assert_metric(&format!("{} 2", h.attempts(0, "retry")));
    h.assert_metric(&format!("{} 1", h.attempts(0, "dead_letter")));
    h.assert_metric(&format!("{} 0", h.attempts(0, "delivered")));
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="hooks",reason="dead_letter"} 1"#);
}

#[tokio::test]
async fn max_age_dead_letters() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let mut policy = retry(20, 20, 1000);
    policy.max_age = Duration::from_millis(300);
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/a")],
        options(Balance::RoundRobin, 4, policy),
    );
    let start = Instant::now();
    // Already older than `max_age`: dead-lettered without an attempt.
    h.admit_at(1, None, start - Duration::from_secs(60));
    h.admit_at(2, None, start);
    h.settled().await;

    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(250),
        "gave up after {elapsed:?}"
    );
    let got = requests(&server).await;
    assert!(
        got.iter().all(|r| update_id(r) == 2),
        "the stale update is never sent"
    );
    assert!(
        (2..1000).contains(&got.len()),
        "{} attempts: max_age, not max_attempts, ended them",
        got.len()
    );
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="hooks",reason="dead_letter"} 2"#);
    // The last attempt counts as a dead letter unless the deadline passed
    // while it waited to retry; either way each request counts once.
    let retries = h.value(&h.attempts(0, "retry"));
    let dead = h.value(&h.attempts(0, "dead_letter"));
    assert!(dead <= 1, "{dead}");
    assert_eq!(usize::try_from(retries + dead).expect("small"), got.len());
}

#[tokio::test]
async fn per_chat_ordering_holds_under_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains(r#""update_id":1,"#))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    ok(&server).await;
    let h = Harness::new(
        queue(Ordering::PerKey),
        vec![endpoint(&server, "/a")],
        options(Balance::RoundRobin, 8, retry(20, 40, 5)),
    );
    h.admit(1, Some(7));
    h.admit(2, Some(7));
    h.admit(3, Some(8));
    h.settled().await;

    let order: Vec<i64> = requests(&server).await.iter().map(update_id).collect();
    assert_eq!(order.iter().filter(|&&id| id == 1).count(), 3, "{order:?}");
    let last_of_1 = order.iter().rposition(|&id| id == 1).expect("1 sent");
    let first_of_2 = order.iter().position(|&id| id == 2).expect("2 sent");
    assert!(last_of_1 < first_of_2, "chat 7 out of order: {order:?}");
    assert!(order.contains(&3), "{order:?}");
}

#[tokio::test]
async fn round_robin_balances() {
    let server = MockServer::start().await;
    ok(&server).await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/a"), endpoint(&server, "/b")],
        options(Balance::RoundRobin, 1, retry(1, 5, 5)),
    );
    for id in 1..=6 {
        h.admit(id, None);
    }
    h.settled().await;

    let got = requests(&server).await;
    let paths: Vec<&str> = got.iter().map(|r| r.url.path()).collect();
    assert_eq!(paths, ["/a", "/b", "/a", "/b", "/a", "/b"]);
}

#[tokio::test]
async fn hash_chat_sticky() {
    let server = MockServer::start().await;
    ok(&server).await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![
            endpoint(&server, "/e0"),
            endpoint(&server, "/e1"),
            endpoint(&server, "/e2"),
        ],
        options(Balance::HashChat, 8, retry(1, 5, 5)),
    );
    for (id, key) in (1..).zip([0u64, 1, 2, 3, 4, 5, 0, 1, 2, 3, 4, 5, 1000, 1000]) {
        h.admit(id, Some(key));
    }
    h.settled().await;

    let got = requests(&server).await;
    assert_eq!(got.len(), 14);
    for req in &got {
        let key = json(req)["key"].as_u64().expect("key");
        assert_eq!(req.url.path(), format!("/e{}", key % 3), "key {key}");
    }
}

#[tokio::test]
async fn least_in_flight_prefers_idle() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(800)))
        .mount(&server)
        .await;
    ok(&server).await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/slow"), endpoint(&server, "/fast")],
        options(Balance::LeastInFlight, 2, retry(1, 5, 5)),
    );
    for id in 1..=4 {
        h.admit(id, None);
    }
    h.settled().await;

    let got = requests(&server).await;
    let slow = got.iter().filter(|r| on(r, "/slow")).count();
    let fast = got.iter().filter(|r| on(r, "/fast")).count();
    assert_eq!((slow, fast), (1, 3), "the busy endpoint gets one");
}

#[tokio::test]
async fn breaker_skips_failing_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/bad"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    ok(&server).await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![
            endpoint_with(&server, "/bad", breaker(1, Duration::from_secs(60))),
            endpoint_with(&server, "/good", breaker(1, Duration::from_secs(60))),
        ],
        options(Balance::RoundRobin, 1, retry(1, 5, 5)),
    );
    for id in 1..=6 {
        h.admit(id, None);
    }
    h.settled().await;

    let got = requests(&server).await;
    assert_eq!(
        got.iter().filter(|r| on(r, "/bad")).count(),
        1,
        "open after one"
    );
    assert_eq!(got.iter().filter(|r| on(r, "/good")).count(), 6);
    h.assert_metric(&format!("{} 1", h.attempts(0, "retry")));
    h.assert_metric(&format!("{} 6", h.attempts(1, "delivered")));
}

#[tokio::test]
async fn open_breakers_wait_for_reopen() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    ok(&server).await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint_with(
            &server,
            "/a",
            breaker(1, Duration::from_millis(400)),
        )],
        options(Balance::RoundRobin, 1, retry(1, 5, 5)),
    );
    let start = Instant::now();
    h.admit(1, None);
    h.settled().await;
    assert!(
        start.elapsed() >= Duration::from_millis(400),
        "probed after {:?}",
        start.elapsed()
    );
    assert_eq!(requests(&server).await.len(), 2);
}

#[tokio::test]
async fn max_in_flight_respected() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(300)))
        .mount(&server)
        .await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/a")],
        options(Balance::RoundRobin, 2, retry(1, 5, 5)),
    );
    for id in 1..=6 {
        h.admit(id, None);
    }
    wait_requests(&server, 2).await;
    // Responses take 300 ms, so no third request can have started yet.
    assert_eq!(requests(&server).await.len(), 2);
    assert_eq!(h.consumer.in_flight(), 2);
    let stats = h.consumer.queue().stats();
    assert_eq!((stats.available, stats.leased), (4, 2));
    h.assert_metric(r#"tgin_consumer_in_flight{consumer="hooks"} 2"#);
    h.assert_metric(r#"tgin_consumer_leased{consumer="hooks"} 2"#);
    h.assert_metric(r#"tgin_consumer_queue_depth{consumer="hooks"} 4"#);

    let start = Instant::now();
    h.settled().await;
    assert!(
        start.elapsed() >= Duration::from_millis(500),
        "two more rounds of two"
    );
    assert_eq!(requests(&server).await.len(), 6);
}

#[tokio::test]
async fn set_endpoints_live() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/old"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(400)))
        .mount(&server)
        .await;
    ok(&server).await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/old")],
        options(Balance::RoundRobin, 4, retry(1, 5, 5)),
    );
    h.admit(1, None);
    wait_requests(&server, 1).await;

    h.consumer.set_endpoints(
        vec![endpoint(&server, "/new1"), endpoint(&server, "/new2")],
        options(Balance::RoundRobin, 4, retry(1, 5, 5)),
    );
    h.admit(2, None);
    h.admit(3, None);
    h.settled().await;

    let got = requests(&server).await;
    let at = |id: i64| {
        got.iter()
            .filter(|r| update_id(r) == id)
            .map(|r| r.url.path().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(at(1), ["/old"], "in flight on the old set");
    let mut new: Vec<String> = [at(2), at(3)].concat();
    new.sort();
    assert_eq!(new, ["/new1", "/new2"]);
    h.assert_metric(&format!("{} 2", h.attempts(0, "delivered")));
    h.assert_metric(&format!("{} 1", h.attempts(1, "delivered")));
}

#[tokio::test]
async fn cancel_finishes_in_flight() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(300)))
        .mount(&server)
        .await;
    let mut h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/a")],
        options(Balance::RoundRobin, 1, retry(1, 5, 5)),
    );
    h.admit(1, None);
    h.admit(2, None);
    wait_requests(&server, 1).await;
    h.cancel.cancel();
    h.join().await;

    let got = requests(&server).await;
    assert_eq!(got.iter().map(update_id).collect::<Vec<_>>(), [1]);
    let stats = h.consumer.queue().stats();
    assert_eq!(
        (stats.available, stats.leased),
        (1, 0),
        "1 delivered, 2 left"
    );
    assert_eq!(h.consumer.in_flight(), 0);
}

#[tokio::test]
async fn force_releases_in_flight() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    ok(&server).await;
    let mut h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/a")],
        options(Balance::RoundRobin, 4, retry(1, 5, 5)),
    );
    h.admit(1, None);
    wait_requests(&server, 1).await;
    h.force.cancel();
    h.join().await;
    let stats = h.consumer.queue().stats();
    assert_eq!((stats.available, stats.leased), (1, 0), "lease released");
    assert_eq!(h.consumer.in_flight(), 0);

    // A new dispatcher picks the update up from the queue.
    h.spawn();
    h.settled().await;
    let got = requests(&server).await;
    assert_eq!(got.iter().map(update_id).collect::<Vec<_>>(), [1, 1]);
}

#[tokio::test]
async fn drain_keeps_dispatching() {
    let server = MockServer::start().await;
    ok(&server).await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/a")],
        options(Balance::RoundRobin, 4, retry(1, 5, 5)),
    );
    h.consumer.begin_drain();
    assert!(h.consumer.is_draining());
    h.admit(1, None);
    h.settled().await;
    assert_eq!(requests(&server).await.len(), 1);
}

#[tokio::test]
async fn cancel_stops_retrying() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "5"))
        .mount(&server)
        .await;
    let mut h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/a")],
        options(Balance::RoundRobin, 4, retry(1, 10_000, 5)),
    );
    h.admit(1, None);
    wait_requests(&server, 1).await;
    let start = Instant::now();
    h.cancel.cancel();
    h.join().await;
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "joined after {:?}",
        start.elapsed()
    );
    let stats = h.consumer.queue().stats();
    assert_eq!((stats.available, stats.leased), (1, 0), "back in the queue");
    assert_eq!(h.consumer.in_flight(), 0);
    assert_eq!(requests(&server).await.len(), 1, "no new attempt");
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="hooks",reason="dead_letter"} 0"#);
}

#[tokio::test]
async fn redirect_dead_letters() {
    let server = MockServer::start().await;
    Mock::given(path("/moved"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", format!("{}/a", server.uri())),
        )
        .mount(&server)
        .await;
    ok(&server).await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/moved")],
        options(Balance::RoundRobin, 4, retry(1, 5, 5)),
    );
    h.admit(3020, None);
    h.settled().await;

    let got = requests(&server).await;
    assert_eq!(got.len(), 1, "the redirect is not followed");
    h.assert_metric(&format!("{} 1", h.attempts(0, "dead_letter")));
    h.assert_metric(&format!("{} 0", h.attempts(0, "delivered")));
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="hooks",reason="dead_letter"} 1"#);
    let logs = LOGS.text();
    let line = logs
        .lines()
        .find(|l| l.contains("update_id=3020"))
        .unwrap_or_else(|| panic!("no dead letter logged in {logs}"));
    assert!(line.contains("redirect"), "{line}");
    assert!(line.contains("302"), "{line}");
}

#[tokio::test]
async fn open_breaker_admits_one_probe() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(300)))
        .mount(&server)
        .await;
    let open = breaker(1, Duration::from_millis(400));
    open.on_failure(std::time::Instant::now());
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint_with(&server, "/a", open)],
        options(Balance::RoundRobin, 4, retry(1, 5, 5)),
    );
    let start = Instant::now();
    for id in 1..=3 {
        h.admit(id, None);
    }
    wait_requests(&server, 1).await;
    // The probe takes 300 ms; nothing else may go out before it answers.
    assert_eq!(requests(&server).await.len(), 1);
    assert_eq!(h.consumer.in_flight(), 3, "the others wait, leased");
    h.settled().await;
    // Reopen (400 ms), the probe alone (300 ms), then the other two at once
    // (300 ms); without the single probe all three would end by ~700 ms.
    assert!(
        start.elapsed() >= Duration::from_millis(900),
        "settled after {:?}",
        start.elapsed()
    );
    assert_eq!(requests(&server).await.len(), 3);
}

#[tokio::test]
async fn force_during_backoff_keeps_chat_order() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains(r#""update_id":1,"#))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "5"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    ok(&server).await;
    let mut h = Harness::new(
        queue(Ordering::PerKey),
        vec![endpoint(&server, "/a")],
        options(Balance::RoundRobin, 4, retry(1, 10_000, 5)),
    );
    h.admit(1, Some(7));
    h.admit(2, Some(7));
    wait_requests(&server, 1).await;
    eventually("update 1 waits to retry", || {
        h.metrics()
            .lines()
            .any(|l| l == format!("{} 1", h.attempts(0, "retry")))
    })
    .await;
    h.force.cancel();
    h.join().await;
    let stats = h.consumer.queue().stats();
    assert_eq!((stats.available, stats.leased), (2, 0));

    h.spawn();
    h.settled().await;
    let order: Vec<i64> = requests(&server).await.iter().map(update_id).collect();
    assert_eq!(order, [1, 1, 2]);
}

#[tokio::test]
async fn runtime_endpoint_receives_updates_with_secret() {
    let server = MockServer::start().await;
    ok(&server).await;
    let configured = MockServer::start().await;
    ok(&configured).await;
    let h = Harness::new(
        queue(Ordering::None),
        Vec::new(),
        options(Balance::RoundRobin, 1, retry(10, 50, 5)),
    );
    assert!(h.consumer.runtime_endpoint_url().is_none());
    let set = runtime(&server, "/rt", Some("rt-secret_1"));
    let url = set.url.clone();
    h.consumer.set_runtime_endpoint(Some(set));
    assert_eq!(h.consumer.runtime_endpoint_url(), Some(url));
    assert_eq!(h.consumer.runtime_endpoint_owner(), Some((SourceId(0), 7)));
    h.admit(1, None);
    h.settled().await;
    let got = requests(&server).await;
    assert_eq!(got.len(), 1);
    assert_eq!(
        header(&got[0], "via"),
        Some(format!("1.1 {}", h.via.pseudonym()).as_str())
    );
    assert!(on(&got[0], "/rt"));
    assert_eq!(
        header(&got[0], "x-telegram-bot-api-secret-token"),
        Some("rt-secret_1")
    );
    let origin: Arc<str> = Arc::from(server.uri());
    assert_eq!(
        h.consumer.endpoints(),
        [(Arc::clone(&origin), BreakerState::Closed, true)]
    );
    h.assert_metric(
        r#"tgin_delivery_attempts_total{consumer="hooks",endpoint="runtime",outcome="delivered"} 1"#,
    );

    // A reload's new endpoint set, or new options, keep it: configured
    // endpoints first, the runtime one last.
    h.consumer.set_endpoints(
        vec![endpoint(&configured, "/cfg")],
        options(Balance::RoundRobin, 1, retry(10, 50, 5)),
    );
    h.consumer
        .set_options(options(Balance::RoundRobin, 1, retry(10, 50, 5)));
    assert_eq!(
        h.consumer.endpoints(),
        [
            (Arc::from(configured.uri()), BreakerState::Closed, false),
            (Arc::clone(&origin), BreakerState::Closed, true),
        ]
    );
    h.admit(2, None);
    h.admit(3, None);
    h.settled().await;
    assert_eq!(requests(&server).await.len(), 2);
    assert_eq!(requests(&configured).await.len(), 1);

    h.consumer.set_runtime_endpoint(None);
    assert!(h.consumer.runtime_endpoint_url().is_none());
    assert_eq!(
        h.consumer.endpoints(),
        [(Arc::from(configured.uri()), BreakerState::Closed, false)]
    );
    assert!(
        !LOGS.text().contains("rt-secret_1"),
        "the secret was logged"
    );
}

/// Push consumer with no endpoint at all (decision 8, R21): it leases
/// nothing, so updates admitted before a `setWebhook` stay queued, none
/// dead-lettered, and the dispatcher sleeps until an endpoint arrives. On
/// the paused clock the 50-minute sleep completes only if the dispatcher
/// idles: a busy loop would never let the clock advance, and the watchdog
/// on the real clock fails the test.
#[test]
fn updates_queued_before_set_webhook_are_delivered() {
    let (done, finished) = std::sync::mpsc::channel();
    let test = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("runtime");
        rt.block_on(async {
            let h = Harness::new(
                queue(Ordering::None),
                Vec::new(),
                options(Balance::RoundRobin, 4, retry(10, 50, 10)),
            );
            for id in 1..=3 {
                h.admit(id, None);
            }
            // Below the 1 h `max_age`, which still counts from admission.
            sleep(Duration::from_secs(50 * 60)).await;
            let stats = h.consumer.queue().stats();
            assert_eq!((stats.available, stats.leased), (3, 0), "nothing leased");
            assert_eq!(h.consumer.in_flight(), 0);
            let dead = r#"tgin_consumer_dropped_total{consumer="hooks",reason="dead_letter"}"#;
            assert_eq!(h.value(dead), 0);

            tokio::time::resume();
            let server = MockServer::start().await;
            ok(&server).await;
            h.consumer
                .set_runtime_endpoint(Some(runtime(&server, "/late", None)));
            let mut ids: Vec<i64> = wait_requests(&server, 3)
                .await
                .iter()
                .map(update_id)
                .collect();
            ids.sort_unstable();
            assert_eq!(ids, [1, 2, 3]);
            h.settled().await;
            assert_eq!(h.value(dead), 0, "none dead-lettered");

            // Without its endpoint again, it stops leasing: a new update
            // stays queued.
            h.consumer.set_runtime_endpoint(None);
            h.admit(4, None);
            tokio::task::yield_now().await;
            let stats = h.consumer.queue().stats();
            assert_eq!((stats.available, stats.leased), (1, 0));
        });
        let _ = done.send(());
    });
    match finished.recv_timeout(Duration::from_secs(60)) {
        Ok(()) => test.join().expect("the test thread"),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            std::panic::resume_unwind(test.join().expect_err("the test panicked"))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("the dispatcher never idled: a busy loop kept the paused clock still")
        }
    }
}

/// A 508 carrying the pushing process's own `Via` is this tgin refusing
/// its own push: the update is dead-lettered at once, never retried.
#[tokio::test]
async fn own_loop_detected_dead_letters_at_once() {
    let server = MockServer::start().await;
    // Answers as tgin's loop refusal does: 508, signed with the `Via` the
    // push came with.
    Mock::given(method("POST"))
        .respond_with(|req: &Request| {
            let via = req.headers.get("via").expect("a Via header").clone();
            ResponseTemplate::new(508).insert_header("via", via)
        })
        .mount(&server)
        .await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/self")],
        options(Balance::RoundRobin, 1, retry(10, 50, 5)),
    );
    h.admit(1, None);
    h.settled().await;
    assert_eq!(requests(&server).await.len(), 1, "never retried");
    h.assert_metric(&format!("{} 1", h.attempts(0, "dead_letter")));
    h.assert_metric(&format!("{} 0", h.attempts(0, "retry")));
}

/// Any other 508 (another server's, or another tgin's) is a transient 5xx:
/// retried like a 503.
#[tokio::test]
async fn other_loop_detected_is_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(508).insert_header("via", "1.1 tgin-0000000000000001"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(508))
        .up_to_n_times(1)
        .with_priority(2)
        .mount(&server)
        .await;
    ok(&server).await;
    let h = Harness::new(
        queue(Ordering::None),
        vec![endpoint(&server, "/elsewhere")],
        options(Balance::RoundRobin, 1, retry(10, 50, 5)),
    );
    h.admit(1, None);
    h.settled().await;
    assert_eq!(requests(&server).await.len(), 3, "two 508s, then delivered");
    h.assert_metric(&format!("{} 2", h.attempts(0, "retry")));
    h.assert_metric(&format!("{} 1", h.attempts(0, "delivered")));
    h.assert_metric(&format!("{} 0", h.attempts(0, "dead_letter")));
}

/// One reply a [`Recorder`] was handed.
#[derive(Debug, PartialEq)]
struct Handed {
    consumer: String,
    source: SourceId,
    content_type: String,
    body: Bytes,
    /// Updates the consumer had leased when the reply was handed over.
    leased: usize,
}

/// A reply executor that records what it is handed.
#[derive(Default)]
struct Recorder {
    consumer: OnceLock<Weak<PushConsumer>>,
    handed: Mutex<Vec<Handed>>,
    too_large: AtomicUsize,
}

impl ReplyExecutor for Recorder {
    fn execute<'a>(
        &'a self,
        consumer: &'a str,
        source: SourceId,
        reply: Reply,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let leased = self
            .consumer
            .get()
            .and_then(Weak::upgrade)
            .map_or(usize::MAX, |c| c.queue().stats().leased);
        self.handed.lock().expect("handed").push(Handed {
            consumer: consumer.to_owned(),
            source,
            content_type: reply.content_type.to_str().expect("ASCII").to_owned(),
            body: reply.body,
            leased,
        });
        Box::pin(async {})
    }

    fn too_large(&self, consumer: &str, _source: SourceId) {
        assert_eq!(consumer, "hooks");
        self.too_large.fetch_add(1, AtomicOrdering::SeqCst);
    }
}

/// A consumer delivering one update at a time to `endpoints`, whose
/// replies go to a new [`Recorder`].
fn replying(endpoints: Vec<Endpoint>) -> (Harness, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let handle: ReplyHandle = Arc::default();
    let executor: Arc<dyn ReplyExecutor> = recorder.clone();
    assert!(handle.set(executor).is_ok());
    let mut opts = options(Balance::RoundRobin, 1, retry(10, 50, 5));
    opts.reply = Some(handle);
    let h = Harness::new(queue(Ordering::None), endpoints, opts);
    assert!(recorder.consumer.set(Arc::downgrade(&h.consumer)).is_ok());
    (h, recorder)
}

/// The endpoint answers the update `update_id` with `response`.
async fn answer(server: &MockServer, update_id: i64, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(body_string_contains(format!(r#""update_id":{update_id},"#)))
        .respond_with(response)
        .mount(server)
        .await;
}

/// A reply is read from a 2xx answer whose type names parameters, and
/// handed over while its update is still leased: not from another type,
/// an empty body or a 204; a body over the limit is reported, not handed.
#[tokio::test]
async fn reply_handed_over_before_ack_by_type_and_size() {
    let server = MockServer::start().await;
    let json = r#"{"method":"sendMessage","chat_id":1,"text":"a"}"#;
    let upload_type = "multipart/form-data; boundary=b";
    let upload =
        "--b\r\nContent-Disposition: form-data; name=\"method\"\r\n\r\nsendDice\r\n--b--\r\n";
    let ok = || ResponseTemplate::new(200);
    answer(&server, 1, ok().set_body_raw(json, "application/json")).await;
    answer(&server, 2, ok().set_body_raw(json, "text/plain")).await;
    answer(&server, 3, ok().set_body_raw("", "application/json")).await;
    let over = format!("{json}{}", " ".repeat(MAX_REPLY_BYTES + 1 - json.len()));
    answer(&server, 4, ok().set_body_raw(over, "application/json")).await;
    answer(&server, 5, ResponseTemplate::new(204)).await;
    answer(
        &server,
        6,
        ResponseTemplate::new(202).set_body_raw(upload, upload_type),
    )
    .await;
    let (h, recorder) = replying(vec![endpoint(&server, "/hook")]);
    for id in 1..=6 {
        h.admit(id, None);
    }
    h.settled().await;

    let handed = std::mem::take(&mut *recorder.handed.lock().expect("handed"));
    let expected = |content_type: &str, body: &str| Handed {
        consumer: "hooks".to_owned(),
        source: SourceId(0),
        content_type: content_type.to_owned(),
        body: Bytes::from(body.to_owned()),
        leased: 1,
    };
    assert_eq!(
        handed,
        [
            expected("application/json", json),
            expected(upload_type, upload)
        ]
    );
    assert_eq!(recorder.too_large.load(AtomicOrdering::SeqCst), 1);
    h.assert_metric(&format!("{} 6", h.attempts(0, "delivered")));
}

/// Serves every push with a chunked answer of `application/json`: for the
/// update 1, `MAX_REPLY_BYTES + 1` bytes in 64 KiB chunks; for any other,
/// the three chunks of [`chunked_reply`]. Blocking sockets on threads of
/// their own, so the test runtime needs no I/O features.
fn chunked_endpoint() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("address");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            std::thread::spawn(move || answer_chunked(stream));
        }
    });
    format!("http://{addr}")
}

fn answer_chunked(mut stream: std::net::TcpStream) {
    use std::io::{Read, Write};
    let mut request = Vec::new();
    let body = loop {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..n]);
        let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map_or(0, |v| v.trim().parse().expect("length"));
        if request.len() >= end + 4 + length {
            break request[end + 4..].to_vec();
        }
    };
    let chunks: Vec<Vec<u8>> = if body.starts_with(br#"{"update_id":1,"#) {
        let mut left = MAX_REPLY_BYTES + 1;
        let mut chunks = Vec::new();
        while left > 0 {
            let n = left.min(64 * 1024);
            chunks.push(vec![b' '; n]);
            left -= n;
        }
        chunks
    } else {
        chunked_reply().into_iter().map(Vec::from).collect()
    };
    let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for chunk in chunks {
        out.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        out.extend_from_slice(&chunk);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    // tgin stops reading an answer over the limit.
    let _ = stream.write_all(&out);
}

/// The chunks of a reply within the limit.
fn chunked_reply() -> [String; 3] {
    [
        r#"{"method":"sendMessage","#.to_owned(),
        format!(r#""text":"{}""#, "x".repeat(100 * 1024)),
        "}".to_owned(),
    ]
}

/// Without a `Content-Length`, a reply is read chunk by chunk until it
/// passes the limit; one within it arrives whole.
#[tokio::test]
async fn chunked_reply_bounded_without_length() {
    let base = chunked_endpoint();
    let endpoint = Endpoint {
        url: format!("{base}/hook").parse().expect("url"),
        origin: Arc::from(base.as_str()),
        secret_token: None,
        timeout: Duration::from_secs(5),
        breaker: breaker(1000, Duration::from_secs(60)),
    };
    let (h, recorder) = replying(vec![endpoint]);
    h.admit(1, None);
    h.admit(2, None);
    h.settled().await;

    assert_eq!(recorder.too_large.load(AtomicOrdering::SeqCst), 1);
    let handed = recorder.handed.lock().expect("handed");
    let [reply] = &handed[..] else {
        panic!("one reply handed over: {}", handed.len());
    };
    assert_eq!(reply.body, Bytes::from(chunked_reply().concat()));
    h.assert_metric(&format!("{} 2", h.attempts(0, "delivered")));
}
