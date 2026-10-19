//! The raw-TL RPC endpoint (contract in DOCS.md, User accounts):
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
//! account's authorizations. The owner may lift the second refusal.
//!
//! Each client has at most [`MAX_CLIENT_IN_FLIGHT`] requests in flight
//! (else 429 `too_many_in_flight`). The account's [`AccountLimiter`]
//! (`max_rps`) counts every request tgin writes to Telegram for it: the
//! call, each resend, each `*_MIGRATE` hop and a media session's export and
//! import.
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
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures_util::StreamExt as _;
use parking_lot::Mutex;
use prometheus_client::encoding::{DescriptorEncoder, EncodeLabelSet, EncodeMetric as _};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use serde::Serialize;
use tgin_core::config::{self, MethodAcl};
use tgin_mtproto::SenderError;
use tgin_mtproto::decode::BoxKind;
use tgin_mtproto::rpc::{
    AccountRpc, AffectedSink, Answer, Charge, ErrorClass, InvokeError, MediaFailure, Refused,
    classify, rpc_error_bytes, unwrap_request,
};
use tgin_mtproto::schema::{API, MTPROTO};
use tgin_runtime::limit::{KeyedLimiter, Quota};
use tgin_runtime::secret::{Secret, secret_eq};
use tokio::sync::Semaphore;
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

/// Every account the endpoint serves, by name. A reload changes the map in
/// place: requests already running finish on the account they started on.
pub struct RpcEndpoint {
    accounts: ArcSwap<HashMap<String, Arc<RpcAccount>>>,
    /// Cancelled when tgin drains: a flood sleep then ends at once, with
    /// 429 and what is left of the wait.
    drain: CancellationToken,
    /// Calls on their way to Telegram (media setups included), and when the
    /// last one ended: the drain waits for them.
    in_flight: AtomicUsize,
    last: Mutex<Option<Instant>>,
    metrics: Arc<RpcMetrics>,
}

impl RpcEndpoint {
    #[must_use]
    pub fn new(drain: CancellationToken) -> Self {
        Self {
            accounts: ArcSwap::from_pointee(HashMap::new()),
            drain,
            in_flight: AtomicUsize::new(0),
            last: Mutex::new(None),
            metrics: Arc::new(RpcMetrics::default()),
        }
    }

    /// Serves `account` under its name, in place of the one it had; the
    /// series of a client the new one no longer has are removed.
    pub fn insert(&self, mut account: RpcAccount) {
        self.metrics.attach(&mut account);
        let account = Arc::new(account);
        let previous = self.accounts.load().get(&account.name).cloned();
        self.accounts.rcu(|accounts| {
            let mut accounts = HashMap::clone(accounts);
            accounts.insert(account.name.clone(), Arc::clone(&account));
            accounts
        });
        if let Some(previous) = previous {
            let kept = |name: &str| account.clients.iter().any(|c| c.name == name);
            self.metrics.forget(&previous, &kept, false);
        }
    }

    /// Whether the account `name` is served.
    #[cfg(test)]
    pub(crate) fn serves(&self, name: &str) -> bool {
        self.accounts.load().contains_key(name)
    }

    /// Stops serving the account `name` and removes its series.
    pub fn remove(&self, name: &str) {
        let previous = self.accounts.load().get(name).cloned();
        self.accounts.rcu(|accounts| {
            let mut accounts = HashMap::clone(accounts);
            accounts.remove(name);
            accounts
        });
        if let Some(previous) = previous {
            self.metrics.forget(&previous, &|_| false, true);
        }
    }

    /// `now` while a call is on its way to Telegram, else when the last one
    /// ended; `None` before any call.
    pub fn busy_at(&self, now: Instant) -> Option<Instant> {
        if self.in_flight.load(Ordering::SeqCst) > 0 {
            return Some(now);
        }
        *self.last.lock()
    }

    /// The endpoint's `tgin_mtproto_rpc_*` metrics.
    #[must_use]
    pub fn metrics(&self) -> Arc<RpcMetrics> {
        Arc::clone(&self.metrics)
    }
}

/// Counts one call on its way to Telegram while it lives.
struct InFlight<'a>(&'a RpcEndpoint);

impl<'a> InFlight<'a> {
    fn new(endpoint: &'a RpcEndpoint) -> Self {
        endpoint.in_flight.fetch_add(1, Ordering::SeqCst);
        Self(endpoint)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        *self.0.last.lock() = Some(Instant::now());
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How an authenticated request ended, the `outcome` of
/// `tgin_mtproto_rpc_total`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Telegram's `rpc_result`.
    Ok,
    /// Telegram's `rpc_error` (other than a flood wait).
    RpcError,
    /// `FLOOD_WAIT`, `SLOWMODE_WAIT` or an init or media-setup flood wait.
    FloodWait,
    /// Written, no answer: it may have been executed.
    NoAnswer,
    /// Not executed: not sent, a connection or media-session failure, an
    /// account that cannot call.
    NotSent,
    /// Refused by tgin: body, layer, wrapper, allowlist.
    Refused,
    /// The account's `max_rps` or the client's requests in flight.
    RateLimited,
}

impl Outcome {
    pub const ALL: [Self; 7] = [
        Self::Ok,
        Self::RpcError,
        Self::FloodWait,
        Self::NoAnswer,
        Self::NotSent,
        Self::Refused,
        Self::RateLimited,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::RpcError => "rpc_error",
            Self::FloodWait => "flood_wait",
            Self::NoAnswer => "no_answer",
            Self::NotSent => "not_sent",
            Self::Refused => "refused",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// `tgin_mtproto_rpc_total{account,client,outcome}` and
/// `tgin_mtproto_rpc_seconds{account}`. Series appear once an account is
/// served, and live as long as the process: a reload keeps them going.
#[derive(Debug)]
pub struct RpcMetrics {
    calls: Family<RpcLabels, Counter>,
    seconds: Family<AccountLabel, Histogram, fn() -> Histogram>,
    /// Whether any account was ever served: before that, no series.
    used: AtomicBool,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RpcLabels {
    account: String,
    client: String,
    outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct AccountLabel {
    account: String,
}

/// From 5 ms to about 82 s: a flood sleep or a media setup lasts up to a
/// minute.
fn rpc_histogram() -> Histogram {
    Histogram::new(exponential_buckets(0.005, 2.0, 15))
}

impl Default for RpcMetrics {
    fn default() -> Self {
        Self {
            calls: Family::default(),
            seconds: Family::new_with_constructor(rpc_histogram),
            used: AtomicBool::new(false),
        }
    }
}

impl RpcMetrics {
    /// Resolves `account`'s handles.
    fn attach(&self, account: &mut RpcAccount) {
        self.used.store(true, Ordering::Relaxed);
        account.seconds = Some(self.seconds.get_or_create_owned(&AccountLabel {
            account: account.name.clone(),
        }));
        for client in &mut account.clients {
            client.calls = Some(Outcome::ALL.map(|outcome| {
                self.calls.get_or_create_owned(&RpcLabels {
                    account: account.name.clone(),
                    client: client.name.clone(),
                    outcome: outcome.as_str(),
                })
            }));
        }
    }

    /// Removes the series of `account`'s clients that `kept` does not
    /// name, and with `whole` the account's own.
    fn forget(&self, account: &RpcAccount, kept: &dyn Fn(&str) -> bool, whole: bool) {
        for client in account.clients.iter().filter(|c| !kept(&c.name)) {
            for outcome in Outcome::ALL {
                self.calls.remove(&RpcLabels {
                    account: account.name.clone(),
                    client: client.name.clone(),
                    outcome: outcome.as_str(),
                });
            }
        }
        if whole {
            self.seconds.remove(&AccountLabel {
                account: account.name.clone(),
            });
        }
    }

    /// Writes the families into a scrape, once any account was served.
    pub fn encode(&self, encoder: &mut DescriptorEncoder<'_>) -> std::fmt::Result {
        if !self.used.load(Ordering::Relaxed) {
            return Ok(());
        }
        let calls = encoder.encode_descriptor(
            "mtproto_rpc",
            "Raw-TL RPC requests by account, client and outcome",
            None,
            self.calls.metric_type(),
        )?;
        self.calls.encode(calls)?;
        let seconds = encoder.encode_descriptor(
            "mtproto_rpc_seconds",
            "Time from a call's admission to Telegram's answer",
            None,
            self.seconds.metric_type(),
        )?;
        self.seconds.encode(seconds)
    }
}

/// How many requests one client may have in flight at once, body reading
/// and decoding included.
pub const MAX_CLIENT_IN_FLIGHT: usize = 8;

/// An account's `max_rps` limiter. It counts every request tgin writes to
/// Telegram for the account's RPC clients: the call, each resend (after a
/// connection that never sent it, or a flood sleep), each `*_MIGRATE` hop,
/// and the export and import that set up a media session. Share one
/// between [`RpcAccount::new`] and the account's
/// [`AccountSessions`](tgin_mtproto::media::AccountSessions). A reload
/// sets a new rate in place ([`AccountLimiter::set_max_rps`]).
#[derive(Debug)]
pub struct AccountLimiter {
    max_rps: AtomicU32,
    limiter: ArcSwap<KeyedLimiter<()>>,
}

impl AccountLimiter {
    /// `max_rps` per second, bursting to one second's worth.
    #[must_use]
    pub fn new(max_rps: NonZeroU32) -> Self {
        Self {
            max_rps: AtomicU32::new(max_rps.get()),
            limiter: ArcSwap::from_pointee(limiter(max_rps)),
        }
    }

    /// Applies a new `max_rps`; the bucket starts full when it changed.
    pub fn set_max_rps(&self, max_rps: NonZeroU32) {
        if self.max_rps.swap(max_rps.get(), Ordering::Relaxed) != max_rps.get() {
            self.limiter.store(Arc::new(limiter(max_rps)));
        }
    }
}

fn limiter(max_rps: NonZeroU32) -> KeyedLimiter<()> {
    KeyedLimiter::new(Quota::per_second(max_rps, max_rps))
}

impl Charge for AccountLimiter {
    fn charge(&self) -> Result<(), Duration> {
        self.limiter
            .load()
            .acquire(&(), Instant::now(), Duration::ZERO)
            .map(drop)
            .map_err(|denied| denied.retry_after)
    }
}

/// One account's RPC side: its clients, its limiter and the running
/// account.
pub struct RpcAccount {
    name: String,
    clients: Vec<Client>,
    limiter: Arc<AccountLimiter>,
    flood_sleep_threshold: Duration,
    rpc: Arc<dyn AccountRpc>,
    sink: Arc<dyn AffectedSink>,
    seconds: Option<Histogram>,
}

struct Client {
    name: String,
    token: Secret<String>,
    acl: MethodAcl,
    in_flight: Arc<Semaphore>,
    /// By [`Outcome`], once the endpoint serves the account.
    calls: Option<[Counter; Outcome::ALL.len()]>,
}

impl Client {
    fn count(&self, outcome: Outcome) {
        if let Some(calls) = &self.calls {
            calls[outcome as usize].inc();
        }
    }
}

impl RpcAccount {
    /// The RPC side of account `name`, configured by `spec`
    /// (`accounts.<name>.rpc`), calling `rpc`, feeding affected pts to
    /// `sink` and charging `limiter`.
    #[must_use]
    pub fn new(
        name: &str,
        spec: &config::Rpc,
        rpc: Arc<dyn AccountRpc>,
        sink: Arc<dyn AffectedSink>,
        limiter: Arc<AccountLimiter>,
    ) -> Self {
        Self {
            name: name.to_owned(),
            clients: spec
                .clients
                .iter()
                .map(|c| Client {
                    name: c.name.clone(),
                    token: c.token.clone(),
                    acl: c.acl.clone(),
                    in_flight: Arc::new(Semaphore::new(MAX_CLIENT_IN_FLIGHT)),
                    calls: None,
                })
                .collect(),
            limiter,
            flood_sleep_threshold: spec.flood_sleep_threshold,
            rpc,
            sink,
            seconds: None,
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
    let Some(account) = endpoint.accounts.load().get(&account).cloned() else {
        let _ = secret_eq(bearer, UNKNOWN_ACCOUNT_TOKEN);
        return unauthorized();
    };
    let Some(client) = account.client(bearer) else {
        return unauthorized();
    };
    let (response, outcome) = serve(&endpoint, &account, client, &headers, body).await;
    client.count(outcome);
    response
}

/// An authenticated client's request, and how it ended.
async fn serve(
    endpoint: &RpcEndpoint,
    account: &RpcAccount,
    client: &Client,
    headers: &HeaderMap,
    body: Body,
) -> (Response, Outcome) {
    let refused = |response| (response, Outcome::Refused);
    // Held until the answer: bounds what one client makes tgin read,
    // decode and wait on.
    let Ok(_in_flight) = Arc::clone(&client.in_flight).try_acquire_owned() else {
        let response = with_retry_after(
            tgin_error(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_in_flight",
                "the client has too many requests in flight",
            ),
            1,
        );
        return (response, Outcome::RateLimited);
    };
    // The body is read only once the client is known, within
    // BODY_TIMEOUT, and up to the bound of its content type.
    let json_body = wants_json(headers, header::CONTENT_TYPE);
    let limit = if json_body {
        MAX_JSON_BODY
    } else {
        MAX_TL_BODY
    };
    let body = match tokio::time::timeout(BODY_TIMEOUT, read_body(body, limit)).await {
        Ok(Ok(body)) => body,
        Ok(Err(BodyError::TooLarge)) => return refused(too_large()),
        Ok(Err(BodyError::Broken)) => {
            return refused(tgin_error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "the body could not be read",
            ));
        }
        Err(_) => {
            return refused(tgin_error(
                StatusCode::REQUEST_TIMEOUT,
                "body_timeout",
                "the request body did not arrive in time",
            ));
        }
    };
    let layer = account.rpc.layer();
    match headers
        .get(LAYER_HEADER)
        .and_then(|v| v.to_str().ok()?.parse::<i32>().ok())
    {
        None => {
            return refused(tgin_error(
                StatusCode::BAD_REQUEST,
                "layer_required",
                "X-Tgin-Layer must name the layer the request is serialized in",
            ));
        }
        Some(got) if got != layer => return refused(layer_mismatch(layer)),
        Some(_) => {}
    }
    let tl: Bytes = if json_body {
        let Ok(text) = std::str::from_utf8(&body) else {
            return refused(tgin_error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "TL-JSON must be UTF-8",
            ));
        };
        let mut out = Vec::new();
        if let Err(e) = API.from_json(text, &mut out) {
            return refused(tgin_error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                &e.to_string(),
            ));
        }
        Bytes::from(out)
    } else {
        body
    };
    if tl.len() > MAX_TL_BODY {
        return refused(too_large());
    }
    let call = match unwrap_request(&tl, layer) {
        Ok(call) => call,
        Err(refusal_of) => return refused(refusal(&refusal_of, layer)),
    };
    let method = call.method;
    if !client.acl.allows(method) {
        return refused(tgin_error(
            StatusCode::FORBIDDEN,
            "method_not_allowed",
            "the client's allowlist does not allow this method",
        ));
    }
    if let Err(retry_after) = account.limiter.charge() {
        return (rate_limited(retry_after), Outcome::RateLimited);
    }
    let context: BoxKind = call.context;
    let inner = tl.slice_ref(call.body);
    let started = Instant::now();
    let answer = {
        let _call = InFlight::new(endpoint);
        tgin_mtproto::rpc::call(
            account.rpc.as_ref(),
            account.sink.as_ref(),
            context,
            inner,
            account.flood_sleep_threshold,
            account.limiter.as_ref(),
            endpoint.drain.cancelled(),
        )
        .await
    };
    if let Some(seconds) = &account.seconds {
        seconds.observe(started.elapsed().as_secs_f64());
    }
    let outcome = answer_outcome(&answer);
    let response = answer_response(answer, wants_json(headers, header::ACCEPT), method);
    tracing::debug!(account = %account.name, method, status = response.status().as_u16(), "mtproto rpc");
    (response, outcome)
}

/// The `outcome` label of an answer from Telegram, or of a call that got
/// none.
fn answer_outcome(answer: &Answer) -> Outcome {
    match answer {
        Answer::Result(_) => Outcome::Ok,
        Answer::Error { class, .. } => match class {
            ErrorClass::FloodWait(_) | ErrorClass::SlowMode(_) => Outcome::FloodWait,
            _ => Outcome::RpcError,
        },
        Answer::Failed(e) => match e {
            InvokeError::RateLimited { .. } => Outcome::RateLimited,
            InvokeError::Media {
                failure: MediaFailure::FloodWait { .. },
                ..
            } => Outcome::FloodWait,
            InvokeError::Media { .. } => Outcome::NotSent,
            InvokeError::Sender(SenderError::NoAnswer) => Outcome::NoAnswer,
            InvokeError::Sender(SenderError::InitRefused { code: 420, .. }) => Outcome::FloodWait,
            InvokeError::Sender(SenderError::BadRequest) => Outcome::Refused,
            InvokeError::Sender(_) => Outcome::NotSent,
        },
    }
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

/// 429 for the account's `max_rps`: the call, or its next step, was not
/// sent.
fn rate_limited(retry_after: Duration) -> Response {
    let secs = retry_after
        .as_secs()
        .saturating_add(u64::from(retry_after.subsec_nanos() > 0));
    with_retry_after(
        tgin_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "the account's max_rps is reached; the call was not executed",
        ),
        secs,
    )
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
        InvokeError::RateLimited { retry_after } => return rate_limited(*retry_after),
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
