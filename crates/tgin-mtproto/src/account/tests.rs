//! The supervisor against a scripted Telegram ([`FakeNetwork`]) and real
//! session stores in scratch directories, on a paused clock.

use tempfile::TempDir;
use tokio::time::timeout;

use super::*;
use crate::login::{self, LoginParams, LoginState, Method};
use crate::testkit::{
    FakeNetwork, NetCall, Reply, ScriptedDial, difference, ids, login_token_success,
    private_message, tl_bytes, updates,
};
use crate::{IdsError, Peer, PeerKind, UpdateState};

/// Bounds every wait; the clock is paused, so it passes only once the
/// runtime is idle.
const WAIT: Duration = Duration::from_secs(3600);
const USER: i64 = 42;

/// What the sink saw of one batch.
#[derive(Clone, Debug)]
struct Seen {
    updates: usize,
    ahead: Vec<i64>,
    /// The channels the store held as behind when the batch was offered.
    behind: Vec<i64>,
}

/// Admits everything; panics the first `panics` times.
#[derive(Default)]
struct Sink {
    seen: Mutex<Vec<Seen>>,
    panics: Mutex<usize>,
}

impl Sink {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().clone()
    }

    fn updates(&self) -> usize {
        self.seen.lock().iter().map(|s| s.updates).sum()
    }
}

impl BatchSink for Sink {
    fn admit(
        &self,
        batch: &Batch,
        ids: &mut UpdateIds,
        store: &SessionStore,
    ) -> Result<Admission, IdsError> {
        let panic = {
            let mut left = self.panics.lock();
            let panic = *left > 0;
            *left = left.saturating_sub(1);
            panic
        };
        if panic {
            panic!("the sink failed");
        }
        let behind = store
            .stored_state()
            .expect("stored state")
            .behind
            .iter()
            .map(|(c, _)| *c)
            .collect();
        self.seen.lock().push(Seen {
            updates: batch.updates.len(),
            ahead: batch.ahead.clone(),
            behind,
        });
        if !batch.updates.is_empty() {
            ids.take(batch.updates.len(), store)?;
        }
        Ok(Admission::Admitted)
    }
}

fn me(id: i64) -> Peer {
    Peer {
        id,
        kind: PeerKind::User {
            bot: Some(false),
            is_self: Some(true),
        },
        access_hash: Some(1),
    }
}

/// A session logged in as `user` on DC 2, at pts 10.
fn logged_in(dir: &std::path::Path, user: i64) {
    let store = SessionStore::open(dir).expect("store");
    store.set_home_key(2, &AuthKey([2; 256])).expect("key");
    store.record_login(me(user)).expect("login");
    store
        .set_update_state(UpdateState {
            pts: 10,
            qts: 0,
            seq: 1,
            date: 1,
        })
        .expect("state");
}

struct Run {
    account: Account,
    sink: Arc<Sink>,
    dir: TempDir,
}

fn start(net: &FakeNetwork, registry: &Arc<Registry>) -> Run {
    start_as("main", net, registry)
}

fn start_as(name: &str, net: &FakeNetwork, registry: &Arc<Registry>) -> Run {
    let dir = tempfile::tempdir().expect("tempdir");
    logged_in(dir.path(), USER);
    start_in(name, dir, net, registry)
}

fn start_in(name: &str, dir: TempDir, net: &FakeNetwork, registry: &Arc<Registry>) -> Run {
    let sink = Arc::new(Sink::default());
    let account = Account::start(
        AccountSpec::new(name, dir.path()),
        Arc::clone(&sink) as Arc<dyn BatchSink>,
        Arc::new(net.clone()),
        Arc::clone(registry),
    );
    Run { account, sink, dir }
}

async fn until(account: &Account, want: impl Fn(&AccountState) -> bool) -> AccountState {
    let mut rx = account.watch();
    let state = timeout(WAIT, rx.wait_for(|s| want(s)))
        .await
        .unwrap_or_else(|_| panic!("state never reached; now {:?}", account.state()))
        .expect("the account lives")
        .clone();
    drop(rx);
    state
}

fn running(s: &AccountState) -> bool {
    *s == AccountState::Running
}

/// Telegram's difference from pts 10: one private message at pts 11.
fn one_message(call: &NetCall) -> Reply {
    match call.ctor() {
        ids::GET_DIFFERENCE => Reply::Ok(difference(&[private_message(1, 100, "hi")], 11, 1, 1)),
        _ => Reply::Rpc(400, "NOT_SCRIPTED"),
    }
}

fn main_dials(net: &FakeNetwork) -> Vec<tokio::time::Instant> {
    net.dials()
        .into_iter()
        .filter(|d| !d.media)
        .map(|d| d.at)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn panic_restarts_with_backoff() {
    let net = FakeNetwork::new();
    net.answer(one_message);
    let registry = Arc::new(Registry::default());
    let dir = tempfile::tempdir().expect("tempdir");
    logged_in(dir.path(), USER);
    let sink = Arc::new(Sink {
        panics: Mutex::new(3),
        ..Sink::default()
    });
    let started = Instant::now();
    let account = Account::start(
        AccountSpec::new("main", dir.path()),
        Arc::clone(&sink) as Arc<dyn BatchSink>,
        Arc::new(net.clone()),
        registry,
    );
    until(&account, |s| *s == AccountState::Failed(Failure::Panicked)).await;
    timeout(WAIT, async {
        while sink.updates() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the message is admitted in the end");
    let at: Vec<u64> = main_dials(&net)
        .iter()
        .map(|t| t.duration_since(started).as_secs())
        .collect();
    assert_eq!(at, [0, 1, 3, 7], "restarts after 1 s, 2 s, 4 s");
    assert_eq!(account.state(), AccountState::Running);
    assert_eq!(account.status().updates, 1);
    account.close().await;
}

#[tokio::test(start_paused = true)]
async fn backoff_is_capped_at_a_minute() {
    let net = FakeNetwork::new();
    net.connects(vec![Reply::NotSent; 9]);
    let registry = Arc::new(Registry::default());
    let run = start(&net, &registry);
    timeout(WAIT, async {
        while main_dials(&net).len() < 10 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .expect("ten connects");
    let dials = main_dials(&net);
    let gaps: Vec<u64> = dials
        .windows(2)
        .map(|w| w[1].duration_since(w[0]).as_secs())
        .collect();
    assert_eq!(gaps, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    until(&run.account, running).await;
    run.account.close().await;
}

#[tokio::test(start_paused = true)]
async fn key_revoked_is_needs_login_and_stops() {
    let net = FakeNetwork::new();
    net.answer(|_| Reply::KeyRevoked);
    let registry = Arc::new(Registry::default());
    let run = start(&net, &registry);
    until(&run.account, |s| *s == AccountState::NeedsLogin).await;
    assert!(!run.account.state().active(), "logins may run");
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(main_dials(&net).len(), 1, "no reconnect");
    // The session is released: a login or logout can open it.
    drop(SessionStore::open(run.dir.path()).expect("the session is free"));
    let call = run
        .account
        .rpc()
        .invoke(Route::Main, Bytes::from_static(&[0; 4]))
        .await;
    assert!(
        matches!(call, Err(InvokeError::Sender(SenderError::KeyRevoked))),
        "{call:?}"
    );
    run.account.close().await;
}

#[tokio::test(start_paused = true)]
async fn key_revoked_on_a_main_rpc_call_is_needs_login() {
    let net = FakeNetwork::new();
    net.answer(|call| match call.ctor() {
        ids::GET_DIFFERENCE => Reply::Ok(crate::testkit::difference_empty(1, 1)),
        _ => Reply::KeyRevoked,
    });
    let registry = Arc::new(Registry::default());
    let run = start(&net, &registry);
    until(&run.account, running).await;
    let call = run
        .account
        .rpc()
        .invoke(Route::Main, Bytes::from_static(&[0; 4]))
        .await;
    assert!(matches!(
        call,
        Err(InvokeError::Sender(SenderError::KeyRevoked))
    ));
    until(&run.account, |s| *s == AccountState::NeedsLogin).await;
    run.account.close().await;
}

/// `auth.exportedAuthorization` with id 1.
fn exported() -> Vec<u8> {
    tl_bytes(r#"{"_":"auth.exportedAuthorization","id":"1","bytes":"AQID"}"#)
}

#[tokio::test(start_paused = true)]
async fn media_key_revoked_is_not_needs_login() {
    let net = FakeNetwork::new();
    net.answer(|call| match (call.media, call.ctor()) {
        (false, ids::GET_DIFFERENCE) => Reply::Ok(crate::testkit::difference_empty(1, 1)),
        (false, ids::EXPORT_AUTHORIZATION) => Reply::Ok(exported()),
        // Every media session's key is refused.
        (true, _) => Reply::KeyRevoked,
        _ => Reply::Rpc(400, "NOT_SCRIPTED"),
    });
    let registry = Arc::new(Registry::default());
    let run = start(&net, &registry);
    until(&run.account, running).await;
    let call = run
        .account
        .rpc()
        .invoke(Route::Media(4), Bytes::from_static(&[0; 4]))
        .await;
    assert!(
        matches!(call, Err(InvokeError::Media { dc: 4, .. })),
        "{call:?}"
    );
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(run.account.state(), AccountState::Running);
    assert_eq!(main_dials(&net).len(), 1);
    run.account.close().await;
}

#[test]
fn only_the_main_route_reports_a_gone_key() {
    let shared = Shared::new(Arc::from("main"));
    let revoked = Err(InvokeError::Sender(SenderError::KeyRevoked));
    observe(&shared, Route::Media(4), 2, &revoked);
    assert!(!shared.key_gone.load(Ordering::SeqCst), "a media route");
    observe(&shared, Route::Media(2), 2, &revoked);
    assert!(shared.key_gone.swap(false, Ordering::SeqCst), "the home DC");
    observe(&shared, Route::Main, 2, &revoked);
    assert!(shared.key_gone.load(Ordering::SeqCst), "the main route");
}

#[tokio::test(start_paused = true)]
async fn frozen_answer_is_frozen() {
    let net = FakeNetwork::new();
    net.answer(|call| match call.ctor() {
        ids::GET_DIFFERENCE => Reply::Ok(crate::testkit::difference_empty(1, 1)),
        _ => Reply::Rpc(420, "FROZEN_METHOD_INVALID"),
    });
    let registry = Arc::new(Registry::default());
    let run = start(&net, &registry);
    until(&run.account, running).await;
    let _ = run
        .account
        .rpc()
        .invoke(Route::Main, Bytes::from_static(&[0; 4]))
        .await;
    assert_eq!(run.account.state(), AccountState::Frozen);
    assert!(run.account.state().active());
    run.account.close().await;
}

#[tokio::test(start_paused = true)]
async fn same_user_twice_fails_second() {
    let registry = Arc::new(Registry::default());
    let first_net = FakeNetwork::new();
    let first = start(&first_net, &registry);
    until(&first.account, running).await;

    let second_net = FakeNetwork::new();
    let second = start_as("second", &second_net, &registry);
    until(&second.account, |s| {
        *s == AccountState::Failed(Failure::DuplicateAccount)
    })
    .await;
    assert!(second_net.dials().is_empty(), "the second never connects");
    assert!(second.sink.seen().is_empty(), "nor receives anything");
    assert!(!second.account.state().active());
    assert_eq!(first.account.state(), AccountState::Running);
    second.account.close().await;
    first.account.close().await;
}

#[tokio::test(start_paused = true)]
async fn fetch_failure_is_retried_not_dropped() {
    let net = FakeNetwork::new();
    let mut first = true;
    net.answer(move |call| {
        if call.ctor() == ids::GET_DIFFERENCE && std::mem::take(&mut first) {
            return Reply::NoAnswer;
        }
        one_message(call)
    });
    let registry = Arc::new(Registry::default());
    let started = Instant::now();
    let run = start(&net, &registry);
    until(&run.account, |s| *s == AccountState::Connecting).await;
    until(&run.account, running).await;
    assert_eq!(Instant::now().duration_since(started), RETRY_FIRST);
    assert_eq!(run.sink.updates(), 1);
    assert_eq!(main_dials(&net).len(), 1, "the same session asked again");
    run.account.close().await;
}

/// The update manager's first retry.
const RETRY_FIRST: Duration = crate::updates::RETRY_MIN;

#[tokio::test(start_paused = true)]
async fn flood_wait_on_init_waits_then_connects_again() {
    let net = FakeNetwork::new();
    let mut first = true;
    net.answer(move |call| {
        if std::mem::take(&mut first) {
            return Reply::InitRefused(420, "FLOOD_WAIT_30");
        }
        one_message(call)
    });
    let registry = Arc::new(Registry::default());
    let started = Instant::now();
    let run = start(&net, &registry);
    let state = until(&run.account, |s| {
        matches!(s, AccountState::FloodWait { .. })
    })
    .await;
    assert!(matches!(state, AccountState::FloodWait { until } if until > SystemTime::now()));
    until(&run.account, running).await;
    let at: Vec<u64> = main_dials(&net)
        .iter()
        .map(|t| t.duration_since(started).as_secs())
        .collect();
    assert_eq!(at, [0, 30]);
    run.account.close().await;
}

#[tokio::test(start_paused = true)]
async fn init_refused_fails_until_started_again() {
    let net = FakeNetwork::new();
    net.answer(|_| Reply::InitRefused(400, "API_ID_INVALID"));
    let registry = Arc::new(Registry::default());
    let run = start(&net, &registry);
    until(&run.account, |s| {
        *s == AccountState::Failed(Failure::InitRefused)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(main_dials(&net).len(), 1);
    run.account.close().await;
}

/// A channel post at `pts` in channel `channel`.
fn channel_post(channel: i64, id: i32, pts: i32) -> String {
    format!(
        r#"{{"_":"updateNewChannelMessage","message":{{"_":"message","id":{id},"peer_id":{{"_":"peerChannel","channel_id":"{channel}"}},"date":1,"message":"p"}},"pts":{pts},"pts_count":1}}"#
    )
}

#[tokio::test(start_paused = true)]
async fn marks_are_persisted_before_admission() {
    let net = FakeNetwork::new();
    let registry = Arc::new(Registry::default());
    let dir = tempfile::tempdir().expect("tempdir");
    logged_in(dir.path(), USER);
    {
        let store = SessionStore::open(dir.path()).expect("store");
        store.set_channel_pts(8, 20).expect("channel");
    }
    let run = start_in("main", dir, &net, &registry);
    until(&run.account, running).await;
    let before = run.sink.seen().len();
    // A known channel and one the account never saw: one push, several
    // batches, each naming channels it moves ahead.
    assert!(net.push(updates(&[channel_post(8, 1, 21), channel_post(7, 1, 5)])));
    timeout(WAIT, async {
        while run.sink.updates() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both posts admitted");
    let seen = &run.sink.seen()[before..];
    let ahead: std::collections::BTreeSet<i64> =
        seen.iter().flat_map(|s| s.ahead.iter().copied()).collect();
    assert!(seen.len() >= 2 && ahead.len() >= 2, "{seen:?}");
    let first: std::collections::BTreeSet<i64> = seen[0].behind.iter().copied().collect();
    assert!(
        first.is_superset(&ahead),
        "every mark of the call is durable before its first batch: {seen:?}"
    );
    run.account.close().await;
}

#[tokio::test(start_paused = true)]
async fn logged_out_account_starts_after_a_login_without_a_restart() {
    let net = FakeNetwork::new();
    let registry = Arc::new(Registry::default());
    let dir = tempfile::tempdir().expect("tempdir");
    let run = start_in("main", dir, &net, &registry);
    until(&run.account, |s| {
        *s == AccountState::LoggedOut { login: false }
    })
    .await;
    assert!(net.dials().is_empty());

    // A login of this process takes the session the account released.
    let store = SessionStore::open(run.dir.path()).expect("the session is free");
    let dial = ScriptedDial::new(vec![(
        ids::EXPORT_LOGIN_TOKEN,
        Reply::Ok(login_token_success(USER, None)),
    )]);
    let params = LoginParams {
        api_id: 1,
        api_hash: "hash".into(),
        phone: None,
    };
    let handle = login::start(store, dial, params, Method::Qr)
        .await
        .expect("login");
    assert_eq!(
        timeout(WAIT, handle.finished()).await.expect("ends"),
        LoginState::LoggedIn { user_id: USER }
    );
    drop(handle);

    run.account.wake();
    until(&run.account, running).await;
    assert_eq!(run.account.status().home_dc, Some(login::FIRST_DC));
    run.account.close().await;
}

#[tokio::test(start_paused = true)]
async fn a_login_in_progress_keeps_the_account_waiting() {
    let net = FakeNetwork::new();
    let busy = Arc::new(AtomicBool::new(true));
    let registry = Arc::new(Registry::with_logins({
        let busy = Arc::clone(&busy);
        move |_| busy.load(Ordering::SeqCst)
    }));
    let run = start(&net, &registry);
    until(&run.account, |s| {
        *s == AccountState::LoggedOut { login: true }
    })
    .await;
    assert!(net.dials().is_empty());
    busy.store(false, Ordering::SeqCst);
    run.account.wake();
    until(&run.account, running).await;
    run.account.close().await;
}

#[tokio::test(start_paused = true)]
async fn stopped_ingest_serves_rpc_until_close() {
    let net = FakeNetwork::new();
    net.answer(|call| match call.ctor() {
        ids::GET_DIFFERENCE => Reply::Ok(crate::testkit::difference_empty(1, 1)),
        _ => Reply::Ok(vec![0x15, 0xc4, 0xb5, 0x1c, 0, 0, 0, 0]),
    });
    let registry = Arc::new(Registry::default());
    let run = start(&net, &registry);
    until(&run.account, running).await;
    run.account.stop_ingest();
    tokio::task::yield_now().await;
    let rpc = run.account.rpc();
    let call = rpc.invoke(Route::Main, Bytes::from_static(&[0; 4])).await;
    assert!(call.is_ok(), "{call:?}");
    run.account.close().await;
    let call = rpc.invoke(Route::Main, Bytes::from_static(&[0; 4])).await;
    assert!(matches!(
        call,
        Err(InvokeError::Sender(SenderError::NotSent))
    ));
    drop(SessionStore::open(run.dir.path()).expect("closed, the session is free"));
}

#[tokio::test(start_paused = true)]
async fn status_reports_layer_home_dc_and_counts() {
    let net = FakeNetwork::new();
    net.answer(one_message);
    let registry = Arc::new(Registry::default());
    let run = start(&net, &registry);
    until(&run.account, running).await;
    timeout(WAIT, async {
        while run.account.status().updates == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("counted");
    let status = run.account.status();
    assert_eq!(&*status.name, "main");
    assert_eq!(status.layer, API.layer);
    assert_eq!(status.home_dc, Some(2));
    assert_eq!(status.catching_up, CatchingUp::default());
    assert_eq!(status.updates, 1);
    run.account.close().await;
}

#[tokio::test(start_paused = true)]
async fn flood_wait_on_a_catch_up_is_waited_out() {
    let net = FakeNetwork::new();
    let mut first = true;
    net.answer(move |call| {
        if call.ctor() == ids::GET_DIFFERENCE && std::mem::take(&mut first) {
            return Reply::Rpc(420, "FLOOD_WAIT_30");
        }
        one_message(call)
    });
    let registry = Arc::new(Registry::default());
    let started = Instant::now();
    let run = start(&net, &registry);
    let state = until(&run.account, |s| {
        matches!(s, AccountState::FloodWait { .. })
    })
    .await;
    assert!(matches!(state, AccountState::FloodWait { until } if until > SystemTime::now()));
    // A signal during the wait asks nothing.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(net.push(crate::testkit::updates_too_long()));
    tokio::time::sleep(Duration::from_secs(5)).await;
    let differences = |net: &FakeNetwork| {
        net.ctors()
            .iter()
            .filter(|&&c| c == ids::GET_DIFFERENCE)
            .count()
    };
    assert_eq!(differences(&net), 1, "nothing asked during the wait");
    until(&run.account, running).await;
    assert_eq!(
        Instant::now().duration_since(started),
        Duration::from_secs(30)
    );
    assert_eq!(run.sink.updates(), 1);
    run.account.close().await;
}

#[tokio::test(start_paused = true)]
async fn a_new_login_clears_frozen() {
    let net = FakeNetwork::new();
    let mut calls = 0;
    net.answer(move |call| {
        if call.ctor() == ids::GET_DIFFERENCE {
            return Reply::Ok(crate::testkit::difference_empty(1, 1));
        }
        calls += 1;
        match calls {
            1 => Reply::Rpc(420, "FROZEN_METHOD_INVALID"),
            2 => Reply::KeyRevoked,
            _ => Reply::Ok(Vec::new()),
        }
    });
    let registry = Arc::new(Registry::default());
    let run = start(&net, &registry);
    until(&run.account, running).await;
    let rpc = run.account.rpc();
    let _ = rpc.invoke(Route::Main, Bytes::from_static(&[0; 4])).await;
    assert_eq!(run.account.state(), AccountState::Frozen);
    let _ = rpc.invoke(Route::Main, Bytes::from_static(&[0; 4])).await;
    until(&run.account, |s| *s == AccountState::NeedsLogin).await;
    run.account.wake();
    until(&run.account, |s| {
        *s != AccountState::NeedsLogin && *s != AccountState::Connecting
    })
    .await;
    assert_eq!(run.account.state(), AccountState::Running);
    run.account.close().await;
}

#[test]
fn an_unauthorized_main_answer_reports_a_gone_key() {
    let shared = Shared::new(Arc::from("main"));
    let error = |code, message: &str| {
        Ok(RpcOutcome::Error {
            code,
            message: message.to_owned(),
        })
    };
    let unregistered = error(401, "AUTH_KEY_UNREGISTERED");
    observe(&shared, Route::Media(4), 2, &unregistered);
    assert!(!shared.key_gone.load(Ordering::SeqCst), "a media route");
    observe(
        &shared,
        Route::Main,
        2,
        &error(401, "SESSION_PASSWORD_NEEDED"),
    );
    assert!(!shared.key_gone.load(Ordering::SeqCst), "a login's 401");
    observe(&shared, Route::Main, 2, &unregistered);
    assert!(
        shared.key_gone.swap(false, Ordering::SeqCst),
        "a 401 on main"
    );
    observe(
        &shared,
        Route::Media(2),
        2,
        &error(406, "AUTH_KEY_DUPLICATED"),
    );
    assert!(shared.key_gone.load(Ordering::SeqCst), "duplicated on home");
}

#[tokio::test(start_paused = true)]
async fn a_cut_short_close_leaves_the_task_to_abort() {
    let net = FakeNetwork::new();
    let registry = Arc::new(Registry::default());
    let run = start(&net, &registry);
    until(&run.account, running).await;
    assert!(
        timeout(Duration::ZERO, run.account.close()).await.is_err(),
        "cut short"
    );
    run.account.abort();
    assert!(!run.account.close().await, "the abort ended it");
}
