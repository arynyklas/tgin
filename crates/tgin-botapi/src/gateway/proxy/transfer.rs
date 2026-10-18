//! The proxy's timing and body wrappers: how long a request waits for its
//! server ([`await_headers`]), and the request and response bodies that
//! stamp progress ([`Upload`]) and count outcomes ([`Download`], [`Tally`]).

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use axum::body::{Body, Bytes};
use http_body::{Body as HttpBody, Frame, SizeHint};
use parking_lot::Mutex;
use tgin_runtime::http::describe_error;
use tokio::sync::Notify;
use tokio::time::{Instant, Sleep, sleep_until};

use super::super::Forwarding;
use super::super::metrics::{BotMetrics, MethodLabel, Outcome};

/// `idle` after `from`; for an `idle` past any representable instant, 30
/// years after `from` (as tokio's own far future), which never comes.
pub(in crate::gateway) fn deadline(from: Instant, idle: Duration) -> Instant {
    const FAR: Duration = Duration::from_secs(30 * 365 * 86_400);
    from.checked_add(idle)
        .or_else(|| from.checked_add(FAR))
        .unwrap_or(from)
}

/// When the request body last moved, whether it has ended, and whether it
/// failed.
pub(super) struct Progress {
    origin: Instant,
    /// Nanoseconds after `origin`.
    last: AtomicU64,
    /// The body has ended: all of it was handed on, and `last` is when.
    ended: AtomicBool,
    /// Notified once when the body ends.
    end: Notify,
    /// Reading the downstream's body failed: the client went away.
    client_failed: AtomicBool,
    /// The downstream's body answered its last poll with `Pending` and has
    /// not woken since: it had nothing to send, so a stall since is the
    /// client's, not the server's. Its waker clears it (see the [`Wake`]
    /// impl): a body ready again while the connection stops polling it (an
    /// HTTP/2 stream whose window closed) is the server's stall.
    client_pending: AtomicBool,
    /// The waker of the last poll of the downstream's body, woken through
    /// this [`Progress`].
    waker: Mutex<Option<Waker>>,
}

impl Progress {
    pub(super) fn new() -> Progress {
        Progress {
            origin: Instant::now(),
            last: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            end: Notify::new(),
            client_failed: AtomicBool::new(false),
            client_pending: AtomicBool::new(false),
            waker: Mutex::new(None),
        }
    }

    fn stamp(&self) {
        let nanos = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.last.fetch_max(nanos, Ordering::Relaxed);
    }

    /// Marks the body ended now, once.
    fn finish(&self) {
        if self.ended.load(Ordering::Acquire) {
            return;
        }
        self.stamp();
        if !self.ended.swap(true, Ordering::AcqRel) {
            self.end.notify_one();
        }
    }

    fn last(&self) -> Instant {
        self.origin + Duration::from_nanos(self.last.load(Ordering::Relaxed))
    }

    pub(super) fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    pub(super) fn client_failed(&self) -> bool {
        self.client_failed.load(Ordering::Relaxed)
    }

    pub(super) fn client_pending(&self) -> bool {
        self.client_pending.load(Ordering::Relaxed)
    }
}

/// The downstream's body is polled with this waker, so that its readiness
/// clears `client_pending` before the connection's own waker runs.
impl Wake for Progress {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.client_pending.store(false, Ordering::Relaxed);
        if let Some(waker) = self.waker.lock().as_ref() {
            waker.wake_by_ref();
        }
    }
}

/// The bounds of [`await_headers`].
#[derive(Clone, Copy, Debug)]
pub(super) struct Bounds {
    /// While the request body has not ended: the longest time without
    /// progress.
    pub(super) idle: Duration,
    /// Once it has ended: the longest wait from its end.
    pub(super) response: Duration,
}

pub(super) enum Waited<T> {
    Ready(T),
    /// The request body did not move for `idle` before it ended.
    Idle,
    /// Nothing came for `response` after the request body ended.
    NoAnswer,
}

/// Waits for `send` until `bounds.idle` has passed since the request body
/// last moved (at the start for a body that never does), or, once the body
/// has ended, until `bounds.response` has passed since its end. Also
/// bounds the read of a body tgin buffers, whose frames stamp `progress`
/// alike.
pub(super) async fn await_headers<F: Future>(
    send: F,
    progress: &Progress,
    bounds: Bounds,
) -> Waited<F::Output> {
    tokio::pin!(send);
    loop {
        let ended = progress.ended();
        let bound = if ended { bounds.response } else { bounds.idle };
        let at = deadline(progress.last(), bound);
        tokio::select! {
            biased;
            output = &mut send => return Waited::Ready(output),
            // The end moves the deadline, possibly earlier.
            () = progress.end.notified(), if !ended => {}
            () = sleep_until(at) => {
                if progress.ended() == ended && deadline(progress.last(), bound) <= Instant::now() {
                    return if ended { Waited::NoAnswer } else { Waited::Idle };
                }
            }
        }
    }
}

/// The downstream's request body, made `Sync` for `reqwest::Body::wrap`,
/// which keeps its exact length (C§3.2), stamping progress on every frame
/// and marking its end: after the frame past which it has no more (hyper
/// stops at the announced length without polling for the end), at its
/// end, or, for an empty body, when hyper first asks whether it has ended.
/// hyper asks as it writes the request head on a connection, never before
/// the connect has finished, so a call whose connect is still being made
/// has not been sent, empty body or not.
pub(super) struct Upload {
    /// Only `size_hint` and `is_end_stream` lock it; `poll_frame` reaches
    /// it through `get_mut`.
    body: Mutex<Body>,
    progress: Arc<Progress>,
}

impl Upload {
    pub(super) fn new(body: Body, progress: Arc<Progress>) -> Upload {
        Upload {
            body: Mutex::new(body),
            progress,
        }
    }
}

impl HttpBody for Upload {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        {
            let mut slot = this.progress.waker.lock();
            if !slot
                .as_ref()
                .is_some_and(|waker| waker.will_wake(cx.waker()))
            {
                *slot = Some(cx.waker().clone());
            }
        }
        // Set before the poll: a wake during or after it clears it.
        this.progress.client_pending.store(true, Ordering::Relaxed);
        let waker = Waker::from(Arc::clone(&this.progress));
        let body = this.body.get_mut();
        let polled = Pin::new(&mut *body).poll_frame(&mut Context::from_waker(&waker));
        if polled.is_ready() {
            this.progress.client_pending.store(false, Ordering::Relaxed);
        }
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                this.progress.stamp();
                // hyper's client polls nothing after a trailers frame.
                if frame.is_trailers() || body.is_end_stream() {
                    this.progress.finish();
                }
            }
            Poll::Ready(None) => this.progress.finish(),
            Poll::Ready(Some(Err(_))) => {
                this.progress.stamp();
                this.progress.client_failed.store(true, Ordering::Relaxed);
            }
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        let ended = self.body.lock().is_end_stream();
        if ended {
            self.progress.finish();
        }
        ended
    }

    fn size_hint(&self) -> SizeHint {
        self.body.lock().size_hint()
    }
}

/// Why a response body stopped before its end.
#[derive(Debug, thiserror::Error)]
pub(super) enum DownloadError {
    #[error("the upstream response body failed: {0}")]
    Upstream(String),
    #[error("the upstream response body made no progress within upstream_idle_timeout")]
    Idle,
}

/// Counts a request once its response body has ended, failed or been
/// dropped by the client.
pub(super) struct Tally {
    pub(super) name: Arc<str>,
    pub(super) bot: BotMetrics,
    pub(super) label: MethodLabel,
    /// From the status; replaced by `upstream_error` when the body fails.
    pub(super) outcome: Outcome,
    /// Keeps the request in flight for the shutdown's drain until the body
    /// ends, fails or is dropped.
    pub(super) _forwarding: Option<Forwarding>,
}

impl Tally {
    fn count(self, outcome: Outcome) {
        self.bot.method(self.label).count(outcome);
    }
}

/// Telegram's response body, streamed with its exact length (C§3.3); an
/// error after `idle` without a frame, which aborts the client's
/// connection.
pub(super) struct Download {
    body: reqwest::Body,
    idle: Duration,
    sleep: Pin<Box<Sleep>>,
    /// `None` while the caller reads the body itself.
    tally: Option<Tally>,
}

impl Download {
    pub(super) fn new(
        response: reqwest::Response,
        idle: Duration,
        tally: Option<Tally>,
    ) -> Download {
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

    use axum::http::HeaderMap;

    use super::*;

    /// While the body has not ended, the wait for headers ends only after
    /// `idle` without progress: each stamp moves the deadline, and a long
    /// `response` bound does not apply.
    #[tokio::test(start_paused = true)]
    async fn headers_wait_follows_request_progress() {
        let bounds = Bounds {
            idle: Duration::from_secs(10),
            response: Duration::from_secs(3_600),
        };
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
        let waited = await_headers(pending::<()>(), &progress, bounds).await;
        assert!(matches!(waited, Waited::Idle));
        assert_eq!(started.elapsed(), Duration::from_secs(50));
        stamper.await.expect("stamper");

        let ready = await_headers(async { 7 }, &Progress::new(), bounds).await;
        assert!(matches!(ready, Waited::Ready(7)));
    }

    /// Once the body has ended, the wait for headers ends `response` after
    /// its end, whether that is longer than `idle` or shorter (the end wakes
    /// a wait asleep towards a later idle deadline).
    #[tokio::test(start_paused = true)]
    async fn headers_wait_switches_to_response_bound_at_the_end() {
        let secs = Duration::from_secs;
        for (idle, response) in [(secs(10), secs(100)), (secs(100), secs(10))] {
            let progress = Arc::new(Progress::new());
            let finisher = {
                let progress = Arc::clone(&progress);
                tokio::spawn(async move {
                    tokio::time::sleep(secs(8)).await;
                    progress.stamp();
                    tokio::time::sleep(secs(8)).await;
                    progress.finish();
                })
            };
            let started = Instant::now();
            let waited = await_headers(pending::<()>(), &progress, Bounds { idle, response }).await;
            assert!(matches!(waited, Waited::NoAnswer), "idle {idle:?}");
            assert_eq!(started.elapsed(), secs(16) + response, "idle {idle:?}");
            finisher.await.expect("finisher");
        }
    }

    /// An empty body has ended once hyper asks, which it does on a
    /// connection, not before; a body of known length ends with its last
    /// frame, before anything polls for its end (hyper stops at the
    /// announced length).
    #[tokio::test]
    async fn upload_ends_with_its_last_frame() {
        let progress = Arc::new(Progress::new());
        let empty = Upload::new(Body::empty(), Arc::clone(&progress));
        assert!(!progress.ended(), "not sent before hyper asks");
        assert!(empty.is_end_stream());
        assert!(progress.ended());

        let progress = Arc::new(Progress::new());
        let mut upload = Upload::new(Body::from("abc"), Arc::clone(&progress));
        assert!(!progress.ended());
        let frame = std::future::poll_fn(|cx| Pin::new(&mut upload).poll_frame(cx)).await;
        assert!(matches!(frame, Some(Ok(_))));
        assert!(progress.ended());
    }

    /// A downstream body that had nothing to send is the client's stall
    /// only until it wakes: ready again, it is not, though the connection
    /// has not polled it since (an HTTP/2 stream whose window closed), and
    /// the connection's own waker still runs.
    #[tokio::test]
    async fn upload_ready_again_is_no_client_stall() {
        struct Counted(AtomicU64);
        impl Wake for Counted {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let progress = Arc::new(Progress::new());
        let mut upload = Upload::new(Body::from_stream(stream), Arc::clone(&progress));
        let woken = Arc::new(Counted(AtomicU64::new(0)));
        let waker = Waker::from(Arc::clone(&woken));
        let polled = Pin::new(&mut upload).poll_frame(&mut Context::from_waker(&waker));
        assert!(polled.is_pending());
        assert!(progress.client_pending());
        tx.send(Ok(Bytes::from_static(b"x"))).await.expect("open");
        assert!(!progress.client_pending(), "ready again, not polled");
        assert_eq!(woken.0.load(Ordering::Relaxed), 1);
        let polled = Pin::new(&mut upload).poll_frame(&mut Context::from_waker(&waker));
        assert!(matches!(polled, Poll::Ready(Some(Ok(_)))));
        assert!(!progress.client_pending());
    }

    /// A chunked body may end with a trailers frame, after which hyper
    /// polls nothing more: the body has ended there.
    #[tokio::test]
    async fn upload_ends_with_a_trailers_frame() {
        struct Trailed(Vec<Frame<Bytes>>);
        impl HttpBody for Trailed {
            type Data = Bytes;
            type Error = std::io::Error;
            fn poll_frame(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
                let frames = &mut self.get_mut().0;
                Poll::Ready((!frames.is_empty()).then(|| Ok(frames.remove(0))))
            }
        }
        let progress = Arc::new(Progress::new());
        let body = Body::new(Trailed(vec![
            Frame::data(Bytes::from_static(b"abc")),
            Frame::trailers(HeaderMap::new()),
        ]));
        let mut upload = Upload::new(body, Arc::clone(&progress));
        let data = std::future::poll_fn(|cx| Pin::new(&mut upload).poll_frame(cx)).await;
        assert!(matches!(data, Some(Ok(_))));
        assert!(!progress.ended());
        let trailers = std::future::poll_fn(|cx| Pin::new(&mut upload).poll_frame(cx)).await;
        assert!(matches!(trailers, Some(Ok(frame)) if frame.is_trailers()));
        assert!(progress.ended());
    }

    /// A response body errors after `idle` without a frame and is counted
    /// once as `upstream_error`; frames in time keep it alive.
    #[tokio::test(start_paused = true)]
    async fn download_aborts_after_idle_and_counts_once() {
        let mut reg = prometheus_client::registry::Registry::with_prefix("tgin");
        let metrics = super::super::super::GatewayMetrics::register(&mut reg);
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
