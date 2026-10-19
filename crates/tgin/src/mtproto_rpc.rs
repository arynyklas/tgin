//! The raw-TL RPC endpoint (design §10.6, plan decisions 3 and 4):
//! `POST /mtproto/v1/{account}/invoke`.
//!
//! A request carries `Authorization: Bearer <client token>` (compared in
//! constant time), `X-Tgin-Layer: <layer>` (the layer the body is
//! serialized in; it must be the account's) and the call:
//! `application/octet-stream` TL, or with `Content-Type: application/json`
//! TL-JSON that the codec encodes. `Accept: application/json` renders the
//! answer as TL-JSON, by the called function's result type (a `Vector<T>`
//! is an array). An answer that cannot be rendered is sent raw, with
//! `X-Tgin-Render: raw`: the call has run, so it is never an error.
//!
//! tgin refuses with 403, whatever a client's allowlist says, the update
//! methods (`updates.getState`, `getDifference`, `getChannelDifference`)
//! and the `auth.*` methods that mint or move an authorization of the
//! account ([`tgin_mtproto::rpc::AUTHORIZATION_METHODS`]): tgin owns the
//! account's authorizations (plan decision 2). The second refusal is
//! ruling P10; the owner may overrule it.
//!
//! The pipeline: unwrap the call ([`tgin_mtproto::rpc::unwrap_request`]),
//! the client's allowlist (deny wins; empty denies), the account's
//! `max_rps` limiter, then [`tgin_mtproto::rpc::call`] on the account.
//!
//! Answers:
//! - Telegram's `rpc_result`: 200, its bytes as Telegram sent them.
//! - Telegram's `rpc_error`: 200 with the `rpc_error` object, except
//!   `FLOOD_WAIT_X` (after one sleep when X ≤ `flood_sleep_threshold`) and
//!   `SLOWMODE_WAIT_X`: 429 with `Retry-After: X`; `FROZEN_*`: 403. The
//!   body is the `rpc_error` in every case.
//! - tgin's own errors: JSON `{"error","description"}`. 502 `no_answer`:
//!   the call was written and may have been executed; tgin never resends
//!   it. 503 with `Retry-After`: not executed, safe to retry; a media
//!   session that could not be set up is one (`media_session`), or 429
//!   `media_flood_wait` with `Retry-After` when Telegram asked to wait.
//!
//! **Order.** The account and the bearer token are checked from the
//! headers before the body is read; the body must then arrive within
//! [`BODY_TIMEOUT`] (else 408).
//!
//! **Bounds.** A TL request is at most [`MAX_TL_BODY`] (the sender's
//! bound, just under 1 MiB); a TL-JSON request body at most
//! [`MAX_JSON_BODY`]. Upload files with `upload.saveFilePart` /
//! `upload.saveBigFilePart` in parts of at most 512 KiB.
//!
//! Request and answer bodies are never logged.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures_util::StreamExt as _;
use serde::Serialize;
use tgin_core::config::{self, MethodAcl};
use tgin_mtproto::SenderError;
use tgin_mtproto::decode::BoxKind;
use tgin_mtproto::rpc::{
    AccountRpc, AffectedSink, Answer, ErrorClass, InvokeError, MediaFailure, Refused, classify,
    rpc_error_bytes, unwrap_request,
};
use tgin_mtproto::schema::{API, MTPROTO};
use tgin_runtime::limit::{KeyedLimiter, Quota};
use tgin_runtime::secret::{Secret, secret_eq};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// The largest TL request: the sender's bound, 1 044 428 bytes (just under
/// 1 MiB).
pub const MAX_TL_BODY: usize = tgin_mtproto::MAX_REQUEST;
/// The largest TL-JSON request body: 2 MiB (base64 makes a 512 KiB file
/// part about 700 KiB).
pub const MAX_JSON_BODY: usize = 2 << 20;

/// How long a request body may take to arrive once the client is
/// authenticated, so a client that trickles its body cannot hold a
/// connection (the webhook routes' bound).
pub const BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// The header naming the request's layer.
pub const LAYER_HEADER: &str = "x-tgin-layer";
/// Set to `raw` on an answer the client asked as TL-JSON that could not be
/// rendered: the body is then Telegram's raw bytes.
pub const RENDER_HEADER: &str = "x-tgin-render";

/// Every account the endpoint serves, by name.
pub struct RpcEndpoint {
    accounts: HashMap<String, Arc<RpcAccount>>,
    /// Cancelled when tgin drains: a flood sleep then ends at once, with
    /// 429 and what is left of the wait.
    drain: CancellationToken,
}

impl RpcEndpoint {
    #[must_use]
    pub fn new(drain: CancellationToken) -> Self {
        Self {
            accounts: HashMap::new(),
            drain,
        }
    }

    /// Serves `account` under its name.
    pub fn insert(&mut self, account: RpcAccount) {
        self.accounts
            .insert(account.name.clone(), Arc::new(account));
    }
}

/// One account's RPC side: its clients, its limiter and the running
/// account.
pub struct RpcAccount {
    name: String,
    clients: Vec<Client>,
    limiter: KeyedLimiter<()>,
    flood_sleep_threshold: Duration,
    rpc: Arc<dyn AccountRpc>,
    sink: Arc<dyn AffectedSink>,
}

struct Client {
    token: Secret<String>,
    acl: MethodAcl,
}

impl RpcAccount {
    /// The RPC side of account `name`, configured by `spec`
    /// (`accounts.<name>.rpc`), calling `rpc` and feeding affected pts to
    /// `sink`.
    #[must_use]
    pub fn new(
        name: &str,
        spec: &config::Rpc,
        rpc: Arc<dyn AccountRpc>,
        sink: Arc<dyn AffectedSink>,
    ) -> Self {
        let max_rps = spec.max_rps;
        // `max_rps` per second, bursting to one second's worth.
        let quota = Quota::per_second(max_rps, max_rps);
        Self {
            name: name.to_owned(),
            clients: spec
                .clients
                .iter()
                .map(|c| Client {
                    token: c.token.clone(),
                    acl: c.acl.clone(),
                })
                .collect(),
            limiter: KeyedLimiter::new(quota),
            flood_sleep_threshold: spec.flood_sleep_threshold,
            rpc,
            sink,
        }
    }

    /// The client whose token `bearer` is. Every token is compared, in
    /// constant time, whichever matches.
    fn client(&self, bearer: &[u8]) -> Option<&Client> {
        let mut found = None;
        for client in &self.clients {
            if secret_eq(bearer, client.token.expose().as_bytes()) && found.is_none() {
                found = Some(client);
            }
        }
        found
    }
}

/// The endpoint's routes. The handler reads the body itself, after
/// authentication, so axum's default body limit is off.
pub fn router(endpoint: Arc<RpcEndpoint>) -> Router {
    Router::new()
        .route("/mtproto/v1/{account}/invoke", post(invoke))
        .layer(DefaultBodyLimit::disable())
        .with_state(endpoint)
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    description: &'a str,
}

fn tgin_error(status: StatusCode, error: &str, description: &str) -> Response {
    (status, axum::Json(ErrorBody { error, description })).into_response()
}

fn with_retry_after(mut response: Response, secs: u64) -> Response {
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
    response
}

fn wants_json(headers: &HeaderMap, name: header::HeaderName) -> bool {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"))
}

/// Why a request body could not be read.
enum BodyError {
    TooLarge,
    Broken,
}

/// Reads `body`, refusing it once it passes `limit` bytes. A body of one
/// chunk is not copied.
async fn read_body(body: Body, limit: usize) -> Result<Bytes, BodyError> {
    let mut stream = body.into_data_stream();
    let mut first: Option<Bytes> = None;
    let mut rest: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| BodyError::Broken)?;
        let held = first.as_ref().map_or(0, Bytes::len) + rest.len();
        if held.saturating_add(chunk.len()) > limit {
            return Err(BodyError::TooLarge);
        }
        match first.take() {
            None if rest.is_empty() => first = Some(chunk),
            None => rest.extend_from_slice(&chunk),
            Some(head) => {
                rest.extend_from_slice(&head);
                rest.extend_from_slice(&chunk);
            }
        }
    }
    Ok(first.unwrap_or_else(|| Bytes::from(rest)))
}

async fn invoke(
    State(endpoint): State<Arc<RpcEndpoint>>,
    Path(account): Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "))
        .unwrap_or_default();
    // An unknown account is answered as a wrong token, after a comparison
    // of the same kind, so the answer does not tell which accounts exist.
    let Some(account) = endpoint.accounts.get(&account) else {
        let _ = secret_eq(bearer, UNKNOWN_ACCOUNT_TOKEN);
        return unauthorized();
    };
    let Some(client) = account.client(bearer) else {
        return unauthorized();
    };
    // The body is read only once the client is known, within
    // BODY_TIMEOUT, and up to the bound of its content type.
    let json_body = wants_json(&headers, header::CONTENT_TYPE);
    let limit = if json_body {
        MAX_JSON_BODY
    } else {
        MAX_TL_BODY
    };
    let body = match tokio::time::timeout(BODY_TIMEOUT, read_body(body, limit)).await {
        Ok(Ok(body)) => body,
        Ok(Err(BodyError::TooLarge)) => return too_large(),
        Ok(Err(BodyError::Broken)) => {
            return tgin_error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "the body could not be read",
            );
        }
        Err(_) => {
            return tgin_error(
                StatusCode::REQUEST_TIMEOUT,
                "body_timeout",
                "the request body did not arrive in time",
            );
        }
    };
    let layer = account.rpc.layer();
    match headers
        .get(LAYER_HEADER)
        .and_then(|v| v.to_str().ok()?.parse::<i32>().ok())
    {
        None => {
            return tgin_error(
                StatusCode::BAD_REQUEST,
                "layer_required",
                "X-Tgin-Layer must name the layer the request is serialized in",
            );
        }
        Some(got) if got != layer => return layer_mismatch(layer),
        Some(_) => {}
    }
    let tl: Bytes = if json_body {
        let Ok(text) = std::str::from_utf8(&body) else {
            return tgin_error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "TL-JSON must be UTF-8",
            );
        };
        let mut out = Vec::new();
        if let Err(e) = API.from_json(text, &mut out) {
            return tgin_error(StatusCode::BAD_REQUEST, "bad_request", &e.to_string());
        }
        Bytes::from(out)
    } else {
        body
    };
    if tl.len() > MAX_TL_BODY {
        return too_large();
    }
    let call = match unwrap_request(&tl, layer) {
        Ok(call) => call,
        Err(refused) => return refusal(&refused, layer),
    };
    let method = call.method;
    if !client.acl.allows(method) {
        return tgin_error(
            StatusCode::FORBIDDEN,
            "method_not_allowed",
            "the client's allowlist does not allow this method",
        );
    }
    if let Err(denied) = account.limiter.acquire(&(), Instant::now(), Duration::ZERO) {
        let secs = denied
            .retry_after
            .as_secs()
            .saturating_add(u64::from(denied.retry_after.subsec_nanos() > 0));
        return with_retry_after(
            tgin_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "the account's max_rps is reached",
            ),
            secs,
        );
    }
    let context: BoxKind = call.context;
    let inner = tl.slice_ref(call.body);
    let answer = tgin_mtproto::rpc::call(
        account.rpc.as_ref(),
        account.sink.as_ref(),
        context,
        inner,
        account.flood_sleep_threshold,
        endpoint.drain.cancelled(),
    )
    .await;
    let response = answer_response(answer, wants_json(&headers, header::ACCEPT), method);
    tracing::debug!(account = %account.name, method, status = response.status().as_u16(), "mtproto rpc");
    response
}

/// What an unknown account's bearer is compared with.
const UNKNOWN_ACCOUNT_TOKEN: &[u8] = b"tgin-unknown-account-0000000000000000";

fn unauthorized() -> Response {
    tgin_error(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "a valid client bearer token is required",
    )
}

fn too_large() -> Response {
    tgin_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
        "a TL request is at most 1 044 428 bytes; upload files in parts with upload.saveFilePart",
    )
}

fn layer_mismatch(layer: i32) -> Response {
    tgin_error(
        StatusCode::CONFLICT,
        "layer_mismatch",
        &format!("the account serves layer {layer} only"),
    )
}

fn refusal(refused: &Refused, layer: i32) -> Response {
    match refused {
        Refused::ForeignLayer { .. } => layer_mismatch(layer),
        Refused::AuthorizationMethod(_) => tgin_error(
            StatusCode::FORBIDDEN,
            "authorization_method",
            &refused.to_string(),
        ),
        Refused::UpdatesMethod(_) => tgin_error(
            StatusCode::FORBIDDEN,
            "updates_method",
            &refused.to_string(),
        ),
        Refused::Wrapper(_) => tgin_error(
            StatusCode::BAD_REQUEST,
            "wrapper_refused",
            &refused.to_string(),
        ),
        Refused::Malformed(_) | Refused::NotAFunction(_) => {
            tgin_error(StatusCode::BAD_REQUEST, "bad_request", &refused.to_string())
        }
    }
}

/// What Telegram's bytes are: the result of a call to a function, or an
/// `rpc_error`.
#[derive(Clone, Copy)]
enum Payload {
    Result(&'static str),
    RpcError,
}

/// Telegram's bytes, raw or, when the client asked, as TL-JSON. The call
/// has run, so a rendering failure never turns into an error: the answer
/// is then the raw bytes, with `X-Tgin-Render: raw`.
fn telegram_bytes(status: StatusCode, bytes: Bytes, json: bool, what: Payload) -> Response {
    if json {
        let mut out = String::new();
        let rendered = match what {
            Payload::Result(method) => API.result_to_json(method, &bytes, &mut out),
            Payload::RpcError => MTPROTO.to_json(&bytes, &mut out),
        };
        if rendered.is_ok() {
            return (status, [(header::CONTENT_TYPE, "application/json")], out).into_response();
        }
    }
    let mut response = (
        status,
        [(header::CONTENT_TYPE, "application/octet-stream")],
        bytes,
    )
        .into_response();
    if json {
        response
            .headers_mut()
            .insert(RENDER_HEADER, HeaderValue::from_static("raw"));
    }
    response
}

fn answer_response(answer: Answer, json: bool, method: &'static str) -> Response {
    match answer {
        Answer::Result(bytes) => {
            telegram_bytes(StatusCode::OK, bytes, json, Payload::Result(method))
        }
        Answer::Error {
            code,
            message,
            class,
        } => {
            let bytes = Bytes::from(rpc_error_bytes(code, &message));
            let status = match class {
                ErrorClass::FloodWait(_) | ErrorClass::SlowMode(_) => StatusCode::TOO_MANY_REQUESTS,
                ErrorClass::Frozen => StatusCode::FORBIDDEN,
                ErrorClass::Migrate(_) | ErrorClass::Other => StatusCode::OK,
            };
            let response = telegram_bytes(status, bytes, json, Payload::RpcError);
            match class {
                ErrorClass::FloodWait(x) | ErrorClass::SlowMode(x) => {
                    with_retry_after(response, u64::from(x))
                }
                _ => response,
            }
        }
        Answer::Failed(e) => failure(&e),
    }
}

fn unavailable(error: &str, description: &str) -> Response {
    with_retry_after(
        tgin_error(StatusCode::SERVICE_UNAVAILABLE, error, description),
        1,
    )
}

fn failure(e: &InvokeError) -> Response {
    let e = match e {
        InvokeError::Sender(e) => e,
        InvokeError::Media {
            failure: MediaFailure::FloodWait { seconds, .. },
            ..
        } => {
            return with_retry_after(
                tgin_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "media_flood_wait",
                    &format!("{e}; the call was not executed"),
                ),
                u64::from(*seconds),
            );
        }
        InvokeError::Media { .. } => {
            return unavailable("media_session", &format!("{e}; the call was not executed"));
        }
    };
    match e {
        SenderError::NoAnswer => tgin_error(
            StatusCode::BAD_GATEWAY,
            "no_answer",
            "the call may have been executed",
        ),
        SenderError::NotSent => unavailable("not_sent", "the call was not executed; retry it"),
        SenderError::BadMessage { .. } => {
            unavailable("not_sent", "Telegram ignored the call; it was not executed")
        }
        SenderError::Connect(_) => unavailable(
            "not_sent",
            "Telegram could not be reached; the call was not executed",
        ),
        SenderError::InitRefused { code, message } => match classify(*code, message) {
            ErrorClass::FloodWait(x) if *code == 420 => with_retry_after(
                tgin_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "init_flood_wait",
                    "Telegram refused the connection's init call for now; the call was not executed",
                ),
                u64::from(x),
            ),
            _ if *code == 420 => with_retry_after(
                tgin_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "init_flood_wait",
                    "Telegram refused the connection's init call for now; the call was not executed",
                ),
                1,
            ),
            _ => tgin_error(
                StatusCode::BAD_GATEWAY,
                "init_refused",
                "Telegram refused the connection's init call; the call was not executed",
            ),
        },
        SenderError::KeyRevoked | SenderError::KeyUnknown => tgin_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "account_unavailable",
            "the account needs a new login",
        ),
        SenderError::BadRequest => tgin_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "the request is not whole TL words or is too large",
        ),
    }
}
