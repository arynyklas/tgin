//! The engine: admission, hot reload, control and status. Time logic runs on
//! tokio's paused clock; tests that reach a wiremock endpoint run in real
//! time with bounded waits.

use std::io;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use bytes::Bytes;
use prometheus_client::encoding::text::encode;
use prometheus_client::registry::Registry;
use tgin_core::config::{self, Config};
use tgin_core::consumer::{PollRequest, PullConsumer};
use tgin_core::engine::{
    BuildError, Consumer, Engine, EngineRuntime, Feed, TokenChanged, WebhookChange,
};
use tgin_core::facts::Facts;
use tgin_core::ids::SourceId;
use tgin_core::metrics::CoreMetrics;
use tgin_core::sink::{Admit, BatchUpdate, Sink};
use tgin_runtime::breaker::BreakerState;
use tgin_runtime::config::MapEnv;
use tgin_runtime::queue::{HolderId, QueueStats};
use tgin_runtime::secret::Secret;
use tgin_runtime::supervisor::{Supervisor, TaskState};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Bound on every wait for a condition.
const WAIT: Duration = Duration::from_secs(10);

/// Everything every test logs. A global subscriber, since a scoped one can
/// miss events while tests on other threads register callsites. Tests that
/// look at logs use names no other test uses.
static LOGS: LazyLock<Logs> = LazyLock::new(|| {
    let logs = Logs::default();
    tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .try_init()
        .expect("the only global subscriber");
    logs
});

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    /// Lines that contain every one of `needles`.
    fn count(&self, needles: &[&str]) -> usize {
        let bytes = self.0.lock().expect("logs").clone();
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|l| needles.iter().all(|n| l.contains(n)))
            .count()
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

/// `message` goes to both pull consumers, `callback_query` to `p1` only and
/// `edited_message` to `p2` only. `p1` refuses when full; `p2` evicts.
const BASE: &str = r#"version: 1
server: {}
reload: { drain_timeout: 2s }
bots:
  - name: a
    token: "1:aaa"
    routes:
      - match: { update_types: [message] }
        to: [{ all: [p1, p2] }]
      - match: { update_types: [callback_query] }
        to: [p1]
      - match: { update_types: [edited_message] }
        to: [p2]
consumers:
  p1: { pull: { capacity: 1 } }
  p2: { pull: { capacity: 1, on_full: drop_oldest } }
"#;

fn load(yaml: &str) -> Config {
    config::load(yaml, Path::new("tgin.yaml"), &MapEnv::default()).unwrap_or_else(|errors| {
        panic!(
            "{}",
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
    })
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("client")
}

struct Harness {
    reg: Registry,
    engine: Engine,
    supervisor: Arc<Supervisor>,
    force: CancellationToken,
}

impl Harness {
    fn new(yaml: &str) -> Self {
        LazyLock::force(&LOGS);
        let mut reg = Registry::with_prefix("tgin");
        let metrics = CoreMetrics::register(&mut reg);
        let supervisor = Arc::new(Supervisor::new());
        let force = CancellationToken::new();
        let rt = EngineRuntime {
            supervisor: Arc::clone(&supervisor),
            stop: CancellationToken::new(),
            force: force.clone(),
            via: tgin_runtime::http::Via::generate(),
            reply: Default::default(),
        };
        let engine = Engine::new(&load(yaml), metrics, client(), rt).expect("engine");
        Self {
            reg,
            engine,
            supervisor,
            force,
        }
    }

    fn admit(&self, update_type: &str, update_id: i64) -> Admit {
        admit_to(&self.engine, SourceId(0), update_type, update_id)
    }

    fn pull(&self, name: &str) -> Arc<PullConsumer> {
        match self.engine.consumer(name) {
            Some(Consumer::Pull(c)) => c,
            other => panic!("{name} is no pull consumer: {}", other.is_some()),
        }
    }

    fn stats(&self, name: &str) -> QueueStats {
        match self.engine.consumer(name).expect("consumer") {
            Consumer::Pull(c) => c.queue().stats(),
            Consumer::Push(c) => c.queue().stats(),
        }
    }

    fn metrics(&self) -> String {
        let mut s = String::new();
        encode(&mut s, &self.reg).expect("encode");
        s
    }

    #[track_caller]
    fn assert_metric(&self, line: &str) {
        let out = self.metrics();
        assert!(out.lines().any(|l| l == line), "missing {line:?} in\n{out}");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.force.cancel();
    }
}

fn payload(update_type: &str, update_id: i64) -> Bytes {
    Bytes::from(format!(
        r#"{{"update_id":{update_id},"{update_type}":{{"chat":{{"id":7}}}}}}"#
    ))
}

fn admit_to(engine: &Engine, source: SourceId, update_type: &str, update_id: i64) -> Admit {
    let facts = Facts {
        update_type,
        chat_id: Some(7),
        ..Facts::default()
    };
    engine.admit(source, update_id, payload(update_type, update_id), &facts)
}

fn req(instance: &str, offset: i64, wait: Duration) -> PollRequest {
    PollRequest {
        instance: HolderId(Arc::from(instance)),
        offset,
        limit: 100,
        timeout: wait,
    }
}

/// Polls without waiting; returns the update ids handed out.
async fn poll(c: &PullConsumer, instance: &str, offset: i64) -> Vec<i64> {
    let resp = c
        .poll(
            req(instance, offset, Duration::ZERO),
            &CancellationToken::new(),
        )
        .await;
    resp.items
        .iter()
        .map(|p| {
            let v: serde_json::Value = serde_json::from_slice(p).expect("json");
            v["update_id"].as_i64().expect("update_id")
        })
        .collect()
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

#[tokio::test(start_paused = true)]
async fn admit_fanout_commits_all() {
    let h = Harness::new(BASE);
    assert_eq!(h.admit("message", 1), Admit::Admitted);
    assert_eq!(poll(&h.pull("p1"), "i", 0).await, [1]);
    assert_eq!(poll(&h.pull("p2"), "i", 0).await, [1]);
}

#[tokio::test(start_paused = true)]
async fn admit_backpressure_reserves_nothing() {
    let h = Harness::new(BASE);
    assert_eq!(h.admit("callback_query", 1), Admit::Admitted);
    assert_eq!(h.admit("message", 2), Admit::Backpressure);
    let p2 = h.stats("p2");
    assert_eq!((p2.available, p2.leased, p2.reserved), (0, 0, 0));
    let p1 = h.stats("p1");
    assert_eq!((p1.available, p1.reserved), (1, 0));
}

#[tokio::test(start_paused = true)]
async fn admit_backpressure_reserves_nothing_and_never_evicts() {
    let h = Harness::new(BASE);
    assert_eq!(h.admit("callback_query", 1), Admit::Admitted);
    assert_eq!(h.admit("edited_message", 2), Admit::Admitted);
    assert_eq!(h.admit("message", 3), Admit::Backpressure);
    assert_eq!(poll(&h.pull("p2"), "i", 0).await, [2]);
    assert_eq!(h.stats("p2").reserved, 0);
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="p2",reason="overflow"} 0"#);
}

#[tokio::test(start_paused = true)]
async fn admit_drop_oldest_target_does_not_block_others() {
    let h = Harness::new(BASE);
    let (p1, p2) = (h.pull("p1"), h.pull("p2"));

    // p2 is full of unleased updates: the oldest is evicted.
    assert_eq!(h.admit("edited_message", 1), Admit::Admitted);
    assert_eq!(h.admit("message", 2), Admit::Admitted);
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="p2",reason="overflow"} 1"#);
    assert_eq!(poll(&p2, "i", 0).await, [2]);

    // p2 is full of leased updates: it skips the update, p1 still gets it.
    assert_eq!(poll(&p1, "i", 0).await, [2]);
    assert!(poll(&p1, "i", 3).await.is_empty());
    assert_eq!(h.admit("message", 3), Admit::Admitted);
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="p2",reason="overflow"} 2"#);
    assert_eq!(poll(&p1, "i", 0).await, [3]);
    let s = h.stats("p2");
    assert_eq!((s.available, s.leased, s.reserved), (0, 1, 0));
}

#[tokio::test(start_paused = true)]
async fn admit_unroutable_counts() {
    let h = Harness::new(BASE);
    assert_eq!(h.admit("poll", 1), Admit::Unroutable);
    assert_eq!(
        admit_to(&h.engine, SourceId(9), "message", 2),
        Admit::Unroutable
    );
    for name in ["p1", "p2"] {
        let s = h.stats(name);
        assert_eq!((s.available, s.reserved), (0, 0), "{name}");
    }
    h.assert_metric(r#"tgin_routes_unmatched_total{source="a"} 1"#);
}

#[tokio::test(start_paused = true)]
async fn unmatched_counted_once_by_engine() {
    let h = Harness::new(BASE);
    for id in 1..=2 {
        assert_eq!(h.admit("poll", id), Admit::Unroutable);
    }
    h.assert_metric(r#"tgin_routes_unmatched_total{source="a"} 2"#);
    // Received updates are the source's to count, not the engine's.
    h.assert_metric(r#"tgin_source_updates_total{source="a"} 0"#);
}

#[tokio::test(start_paused = true)]
async fn backpressure_seconds_counted_until_relieved() {
    let h = Harness::new(BASE);
    let p1 = h.pull("p1");
    assert_eq!(h.admit("callback_query", 1), Admit::Admitted);
    assert_eq!(h.admit("callback_query", 2), Admit::Backpressure);
    sleep(Duration::from_secs(1)).await;
    // Every refusal adds the time since the previous one.
    assert_eq!(h.admit("callback_query", 2), Admit::Backpressure);
    h.assert_metric(r#"tgin_consumer_backpressure_seconds_total{consumer="p1"} 1.0"#);
    sleep(Duration::from_secs(2)).await;
    assert_eq!(poll(&p1, "i", 0).await, [1]);
    assert!(poll(&p1, "i", 2).await.is_empty());
    assert_eq!(h.admit("callback_query", 2), Admit::Admitted);
    h.assert_metric(r#"tgin_consumer_backpressure_seconds_total{consumer="p1"} 3.0"#);
    // A new episode starts from zero.
    assert_eq!(h.admit("callback_query", 3), Admit::Backpressure);
    sleep(Duration::from_secs(5)).await;
    h.assert_metric(r#"tgin_consumer_backpressure_seconds_total{consumer="p1"} 3.0"#);
}

#[tokio::test(start_paused = true)]
async fn oversized_update_skips_target_never_backpressure() {
    let yaml = BASE
        .replace(
            "p1: { pull: { capacity: 1 } }",
            "p1: { pull: { capacity: 1, capacity_bytes: 1MiB } }",
        )
        .replace(
            "p2: { pull: { capacity: 1, on_full: drop_oldest } }",
            "p2: { pull: { capacity: 1, on_full: drop_oldest, capacity_bytes: 1MiB } }",
        );
    let h = Harness::new(&yaml);
    let mut big = r#"{"update_id":1,"message":{"text":""#.to_owned();
    big.push_str(&"x".repeat(1 << 20));
    big.push_str(r#""}}"#);
    let facts = Facts {
        update_type: "message",
        chat_id: Some(7),
        ..Facts::default()
    };
    let admit = h.engine.admit(SourceId(0), 1, Bytes::from(big), &facts);
    assert_eq!(admit, Admit::Admitted);
    for name in ["p1", "p2"] {
        let s = h.stats(name);
        assert_eq!((s.available, s.reserved), (0, 0), "{name}");
        h.assert_metric(&format!(
            r#"tgin_consumer_dropped_total{{consumer="{name}",reason="oversized"}} 1"#
        ));
    }
    h.assert_metric(r#"tgin_consumer_backpressure_seconds_total{consumer="p1"} 0.0"#);
    // Smaller updates still queue.
    assert_eq!(h.admit("message", 2), Admit::Admitted);
    assert_eq!(h.stats("p1").available, 1);
}

#[tokio::test(start_paused = true)]
async fn overflow_and_unmatched_warn_once_per_episode() {
    let h = Harness::new(
        r#"version: 1
server: {}
bots:
  - name: logbot
    token: "1:aaa"
    routes:
      - match: { update_types: [message] }
        to: [logq]
consumers:
  logq: { pull: { capacity: 1, on_full: drop_oldest } }
"#,
    );
    let q = h.pull("logq");
    let overflow = ["WARN", "logq", "dropping updates"];
    let unmatched = ["WARN", "logbot", "match no rule"];

    assert_eq!(h.admit("message", 1), Admit::Admitted);
    assert_eq!(poll(&q, "i", 0).await, [1]);
    // Full of leased updates: each new one is skipped.
    for id in 2..=3 {
        assert_eq!(h.admit("message", id), Admit::Admitted);
    }
    assert_eq!(LOGS.count(&overflow), 1);
    // Room again: the episode ends, and the next drop warns again.
    assert!(poll(&q, "i", 2).await.is_empty());
    assert_eq!(h.admit("message", 4), Admit::Admitted);
    assert_eq!(poll(&q, "i", 0).await, [4]);
    assert_eq!(h.admit("message", 5), Admit::Admitted);
    assert_eq!(LOGS.count(&overflow), 2);

    for id in 6..=7 {
        assert_eq!(h.admit("poll", id), Admit::Unroutable);
    }
    assert_eq!(LOGS.count(&unmatched), 1);
    assert_eq!(LOGS.count(&["DEBUG", "logbot", "update_id=7"]), 1);
    assert_eq!(h.admit("message", 8), Admit::Admitted);
    assert_eq!(h.admit("poll", 9), Admit::Unroutable);
    assert_eq!(LOGS.count(&unmatched), 2);
}

/// Two bots: `a` routes `callback_query` to `p1`, `b` everything to `p2`.
const TWO_BOTS: &str = r#"version: 1
server: {}
bots:
  - name: a
    token: "1:aaa"
    routes: [{ match: { update_types: [callback_query] }, to: [p1] }]
  - name: b
    token: "2:bbb"
    routes: [{ to: [p2] }]
consumers:
  p1: { pull: {} }
  p2: { pull: {} }
"#;

#[tokio::test(start_paused = true)]
async fn source_ids_are_stable_across_reloads() {
    let h = Harness::new(TWO_BOTS);
    assert_eq!(h.engine.source_id("a"), Some(SourceId(0)));
    assert_eq!(h.engine.source_id("b"), Some(SourceId(1)));

    // Removing the first bot keeps the second one's id.
    let without_a = TWO_BOTS.replace(
        "  - name: a\n    token: \"1:aaa\"\n    routes: [{ match: { update_types: [callback_query] }, to: [p1] }]\n",
        "",
    );
    let report = h.engine.apply(&load(&without_a)).expect("apply");
    assert_eq!(report.sources_removed, ["a"]);
    assert!(report.sources_added.is_empty());
    assert_eq!(h.engine.source_id("a"), None);
    assert_eq!(h.engine.source_id("b"), Some(SourceId(1)));
    assert_eq!(
        admit_to(&h.engine, SourceId(1), "message", 1),
        Admit::Admitted
    );
    assert_eq!(
        admit_to(&h.engine, SourceId(0), "callback_query", 2),
        Admit::Unroutable
    );
    assert_eq!(poll(&h.pull("p2"), "i", 0).await, [1]);

    // A bot inserted in front gets a fresh id; `b` keeps its id and rules.
    let c_first = without_a.replace(
        "bots:\n",
        "bots:\n  - name: c\n    token: \"3:ccc\"\n    routes: [{ to: [p1] }]\n",
    );
    let report = h.engine.apply(&load(&c_first)).expect("apply");
    assert_eq!(report.sources_added, ["c"]);
    assert_eq!(h.engine.source_id("c"), Some(SourceId(2)));
    assert_eq!(h.engine.source_id("b"), Some(SourceId(1)));
    assert_eq!(
        admit_to(&h.engine, SourceId(1), "message", 3),
        Admit::Admitted
    );
    assert_eq!(poll(&h.pull("p2"), "i", 2).await, [3]);
    assert_eq!(
        admit_to(&h.engine, SourceId(2), "message", 4),
        Admit::Admitted
    );
    assert_eq!(poll(&h.pull("p1"), "i", 0).await, [4]);

    // A removed bot that comes back is a new bot: its old id stays unused.
    let a_back = c_first.replace(
        "consumers:\n",
        "  - name: a\n    token: \"1:aaa\"\n    routes: [{ to: [p3] }]\nconsumers:\n  p3: { pull: {} }\n",
    );
    let report = h.engine.apply(&load(&a_back)).expect("apply");
    assert_eq!(report.sources_added, ["a"]);
    assert_eq!(h.engine.source_id("a"), Some(SourceId(3)));
    assert_eq!(
        admit_to(&h.engine, SourceId(0), "message", 5),
        Admit::Unroutable
    );
}

#[tokio::test(start_paused = true)]
async fn removed_pull_consumer_stays_pollable_while_draining() {
    let yaml = r#"version: 1
server: {}
reload: { drain_timeout: 2s }
bots:
  - name: a
    token: "1:aaa"
    routes:
      - match: { update_types: [callback_query] }
        to: [pollme]
      - to: [p2]
consumers:
  pollme: { pull: {} }
  p2: { pull: {} }
"#;
    let h = Harness::new(yaml);
    assert_eq!(h.admit("callback_query", 1), Admit::Admitted);
    let old = h.pull("pollme");
    let without = yaml
        .replace(
            "      - match: { update_types: [callback_query] }\n        to: [pollme]\n",
            "",
        )
        .replace("  pollme: { pull: {} }\n", "");
    h.engine.apply(&load(&without)).expect("apply");
    assert!(h.engine.consumer("pollme").is_none());
    let draining = h.engine.pull_consumer("pollme").expect("still draining");
    assert!(Arc::ptr_eq(&old, &draining));
    assert!(Arc::ptr_eq(
        &h.engine.pull_consumer("p2").expect("current"),
        &h.pull("p2")
    ));
    assert!(h.engine.pull_consumer("nope").is_none());

    assert_eq!(poll(&draining, "i", 0).await, [1]);
    assert!(poll(&draining, "i", 2).await.is_empty());
    drop(draining);
    // The drain ends well before `drain_timeout` (2s), with nothing left.
    sleep(Duration::from_millis(300)).await;
    assert!(h.engine.pull_consumer("pollme").is_none());
    assert_eq!(Arc::strong_count(&old), 1);
    assert_eq!(h.engine.pending(), 0);
    assert_eq!(
        LOGS.count(&["consumer=pollme", "still held"]),
        0,
        "nothing was dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn retired_instance_leaves_shared_gauges_alone() {
    let h = Harness::new(BASE);
    assert_eq!(h.admit("edited_message", 1), Admit::Admitted);
    h.assert_metric(r#"tgin_consumer_queue_depth{consumer="p2"} 1"#);
    let pushed = BASE.replace(
        "p2: { pull: { capacity: 1, on_full: drop_oldest } }",
        r#"p2: { push: { endpoints: [{ url: "http://127.0.0.1:9/" }] } }"#,
    );
    h.engine.apply(&load(&pushed)).expect("apply");
    h.assert_metric(r#"tgin_consumer_queue_depth{consumer="p2"} 0"#);
    // The old instance's lease expiry runs every second; it still holds one.
    sleep(Duration::from_millis(1500)).await;
    h.assert_metric(r#"tgin_consumer_queue_depth{consumer="p2"} 0"#);
    sleep(Duration::from_secs(1)).await;
    assert_eq!(h.engine.pending(), 0);
    h.assert_metric(r#"tgin_consumer_queue_depth{consumer="p2"} 0"#);
}

#[tokio::test(start_paused = true)]
async fn readded_consumer_revives_its_draining_instance() {
    let with_h = BASE
        .replace(
            "consumers:\n",
            "consumers:\n  h: { push: { endpoints: [{ url: \"http://127.0.0.1:9/\" }] } }\n",
        )
        .replace(
            "        to: [p2]\n",
            "        to: [p2]\n      - match: { update_types: [channel_post] }\n        to: [h]\n",
        );
    let h = Harness::new(&with_h);
    let Some(Consumer::Push(hook)) = h.engine.consumer("h") else {
        panic!("h is a push consumer");
    };
    let mut dispatcher = h.supervisor.subscribe("push:h").expect("supervised");
    dispatcher
        .wait_for(|s| *s == TaskState::Running)
        .await
        .expect("running");
    // Paused, so nothing reaches the unreachable endpoint.
    assert!(h.engine.pause("h"));
    assert_eq!(h.admit("channel_post", 1), Admit::Admitted);

    let report = h.engine.apply(&load(BASE)).expect("apply");
    assert_eq!(report.removed, ["h"]);
    sleep(Duration::from_millis(500)).await;
    let report = h.engine.apply(&load(&with_h)).expect("apply");
    assert_eq!(report.added, ["h"]);
    let Some(Consumer::Push(back)) = h.engine.consumer("h") else {
        panic!("h is a push consumer");
    };
    assert!(Arc::ptr_eq(&hook, &back), "the draining instance came back");
    assert!(
        !dispatcher.has_changed().expect("supervised"),
        "the dispatcher kept running"
    );

    // Its drain was called off: the update outlives `drain_timeout`.
    sleep(Duration::from_secs(3)).await;
    assert_eq!(h.stats("h").available, 1);
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="h",reason="reload"} 0"#);
    assert_eq!(h.engine.pending(), 1);

    // Removed for good, its supervised task is forgotten once it ends.
    drop(back);
    h.engine.apply(&load(BASE)).expect("apply");
    sleep(Duration::from_secs(3)).await;
    eventually("push:h is forgotten", || {
        h.supervisor.subscribe("push:h").is_none()
    })
    .await;
    assert_eq!(Arc::strong_count(&hook), 1);
}

#[tokio::test(start_paused = true)]
async fn set_username_enables_bot_suffixed_commands() {
    let h = Harness::new(
        r#"version: 1
server: {}
bots:
  - name: a
    token: "1:aaa"
    routes:
      - match: { commands: [start] }
        to: [p1]
consumers:
  p1: { pull: {} }
"#,
    );
    let start = |id| {
        let facts = Facts {
            update_type: "message",
            command: Some(("start", Some("MyBot"))),
            ..Facts::default()
        };
        h.engine
            .admit(SourceId(0), id, payload("message", id), &facts)
    };
    assert_eq!(start(1), Admit::Unroutable);
    h.engine.set_username(SourceId(0), Some("mybot".to_owned()));
    assert_eq!(start(2), Admit::Admitted);
    assert_eq!(h.engine.state().generation, 1);
    assert_eq!(poll(&h.pull("p1"), "i", 0).await, [2]);
}

#[tokio::test(start_paused = true)]
async fn reload_keeps_queue_of_unchanged_consumer() {
    let h = Harness::new(BASE);
    let before = h.pull("p1");
    assert_eq!(h.admit("callback_query", 1), Admit::Admitted);
    let report = h
        .engine
        .apply(&load(&format!("{BASE}  p3: {{ pull: {{}} }}\n")))
        .expect("apply");
    assert_eq!(report.added, ["p3"]);
    assert!(report.removed.is_empty() && report.changed.is_empty());
    assert!(Arc::ptr_eq(&before, &h.pull("p1")));
    assert_eq!(poll(&h.pull("p1"), "i", 0).await, [1]);
}

#[tokio::test]
async fn reload_changes_params_live() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let yaml = |capacity: u32, route: &str| {
        format!(
            r#"{BASE}  h: {{ push: {{ endpoints: [{{ url: "{}{route}" }}] }} }}
"#,
            server.uri()
        )
        .replace(
            "p1: { pull: { capacity: 1 } }",
            &format!("p1: {{ pull: {{ capacity: {capacity} }} }}"),
        )
        .replace(
            "        to: [p2]\n",
            "        to: [p2]\n      - match: { update_types: [channel_post] }\n        to: [h]\n",
        )
    };
    let h = Harness::new(&yaml(1, "/a"));
    let p1 = h.pull("p1");
    let Some(Consumer::Push(hook)) = h.engine.consumer("h") else {
        panic!("h is a push consumer");
    };
    assert_eq!(h.admit("callback_query", 1), Admit::Admitted);
    assert_eq!(h.admit("callback_query", 2), Admit::Backpressure);

    let report = h.engine.apply(&load(&yaml(3, "/b"))).expect("apply");
    assert_eq!(report.changed, ["h", "p1"]);
    assert!(Arc::ptr_eq(&p1, &h.pull("p1")));
    let Some(Consumer::Push(after)) = h.engine.consumer("h") else {
        panic!("h is a push consumer");
    };
    assert!(Arc::ptr_eq(&hook, &after));

    assert_eq!(h.admit("callback_query", 2), Admit::Admitted);
    assert_eq!(h.admit("callback_query", 3), Admit::Admitted);
    assert_eq!(h.admit("channel_post", 4), Admit::Admitted);
    timeout(WAIT, async {
        loop {
            let got = server.received_requests().await.expect("recording");
            if let Some(req) = got.first() {
                assert_eq!(req.url.path(), "/b");
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("delivered to the new endpoint");
}

/// A reload that only adds a bot keeps every push consumer's endpoints, with
/// their breakers and in-flight counts; only the source names it reports in
/// `X-Tgin-Source` change.
#[tokio::test]
async fn adding_a_bot_keeps_push_breakers() {
    let down = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&down)
        .await;
    let up = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&up)
        .await;
    let base = format!(
        r#"version: 1
bots:
  - name: a
    token: "1:aaa"
    routes: [{{ to: [down, up] }}]
consumers:
  down:
    push:
      endpoints: [{{ url: "{}/hook" }}]
      retry: {{ max_attempts: 1 }}
      breaker: {{ failure_threshold: 1, open_for: 1h }}
  up: {{ push: {{ endpoints: [{{ url: "{}/hook" }}] }} }}
"#,
        down.uri(),
        up.uri()
    );
    let h = Harness::new(&base);
    let Some(Consumer::Push(dead)) = h.engine.consumer("down") else {
        panic!("down is a push consumer");
    };
    assert_eq!(h.admit("message", 1), Admit::Admitted);
    eventually("the breaker opens", || {
        dead.endpoints()[0].1 == BreakerState::Open
    })
    .await;

    let with_b = base.replace(
        "consumers:\n",
        "  - name: b\n    token: \"2:bbb\"\n    routes: [{ to: [down, up] }]\nconsumers:\n",
    );
    let report = h.engine.apply(&load(&with_b)).expect("apply");
    assert_eq!(report.sources_added, ["b"]);
    assert!(report.changed.is_empty(), "{report:?}");
    assert_eq!(dead.endpoints()[0].1, BreakerState::Open);

    let b = h.engine.source_id("b").expect("b has a source");
    assert_eq!(admit_to(&h.engine, b, "message", 2), Admit::Admitted);
    timeout(WAIT, async {
        loop {
            let got = up.received_requests().await.expect("recording");
            if let Some(req) = got
                .iter()
                .find(|r| r.body.starts_with(br#"{"update_id":2,"#))
            {
                assert_eq!(
                    req.headers.get("x-tgin-source").map(|v| v.as_bytes()),
                    Some(&b"b"[..])
                );
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("b's update delivered with its name");
    assert_eq!(dead.endpoints()[0].1, BreakerState::Open);
    assert_eq!(down.received_requests().await.expect("recording").len(), 1);
}

/// A reload that changes only `webhook_reply` keeps the push consumer's
/// endpoints, with their breakers: only its options change.
#[tokio::test]
async fn webhook_reply_change_keeps_push_breakers() {
    let down = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&down)
        .await;
    let base = format!(
        r#"version: 1
bots:
  - name: a
    token: "1:aaa"
    routes: [{{ to: [down] }}]
consumers:
  down:
    push:
      endpoints: [{{ url: "{}/hook" }}]
      retry: {{ max_attempts: 1 }}
      breaker: {{ failure_threshold: 1, open_for: 1h }}
"#,
        down.uri()
    );
    let h = Harness::new(&base);
    let Some(Consumer::Push(dead)) = h.engine.consumer("down") else {
        panic!("down is a push consumer");
    };
    assert_eq!(h.admit("message", 1), Admit::Admitted);
    eventually("the breaker opens", || {
        dead.endpoints()[0].1 == BreakerState::Open
    })
    .await;

    let ignore = base.replace("      retry:", "      webhook_reply: ignore\n      retry:");
    let report = h.engine.apply(&load(&ignore)).expect("apply");
    assert_eq!(report.changed, ["down"]);
    let Some(Consumer::Push(after)) = h.engine.consumer("down") else {
        panic!("down is a push consumer");
    };
    assert!(Arc::ptr_eq(&dead, &after));
    assert_eq!(dead.endpoints()[0].1, BreakerState::Open);
    assert_eq!(down.received_requests().await.expect("recording").len(), 1);
}

#[tokio::test(start_paused = true)]
async fn reload_mode_change_drains_then_drops() {
    let h = Harness::new(BASE);
    let old = h.pull("p2");
    assert_eq!(h.admit("edited_message", 1), Admit::Admitted);
    let pushed = BASE.replace(
        "p2: { pull: { capacity: 1, on_full: drop_oldest } }",
        r#"p2: { push: { endpoints: [{ url: "http://127.0.0.1:9/" }] } }"#,
    );
    let report = h.engine.apply(&load(&pushed)).expect("apply");
    assert_eq!(report.changed, ["p2"]);
    assert!(matches!(h.engine.consumer("p2"), Some(Consumer::Push(_))));
    assert_eq!(h.stats("p2").available, 0, "the new instance starts empty");

    // The old instance drains for `drain_timeout`; nobody polls it.
    sleep(Duration::from_millis(1900)).await;
    assert_eq!(old.pending(), 1);
    assert_eq!(h.engine.pending(), 1);
    sleep(Duration::from_millis(200)).await;
    assert_eq!(old.pending(), 0);
    assert_eq!(h.engine.pending(), 0);
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="p2",reason="reload"} 1"#);
}

#[tokio::test(start_paused = true)]
async fn mode_change_keeps_metric_series() {
    let h = Harness::new(BASE);
    assert_eq!(h.admit("edited_message", 1), Admit::Admitted);
    let pushed = BASE.replace(
        "p2: { pull: { capacity: 1, on_full: drop_oldest } }",
        r#"p2: { push: { endpoints: [{ url: "http://127.0.0.1:9/" }] } }"#,
    );
    h.engine.apply(&load(&pushed)).expect("apply");
    sleep(Duration::from_secs(3)).await;
    assert_eq!(h.engine.pending(), 0);
    let out = h.metrics();
    for series in [
        r#"tgin_consumer_dropped_total{consumer="p2",reason="reload"} 1"#,
        r#"tgin_consumer_queue_depth{consumer="p2"}"#,
        r#"tgin_delivery_latency_seconds_count{consumer="p2"}"#,
    ] {
        assert!(out.contains(series), "missing {series:?} in\n{out}");
    }
}

#[tokio::test(start_paused = true)]
async fn reload_removed_consumer_metrics_removed() {
    // A second bot, `b`, routes to a push consumer `p3`.
    let two_bots = BASE.replace(
        "consumers:\n",
        "  - name: b\n    token: \"2:bbb\"\n    routes: [{ to: [p3] }]\nconsumers:\n  p3: { push: { endpoints: [{ url: \"http://127.0.0.1:9/\" }] } }\n",
    );
    let h = Harness::new(&two_bots);
    h.assert_metric(r#"tgin_routes_unmatched_total{source="b"} 0"#);
    let without = BASE
        .replace("        to: [{ all: [p1, p2] }]\n", "        to: [p1]\n")
        .replace(
            "      - match: { update_types: [edited_message] }\n        to: [p2]\n",
            "",
        )
        .replace(
            "  p2: { pull: { capacity: 1, on_full: drop_oldest } }\n",
            "",
        );
    let report = h.engine.apply(&load(&without)).expect("apply");
    assert_eq!(report.removed, ["p2", "p3"]);
    eventually("the removed consumers drained", || h.engine.pending() == 0).await;
    let removed = || {
        let out = h.metrics();
        !out.contains(r#"consumer="p2""#)
            && !out.contains(r#"consumer="p3""#)
            && !out.contains(r#"source="b""#)
    };
    eventually("the removed label sets are gone", removed).await;
    let out = h.metrics();
    assert!(out.contains(r#"consumer="p1""#), "{out}");
    assert!(out.contains(r#"source="a""#), "{out}");
}

#[tokio::test(start_paused = true)]
async fn removed_consumer_drains_then_drops_and_cancels_tasks() {
    let yaml = BASE.replace(
        "consumers:\n",
        "consumers:\n  h: { push: { endpoints: [{ url: \"http://127.0.0.1:9/\" }] } }\n",
    );
    let h = Harness::new(&yaml);
    let p1 = h.pull("p1");
    let Some(Consumer::Push(hook)) = h.engine.consumer("h") else {
        panic!("h is a push consumer");
    };
    let mut dispatcher = h.supervisor.subscribe("push:h").expect("supervised");
    dispatcher
        .wait_for(|s| *s == TaskState::Running)
        .await
        .expect("running");
    assert_eq!(h.admit("callback_query", 1), Admit::Admitted);

    let only_p2 = r#"version: 1
server: {}
reload: { drain_timeout: 2s }
bots:
  - name: a
    token: "1:aaa"
    routes: [{ to: [p2] }]
consumers:
  p2: { pull: { capacity: 1, on_full: drop_oldest } }
"#;
    let report = h.engine.apply(&load(only_p2)).expect("apply");
    assert_eq!(report.removed, ["h", "p1"]);
    assert!(h.engine.consumer("p1").is_none());
    assert_eq!(
        h.admit("callback_query", 2),
        Admit::Admitted,
        "routed to p2"
    );

    // `h` is empty: its drain ends at once and its dispatcher stops.
    dispatcher
        .wait_for(|s| *s == TaskState::Stopped)
        .await
        .expect("stopped");
    eventually("h is released", || Arc::strong_count(&hook) == 1).await;
    eventually("push:h is forgotten", || {
        h.supervisor.subscribe("push:h").is_none()
    })
    .await;

    // `p1` holds an update until the drain timeout.
    sleep(Duration::from_millis(1900)).await;
    assert!(Arc::strong_count(&p1) > 1, "p1 is still draining");
    assert_eq!(p1.pending(), 1);
    sleep(Duration::from_millis(200)).await;
    eventually("p1 is released", || Arc::strong_count(&p1) == 1).await;
    assert_eq!(p1.pending(), 0);
    assert_eq!(h.engine.pending(), 1, "only p2's update is left");
}

#[tokio::test(start_paused = true)]
async fn reload_reports_restart_required() {
    let h = Harness::new(BASE);
    let same = h.engine.apply(&load(BASE)).expect("apply");
    assert!(same.restart_required.is_empty() && same.sources_restarted.is_empty());
    assert!(same.added.is_empty() && same.removed.is_empty() && same.changed.is_empty());

    let changed = BASE
        .replace(
            "server: {}\n",
            "server: { listen: [\"127.0.0.1:8081\"], max_connections: 9, header_read_timeout: 3s, tls: { cert_file: c.pem, key_file: k.pem } }\nadmin: { listen: [\"127.0.0.1:9191\"] }\nruntime: { worker_threads: 2 }\nlog: { level: debug, format: json }\n",
        )
        .replace("token: \"1:aaa\"", "token: \"1:bbb\"");
    let report = h.engine.apply(&load(&changed)).expect("apply");
    // Sorted; `log.level` applies live.
    assert_eq!(
        report.restart_required,
        [
            "admin.listen",
            "log.format",
            "runtime.worker_threads",
            "server.header_read_timeout",
            "server.listen",
            "server.max_connections",
            "server.tls",
        ]
    );
    assert_eq!(report.sources_restarted, ["a"]);

    let api = changed.replace(
        "token: \"1:bbb\"",
        "token: \"1:bbb\"\n    api_url: \"http://127.0.0.1:8082/\"",
    );
    let report = h.engine.apply(&load(&api)).expect("apply");
    assert!(report.restart_required.is_empty());
    assert_eq!(report.sources_restarted, ["a"]);

    let ingress = api.replace(
        "api_url: \"http://127.0.0.1:8082/\"",
        "api_url: \"http://127.0.0.1:8082/\"\n    ingress: { long_poll: {} }",
    );
    let report = h.engine.apply(&load(&ingress)).expect("apply");
    assert_eq!(report.sources_restarted, ["a"]);
}

/// Moving a bot to another upstream restarts its source and lists it in
/// `upstream_changed` (the app warns of the update split, or says that a
/// parked bot starts there); editing another bot's upstream, or one no bot
/// uses, leaves it alone.
#[tokio::test(start_paused = true)]
async fn upstream_change_reported() {
    const MOVES: &str = r#"version: 1
server: {}
upstreams:
  local: { kind: local, url: "http://127.0.0.1:8081" }
  other: { kind: local, url: "http://127.0.0.1:8082" }
  spare: { kind: local, url: "http://127.0.0.1:8090" }
bots:
  - name: mover
    token: "1:mover"
    routes: [{ to: [p] }]
  - name: stayer
    token: "2:stayer"
    upstream: other
    routes: [{ to: [q] }]
consumers:
  p: { pull: {} }
  q: { pull: {} }
"#;
    let h = Harness::new(MOVES);

    let moved = MOVES.replace(
        "token: \"1:mover\"\n",
        "token: \"1:mover\"\n    upstream: local\n",
    );
    let report = h.engine.apply(&load(&moved)).expect("apply");
    assert_eq!(report.sources_restarted, ["mover"]);
    assert_eq!(report.upstream_changed, ["mover"]);
    assert!(report.restart_required.is_empty(), "{report:?}");

    let edited = moved.replace("8082", "8083");
    let report = h.engine.apply(&load(&edited)).expect("apply");
    assert_eq!(report.sources_restarted, ["stayer"]);
    assert_eq!(report.upstream_changed, ["stayer"]);

    let same = h.engine.apply(&load(&edited)).expect("apply");
    assert!(same.sources_restarted.is_empty(), "{same:?}");
    assert!(same.upstream_changed.is_empty(), "{same:?}");

    let spare = edited.replace("8090", "8091");
    let report = h.engine.apply(&load(&spare)).expect("apply");
    assert!(report.sources_restarted.is_empty(), "{report:?}");
    assert!(report.upstream_changed.is_empty(), "{report:?}");
}

#[tokio::test(start_paused = true)]
async fn reload_generation_increments() {
    let h = Harness::new(BASE);
    assert_eq!(h.engine.state().generation, 1);
    h.assert_metric("tgin_config_generation 1");
    for want in 2..=3 {
        let report = h.engine.apply(&load(BASE)).expect("apply");
        assert_eq!(report.generation, want);
        assert_eq!(h.engine.state().generation, want);
        assert_eq!(h.engine.status().generation, want);
    }
    h.assert_metric("tgin_config_generation 3");
    h.assert_metric(r#"tgin_config_reloads_total{outcome="applied"} 2"#);
    h.assert_metric(r#"tgin_config_reloads_total{outcome="rejected"} 0"#);
}

#[tokio::test(start_paused = true)]
async fn pause_resume_purge() {
    let yaml = BASE
        .replace(
            "consumers:\n",
            "consumers:\n  h: { push: { endpoints: [{ url: \"http://127.0.0.1:9/\" }] } }\n",
        )
        .replace(
            "        to: [p2]\n",
            "        to: [p2]\n      - match: { update_types: [channel_post] }\n        to: [h]\n",
        );
    let h = Harness::new(&yaml);
    let p1 = h.pull("p1");
    assert!(!h.engine.pause("nope"));
    assert!(!h.engine.resume("nope"));
    assert_eq!(h.engine.purge("nope"), None);

    // A paused pull consumer admits but leases nothing new.
    assert!(h.engine.pause("p1"));
    assert_eq!(h.admit("callback_query", 1), Admit::Admitted);
    assert!(poll(&p1, "i", 0).await.is_empty());
    let status = h.engine.status();
    let p1_status = status.consumers.iter().find(|c| c.name == "p1").unwrap();
    assert!(p1_status.paused);
    assert_eq!(p1_status.depth, 1);

    // It stays paused across a reload that keeps it.
    h.engine.apply(&load(&yaml)).expect("apply");
    assert!(poll(&p1, "i", 0).await.is_empty());

    // A parked poll leases once the consumer resumes.
    let parked = {
        let p1 = Arc::clone(&p1);
        tokio::spawn(async move {
            p1.poll(
                req("i", 0, Duration::from_secs(30)),
                &CancellationToken::new(),
            )
            .await
        })
    };
    sleep(Duration::from_secs(1)).await;
    assert!(!parked.is_finished());
    assert!(h.engine.resume("p1"));
    let resp = timeout(Duration::from_secs(1), parked)
        .await
        .expect("woken")
        .expect("poll");
    assert_eq!(resp.items.len(), 1);

    // Purge drops unleased updates only.
    assert_eq!(h.admit("edited_message", 2), Admit::Admitted);
    assert_eq!(h.engine.purge("p2"), Some(1));
    assert_eq!(h.engine.purge("p1"), Some(0), "p1's update is leased");
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="p2",reason="purge"} 1"#);
    assert_eq!(h.stats("p2").available, 0);

    // A paused push consumer admits but does not dispatch.
    assert!(h.engine.pause("h"));
    assert_eq!(h.admit("channel_post", 3), Admit::Admitted);
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let s = h.stats("h");
    assert_eq!((s.available, s.leased), (1, 0));
    assert!(h.engine.resume("h"));
    eventually("h leases after resuming", || h.stats("h").available == 0).await;
}

#[tokio::test(start_paused = true)]
async fn status_snapshot_has_no_secrets() {
    let yaml = r#"version: 1
server: {}
bots:
  - name: a
    token: "123:t0kens3cret"
    routes:
      - match: { update_types: [message] }
        to: [h]
      - to: [p1]
consumers:
  h:
    push:
      endpoints:
        - url: "https://user:pa55word@hooks.example:8443/s3cretpath?token=qs3cret"
          secret_token: hdrs3cret
        - url: "http://plain.example/"
      accept_set_webhook: true
  p1: { pull: { auth_token: pulls3cret } }
"#;
    let h = Harness::new(yaml);
    h.engine.pause("p1");
    let runtime = (
        "https://rtuser:rtpa55@rt.example/rts3cret?q=rtq"
            .parse()
            .expect("url"),
        Some(Secret::new("rthdrs3cret".to_owned())),
    );
    assert_eq!(
        h.engine
            .set_runtime_endpoint("h", SourceId(0), &tok("123:t0kens3cret"), Some(runtime)),
        Ok(Some(WebhookChange::Set))
    );
    let status = h.engine.status();
    let json = serde_json::to_value(&status).expect("serialize");
    let text = json.to_string();
    for secret in [
        "t0kens3cret",
        "pa55word",
        "s3cretpath",
        "qs3cret",
        "hdrs3cret",
        "pulls3cret",
        "user",
        "rtpa55",
        "rts3cret",
        "rtq",
    ] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
    assert_eq!(json["generation"], 1);
    assert_eq!(
        json["consumers"],
        serde_json::json!([
            {
                "name": "h",
                "mode": "push",
                "depth": 0,
                "leased": 0,
                "in_flight": 0,
                "paused": false,
                "endpoints": [
                    { "index": 0, "origin": "https://hooks.example:8443", "breaker": "closed", "runtime": false },
                    { "index": 1, "origin": "http://plain.example", "breaker": "closed", "runtime": false },
                    { "index": 2, "origin": "https://rt.example", "breaker": "closed", "runtime": true },
                ],
            },
            {
                "name": "p1",
                "mode": "pull",
                "depth": 0,
                "leased": 0,
                "in_flight": 0,
                "paused": true,
            },
        ])
    );
}

/// Bot `a` feeds `p1` and `q` (inside an `all`), bot `b` feeds `p2`.
const FEEDERS: &str = r#"version: 1
server: {}
reload: { drain_timeout: 2s }
bots:
  - name: a
    token: "1:aaa"
    routes:
      - match: { update_types: [callback_query] }
        to: [{ all: [p1, q] }]
  - name: b
    token: "2:bbb"
    routes:
      - to: [p2]
consumers:
  p1: { pull: {} }
  q: { pull: {} }
  p2: { pull: {} }
"#;

/// `routes_to` follows the rules in effect: nested targets count, other
/// bots' consumers and unknown names do not, and a reload that moves a
/// consumer to another bot moves the answer with it.
#[tokio::test(start_paused = true)]
async fn routes_to_follows_current_rules() {
    let h = Harness::new(FEEDERS);
    let (a, b) = (SourceId(0), SourceId(1));
    assert!(h.engine.routes_to(a, "p1"));
    assert!(h.engine.routes_to(a, "q"));
    assert!(!h.engine.routes_to(a, "p2"));
    assert!(h.engine.routes_to(b, "p2"));
    assert!(!h.engine.routes_to(b, "p1"));
    assert!(!h.engine.routes_to(a, "nope"));
    assert!(!h.engine.routes_to(SourceId(7), "p1"));

    let moved = FEEDERS
        .replace("to: [{ all: [p1, q] }]", "to: [q]")
        .replace("to: [p2]", "to: [{ all: [p2, p1] }]");
    h.engine.apply(&load(&moved)).expect("apply");
    assert!(!h.engine.routes_to(a, "p1"), "kept instance, now b's");
    assert!(h.engine.routes_to(b, "p1"));
    assert!(h.engine.routes_to(a, "q"));
}

/// A removed pull consumer still draining is routed to by the bot that fed
/// it, and by no other, until its drain ends.
#[tokio::test(start_paused = true)]
async fn routes_to_keeps_draining_retired_pull_feeder() {
    let h = Harness::new(FEEDERS);
    let (a, b) = (SourceId(0), SourceId(1));
    assert_eq!(h.admit("callback_query", 1), Admit::Admitted);
    let without = FEEDERS
        .replace("to: [{ all: [p1, q] }]", "to: [q]")
        .replace("  p1: { pull: {} }\n", "");
    h.engine.apply(&load(&without)).expect("apply");
    assert!(h.engine.consumer("p1").is_none());
    assert!(h.engine.routes_to(a, "p1"), "draining, fed by a");
    assert!(!h.engine.routes_to(b, "p1"));

    let draining = h.engine.pull_consumer("p1").expect("still draining");
    assert_eq!(poll(&draining, "i", 0).await, [1]);
    assert!(poll(&draining, "i", 2).await.is_empty());
    drop(draining);
    sleep(Duration::from_millis(300)).await;
    assert!(h.engine.pull_consumer("p1").is_none());
    assert!(!h.engine.routes_to(a, "p1"), "the drain ended");
}

/// A kept pull consumer moved from bot `a` to bot `b`, then removed while
/// it still holds `b`'s update: it drains for `b`, not for `a`, which fed
/// it when it was built.
#[tokio::test(start_paused = true)]
async fn routes_to_follows_feeder_of_moved_then_retired_pull() {
    let h = Harness::new(FEEDERS);
    let (a, b) = (SourceId(0), SourceId(1));
    let moved = FEEDERS
        .replace("to: [{ all: [p1, q] }]", "to: [q]")
        .replace("to: [p2]", "to: [{ all: [p2, p1] }]");
    h.engine.apply(&load(&moved)).expect("apply");
    assert_eq!(admit_to(&h.engine, b, "message", 1), Admit::Admitted);
    let without = moved
        .replace("to: [{ all: [p2, p1] }]", "to: [p2]")
        .replace("  p1: { pull: {} }\n", "");
    h.engine.apply(&load(&without)).expect("apply");
    assert!(h.engine.consumer("p1").is_none());
    assert!(h.engine.pull_consumer("p1").is_some(), "still draining");
    assert!(
        !h.engine.routes_to(a, "p1"),
        "a fed it only before the move"
    );
    assert!(h.engine.routes_to(b, "p1"));
}

/// The pull consumer a bot's scoped `getUpdates` polls: the current
/// instance it feeds, or a draining one it fed, never an instance another
/// bot fed, even when the name now belongs to a consumer of this bot.
#[tokio::test(start_paused = true)]
async fn pull_consumer_for_follows_the_feeding_bot() {
    let h = Harness::new(FEEDERS);
    let (a, b) = (SourceId(0), SourceId(1));
    assert!(h.engine.pull_consumer_for(a, "p1").is_some());
    assert!(h.engine.pull_consumer_for(b, "p1").is_none());
    assert!(h.engine.pull_consumer_for(b, "p2").is_some());
    assert!(h.engine.pull_consumer_for(a, "nope").is_none());
    assert_eq!(h.admit("callback_query", 1), Admit::Admitted);

    // `p1` becomes a push consumer of `b`; its pull instance, fed by `a`,
    // drains.
    let switched = FEEDERS
        .replace("to: [{ all: [p1, q] }]", "to: [q]")
        .replace("to: [p2]", "to: [{ all: [p2, p1] }]")
        .replace(
            "  p1: { pull: {} }\n",
            "  p1: { push: { endpoints: [{ url: \"http://127.0.0.1:9/in\" }] } }\n",
        );
    h.engine.apply(&load(&switched)).expect("apply");
    assert!(h.engine.routes_to(b, "p1"), "b feeds the push consumer");
    assert!(h.engine.pull_consumer("p1").is_some(), "draining");
    assert!(
        h.engine.pull_consumer_for(b, "p1").is_none(),
        "a's draining updates are not b's to poll"
    );
    let draining = h.engine.pull_consumer_for(a, "p1").expect("a's drain");
    assert_eq!(poll(&draining, "i", 0).await, [1]);
}

/// Bot `a` feeds the push consumer `hooks`, which takes its endpoint from
/// `setWebhook`; bot `b` feeds the pull consumer `p`.
const CAPTURE: &str = r#"version: 1
server: {}
bots:
  - name: a
    token: "1:aaa"
    routes: [{ to: [hooks] }]
  - name: b
    token: "2:bbb"
    routes: [{ to: [p] }]
consumers:
  hooks: { push: { accept_set_webhook: true } }
  p: { pull: {} }
"#;

fn webhook(url: &str, secret: Option<&str>) -> Option<(reqwest::Url, Option<Secret<String>>)> {
    Some((
        url.parse().expect("url"),
        secret.map(|s| Secret::new(s.to_owned())),
    ))
}

fn runtime_url(h: &Harness, name: &str) -> Option<String> {
    match h.engine.consumer(name) {
        Some(Consumer::Push(push)) => push.runtime_endpoint_url().map(String::from),
        other => panic!("{name} is no push consumer: {}", other.is_some()),
    }
}

fn tok(token: &str) -> Secret<String> {
    Secret::new(token.to_owned())
}

/// `set_runtime_endpoint` as the bot `source` with its current `token`.
fn capture(
    h: &Harness,
    consumer: &str,
    source: SourceId,
    token: &str,
    endpoint: Option<(reqwest::Url, Option<Secret<String>>)>,
) -> Option<WebhookChange> {
    h.engine
        .set_runtime_endpoint(consumer, source, &tok(token), endpoint)
        .expect("the token is the bot's")
}

/// Only the bot that feeds an accepting push consumer sets its runtime
/// endpoint; the answer tells a change from a repeat.
#[tokio::test(start_paused = true)]
async fn set_runtime_endpoint_only_by_the_feeding_bot() {
    let h = Harness::new(CAPTURE);
    let (a, b) = (SourceId(0), SourceId(1));
    let url = "http://127.0.0.1:9/in";
    assert_eq!(capture(&h, "hooks", b, "2:bbb", webhook(url, None)), None);
    assert_eq!(capture(&h, "p", b, "2:bbb", webhook(url, None)), None);
    assert_eq!(capture(&h, "nope", a, "1:aaa", webhook(url, None)), None);
    assert_eq!(runtime_url(&h, "hooks"), None);
    assert_eq!(
        capture(&h, "hooks", a, "1:aaa", webhook(url, Some("s1"))),
        Some(WebhookChange::Set)
    );
    assert_eq!(
        capture(&h, "hooks", a, "1:aaa", webhook(url, Some("s1"))),
        Some(WebhookChange::Unchanged)
    );
    assert_eq!(
        capture(&h, "hooks", a, "1:aaa", webhook(url, Some("s2"))),
        Some(WebhookChange::Set)
    );
    assert_eq!(runtime_url(&h, "hooks").as_deref(), Some(url));
    assert_eq!(
        capture(&h, "hooks", a, "1:aaa", None),
        Some(WebhookChange::Deleted)
    );
    assert_eq!(
        capture(&h, "hooks", a, "1:aaa", None),
        Some(WebhookChange::AlreadyDeleted)
    );
    assert_eq!(runtime_url(&h, "hooks"), None);

    let settings = h.engine.consumer_settings(a, "hooks").expect("hooks");
    assert!(settings.accept_set_webhook && !settings.honor_drop_pending);
    assert_eq!(settings.mode, tgin_core::engine::Mode::Push);
    let settings = h.engine.consumer_settings(b, "p").expect("p");
    assert!(!settings.accept_set_webhook);
    assert_eq!(settings.mode, tgin_core::engine::Mode::Pull);
    assert!(h.engine.consumer_settings(a, "nope").is_none());
    assert!(
        h.engine.consumer_settings(b, "hooks").is_none(),
        "not b's consumer"
    );
    assert!(
        h.engine.consumer_settings(a, "p").is_none(),
        "not a's consumer"
    );
    assert!(matches!(
        h.engine.consumer_for(a, "hooks"),
        Some(Consumer::Push(_))
    ));
    assert!(h.engine.consumer_for(b, "hooks").is_none());
    assert!(matches!(
        h.engine.consumer_for(b, "p"),
        Some(Consumer::Pull(_))
    ));
    assert!(h.engine.consumer_for(a, "p").is_none());
}

/// `drop_pending_updates` purges the caller's consumer only when it is the
/// bot's and honours the flag.
#[tokio::test(start_paused = true)]
async fn drop_pending_needs_the_feeding_bot_and_the_flag() {
    let honoring = CAPTURE
        .replace(
            "accept_set_webhook: true",
            "accept_set_webhook: true, honor_drop_pending: true",
        )
        .replace(
            "p: { pull: {} }",
            "p: { pull: { honor_drop_pending: true } }",
        );
    let h = Harness::new(&honoring);
    let (a, b) = (SourceId(0), SourceId(1));
    assert_eq!(admit_to(&h.engine, a, "message", 1), Admit::Admitted);
    assert_eq!(admit_to(&h.engine, b, "message", 2), Admit::Admitted);
    let drop = |source, token: &str, name| {
        h.engine
            .drop_pending(source, &tok(token), name)
            .expect("the token is the bot's")
    };
    assert_eq!(drop(b, "2:bbb", "hooks"), None, "not b's consumer");
    assert_eq!(drop(a, "1:aaa", "p"), None, "not a's consumer");
    assert_eq!(drop(a, "1:aaa", "nope"), None);
    assert_eq!(h.stats("hooks").available, 1);
    assert_eq!(h.stats("p").available, 1);
    assert_eq!(drop(a, "1:aaa", "hooks"), Some(1));
    assert_eq!(drop(b, "2:bbb", "p"), Some(1));
    assert_eq!(h.stats("hooks").available, 0);
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="p",reason="purge"} 1"#);

    let h = Harness::new(CAPTURE);
    assert_eq!(admit_to(&h.engine, a, "message", 1), Admit::Admitted);
    assert_eq!(
        h.engine.drop_pending(a, &tok("1:aaa"), "hooks"),
        Ok(None),
        "not honoured"
    );
    assert_eq!(h.stats("hooks").available, 1);
}

/// A consumer fed by several bots: one bot's `drop_pending_updates` drops
/// only that bot's updates, as Telegram drops only the caller's.
#[tokio::test(start_paused = true)]
async fn drop_pending_drops_only_the_callers_updates() {
    let yaml = r#"version: 1
server: {}
bots:
  - name: a
    token: "1:aaa"
    routes: [{ to: [shared] }]
  - name: b
    token: "2:bbb"
    routes: [{ to: [shared] }]
consumers:
  shared: { push: { endpoints: [{ url: "http://127.0.0.1:9/x" }], honor_drop_pending: true } }
"#;
    let h = Harness::new(yaml);
    let (a, b) = (SourceId(0), SourceId(1));
    // Paused, so the updates stay queued.
    assert!(h.engine.pause("shared"));
    assert_eq!(admit_to(&h.engine, a, "message", 1), Admit::Admitted);
    assert_eq!(admit_to(&h.engine, b, "message", 2), Admit::Admitted);
    assert_eq!(admit_to(&h.engine, a, "message", 3), Admit::Admitted);
    assert_eq!(h.stats("shared").available, 3);
    assert_eq!(
        h.engine.drop_pending(a, &tok("1:aaa"), "shared"),
        Ok(Some(2))
    );
    assert_eq!(h.stats("shared").available, 1, "b's update survives");
    h.assert_metric(r#"tgin_consumer_dropped_total{consumer="shared",reason="purge"} 2"#);
    assert_eq!(
        h.engine.drop_pending(b, &tok("2:bbb"), "shared"),
        Ok(Some(1))
    );
    assert_eq!(h.stats("shared").available, 0);
}

/// A consumer that one bot feeds after a reload still holds updates of the
/// bot that fed it before: the new feeder's `drop_pending_updates` keeps
/// them, as it drops only its own.
#[tokio::test(start_paused = true)]
async fn drop_pending_keeps_a_former_feeders_leftovers() {
    let config = |a_to: &str, x_to: &str| {
        format!(
            r#"version: 1
server: {{}}
bots:
  - name: a
    token: "1:aaa"
    routes: [{{ to: [{a_to}] }}]
  - name: x
    token: "2:xxx"
    routes: [{{ to: [{x_to}] }}]
consumers:
  shared: {{ push: {{ endpoints: [{{ url: "http://127.0.0.1:9/x" }}], honor_drop_pending: true }} }}
  other: {{ pull: {{}} }}
"#
        )
    };
    let h = Harness::new(&config("other", "shared"));
    let (a, x) = (SourceId(0), SourceId(1));
    // Paused, so the updates stay queued.
    assert!(h.engine.pause("shared"));
    assert_eq!(admit_to(&h.engine, x, "message", 1), Admit::Admitted);
    h.engine
        .apply(&load(&config("shared", "other")))
        .expect("apply");
    assert_eq!(admit_to(&h.engine, a, "message", 2), Admit::Admitted);
    assert_eq!(h.stats("shared").available, 2);
    assert_eq!(
        h.engine.drop_pending(a, &tok("1:aaa"), "shared"),
        Ok(Some(1))
    );
    assert_eq!(h.stats("shared").available, 1, "x's leftover stays");
}

/// A runtime endpoint belongs to the bot whose `setWebhook` set it:
/// a reload that keeps the consumer and its bot keeps it; one that moves
/// the consumer to another bot, leaves it with none, or turns
/// `accept_set_webhook` off clears it.
#[tokio::test(start_paused = true)]
async fn runtime_endpoint_cleared_when_feeding_bot_changes() {
    let h = Harness::new(CAPTURE);
    let (a, b) = (SourceId(0), SourceId(1));
    let url = "http://127.0.0.1:9/in";
    assert!(capture(&h, "hooks", a, "1:aaa", webhook(url, None)).is_some());

    let kept = CAPTURE.replace(
        "accept_set_webhook: true",
        "accept_set_webhook: true, max_in_flight: 3, breaker: { failure_threshold: 2 }",
    );
    h.engine.apply(&load(&kept)).expect("apply");
    assert_eq!(runtime_url(&h, "hooks").as_deref(), Some(url), "same bot");

    let moved = CAPTURE
        .replace("routes: [{ to: [hooks] }]", "routes: [{ to: [p] }]")
        .replace(
            "token: \"2:bbb\"\n    routes: [{ to: [p] }]",
            "token: \"2:bbb\"\n    routes: [{ to: [hooks] }]",
        );
    h.engine.apply(&load(&moved)).expect("apply");
    assert_eq!(runtime_url(&h, "hooks"), None, "b feeds it now");
    assert_eq!(
        LOGS.count(&[
            "runtime endpoint cleared",
            "consumer=hooks",
            "no longer feeds"
        ]),
        1,
        "the clearing is logged"
    );
    assert!(capture(&h, "hooks", a, "1:aaa", webhook(url, None)).is_none());
    assert!(capture(&h, "hooks", b, "2:bbb", webhook(url, None)).is_some());

    let orphaned = moved
        .replace(
            "token: \"2:bbb\"\n    routes: [{ to: [hooks] }]",
            "token: \"2:bbb\"\n    routes: [{ to: [q] }]",
        )
        .replace(
            "  p: { pull: {} }\n",
            "  p: { pull: {} }\n  q: { pull: {} }\n",
        );
    h.engine.apply(&load(&orphaned)).expect("apply");
    assert_eq!(runtime_url(&h, "hooks"), None, "no bot feeds it");
    assert!(capture(&h, "hooks", b, "2:bbb", webhook(url, None)).is_none());

    h.engine.apply(&load(CAPTURE)).expect("apply");
    assert!(capture(&h, "hooks", a, "1:aaa", webhook(url, None)).is_some());
    let refused = CAPTURE.replace(
        "hooks: { push: { accept_set_webhook: true } }",
        "hooks: { push: { endpoints: [{ url: \"http://127.0.0.1:9/cfg\" }] } }",
    );
    h.engine.apply(&load(&refused)).expect("apply");
    assert_eq!(runtime_url(&h, "hooks"), None, "accept_set_webhook is off");
    assert!(capture(&h, "hooks", a, "1:aaa", webhook(url, None)).is_none());
}

/// A reload that gives the setting bot another token clears its runtime
/// endpoint: rotating a leaked token evicts what the old token's holder
/// set, and the bot's new holder sets its own.
#[tokio::test(start_paused = true)]
async fn runtime_endpoint_cleared_when_token_changes() {
    let h = Harness::new(CAPTURE);
    let a = SourceId(0);
    let url = "http://127.0.0.1:9/in";
    assert_eq!(
        capture(&h, "hooks", a, "1:aaa", webhook(url, None)),
        Some(WebhookChange::Set)
    );
    let rotated = CAPTURE.replace("token: \"1:aaa\"", "token: \"9:zzz\"");
    h.engine.apply(&load(&rotated)).expect("apply");
    assert_eq!(h.engine.source_id("a"), Some(a), "same bot name, same id");
    assert_eq!(runtime_url(&h, "hooks"), None, "the token changed");
    assert_eq!(
        LOGS.count(&["runtime endpoint cleared", "consumer=hooks", "token"]),
        1
    );
    // The new token's holder sets it again; the same token keeps it.
    assert_eq!(
        capture(&h, "hooks", a, "9:zzz", webhook(url, None)),
        Some(WebhookChange::Set)
    );
    h.engine.apply(&load(&rotated)).expect("apply");
    assert_eq!(runtime_url(&h, "hooks").as_deref(), Some(url));
}

/// A request that authenticated with a token the bot no longer has (a
/// reload rotated it while the request was in flight) changes nothing:
/// neither a runtime endpoint nor a purge.
#[tokio::test(start_paused = true)]
async fn bot_scoped_changes_refuse_a_rotated_token() {
    let honoring = CAPTURE.replace(
        "accept_set_webhook: true",
        "accept_set_webhook: true, honor_drop_pending: true",
    );
    let h = Harness::new(&honoring);
    let a = SourceId(0);
    let url = "http://127.0.0.1:9/in";
    h.engine
        .apply(&load(&honoring.replace("1:aaa", "9:zzz")))
        .expect("apply");
    assert_eq!(admit_to(&h.engine, a, "message", 1), Admit::Admitted);
    let old = tok("1:aaa");
    assert_eq!(
        h.engine
            .set_runtime_endpoint("hooks", a, &old, webhook(url, None)),
        Err(TokenChanged)
    );
    assert_eq!(
        h.engine.set_runtime_endpoint("hooks", a, &old, None),
        Err(TokenChanged)
    );
    assert_eq!(runtime_url(&h, "hooks"), None);
    assert_eq!(h.engine.drop_pending(a, &old, "hooks"), Err(TokenChanged));
    assert_eq!(h.stats("hooks").available, 1, "nothing purged");
    // Another bot's token is no better; the current one works.
    assert_eq!(
        h.engine.drop_pending(a, &tok("2:bbb"), "hooks"),
        Err(TokenChanged)
    );
    assert_eq!(
        h.engine.drop_pending(a, &tok("9:zzz"), "hooks"),
        Ok(Some(1))
    );
}

// ------------------------------------------------------------- accounts

/// One account, `acct`: new messages go to `raw`, channel posts to the
/// `tl_json` consumers `r1` and `r2`.
const ACCOUNT: &str = r#"version: 1
server: {}
accounts:
  acct:
    api_id: 1
    api_hash: h
    session_dir: sessions/acct
    routes:
      - match: { constructors: [updateNewMessage] }
        to: [raw]
      - match: { constructors: [updateNewChannelMessage] }
        to: [{ all: [r1, r2] }]
      - match: { constructors: [updateEditMessage] }
        to: [{ all: [raw, r1] }]
consumers:
  raw: { pull: { auth_token: t0, capacity: 3 } }
  r1: { pull: { auth_token: t1, render: tl_json } }
  r2: { pull: { auth_token: t2, render: tl_json } }
"#;

/// A batch update that counts how often each payload is built.
struct Counted {
    update_type: &'static str,
    id: i64,
    payloads: usize,
    rendered: usize,
}

impl Counted {
    fn new(update_type: &'static str, id: i64) -> Self {
        Self {
            update_type,
            id,
            payloads: 0,
            rendered: 0,
        }
    }
}

impl BatchUpdate for Counted {
    fn facts(&self) -> Facts<'_> {
        Facts {
            update_type: self.update_type,
            chat_id: Some(7),
            ..Facts::default()
        }
    }

    fn update_id(&self) -> i64 {
        self.id
    }

    fn update_type(&self) -> &'static str {
        self.update_type
    }

    fn payload(&mut self) -> Bytes {
        self.payloads += 1;
        Bytes::from(format!(r#"{{"update_id":{},"form":"raw"}}"#, self.id))
    }

    fn rendered(&mut self) -> Bytes {
        self.rendered += 1;
        Bytes::from(format!(r#"{{"update_id":{},"form":"tl_json"}}"#, self.id))
    }
}

async fn payloads(c: &PullConsumer) -> Vec<String> {
    let resp = c
        .poll(req("x", 0, Duration::ZERO), &CancellationToken::new())
        .await;
    resp.items
        .iter()
        .map(|b| String::from_utf8(b.to_vec()).expect("UTF-8"))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn rendered_built_once_and_only_for_tl_json_targets() {
    let h = Harness::new(ACCOUNT);
    let acct = h.engine.account_source("acct").expect("an account source");
    let mut batch = [
        Counted::new("updateNewMessage", 1),
        Counted::new("updateNewChannelMessage", 2),
        Counted::new("updateUserStatus", 3),
    ];
    assert_eq!(h.engine.admit_batch(acct, &mut batch), Admit::Admitted);
    assert_eq!((batch[0].payloads, batch[0].rendered), (1, 0), "raw only");
    assert_eq!(
        (batch[1].payloads, batch[1].rendered),
        (0, 1),
        "rendered once for two tl_json consumers, and no raw form"
    );
    assert_eq!((batch[2].payloads, batch[2].rendered), (0, 0), "unrouted");
    assert_eq!(
        payloads(&h.pull("raw")).await,
        [r#"{"update_id":1,"form":"raw"}"#]
    );
    for c in ["r1", "r2"] {
        assert_eq!(
            payloads(&h.pull(c)).await,
            [r#"{"update_id":2,"form":"tl_json"}"#]
        );
    }
    let mut none = [Counted::new("updateUserStatus", 4)];
    assert_eq!(h.engine.admit_batch(acct, &mut none), Admit::Unroutable);
    let mut both = [Counted::new("updateEditMessage", 5)];
    assert_eq!(h.engine.admit_batch(acct, &mut both), Admit::Admitted);
    assert_eq!(
        (both[0].payloads, both[0].rendered),
        (1, 1),
        "each form once"
    );
}

#[tokio::test(start_paused = true)]
async fn batch_is_admitted_whole_or_not_at_all() {
    let h = Harness::new(ACCOUNT);
    let acct = h.engine.account_source("acct").expect("an account source");
    let mut first = [
        Counted::new("updateNewMessage", 1),
        Counted::new("updateNewMessage", 2),
    ];
    assert_eq!(h.engine.admit_batch(acct, &mut first), Admit::Admitted);
    // Room for one more, and the batch has two: neither is queued.
    let mut second = [
        Counted::new("updateNewMessage", 3),
        Counted::new("updateNewMessage", 4),
    ];
    assert_eq!(h.engine.admit_batch(acct, &mut second), Admit::Backpressure);
    assert_eq!(h.stats("raw").available, 2);
    assert_eq!(h.stats("raw").reserved, 0, "every reservation released");
    assert_eq!(poll(&h.pull("raw"), "x", 0).await, [1, 2]);
    assert_eq!(poll(&h.pull("raw"), "x", 3).await, Vec::<i64>::new());
    assert_eq!(h.engine.admit_batch(acct, &mut second), Admit::Admitted);
    assert_eq!(poll(&h.pull("raw"), "x", 0).await, [3, 4], "in batch order");
}

/// A bot source, or a source no rule names, cannot purge an account's
/// queue: `honor_drop_pending` applies to bots' own updates only.
#[tokio::test(start_paused = true)]
async fn account_ids_survive_reloads_and_bots_cannot_purge_them() {
    let with_bot = ACCOUNT.replace(
        "accounts:",
        "bots:\n  - { name: b, token: \"1:aaa\", routes: [{ to: [other] }] }\naccounts:",
    ) + "  other: { pull: { honor_drop_pending: true } }\n";
    let h = Harness::new(&with_bot);
    let acct = h.engine.account_source("acct").expect("an account source");
    let bot = h.engine.source_id("b").expect("a bot source");
    assert_ne!(acct, bot);
    assert_eq!(h.engine.source_id("acct"), None, "an account is no bot");
    h.engine.apply(&load(&with_bot)).expect("apply");
    assert_eq!(h.engine.account_source("acct"), Some(acct));
    let mut batch = [Counted::new("updateNewMessage", 1)];
    assert_eq!(h.engine.admit_batch(acct, &mut batch), Admit::Admitted);
    assert_eq!(h.engine.drop_pending(bot, &tok("1:aaa"), "raw"), Ok(None));
    assert_eq!(h.stats("raw").available, 1);
}

/// A reload never gives a consumer the other kind of updates while it
/// still holds, or is still given, the kind it had: it is refused, and
/// the consumer keeps its queue. Once nothing feeds it and it is empty,
/// it may change.
#[tokio::test(start_paused = true)]
async fn reload_refuses_turning_a_bot_queue_into_an_account_queue() {
    const BOT_FED: &str = r#"version: 1
server: {}
bots:
  - { name: b, token: "1:aaa", routes: [{ to: [q] }] }
accounts:
  acct: { api_id: 1, api_hash: h, session_dir: sessions/acct }
consumers:
  q: { pull: { auth_token: t } }
"#;
    let unfed = BOT_FED.replace("routes: [{ to: [q] }]", "routes: []");
    let account_fed = unfed.replace(
        "session_dir: sessions/acct }",
        "session_dir: sessions/acct, routes: [{ to: [q] }] }",
    );
    let h = Harness::new(BOT_FED);
    let bot = h.engine.source_id("b").expect("bot");
    assert_eq!(admit_to(&h.engine, bot, "message", 1), Admit::Admitted);

    let err = h.engine.apply(&load(&account_fed)).expect_err("refused");
    assert!(
        matches!(&err, BuildError::FeedChanged { consumer, from: Feed::Bots, to: Feed::Account } if consumer == "q"),
        "{err}"
    );
    assert_eq!(h.stats("q").available, 1, "the queue is kept");
    assert_eq!(
        h.engine.pull_consumer_fed("q").map(|(_, f)| f),
        Some(Some(Feed::Bots))
    );

    // Unfed, it still holds a bot update: still refused.
    h.engine.apply(&load(&unfed)).expect("unfed");
    assert!(h.engine.apply(&load(&account_fed)).is_err());
    // Drained, it may change.
    assert_eq!(poll(&h.pull("q"), "x", 0).await, [1]);
    assert_eq!(poll(&h.pull("q"), "x", -1).await, Vec::<i64>::new());
    h.engine.apply(&load(&account_fed)).expect("drained");
    assert_eq!(
        h.engine.pull_consumer_fed("q").map(|(_, f)| f),
        Some(Some(Feed::Account))
    );

    // And back: refused while the account still feeds it.
    let acct = h.engine.account_source("acct").expect("account");
    let mut batch = [Counted::new("updateNewMessage", 1)];
    assert_eq!(h.engine.admit_batch(acct, &mut batch), Admit::Admitted);
    let err = h.engine.apply(&load(BOT_FED)).expect_err("refused");
    assert!(
        matches!(
            err,
            BuildError::FeedChanged {
                from: Feed::Account,
                to: Feed::Bots,
                ..
            }
        ),
        "{err}"
    );
}

/// A batch larger than a `backpressure` consumer's `capacity` is taken
/// whole once the consumer is empty, so it never waits for room that
/// cannot come; while the consumer holds anything, it waits.
#[tokio::test(start_paused = true)]
async fn oversized_batch_admitted_into_an_empty_queue() {
    let h = Harness::new(ACCOUNT);
    let acct = h.engine.account_source("acct").expect("an account source");
    let mut five: Vec<Counted> = (1..=5)
        .map(|i| Counted::new("updateNewMessage", i))
        .collect();
    assert_eq!(h.engine.admit_batch(acct, &mut five), Admit::Admitted);
    assert_eq!(h.stats("raw").available, 5, "capacity 3, one batch over");
    let mut more: Vec<Counted> = (6..=9)
        .map(|i| Counted::new("updateNewMessage", i))
        .collect();
    assert_eq!(h.engine.admit_batch(acct, &mut more), Admit::Backpressure);
    let mut one = [Counted::new("updateNewMessage", 10)];
    assert_eq!(
        h.engine.admit_batch(acct, &mut one),
        Admit::Backpressure,
        "full"
    );
    assert_eq!(poll(&h.pull("raw"), "x", 0).await, [1, 2, 3, 4, 5]);
    // Leased is not empty.
    assert_eq!(h.engine.admit_batch(acct, &mut more), Admit::Backpressure);
    assert_eq!(poll(&h.pull("raw"), "x", 6).await, Vec::<i64>::new());
    assert_eq!(h.engine.admit_batch(acct, &mut more), Admit::Admitted);
    assert_eq!(poll(&h.pull("raw"), "x", 0).await, [6, 7, 8, 9]);
}

/// A `drop_oldest` consumer short of room keeps a batch's newest updates,
/// as it would admitting them one by one.
#[tokio::test(start_paused = true)]
async fn drop_oldest_keeps_a_batch_s_newest_updates() {
    let yaml = ACCOUNT.replace(
        "raw: { pull: { auth_token: t0, capacity: 3 } }",
        "raw: { pull: { auth_token: t0, capacity: 2, on_full: drop_oldest } }",
    );
    let h = Harness::new(&yaml);
    let acct = h.engine.account_source("acct").expect("an account source");
    let mut batch: Vec<Counted> = (1..=5)
        .map(|i| Counted::new("updateNewMessage", i))
        .collect();
    assert_eq!(h.engine.admit_batch(acct, &mut batch), Admit::Admitted);
    assert_eq!(poll(&h.pull("raw"), "x", 0).await, [4, 5]);
}

/// A reload refused for a consumer's feed creates nothing: no metric series
/// for the bots and consumers it would have added. Its message says how to
/// proceed.
#[tokio::test(start_paused = true)]
async fn refused_feed_change_creates_nothing() {
    const BOT_FED: &str = r#"version: 1
server: {}
bots:
  - { name: b, token: "1:aaa", routes: [{ to: [q] }] }
accounts:
  acct: { api_id: 1, api_hash: h, session_dir: sessions/acct }
consumers:
  q: { pull: { auth_token: t } }
"#;
    let h = Harness::new(BOT_FED);
    let refused = r#"version: 1
server: {}
bots:
  - { name: newbot, token: "2:bbb", routes: [{ to: [freshq] }] }
accounts:
  acct: { api_id: 1, api_hash: h, session_dir: sessions/acct, routes: [{ to: [q] }] }
consumers:
  freshq: { pull: {} }
  q: { pull: { auth_token: t } }
"#;
    let err = h.engine.apply(&load(refused)).expect_err("refused");
    let text = err.to_string();
    assert!(
        text.contains("another name") && text.contains("purge"),
        "{text}"
    );
    let metrics = h.metrics();
    assert!(!metrics.contains("freshq"), "{metrics}");
    assert!(!metrics.contains("newbot"), "{metrics}");
    assert_eq!(h.engine.consumer("freshq").map(|c| c.pending()), None);
}

/// An oversized batch gets in while a steady trickle of small batches
/// keeps the consumer busy but within its limits: it never waits for an
/// empty queue. Once in, the consumer takes nothing more until it is back
/// within its limits.
#[tokio::test(start_paused = true)]
async fn oversized_batch_gets_in_beside_a_trickle() {
    let h = Harness::new(ACCOUNT);
    let acct = h.engine.account_source("acct").expect("an account source");
    let raw = h.pull("raw");
    let mut id = 0;
    let mut next = |n: i64| -> Vec<Counted> {
        let batch = (id + 1..=id + n)
            .map(|i| Counted::new("updateNewMessage", i))
            .collect();
        id += n;
        batch
    };
    // The trickle: each round one update comes in and the one before it
    // is acked, so the consumer never empties.
    for n in 1..=3 {
        let mut small = next(1);
        assert_eq!(h.engine.admit_batch(acct, &mut small), Admit::Admitted);
        assert_eq!(poll(&raw, "x", n).await, [n]);
        assert_eq!(h.stats("raw").available + h.stats("raw").leased, 1);
    }
    let mut big = next(5);
    assert_eq!(
        h.engine.admit_batch(acct, &mut big),
        Admit::Admitted,
        "capacity 3, one update held: within limits"
    );
    assert_eq!(h.stats("raw").available + h.stats("raw").leased, 6);
    let mut small = next(1);
    assert_eq!(
        h.engine.admit_batch(acct, &mut small),
        Admit::Backpressure,
        "over"
    );
}
