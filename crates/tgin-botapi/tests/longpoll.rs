//! The long-poll source against a wiremock Telegram and a recording sink.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::Mutex;
use prometheus_client::encoding::text::encode;
use prometheus_client::registry::Registry;
use serde_json::{Value, json};
use tgin_botapi::{BotClient, BotUser, LongPollOptions, LongPollSource};
use tgin_core::facts::Facts;
use tgin_core::ids::SourceId;
use tgin_core::metrics::CoreMetrics;
use tgin_core::sink::{Admit, Sink};
use tgin_runtime::secret::Secret;
use tgin_runtime::supervisor::Exit;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TOKEN: &str = "123456789:AAAAexampleSecretTokenValue_-xyz";
const SOURCE: SourceId = SourceId(7);
/// Bounds every wait, so a source that stalls fails the test instead of
/// hanging it.
const PATIENCE: Duration = Duration::from_secs(10);
/// Long enough that a test waiting this long times out first.
const FOREVER: Duration = Duration::from_secs(3600);

const WEBHOOK_ACTIVE: &str = "Conflict: can't use getUpdates method while webhook is active; use deleteWebhook to delete the webhook first";
const OTHER_POLLER: &str = "Conflict: terminated by other getUpdates request; make sure that only one bot instance is running";

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn at(m: &str) -> String {
    format!("/bot{TOKEN}/{m}")
}

/// One update with a private chat; its exact bytes are what the sink must see.
fn update(id: i64) -> String {
    format!(
        r#"{{"update_id":{id},"message":{{"message_id":{id},"chat":{{"id":500,"type":"private"}},"text":"hi {id}"}}}}"#
    )
}

fn batch(ids: &[i64]) -> ResponseTemplate {
    let updates: Vec<String> = ids.iter().map(|&id| update(id)).collect();
    ResponseTemplate::new(200).set_body_raw(
        format!(r#"{{"ok":true,"result":[{}]}}"#, updates.join(",")),
        "application/json",
    )
}

fn ok(result: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": result }))
}

fn api_error(status: u16, description: &str, retry_after: Option<u64>) -> ResponseTemplate {
    let mut body = json!({ "ok": false, "error_code": status, "description": description });
    if let Some(secs) = retry_after {
        body["parameters"] = json!({ "retry_after": secs });
    }
    ResponseTemplate::new(status).set_body_json(body)
}

/// Defaults that make a wrong wait obvious: a source that waits out the
/// conflict backoff where it should not never reaches the next step.
fn options() -> LongPollOptions {
    LongPollOptions {
        delete_webhook_on_start: false,
        backoff: (ms(1), ms(10), ms(1)),
        conflict_backoff: FOREVER,
        admit_retry: ms(5),
        ..LongPollOptions::default()
    }
}

/// One `getUpdates` request as the server saw it.
#[derive(Debug)]
struct Poll {
    body: Value,
    at: Instant,
}

/// Answers `getUpdates` from a script; once it runs out, each poll hangs
/// like an idle long poll.
struct Script {
    replies: Mutex<VecDeque<ResponseTemplate>>,
    polls: mpsc::UnboundedSender<Poll>,
}

impl Respond for Script {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let _ = self.polls.send(Poll {
            body,
            at: Instant::now(),
        });
        self.replies.lock().pop_front().unwrap_or_else(|| {
            ResponseTemplate::new(200)
                .set_body_json(json!({ "ok": true, "result": [] }))
                .set_delay(FOREVER)
        })
    }
}

struct Telegram {
    server: MockServer,
    polls: mpsc::UnboundedReceiver<Poll>,
}

impl Telegram {
    async fn start(replies: Vec<ResponseTemplate>) -> Self {
        let me =
            ok(json!({ "id": 42, "is_bot": true, "first_name": "Bot", "username": "tgin_bot" }));
        Self::start_with(me, replies).await
    }

    async fn start_with(get_me: ResponseTemplate, replies: Vec<ResponseTemplate>) -> Self {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(at("getMe")))
            .respond_with(get_me)
            .mount(&server)
            .await;
        let (tx, rx) = mpsc::unbounded_channel();
        Mock::given(method("POST"))
            .and(path(at("getUpdates")))
            .respond_with(Script {
                replies: Mutex::new(replies.into()),
                polls: tx,
            })
            .mount(&server)
            .await;
        Self { server, polls: rx }
    }

    /// `deleteWebhook` keeping pending updates answers `reply`.
    async fn delete_webhook(&self, reply: ResponseTemplate, times: Option<u64>) {
        let mock = Mock::given(method("POST"))
            .and(path(at("deleteWebhook")))
            .and(body_json(json!({ "drop_pending_updates": false })))
            .respond_with(reply);
        let mock = match times {
            Some(n) => mock.up_to_n_times(n),
            None => mock,
        };
        mock.mount(&self.server).await;
    }

    fn client(&self) -> BotClient {
        // `no_proxy`: proxy variables in the test environment must not
        // reroute requests meant for the local mock server.
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        BotClient::new(
            http,
            &self.server.uri().parse().unwrap(),
            &Secret::new(TOKEN.to_owned()),
        )
        .unwrap()
    }

    async fn next_poll(&mut self) -> Poll {
        timeout(PATIENCE, self.polls.recv())
            .await
            .expect("timed out waiting for getUpdates")
            .expect("the mock server is gone")
    }

    /// The methods called so far, in arrival order.
    async fn methods(&self) -> Vec<String> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| {
                r.url
                    .path()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }

    async fn count(&self, method: &str) -> usize {
        self.methods().await.iter().filter(|m| *m == method).count()
    }
}

#[derive(Clone, Debug)]
struct Call {
    source: SourceId,
    update_id: i64,
    payload: Bytes,
    update_type: String,
    chat_id: Option<i64>,
    result: Admit,
}

/// A sink that answers from `policy` and records every offer.
struct Recorder {
    policy: Box<dyn Fn(i64) -> Admit + Send + Sync>,
    calls: Mutex<Vec<Call>>,
    count: watch::Sender<usize>,
}

impl Recorder {
    fn new(policy: impl Fn(i64) -> Admit + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            policy: Box::new(policy),
            calls: Mutex::default(),
            count: watch::Sender::new(0),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().clone()
    }

    fn ids(&self) -> Vec<i64> {
        self.calls().iter().map(|c| c.update_id).collect()
    }

    async fn wait_calls(&self, n: usize) {
        let mut rx = self.count.subscribe();
        timeout(PATIENCE, rx.wait_for(|c| *c >= n))
            .await
            .expect("timed out waiting for admits")
            .unwrap();
    }
}

impl Sink for Recorder {
    fn admit(&self, source: SourceId, update_id: i64, payload: Bytes, facts: &Facts<'_>) -> Admit {
        let result = (self.policy)(update_id);
        self.calls.lock().push(Call {
            source,
            update_id,
            payload,
            update_type: facts.update_type.to_owned(),
            chat_id: facts.chat_id,
            result,
        });
        self.count.send_modify(|c| *c += 1);
        result
    }
}

type Identities = Arc<Mutex<Vec<(SourceId, BotUser)>>>;

struct Fixture {
    registry: Registry,
    source: Arc<LongPollSource<Recorder>>,
    identities: Identities,
}

impl Fixture {
    fn new(tg: &Telegram, sink: Arc<Recorder>, opts: LongPollOptions) -> Self {
        let mut registry = Registry::with_prefix("tgin");
        let metrics = CoreMetrics::register(&mut registry).source("lp");
        let identities = Identities::default();
        let seen = Arc::clone(&identities);
        let source = LongPollSource::new(
            SOURCE,
            Arc::from("lp"),
            tg.client(),
            opts,
            sink,
            metrics,
            Arc::new(move |id: SourceId, user: &BotUser| {
                seen.lock().push((id, user.clone()));
            }),
        );
        Self {
            registry,
            source: Arc::new(source),
            identities,
        }
    }

    fn spawn(&self) -> (CancellationToken, JoinHandle<Exit>) {
        let ingest = CancellationToken::new();
        let source = Arc::clone(&self.source);
        let token = ingest.clone();
        (ingest, tokio::spawn(async move { source.run(token).await }))
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

/// Cancels `ingest` and expects the source to stop cleanly.
async fn stop(ingest: CancellationToken, handle: JoinHandle<Exit>) {
    ingest.cancel();
    let exit = timeout(PATIENCE, handle)
        .await
        .expect("the source did not stop")
        .expect("the source panicked");
    assert_eq!(exit, Exit::Done);
}

#[tokio::test]
async fn admits_in_order_and_confirms_offset() {
    let mut tg = Telegram::start(vec![batch(&[10, 11, 12])]).await;
    let sink = Recorder::new(|id| {
        if id == 11 {
            Admit::Unroutable
        } else {
            Admit::Admitted
        }
    });
    let fx = Fixture::new(&tg, Arc::clone(&sink), options());
    let (ingest, handle) = fx.spawn();

    let first = tg.next_poll().await;
    assert_eq!(
        first.body,
        json!({ "offset": 0, "limit": 100, "timeout": 30 })
    );
    // An unroutable update counts as admitted, so the offset passes it.
    let second = tg.next_poll().await;
    assert_eq!(second.body["offset"], 13);

    let calls = sink.calls();
    let ids: Vec<i64> = calls.iter().map(|c| c.update_id).collect();
    assert_eq!(ids, [10, 11, 12]);
    for call in &calls {
        assert_eq!(call.source, SOURCE);
        assert_eq!(call.payload, Bytes::from(update(call.update_id)));
        assert_eq!(call.update_type, "message");
        assert_eq!(call.chat_id, Some(500));
    }
    fx.assert_metrics(&[
        r#"tgin_source_updates_total{source="lp"} 3"#,
        r#"tgin_routes_unmatched_total{source="lp"} 1"#,
        r#"tgin_source_state{source="lp",state="running"} 1"#,
    ]);

    stop(ingest, handle).await;
    fx.assert_metrics(&[r#"tgin_source_state{source="lp",state="stopped"} 1"#]);
}

#[tokio::test]
async fn backpressure_holds_batch_and_stops_polling() {
    let mut tg = Telegram::start(vec![batch(&[1, 2, 3])]).await;
    let full = Arc::new(AtomicBool::new(true));
    let queue = Arc::clone(&full);
    let sink = Recorder::new(move |id| {
        if id == 2 && queue.load(Ordering::SeqCst) {
            Admit::Backpressure
        } else {
            Admit::Admitted
        }
    });
    let fx = Fixture::new(&tg, Arc::clone(&sink), options());
    let (ingest, handle) = fx.spawn();

    assert_eq!(tg.next_poll().await.body["offset"], 0);
    // Update 1, then update 2 offered again and again.
    sink.wait_calls(6).await;
    assert!(
        tg.polls.try_recv().is_err(),
        "polled while a batch was held"
    );
    assert_eq!(tg.count("getUpdates").await, 1);

    full.store(false, Ordering::SeqCst);
    assert_eq!(tg.next_poll().await.body["offset"], 4);
    assert_eq!(tg.count("getUpdates").await, 2);

    let calls = sink.calls();
    let ids: Vec<i64> = calls.iter().map(|c| c.update_id).collect();
    let (first, rest) = ids.split_first().unwrap();
    let (last, held) = rest.split_last().unwrap();
    assert_eq!((*first, *last), (1, 3), "{ids:?}");
    assert!(held.len() >= 6 && held.iter().all(|&id| id == 2), "{ids:?}");
    // Every retry offers the same bytes; only the last offer of 2 lands.
    let retries = &calls[1..calls.len() - 1];
    assert!(retries.iter().all(|c| c.payload == update(2)));
    let (admitted, refused) = retries.split_last().unwrap();
    assert_eq!(admitted.result, Admit::Admitted);
    assert!(refused.iter().all(|c| c.result == Admit::Backpressure));
    fx.assert_metrics(&[r#"tgin_source_updates_total{source="lp"} 3"#]);

    stop(ingest, handle).await;
}

#[tokio::test]
async fn fatal_on_401() {
    // getMe rejects the token; the description echoes it.
    let tg = Telegram::start_with(
        api_error(401, &format!("Unauthorized: bot{TOKEN}"), None),
        vec![],
    )
    .await;
    let fx = Fixture::new(&tg, Recorder::new(|_| Admit::Admitted), options());
    let exit = timeout(PATIENCE, fx.source.run(CancellationToken::new()))
        .await
        .expect("getMe 401 did not end the run");
    let Exit::Fatal(message) = exit else {
        panic!("expected Fatal, got {exit:?}");
    };
    assert!(!message.contains(TOKEN), "{message}");
    assert_eq!(tg.count("getUpdates").await, 0);
    fx.assert_metrics(&[
        r#"tgin_source_errors_total{source="lp",reason="unauthorized"} 1"#,
        r#"tgin_source_state{source="lp",state="failed"} 1"#,
    ]);
    assert!(fx.identities.lock().is_empty());

    // getUpdates rejects it after a good getMe.
    for status in [401, 403, 404] {
        let tg = Telegram::start(vec![api_error(status, &format!("rejected {TOKEN}"), None)]).await;
        let fx = Fixture::new(&tg, Recorder::new(|_| Admit::Admitted), options());
        let exit = timeout(PATIENCE, fx.source.run(CancellationToken::new()))
            .await
            .unwrap_or_else(|_| panic!("getUpdates {status} did not end the run"));
        let Exit::Fatal(message) = exit else {
            panic!("{status}: expected Fatal, got {exit:?}");
        };
        assert!(!message.contains(TOKEN), "{message}");
        assert_eq!(tg.count("getUpdates").await, 1, "{status}");
        fx.assert_metrics(&[
            r#"tgin_source_errors_total{source="lp",reason="unauthorized"} 1"#,
            r#"tgin_source_state{source="lp",state="failed"} 1"#,
        ]);
    }
}

#[tokio::test]
async fn get_me_learns_username() {
    let mut tg = Telegram::start(vec![]).await;
    let fx = Fixture::new(&tg, Recorder::new(|_| Admit::Admitted), options());
    let bot = BotUser {
        id: 42,
        username: Some("tgin_bot".into()),
    };

    let (ingest, handle) = fx.spawn();
    tg.next_poll().await;
    assert_eq!(*fx.identities.lock(), [(SOURCE, bot.clone())]);
    fx.assert_metrics(&[r#"tgin_source_state{source="lp",state="running"} 1"#]);
    stop(ingest, handle).await;

    // A restarted attempt asks again and reports the identity again.
    let (ingest, handle) = fx.spawn();
    tg.next_poll().await;
    assert_eq!(
        *fx.identities.lock(),
        [(SOURCE, bot.clone()), (SOURCE, bot)]
    );
    assert_eq!(tg.count("getMe").await, 2);
    stop(ingest, handle).await;
}

#[tokio::test]
async fn deletes_webhook_on_start() {
    let mut tg = Telegram::start(vec![]).await;
    tg.delete_webhook(ok(json!(true)), None).await;
    let opts = LongPollOptions {
        delete_webhook_on_start: true,
        ..options()
    };
    let fx = Fixture::new(&tg, Recorder::new(|_| Admit::Admitted), opts);
    let (ingest, handle) = fx.spawn();

    tg.next_poll().await;
    assert_eq!(tg.methods().await, ["getMe", "deleteWebhook", "getUpdates"]);
    stop(ingest, handle).await;
}

#[tokio::test]
async fn deletes_webhook_on_409_once() {
    let conflict = Duration::from_secs(1);
    let mut tg = Telegram::start(vec![
        api_error(409, WEBHOOK_ACTIVE, None),
        batch(&[5]),
        api_error(409, WEBHOOK_ACTIVE, None),
        batch(&[6]),
    ])
    .await;
    tg.delete_webhook(ok(json!(true)), None).await;
    // Any error backoff would outlast the test.
    let opts = LongPollOptions {
        backoff: (FOREVER, FOREVER, FOREVER),
        conflict_backoff: conflict,
        ..options()
    };
    let fx = Fixture::new(&tg, Recorder::new(|_| Admit::Admitted), opts);
    let (ingest, handle) = fx.spawn();

    let mut polls = Vec::new();
    for _ in 0..5 {
        polls.push(tg.next_poll().await);
    }
    let offsets: Vec<&Value> = polls.iter().map(|p| &p.body["offset"]).collect();
    assert_eq!(offsets, [0, 0, 6, 6, 7]);
    // The first 409 deletes the webhook and polls again at once; the second,
    // within a minute of that delete, means another process set the webhook
    // again, so tgin waits instead of fighting it.
    assert_eq!(tg.count("deleteWebhook").await, 1);
    assert!(polls[1].at - polls[0].at < conflict, "{polls:?}");
    assert!(polls[3].at - polls[2].at >= conflict, "{polls:?}");
    fx.assert_metrics(&[
        r#"tgin_source_errors_total{source="lp",reason="conflict"} 2"#,
        r#"tgin_source_updates_total{source="lp"} 2"#,
    ]);

    stop(ingest, handle).await;
}

#[tokio::test]
async fn retries_failed_webhook_delete() {
    let mut tg = Telegram::start(vec![
        api_error(409, WEBHOOK_ACTIVE, None),
        api_error(409, WEBHOOK_ACTIVE, None),
        batch(&[1]),
    ])
    .await;
    tg.delete_webhook(api_error(500, "Internal Server Error", None), Some(1))
        .await;
    tg.delete_webhook(ok(json!(true)), None).await;
    // A failed delete arms no guard: the next 409 deletes again instead of
    // waiting out the conflict backoff.
    let fx = Fixture::new(&tg, Recorder::new(|_| Admit::Admitted), options());
    let (ingest, handle) = fx.spawn();

    let mut offsets = Vec::new();
    for _ in 0..4 {
        offsets.push(tg.next_poll().await.body["offset"].clone());
    }
    assert_eq!(offsets, [0, 0, 0, 2]);
    assert_eq!(tg.count("deleteWebhook").await, 2);
    fx.assert_metrics(&[
        r#"tgin_source_errors_total{source="lp",reason="conflict"} 2"#,
        r#"tgin_source_errors_total{source="lp",reason="http"} 1"#,
    ]);

    stop(ingest, handle).await;
}

#[tokio::test]
async fn other_409_backs_off() {
    let conflict = ms(300);
    let mut tg = Telegram::start(vec![api_error(409, OTHER_POLLER, None), batch(&[1])]).await;
    tg.delete_webhook(ok(json!(true)), None).await;
    let opts = LongPollOptions {
        backoff: (FOREVER, FOREVER, FOREVER),
        conflict_backoff: conflict,
        ..options()
    };
    let fx = Fixture::new(&tg, Recorder::new(|_| Admit::Admitted), opts);
    let (ingest, handle) = fx.spawn();

    let polls = [
        tg.next_poll().await,
        tg.next_poll().await,
        tg.next_poll().await,
    ];
    assert!(polls[1].at - polls[0].at >= conflict, "{polls:?}");
    assert_eq!(polls[2].body["offset"], 2);
    assert_eq!(tg.count("deleteWebhook").await, 0);
    fx.assert_metrics(&[r#"tgin_source_errors_total{source="lp",reason="conflict"} 1"#]);

    stop(ingest, handle).await;
}

#[tokio::test]
async fn honors_retry_after() {
    let mut tg = Telegram::start(vec![
        api_error(429, "Too Many Requests: retry after 1", Some(1)),
        batch(&[1]),
    ])
    .await;
    let fx = Fixture::new(&tg, Recorder::new(|_| Admit::Admitted), options());
    let (ingest, handle) = fx.spawn();

    let polls = [
        tg.next_poll().await,
        tg.next_poll().await,
        tg.next_poll().await,
    ];
    assert!(
        polls[1].at - polls[0].at >= Duration::from_secs(1),
        "{polls:?}"
    );
    assert_eq!(polls[2].body["offset"], 2);
    fx.assert_metrics(&[r#"tgin_source_errors_total{source="lp",reason="rate_limited"} 1"#]);

    stop(ingest, handle).await;
}

#[tokio::test]
async fn transient_errors_back_off_and_continue() {
    let mut tg = Telegram::start(vec![
        api_error(502, "Bad Gateway", None),
        ResponseTemplate::new(200).set_body_raw("<html>oops</html>", "text/html"),
        batch(&[1]),
    ])
    .await;
    let fx = Fixture::new(&tg, Recorder::new(|_| Admit::Admitted), options());
    let (ingest, handle) = fx.spawn();

    let mut offsets = Vec::new();
    for _ in 0..4 {
        offsets.push(tg.next_poll().await.body["offset"].clone());
    }
    assert_eq!(offsets, [0, 0, 0, 2]);
    assert!(!handle.is_finished());
    fx.assert_metrics(&[
        r#"tgin_source_errors_total{source="lp",reason="http"} 1"#,
        r#"tgin_source_errors_total{source="lp",reason="decode"} 1"#,
        r#"tgin_source_state{source="lp",state="running"} 1"#,
    ]);

    stop(ingest, handle).await;
}

#[tokio::test]
async fn ingest_cancel_stops_promptly() {
    let mut tg = Telegram::start(vec![batch(&[1, 2])]).await;
    let sink = Recorder::new(|_| Admit::Backpressure);
    let opts = LongPollOptions {
        admit_retry: FOREVER,
        ..options()
    };
    let fx = Fixture::new(&tg, Arc::clone(&sink), opts);
    let (ingest, handle) = fx.spawn();

    tg.next_poll().await;
    sink.wait_calls(1).await;
    // The source now waits on timers only, so the paused clock is safe: if
    // it waited for any timer, the clock would jump forward to it.
    tokio::time::pause();
    let start = tokio::time::Instant::now();
    ingest.cancel();
    let exit = timeout(PATIENCE, handle)
        .await
        .expect("the source did not stop")
        .expect("the source panicked");
    assert_eq!(exit, Exit::Done);
    assert_eq!(start.elapsed(), Duration::ZERO);
    // The held update was never confirmed: no further poll carried its offset.
    assert_eq!(sink.ids(), [1]);
    assert!(tg.polls.try_recv().is_err());
    fx.assert_metrics(&[r#"tgin_source_state{source="lp",state="stopped"} 1"#]);
}
