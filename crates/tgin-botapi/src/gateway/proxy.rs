//! The streaming proxy: a gateway request goes to `{api_url}/bot{token}/…`
//! with an allowlist of headers, both bodies stream without buffering and
//! keep their `Content-Length`, and `upstream_idle_timeout` bounds the time
//! without progress in either direction.
//!
//! reqwest's own `read_timeout` cannot be that bound: it runs one fixed
//! timer from sending until the response headers (reqwest 0.13.5,
//! `async_impl/client.rs` `PendingRequest`), so an upload taking longer
//! would be cut while its bytes still flow. The client sets no timeout but
//! the connect one, and this module measures idleness itself.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::header::{
    ACCEPT, ACCEPT_ENCODING, ACCEPT_RANGES, CACHE_CONTROL, CONNECTION, CONTENT_DISPOSITION,
    CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderName,
    IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, RANGE, RETRY_AFTER, USER_AGENT,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use http_body::{Body as HttpBody, Frame, SizeHint};
use parking_lot::Mutex;
use reqwest::Url;
use tgin_runtime::http::describe_error;
use tokio::time::{Instant, Sleep, sleep_until};

use super::metrics::{BotMetrics, MethodLabel, MethodMetrics, Outcome};
use super::{Bot, errors};

/// Request headers forwarded on every route.
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

/// What a gateway path asks of Telegram.
pub(super) enum Upstream<'a> {
    /// `<method>`: `{api_url}/bot{token}/<method>`.
    Method(&'a str),
    /// `<file_path>`, still percent-encoded:
    /// `{api_url}/file/bot{token}/<file_path>`.
    File(&'a str),
}

/// Streams `req` to Telegram for `bot` and Telegram's answer back.
pub(super) async fn forward(
    bot: &Bot,
    upstream: Upstream<'_>,
    label: MethodLabel,
    req: Request,
) -> Response {
    let handles = bot.metrics.method(label);
    let idle = bot.gateway.upstream_idle_timeout;
    let (parts, body) = req.into_parts();
    let (url, file) = match upstream {
        Upstream::Method(method) => (url(&bot.method_base, method, parts.uri.query()), false),
        Upstream::File(file_path) => (url(&bot.file_base, file_path, parts.uri.query()), true),
    };
    let mut headers = HeaderMap::new();
    copy_headers(&parts.headers, &REQUEST_HEADERS, &mut headers);
    if file {
        copy_headers(&parts.headers, &FILE_REQUEST_HEADERS, &mut headers);
    }

    let progress = Arc::new(Progress::new());
    let upload = Upload {
        body: Mutex::new(body),
        progress: Arc::clone(&progress),
    };
    let started = Instant::now();
    let send = bot
        .client
        .request(parts.method, url)
        .headers(headers)
        .body(reqwest::Body::wrap(upload))
        .send();
    let response = match await_headers(send, &progress, idle).await {
        Waited::Ready(Ok(response)) => response,
        Waited::Ready(Err(err)) => {
            tracing::warn!(bot = %bot.name, method = label.as_str(), error = %describe_error(&err), "gateway request failed: Telegram is unreachable");
            return unreachable(handles);
        }
        Waited::Idle => {
            tracing::warn!(bot = %bot.name, method = label.as_str(), idle_timeout = ?idle, "gateway request failed: no progress within upstream_idle_timeout");
            return unreachable(handles);
        }
    };
    handles.upstream_seconds(started.elapsed().as_secs_f64());

    let status = response.status();
    let mut headers = HeaderMap::new();
    copy_headers(response.headers(), &RESPONSE_HEADERS, &mut headers);
    tracing::debug!(bot = %bot.name, method = label.as_str(), status = status.as_u16(), "gateway request proxied");
    let body = axum::http::Response::<reqwest::Body>::from(response).into_body();
    let download = Download {
        body,
        idle,
        sleep: Box::pin(sleep_until(Instant::now() + idle)),
        tally: Some(Tally {
            name: Arc::clone(&bot.name),
            bot: bot.metrics.clone(),
            label,
            outcome: outcome(status),
        }),
    };
    let mut out = Response::new(Body::new(download));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

/// `base` with `rest` (a method, or a file path still percent-encoded)
/// appended to its path, and `query` as the query. Appending keeps the
/// request under `base` whatever `rest` holds.
fn url(base: &Url, rest: &str, query: Option<&str>) -> Url {
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
fn outcome(status: StatusCode) -> Outcome {
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

/// When the request body last moved.
struct Progress {
    origin: Instant,
    /// Nanoseconds after `origin`.
    last: AtomicU64,
}

impl Progress {
    fn new() -> Progress {
        Progress {
            origin: Instant::now(),
            last: AtomicU64::new(0),
        }
    }

    fn stamp(&self) {
        let nanos = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.last.fetch_max(nanos, Ordering::Relaxed);
    }

    fn last(&self) -> Instant {
        self.origin + Duration::from_nanos(self.last.load(Ordering::Relaxed))
    }
}

enum Waited<T> {
    Ready(T),
    /// Neither the request body nor the response moved for the idle
    /// timeout.
    Idle,
}

/// Waits for `send` until `idle` has passed since the request body last
/// moved (or ended; at the start for a body that never does).
async fn await_headers<F: Future>(
    send: F,
    progress: &Progress,
    idle: Duration,
) -> Waited<F::Output> {
    tokio::pin!(send);
    loop {
        let deadline = progress.last() + idle;
        tokio::select! {
            biased;
            output = &mut send => return Waited::Ready(output),
            () = sleep_until(deadline) => {
                if progress.last() + idle <= Instant::now() {
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
    tally: Option<Tally>,
}

impl Download {
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
                let deadline = Instant::now() + this.idle;
                this.sleep.as_mut().reset(deadline);
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
