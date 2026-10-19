//! The raw-TL RPC endpoint against a fake account (plan Task 9).

use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use parking_lot::Mutex;
use tgin::mtproto_rpc::{RpcAccount, RpcEndpoint, router};
use tgin_core::config::{MethodAcl, MethodGlob, Rpc, RpcClient};
use tgin_mtproto::decode::BoxKind;
use tgin_mtproto::media::{AccountSessions, Connect, KeyStore, MediaConnected, Session};
use tgin_mtproto::rpc::{AccountRpc, AffectedSink, BoxFuture, InvokeError, Route};
use tgin_mtproto::schema::{API, MTPROTO};
use tgin_mtproto::{AuthKey, RpcOutcome, SenderError, SessionError};
use tgin_runtime::secret::Secret;
use tower::ServiceExt as _;

const TOKEN: &str = "client-token-0123456789";
const LAYER: i32 = 230;

type Scripted = Result<RpcOutcome, InvokeError>;

/// An account whose answers are scripted; records every call.
#[derive(Default)]
struct FakeAccount {
    script: Mutex<VecDeque<Scripted>>,
    calls: Mutex<Vec<(Route, Bytes)>>,
}

impl FakeAccount {
    fn new(script: impl IntoIterator<Item = Scripted>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into_iter().collect()),
            calls: Mutex::default(),
        })
    }

    fn calls(&self) -> Vec<(Route, Bytes)> {
        self.calls.lock().clone()
    }
}

impl AccountRpc for FakeAccount {
    fn layer(&self) -> i32 {
        LAYER
    }

    fn invoke(&self, route: Route, body: Bytes) -> BoxFuture<'_, Result<RpcOutcome, InvokeError>> {
        self.calls.lock().push((route, body));
        let next = self
            .script
            .lock()
            .pop_front()
            .unwrap_or(Err(InvokeError::Sender(SenderError::NotSent)));
        Box::pin(async move { next })
    }
}

#[derive(Default)]
struct FakeSink(Mutex<Vec<(BoxKind, i32, i32)>>);

impl AffectedSink for FakeSink {
    fn feed_affected(&self, box_kind: BoxKind, pts: i32, pts_count: i32) {
        self.0.lock().push((box_kind, pts, pts_count));
    }
}

fn tl(json: &str) -> Vec<u8> {
    let mut out = Vec::new();
    API.from_json(json, &mut out).expect("valid TL-JSON");
    out
}

fn rpc_error_tl(code: i32, message: &str) -> Vec<u8> {
    let mut out = Vec::new();
    MTPROTO
        .from_json(
            &format!(r#"{{"_":"rpc_error","error_code":{code},"error_message":"{message}"}}"#),
            &mut out,
        )
        .expect("valid rpc_error");
    out
}

fn ok(json: &str) -> Scripted {
    Ok(RpcOutcome::Result(Bytes::from(tl(json))))
}

fn err(code: i32, message: &str) -> Scripted {
    Ok(RpcOutcome::Error {
        code,
        message: message.to_owned(),
    })
}

const GET_CONFIG: &str = r#"{"_":"help.getConfig"}"#;
const NEAREST: &str = r#"{"_":"nearestDc","country":"KZ","this_dc":2,"nearest_dc":2}"#;

fn rpc_config(max_rps: u32, acl: MethodAcl) -> Rpc {
    Rpc {
        max_rps: NonZeroU32::new(max_rps).expect("positive"),
        flood_sleep_threshold: Duration::from_secs(10),
        clients: vec![RpcClient {
            name: "bot".to_owned(),
            token: Secret::new(TOKEN.to_owned()),
            acl,
        }],
    }
}

fn allow_all() -> MethodAcl {
    MethodAcl {
        allow: vec![MethodGlob::Any],
        deny: Vec::new(),
    }
}

fn app_with(account: Arc<dyn AccountRpc>, sink: Arc<FakeSink>, rpc: &Rpc) -> Router {
    let mut endpoint = RpcEndpoint::new();
    endpoint.insert(RpcAccount::new("main", rpc, account, sink));
    router(Arc::new(endpoint))
}

fn app(account: Arc<FakeAccount>) -> Router {
    app_with(account, Arc::default(), &rpc_config(100, allow_all()))
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Answer {
    fn error(&self) -> String {
        let v: serde_json::Value = serde_json::from_slice(&self.body).expect("JSON error");
        v["error"].as_str().expect("error field").to_owned()
    }

    fn retry_after(&self) -> Option<&str> {
        self.headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
    }
}

async fn send_with(app: &Router, layer: &str, body: Vec<u8>, extra: &[(&str, &str)]) -> Answer {
    let mut req = Request::post("/mtproto/v1/main/invoke")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-tgin-layer", layer);
    if !extra.iter().any(|(k, _)| *k == "content-type") {
        req = req.header("content-type", "application/octet-stream");
    }
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body)).expect("request"))
        .await
        .expect("infallible");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
    Answer {
        status,
        headers,
        body,
    }
}

async fn send(app: &Router, body: Vec<u8>) -> Answer {
    send_with(app, "230", body, &[]).await
}

fn init_connection(layer: i32, query: &str) -> String {
    format!(
        r#"{{"_":"invokeWithLayer","layer":{layer},"query":{{"_":"initConnection","api_id":1,"device_model":"pc","system_version":"x","app_version":"1","system_lang_code":"en","lang_pack":"","lang_code":"en","query":{query}}}}}"#
    )
}

#[tokio::test]
async fn wrapped_first_call_is_unwrapped() {
    let account = FakeAccount::new([ok(NEAREST)]);
    let app = app(Arc::clone(&account));
    let answer = send(&app, tl(&init_connection(LAYER, GET_CONFIG))).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, Bytes::from(tl(NEAREST)));
    // Telegram sees the bare call: the account's layer never changes.
    assert_eq!(
        account.calls(),
        vec![(Route::Main, Bytes::from(tl(GET_CONFIG)))]
    );
}

#[tokio::test]
async fn foreign_layer_409() {
    let account = FakeAccount::new([]);
    let app = app(Arc::clone(&account));
    let wrapped = send(&app, tl(&init_connection(229, GET_CONFIG))).await;
    assert_eq!(wrapped.status, StatusCode::CONFLICT);
    assert_eq!(wrapped.error(), "layer_mismatch");
    let header = send_with(&app, "229", tl(GET_CONFIG), &[]).await;
    assert_eq!(header.status, StatusCode::CONFLICT);
    // Any other wrapper is refused.
    let other = send(
        &app,
        tl(&format!(
            r#"{{"_":"invokeWithoutUpdates","query":{GET_CONFIG}}}"#
        )),
    )
    .await;
    assert_eq!(other.status, StatusCode::BAD_REQUEST);
    assert_eq!(other.error(), "wrapper_refused");
    assert!(account.calls().is_empty());
}

#[tokio::test]
async fn get_difference_403() {
    let account = FakeAccount::new([]);
    let app = app(Arc::clone(&account));
    for body in [
        r#"{"_":"updates.getDifference","pts":1,"date":1,"qts":0}"#,
        r#"{"_":"updates.getState"}"#,
    ] {
        let answer = send(&app, tl(body)).await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN);
        assert_eq!(answer.error(), "updates_method");
    }
    // Wrapped, too.
    let wrapped = send(
        &app,
        tl(&init_connection(LAYER, r#"{"_":"updates.getState"}"#)),
    )
    .await;
    assert_eq!(wrapped.status, StatusCode::FORBIDDEN);
    assert!(account.calls().is_empty());
}

#[tokio::test]
async fn allowlist_deny_by_default() {
    let account = FakeAccount::new([ok(NEAREST)]);
    let empty = app_with(
        Arc::clone(&account) as Arc<dyn AccountRpc>,
        Arc::default(),
        &rpc_config(100, MethodAcl::default()),
    );
    let answer = send(&empty, tl(GET_CONFIG)).await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(answer.error(), "method_not_allowed");

    let acl = MethodAcl {
        allow: vec![MethodGlob::Namespace("help".to_owned())],
        deny: vec![MethodGlob::Exact("help.getNearestDc".to_owned())],
    };
    let partial = app_with(
        Arc::clone(&account) as Arc<dyn AccountRpc>,
        Arc::default(),
        &rpc_config(100, acl),
    );
    let denied = send(&partial, tl(r#"{"_":"help.getNearestDc"}"#)).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert!(account.calls().is_empty());
    let allowed = send(&partial, tl(GET_CONFIG)).await;
    assert_eq!(allowed.status, StatusCode::OK);
    assert_eq!(account.calls().len(), 1);
}

#[tokio::test]
async fn wrong_token_401() {
    let account = FakeAccount::new([]);
    let app = app(Arc::clone(&account));
    let resp = app
        .oneshot(
            Request::post("/mtproto/v1/main/invoke")
                .header("authorization", "Bearer client-token-0123456780")
                .header("x-tgin-layer", "230")
                .body(Body::from(tl(GET_CONFIG)))
                .expect("request"),
        )
        .await
        .expect("infallible");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(account.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn flood_below_threshold_slept_once() {
    let account = FakeAccount::new([err(420, "FLOOD_WAIT_5"), ok(NEAREST)]);
    let app = app(Arc::clone(&account));
    let start = tokio::time::Instant::now();
    let answer = send(&app, tl(GET_CONFIG)).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    let calls = account.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|(route, _)| *route == Route::Main));

    // A second flood wait after the sleep is not slept again.
    let account = FakeAccount::new([err(420, "FLOOD_WAIT_5"), err(420, "FLOOD_WAIT_5")]);
    let app = self::app(Arc::clone(&account));
    let start = tokio::time::Instant::now();
    let answer = send(&app, tl(GET_CONFIG)).await;
    assert_eq!(answer.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(answer.retry_after(), Some("5"));
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    assert_eq!(account.calls().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn flood_above_threshold_429_retry_after() {
    let account = FakeAccount::new([err(420, "FLOOD_WAIT_60"), err(420, "SLOWMODE_WAIT_7")]);
    let app = app(Arc::clone(&account));
    let start = tokio::time::Instant::now();
    let flood = send(&app, tl(GET_CONFIG)).await;
    assert_eq!(flood.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(flood.retry_after(), Some("60"));
    assert_eq!(flood.body, Bytes::from(rpc_error_tl(420, "FLOOD_WAIT_60")));
    assert_eq!(start.elapsed(), Duration::ZERO);
    let slow = send(&app, tl(GET_CONFIG)).await;
    assert_eq!(slow.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(slow.retry_after(), Some("7"));
    assert_eq!(account.calls().len(), 2);
}

#[tokio::test]
async fn no_answer_502_never_resent() {
    let account = FakeAccount::new([Err(InvokeError::Sender(SenderError::NoAnswer)), ok(NEAREST)]);
    let app = app(Arc::clone(&account));
    let answer = send(&app, tl(GET_CONFIG)).await;
    assert_eq!(answer.status, StatusCode::BAD_GATEWAY);
    assert_eq!(answer.error(), "no_answer");
    assert_eq!(account.calls().len(), 1);
}

#[tokio::test]
async fn not_sent_resent_once_fresh_connection() {
    // The sender opens a fresh connection for the resend (its contract);
    // the endpoint sends once more, never twice.
    let account = FakeAccount::new([Err(InvokeError::Sender(SenderError::NotSent)), ok(NEAREST)]);
    let app = app(Arc::clone(&account));
    let answer = send(&app, tl(GET_CONFIG)).await;
    assert_eq!(answer.status, StatusCode::OK);
    let calls = account.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1]);

    let account = FakeAccount::new([
        Err(InvokeError::Sender(SenderError::NotSent)),
        Err(InvokeError::Sender(SenderError::NotSent)),
        ok(NEAREST),
    ]);
    let app = self::app(Arc::clone(&account));
    let answer = send(&app, tl(GET_CONFIG)).await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.retry_after(), Some("1"));
    assert_eq!(account.calls().len(), 2);
}

#[tokio::test]
async fn init_refused_420_is_429_else_502() {
    let account = FakeAccount::new([
        Err(InvokeError::Sender(SenderError::InitRefused {
            code: 420,
            message: "FLOOD_WAIT_30".to_owned(),
        })),
        Err(InvokeError::Sender(SenderError::InitRefused {
            code: 400,
            message: "API_ID_INVALID".to_owned(),
        })),
    ]);
    let app = app(Arc::clone(&account));
    let flood = send(&app, tl(GET_CONFIG)).await;
    assert_eq!(flood.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(flood.retry_after(), Some("30"));
    let refused = send(&app, tl(GET_CONFIG)).await;
    assert_eq!(refused.status, StatusCode::BAD_GATEWAY);
    assert_eq!(refused.error(), "init_refused");
    assert_eq!(account.calls().len(), 2);
}

#[tokio::test]
async fn frozen_403() {
    let account = FakeAccount::new([err(420, "FROZEN_METHOD_INVALID")]);
    let app = app(Arc::clone(&account));
    let answer = send(&app, tl(GET_CONFIG)).await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(
        answer.body,
        Bytes::from(rpc_error_tl(420, "FROZEN_METHOD_INVALID"))
    );
}

#[tokio::test]
async fn rpc_error_passed_raw() {
    let account = FakeAccount::new([err(400, "PEER_ID_INVALID"), err(400, "PEER_ID_INVALID")]);
    let app = app(Arc::clone(&account));
    let raw = send(&app, tl(GET_CONFIG)).await;
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(raw.body, Bytes::from(rpc_error_tl(400, "PEER_ID_INVALID")));
    let json = send_with(
        &app,
        "230",
        tl(GET_CONFIG),
        &[("accept", "application/json")],
    )
    .await;
    assert_eq!(
        json.body,
        Bytes::from_static(
            br#"{"_":"rpc_error","error_code":400,"error_message":"PEER_ID_INVALID"}"#
        )
    );
}

#[tokio::test]
async fn tl_json_request_and_answer() {
    let account = FakeAccount::new([ok(NEAREST)]);
    let app = app(Arc::clone(&account));
    let answer = send_with(
        &app,
        "230",
        GET_CONFIG.as_bytes().to_vec(),
        &[
            ("content-type", "application/json"),
            ("accept", "application/json"),
        ],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, Bytes::from_static(NEAREST.as_bytes()));
    assert_eq!(account.calls()[0].1, Bytes::from(tl(GET_CONFIG)));
}

#[tokio::test]
async fn affected_pts_reach_the_update_manager_in_the_request_context() {
    let affected = r#"{"_":"messages.affectedMessages","pts":10,"pts_count":2}"#;
    let account = FakeAccount::new([ok(affected), ok(affected)]);
    let sink = Arc::new(FakeSink::default());
    let app = app_with(
        Arc::clone(&account) as Arc<dyn AccountRpc>,
        Arc::clone(&sink),
        &rpc_config(100, allow_all()),
    );
    let common = send(&app, tl(r#"{"_":"messages.deleteMessages","id":[1,2]}"#)).await;
    assert_eq!(common.status, StatusCode::OK);
    let channel = send(
        &app,
        tl(r#"{"_":"channels.deleteMessages","channel":{"_":"inputChannel","channel_id":"77","access_hash":"5"},"id":[1,2]}"#),
    )
    .await;
    assert_eq!(channel.status, StatusCode::OK);
    assert_eq!(
        *sink.0.lock(),
        vec![(BoxKind::Common, 10, 2), (BoxKind::Channel(77), 10, 2)]
    );
}

#[tokio::test(start_paused = true)]
async fn max_rps_limits() {
    let account = FakeAccount::new((0..4).map(|_| ok(NEAREST)));
    let app = app_with(
        Arc::clone(&account) as Arc<dyn AccountRpc>,
        Arc::default(),
        &rpc_config(2, allow_all()),
    );
    assert_eq!(send(&app, tl(GET_CONFIG)).await.status, StatusCode::OK);
    assert_eq!(send(&app, tl(GET_CONFIG)).await.status, StatusCode::OK);
    let limited = send(&app, tl(GET_CONFIG)).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.error(), "rate_limited");
    assert_eq!(limited.retry_after(), Some("1"));
    assert_eq!(account.calls().len(), 2);
    tokio::time::advance(Duration::from_millis(500)).await;
    assert_eq!(send(&app, tl(GET_CONFIG)).await.status, StatusCode::OK);
    assert_eq!(account.calls().len(), 3);
}

#[tokio::test]
async fn body_bound_413() {
    let account = FakeAccount::new([]);
    let app = app(Arc::clone(&account));
    let answer = send(&app, vec![0; tgin_mtproto::MAX_REQUEST + 4]).await;
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(account.calls().is_empty());
}

// --- media sessions -------------------------------------------------------

/// A session with scripted answers.
#[derive(Default)]
struct FakeSession {
    script: Mutex<VecDeque<Result<RpcOutcome, SenderError>>>,
    calls: Mutex<Vec<Bytes>>,
}

impl FakeSession {
    fn new(script: impl IntoIterator<Item = Result<RpcOutcome, SenderError>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into_iter().collect()),
            calls: Mutex::default(),
        })
    }

    fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .iter()
            .map(|b| {
                let mut out = String::new();
                API.to_json(b, &mut out).expect("a known call");
                out
            })
            .collect()
    }
}

impl Session for FakeSession {
    fn invoke(&self, body: Bytes) -> BoxFuture<'_, Result<RpcOutcome, SenderError>> {
        self.calls.lock().push(body);
        let next = self
            .script
            .lock()
            .pop_front()
            .unwrap_or(Err(SenderError::NotSent));
        Box::pin(async move { next })
    }
}

struct FakeConnect {
    media: Arc<FakeSession>,
    connects: Arc<Mutex<Vec<(i32, bool)>>>,
}

impl Connect for FakeConnect {
    type Session = Arc<FakeSession>;

    fn connect(
        &self,
        dc: i32,
        key: Option<AuthKey>,
    ) -> BoxFuture<'_, Result<MediaConnected<Arc<FakeSession>>, SenderError>> {
        self.connects.lock().push((dc, key.is_some()));
        let made = key.is_none().then_some(AuthKey([7; 256]));
        let media = Arc::clone(&self.media);
        Box::pin(async move { Ok((media, made)) })
    }
}

#[derive(Default)]
struct FakeKeys(Mutex<Vec<(i32, AuthKey)>>);

impl KeyStore for FakeKeys {
    fn load(&self, dc: i32) -> Result<Option<AuthKey>, SessionError> {
        Ok(self
            .0
            .lock()
            .iter()
            .find(|(d, _)| *d == dc)
            .map(|(_, k)| k.clone()))
    }

    fn save(&self, dc: i32, key: &AuthKey) -> Result<(), SessionError> {
        self.0.lock().push((dc, key.clone()));
        Ok(())
    }
}

fn sender_ok(json: &str) -> Result<RpcOutcome, SenderError> {
    Ok(RpcOutcome::Result(Bytes::from(tl(json))))
}

#[tokio::test]
async fn file_migrate_uses_media_session_with_exported_auth() {
    let get_file = r#"{"_":"upload.getFile","location":{"_":"inputDocumentFileLocation","id":"1","access_hash":"2","file_reference":"AQ==","thumb_size":""},"offset":"0","limit":1024}"#;
    let file = r#"{"_":"upload.file","type":{"_":"storage.filePartial"},"mtime":0,"bytes":"AAEC"}"#;
    let main = FakeSession::new([
        Ok(RpcOutcome::Error {
            code: 303,
            message: "FILE_MIGRATE_4".to_owned(),
        }),
        sender_ok(r#"{"_":"auth.exportedAuthorization","id":"99","bytes":"BQYH"}"#),
        Ok(RpcOutcome::Error {
            code: 303,
            message: "FILE_MIGRATE_4".to_owned(),
        }),
    ]);
    let media = FakeSession::new([
        sender_ok(r#"{"_":"auth.authorization","user":{"_":"userEmpty","id":"1"}}"#),
        sender_ok(file),
        sender_ok(file),
    ]);
    let connects = Arc::new(Mutex::new(Vec::new()));
    let connect = FakeConnect {
        media: Arc::clone(&media),
        connects: Arc::clone(&connects),
    };
    let keys = Arc::new(FakeKeys::default());
    let sessions = Arc::new(AccountSessions::new(
        Arc::clone(&main),
        2,
        LAYER,
        connect,
        Arc::clone(&keys),
    ));
    let app = app_with(
        Arc::clone(&sessions) as Arc<dyn AccountRpc>,
        Arc::default(),
        &rpc_config(100, allow_all()),
    );

    let first = send(&app, tl(get_file)).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.body, Bytes::from(tl(file)));
    let second = send(&app, tl(get_file)).await;
    assert_eq!(second.status, StatusCode::OK);

    assert_eq!(
        main.methods(),
        vec![
            tl_json(get_file),
            r#"{"_":"auth.exportAuthorization","dc_id":4}"#.to_owned(),
            tl_json(get_file),
        ]
    );
    assert_eq!(
        media.methods(),
        vec![
            r#"{"_":"auth.importAuthorization","id":"99","bytes":"BQYH"}"#.to_owned(),
            tl_json(get_file),
            tl_json(get_file),
        ]
    );
    // One media session for DC 4, its new key stored once imported.
    assert_eq!(*connects.lock(), vec![(4, false)]);
    assert_eq!(*keys.0.lock(), vec![(4, AuthKey([7; 256]))]);
}

fn tl_json(json: &str) -> String {
    let mut out = String::new();
    API.to_json(&tl(json), &mut out).expect("round trip");
    out
}
