//! The login flows over scripted TL bytes (DECODE-ONLY: ported from the
//! spike's `login.rs` and `code_login.rs` tests) and the store they write.
//! The MTP-framed run over a real [`Sender`] is in `sender/tests.rs`.

use std::time::Duration;

use tempfile::TempDir;
use tokio::time::timeout;

use super::*;
use crate::testkit::{
    Reply, ScriptedDial, Tl, account_password, account_password_sized, account_password_srp,
    authorization, ids, logged_out, login_token, login_token_migrate, login_token_success,
    sent_code, sign_up_required, sms, update_login_token, updates_too_long, users,
};

const WAIT: Duration = Duration::from_secs(10);
const PHONE: &str = "+888 0000 0000";

fn store() -> (TempDir, SessionStore) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = SessionStore::open(tmp.path()).expect("open");
    (tmp, store)
}

fn params(phone: Option<&str>) -> LoginParams {
    LoginParams {
        api_id: 1,
        api_hash: "h".into(),
        phone: phone.map(str::to_owned),
    }
}

/// Telegram's `expires`, ten minutes ahead.
fn later() -> i32 {
    i32::try_from(now_unix() + 600).expect("i32 time")
}

async fn settled(h: &LoginHandle) -> LoginState {
    timeout(WAIT, h.settled()).await.expect("the login settles")
}

/// The final state, once the login's task ended.
async fn end(h: &LoginHandle) -> LoginState {
    timeout(WAIT, h.finished()).await.expect("the login ends")
}

fn reopen(tmp: &TempDir) -> SessionStore {
    SessionStore::open(tmp.path()).expect("released and reopened")
}

/// Starts `method` and returns the handle, the dial and the session dir.
fn begin(
    method: Method,
    phone: Option<&str>,
    script: Vec<(u32, Reply)>,
) -> (LoginHandle, ScriptedDial, TempDir) {
    let (tmp, store) = store();
    let dial = ScriptedDial::new(script);
    let h = start(store, dial.clone(), params(phone), method).expect("start");
    (h, dial, tmp)
}

/// DECODE-ONLY: `auth.loginToken` → a QR state whose URL is the token in
/// base64url, and nothing else of it shows in `Debug`.
#[tokio::test]
async fn login_token_maps_to_qr() {
    let expires = later();
    let (h, dial, _tmp) = begin(
        Method::Qr,
        None,
        vec![(
            ids::EXPORT_LOGIN_TOKEN,
            Reply::Ok(login_token(b"tok?", expires)),
        )],
    );
    match settled(&h).await {
        LoginState::Qr { url, expires: e } => {
            assert_eq!(url.expose(), "tg://login?token=dG9rPw");
            assert_eq!(e, expires);
            let shown = format!("{:?}", LoginState::Qr { url, expires: e });
            assert!(!shown.contains("dG9rPw"), "{shown}");
        }
        other => panic!("expected Qr, got {other:?}"),
    }
    assert_eq!(dial.dials(), vec![(FIRST_DC, false)]);
}

/// A scan (`updateLoginToken`) re-exports at once; the synthetic
/// `updatesTooLong` pushes, more than the push channel holds, are drained
/// and dropped meanwhile. The login is recorded, then `mark_logged_in`.
#[tokio::test]
async fn scan_logs_in_and_other_pushes_are_dropped() {
    let (h, dial, tmp) = begin(
        Method::Qr,
        None,
        vec![
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token(b"t", later())),
            ),
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token_success(77, None)),
            ),
        ],
    );
    assert!(matches!(settled(&h).await, LoginState::Qr { .. }));
    for _ in 0..64 {
        push(&dial, updates_too_long()).await;
    }
    assert!(
        matches!(h.state(), LoginState::Qr { .. }),
        "dropped, not acted on"
    );
    push(&dial, update_login_token()).await;
    assert_eq!(end(&h).await, LoginState::LoggedIn { user_id: 77 });
    assert!(dial.marked_logged_in());
    assert!(dial.done());
    drop(h);
    let store = wait_reopen(&tmp).await;
    let login = store.login().expect("login").expect("recorded");
    assert_eq!(login.self_user_id, 77);
    assert_eq!(store.home_dc().expect("home"), Some(FIRST_DC));
    let me = store.peer(77).expect("peer").expect("self row");
    assert_eq!(
        me.kind,
        PeerKind::User {
            bot: Some(false),
            is_self: Some(true)
        }
    );
}

/// Pushes `body`, waiting for room: the drain empties the channel.
async fn push(dial: &ScriptedDial, body: Vec<u8>) {
    timeout(WAIT, async {
        while !dial.push(body.clone()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the pushes are drained");
}

/// Without a scan the token is exported again once it expires (at most
/// 30 s).
#[tokio::test(start_paused = true)]
async fn token_is_renewed_before_it_expires() {
    let (h, dial, _tmp) = begin(
        Method::Qr,
        None,
        vec![
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token(b"one", later())),
            ),
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token(b"two", later())),
            ),
        ],
    );
    let first = settled(&h).await;
    let second = timeout(Duration::from_secs(31), h.changed())
        .await
        .expect("renewed within 30 s");
    assert_ne!(first, second);
    assert!(matches!(second, LoginState::Qr { .. }));
    assert_eq!(dial.calls().len(), 2);
}

/// DECODE-ONLY: `loginTokenMigrateTo` → a new key on the named DC, stored
/// as home, and `auth.importLoginToken` there → logged in.
#[tokio::test]
async fn migrate_to_imports_on_the_named_dc() {
    let (h, dial, tmp) = begin(
        Method::Qr,
        None,
        vec![
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token_migrate(4, b"mig")),
            ),
            (
                ids::IMPORT_LOGIN_TOKEN,
                Reply::Ok(login_token_success(1234, None)),
            ),
        ],
    );
    assert_eq!(end(&h).await, LoginState::LoggedIn { user_id: 1234 });
    assert_eq!(dial.dials(), vec![(2, false), (4, false)]);
    assert_eq!(
        dial.calls(),
        vec![(2, ids::EXPORT_LOGIN_TOKEN), (4, ids::IMPORT_LOGIN_TOKEN)]
    );
    drop(h);
    let store = wait_reopen(&tmp).await;
    assert_eq!(store.home_dc().expect("home"), Some(4));
    assert!(store.auth_key(4).expect("key").is_some());
}

async fn wait_reopen(tmp: &TempDir) -> SessionStore {
    timeout(WAIT, async {
        loop {
            match SessionStore::open(tmp.path()) {
                Ok(s) => return s,
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("the login releases the session")
}

/// DECODE-ONLY: a second migrate after the import is a typed error.
#[tokio::test]
async fn migrate_after_import_is_a_loop_error() {
    let (h, _dial, _tmp) = begin(
        Method::Qr,
        None,
        vec![
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token_migrate(4, b"a")),
            ),
            (
                ids::IMPORT_LOGIN_TOKEN,
                Reply::Ok(login_token_migrate(5, b"b")),
            ),
        ],
    );
    assert_eq!(
        end(&h).await,
        LoginState::Failed(LoginError::MigrateLoop { dc: 5 })
    );
}

/// After `importLoginToken` was written and no answer came, the probe
/// finds the login: it happened.
#[tokio::test]
async fn import_without_answer_is_probed() {
    let (h, dial, _tmp) = begin(
        Method::Qr,
        None,
        vec![
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token_migrate(4, b"a")),
            ),
            (ids::IMPORT_LOGIN_TOKEN, Reply::NoAnswer),
            (ids::GET_USERS, Reply::Ok(users(55, None))),
        ],
    );
    assert_eq!(end(&h).await, LoginState::LoggedIn { user_id: 55 });
    assert!(dial.done());
}

/// DECODE-ONLY: `SESSION_PASSWORD_NEEDED` on export → `account.getPassword`
/// → `NeedPassword { hint }`.
#[tokio::test]
async fn session_password_needed_asks_for_the_password() {
    let (h, _dial, _tmp) = begin(
        Method::Qr,
        None,
        vec![
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Rpc(401, "SESSION_PASSWORD_NEEDED"),
            ),
            (ids::GET_PASSWORD, Reply::Ok(account_password())),
        ],
    );
    assert_eq!(
        settled(&h).await,
        LoginState::NeedPassword {
            hint: Some("pet".into())
        }
    );
}

fn two_fa(check: Vec<(u32, Reply)>) -> Vec<(u32, Reply)> {
    let mut script = vec![
        (
            ids::EXPORT_LOGIN_TOKEN,
            Reply::Rpc(401, "SESSION_PASSWORD_NEEDED"),
        ),
        (ids::GET_PASSWORD, Reply::Ok(account_password_srp())),
    ];
    script.extend(check);
    script
}

/// A wrong password is answered and may be tried again; the right one
/// logs in. Real SRP runs on Telegram's prime.
#[tokio::test]
async fn wrong_password_is_retryable_then_right_one_logs_in() {
    let (h, dial, _tmp) = begin(
        Method::Qr,
        None,
        two_fa(vec![
            (ids::GET_PASSWORD, Reply::Ok(account_password_srp())),
            (
                ids::CHECK_PASSWORD,
                Reply::Rpc(400, "PASSWORD_HASH_INVALID"),
            ),
            (ids::GET_PASSWORD, Reply::Ok(account_password_srp())),
            (ids::CHECK_PASSWORD, Reply::Ok(authorization(777, None))),
        ]),
    );
    assert!(matches!(settled(&h).await, LoginState::NeedPassword { .. }));
    assert_eq!(
        h.submit_password("wrong".into()).await,
        Err(LoginError::PasswordInvalid)
    );
    assert!(matches!(h.state(), LoginState::NeedPassword { .. }));
    assert_eq!(
        h.submit_password("hunter2".into()).await,
        Ok(LoginState::LoggedIn { user_id: 777 })
    );
    assert!(dial.done());
    // The password itself never reaches Telegram, only the SRP proof.
    assert!(
        dial.bodies()
            .iter()
            .all(|b| !b.windows(7).any(|w| w == b"hunter2"))
    );
}

/// After `auth.checkPassword` was written and no answer came, the probe
/// asks who is logged in.
#[tokio::test]
async fn check_password_without_answer_is_probed() {
    for (probe, want) in [
        (
            Reply::Ok(users(9, None)),
            LoginState::LoggedIn { user_id: 9 },
        ),
        (
            Reply::Rpc(401, "AUTH_KEY_UNREGISTERED"),
            LoginState::Failed(LoginError::Sender(SenderError::NoAnswer.to_string())),
        ),
    ] {
        let (h, dial, _tmp) = begin(
            Method::Qr,
            None,
            two_fa(vec![
                (ids::GET_PASSWORD, Reply::Ok(account_password_srp())),
                (ids::CHECK_PASSWORD, Reply::NoAnswer),
                (ids::GET_USERS, probe),
            ]),
        );
        settled(&h).await;
        h.submit_password("pw".into()).await.expect("handed over");
        assert_eq!(end(&h).await, want);
        assert!(dial.done());
    }
}

/// DECODE-ONLY: bad SRP parameters end typed instead of panicking: a `p`
/// that is not a safe prime, and an `srp_B` or `secure_random` longer than
/// 256 bytes (grammers would panic padding them) [C: S3 F8]. No
/// `auth.checkPassword` is sent.
#[tokio::test]
async fn unusable_srp_parameters_end_typed() {
    for (want, password) in [
        ("p/g failed the safety check", account_password()),
        (
            "srp_B longer than 256 bytes",
            account_password_sized(&[5; 257], &[6; 32]),
        ),
        (
            "secure_random longer than 256 bytes",
            account_password_sized(&[5; 256], &[6; 257]),
        ),
    ] {
        let (h, dial, _tmp) = begin(
            Method::Qr,
            None,
            two_fa(vec![(ids::GET_PASSWORD, Reply::Ok(password))]),
        );
        settled(&h).await;
        h.submit_password("x".into()).await.expect("handed over");
        assert_eq!(
            end(&h).await,
            LoginState::Failed(LoginError::PasswordParams(want))
        );
        assert!(
            !dial
                .calls()
                .iter()
                .any(|(_, id)| *id == ids::CHECK_PASSWORD),
            "{want}"
        );
    }
}

/// At exactly 256 bytes SRP runs.
#[tokio::test]
async fn srp_at_256_bytes_runs() {
    let (h, _dial, _tmp) = begin(
        Method::Qr,
        None,
        two_fa(vec![
            (
                ids::GET_PASSWORD,
                Reply::Ok(account_password_sized(&[5; 256], &[6; 256])),
            ),
            (ids::CHECK_PASSWORD, Reply::Ok(authorization(3, None))),
        ]),
    );
    settled(&h).await;
    assert_eq!(
        h.submit_password("x".into()).await,
        Ok(LoginState::LoggedIn { user_id: 3 })
    );
}

/// The identity gate: a scan by another account is logged out again
/// (`auth.logOut` on the new key), nothing is recorded and the key is
/// gone [C: S3 F1].
#[tokio::test]
async fn wrong_account_is_logged_out_and_not_recorded() {
    let (h, dial, tmp) = begin(
        Method::Qr,
        Some(PHONE),
        vec![
            (
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token_success(66, Some("79990000000"))),
            ),
            (ids::LOG_OUT, Reply::Ok(logged_out())),
        ],
    );
    assert_eq!(end(&h).await, LoginState::Failed(LoginError::WrongAccount));
    assert!(dial.done(), "auth.logOut was sent");
    assert!(!dial.marked_logged_in());
    drop(h);
    let store = wait_reopen(&tmp).await;
    assert_eq!(store.login().expect("login"), None);
    assert_eq!(store.home_dc().expect("home"), None);
    assert_eq!(store.auth_key(FIRST_DC).expect("key"), None);
}

/// A hidden phone is no match either; the configured number matches in
/// any formatting.
#[tokio::test]
async fn identity_gate_compares_digits() {
    assert!(same_phone("+888 0000-0000", Some("88800000000")));
    assert!(!same_phone("+888 0000 0000", Some("88800000001")));
    assert!(!same_phone("+888 0000 0000", None));
    assert!(!same_phone("", Some("")));
    let (h, _dial, _tmp) = begin(
        Method::Qr,
        Some(PHONE),
        vec![(
            ids::EXPORT_LOGIN_TOKEN,
            Reply::Ok(login_token_success(5, Some("88800000000"))),
        )],
    );
    assert_eq!(end(&h).await, LoginState::LoggedIn { user_id: 5 });
}

#[test]
fn logged_in_session_refuses_a_login() {
    let (_tmp, store) = store();
    store.set_home_key(2, &AuthKey([1; 256])).expect("key");
    store
        .record_login(Peer {
            id: 1,
            kind: PeerKind::User {
                bot: Some(false),
                is_self: None,
            },
            access_hash: None,
        })
        .expect("login");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let _guard = runtime.enter();
    assert!(matches!(
        start(store, ScriptedDial::default(), params(None), Method::Qr),
        Err(StartError::LoggedIn)
    ));
}

/// A key left by an unfinished attempt is replaced: the login starts from
/// a reset session.
#[tokio::test]
async fn a_new_login_resets_a_half_done_one() {
    let (tmp, store) = store();
    store.set_home_key(4, &AuthKey([7; 256])).expect("key");
    let dial = ScriptedDial::new(vec![(
        ids::EXPORT_LOGIN_TOKEN,
        Reply::Ok(login_token_success(8, None)),
    )]);
    let h = start(store, dial.clone(), params(None), Method::Qr).expect("start");
    assert_eq!(end(&h).await, LoginState::LoggedIn { user_id: 8 });
    drop(h);
    let store = wait_reopen(&tmp).await;
    assert_eq!(store.auth_key(4).expect("old key"), None);
    assert_eq!(store.home_dc().expect("home"), Some(FIRST_DC));
}

/// `log_out`: `auth.logOut` on the home key, then the reset.
#[tokio::test]
async fn log_out_calls_telegram_then_resets() {
    let (tmp, store) = store();
    store.set_home_key(2, &AuthKey([1; 256])).expect("key");
    let dial = ScriptedDial::new(vec![(ids::LOG_OUT, Reply::Ok(logged_out()))]);
    log_out(&store, &dial).await.expect("logged out");
    assert_eq!(dial.dials(), vec![(2, true)]);
    assert!(dial.done());
    assert_eq!(store.home_dc().expect("home"), None);
    drop(store);
    assert_eq!(reopen(&tmp).auth_key(2).expect("key"), None);
}

/// A key Telegram no longer knows counts as logged out; no answer deletes
/// nothing.
#[tokio::test]
async fn log_out_outcomes() {
    for (reply, gone) in [
        (Reply::KeyRevoked, true),
        (Reply::Rpc(401, "AUTH_KEY_UNREGISTERED"), true),
        (Reply::NoAnswer, false),
        (Reply::Rpc(500, "INTERNAL"), false),
    ] {
        let (_tmp, store) = store();
        store.set_home_key(2, &AuthKey([1; 256])).expect("key");
        let dial = ScriptedDial::new(vec![(ids::LOG_OUT, reply.clone())]);
        let out = log_out(&store, &dial).await;
        assert_eq!(out.is_ok(), gone, "{reply:?}");
        assert_eq!(store.auth_key(2).expect("key").is_none(), gone, "{reply:?}");
    }
    // Nothing to log out: only the reset.
    let (_tmp, store) = store();
    let dial = ScriptedDial::default();
    log_out(&store, &dial).await.expect("nothing to do");
    assert!(dial.dials().is_empty());
}

// ---- the code flow (DECODE-ONLY; never run against the account) ----

fn code_state(reply: Reply) -> (LoginHandle, ScriptedDial, TempDir) {
    begin(Method::Code, Some(PHONE), vec![(ids::SEND_CODE, reply)])
}

#[tokio::test]
async fn each_code_type_maps_to_code_sent() {
    let cases: Vec<(&str, Vec<u8>, CodeKind)> = vec![
        ("app", Tl::ctor(0x3dbb_5986).int(5).done(), CodeKind::App),
        ("sms", sms(), CodeKind::Sms),
        ("call", Tl::ctor(0x5353_e5a7).int(5).done(), CodeKind::Call),
        (
            "flash",
            Tl::ctor(0xab03_c6d9).string("+1*").done(),
            CodeKind::FlashCall,
        ),
        (
            "missed",
            Tl::ctor(0x8200_6484).string("+1").int(4).done(),
            CodeKind::MissedCall,
        ),
        (
            "fragment",
            Tl::ctor(0xd956_5c39)
                .string("https://fragment.com/x")
                .int(5)
                .done(),
            CodeKind::FragmentSms,
        ),
        (
            "firebase",
            Tl::ctor(0x009f_d736)
                .uint((1 << 0) | (1 << 1))
                .bytes(b"nonce")
                .string("receipt")
                .int(30)
                .int(6)
                .done(),
            CodeKind::FirebaseSms,
        ),
        (
            "word",
            Tl::ctor(0xa416_ac81).uint(1).string("ab").done(),
            CodeKind::SmsWord,
        ),
        (
            "phrase",
            Tl::ctor(0xb377_94af).uint(0).done(),
            CodeKind::SmsPhrase,
        ),
    ];
    for (name, bytes, want) in cases {
        let (h, _dial, _tmp) = code_state(Reply::Ok(sent_code(&bytes)));
        assert_eq!(
            settled(&h).await,
            LoginState::CodeSent {
                kind: want,
                timeout: Some(60)
            },
            "{name}"
        );
    }
}

#[tokio::test]
async fn email_and_payment_states_end_the_login() {
    let email = Tl::ctor(0xf450_f59b)
        .uint((1 << 0) | (1 << 3))
        .string("a***@x.y")
        .int(6)
        .int(3600)
        .done();
    let setup = Tl::ctor(0xa549_1dea).uint(1 << 1).done();
    let payment = Tl::ctor(0xf882_7ebf)
        .string("store")
        .string("hash")
        .string("support@x.y")
        .string("subject")
        .int(30)
        .string("USD")
        .long(499)
        .done();
    for (reply, want) in [
        (sent_code(&email), LoginState::NeedEmailCode),
        (sent_code(&setup), LoginState::NeedEmailSetup),
        (payment, LoginState::PaymentRequired),
    ] {
        let (h, _dial, _tmp) = code_state(Reply::Ok(reply));
        assert_eq!(end(&h).await, want);
    }
}

#[tokio::test]
async fn sent_code_success_logs_in() {
    let bytes = Tl::ctor(0x2390_fe44)
        .raw(&authorization(555, Some("88800000000")))
        .done();
    let (h, _dial, _tmp) = code_state(Reply::Ok(bytes));
    assert_eq!(end(&h).await, LoginState::LoggedIn { user_id: 555 });
}

fn after_code(sign_in: Reply, more: Vec<(u32, Reply)>) -> (LoginHandle, ScriptedDial, TempDir) {
    let mut script = vec![
        (ids::SEND_CODE, Reply::Ok(sent_code(&sms()))),
        (ids::SIGN_IN, sign_in),
    ];
    script.extend(more);
    begin(Method::Code, Some(PHONE), script)
}

#[tokio::test]
async fn code_errors_map() {
    let (h, dial, _tmp) = after_code(
        Reply::Rpc(400, "PHONE_CODE_INVALID"),
        vec![(ids::SIGN_IN, Reply::Rpc(400, "PHONE_CODE_EXPIRED"))],
    );
    assert!(matches!(settled(&h).await, LoginState::CodeSent { .. }));
    assert_eq!(
        h.submit_code("1".into()).await,
        Err(LoginError::CodeInvalid),
        "retryable"
    );
    h.submit_code("2".into()).await.expect("handed over");
    assert_eq!(end(&h).await, LoginState::Failed(LoginError::CodeExpired));
    assert!(dial.done());
}

#[tokio::test]
async fn code_then_password_logs_in() {
    let (h, _dial, _tmp) = after_code(
        Reply::Rpc(401, "SESSION_PASSWORD_NEEDED"),
        vec![
            (ids::GET_PASSWORD, Reply::Ok(account_password_srp())),
            (ids::GET_PASSWORD, Reply::Ok(account_password_srp())),
            (
                ids::CHECK_PASSWORD,
                Reply::Ok(authorization(777, Some("88800000000"))),
            ),
        ],
    );
    settled(&h).await;
    assert_eq!(
        h.submit_code("12345".into()).await,
        Ok(LoginState::NeedPassword {
            hint: Some("pet".into())
        })
    );
    assert_eq!(
        h.submit_password("x".into()).await,
        Ok(LoginState::LoggedIn { user_id: 777 })
    );
}

#[tokio::test]
async fn sign_up_required_and_truncated_replies_are_typed() {
    let (h, _dial, _tmp) = after_code(Reply::Ok(sign_up_required()), vec![]);
    settled(&h).await;
    h.submit_code("1".into()).await.expect("handed over");
    assert_eq!(
        end(&h).await,
        LoginState::Failed(LoginError::SignUpRequired)
    );
    let mut bytes = sent_code(&sms());
    bytes.truncate(10);
    let (h, _dial, _tmp) = code_state(Reply::Ok(bytes));
    assert_eq!(end(&h).await, LoginState::Failed(LoginError::Decode));
}

#[tokio::test]
async fn code_flow_needs_a_phone_and_inputs_must_fit_the_state() {
    let (h, dial, _tmp) = begin(Method::Code, None, vec![]);
    assert_eq!(end(&h).await, LoginState::Failed(LoginError::NoPhone));
    assert!(dial.calls().is_empty());

    let (h, _dial, _tmp) = begin(
        Method::Qr,
        None,
        vec![(
            ids::EXPORT_LOGIN_TOKEN,
            Reply::Ok(login_token(b"t", later())),
        )],
    );
    settled(&h).await;
    assert_eq!(
        h.submit_password("x".into()).await,
        Err(LoginError::NotExpected)
    );
    assert_eq!(
        h.submit_code("1".into()).await,
        Err(LoginError::NotExpected)
    );
}

#[test]
fn rpc_errors_map() {
    assert_eq!(
        LoginError::rpc(420, "FLOOD_WAIT_30"),
        LoginError::FloodWait { seconds: 30 }
    );
    assert_eq!(
        LoginError::rpc(400, "PASSWORD_HASH_INVALID"),
        LoginError::PasswordInvalid
    );
    assert_eq!(
        LoginError::rpc(400, "PHONE_NUMBER_BANNED"),
        LoginError::Rpc {
            code: 400,
            name: "PHONE_NUMBER_BANNED".into()
        }
    );
}

#[test]
fn login_token_push_is_recognised() {
    assert!(is_login_token(&update_login_token()));
    assert!(!is_login_token(&updates_too_long()));
    assert!(!is_login_token(&[1, 2]));
}

/// Whoever sees a login's end may use the session at once: the store is
/// released before the final state is published. On a multi-thread
/// runtime the watcher runs beside the flow, so publishing first would let
/// this open hit `Held`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn final_state_is_published_after_the_session_is_released() {
    for round in 0..20 {
        let (h, _dial, tmp) = begin(
            Method::Qr,
            None,
            vec![(
                ids::EXPORT_LOGIN_TOKEN,
                Reply::Ok(login_token_success(8, None)),
            )],
        );
        assert_eq!(end(&h).await, LoginState::LoggedIn { user_id: 8 });
        let opened = SessionStore::open(tmp.path());
        assert!(
            opened.is_ok(),
            "round {round}: the session is still held after the end was published"
        );
        drop(opened);
        drop(h);
    }
}

/// The gate resets only after Telegram confirmed the logout (an answer, a
/// 401, or a key it no longer knows). Without one, the key stays home with
/// no login marker, for `tgin admin logout` to retry, and the end says so.
#[tokio::test]
async fn wrong_account_logout_must_be_confirmed_before_the_reset() {
    for (reply, confirmed) in [
        (Reply::Ok(logged_out()), true),
        (Reply::Rpc(401, "AUTH_KEY_UNREGISTERED"), true),
        (Reply::KeyRevoked, true),
        (Reply::NoAnswer, false),
        (Reply::Rpc(420, "FLOOD_WAIT_9"), false),
    ] {
        let (h, dial, tmp) = begin(
            Method::Qr,
            Some(PHONE),
            vec![
                (
                    ids::EXPORT_LOGIN_TOKEN,
                    Reply::Ok(login_token_success(66, Some("79990000000"))),
                ),
                (ids::LOG_OUT, reply.clone()),
            ],
        );
        let want = if confirmed {
            LoginError::WrongAccount
        } else {
            LoginError::WrongAccountNotLoggedOut
        };
        assert_eq!(end(&h).await, LoginState::Failed(want.clone()), "{reply:?}");
        assert!(dial.done(), "{reply:?}");
        drop(h);
        let store = wait_reopen(&tmp).await;
        assert_eq!(store.login().expect("login"), None, "{reply:?}");
        assert_eq!(
            store.home_dc().expect("home").is_some(),
            !confirmed,
            "{reply:?}"
        );
        if !confirmed {
            assert!(want.to_string().contains("tgin admin logout"), "{want}");
            assert_eq!(want.code(), "wrong_account_not_logged_out");
        }
    }
}
