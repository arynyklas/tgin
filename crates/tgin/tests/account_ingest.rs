//! An account's batches through [`AccountIngest`] into pull and push
//! consumers, fed by a real update manager from scripted TL.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use serde_json::Value;
use tgin::account_ingest::AccountIngest;
use tgin_botapi::pull::{PollPath, PullRoutes};
use tgin_core::config::{self, Config, ConsumerKind};
use tgin_core::engine::{Consumer, Engine, EngineRuntime};
use tgin_core::ids::SourceId;
use tgin_core::metrics::CoreMetrics;
use tgin_core::reply::{Reply, ReplyExecutor, ReplyHandle};
use tgin_mtproto::tl_json::from_json;
use tgin_mtproto::{
    Admission, Batch, BatchSink, Fetch, IdsError, SessionStore, StoredState, UpdateIds,
    UpdateManager, UpdateState,
};
use tgin_runtime::config::MapEnv;
use tgin_runtime::obs::MetricsRegistry;
use tgin_runtime::supervisor::Supervisor;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Every message's text: account content, which must never reach a log.
const SECRET: &str = "s3cr3t-account-content";
/// Bound on every wait.
const WAIT: Duration = Duration::from_secs(10);

/// Everything the tests log, at every level.
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

// ------------------------------------------------------------ the engine

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

/// The account `main` with `routes` and `consumers` (YAML fragments).
fn account_config(routes: &str, consumers: &str) -> String {
    format!(
        "version: 1\nserver: {{}}\naccounts:\n  main:\n    api_id: 1\n    api_hash: h\n    session_dir: sessions/main\n    routes:\n{routes}consumers:\n{consumers}"
    )
}

struct Harness {
    cfg: Config,
    engine: Arc<Engine>,
    ingest: AccountIngest,
    force: CancellationToken,
    _dir: tempfile::TempDir,
    store: SessionStore,
    ids: UpdateIds,
}

impl Harness {
    fn new(yaml: &str, reply: ReplyHandle) -> Self {
        LazyLock::force(&LOGS);
        let cfg = load(yaml);
        let mut registry = MetricsRegistry::new("test", "test");
        let force = CancellationToken::new();
        let engine = Arc::new(
            Engine::new(
                &cfg,
                CoreMetrics::register(registry.registry_mut()),
                reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .expect("client"),
                EngineRuntime {
                    supervisor: Arc::new(Supervisor::new()),
                    stop: CancellationToken::new(),
                    force: force.clone(),
                    via: tgin_runtime::http::Via::generate(),
                    reply,
                },
            )
            .expect("engine"),
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::open(&dir.path().join("main")).expect("store");
        let ids = UpdateIds::load(&store).expect("ids");
        Self {
            ingest: AccountIngest::new(Arc::clone(&engine), "main"),
            cfg,
            engine,
            force,
            _dir: dir,
            store,
            ids,
        }
    }

    fn admit(&mut self, batch: &Batch) -> Admission {
        self.ingest
            .admit(batch, &mut self.ids, &self.store)
            .expect("ids handed out")
    }

    fn pending(&self, consumer: &str) -> usize {
        self.engine.consumer(consumer).expect("consumer").pending()
    }

    fn pull_routes(&self) -> PullRoutes {
        let auth = self
            .cfg
            .consumers
            .iter()
            .filter_map(|c| match &c.kind {
                ConsumerKind::Pull(pull) => {
                    Some((Arc::<str>::from(c.name.as_str()), pull.auth_token.clone()))
                }
                ConsumerKind::Push(_) => None,
            })
            .collect::<HashMap<_, _>>();
        PullRoutes::new(Arc::clone(&self.engine), auth, Vec::new())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.force.cancel();
    }
}

// ---------------------------------------------------------------- TL

fn tl(json: &str) -> Bytes {
    let mut out = Vec::new();
    from_json(json, &mut out).unwrap_or_else(|e| panic!("{e}: {json}"));
    Bytes::from(out)
}

/// A private message from user 3 with the secret text.
fn message(id: i32) -> String {
    format!(
        r#"{{"_":"message","id":{id},"peer_id":{{"_":"peerUser","user_id":"3"}},"date":1,"message":"{SECRET} {id}"}}"#
    )
}

fn new_message(pts: i32) -> String {
    format!(
        r#"{{"_":"updateNewMessage","pts":{pts},"pts_count":1,"message":{}}}"#,
        message(pts)
    )
}

/// An `updates` push of `updateNewMessage`s with these pts.
fn push(pts: &[i32]) -> Bytes {
    let updates: Vec<String> = pts.iter().map(|&p| new_message(p)).collect();
    tl(&format!(
        r#"{{"_":"updates","updates":[{}],"users":[],"chats":[],"date":1,"seq":0}}"#,
        updates.join(",")
    ))
}

/// A getDifference answer with messages of these pts, up to the last.
fn difference(pts: &[i32]) -> Bytes {
    let messages: Vec<String> = pts.iter().map(|&p| message(p)).collect();
    let last = pts.last().copied().unwrap_or(100);
    tl(&format!(
        r#"{{"_":"updates.difference","new_messages":[{}],"new_encrypted_messages":[],"other_updates":[],"chats":[],"users":[],
        "state":{{"_":"updates.state","pts":{last},"qts":0,"date":2,"seq":0,"unread_count":0}}}}"#,
        messages.join(",")
    ))
}

/// A manager at common pts 100, past its start-up difference.
fn manager() -> UpdateManager {
    let mut m = UpdateManager::new(StoredState {
        common: UpdateState {
            pts: 100,
            qts: 0,
            seq: 0,
            date: 1,
        },
        ..StoredState::default()
    });
    assert!(matches!(
        m.next_action(),
        Some(Fetch::GetDifference { pts: 100, .. })
    ));
    for b in m.feed_difference(tl(r#"{"_":"updates.differenceEmpty","date":1,"seq":0}"#)) {
        m.committed(b.id);
    }
    m
}

/// The one batch with updates among `batches`; the others are committed.
fn the_batch(m: &mut UpdateManager, batches: Vec<Batch>) -> Batch {
    let mut found = None;
    for b in batches {
        if b.updates.is_empty() {
            m.committed(b.id);
        } else {
            assert!(found.is_none(), "one batch with updates");
            found = Some(b);
        }
    }
    found.expect("a batch with updates")
}

/// The batch of a push of new messages with these pts.
fn pushed(m: &mut UpdateManager, pts: &[i32]) -> Batch {
    let batches = m.feed_push(push(pts));
    the_batch(m, batches)
}

/// The batch of a getDifference answer with messages of these pts.
fn recovered(m: &mut UpdateManager, pts: &[i32]) -> Batch {
    let batches = m.feed_difference(difference(pts));
    the_batch(m, batches)
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("JSON")
}

// ------------------------------------------------------------- pulling

async fn get(
    routes: &PullRoutes,
    name: &str,
    path: PollPath,
    query: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::get(format!("/x?{query}")).header("x-tgin-instance", "a");
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let resp = routes
        .get_updates(name, path, req.body(Body::empty()).expect("request"))
        .await;
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, json(&body))
}

fn ids(v: &Value) -> Vec<i64> {
    v["result"]
        .as_array()
        .expect("a result array")
        .iter()
        .map(|e| e["update_id"].as_i64().expect("update_id"))
        .collect()
}

const PULL: &str =
    "      - match: { constructors: [updateNewMessage] }\n        to: [feed, rendered]\n";
const PULL_CONSUMERS: &str = "  feed: { pull: { auth_token: tok, capacity: 3 } }\n  rendered: { pull: { auth_token: tok2, render: tl_json } }\n";

#[tokio::test]
async fn tl_json_variant_only_when_asked() {
    let mut h = Harness::new(
        &account_config(PULL, PULL_CONSUMERS),
        ReplyHandle::default(),
    );
    let mut m = manager();
    let batch = pushed(&mut m, &[101]);
    assert_eq!(h.admit(&batch), Admission::Admitted);
    let routes = h.pull_routes();

    let (status, raw) = get(&routes, "feed", PollPath::Account, "timeout=0", Some("tok")).await;
    assert_eq!(status, StatusCode::OK);
    let raw = &raw["result"][0];
    assert!(
        raw.get("update").is_none(),
        "no TL-JSON unless asked: {raw}"
    );

    let (status, rendered) = get(
        &routes,
        "rendered",
        PollPath::Account,
        "timeout=0",
        Some("tok2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rendered = &rendered["result"][0];
    assert_eq!(rendered["update"]["_"], "updateNewMessage");
    assert_eq!(
        rendered["update"]["message"]["message"],
        format!("{SECRET} 101")
    );
    assert_eq!(rendered["update_id"], raw["update_id"], "the same update");
    assert_eq!(rendered["tl"], raw["tl"]);
}

#[tokio::test]
async fn envelope_carries_the_update_and_its_entities() {
    let mut h = Harness::new(
        &account_config(PULL, PULL_CONSUMERS),
        ReplyHandle::default(),
    );
    let mut m = manager();
    let batch = pushed(&mut m, &[101]);
    assert_eq!(h.admit(&batch), Admission::Admitted);
    let (_, v) = get(&h.pull_routes(), "feed", PollPath::Account, "", Some("tok")).await;
    let e = &v["result"][0];
    assert_eq!(e["update_id"], 1);
    assert_eq!(e["account"], "main");
    assert_eq!(e["layer"], tgin_mtproto::schema::API.layer);
    assert_eq!(e["constructor"], "updateNewMessage");
    let b64 = |k: &str| STANDARD.decode(e[k].as_str().expect(k)).expect("base64");
    assert_eq!(b64("tl"), batch.updates[0].bytes().to_vec());
    assert_eq!(b64("users"), batch.users.to_vec());
    assert_eq!(b64("chats"), batch.chats.to_vec());
}

#[tokio::test(start_paused = true)]
async fn backpressure_drops_sequenced_without_commit_then_recovered() {
    let mut h = Harness::new(
        &account_config(PULL, PULL_CONSUMERS),
        ReplyHandle::default(),
    );
    let mut m = manager();
    let routes = h.pull_routes();

    // Two of `feed`'s three places taken.
    let a = pushed(&mut m, &[101, 102]);
    assert_eq!(h.admit(&a), Admission::Admitted);
    let delta = m.committed(a.id).expect("a state to persist");
    assert_eq!(delta.pts, Some(102));

    // A batch of two finds room for one: none of it is queued, and the
    // caller refuses it, so its state is never committed.
    let b = pushed(&mut m, &[103, 104]);
    assert_eq!(h.admit(&b), Admission::Refused);
    assert_eq!(h.pending("feed"), 2, "nothing of the refused batch");
    assert_eq!(h.pending("rendered"), 2, "nor in the consumer with room");
    assert!(
        m.refused(b.id)
            .is_none_or(|d| d.pts.is_none_or(|p| p <= 102))
    );
    assert_eq!(m.next_action(), None, "not at once: the refetch backs off");
    tokio::time::advance(tgin_mtproto::updates::RETRY_MIN).await;
    assert!(
        matches!(m.next_action(), Some(Fetch::GetDifference { pts: 102, .. })),
        "gap recovery asks from the committed pts"
    );

    // The consumers catch up; the difference brings the updates back.
    let (_, v) = get(&routes, "feed", PollPath::Account, "", Some("tok")).await;
    let first = ids(&v);
    assert_eq!(first.len(), 2);
    let offset = first[1] + 1;
    let (_, v) = get(
        &routes,
        "feed",
        PollPath::Account,
        &format!("offset={offset}"),
        Some("tok"),
    )
    .await;
    assert!(ids(&v).is_empty());
    let (_, v) = get(&routes, "rendered", PollPath::Account, "", Some("tok2")).await;
    assert_eq!(ids(&v), first, "the same updates, with the same ids");
    let (_, v) = get(
        &routes,
        "rendered",
        PollPath::Account,
        "offset=-1",
        Some("tok2"),
    )
    .await;
    assert!(ids(&v).is_empty());
    assert_eq!(h.pending("rendered"), 0);

    let c = recovered(&mut m, &[103, 104]);
    assert_eq!(c.updates.len(), 2);
    assert_eq!(h.admit(&c), Admission::Admitted);
    assert_eq!(m.committed(c.id).expect("committed").pts, Some(104));
    let (_, v) = get(&routes, "feed", PollPath::Account, "", Some("tok")).await;
    let again = ids(&v);
    assert_eq!(again.len(), 2);
    assert!(
        again[0] > first[1] && again[0] < again[1],
        "{first:?} then {again:?}"
    );
    let texts: Vec<String> = v["result"]
        .as_array()
        .expect("result")
        .iter()
        .map(|e| e["constructor"].as_str().expect("constructor").to_owned())
        .collect();
    assert_eq!(texts, ["updateNewMessage", "updateNewMessage"]);
}

#[tokio::test]
async fn pull_offset_acks() {
    let mut h = Harness::new(
        &account_config(PULL, PULL_CONSUMERS),
        ReplyHandle::default(),
    );
    let mut m = manager();
    let batch = pushed(&mut m, &[101, 102, 103]);
    assert_eq!(h.admit(&batch), Admission::Admitted);
    let routes = h.pull_routes();

    // The bearer token is required, and checked.
    let (status, _) = get(&routes, "feed", PollPath::Account, "", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = get(&routes, "feed", PollPath::Account, "", Some("tok2")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Never on the Bot API path; aliases serve what the consumer holds.
    let (status, _) = get(&routes, "feed", PollPath::Bot, "", Some("tok")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, v) = get(&routes, "feed", PollPath::Account, "limit=2", Some("tok")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["ok"], true);
    let got = ids(&v);
    assert_eq!(got.len(), 2);
    // The same offset hands the same updates back.
    let (_, v) = get(&routes, "feed", PollPath::Alias, "limit=2", Some("tok")).await;
    assert_eq!(ids(&v), got);
    // An offset past the first acks it.
    let (_, v) = get(
        &routes,
        "feed",
        PollPath::Account,
        &format!("offset={}", got[1]),
        Some("tok"),
    )
    .await;
    assert_eq!(ids(&v), [got[1]]);
    assert_eq!(h.pending("feed"), 2);
    let (_, v) = get(
        &routes,
        "feed",
        PollPath::Account,
        &format!("offset={}", got[1] + 1),
        Some("tok"),
    )
    .await;
    let last = ids(&v);
    assert_eq!(last.len(), 1);
    assert!(last[0] > got[1]);
    let (_, v) = get(
        &routes,
        "feed",
        PollPath::Account,
        &format!("offset={}", last[0] + 1),
        Some("tok"),
    )
    .await;
    assert!(ids(&v).is_empty());
    assert_eq!(h.pending("feed"), 0);
}

#[tokio::test]
async fn pull_body_splices_the_envelopes() {
    let mut h = Harness::new(
        &account_config(PULL, PULL_CONSUMERS),
        ReplyHandle::default(),
    );
    let mut m = manager();
    let batch = pushed(&mut m, &[101, 102]);
    assert_eq!(h.admit(&batch), Admission::Admitted);
    let Some(Consumer::Pull(feed)) = h.engine.consumer("feed") else {
        panic!("feed is a pull consumer");
    };
    let leased = feed.queue().stats();
    assert_eq!(leased.available, 2);
    let resp = h
        .pull_routes()
        .get_updates(
            "feed",
            PollPath::Account,
            Request::get("/x")
                .header("x-tgin-instance", "a")
                .header("authorization", "Bearer tok")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("body");
    let v = json(&body);
    let parts: Vec<String> = v["result"]
        .as_array()
        .expect("result")
        .iter()
        .map(|e| serde_json::to_string(e).expect("JSON"))
        .collect();
    assert!(
        body.starts_with(b"{\"ok\":true,\"result\":[{\"update_id\":"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(parts.len(), 2);
}

// ------------------------------------------------------------- pushing

/// Counts the replies it is asked to execute.
#[derive(Default)]
struct CountingExecutor(AtomicUsize);

impl ReplyExecutor for CountingExecutor {
    fn execute<'a>(
        &'a self,
        _consumer: &'a str,
        _source: SourceId,
        _reply: Reply,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }

    fn too_large(&self, _consumer: &str, _source: SourceId) {}

    fn forget(&self, _consumer: &str) {}
}

#[tokio::test]
async fn push_one_object_per_request() {
    let server = MockServer::start().await;
    // A reply a bot's endpoint could send: an account consumer ignores it.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"method":"sendMessage","chat_id":3,"text":"x"}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let executor = Arc::new(CountingExecutor::default());
    let reply = ReplyHandle::default();
    assert!(
        reply
            .set(Arc::clone(&executor) as Arc<dyn ReplyExecutor>)
            .is_ok()
    );
    let consumers = format!(
        "  hook: {{ push: {{ endpoints: [{{ url: \"{}/in\" }}], max_in_flight: 1 }} }}\n",
        server.uri()
    );
    let routes = "      - to: [hook]\n";
    let mut h = Harness::new(&account_config(routes, &consumers), reply);
    let mut m = manager();
    let batch = pushed(&mut m, &[101, 102, 103]);
    assert_eq!(h.admit(&batch), Admission::Admitted);

    tokio::time::timeout(WAIT, async {
        while h.pending("hook") > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("all delivered");
    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(requests.len(), 3, "one request per update");
    let mut seen = Vec::new();
    for (r, update) in requests.iter().zip(&batch.updates) {
        let v = json(&r.body);
        assert!(v.is_object(), "one object, not an array");
        assert_eq!(v["account"], "main");
        assert_eq!(
            STANDARD.decode(v["tl"].as_str().expect("tl")).expect("b64"),
            update.bytes().to_vec()
        );
        assert_eq!(
            r.headers.get("content-type").map(|v| v.as_bytes()),
            Some(&b"application/json"[..])
        );
        assert_eq!(
            r.headers.get("x-tgin-source").map(|v| v.as_bytes()),
            Some(&b"main"[..])
        );
        assert_eq!(
            r.headers.get("x-tgin-update-type").map(|v| v.as_bytes()),
            Some(&b"updateNewMessage"[..])
        );
        seen.push(v["update_id"].as_i64().expect("update_id"));
    }
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
    assert_eq!(
        executor.0.load(Ordering::SeqCst),
        0,
        "webhook replies are ignored"
    );
}

/// What the ingest logs (a queue that overflows), and whatever the other
/// tests logged before, holds no account content.
#[tokio::test]
async fn no_content_in_logs() {
    let routes = "      - match: { constructors: [updateNewMessage] }\n        to: [tiny]\n";
    let consumers = "  tiny: { pull: { auth_token: tok, capacity: 1, on_full: drop_oldest } }\n";
    let mut h = Harness::new(&account_config(routes, consumers), ReplyHandle::default());
    let mut m = manager();
    // Two for one place: the first is evicted, with a warning.
    let batch = pushed(&mut m, &[101, 102]);
    assert_eq!(h.admit(&batch), Admission::Admitted);
    assert_eq!(h.pending("tiny"), 1);
    let logs = LOGS.text();
    assert!(
        logs.contains("the consumer queue is full; dropping updates"),
        "{logs}"
    );
    let tl = STANDARD.encode(batch.updates[0].bytes());
    for needle in [SECRET, &tl[..16], "s3cr3t"] {
        assert!(!logs.contains(needle), "{needle} logged:\n{logs}");
    }
}

/// Ids that cannot be handed out are a storage failure, not backpressure:
/// the caller fails the account instead of refetching forever. Nothing is
/// admitted.
#[tokio::test]
async fn id_failure_is_an_error_not_a_refusal() {
    let mut h = Harness::new(
        &account_config(PULL, PULL_CONSUMERS),
        ReplyHandle::default(),
    );
    h.store.set_update_id(i64::MAX - 1).expect("mark");
    h.ids = UpdateIds::load(&h.store).expect("ids");
    let mut m = manager();
    let batch = pushed(&mut m, &[101, 102]);
    let got = h.ingest.admit(&batch, &mut h.ids, &h.store);
    assert!(matches!(got, Err(IdsError::Exhausted)), "{got:?}");
    assert_eq!(h.pending("feed"), 0);
}
