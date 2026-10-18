//! The streaming proxy: a gateway request goes to `{api_url}/bot{token}/…`
//! (or to the bot's file base, see `files`) with an allowlist of headers,
//! both bodies stream without buffering and keep their `Content-Length`,
//! and `upstream_idle_timeout` bounds the time without progress in either
//! direction.
//!
//! reqwest's own `read_timeout` cannot be that bound: it runs one fixed
//! timer from sending until the response headers (reqwest 0.13.5,
//! `async_impl/client.rs` `PendingRequest`), so an upload taking longer
//! would be cut while its bytes still flow. The client sets no timeout but
//! the connect one, and this module measures idleness itself.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::header::{
    ACCEPT, ACCEPT_ENCODING, ACCEPT_RANGES, CACHE_CONTROL, CONNECTION, CONTENT_DISPOSITION,
    CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderName,
    IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, RANGE, RETRY_AFTER, USER_AGENT, VIA,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use http_body::{Body as HttpBody, Frame, SizeHint};
use parking_lot::Mutex;
use reqwest::Url;
use serde::Deserialize;
use tgin_runtime::http::describe_error;
use tokio::sync::oneshot;
use tokio::time::{Instant, Sleep, sleep_until};

use super::chat_key::{ChatKey, chat_key};
use super::limit::Limited;
use super::local_files::{
    MAX_PARAMETERS, MultipartGuard, Refusal, Verdict, boundary, form_has_local_file,
    json_has_local_file,
};
use super::metrics::{BotMetrics, MethodLabel, MethodMetrics, Outcome, RateAction};
use super::params::{Encoding, encoding};
use super::{Bot, Bounded, Forwarding, errors, read_bounded};

/// The longest request body of a limited method read for its chat and
/// kept to be sent again after a 429.
const BUFFER_LIMIT: usize = 64 * 1024;
/// The longest upstream 429 body read for its `retry_after`.
const FLOOD_LIMIT: usize = 64 * 1024;

/// Request headers forwarded on every route. Never `Content-Encoding`:
/// telegram-bot-api inflates gzip and deflate bodies, and the local file
/// scan (`local_files`) reads a body as it is sent.
const REQUEST_HEADERS: [HeaderName; 5] = [
    CONTENT_TYPE,
    CONTENT_LENGTH,
    ACCEPT,
    ACCEPT_ENCODING,
    USER_AGENT,
];
/// Request headers forwarded on file routes as well.
const FILE_REQUEST_HEADERS: [HeaderName; 3] = [RANGE, IF_NONE_MATCH, IF_MODIFIED_SINCE];
/// Response headers forwarded.
const RESPONSE_HEADERS: [HeaderName; 10] = [
    CONTENT_TYPE,
    CONTENT_LENGTH,
    CONTENT_ENCODING,
    CONTENT_DISPOSITION,
    RETRY_AFTER,
    CACHE_CONTROL,
    ETAG,
    LAST_MODIFIED,
    ACCEPT_RANGES,
    CONTENT_RANGE,
];

/// What a gateway path asks of the upstream.
pub(super) enum Upstream<'a> {
    /// `<method>`: `{api_url}/bot{token}/<method>`.
    Method(&'a str),
    /// `<file_path>`, still percent-encoded, below the bot's file `base`:
    /// `{api_url}/file/bot{token}/` or `{files.url}/{token}/`.
    File { base: &'a Url, path: &'a str },
}

/// Streams `req` to Telegram for `bot` and Telegram's answer back.
///
/// A method of `rate_limit.methods` (whatever the mode) has its small body
/// buffered when the limits apply to it or `retry_429.max_wait` is above
/// zero. Where the limits apply, the request waits for, or is refused by,
/// them first. A buffered body goes once more after Telegram's 429.
///
/// A method call of a bot whose server must not get a local file reference
/// (`local_files`) has its parameters scanned first: the query string, a
/// JSON or urlencoded body read whole, a multipart body as it streams.
pub(super) async fn forward(
    bot: &Bot,
    upstream: Upstream<'_>,
    label: MethodLabel,
    req: Request,
    forwarding: Forwarding,
) -> Response {
    let (parts, body) = req.into_parts();
    let (url, file, method) = match upstream {
        Upstream::Method(method) => (
            url(&bot.method_base, method, parts.uri.query()),
            false,
            Some(method),
        ),
        Upstream::File { base, path } => (url(base, path, parts.uri.query()), true, None),
    };
    let limited = method.is_some_and(|method| bot.limiter.applies(method));
    let buffered = limited
        || (!bot.gateway.retry_429_max_wait.is_zero()
            && method.is_some_and(|method| bot.limiter.covers(method)));
    let mut headers = HeaderMap::new();
    copy_headers(&parts.headers, &REQUEST_HEADERS, &mut headers);
    if file {
        copy_headers(&parts.headers, &FILE_REQUEST_HEADERS, &mut headers);
    }
    // The intermediaries the request passed, then this process: a request
    // that comes back to any of them, through an `api_url` that points at
    // tgin, is refused with `508` instead of forwarded again.
    for via in parts.headers.get_all(VIA) {
        headers.append(VIA, via.clone());
    }
    headers.append(VIA, bot.via.clone());
    let call = Call {
        bot,
        label,
        handles: bot.metrics.method(label),
        method: parts.method,
        limited,
        counted: AtomicBool::new(false),
        forwarding: Mutex::new(Some(forwarding)),
    };
    let scanned = if method.is_some() && bot.refuses_local_files {
        match scanned_encoding(&url, &headers) {
            Ok(scanned) => scanned,
            Err(refusal) => return call.failed(Failed::Refused(refusal)),
        }
    } else {
        None
    };
    if !buffered && scanned.is_none() {
        return match call.send(url, headers, body).await {
            Ok((response, _)) => call.relay(response),
            Err(failed) => call.failed(failed),
        };
    }

    let idle = bot.gateway.upstream_idle_timeout;
    let query = parts.uri.query();
    let prepared = match scanned {
        Some(scanned) => {
            let content_type = headers.get(CONTENT_TYPE);
            prepare_scanned(scanned, content_type, query, body, idle, limited).await
        }
        None => prepare(&parts.headers, query, body, idle, limited)
            .await
            .map(|(payload, chat)| (payload, chat, None)),
    };
    let (payload, chat, verdict) = match prepared {
        Ok(prepared) => prepared,
        Err(failed) => return call.failed(failed),
    };
    if let Err(limited) = call.admit(chat.as_ref()).await {
        call.refuse(limited);
        return errors::too_many_requests(limited.retry_after_secs());
    }
    match payload {
        Payload::Streamed(body) => match call.send(url, headers, body).await {
            Ok((response, _)) => call.relay(response),
            Err(failed) => call.failed(refused_by(failed, verdict)),
        },
        Payload::Buffered(request) => {
            let first = call
                .send(url.clone(), headers.clone(), Body::from(request.clone()))
                .await;
            let (response, at) = match first {
                Ok(first) => first,
                Err(failed) => return call.failed(failed),
            };
            if response.status() != StatusCode::TOO_MANY_REQUESTS
                || bot.gateway.retry_429_max_wait.is_zero()
                || !buffered
            {
                return call.relay(response);
            }
            let again = Again {
                url,
                headers,
                body: request,
                chat,
            };
            call.flood(response, at, again).await
        }
    }
}

/// A request body as it goes upstream.
enum Payload {
    /// Read whole: it can be sent again.
    Buffered(Bytes),
    /// Streamed: it goes once.
    Streamed(Body),
}

/// Why a request got no response from Telegram.
#[derive(Debug)]
enum Failed {
    /// Reading the client's body failed: the client went away.
    ClientClosed,
    /// Telegram could not be reached (logged where it happened).
    Unreachable,
    /// Nothing moved for `upstream_idle_timeout`, in either direction.
    Idle,
    /// Its parameters may not reach the bot's server in local mode.
    Refused(Refusal),
}

/// The body of a request to buffer, and, when the limits apply (`limited`),
/// the chat it targets:
/// - a JSON or urlencoded body (or an empty one) is read up to
///   [`BUFFER_LIMIT`], waiting at most `idle` for each frame; whole, it is
///   buffered;
/// - longer, what was read and the rest stream, as does any other body,
///   multipart included, unread.
///
/// The chat is the query string's `chat_id`, else the buffered body's, as
/// Telegram reads it (`HttpReader` parses the URL before the body, and
/// `Query::arg` takes the first match). A streamed body's `chat_id` cannot
/// be read, so the query string's is the only chat tgin can know there;
/// without one only the global quota applies.
async fn prepare(
    headers: &HeaderMap,
    query: Option<&str>,
    body: Body,
    idle: Duration,
    limited: bool,
) -> Result<(Payload, Option<ChatKey>), Failed> {
    let content_type = headers.get(CONTENT_TYPE);
    let readable = matches!(
        content_type.and_then(encoding),
        Some(Encoding::Json | Encoding::Form)
    ) || body.size_hint().exact() == Some(0);
    if !readable {
        return Ok((
            Payload::Streamed(body),
            limited.then(|| query_chat(query)).flatten(),
        ));
    }
    Ok(match read_within(body, BUFFER_LIMIT, idle).await? {
        Bounded::Complete(bytes) => {
            let chat = limited
                .then(|| query_chat(query).or_else(|| chat_key(content_type, &bytes)))
                .flatten();
            (Payload::Buffered(bytes), chat)
        }
        Bounded::Over(partial) => (
            Payload::Streamed(partial.into_body()),
            limited.then(|| query_chat(query)).flatten(),
        ),
    })
}

/// The query string's chat.
fn query_chat(query: Option<&str>) -> Option<ChatKey> {
    const FORM: HeaderValue = HeaderValue::from_static("application/x-www-form-urlencoded");
    query.and_then(|query| chat_key(Some(&FORM), query.as_bytes()))
}

/// `body` read up to `limit` bytes, waiting at most `idle` for each frame.
async fn read_within(body: Body, limit: usize, idle: Duration) -> Result<Bounded, Failed> {
    let progress = Arc::new(Progress::new());
    let stamped = Body::new(Upload {
        body: Mutex::new(body),
        progress: Arc::clone(&progress),
    });
    match await_headers(read_bounded(stamped, limit), &progress, idle).await {
        Waited::Ready(Ok(read)) => Ok(read),
        Waited::Ready(Err(_)) => Err(Failed::ClientClosed),
        Waited::Idle => Err(Failed::Idle),
    }
}

/// For a bot whose server must not get a local file reference: refuses a
/// query string the server would read one from, and a request with more
/// than one `Content-Type` (which one the server reads is unclear);
/// otherwise the encoding of a body whose parameters are to be scanned.
fn scanned_encoding(url: &Url, headers: &HeaderMap) -> Result<Option<Encoding>, Refusal> {
    if url
        .query()
        .is_some_and(|query| form_has_local_file(query.as_bytes()))
    {
        return Err(Refusal::LocalFile);
    }
    let mut content_types = headers.get_all(CONTENT_TYPE).iter();
    let content_type = content_types.next();
    if content_types.next().is_some() {
        return Err(Refusal::Unreadable);
    }
    Ok(content_type.and_then(encoding))
}

/// [`prepare`] for a body whose parameters are scanned (ruling R2):
/// - JSON or urlencoded, it is read whole up to [`MAX_PARAMETERS`] (longer,
///   refused as too large), refused when it carries a local file
///   reference, else buffered, its chat read from it;
/// - multipart, it streams through a [`MultipartGuard`], whose verdict
///   comes back.
async fn prepare_scanned(
    scanned: Encoding,
    content_type: Option<&HeaderValue>,
    query: Option<&str>,
    body: Body,
    idle: Duration,
    limited: bool,
) -> Result<(Payload, Option<ChatKey>, Option<oneshot::Receiver<Verdict>>), Failed> {
    if scanned == Encoding::Multipart {
        let Some(boundary) =
            content_type.and_then(|content_type| boundary(content_type.as_bytes()))
        else {
            return Err(Failed::Refused(Refusal::Unreadable));
        };
        let (guard, verdict) = MultipartGuard::new(body, boundary);
        let chat = limited.then(|| query_chat(query)).flatten();
        return Ok((Payload::Streamed(Body::new(guard)), chat, Some(verdict)));
    }
    let Bounded::Complete(bytes) = read_within(body, MAX_PARAMETERS, idle).await? else {
        return Err(Failed::Refused(Refusal::TooLarge));
    };
    let found = match scanned {
        Encoding::Json => json_has_local_file(&bytes),
        Encoding::Form | Encoding::Multipart => form_has_local_file(&bytes),
    };
    if found {
        return Err(Failed::Refused(Refusal::LocalFile));
    }
    let chat = limited
        .then(|| query_chat(query).or_else(|| chat_key(content_type, &bytes)))
        .flatten();
    Ok((Payload::Buffered(bytes), chat, None))
}

/// `failed`, or the refusal that made it fail: a [`MultipartGuard`] fails
/// the upload it refuses.
fn refused_by(failed: Failed, verdict: Option<oneshot::Receiver<Verdict>>) -> Failed {
    match verdict.and_then(|mut verdict| verdict.try_recv().ok()) {
        Some(Verdict::Refused(refusal)) => Failed::Refused(refusal),
        _ => failed,
    }
}

/// What a buffered request needs to go upstream once more.
struct Again {
    url: Url,
    headers: HeaderMap,
    body: Bytes,
    chat: Option<ChatKey>,
}

/// One gateway request to Telegram: what each of its upstream attempts
/// shares. Dropped before its outcome was counted, it counts
/// `client_closed`: hyper drops a request's future when its client goes
/// away, while it waits for Telegram's headers, a shaping delay or a 429.
struct Call<'a> {
    bot: &'a Bot,
    label: MethodLabel,
    handles: &'a MethodMetrics,
    method: Method,
    /// Whether the bot's limits apply to the method; without them `admit`
    /// books nothing.
    limited: bool,
    /// Whether the request's outcome is counted, or left to its response
    /// body's [`Tally`].
    counted: AtomicBool,
    /// The request in flight for the shutdown's drain: handed to the
    /// relayed body, else dropped with this call.
    forwarding: Mutex<Option<Forwarding>>,
}

impl Drop for Call<'_> {
    fn drop(&mut self) {
        if !*self.counted.get_mut() {
            tracing::debug!(bot = %self.bot.name, method = self.label.as_str(), "gateway request abandoned: the client went away");
            self.handles.count(Outcome::ClientClosed);
        }
    }
}

impl Call<'_> {
    /// Counts the request's outcome, its one count.
    fn count(&self, outcome: Outcome) {
        self.counted.store(true, Ordering::Relaxed);
        self.handles.count(outcome);
    }

    /// Books the request on the bot's limits and waits out the delay they
    /// give; where the limits do not apply (mode `off`), books nothing.
    async fn admit(&self, chat: Option<&ChatKey>) -> Result<(), Limited> {
        if !self.limited {
            return Ok(());
        }
        let now = Instant::now();
        let admitted = self.bot.limiter.acquire(chat, now)?;
        if !admitted.delay.is_zero() {
            self.bot
                .metrics
                .ratelimited(admitted.scope, RateAction::Delayed);
            tracing::debug!(bot = %self.bot.name, method = self.label.as_str(), scope = admitted.scope.as_str(), delay = ?admitted.delay, "gateway request delayed by the rate limits");
            sleep_until(deadline(now, admitted.delay)).await;
        }
        Ok(())
    }

    /// Counts a request the limits refused.
    fn refuse(&self, limited: Limited) {
        self.bot
            .metrics
            .ratelimited(limited.scope, RateAction::Rejected);
        self.count(Outcome::RateLimited);
        tracing::debug!(bot = %self.bot.name, method = self.label.as_str(), scope = limited.scope.as_str(), retry_after = ?limited.retry_after, "gateway request refused by the rate limits");
    }

    /// Sends `body` to `url` and waits for Telegram's response headers:
    /// the response and when its headers arrived.
    async fn send(
        &self,
        url: Url,
        headers: HeaderMap,
        body: Body,
    ) -> Result<(reqwest::Response, Instant), Failed> {
        let idle = self.bot.gateway.upstream_idle_timeout;
        let progress = Arc::new(Progress::new());
        let upload = Upload {
            body: Mutex::new(body),
            progress: Arc::clone(&progress),
        };
        let started = Instant::now();
        let send = self
            .bot
            .client
            .request(self.method.clone(), url)
            .headers(headers)
            .body(reqwest::Body::wrap(upload))
            .send();
        let response = match await_headers(send, &progress, idle).await {
            Waited::Ready(Ok(response)) => response,
            Waited::Ready(Err(_)) if progress.client_failed() => return Err(Failed::ClientClosed),
            Waited::Ready(Err(err)) => {
                tracing::warn!(bot = %self.bot.name, method = self.label.as_str(), error = %describe_error(&err), "gateway request failed: Telegram is unreachable");
                return Err(Failed::Unreachable);
            }
            Waited::Idle => return Err(Failed::Idle),
        };
        let at = Instant::now();
        self.handles
            .upstream_seconds(at.duration_since(started).as_secs_f64());
        Ok((response, at))
    }

    /// The answer to a request that got no response, counted.
    fn failed(&self, failed: Failed) -> Response {
        match failed {
            Failed::ClientClosed => {
                // Not Telegram's failure, and nobody reads the answer.
                tracing::debug!(bot = %self.bot.name, method = self.label.as_str(), "gateway request abandoned: the client closed the request body early");
                self.count(Outcome::ClientClosed);
                errors::error(
                    StatusCode::BAD_REQUEST,
                    "Bad Request: the request body ended early",
                )
            }
            Failed::Unreachable => {
                self.counted.store(true, Ordering::Relaxed);
                unreachable(self.handles)
            }
            Failed::Idle => {
                tracing::warn!(bot = %self.bot.name, method = self.label.as_str(), idle_timeout = ?self.bot.gateway.upstream_idle_timeout, "gateway request failed: no progress within upstream_idle_timeout");
                self.counted.store(true, Ordering::Relaxed);
                unreachable(self.handles)
            }
            Failed::Refused(refusal) => {
                tracing::info!(bot = %self.bot.name, method = self.label.as_str(), reason = refusal.reason(), "gateway request refused: a local file reference, or parameters tgin cannot check, for a Bot API server in local mode");
                self.count(Outcome::Rejected);
                refusal.answer()
            }
        }
    }

    /// Telegram's answer, streamed to the client and counted once it ends.
    fn relay(&self, response: reqwest::Response) -> Response {
        self.counted.store(true, Ordering::Relaxed);
        let status = response.status();
        let headers = response_headers(&response);
        tracing::debug!(bot = %self.bot.name, method = self.label.as_str(), status = status.as_u16(), "gateway request proxied");
        let tally = Tally {
            name: Arc::clone(&self.bot.name),
            bot: self.bot.metrics.clone(),
            label: self.label,
            outcome: outcome(status),
            _forwarding: self.forwarding.lock().take(),
        };
        let download = Download::new(
            response,
            self.bot.gateway.upstream_idle_timeout,
            Some(tally),
        );
        answer(status, headers, Body::new(download))
    }

    /// Telegram answered a buffered request with 429. Its body is read up
    /// to [`FLOOD_LIMIT`]; when its `parameters.retry_after` is above zero
    /// and within `retry_429.max_wait`, the request waits that long after
    /// the 429 arrived (at `at`), books the limits again where they apply,
    /// and goes once more, whatever the rate-limit mode. Otherwise, or when
    /// the limits refuse it, the 429 goes to the client as it came.
    async fn flood(&self, response: reqwest::Response, at: Instant, again: Again) -> Response {
        let status = response.status();
        let headers = response_headers(&response);
        let idle = self.bot.gateway.upstream_idle_timeout;
        let body = Body::new(Download::new(response, idle, None));
        let flood = match read_bounded(body, FLOOD_LIMIT).await {
            Ok(Bounded::Complete(flood)) => flood,
            Ok(Bounded::Over(partial)) => {
                // Telegram's 429s are small: this one streams through, and
                // counts as relayed.
                self.count(Outcome::RateLimited);
                return answer(status, headers, partial.into_body());
            }
            Err(err) => {
                tracing::warn!(bot = %self.bot.name, method = self.label.as_str(), error = %err, "gateway response aborted");
                self.count(Outcome::UpstreamError);
                return errors::error(
                    StatusCode::BAD_GATEWAY,
                    "Bad Gateway: Telegram is unreachable",
                );
            }
        };
        let max_wait = self.bot.gateway.retry_429_max_wait;
        let Some(wait) = retry_after(&flood).filter(|wait| !wait.is_zero() && *wait <= max_wait)
        else {
            self.count(Outcome::RateLimited);
            return answer(status, headers, Body::from(flood));
        };
        sleep_until(deadline(at, wait)).await;
        if let Err(limited) = self.admit(again.chat.as_ref()).await {
            self.refuse(limited);
            return answer(status, headers, Body::from(flood));
        }
        self.bot.metrics.retried();
        tracing::debug!(bot = %self.bot.name, method = self.label.as_str(), retry_after = ?wait, "gateway request sent again after Telegram's 429");
        match self
            .send(again.url, again.headers, Body::from(again.body))
            .await
        {
            Ok((response, _)) => self.relay(response),
            Err(failed) => self.failed(failed),
        }
    }
}

/// `parameters.retry_after` of a Bot API error body.
fn retry_after(body: &[u8]) -> Option<Duration> {
    #[derive(Deserialize)]
    struct Flood {
        parameters: Option<Parameters>,
    }
    #[derive(Deserialize)]
    struct Parameters {
        retry_after: Option<u64>,
    }
    let flood: Flood = serde_json::from_slice(body).ok()?;
    flood.parameters?.retry_after.map(Duration::from_secs)
}

/// The allowlisted headers of `response`.
fn response_headers(response: &reqwest::Response) -> HeaderMap {
    let mut headers = HeaderMap::new();
    copy_headers(response.headers(), &RESPONSE_HEADERS, &mut headers);
    headers
}

fn answer(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut out = Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

/// `base` with `rest` (a method, or a file path still percent-encoded)
/// appended to its path, and `query` as the query. Appending keeps the
/// request under `base` whatever `rest` holds.
pub(super) fn url(base: &Url, rest: &str, query: Option<&str>) -> Url {
    let mut url = base.clone();
    let path = format!("{}{rest}", base.path());
    url.set_path(&path);
    url.set_query(query);
    url
}

/// Appends the values of every header of `names` that `from` has, except
/// those its `Connection` header names (hop-by-hop).
fn copy_headers(from: &HeaderMap, names: &[HeaderName], to: &mut HeaderMap) {
    for name in names {
        if hop_by_hop(from, name) {
            continue;
        }
        for value in from.get_all(name) {
            to.append(name.clone(), value.clone());
        }
    }
}

/// Whether `from`'s `Connection` header lists `name`.
fn hop_by_hop(from: &HeaderMap, name: &HeaderName) -> bool {
    from.get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|listed| listed.trim().eq_ignore_ascii_case(name.as_str()))
}

/// The outcome of a response with `status`.
pub(super) fn outcome(status: StatusCode) -> Outcome {
    if status == StatusCode::TOO_MANY_REQUESTS {
        Outcome::RateLimited
    } else if status.is_client_error() {
        Outcome::Rejected
    } else if status.is_server_error() {
        Outcome::UpstreamError
    } else {
        Outcome::Ok
    }
}

fn unreachable(handles: &MethodMetrics) -> Response {
    handles.count(Outcome::Unreachable);
    errors::error(
        StatusCode::BAD_GATEWAY,
        "Bad Gateway: Telegram is unreachable",
    )
}

/// `idle` after `from`; for an `idle` past any representable instant, 30
/// years after `from` (as tokio's own far future), which never comes.
pub(super) fn deadline(from: Instant, idle: Duration) -> Instant {
    const FAR: Duration = Duration::from_secs(30 * 365 * 86_400);
    from.checked_add(idle)
        .or_else(|| from.checked_add(FAR))
        .unwrap_or(from)
}

/// When the request body last moved, and whether it failed.
struct Progress {
    origin: Instant,
    /// Nanoseconds after `origin`.
    last: AtomicU64,
    /// Reading the downstream's body failed: the client went away.
    client_failed: AtomicBool,
}

impl Progress {
    fn new() -> Progress {
        Progress {
            origin: Instant::now(),
            last: AtomicU64::new(0),
            client_failed: AtomicBool::new(false),
        }
    }

    fn stamp(&self) {
        let nanos = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.last.fetch_max(nanos, Ordering::Relaxed);
    }

    fn last(&self) -> Instant {
        self.origin + Duration::from_nanos(self.last.load(Ordering::Relaxed))
    }

    fn client_failed(&self) -> bool {
        self.client_failed.load(Ordering::Relaxed)
    }
}

enum Waited<T> {
    Ready(T),
    /// Neither the request body nor the response moved for the idle
    /// timeout.
    Idle,
}

/// Waits for `send` until `idle` has passed since the request body last
/// moved (or ended; at the start for a body that never does). Also bounds
/// the read of a body tgin buffers, whose frames stamp `progress` alike.
async fn await_headers<F: Future>(
    send: F,
    progress: &Progress,
    idle: Duration,
) -> Waited<F::Output> {
    tokio::pin!(send);
    loop {
        let at = deadline(progress.last(), idle);
        tokio::select! {
            biased;
            output = &mut send => return Waited::Ready(output),
            () = sleep_until(at) => {
                if deadline(progress.last(), idle) <= Instant::now() {
                    return Waited::Idle;
                }
            }
        }
    }
}

/// The downstream's request body, made `Sync` for `reqwest::Body::wrap`,
/// which keeps its exact length (C§3.2), and stamping progress on every
/// frame and at its end.
struct Upload {
    /// Only `size_hint` and `is_end_stream` lock it; `poll_frame` reaches
    /// it through `get_mut`.
    body: Mutex<Body>,
    progress: Arc<Progress>,
}

impl HttpBody for Upload {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(this.body.get_mut()).poll_frame(cx);
        if polled.is_ready() {
            this.progress.stamp();
        }
        if let Poll::Ready(Some(Err(_))) = &polled {
            this.progress.client_failed.store(true, Ordering::Relaxed);
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.body.lock().is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.lock().size_hint()
    }
}

/// Why a response body stopped before its end.
#[derive(Debug, thiserror::Error)]
enum DownloadError {
    #[error("the upstream response body failed: {0}")]
    Upstream(String),
    #[error("the upstream response body made no progress within upstream_idle_timeout")]
    Idle,
}

/// Counts a request once its response body has ended, failed or been
/// dropped by the client.
struct Tally {
    name: Arc<str>,
    bot: BotMetrics,
    label: MethodLabel,
    /// From the status; replaced by `upstream_error` when the body fails.
    outcome: Outcome,
    /// Keeps the request in flight for the shutdown's drain until the body
    /// ends, fails or is dropped.
    _forwarding: Option<Forwarding>,
}

impl Tally {
    fn count(self, outcome: Outcome) {
        self.bot.method(self.label).count(outcome);
    }
}

/// Telegram's response body, streamed with its exact length (C§3.3); an
/// error after `idle` without a frame, which aborts the client's
/// connection.
struct Download {
    body: reqwest::Body,
    idle: Duration,
    sleep: Pin<Box<Sleep>>,
    /// `None` while the caller reads the body itself.
    tally: Option<Tally>,
}

impl Download {
    fn new(response: reqwest::Response, idle: Duration, tally: Option<Tally>) -> Download {
        Download {
            body: axum::http::Response::<reqwest::Body>::from(response).into_body(),
            idle,
            sleep: Box::pin(sleep_until(deadline(Instant::now(), idle))),
            tally,
        }
    }

    fn fail(&mut self, err: DownloadError) -> Poll<Option<Result<Frame<Bytes>, DownloadError>>> {
        if let Some(tally) = self.tally.take() {
            tracing::warn!(bot = %tally.name, method = tally.label.as_str(), error = %err, "gateway response aborted");
            tally.count(Outcome::UpstreamError);
        }
        Poll::Ready(Some(Err(err)))
    }
}

impl HttpBody for Download {
    type Data = Bytes;
    type Error = DownloadError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, DownloadError>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.body).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                this.sleep
                    .as_mut()
                    .reset(deadline(Instant::now(), this.idle));
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(err))) => this.fail(DownloadError::Upstream(describe_error(&err))),
            Poll::Ready(None) => {
                if let Some(tally) = this.tally.take() {
                    let outcome = tally.outcome;
                    tally.count(outcome);
                }
                Poll::Ready(None)
            }
            Poll::Pending => match this.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => this.fail(DownloadError::Idle),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        // Ended without an error: complete (hyper stops at the announced
        // length without polling the end), or the client went away.
        if let Some(tally) = self.tally.take() {
            let outcome = tally.outcome;
            tally.count(outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use super::*;

    /// A body tgin buffers is read with the same idle bound: a client that
    /// stops sending gets `Idle` after exactly `idle` without a frame, and
    /// one that goes away is told apart.
    #[tokio::test(start_paused = true)]
    async fn buffered_read_waits_at_most_idle_per_frame() {
        let idle = Duration::from_secs(5);
        let headers = json_headers();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let feeder = tokio::spawn(async move {
            for chunk in [&b"{\"chat_id\""[..], b":-5,"] {
                tokio::time::sleep(Duration::from_secs(4)).await;
                tx.send(Ok(Bytes::from_static(chunk))).await.expect("open");
            }
            // Stalls, keeping the body open.
            pending::<()>().await;
        });
        let started = Instant::now();
        let read = prepare(&headers, None, Body::from_stream(stream), idle, true).await;
        assert!(matches!(read, Err(Failed::Idle)));
        assert_eq!(started.elapsed(), Duration::from_secs(13));
        feeder.abort();

        let failing = futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other("gone"))]);
        let read = prepare(&headers, None, Body::from_stream(failing), idle, true).await;
        assert!(matches!(read, Err(Failed::ClientClosed)));
    }

    fn json_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers
    }

    /// The chat of a buffered body is the query string's `chat_id`, as
    /// Telegram reads it (`HttpReader` parses the URL before the body, and
    /// `Query::arg` takes the first match), else the body's own; a streamed
    /// body's is the query string's, the only one tgin can know. Without
    /// limits, none is read.
    #[tokio::test]
    async fn chat_from_query_before_body() {
        let idle = Duration::from_secs(5);
        let headers = json_headers();
        let query = Some("chat_id=%40Chan");
        let body = || Body::from(r#"{"chat_id":-5}"#);

        let Ok((Payload::Buffered(bytes), chat)) =
            prepare(&headers, query, body(), idle, true).await
        else {
            panic!("buffered");
        };
        assert_eq!(&bytes[..], br#"{"chat_id":-5}"#);
        assert_eq!(
            chat,
            Some(ChatKey::Username("chan".into())),
            "the query comes first"
        );

        let Ok((Payload::Buffered(_), chat)) =
            prepare(&headers, Some("x=1"), body(), idle, true).await
        else {
            panic!("buffered");
        };
        assert_eq!(chat, Some(ChatKey::Id(-5)), "else the body's");

        let Ok((Payload::Buffered(_), chat)) =
            prepare(&headers, query, Body::from("{}"), idle, true).await
        else {
            panic!("buffered");
        };
        assert_eq!(chat, Some(ChatKey::Username("chan".into())));

        let long = format!(r#"{{"chat_id":-5,"text":"{}"}}"#, "x".repeat(BUFFER_LIMIT));
        let Ok((Payload::Streamed(_), chat)) =
            prepare(&headers, query, Body::from(long), idle, true).await
        else {
            panic!("streamed");
        };
        assert_eq!(chat, Some(ChatKey::Username("chan".into())));

        let mut multipart = HeaderMap::new();
        multipart.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("multipart/form-data; boundary=b"),
        );
        let Ok((Payload::Streamed(_), chat)) = prepare(&multipart, None, body(), idle, true).await
        else {
            panic!("streamed");
        };
        assert_eq!(chat, None);
        let Ok((Payload::Streamed(_), chat)) = prepare(&multipart, query, body(), idle, true).await
        else {
            panic!("streamed");
        };
        assert_eq!(
            chat,
            Some(ChatKey::Username("chan".into())),
            "a multipart body's chat is the query's"
        );

        let Ok((Payload::Buffered(_), chat)) = prepare(&headers, query, body(), idle, false).await
        else {
            panic!("buffered");
        };
        assert_eq!(chat, None, "no limits, no chat");
    }

    #[test]
    fn retry_after_of_flood_bodies() {
        let flood = br#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 7","parameters":{"retry_after":7}}"#;
        assert_eq!(retry_after(flood), Some(Duration::from_secs(7)));
        for body in [
            &br#"{"ok":false,"error_code":429,"description":"Too Many Requests"}"#[..],
            br#"{"parameters":{"retry_after":-1}}"#,
            br#"{"parameters":{"migrate_to_chat_id":-100}}"#,
            b"<html>busy</html>",
        ] {
            assert_eq!(retry_after(body), None);
        }
    }

    /// The wait for headers ends only after `idle` without progress: each
    /// stamp moves the deadline.
    #[tokio::test(start_paused = true)]
    async fn headers_wait_follows_request_progress() {
        let idle = Duration::from_secs(10);
        let progress = Arc::new(Progress::new());
        let stamper = {
            let progress = Arc::clone(&progress);
            tokio::spawn(async move {
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_secs(8)).await;
                    progress.stamp();
                }
            })
        };
        let started = Instant::now();
        let waited = await_headers(pending::<()>(), &progress, idle).await;
        assert!(matches!(waited, Waited::Idle));
        assert_eq!(started.elapsed(), Duration::from_secs(50));
        stamper.await.expect("stamper");

        let ready = await_headers(async { 7 }, &Progress::new(), idle).await;
        assert!(matches!(ready, Waited::Ready(7)));
    }

    /// A response body errors after `idle` without a frame and is counted
    /// once as `upstream_error`; frames in time keep it alive.
    #[tokio::test(start_paused = true)]
    async fn download_aborts_after_idle_and_counts_once() {
        let mut reg = prometheus_client::registry::Registry::with_prefix("tgin");
        let metrics = super::super::GatewayMetrics::register(&mut reg);
        let bot = metrics.bot("b");
        let idle = Duration::from_secs(5);
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let mut download = Download {
            body: reqwest::Body::wrap(Upload {
                body: Mutex::new(Body::from_stream(stream)),
                progress: Arc::new(Progress::new()),
            }),
            idle,
            sleep: Box::pin(sleep_until(Instant::now() + idle)),
            tally: Some(Tally {
                name: Arc::from("b"),
                bot: bot.clone(),
                label: MethodLabel::File,
                outcome: Outcome::Ok,
                _forwarding: None,
            }),
        };
        async fn next(d: &mut Download) -> Option<Result<Frame<Bytes>, DownloadError>> {
            std::future::poll_fn(|cx| Pin::new(&mut *d).poll_frame(cx)).await
        }
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_secs(4)).await;
            tx.send(Ok(Bytes::from_static(b"x"))).await.expect("open");
            assert!(matches!(next(&mut download).await, Some(Ok(_))));
        }
        let started = Instant::now();
        assert!(matches!(
            next(&mut download).await,
            Some(Err(DownloadError::Idle))
        ));
        assert_eq!(started.elapsed(), idle);
        drop(download);
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &reg).expect("encode");
        assert!(out.lines().any(|l| l == r#"tgin_gateway_requests_total{bot="b",method="file",outcome="upstream_error"} 1"#), "{out}");
        assert!(
            out.lines().any(
                |l| l == r#"tgin_gateway_requests_total{bot="b",method="file",outcome="ok"} 0"#
            ),
            "{out}"
        );
    }
}
