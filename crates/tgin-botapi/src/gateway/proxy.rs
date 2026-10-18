//! The streaming proxy: a gateway request goes to `{url}/bot{token}/…` (or
//! to the bot's file base, see `files`) with an allowlist of headers, both
//! bodies stream without buffering and keep their `Content-Length`, and two
//! bounds apply: `upstream_idle_timeout` to the time without progress in
//! either direction while the request body is being sent and while the
//! response body streams, the bot's response wait (`Bot::response_timeout`:
//! the upstream's `response_timeout`, or `upstream_idle_timeout` on the
//! `api_url` shorthand) to the wait for the response headers once the
//! request body has been sent. A local Bot API server may take minutes to
//! answer a large upload it has whole.
//!
//! reqwest's own `read_timeout` cannot be either bound: it runs one fixed
//! timer from sending until the response headers (reqwest 0.13.5,
//! `async_impl/client.rs` `PendingRequest`), so an upload taking longer
//! would be cut while its bytes still flow. The client sets no timeout but
//! the connect one, and this module measures both itself.

mod prepare;
pub(super) mod transfer;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{
    ACCEPT, ACCEPT_ENCODING, ACCEPT_RANGES, CACHE_CONTROL, CONNECTION, CONTENT_DISPOSITION,
    CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderName,
    IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, RANGE, RETRY_AFTER, USER_AGENT, VIA,
};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use parking_lot::Mutex;
use reqwest::Url;
use serde::Deserialize;
use tgin_runtime::http::describe_error;
use tokio::time::{Instant, sleep_until};

use self::prepare::{Payload, prepare, prepare_scanned, refused_by, scanned_encoding};
use self::transfer::{Bounds, Download, Progress, Tally, Upload, Waited, await_headers, deadline};
use super::chat_key::ChatKey;
use super::limit::Limited;
use super::local_files::Refusal;
use super::metrics::{MethodLabel, MethodMetrics, Outcome, RateAction};
use super::{Bot, Bounded, Forwarding, errors, read_bounded, refuse_parked};

/// The longest upstream 429 body read for its `retry_after`.
const FLOOD_LIMIT: usize = 64 * 1024;
/// The 502 once the whole request was sent and no response came, because
/// the connection was lost or `response_timeout` passed: the server may
/// have executed the call, so a client must not take it as not done.
pub(super) const NO_ANSWER: &str =
    "Bad Gateway: the Bot API server did not answer; the call may have been executed";
/// The 502 when the call was not executed: the server never had the whole
/// request (the connect failed or never finished, or the upload stalled or
/// broke off), or it answered 429 and that answer's body broke (flood
/// control refused the call). A client may send it again.
const UNREACHABLE: &str = "Bad Gateway: the Bot API server is unreachable";
/// The 502 when a file origin (`files.url`) sent no answer: a download
/// changes nothing, so there is nothing to be unsure of.
const FILE_NO_ANSWER: &str = "Bad Gateway: the file server did not answer";
/// The 502 when a file origin could not be reached.
const FILE_UNREACHABLE: &str = "Bad Gateway: the file server is unreachable";

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
    /// `{api_url}/file/bot{token}/` (`server`: the Bot API server itself)
    /// or `{files.url}/{token}/` (a file origin, not the server).
    File {
        base: &'a Url,
        path: &'a str,
        server: bool,
    },
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
    let (url, file, method, server) = match upstream {
        Upstream::Method(method) => (
            url(&bot.method_base, method, parts.uri.query()),
            false,
            Some(method),
            true,
        ),
        Upstream::File { base, path, server } => {
            (url(base, path, parts.uri.query()), true, None, server)
        }
    };
    let limited = method.is_some_and(|method| bot.limiter.applies(method));
    let resendable = limited
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
        server,
    };
    let scanned = if method.is_some() && bot.refuses_local_files {
        match scanned_encoding(&url, &headers) {
            Ok(scanned) => scanned,
            Err(refusal) => return call.failed(Failed::Refused(refusal)),
        }
    } else {
        None
    };
    if !resendable && scanned.is_none() {
        return match call.send(url, headers, body, false).await {
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
        Payload::Streamed(body) => match call.send(url, headers, body, false).await {
            Ok((response, _)) => call.relay(response),
            Err(failed) => call.failed(refused_by(failed, verdict)),
        },
        Payload::Buffered(request) => {
            let first = call
                .send(
                    url.clone(),
                    headers.clone(),
                    Body::from(request.clone()),
                    false,
                )
                .await;
            let (response, at) = match first {
                Ok(first) => first,
                Err(failed) => return call.failed(failed),
            };
            if response.status() != StatusCode::TOO_MANY_REQUESTS
                || bot.gateway.retry_429_max_wait.is_zero()
                || !resendable
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

/// Why a request got no response from the Bot API server.
#[derive(Debug)]
enum Failed {
    /// Reading the client's body failed: the client went away.
    ClientClosed,
    /// The server never had the whole request: the connect failed or never
    /// finished, or the connection broke off before the request body was
    /// all sent (logged where it happened).
    Unreachable,
    /// The body did not move for `upstream_idle_timeout`: while it was sent
    /// upstream, in either direction, with the server not reading it (the
    /// client's body had a frame ready); or while tgin read it from the
    /// client before sending anything (`prepare`, `read_within`), a client
    /// stall that never reached the server.
    Idle,
    /// As [`Failed::Idle`], but the client's body had nothing to send: the
    /// downstream stalled, not the server. Answered as `Idle` is.
    ClientIdle,
    /// The whole request was sent, and the connection was lost or
    /// `response_timeout` passed before the response headers (logged where
    /// it happened): the server may have executed the call.
    NoAnswer,
    /// Its parameters may not reach the bot's self-hosted server.
    Refused(Refusal),
    /// The bot was parked while the call waited (a rate limit, a 429): it
    /// never left tgin.
    Parked,
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
    /// Whether the request goes to the bot's Bot API server, whose
    /// outcomes feed `tgin_upstream_up` (a file origin's do not).
    server: bool,
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

    /// Sends `body` to `url` and waits for the server's response headers:
    /// the response and when its headers arrived. Feeds the bot's
    /// `tgin_upstream_up`: any response is an answer; every failure but
    /// the client's own leaves the upstream unanswering. A bot parked since
    /// the call came in sends nothing; the call counts as being sent until
    /// the answer's headers came, so a move waits for it. `again`, the
    /// resend after a 429, counts in `tgin_gateway_retries_total` once it
    /// goes.
    async fn send(
        &self,
        url: Url,
        headers: HeaderMap,
        body: Body,
        again: bool,
    ) -> Result<(reqwest::Response, Instant), Failed> {
        let Some(_sending) = self.bot.gate.enter() else {
            return Err(Failed::Parked);
        };
        if again {
            self.bot.metrics.retried();
            tracing::debug!(bot = %self.bot.name, method = self.label.as_str(), "gateway request sent again after the Bot API server's 429");
        }
        let sent = self.exchange(url, headers, body).await;
        if self.server {
            let up = self.bot.upstream_metrics.up();
            match &sent {
                Ok(_) => up.answered(),
                Err(Failed::ClientClosed | Failed::ClientIdle) => {}
                Err(_) => up.unreachable(),
            }
        }
        sent
    }

    /// [`Call::send`]'s exchange. While the body is being sent,
    /// `upstream_idle_timeout` bounds its stalls; once it has been sent,
    /// `response_timeout` bounds the wait from its end, for the Bot API
    /// server. A file origin serves static files: `upstream_idle_timeout`
    /// bounds its answer too.
    async fn exchange(
        &self,
        url: Url,
        headers: HeaderMap,
        body: Body,
    ) -> Result<(reqwest::Response, Instant), Failed> {
        let idle = self.bot.gateway.upstream_idle_timeout;
        let bounds = Bounds {
            idle,
            response: if self.server {
                self.bot.response_timeout
            } else {
                idle
            },
        };
        let peer = self.peer();
        let progress = Arc::new(Progress::new());
        let upload = Upload::new(body, Arc::clone(&progress));
        let started = Instant::now();
        let send = self
            .bot
            .client
            .request(self.method.clone(), url)
            .headers(headers)
            .body(reqwest::Body::wrap(upload))
            .send();
        let response = match await_headers(send, &progress, bounds).await {
            Waited::Ready(Ok(response)) => response,
            Waited::Ready(Err(_)) if progress.client_failed() => return Err(Failed::ClientClosed),
            // An empty body counts as sent once hyper has asked for its
            // end on a connection; a failed connect is told apart by the
            // error as well.
            Waited::Ready(Err(err)) if err.is_connect() || !progress.ended() => {
                tracing::warn!(bot = %self.bot.name, method = self.label.as_str(), error = %describe_error(&err), "gateway request failed: {peer} is unreachable");
                return Err(Failed::Unreachable);
            }
            Waited::Ready(Err(err)) => {
                if self.server {
                    tracing::warn!(bot = %self.bot.name, method = self.label.as_str(), error = %describe_error(&err), "gateway request failed: the Bot API server did not answer; the call may have been executed");
                } else {
                    tracing::warn!(bot = %self.bot.name, method = self.label.as_str(), error = %describe_error(&err), "gateway request failed: the file server did not answer");
                }
                return Err(Failed::NoAnswer);
            }
            Waited::Idle if progress.client_pending() => return Err(Failed::ClientIdle),
            Waited::Idle => return Err(Failed::Idle),
            Waited::NoAnswer => {
                if self.server {
                    tracing::warn!(bot = %self.bot.name, method = self.label.as_str(), response_timeout = ?bounds.response, "gateway request failed: no response within response_timeout after the request was sent; the call may have been executed");
                } else {
                    tracing::warn!(bot = %self.bot.name, method = self.label.as_str(), idle_timeout = ?bounds.response, "gateway request failed: the file server did not answer within upstream_idle_timeout");
                }
                return Err(Failed::NoAnswer);
            }
        };
        let at = Instant::now();
        self.handles
            .upstream_seconds(at.duration_since(started).as_secs_f64());
        Ok((response, at))
    }

    /// Who the request goes to, for log lines.
    fn peer(&self) -> &'static str {
        if self.server {
            "the Bot API server"
        } else {
            "the file server"
        }
    }

    /// The description of the 502 for a request without an answer: `sent`,
    /// the whole request had gone out. Only a call to the Bot API server
    /// may have been executed.
    fn no_answer(&self, sent: bool) -> &'static str {
        match (self.server, sent) {
            (true, true) => NO_ANSWER,
            (true, false) => UNREACHABLE,
            (false, true) => FILE_NO_ANSWER,
            (false, false) => FILE_UNREACHABLE,
        }
    }

    /// The answer to a request that got no response, counted.
    fn failed(&self, failed: Failed) -> Response {
        match failed {
            Failed::ClientClosed => {
                // Not the server's failure, and nobody reads the answer.
                tracing::debug!(bot = %self.bot.name, method = self.label.as_str(), "gateway request abandoned: the client closed the request body early");
                self.count(Outcome::ClientClosed);
                errors::error(
                    StatusCode::BAD_REQUEST,
                    "Bad Request: the request body ended early",
                )
            }
            Failed::Unreachable => {
                self.counted.store(true, Ordering::Relaxed);
                unreachable(self.handles, self.no_answer(false))
            }
            Failed::Idle | Failed::ClientIdle => {
                tracing::warn!(bot = %self.bot.name, method = self.label.as_str(), idle_timeout = ?self.bot.gateway.upstream_idle_timeout, "gateway request failed: no progress within upstream_idle_timeout");
                self.counted.store(true, Ordering::Relaxed);
                unreachable(self.handles, self.no_answer(false))
            }
            Failed::NoAnswer => {
                self.counted.store(true, Ordering::Relaxed);
                unreachable(self.handles, self.no_answer(true))
            }
            Failed::Refused(refusal) => {
                tracing::info!(bot = %self.bot.name, method = self.label.as_str(), reason = refusal.reason(), "gateway request refused: a local file reference, or parameters tgin cannot check, for a Bot API server other than the Telegram cloud");
                self.count(Outcome::Rejected);
                refusal.answer()
            }
            Failed::Parked => {
                self.counted.store(true, Ordering::Relaxed);
                refuse_parked(self.bot, self.handles, self.label)
            }
        }
    }

    /// The server's answer, streamed to the client and counted once it ends.
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

    /// The server answered a buffered request with 429. Its body is read up
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
                // 429s are small: this one streams through, and counts as
                // relayed.
                self.count(Outcome::RateLimited);
                return answer(status, headers, partial.into_body());
            }
            Err(err) => {
                tracing::warn!(bot = %self.bot.name, method = self.label.as_str(), error = %err, "gateway response aborted");
                self.count(Outcome::UpstreamError);
                // The 429 status already says flood control refused the
                // call: it was not executed, so the client may send it again.
                return errors::error(StatusCode::BAD_GATEWAY, UNREACHABLE);
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
        tracing::debug!(bot = %self.bot.name, method = self.label.as_str(), retry_after = ?wait, "gateway request waited out the Bot API server's 429");
        match self
            .send(again.url, again.headers, Body::from(again.body), true)
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

/// Counts a request that got no HTTP answer from the server, whatever the
/// reason (a failed connect, a stalled upload, a lost connection or
/// `response_timeout`), as `unreachable`, and answers it with a 502 whose
/// `description` tells whether the server may have executed the call.
fn unreachable(handles: &MethodMetrics, description: &str) -> Response {
    handles.count(Outcome::Unreachable);
    errors::error(StatusCode::BAD_GATEWAY, description)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
