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
async fn authorization_methods_refused_even_when_listed() {
    let account = FakeAccount::new([]);
    let acl = MethodAcl {
        allow: vec![
            MethodGlob::Any,
            MethodGlob::Exact("auth.exportAuthorization".to_owned()),
            MethodGlob::Exact("auth.acceptLoginToken".to_owned()),
        ],
        deny: Vec::new(),
    };
    let app = app_with(
        Arc::clone(&account) as Arc<dyn AccountRpc>,
        Arc::default(),
        &rpc_config(100, acl),
    );
    for body in [
        r#"{"_":"auth.exportAuthorization","dc_id":4}"#,
        r#"{"_":"auth.acceptLoginToken","token":"AQID"}"#,
    ] {
        let answer = send(&app, tl(body)).await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN);
        assert_eq!(answer.error(), "authorization_method");
    }
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
async fn unknown_account_answers_as_a_wrong_token() {
    let app = app(FakeAccount::new([]));
    let mut answers = Vec::new();
    for (path, token) in [("main", "wrong"), ("nobody", "wrong"), ("nobody", TOKEN)] {
        let resp = app
            .clone()
            .oneshot(
                Request::post(format!("/mtproto/v1/{path}/invoke"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("x-tgin-layer", "230")
                    .body(Body::from(tl(GET_CONFIG)))
                    .expect("request"),
            )
            .await
            .expect("infallible");
        let status = resp.status();
        let body = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        answers.push((status, body));
    }
    assert_eq!(answers[0].0, StatusCode::UNAUTHORIZED);
    assert_eq!(answers[1], answers[0]);
    assert_eq!(answers[2], answers[0]);
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

const GET_NEAREST: &str = r#"{"_":"help.getNearestDc"}"#;

#[tokio::test]
async fn tl_json_request_and_answer() {
    let account = FakeAccount::new([ok(NEAREST)]);
    let app = app(Arc::clone(&account));
    let answer = send_with(
        &app,
        "230",
        GET_NEAREST.as_bytes().to_vec(),
        &[
            ("content-type", "application/json"),
            ("accept", "application/json"),
        ],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, Bytes::from_static(NEAREST.as_bytes()));
    assert_eq!(account.calls()[0].1, Bytes::from(tl(GET_NEAREST)));
}

#[tokio::test]
async fn vector_result_renders_as_array() {
    let mut users = Vec::new();
    users.extend_from_slice(&0x1cb5_c415_u32.to_le_bytes());
    users.extend_from_slice(&1_i32.to_le_bytes());
    users.extend_from_slice(&tl(r#"{"_":"userEmpty","id":"5"}"#));
    let account = FakeAccount::new([
        Ok(RpcOutcome::Result(Bytes::from(users.clone()))),
        Ok(RpcOutcome::Result(Bytes::from_static(&[1, 2, 3, 4]))),
    ]);
    let app = app(Arc::clone(&account));
    let get_users = tl(r#"{"_":"users.getUsers","id":[{"_":"inputUserSelf"}]}"#);
    let json = [("accept", "application/json")];
    let answer = send_with(&app, "230", get_users.clone(), &json).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        answer.body,
        Bytes::from_static(br#"[{"_":"userEmpty","id":"5"}]"#)
    );
    assert!(answer.headers.get("x-tgin-render").is_none());
    // Bytes that do not render: the call ran, so 200 with the raw bytes.
    let raw = send_with(&app, "230", get_users, &json).await;
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(raw.body, Bytes::from_static(&[1, 2, 3, 4]));
    assert_eq!(
        raw.headers
            .get("x-tgin-render")
            .and_then(|v| v.to_str().ok()),
        Some("raw")
    );
    assert_eq!(
        raw.headers
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/octet-stream")
    );
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

const CHANNEL_PEER: &str = r#"{"_":"inputPeerChannel","channel_id":"88","access_hash":"5"}"#;
const USER_PEER: &str = r#"{"_":"inputPeerUser","user_id":"88","access_hash":"5"}"#;

async fn affected_box(call: &str) -> Vec<(BoxKind, i32, i32)> {
    let affected = r#"{"_":"messages.affectedHistory","pts":500,"pts_count":3,"offset":0}"#;
    let account = FakeAccount::new([ok(affected)]);
    let sink = Arc::new(FakeSink::default());
    let app = app_with(
        Arc::clone(&account) as Arc<dyn AccountRpc>,
        Arc::clone(&sink),
        &rpc_config(100, allow_all()),
    );
    assert_eq!(send(&app, tl(call)).await.status, StatusCode::OK);
    std::mem::take(&mut *sink.0.lock())
}

#[tokio::test]
async fn affected_on_a_channel_peer_goes_to_the_channel_box() {
    let read_mentions = format!(r#"{{"_":"messages.readMentions","peer":{CHANNEL_PEER}}}"#);
    assert_eq!(
        affected_box(&read_mentions).await,
        vec![(BoxKind::Channel(88), 500, 3)]
    );
    let topic =
        format!(r#"{{"_":"messages.deleteTopicHistory","peer":{CHANNEL_PEER},"top_msg_id":1}}"#);
    assert_eq!(
        affected_box(&topic).await,
        vec![(BoxKind::Channel(88), 500, 3)]
    );
    let from_message = format!(
        r#"{{"_":"messages.readMentions","peer":{{"_":"inputPeerChannelFromMessage","peer":{USER_PEER},"msg_id":1,"channel_id":"99"}}}}"#
    );
    assert_eq!(
        affected_box(&from_message).await,
        vec![(BoxKind::Channel(99), 500, 3)]
    );
    // A user peer is the common box.
    let user = format!(r#"{{"_":"messages.readMentions","peer":{USER_PEER}}}"#);
    assert_eq!(affected_box(&user).await, vec![(BoxKind::Common, 500, 3)]);
    // deleteSavedHistory: `parent_peer` decides; `peer` is a saved dialog.
    let saved =
        format!(r#"{{"_":"messages.deleteSavedHistory","peer":{CHANNEL_PEER},"max_id":0}}"#);
    assert_eq!(affected_box(&saved).await, vec![(BoxKind::Common, 500, 3)]);
    let parent = format!(
        r#"{{"_":"messages.deleteSavedHistory","parent_peer":{CHANNEL_PEER},"peer":{USER_PEER},"max_id":0}}"#
    );
    assert_eq!(
        affected_box(&parent).await,
        vec![(BoxKind::Channel(88), 500, 3)]
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

/// A request whose body never arrives.
fn stalled(auth: Option<&str>) -> Request<Body> {
    let mut req = Request::post("/mtproto/v1/main/invoke")
        .header("x-tgin-layer", "230")
        .header("content-length", "4096");
    if let Some(token) = auth {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let never = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
    req.body(Body::from_stream(never)).expect("request")
}

#[tokio::test(start_paused = true)]
async fn body_read_only_after_auth_and_bounded_in_time() {
    let account = FakeAccount::new([]);
    let app = app(Arc::clone(&account));
    // Refused from the headers, without waiting for the body.
    let start = tokio::time::Instant::now();
    let guard = Duration::from_secs(3600);
    let refused = tokio::time::timeout(guard, app.clone().oneshot(stalled(None)))
        .await
        .expect("answered from the headers")
        .expect("infallible");
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(start.elapsed(), Duration::ZERO);
    // Authenticated: the body must arrive within BODY_TIMEOUT.
    let slow = tokio::time::timeout(guard, app.oneshot(stalled(Some(TOKEN))))
        .await
        .expect("bounded")
        .expect("infallible");
    assert_eq!(slow.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(start.elapsed(), tgin::mtproto_rpc::BODY_TIMEOUT);
    assert!(account.calls().is_empty());
}

#[tokio::test]
async fn body_bound_follows_content_type() {
    let account = FakeAccount::new([ok(NEAREST)]);
    let app = app(Arc::clone(&account));
    // TL-JSON may be larger than a TL request: 1.5 MiB of it is read.
    let mut json = GET_NEAREST.to_owned();
    json.push_str(&" ".repeat(1_500_000));
    let answer = send_with(
        &app,
        "230",
        json.into_bytes(),
        &[("content-type", "application/json")],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    let mut huge = GET_NEAREST.to_owned();
    huge.push_str(&" ".repeat(tgin::mtproto_rpc::MAX_JSON_BODY));
    let answer = send_with(
        &app,
        "230",
        huge.into_bytes(),
        &[("content-type", "application/json")],
    )
    .await;
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(account.calls().len(), 1);
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

    fn remove(&self, dc: i32) -> Result<(), SessionError> {
        self.0.lock().retain(|(d, _)| *d != dc);
        Ok(())
    }
}

const GET_FILE: &str = r#"{"_":"upload.getFile","location":{"_":"inputDocumentFileLocation","id":"1","access_hash":"2","file_reference":"AQ==","thumb_size":""},"offset":"0","limit":1024}"#;
const FILE: &str =
    r#"{"_":"upload.file","type":{"_":"storage.filePartial"},"mtime":0,"bytes":"AAEC"}"#;
const EXPORTED: &str = r#"{"_":"auth.exportedAuthorization","id":"99","bytes":"BQYH"}"#;
const IMPORTED: &str = r#"{"_":"auth.authorization","user":{"_":"userEmpty","id":"1"}}"#;
const EXPORT_4: &str = r#"{"_":"auth.exportAuthorization","dc_id":4}"#;
const IMPORT: &str = r#"{"_":"auth.importAuthorization","id":"99","bytes":"BQYH"}"#;

fn migrate(dc: i32) -> Result<RpcOutcome, SenderError> {
    Ok(RpcOutcome::Error {
        code: 303,
        message: format!("FILE_MIGRATE_{dc}"),
    })
}

/// A media fixture: the main and media sessions' scripts, the keys stored
/// before, and what the endpoint makes of them.
struct Media {
    main: Arc<FakeSession>,
    media: Arc<FakeSession>,
    connects: Arc<Mutex<Vec<(i32, bool)>>>,
    app: Router,
}

fn media_app<K: KeyStore + 'static>(
    main: impl IntoIterator<Item = Result<RpcOutcome, SenderError>>,
    media: impl IntoIterator<Item = Result<RpcOutcome, SenderError>>,
    home_dc: i32,
    keys: K,
) -> Media {
    let main = FakeSession::new(main);
    let media = FakeSession::new(media);
    let connects = Arc::new(Mutex::new(Vec::new()));
    let connect = FakeConnect {
        media: Arc::clone(&media),
        connects: Arc::clone(&connects),
    };
    let sessions = AccountSessions::new(Arc::clone(&main), home_dc, LAYER, connect, keys);
    let app = app_with(
        Arc::new(sessions),
        Arc::default(),
        &rpc_config(100, allow_all()),
    );
    Media {
        main,
        media,
        connects,
        app,
    }
}

#[tokio::test]
async fn stored_never_imported_key_gets_imported() {
    // A login that started on DC 2 and moved home to DC 4 leaves DC 2's
    // key in `auth_keys`: it was never imported, so it is not a media key.
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(tgin_mtproto::SessionStore::open(dir.path()).expect("store"));
    store.set_home_key(2, &AuthKey([1; 256])).expect("first DC");
    store.set_home_key(4, &AuthKey([2; 256])).expect("home");
    let export_2 = r#"{"_":"auth.exportAuthorization","dc_id":2}"#;
    let m = media_app(
        [migrate(2), sender_ok(EXPORTED)],
        [sender_ok(IMPORTED), sender_ok(FILE)],
        4,
        Arc::clone(&store),
    );
    let answer = send(&m.app, tl(GET_FILE)).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(*m.connects.lock(), vec![(2, false)]);
    assert_eq!(m.main.methods(), vec![tl_json(GET_FILE), tl_json(export_2)]);
    assert_eq!(m.media.methods(), vec![tl_json(IMPORT), tl_json(GET_FILE)]);
    assert_eq!(store.media_key(2).expect("read"), Some(AuthKey([7; 256])));
    assert_eq!(store.auth_key(2).expect("read"), Some(AuthKey([1; 256])));
}

#[tokio::test]
async fn refused_media_key_is_dropped_and_authorized_again() {
    for refusal in [
        Ok(RpcOutcome::Error {
            code: 401,
            message: "AUTH_KEY_UNREGISTERED".to_owned(),
        }),
        Ok(RpcOutcome::Error {
            code: 401,
            message: "SESSION_REVOKED".to_owned(),
        }),
        Err(SenderError::KeyUnknown),
    ] {
        let keys = Arc::new(FakeKeys::default());
        keys.save(4, &AuthKey([3; 256])).expect("stored");
        let m = media_app(
            [migrate(4), sender_ok(EXPORTED)],
            [refusal, sender_ok(IMPORTED), sender_ok(FILE)],
            2,
            Arc::clone(&keys),
        );
        let answer = send(&m.app, tl(GET_FILE)).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.body, Bytes::from(tl(FILE)));
        // The stored key first, then a new one, authorized and stored.
        assert_eq!(*m.connects.lock(), vec![(4, true), (4, false)]);
        assert_eq!(m.main.methods(), vec![tl_json(GET_FILE), tl_json(EXPORT_4)]);
        assert_eq!(
            m.media.methods(),
            vec![tl_json(GET_FILE), tl_json(IMPORT), tl_json(GET_FILE)]
        );
        assert_eq!(*keys.0.lock(), vec![(4, AuthKey([7; 256]))]);
    }
}

fn rpc_err(code: i32, message: &str) -> Result<RpcOutcome, SenderError> {
    Ok(RpcOutcome::Error {
        code,
        message: message.to_owned(),
    })
}

#[tokio::test]
async fn media_export_refused_is_503_not_executed() {
    let m = media_app(
        [migrate(4), rpc_err(400, "DC_ID_INVALID")],
        [],
        2,
        Arc::new(FakeKeys::default()),
    );
    let answer = send(&m.app, tl(GET_FILE)).await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.error(), "media_session");
    assert_eq!(answer.retry_after(), Some("1"));
    assert!(m.media.methods().is_empty());
}

#[tokio::test]
async fn media_import_refused_is_503_and_retry_repeats_only_export_import() {
    let keys = Arc::new(FakeKeys::default());
    let m = media_app(
        [
            migrate(4),
            sender_ok(EXPORTED),
            migrate(4),
            sender_ok(EXPORTED),
        ],
        [
            rpc_err(400, "AUTH_BYTES_INVALID"),
            sender_ok(IMPORTED),
            sender_ok(FILE),
        ],
        2,
        Arc::clone(&keys),
    );
    let first = send(&m.app, tl(GET_FILE)).await;
    assert_eq!(first.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(first.error(), "media_session");
    assert!(keys.0.lock().is_empty());
    let second = send(&m.app, tl(GET_FILE)).await;
    assert_eq!(second.status, StatusCode::OK);
    // One key exchange for both attempts.
    assert_eq!(*m.connects.lock(), vec![(4, false)]);
    assert_eq!(
        m.media.methods(),
        vec![tl_json(IMPORT), tl_json(IMPORT), tl_json(GET_FILE)]
    );
    assert_eq!(*keys.0.lock(), vec![(4, AuthKey([7; 256]))]);
}

#[tokio::test]
async fn media_no_answer_during_setup_is_not_executed() {
    for (main, media) in [
        (vec![migrate(4), Err(SenderError::NoAnswer)], vec![]),
        (
            vec![migrate(4), sender_ok(EXPORTED)],
            vec![Err(SenderError::NoAnswer)],
        ),
    ] {
        let m = media_app(main, media, 2, Arc::new(FakeKeys::default()));
        let answer = send(&m.app, tl(GET_FILE)).await;
        assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(answer.error(), "media_session");
    }
}

#[tokio::test]
async fn media_flood_wait_during_setup_is_429() {
    for (main, media) in [
        (vec![migrate(4), rpc_err(420, "FLOOD_WAIT_30")], vec![]),
        (
            vec![migrate(4), sender_ok(EXPORTED)],
            vec![rpc_err(420, "FLOOD_WAIT_30")],
        ),
    ] {
        let m = media_app(main, media, 2, Arc::new(FakeKeys::default()));
        let answer = send(&m.app, tl(GET_FILE)).await;
        assert_eq!(answer.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(answer.error(), "media_flood_wait");
        assert_eq!(answer.retry_after(), Some("30"));
    }
}

#[tokio::test]
async fn media_key_gone_is_a_media_failure_not_a_login() {
    let keys = Arc::new(FakeKeys::default());
    keys.save(4, &AuthKey([3; 256])).expect("stored");
    let m = media_app(
        [migrate(4), sender_ok(EXPORTED)],
        [
            Err(SenderError::KeyRevoked),
            sender_ok(IMPORTED),
            Err(SenderError::KeyRevoked),
        ],
        2,
        Arc::clone(&keys),
    );
    let answer = send(&m.app, tl(GET_FILE)).await;
    assert_ne!(answer.error(), "account_unavailable");
    assert!(answer.error().starts_with("media"), "{}", answer.error());
    // Sent twice at most: once per key.
    assert_eq!(m.media.methods().len(), 3);
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
