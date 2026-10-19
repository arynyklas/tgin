//! The account routes against a fake [`AccountHandle`]: a real login on a
//! real session store, with Telegram scripted (`tgin_mtproto::testkit`).

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use axum::http::{Request, header};
use serde_json::Value;
use tempfile::TempDir;
use tgin_mtproto::testkit::{
    Reply, ScriptedDial, account_password_srp, authorization, ids, logged_out, login_token,
    login_token_success, sent_code, sms, update_login_token,
};
use tokio::time::timeout;
use tower::ServiceExt as _;

use super::*;

const WAIT: Duration = Duration::from_secs(10);
const PHONE: &str = "+1 555 0100";

struct Fake {
    running: AtomicBool,
    dial: ScriptedDial,
}

impl AccountHandle for Fake {
    fn running(&self, _account: &str) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn start_login(
        &self,
        target: &Target,
        method: Method,
    ) -> BoxFuture<'static, Result<LoginHandle, Refusal>> {
        let dir = target.session_dir.clone();
        let params = target.params.clone();
        let dial = self.dial.clone();
        Box::pin(async move {
            let store = SessionStore::open(&dir)?;
            login::start(store, dial, params, method)
                .await
                .map_err(start_refusal)
        })
    }

    fn log_out(&self, target: &Target) -> BoxFuture<'static, Result<(), Refusal>> {
        let dir = target.session_dir.clone();
        let dial = self.dial.clone();
        Box::pin(async move {
            let store = SessionStore::open(&dir)?;
            login::log_out(&store, &dial).await.map_err(logout_refusal)
        })
    }
}

#[derive(Clone)]
struct TestState {
    accounts: Arc<Accounts>,
    targets: Arc<HashMap<String, Target>>,
    token: bool,
}

impl AccountsState for TestState {
    fn accounts(&self) -> &Accounts {
        &self.accounts
    }

    fn target(&self, name: &str) -> Option<Target> {
        self.targets.get(name).cloned()
    }

    fn token_configured(&self) -> bool {
        self.token
    }
}

struct Setup {
    app: Router,
    fake: Arc<Fake>,
    dir: TempDir,
    state: TestState,
}

fn setup(phone: Option<&str>, script: Vec<(u32, Reply)>) -> Setup {
    setup_with(true, phone, script)
}

fn setup_with(token: bool, phone: Option<&str>, script: Vec<(u32, Reply)>) -> Setup {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = Arc::new(Fake {
        running: AtomicBool::new(false),
        dial: ScriptedDial::new(script),
    });
    let target = Target {
        session_dir: dir.path().join("main"),
        params: LoginParams {
            api_id: 1,
            api_hash: "api-hash-secret".into(),
            phone: phone.map(str::to_owned),
        },
    };
    let handle: Arc<dyn AccountHandle> = fake.clone();
    let state = TestState {
        accounts: Arc::new(Accounts::new(handle)),
        targets: Arc::new(HashMap::from([
            ("second".to_owned(), {
                let mut second = target.clone();
                second.session_dir = dir.path().join("second");
                second
            }),
            ("main".to_owned(), target),
        ])),
        token,
    };
    Setup {
        app: routes().with_state(state.clone()),
        fake,
        dir,
        state,
    }
}

/// `(status, body)` of one request.
async fn call(app: &Router, method: &str, path: &str, body: &str) -> (StatusCode, String) {
    let (status, _, body) = call_full(app, method, path, body).await;
    (status, body)
}

/// `(status, headers, body)` of one request.
async fn call_full(
    app: &Router,
    method: &str,
    path: &str,
    body: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn parse(body: &str) -> LoginStatus {
    serde_json::from_str(body).expect("a login status")
}

/// `GET …/login` until `done` holds.
async fn poll(app: &Router, done: impl Fn(&LoginStatus) -> bool) -> LoginStatus {
    timeout(WAIT, async {
        loop {
            let (status, body) = call(app, "GET", "/v1/accounts/main/login", "").await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let state = parse(&body);
            if done(&state) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the state arrives")
}

fn later() -> i32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    i32::try_from(now + 600).expect("i32 time")
}

/// Everything logged while the guard lives, with no redactor in between.
#[derive(Clone, Default)]
struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

impl io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        let logs = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || logs.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(
            &self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned()
    }
}

async fn reopen(dir: &TempDir) -> SessionStore {
    timeout(WAIT, async {
        loop {
            match SessionStore::open(&dir.path().join("main")) {
                Ok(s) => return s,
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("the login released the session")
}

/// QR: `qr` with the URL, the same from `GET`, then `logged_in` once the
/// scan's push arrives. The token is in the answers only, not the logs.
/// A second login on the logged-in session is refused; logout clears it.
/// The clock is paused on a current-thread runtime, so the token's 30 s
/// renewal and every wait run on virtual time, and load cannot reorder them.
#[tokio::test(start_paused = true)]
async fn qr_flow_states() {
    let logs = Logs::default();
    let _guard = logs.capture();
    // One wall-clock read: a second `later()` may land a second on.
    let expires = later();
    let s = setup(
        None,
        vec![
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token(b"secret-token", expires)),
            ),
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token_success(77, None)),
            ),
            (ids::LOG_OUT, Reply::Ok(logged_out())),
        ],
    );
    let (status, headers, body) = call_full(
        &s.app,
        "POST",
        "/v1/accounts/main/login",
        r#"{"method":"qr"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "no-store",
        "the URL is a secret"
    );
    let (_, headers, again) = call_full(&s.app, "GET", "/v1/accounts/main/login", "").await;
    assert_eq!(parse(&again), parse(&body));
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["state"], "qr");
    assert_eq!(json["url"], "tg://login?token=c2VjcmV0LXRva2Vu");
    assert_eq!(json["expires"], expires);
    let LoginStatus::Qr { url, .. } = parse(&body) else {
        panic!("qr: {body}");
    };
    assert!(!format!("{url:?}").contains("c2VjcmV0"));

    // A repeated POST answers the running login's state.
    let (status, again) = call(
        &s.app,
        "POST",
        "/v1/accounts/main/login",
        r#"{"method":"qr"}"#,
    )
    .await;
    assert_eq!((status, parse(&again)), (StatusCode::OK, parse(&body)));
    let (status, _) = call(
        &s.app,
        "POST",
        "/v1/accounts/main/login",
        r#"{"method":"code"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "the other method waits");

    assert!(s.fake.dial.push(update_login_token()));
    let end = poll(&s.app, |st| !matches!(st, LoginStatus::Qr { .. })).await;
    assert_eq!(end, LoginStatus::LoggedIn { user_id: 77 });
    assert!(s.fake.dial.marked_logged_in());

    let (status, body) = call(
        &s.app,
        "POST",
        "/v1/accounts/main/login",
        r#"{"method":"qr"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body, r#"{"error":"logged in"}"#);

    let (status, body) = call(&s.app, "POST", "/v1/accounts/main/logout", "").await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"logged_out":true}"#)
    );
    assert!(s.fake.dial.done());
    let store = reopen(&s.dir).await;
    assert_eq!(store.login().expect("login"), None);
    assert_eq!(store.home_dc().expect("home"), None);

    let logs = logs.text();
    assert!(logs.contains("account login"), "{logs}");
    for secret in [
        "c2VjcmV0LXRva2Vu",
        "secret-token",
        "tg://login",
        "api-hash-secret",
    ] {
        assert!(!logs.contains(secret), "{secret} logged: {logs}");
    }
}

/// The code flow with 2FA: neither the code, the passwords (wrong and
/// right) nor the phone reach an answer or a log line; a wrong password
/// is 422 and may be tried again.
#[tokio::test]
async fn password_never_in_status_or_logs() {
    const CODE: &str = "24680";
    const WRONG: &str = "wrong-horse-battery";
    const RIGHT: &str = "correct-horse-battery";
    let logs = Logs::default();
    let _guard = logs.capture();
    let s = setup(
        Some(PHONE),
        vec![
            (ids::SEND_CODE, Reply::Ok(sent_code(&sms()))),
            (ids::SIGN_IN, Reply::Rpc(401, "SESSION_PASSWORD_NEEDED")),
            (ids::GET_PASSWORD, Reply::Ok(account_password_srp())),
            (ids::GET_PASSWORD, Reply::Ok(account_password_srp())),
            (
                ids::CHECK_PASSWORD,
                Reply::Rpc(400, "PASSWORD_HASH_INVALID"),
            ),
            (ids::GET_PASSWORD, Reply::Ok(account_password_srp())),
            (
                ids::CHECK_PASSWORD,
                Reply::Ok(authorization(777, Some("15550100"))),
            ),
        ],
    );
    let mut answers = Vec::new();
    let mut step = async |method: &str, path: &str, body: String| {
        let (status, text) = call(&s.app, method, path, &body).await;
        answers.push(text.clone());
        (status, text)
    };
    let (status, body) = step(
        "POST",
        "/v1/accounts/main/login",
        r#"{"method":"code"}"#.into(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        parse(&body),
        LoginStatus::CodeSent {
            kind: "sms".into(),
            timeout: Some(60)
        }
    );
    let (status, body) = step(
        "POST",
        "/v1/accounts/main/login/code",
        json!({ "code": CODE }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        parse(&body),
        LoginStatus::NeedPassword {
            hint: Some("pet".into())
        }
    );
    // A body that is not the expected JSON is refused without echoing it.
    let (status, body) = step(
        "POST",
        "/v1/accounts/main/login/password",
        format!(r#"{{"pass":"{WRONG}"}}"#),
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::BAD_REQUEST, r#"{"error":"invalid body"}"#)
    );
    let (status, body) = step(
        "POST",
        "/v1/accounts/main/login/password",
        json!({ "password": WRONG }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["reason"], "password_invalid");
    let (_, body) = step("GET", "/v1/accounts/main/login", String::new()).await;
    assert!(matches!(parse(&body), LoginStatus::NeedPassword { .. }));
    let (status, body) = step(
        "POST",
        "/v1/accounts/main/login/password",
        json!({ "password": RIGHT }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(parse(&body), LoginStatus::LoggedIn { user_id: 777 });
    let (_, body) = step("GET", "/v1/accounts/main/login", String::new()).await;
    assert_eq!(parse(&body), LoginStatus::LoggedIn { user_id: 777 });

    let logs = logs.text();
    assert!(logs.contains("need_password"), "{logs}");
    for secret in [CODE, WRONG, RIGHT, "15550100", "555 0100"] {
        assert!(!logs.contains(secret), "{secret} logged: {logs}");
        for answer in &answers {
            assert!(!answer.contains(secret), "{secret} answered: {answer}");
        }
    }
}

/// The identity gate through the API: another account's scan is logged
/// out again (`auth.logOut`), reported `wrong_account`, and the session
/// keeps neither a login nor a key.
#[tokio::test]
async fn wrong_account_logged_out() {
    let s = setup(
        Some(PHONE),
        vec![
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token_success(66, Some("15550199"))),
            ),
            (ids::LOG_OUT, Reply::Ok(logged_out())),
        ],
    );
    let (status, body) = call(
        &s.app,
        "POST",
        "/v1/accounts/main/login",
        r#"{"method":"qr"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let end = poll(&s.app, |st| matches!(st, LoginStatus::Failed { .. })).await;
    let LoginStatus::Failed { reason, message } = end else {
        unreachable!()
    };
    assert_eq!(reason, "wrong_account");
    assert!(!message.contains("15550"), "{message}");
    assert_eq!(
        s.fake.dial.calls(),
        vec![(2, ids::EXPORT_LOGIN_TOKEN), (2, ids::LOG_OUT)]
    );
    assert!(!s.fake.dial.marked_logged_in());
    let store = reopen(&s.dir).await;
    assert_eq!(store.login().expect("login"), None);
    assert_eq!(store.home_dc().expect("home"), None);
    assert_eq!(store.auth_key(2).expect("key"), None);
}

/// While the account runs, login and logout refuse with 409 and touch
/// neither Telegram nor the session.
#[tokio::test]
async fn login_refused_while_running() {
    let s = setup(None, vec![]);
    s.fake.running.store(true, Ordering::SeqCst);
    for (path, body) in [
        ("/v1/accounts/main/login", r#"{"method":"qr"}"#),
        ("/v1/accounts/main/logout", ""),
    ] {
        let (status, text) = call(&s.app, "POST", path, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}");
        assert_eq!(text, r#"{"error":"account running"}"#, "{path}");
    }
    assert!(s.fake.dial.dials().is_empty());
    assert!(
        !s.dir.path().join("main").exists(),
        "the session was not opened"
    );

    s.fake.running.store(false, Ordering::SeqCst);
    let (status, body) = call(
        &s.app,
        "POST",
        "/v1/accounts/other/login",
        r#"{"method":"qr"}"#,
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::NOT_FOUND, r#"{"error":"unknown account"}"#)
    );
    let (status, body) = call(&s.app, "GET", "/v1/accounts/main/login", "").await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::NOT_FOUND, r#"{"error":"no login"}"#)
    );
    let (status, _) = call(
        &s.app,
        "POST",
        "/v1/accounts/main/login",
        r#"{"method":"sms"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Another process holds the session: 409 `session held`.
#[tokio::test]
async fn held_session_is_refused() {
    let s = setup(None, vec![]);
    let _held = SessionStore::open(&s.dir.path().join("main")).expect("hold");
    let (status, body) = call(
        &s.app,
        "POST",
        "/v1/accounts/main/login",
        r#"{"method":"qr"}"#,
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::CONFLICT, r#"{"error":"session held"}"#)
    );
}

/// Telegram does not confirm the logout: 502, nothing cleared.
#[tokio::test]
async fn unconfirmed_logout_keeps_the_session() {
    let s = setup(None, vec![(ids::LOG_OUT, Reply::NoAnswer)]);
    {
        let store = SessionStore::open(&s.dir.path().join("main")).expect("open");
        store
            .set_home_key(2, &tgin_mtproto::AuthKey([1; 256]))
            .expect("key");
    }
    let (status, body) = call(&s.app, "POST", "/v1/accounts/main/logout", "").await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::BAD_GATEWAY, r#"{"error":"logout unconfirmed"}"#)
    );
    let store = reopen(&s.dir).await;
    assert_eq!(store.home_dc().expect("home"), Some(2));
}

/// Right after a login's end is seen, the session is free: a logout after
/// `logged_in`, and a new login after `failed`, never get 409 `session
/// held`. Multi-thread, as the admin server runs, 20 rounds each.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_free_right_after_the_login_ends() {
    for round in 0..20 {
        let s = setup(
            None,
            vec![
                (
                    ids::EXPORT_LOGIN_TOKEN,
                    Reply::Ok(login_token_success(77, None)),
                ),
                (ids::LOG_OUT, Reply::Ok(logged_out())),
            ],
        );
        let (status, body) = call(
            &s.app,
            "POST",
            "/v1/accounts/main/login",
            r#"{"method":"qr"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "round {round}: {body}");
        assert_eq!(parse(&body), LoginStatus::LoggedIn { user_id: 77 });
        let (status, body) = call(&s.app, "POST", "/v1/accounts/main/logout", "").await;
        assert_eq!(status, StatusCode::OK, "round {round}: logout: {body}");

        let s = setup(
            None,
            vec![
                (ids::EXPORT_LOGIN_TOKEN, Reply::Rpc(400, "API_ID_INVALID")),
                // The failed login left its key: asked, unknown, reset.
                (ids::GET_USERS, Reply::Rpc(401, "AUTH_KEY_UNREGISTERED")),
                (
                    ids::EXPORT_LOGIN_TOKEN,
                    Reply::Ok(login_token_success(78, None)),
                ),
            ],
        );
        let (_, body) = call(
            &s.app,
            "POST",
            "/v1/accounts/main/login",
            r#"{"method":"qr"}"#,
        )
        .await;
        assert!(
            matches!(parse(&body), LoginStatus::Failed { .. }),
            "round {round}: {body}"
        );
        let (status, body) = call(
            &s.app,
            "POST",
            "/v1/accounts/main/login",
            r#"{"method":"qr"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "round {round}: login again: {body}");
        assert_eq!(parse(&body), LoginStatus::LoggedIn { user_id: 78 });
    }
}

/// Without `admin.token` the account routes refuse with 403, before they
/// look at the account, Telegram or the session: on a token-less loopback
/// listener every local user and every web page could reach them.
#[tokio::test]
async fn account_routes_need_an_admin_token() {
    let s = setup_with(
        false,
        Some(PHONE),
        vec![(
            ids::EXPORT_LOGIN_TOKEN,
            Reply::Ok(login_token(b"t", later())),
        )],
    );
    let refused = r#"{"error":"admin.token required for account login"}"#;
    for (method, path, body) in [
        ("POST", "/v1/accounts/main/login", r#"{"method":"qr"}"#),
        ("POST", "/v1/accounts/main/login", r#"{"method":"code"}"#),
        ("GET", "/v1/accounts/main/login", ""),
        ("POST", "/v1/accounts/main/login/code", r#"{"code":"1"}"#),
        (
            "POST",
            "/v1/accounts/main/login/password",
            r#"{"password":"x"}"#,
        ),
        ("POST", "/v1/accounts/main/logout", ""),
        ("DELETE", "/v1/accounts/main/login", ""),
        ("POST", "/v1/accounts/other/login", r#"{"method":"qr"}"#),
    ] {
        let (status, text) = call(&s.app, method, path, body).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::FORBIDDEN, refused),
            "{method} {path}"
        );
    }
    assert!(s.fake.dial.dials().is_empty());
    assert!(
        !s.dir.path().join("main").exists(),
        "the session was not opened"
    );
}

/// A leftover key Telegram gives no clear answer about: 409 `session
/// unverified`, the key kept for a retry or `tgin admin logout`.
#[tokio::test]
async fn unverified_leftover_key_is_refused() {
    let s = setup(None, vec![(ids::GET_USERS, Reply::Rpc(500, "INTERNAL"))]);
    {
        let store = SessionStore::open(&s.dir.path().join("main")).expect("open");
        store
            .set_home_key(2, &tgin_mtproto::AuthKey([1; 256]))
            .expect("key");
    }
    let (status, body) = call(
        &s.app,
        "POST",
        "/v1/accounts/main/login",
        r#"{"method":"qr"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["error"], "session unverified");
    let store = reopen(&s.dir).await;
    assert_eq!(store.home_dc().expect("home"), Some(2));
}

/// `DELETE …/login` ends the login and releases the session at once; then
/// there is no login.
#[tokio::test]
async fn delete_cancels_the_login() {
    let s = setup(
        None,
        vec![(
            ids::EXPORT_LOGIN_TOKEN,
            Reply::Ok(login_token(b"t", later())),
        )],
    );
    let (status, body) = call(&s.app, "DELETE", "/v1/accounts/main/login", "").await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::NOT_FOUND, r#"{"error":"no login"}"#)
    );
    let (status, body) = call(
        &s.app,
        "POST",
        "/v1/accounts/main/login",
        r#"{"method":"qr"}"#,
    )
    .await;
    assert!(
        matches!(parse(&body), LoginStatus::Qr { .. }),
        "{status} {body}"
    );
    let (status, body) = call(&s.app, "DELETE", "/v1/accounts/main/login", "").await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"cancelled":true}"#)
    );
    // Released before the answer.
    SessionStore::open(&s.dir.path().join("main")).expect("the session is free");
    let (status, _) = call(&s.app, "GET", "/v1/accounts/main/login", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&s.app, "DELETE", "/v1/accounts/other/login", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A reload closes a login whose account is gone or whose `session_dir`
/// or `phone` changed, and keeps one whose account is unchanged.
#[tokio::test]
async fn reload_closes_logins_of_changed_accounts() {
    let s = setup(
        Some(PHONE),
        vec![(
            ids::EXPORT_LOGIN_TOKEN,
            Reply::Ok(login_token(b"t", later())),
        )],
    );
    let (_, body) = call(
        &s.app,
        "POST",
        "/v1/accounts/main/login",
        r#"{"method":"qr"}"#,
    )
    .await;
    assert!(matches!(parse(&body), LoginStatus::Qr { .. }), "{body}");
    let state = s.state.clone();
    let same = state.targets["main"].clone();

    // Unchanged (api_hash may change: it is not the login's identity).
    let mut hash = same.clone();
    hash.params.api_hash = "rotated".into();
    state.accounts.reconcile(|_| Some(hash.clone())).await;
    let (status, _) = call(&s.app, "GET", "/v1/accounts/main/login", "").await;
    assert_eq!(status, StatusCode::OK, "kept");

    for change in ["phone", "session_dir", "removed"] {
        let mut changed = same.clone();
        match change {
            "phone" => changed.params.phone = Some("+1 555 0199".into()),
            "session_dir" => changed.session_dir = s.dir.path().join("other"),
            _ => {}
        }
        if change != "phone" {
            // Start a fresh login for each case (the previous one closed).
            // The closed login left its key: asked, unknown, reset.
            s.fake.dial.extend(vec![
                (ids::GET_USERS, Reply::Rpc(401, "AUTH_KEY_UNREGISTERED")),
                (
                    ids::EXPORT_LOGIN_TOKEN,
                    Reply::Ok(login_token(b"u", later())),
                ),
            ]);
            let (_, body) = call(
                &s.app,
                "POST",
                "/v1/accounts/main/login",
                r#"{"method":"qr"}"#,
            )
            .await;
            assert!(
                matches!(parse(&body), LoginStatus::Qr { .. }),
                "{change}: {body}"
            );
        }
        let target = (change != "removed").then_some(changed);
        state.accounts.reconcile(|_| target.clone()).await;
        let (status, _) = call(&s.app, "GET", "/v1/accounts/main/login", "").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{change}: closed");
        SessionStore::open(&s.dir.path().join("main")).expect("released");
    }
}

/// A start that waits on Telegram about a leftover key holds no lock that
/// other requests need: GET and DELETE for another account, a reload's
/// reconcile, and a second start for the same account (409) all answer at
/// once. Reconcile ends the waiting start: when it finishes, its login is
/// not installed.
#[tokio::test(start_paused = true)]
async fn a_hanging_probe_blocks_nothing_else() {
    let s = setup(Some(PHONE), vec![(ids::GET_USERS, Reply::Hang)]);
    {
        let store = SessionStore::open(&s.dir.path().join("main")).expect("open");
        store
            .set_home_key(2, &tgin_mtproto::AuthKey([1; 256]))
            .expect("key");
    }
    let app = s.app.clone();
    let probing = tokio::spawn(async move {
        call(
            &app,
            "POST",
            "/v1/accounts/main/login",
            r#"{"method":"qr"}"#,
        )
        .await
    });
    // Until the start reserved its name and waits on Telegram.
    while s.fake.dial.calls().is_empty() {
        tokio::task::yield_now().await;
    }
    let quick = Duration::from_millis(1);
    for (method, path, body, want) in [
        (
            "GET",
            "/v1/accounts/second/login",
            "",
            StatusCode::NOT_FOUND,
        ),
        (
            "DELETE",
            "/v1/accounts/second/login",
            "",
            StatusCode::NOT_FOUND,
        ),
        ("GET", "/v1/accounts/main/login", "", StatusCode::OK),
        (
            "POST",
            "/v1/accounts/main/login",
            r#"{"method":"qr"}"#,
            StatusCode::CONFLICT,
        ),
        ("POST", "/v1/accounts/main/logout", "", StatusCode::CONFLICT),
    ] {
        let (status, text) = timeout(quick, call(&s.app, method, path, body))
            .await
            .unwrap_or_else(|_| panic!("{method} {path} waited on the probe"));
        assert_eq!(status, want, "{method} {path}: {text}");
        if want == StatusCode::CONFLICT {
            assert_eq!(text, r#"{"error":"login starting"}"#, "{method} {path}");
        }
    }
    let (_, text) = call(&s.app, "GET", "/v1/accounts/main/login", "").await;
    assert_eq!(parse(&text), LoginStatus::Connecting);

    // A reload that removes the account does not wait for the probe.
    timeout(quick, s.state.accounts.reconcile(|_| None))
        .await
        .expect("reconcile waited on the probe");
    let (status, _) = call(&s.app, "GET", "/v1/accounts/main/login", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The probe gives up at its deadline; the session is unchanged.
    let (status, text) = probing.await.expect("joined");
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert!(text.contains("session unverified"), "{text}");
    let store = reopen(&s.dir).await;
    assert_eq!(store.home_dc().expect("home"), Some(2));
}

/// A start cancelled while it waits on Telegram (`DELETE …/login`) is not
/// installed when Telegram answers: the new login ends at once, and the
/// session is free.
#[tokio::test(start_paused = true)]
async fn a_start_cancelled_while_probing_is_not_installed() {
    let s = setup(
        Some(PHONE),
        vec![
            (
                ids::GET_USERS,
                Reply::Later(
                    Duration::from_secs(5),
                    Box::new(Reply::Rpc(401, "AUTH_KEY_UNREGISTERED")),
                ),
            ),
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token(b"t", later())),
            ),
        ],
    );
    {
        let store = SessionStore::open(&s.dir.path().join("main")).expect("open");
        store
            .set_home_key(2, &tgin_mtproto::AuthKey([1; 256]))
            .expect("key");
    }
    let app = s.app.clone();
    let starting = tokio::spawn(async move {
        call(
            &app,
            "POST",
            "/v1/accounts/main/login",
            r#"{"method":"qr"}"#,
        )
        .await
    });
    while s.fake.dial.calls().is_empty() {
        tokio::task::yield_now().await;
    }
    let (status, text) = call(&s.app, "DELETE", "/v1/accounts/main/login", "").await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::OK, r#"{"cancelled":true}"#)
    );
    let (status, text) = starting.await.expect("joined");
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::CONFLICT, r#"{"error":"login cancelled"}"#)
    );
    let (status, _) = call(&s.app, "GET", "/v1/accounts/main/login", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    reopen(&s.dir).await;
}
