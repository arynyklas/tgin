//! Telegram accounts in the running app, against a scripted Telegram
//! (`tgin_mtproto::testkit::FakeNetwork`): their status and metrics, the
//! raw-TL RPC endpoint through the data plane, reloads, and the shutdown.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tgin::app::{App, Boot};
use tgin::exit::Outcome;
use tgin_mtproto::testkit::{FakeNetwork, Reply, difference_empty, ids};
use tgin_mtproto::{AuthKey, Peer, PeerKind, SessionStore, UpdateState};
use tgin_runtime::config::MapEnv;
use tgin_runtime::lifecycle::{SignalKind, Signals};
use tgin_runtime::obs::MetricsRegistry;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const PATIENCE: Duration = Duration::from_secs(10);
const ADMIN: &str = "admin-token-value-123";
const API_HASH: &str = "api-hash-secret-value-0123";
const PHONE: &str = "+15550100777";
const CLIENT_A: &str = "client-token-aaaaaaaaaaaaaaaa";
const CLIENT_B: &str = "client-token-bbbbbbbbbbbbbbbb";
/// `help.getConfig#c4f9186b`.
const GET_CONFIG: [u8; 4] = 0xc4f9_186b_u32.to_le_bytes();
/// An empty `Vector`: what the scripted Telegram answers to a call.
const ANSWER: [u8; 8] = [0x15, 0xc4, 0xb5, 0x1c, 0, 0, 0, 0];

/// A config with the account `main` (session in `sessions/main`), one RPC
/// client `bot` with `token`, and `extra` at the top.
fn config(token: &str, extra: &str) -> String {
    format!(
        "version: 1\nruntime: {{ lame_duck: 0s, shutdown_grace: 10s }}\nreload: {{ watch: false }}\nserver: {{ listen: [\"127.0.0.1:0\"] }}\nadmin: {{ listen: [\"127.0.0.1:0\"], token: {ADMIN} }}\n{extra}accounts:\n  main:\n    api_id: 1\n    api_hash: {API_HASH}\n    phone: \"{PHONE}\"\n    session_dir: sessions/main\n    rpc:\n      clients:\n        bot: {{ token: {token}, allow: [\"*\"] }}\n"
    )
}

/// The account's session, logged in on DC 2.
fn logged_in(dir: &Path) {
    logged_in_as(dir, "main");
}

/// The account `name`'s session, logged in as user 42 on DC 2.
fn logged_in_as(dir: &Path, name: &str) {
    let store = SessionStore::open(&dir.join("sessions").join(name)).expect("store");
    store.set_home_key(2, &AuthKey([2; 256])).expect("key");
    store
        .record_login(Peer {
            id: 42,
            kind: PeerKind::User {
                bot: Some(false),
                is_self: Some(true),
            },
            access_hash: Some(1),
        })
        .expect("login");
    store
        .set_update_state(UpdateState {
            pts: 10,
            qts: 0,
            seq: 1,
            date: 1,
        })
        .expect("state");
}

/// Answers every call; `help.getConfig` after `delay`.
fn telegram(delay: Duration) -> FakeNetwork {
    let net = FakeNetwork::new();
    net.answer(move |call| match call.ctor() {
        ids::GET_DIFFERENCE => Reply::Ok(difference_empty(1, 1)),
        _ if delay.is_zero() => Reply::Ok(ANSWER.to_vec()),
        _ => Reply::Later(delay, Box::new(Reply::Ok(ANSWER.to_vec()))),
    });
    net
}

struct Tgin {
    dir: tempfile::TempDir,
    file: PathBuf,
    data: std::net::SocketAddr,
    admin: std::net::SocketAddr,
    signals: mpsc::Sender<SignalKind>,
    run: JoinHandle<Outcome>,
    metrics: Arc<MetricsRegistry>,
    http: reqwest::Client,
}

async fn start(yaml: &str, net: Option<FakeNetwork>) -> Tgin {
    let dir = tempfile::tempdir().expect("tempdir");
    if net.is_some() {
        logged_in(dir.path());
    }
    let file = dir.path().join("tgin.yaml");
    std::fs::write(&file, yaml).expect("config");
    let mut boot = Boot::load(&file, Arc::new(MapEnv::default())).expect("config loads");
    if let Some(net) = net {
        boot.accounts_network = Arc::new(move |_| Arc::new(net.clone()));
    }
    let app = App::start(boot).await.expect("tgin starts");
    let (signals, rx) = mpsc::channel(4);
    let data = app.data_addrs()[0];
    let admin = app.admin_addrs()[0];
    let metrics = app.metrics();
    let run = tokio::spawn(app.run(Signals::from_channel(rx)));
    Tgin {
        dir,
        file,
        data,
        admin,
        signals,
        run,
        metrics,
        http: reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client"),
    }
}

impl Tgin {
    async fn accounts(&self) -> (u16, String) {
        let response = self
            .http
            .get(format!("http://{}/v1/accounts", self.admin))
            .bearer_auth(ADMIN)
            .send()
            .await
            .expect("answered");
        (
            response.status().as_u16(),
            response.text().await.expect("body"),
        )
    }

    async fn until_state(&self, state: &str) -> Value {
        timeout(PATIENCE, async {
            loop {
                let (status, body) = self.accounts().await;
                assert_eq!(status, 200, "{body}");
                let json: Value = serde_json::from_str(&body).expect("json");
                if json["accounts"][0]["state"] == state {
                    return json;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the account never got to {state}"))
    }

    async fn call(&self, token: &str) -> reqwest::Response {
        self.http
            .post(format!("http://{}/mtproto/v1/main/invoke", self.data))
            .bearer_auth(token)
            .header("x-tgin-layer", "230")
            .body(GET_CONFIG.to_vec())
            .send()
            .await
            .expect("answered")
    }

    async fn reload(&self, yaml: &str) {
        std::fs::write(&self.file, yaml).expect("config");
        let response = self
            .http
            .post(format!("http://{}/v1/reload", self.admin))
            .bearer_auth(ADMIN)
            .send()
            .await
            .expect("answered");
        assert_eq!(
            response.status(),
            200,
            "{}",
            response.text().await.unwrap_or_default()
        );
    }

    async fn stop(self) {
        self.signals
            .send(SignalKind::Terminate)
            .await
            .expect("signal");
        let outcome = timeout(PATIENCE, self.run)
            .await
            .expect("tgin exits in time")
            .expect("no panic");
        assert_eq!(outcome, Outcome::Clean);
    }
}

#[tokio::test]
async fn status_has_no_secrets() {
    let net = telegram(Duration::ZERO);
    let tgin = start(&config(CLIENT_A, ""), Some(net.clone())).await;
    let json = tgin.until_state("running").await;
    let account = &json["accounts"][0];
    assert_eq!(account["name"], "main");
    assert_eq!(account["layer"], 230);
    assert_eq!(account["home_dc"], 2);
    assert_eq!(account["catching_up"]["common"], false);
    assert_eq!(account["catching_up"]["channels"], 0);
    let (_, body) = tgin.accounts().await;
    let metrics = tgin.metrics.encode();
    assert!(
        metrics
            .lines()
            .any(|l| l == r#"tgin_mtproto_account_state{account="main",state="running"} 1"#),
        "{metrics}"
    );
    let session = tgin.dir.path().join("sessions").display().to_string();
    for secret in [
        API_HASH,
        PHONE,
        CLIENT_A,
        ADMIN,
        session.as_str(),
        "sessions",
    ] {
        assert!(!body.contains(secret), "{secret} in {body}");
        assert!(!metrics.contains(secret), "{secret} in the metrics");
    }
    tgin.stop().await;
}

#[tokio::test]
async fn no_account_no_series() {
    let yaml = format!(
        "version: 1\nreload: {{ watch: false }}\nserver: {{ listen: [\"127.0.0.1:0\"] }}\nadmin: {{ listen: [\"127.0.0.1:0\"], token: {ADMIN} }}\n"
    );
    let tgin = start(&yaml, None).await;
    let metrics = tgin.metrics.encode();
    assert!(!metrics.contains("mtproto"), "{metrics}");
    let (status, body) = tgin.accounts().await;
    assert_eq!((status, body.as_str()), (200, r#"{"accounts":[]}"#));
    tgin.stop().await;

    // With an account, the series are there.
    let tgin = start(&config(CLIENT_A, ""), Some(telegram(Duration::ZERO))).await;
    tgin.until_state("running").await;
    let metrics = tgin.metrics.encode();
    for line in [
        r#"tgin_mtproto_updates_total{account="main"} 0"#,
        r#"tgin_mtproto_updates_dropped_total{account="main",reason="undecodable"} 0"#,
        r#"tgin_mtproto_rpc_total{account="main",client="bot",outcome="ok"} 0"#,
    ] {
        assert!(
            metrics.lines().any(|l| l == line),
            "{line} missing:\n{metrics}"
        );
    }
    tgin.stop().await;
}

#[tokio::test]
async fn reload_rpc_clients_live() {
    let net = telegram(Duration::ZERO);
    let tgin = start(&config(CLIENT_A, ""), Some(net.clone())).await;
    tgin.until_state("running").await;
    assert_eq!(tgin.call(CLIENT_A).await.status(), 200);
    assert_eq!(tgin.call(CLIENT_B).await.status(), 401);

    tgin.reload(&config(CLIENT_B, "")).await;
    assert_eq!(
        tgin.call(CLIENT_A).await.status(),
        401,
        "the old token is gone"
    );
    let answer = tgin.call(CLIENT_B).await;
    assert_eq!(answer.status(), 200);
    assert_eq!(answer.bytes().await.expect("body").as_ref(), ANSWER);
    let main: Vec<_> = net.dials().into_iter().filter(|d| !d.media).collect();
    assert_eq!(main.len(), 1, "the account did not restart");
    let metrics = tgin.metrics.encode();
    let line = r#"tgin_mtproto_rpc_total{account="main",client="bot",outcome="ok"} 2"#;
    assert!(metrics.lines().any(|l| l == line), "{metrics}");
    assert!(metrics.contains("tgin_mtproto_rpc_seconds_count{account=\"main\"} 2"));

    // A session setting restarts it.
    tgin.reload(&config(CLIENT_B, "").replace(API_HASH, "another-api-hash-value"))
        .await;
    timeout(PATIENCE, async {
        while net.dials().iter().filter(|d| !d.media).count() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the account restarts");
    tgin.until_state("running").await;
    tgin.stop().await;
}

#[tokio::test]
async fn shutdown_waits_for_an_in_flight_rpc() {
    let net = telegram(Duration::from_secs(3));
    let tgin = start(&config(CLIENT_A, ""), Some(net.clone())).await;
    tgin.until_state("running").await;
    let call = {
        let http = tgin.http.clone();
        let url = format!("http://{}/mtproto/v1/main/invoke", tgin.data);
        tokio::spawn(async move {
            http.post(url)
                .bearer_auth(CLIENT_A)
                .header("x-tgin-layer", "230")
                .body(GET_CONFIG.to_vec())
                .send()
                .await
        })
    };
    timeout(PATIENCE, async {
        while !net.ctors().contains(&0xc4f9_186b) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the call reaches Telegram");
    tgin.stop().await;
    let answer = call.await.expect("no panic").expect("the call is answered");
    assert_eq!(answer.status(), 200);
}

#[tokio::test]
async fn unmatched_account_updates_are_counted() {
    let net = FakeNetwork::new();
    net.answer(|call| match call.ctor() {
        ids::GET_DIFFERENCE => Reply::Ok(tgin_mtproto::testkit::difference(
            &[tgin_mtproto::testkit::private_message(1, 100, "hi")],
            11,
            1,
            1,
        )),
        _ => Reply::Ok(ANSWER.to_vec()),
    });
    // The account has no rule: its update matches none.
    let tgin = start(&config(CLIENT_A, ""), Some(net)).await;
    tgin.until_state("running").await;
    let line = r#"tgin_routes_unmatched_total{source="main"} 1"#;
    timeout(PATIENCE, async {
        while !tgin.metrics.encode().lines().any(|l| l == line) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{line} missing:\n{}", tgin.metrics.encode()));
    tgin.stop().await;
}

/// Two accounts of one config, both logged in as one user: one runs, the
/// other waits as a duplicate until the first is removed. The removed
/// account's series go with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_duplicate_runs_once_its_user_is_released() {
    let net = FakeNetwork::new();
    net.answer(|call| match call.ctor() {
        ids::GET_DIFFERENCE => Reply::Ok(tgin_mtproto::testkit::difference(
            &[tgin_mtproto::testkit::private_message(1, 100, "hi")],
            11,
            1,
            1,
        )),
        _ => Reply::Ok(ANSWER.to_vec()),
    });
    let second = "  second:\n    api_id: 1\n    api_hash: other-api-hash-value-01\n    session_dir: sessions/second\n";
    let yaml = format!("{}{second}", config(CLIENT_A, ""));
    let dir = tempfile::tempdir().expect("tempdir");
    logged_in_as(dir.path(), "main");
    logged_in_as(dir.path(), "second");
    let file = dir.path().join("tgin.yaml");
    std::fs::write(&file, &yaml).expect("config");
    let mut boot = Boot::load(&file, Arc::new(MapEnv::default())).expect("config loads");
    boot.accounts_network = Arc::new(move |_| Arc::new(net.clone()));
    let app = App::start(boot).await.expect("tgin starts");
    let (signals, rx) = mpsc::channel(4);
    let tgin = Tgin {
        data: app.data_addrs()[0],
        admin: app.admin_addrs()[0],
        metrics: app.metrics(),
        run: tokio::spawn(app.run(Signals::from_channel(rx))),
        signals,
        file,
        dir,
        http: reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client"),
    };
    let states = || async {
        let (_, body) = tgin.accounts().await;
        let json: Value = serde_json::from_str(&body).expect("json");
        json["accounts"]
            .as_array()
            .expect("accounts")
            .iter()
            .map(|a| {
                (
                    a["name"].as_str().unwrap_or_default().to_owned(),
                    a["state"].as_str().unwrap_or_default().to_owned(),
                    a["reason"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect::<Vec<_>>()
    };
    timeout(PATIENCE, async {
        loop {
            let s = states().await;
            if s.iter().any(|a| a.1 == "running") && s.iter().any(|a| a.1 == "failed") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("one runs, one fails");
    let s = states().await;
    let winner = s
        .iter()
        .find(|a| a.1 == "running")
        .expect("one runs")
        .0
        .clone();
    let loser = s.iter().find(|a| a.1 == "failed").expect("one fails");
    assert_eq!(loser.2, "duplicate_account", "{s:?}");
    let loser = loser.0.clone();
    let unmatched = format!(r#"tgin_routes_unmatched_total{{source="{winner}"}} 1"#);
    timeout(PATIENCE, async {
        while !tgin.metrics.encode().lines().any(|l| l == unmatched) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the winner's update is unmatched");

    // Without the winner, the other takes the user.
    tgin.reload(
        &yaml
            .replace(&format!("  {winner}:\n"), "  first:\n")
            .replace(&format!("sessions/{winner}"), "sessions/first"),
    )
    .await;
    timeout(PATIENCE, async {
        while !states()
            .await
            .contains(&(loser.clone(), "running".into(), String::new()))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the other runs");
    let metrics = tgin.metrics.encode();
    for gone in [
        format!(r#"account="{winner}""#),
        format!(r#"source="{winner}""#),
    ] {
        assert!(!metrics.contains(&gone), "{gone} still in:\n{metrics}");
    }
    tgin.stop().await;
}
