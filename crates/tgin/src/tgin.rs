use crate::api::message::{ApiError, ApiMessage};
use crate::api::router::Api;
use crate::base::{
    AddRouteError, RemoveRouteError, RouteableComponent, Serverable, UpdaterComponent,
};

use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Sender, WeakSender};

use axum_server::tls_rustls::RustlsConfig;
use axum_server::Handle;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use tokio::runtime::Builder;

use crate::dynamic::handler::dynamic_handler;

use tracing::Instrument;

pub struct Tgin {
    updates: Vec<Box<dyn UpdaterComponent>>,
    route: Arc<dyn RouteableComponent>,
    dark_threads: usize,
    server_port: Option<u16>,

    pub ssl_cert: Option<String>,
    pub ssl_key: Option<String>,

    api: Option<Api>,

    auth_token: Option<String>,

    ingress_health: Option<IngressHealth>,
}

impl Tgin {
    pub fn new(
        updates: Vec<Box<dyn UpdaterComponent>>,
        route: Arc<dyn RouteableComponent>,
        dark_threads: usize,
        server_port: Option<u16>,
    ) -> Self {
        Self {
            updates,
            route,
            dark_threads,
            server_port,
            ssl_cert: None,
            ssl_key: None,
            api: None,
            auth_token: None,
            ingress_health: None,
        }
    }

    pub fn set_api(&mut self, api: Api) {
        self.api = Some(api);
    }

    pub fn set_ssl(&mut self, ssl_cert: String, ssl_key: String) {
        self.ssl_cert = Some(ssl_cert);
        self.ssl_key = Some(ssl_key);
    }

    pub fn set_auth_token(&mut self, auth_token: Option<String>) {
        self.auth_token = auth_token;
    }

    /// Share the long-poll permanent-failure counter so shutdown can fire
    /// [`ShutdownCause::AllIngressFailed`] once no ingress can deliver
    /// updates any more: every one of the `long_poll_updaters` long-poll
    /// updaters has failed permanently and there is no webhook ingress.
    pub fn set_ingress_health(
        &mut self,
        permanent_failures: Arc<AtomicUsize>,
        long_poll_updaters: usize,
        has_webhook_ingress: bool,
    ) {
        self.ingress_health = Some(IngressHealth {
            permanent_failures,
            long_poll_updaters,
            has_webhook_ingress,
        });
    }

    pub fn run(self) -> std::io::Result<ShutdownOutcome> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(self.dark_threads)
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime");
        runtime.block_on(async {
            tracing::info!(worker_threads = self.dark_threads, "tgin started");
            for update in &self.updates {
                tracing::info!(source = %update.print().await, "ingress configured");
            }
            tracing::info!(routing = %self.route.print().await, "routing tree configured");
        });

        let outcome = runtime.block_on(self.run_async());
        if matches!(outcome, Ok(ShutdownOutcome::Forced)) {
            // Exit now: do not wait for dispatches still in flight.
            runtime.shutdown_background();
        }
        outcome
    }

    /// Serve until a process signal, then drain with the default timing.
    pub async fn run_async(self) -> std::io::Result<ShutdownOutcome> {
        self.run_until(OsSignals::default(), ShutdownTiming::default())
            .await
    }

    /// Serve until `signals` delivers a shutdown cause or every ingress has
    /// failed permanently, then drain as paced by `timing` (see the
    /// coordinator below) and return. A signal received once shutdown has
    /// started cuts the drain short ([`ShutdownOutcome::Forced`]).
    pub async fn run_until(
        self,
        signals: impl ShutdownSignals,
        timing: ShutdownTiming,
    ) -> std::io::Result<ShutdownOutcome> {
        // Bounded buffer: a small queue is flow control. The previous
        // 1_000_000-slot channel was not — it was a runway for OOM that
        // hid downstream slowness from producers until memory ran out.
        // With a bounded buffer, a slow downstream backpressures producers
        // (LongPoll stops calling `getUpdates`; webhook handlers hold open)
        // and overload becomes immediately visible.
        let (tx, rx) = mpsc::channel::<Bytes>(UPDATE_CHANNEL_CAPACITY);

        crate::observe::set_route(self.route.clone());

        let api = self.api;
        let auth_token = self.auth_token;

        // Cancelled after the lame duck: long-poll updaters stop pulling from
        // Telegram and webhook ingress starts answering 503.
        let ingest = CancellationToken::new();
        // Released only after the HTTP server has closed, so the worker pool
        // and the control-plane loop keep consuming updates produced by
        // in-flight webhook handlers right up until the server is done —
        // then the workers drain the backlog until `stop`.
        let drain = CancellationToken::new();
        // Cancelled at the drain deadline: workers abort the dispatch in
        // hand and exit, leaving the rest of the queue.
        let stop = CancellationToken::new();
        // Cancelled on a second shutdown signal: `run_until` returns at once.
        let forced = CancellationToken::new();
        // Set by the shutdown coordinator as its first action, flipping
        // `/readyz` to 503 so load balancers stop routing to this instance.
        let draining = Arc::new(AtomicBool::new(false));

        let server: Option<(Handle<SocketAddr>, tokio::task::JoinHandle<()>)> =
            if let Some(port) = self.server_port {
                let mut router: Router<Sender<Bytes>> = Router::new();

                for provider in &self.updates {
                    router = provider.set_server(router).await;
                }

                router = self.route.set_server(router).await;

                if let Some(ref api) = api {
                    router = api.set_server(router).await;
                }

                // Always-on observability endpoints and probes, independent
                // of the management API. Explicit routes take precedence over
                // the `dynamic_handler` fallback. `/status` and `/metrics`
                // share the management API's optional bearer gate; `/livez`
                // and `/readyz` stay open for orchestrator probes.
                router = router.merge(observability_routes(auth_token.clone(), draining.clone()));

                let status_metrics = if auth_token.is_some() {
                    "bearer-guarded"
                } else {
                    "unauthenticated"
                };
                tracing::info!(
                    "observability endpoints mounted: /status and /metrics \
                     ({status_metrics}), /livez and /readyz (open)"
                );

                let app = router.with_state(tx.clone());

                let app = if api.is_some() {
                    app.fallback(dynamic_handler)
                } else {
                    app
                };

                let addr = SocketAddr::from(([0, 0, 0, 0], port));

                // Bind on THIS task so a bind failure (EADDRINUSE, bad addr) is
                // an honest startup error propagated to `main`, not a panic
                // lost in a detached task that leaves a live process with no
                // HTTP plane.
                let std_listener = std::net::TcpListener::bind(addr)?;
                std_listener.set_nonblocking(true)?;

                let handle = Handle::new();
                tracing::info!(%addr, "listening");

                let task = match (self.ssl_cert.clone(), self.ssl_key.clone()) {
                    (Some(cert_path), Some(key_path)) => {
                        let config = RustlsConfig::from_pem_file(cert_path, key_path).await?;
                        let server = axum_server::from_tcp_rustls(std_listener, config)?
                            .handle(handle.clone());
                        tokio::spawn(async move {
                            if let Err(err) = server.serve(app.into_make_service()).await {
                                tracing::error!(error = %err, "HTTPS server terminated with error");
                            }
                        })
                    }
                    _ => {
                        let server = axum_server::from_tcp(std_listener)?.handle(handle.clone());
                        tokio::spawn(async move {
                            if let Err(err) = server.serve(app.into_make_service()).await {
                                tracing::error!(error = %err, "HTTP server terminated with error");
                            }
                        })
                    }
                };

                Some((handle, task))
            } else {
                None
            };

        for provider in self.updates {
            if let Some(m) = provider.metrics() {
                crate::observe::register_updater(m);
            }
            let tx_clone = tx.clone();
            let ingest = ingest.clone();
            tokio::spawn(async move {
                provider.start(tx_clone, ingest).await;
            });
        }

        // Shutdown coordinator. On a cause (a signal, or every ingress having
        // failed permanently):
        //   1. set `draining` so `/readyz` answers 503, and keep serving as
        //      usual for `lame_duck` while load balancers move traffic away;
        //   2. cancel `ingest`: long-poll updaters stop pulling from Telegram
        //      and webhook ingress answers 503 so Telegram redelivers later;
        //   3. `begin_drain` the routing tree: parked long polls wake, and a
        //      poll on an empty buffer is answered with an empty batch within
        //      `DRAINING_EMPTY_POLL_HOLD` instead of its full timeout;
        //   4. keep the HTTP plane up until the update channel is empty and
        //      no route holds pending updates, or `grace` elapses, so
        //      consumers still fetch what was already acknowledged;
        //   5. close the HTTP plane (in-flight requests get
        //      `HTTP_CLOSE_GRACE`), then cancel `drain` so the worker pool
        //      and control-plane loop finish any backlog and exit;
        //   6. at the drain deadline (see `drain_deadline`) cancel `stop`:
        //      workers abort the dispatch in hand and exit, so a slow or dead
        //      downstream cannot hold the process past the deadline.
        // Without an HTTP plane there is no traffic to move away and nobody
        // to fetch buffered updates, so steps 1, 3 and 4 and the HTTP close
        // are skipped: ingest and `drain` are cancelled at once and `stop`
        // follows after `grace`. A signal received at any point after the
        // cause cancels `forced` instead, and `run_until` returns at once.
        // The coordinator holds only a weak sender, so it never keeps the
        // update channel open.
        {
            let ingest = ingest.clone();
            let drain = drain.clone();
            let stop = stop.clone();
            let forced = forced.clone();
            let draining = draining.clone();
            let route = self.route.clone();
            let updates = tx.downgrade();
            let ingress_health = self.ingress_health;
            let mut signals = signals;
            tokio::spawn(async move {
                let cause = tokio::select! {
                    cause = signals.recv() => cause,
                    () = all_ingress_failed(ingress_health) => ShutdownCause::AllIngressFailed,
                };
                let timing = timing.for_cause(cause);
                let sequence = async {
                    let deadline = match server {
                        None => {
                            tracing::info!(
                                ?cause,
                                grace = ?timing.grace,
                                "shutdown started; no HTTP plane, stopping ingest"
                            );
                            ingest.cancel();
                            drain.cancel();
                            Instant::now() + timing.grace
                        }
                        Some((handle, task)) => {
                            draining.store(true, Ordering::SeqCst);
                            tracing::info!(
                                ?cause,
                                lame_duck = ?timing.lame_duck,
                                grace = ?timing.grace,
                                "shutdown started; draining"
                            );
                            tokio::time::sleep(timing.lame_duck).await;
                            ingest.cancel();
                            route.begin_drain();
                            let drain_start = Instant::now();
                            let _ = tokio::time::timeout(
                                timing.grace,
                                backlog_drained(&updates, route.as_ref()),
                            )
                            .await;
                            let close_start = Instant::now();
                            handle.graceful_shutdown(Some(HTTP_CLOSE_GRACE));
                            let deadline = drain_deadline(drain_start, timing.grace, close_start);
                            let _ = tokio::time::timeout_at(deadline, task).await;
                            drain.cancel();
                            deadline
                        }
                    };
                    tokio::time::sleep_until(deadline).await;
                    stop.cancel();
                };
                tokio::select! {
                    () = sequence => {}
                    _ = signals.recv() => {
                        tracing::warn!("second shutdown signal; exiting without completing the drain");
                        forced.cancel();
                    }
                }
            });
        }

        // Drop the dispatcher's own sender so the channel closes cleanly
        // once every producer task ends — that signals workers to drain and
        // exit.
        drop(tx);

        // Fixed-size consumer pool: `dark_threads` workers each pull one
        // update at a time and run `route.process` to completion before
        // taking the next, capping in-flight work (and any `AllLB` fanout) at
        // `dark_threads`. Workers stop when the channel closes on the clean
        // no-signal path, or when `drain` is cancelled (after the HTTP server
        // has finished draining), at which point they finish the buffered
        // backlog and exit, or stop early at the drain deadline. They never
        // wait on every `Sender` clone dropping, which the axum router does
        // not guarantee on shutdown.
        let pool = spawn_worker_pool(
            rx,
            self.route.clone(),
            self.dark_threads,
            drain.clone(),
            stop,
        );

        let route = self.route;
        let finished = async {
            if let Some(api) = api {
                // Control plane runs alongside the worker pool. Move the
                // receiver out of `Api`, dropping its own `Sender` clone — on
                // the no-server path that is the last sender, so `recv`
                // returns `None` and the loop ends. With a server running, the
                // `drain` token is authoritative: the loop keeps serving
                // control-plane requests through the HTTP drain and breaks once
                // `drain` is cancelled, rather than waiting for every
                // router-held sender clone to drop (which is not guaranteed).
                let Api { mut rx, .. } = api;
                loop {
                    let api_msg = tokio::select! {
                        biased;
                        msg = rx.recv() => msg,
                        _ = drain.cancelled() => break,
                    };
                    let Some(api_msg) = api_msg else {
                        break;
                    };
                    match api_msg {
                        ApiMessage::GetRoutes(tx_response) => {
                            let _ = tx_response.send(route.json_struct().await);
                        }

                        // Run synchronously in the API loop and plumb the
                        // result back through the oneshot. Mutation is
                        // sub-millisecond (`arc_swap::rcu` over a `Vec`) so
                        // there is no latency case for spawning, and
                        // synchronous handling is what lets the trait-level
                        // `AddRouteError` surface as an honest 4xx instead of
                        // a silent 200.
                        ApiMessage::AddRoute { route: added, resp } => {
                            let result = route.add_route(added).await.map_err(|err| match err {
                                AddRouteError::Conflict => ApiError::Conflict(
                                    "top-level route does not accept dynamic children".into(),
                                ),
                            });
                            let _ = resp.send(result);
                        }

                        // Symmetric with AddRoute: the previous
                        // `tokio::spawn` made `add` then `rm` racy and
                        // hid "target absent" behind a 200. Running in
                        // the loop preserves caller ordering and lets
                        // `RemoveRouteError::NotFound` surface as a 404.
                        ApiMessage::RmRoute { target, resp } => {
                            let result =
                                route.remove_route(target).await.map_err(|err| match err {
                                    RemoveRouteError::NotFound => ApiError::NotFound(
                                        "no route in the tree matched the supplied target".into(),
                                    ),
                                });
                            let _ = resp.send(result);
                        }
                    }
                }
            }

            // Without the API, workers exit when the channel closes (every
            // ingress producer dropped its `Sender` clone) or at the drain.
            // Wait for them so the runtime doesn't tear down mid-dispatch.
            pool.join().await
        };

        let left = tokio::select! {
            biased;
            () = forced.cancelled() => return Ok(ShutdownOutcome::Forced),
            left = finished => left,
        };

        // Updates still queued or aborted mid-dispatch at the deadline, and
        // whatever long-poll consumers did not fetch, are lost with the
        // process.
        let buffered = route.pending();
        let abandoned = left.queued + left.aborted + buffered;
        if abandoned > 0 {
            tracing::warn!(
                abandoned,
                queued = left.queued,
                aborted = left.aborted,
                buffered,
                "exiting with updates not delivered before the drain deadline"
            );
        }

        Ok(ShutdownOutcome::Completed { abandoned })
    }
}

/// Bounded capacity for the update mpsc. Sized to absorb short bursts
/// without giving slow downstreams a runway to OOM. The previous
/// 1_000_000-slot buffer was a buffer, not flow control; with this cap,
/// producers see backpressure immediately when the worker pool falls
/// behind.
const UPDATE_CHANNEL_CAPACITY: usize = 1024;

/// Shutdown pacing. On a cause, `/readyz` flips to 503 and the listeners keep
/// accepting for `lame_duck`, so load balancers stop routing here before
/// ingest stops; then buffered updates get up to `grace` to reach their
/// consumers before the HTTP plane closes, and dispatch stops at the
/// [`drain_deadline`]. The whole shutdown takes at most about
/// `lame_duck + grace + HTTP_CLOSE_GRACE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownTiming {
    pub lame_duck: Duration,
    pub grace: Duration,
}

impl Default for ShutdownTiming {
    /// 5 s + 15 s (+ the final 1 s HTTP close) fits the common 30 s
    /// orchestrator (k8s/systemd) kill window.
    fn default() -> Self {
        Self {
            lame_duck: Duration::from_secs(5),
            grace: Duration::from_secs(15),
        }
    }
}

impl ShutdownTiming {
    /// Timing for one cause. Windows kills the process about 5 s after
    /// `CTRL_CLOSE_EVENT`, so closing the console skips the lame duck and
    /// caps the grace at 4 s to finish inside that window.
    fn for_cause(self, cause: ShutdownCause) -> ShutdownTiming {
        match cause {
            #[cfg(windows)]
            ShutdownCause::CtrlClose => ShutdownTiming {
                lame_duck: Duration::ZERO,
                grace: self.grace.min(Duration::from_secs(4)),
            },
            _ => self,
        }
    }
}

/// Inputs for [`ShutdownCause::AllIngressFailed`]; see
/// [`Tgin::set_ingress_health`].
struct IngressHealth {
    permanent_failures: Arc<AtomicUsize>,
    long_poll_updaters: usize,
    has_webhook_ingress: bool,
}

impl IngressHealth {
    /// True when at least one long-poll updater exists, there is no webhook
    /// ingress, and every long-poll updater has failed permanently.
    fn all_failed(&self) -> bool {
        self.long_poll_updaters > 0
            && !self.has_webhook_ingress
            && self.permanent_failures.load(Ordering::Relaxed) >= self.long_poll_updaters
    }
}

/// How often [`all_ingress_failed`] re-checks the permanent-failure counter.
const INGRESS_HEALTH_INTERVAL: Duration = Duration::from_secs(1);

/// How often [`backlog_drained`] re-checks the channel and the routes.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Deadline for in-flight HTTP requests once the backlog is drained (or the
/// grace has elapsed) and the HTTP plane closes.
const HTTP_CLOSE_GRACE: Duration = Duration::from_secs(1);

/// When the workers stop: the end of the grace that began at `drain_start`
/// or, if the grace is already spent, `HTTP_CLOSE_GRACE` after the HTTP
/// close began at `close_start`. The close begins no later than the end of
/// the grace, so the deadline is at most `grace + HTTP_CLOSE_GRACE` after
/// `drain_start`.
fn drain_deadline(drain_start: Instant, grace: Duration, close_start: Instant) -> Instant {
    (drain_start + grace).max(close_start + HTTP_CLOSE_GRACE)
}

/// Resolves once every ingress has failed permanently; never resolves
/// without [`Tgin::set_ingress_health`].
async fn all_ingress_failed(health: Option<IngressHealth>) {
    let Some(health) = health else {
        return std::future::pending().await;
    };
    let mut tick = tokio::time::interval(INGRESS_HEALTH_INTERVAL);
    loop {
        tick.tick().await;
        if health.all_failed() {
            return;
        }
    }
}

/// Resolves once the update channel is empty and `route` holds no pending
/// updates. The channel also counts as empty once every sender is gone: the
/// workers then drain it on their own.
///
/// Known residual: updates workers have taken from the channel but not yet
/// pushed into their routes (up to one per worker, so `dark_threads`) are
/// visible to neither check. If this samples in that window (a synchronous
/// push for long-poll routes), the HTTP plane may close with those updates
/// still on their way; they are then reported in the `abandoned` count
/// logged when `run_until` returns.
async fn backlog_drained(updates: &WeakSender<Bytes>, route: &dyn RouteableComponent) {
    loop {
        let channel_empty = updates
            .upgrade()
            .is_none_or(|tx| tx.capacity() == tx.max_capacity());
        if channel_empty && route.pending() == 0 {
            return;
        }
        tokio::time::sleep(DRAIN_POLL_INTERVAL).await;
    }
}

/// Always-on observability routes. `/livez` always answers 200 while the
/// process serves HTTP; `/readyz` answers 200 until `draining` is set, then
/// 503. Both probes sit outside the bearer gate so orchestrators can probe
/// without credentials; `/status` and `/metrics` stay behind it. The
/// handlers take no `State`, so the router is valid on any state type.
fn observability_routes(
    auth_token: Option<String>,
    draining: Arc<AtomicBool>,
) -> Router<Sender<Bytes>> {
    let guarded = crate::utils::auth::guard(
        Router::new()
            .route("/status", get(crate::observe::status_handler))
            .route("/metrics", get(crate::observe::metrics_handler)),
        auth_token,
    );

    Router::new()
        .route("/livez", get(|| async { "ok" }))
        .route(
            "/readyz",
            get(move || {
                let ready = !draining.load(Ordering::SeqCst);
                async move {
                    if ready {
                        (StatusCode::OK, "ready")
                    } else {
                        (StatusCode::SERVICE_UNAVAILABLE, "draining")
                    }
                }
            }),
        )
        .merge(guarded)
}

/// What triggered shutdown. Signal variants are gated to the platform that
/// can deliver them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownCause {
    /// SIGTERM.
    #[cfg(unix)]
    Terminate,
    /// SIGINT on Unix, `CTRL_C_EVENT` on Windows.
    CtrlC,
    /// `CTRL_BREAK_EVENT`, also what `GenerateConsoleCtrlEvent` delivers to
    /// a process group started with `CREATE_NEW_PROCESS_GROUP`.
    #[cfg(windows)]
    CtrlBreak,
    /// `CTRL_CLOSE_EVENT`: the console window is closing.
    #[cfg(windows)]
    CtrlClose,
    /// `CTRL_SHUTDOWN_EVENT`: the system is shutting down.
    #[cfg(windows)]
    CtrlShutdown,
    /// Every ingress has failed permanently; nothing can deliver updates.
    AllIngressFailed,
}

/// How [`Tgin::run_until`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// The drain ran to completion or to its deadline. `abandoned` counts
    /// the updates it did not deliver: still queued for dispatch, aborted
    /// mid-dispatch at the deadline, or left in long-poll buffers.
    Completed { abandoned: usize },
    /// A second shutdown signal cut the drain short.
    Forced,
}

/// Source of shutdown signals for [`Tgin::run_until`]. The first signal
/// starts the drain; any signal after that forces exit.
pub trait ShutdownSignals: Send + 'static {
    /// Resolves with the next signal.
    fn recv(&mut self) -> impl Future<Output = ShutdownCause> + Send;
}

/// The process's own shutdown signals: SIGINT and SIGTERM on Unix; Ctrl-C,
/// Ctrl-Break, console close and system shutdown on Windows. Listeners are
/// registered on the first [`ShutdownSignals::recv`] and stay registered
/// for the rest of the run, so a second signal reaches tgin. On Windows,
/// that is what keeps a second Ctrl-C from the default handler, which
/// would kill the process outright.
#[derive(Default)]
pub struct OsSignals {
    listeners: Option<SignalListeners>,
}

impl ShutdownSignals for OsSignals {
    async fn recv(&mut self) -> ShutdownCause {
        self.listeners
            .get_or_insert_with(SignalListeners::register)
            .recv()
            .await
    }
}

/// `Some(listener)`, or `None` with a warning if it could not be registered:
/// a registration failure must not shut the process down.
fn listen<T>(listener: std::io::Result<T>, name: &str) -> Option<T> {
    listener
        .inspect_err(|err| tracing::warn!(error = %err, signal = name, "cannot listen for signal"))
        .ok()
}

/// Await the next event on an `&mut Option<listener>`, or never resolve
/// without a listener.
macro_rules! recv_or_pending {
    ($listener:expr) => {
        async {
            match $listener {
                Some(sig) => {
                    sig.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        }
    };
}

#[cfg(unix)]
struct SignalListeners {
    interrupt: Option<tokio::signal::unix::Signal>,
    terminate: Option<tokio::signal::unix::Signal>,
}

#[cfg(unix)]
impl SignalListeners {
    fn register() -> Self {
        use tokio::signal::unix::{signal, SignalKind};

        Self {
            interrupt: listen(signal(SignalKind::interrupt()), "SIGINT"),
            terminate: listen(signal(SignalKind::terminate()), "SIGTERM"),
        }
    }

    /// Resolves with the next SIGINT or SIGTERM.
    async fn recv(&mut self) -> ShutdownCause {
        let Self {
            interrupt,
            terminate,
        } = self;
        tokio::select! {
            () = recv_or_pending!(interrupt) => ShutdownCause::CtrlC,
            () = recv_or_pending!(terminate) => ShutdownCause::Terminate,
        }
    }
}

#[cfg(windows)]
struct SignalListeners {
    ctrl_c: Option<tokio::signal::windows::CtrlC>,
    ctrl_break: Option<tokio::signal::windows::CtrlBreak>,
    ctrl_close: Option<tokio::signal::windows::CtrlClose>,
    ctrl_shutdown: Option<tokio::signal::windows::CtrlShutdown>,
}

#[cfg(windows)]
impl SignalListeners {
    fn register() -> Self {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};

        Self {
            ctrl_c: listen(ctrl_c(), "ctrl_c"),
            ctrl_break: listen(ctrl_break(), "ctrl_break"),
            ctrl_close: listen(ctrl_close(), "ctrl_close"),
            ctrl_shutdown: listen(ctrl_shutdown(), "ctrl_shutdown"),
        }
    }

    /// Resolves with the next console control event. For close and
    /// shutdown, tokio holds the OS handler open, so the process keeps
    /// running only until Windows' own kill deadline.
    async fn recv(&mut self) -> ShutdownCause {
        let Self {
            ctrl_c,
            ctrl_break,
            ctrl_close,
            ctrl_shutdown,
        } = self;
        tokio::select! {
            () = recv_or_pending!(ctrl_c) => ShutdownCause::CtrlC,
            () = recv_or_pending!(ctrl_break) => ShutdownCause::CtrlBreak,
            () = recv_or_pending!(ctrl_close) => ShutdownCause::CtrlClose,
            () = recv_or_pending!(ctrl_shutdown) => ShutdownCause::CtrlShutdown,
        }
    }
}

/// The dispatch workers, plus what [`WorkerPool::join`] needs to count the
/// updates they leave undelivered.
struct WorkerPool {
    workers: Vec<tokio::task::JoinHandle<()>>,
    rx: Arc<tokio::sync::Mutex<mpsc::Receiver<Bytes>>>,
    aborted: Arc<AtomicUsize>,
}

/// Updates a [`WorkerPool`] left undelivered when it stopped.
#[derive(Debug, PartialEq, Eq)]
struct LeftOver {
    /// Still in the dispatch queue.
    queued: usize,
    /// Taken from the queue, then aborted mid-dispatch at the deadline.
    aborted: usize,
}

impl WorkerPool {
    /// Wait for every worker to exit, then count what they left behind.
    async fn join(self) -> LeftOver {
        for w in self.workers {
            let _ = w.await;
        }
        LeftOver {
            queued: self.rx.lock().await.len(),
            aborted: self.aborted.load(Ordering::Relaxed),
        }
    }
}

/// Spawn `worker_count` (min 1) consumers over a shared receiver. Each worker
/// pulls one update at a time and runs `route.process` to completion before
/// taking the next, so in-flight work is capped at `worker_count`.
///
/// Termination is deterministic and does not rely on every `Sender` clone
/// being dropped (the axum router retains state-sender clones past its own
/// drop): a worker stops when the channel closes (clean no-signal path),
/// when `drain` is cancelled and the queue is empty, or when `stop` is
/// cancelled. After `drain` the biased `recv` still wins whenever an update
/// is ready, so a backlog keeps draining. `stop` wins over everything: the
/// worker takes nothing more from the queue and drops the dispatch in hand,
/// which aborts it (its outbound request, or an `AllLB` fanout).
fn spawn_worker_pool(
    rx: mpsc::Receiver<Bytes>,
    route: Arc<dyn RouteableComponent>,
    worker_count: usize,
    drain: CancellationToken,
    stop: CancellationToken,
) -> WorkerPool {
    let worker_count = worker_count.max(1);
    // Shared via a `tokio::sync::Mutex`: the guard is held only across the
    // pick (`recv`), never across `route.process`, so N-1 workers can be
    // processing while one is parked waiting for the next update.
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let aborted = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let rx = rx.clone();
        let route = route.clone();
        let drain = drain.clone();
        let stop = stop.clone();
        let aborted = aborted.clone();
        workers.push(tokio::spawn(async move {
            loop {
                let next = {
                    let mut guard = rx.lock().await;
                    tokio::select! {
                        biased;
                        () = stop.cancelled() => None,
                        msg = guard.recv() => msg,
                        // Draining: take whatever is buffered (one per
                        // iteration via the biased `recv` above); `try_recv`
                        // yields `None` once empty, ending the worker.
                        () = drain.cancelled() => guard.try_recv().ok(),
                    }
                };
                // `Bytes::clone` inside the routing tree is one atomic op; no
                // `Value` allocation, no re-serialization on egress.
                let Some(update) = next else {
                    return;
                };
                let span = crate::observe::dispatch_span(&update);
                tokio::select! {
                    biased;
                    () = route.process(update).instrument(span) => {}
                    () = stop.cancelled() => {
                        aborted.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                }
            }
        }));
    }
    WorkerPool {
        workers,
        rx,
        aborted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::{Printable, Routeable, Serverable};
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    /// Counts processed updates. Records the maximum number of
    /// `process` calls observed in flight at the same time so a test can
    /// assert that work actually parallelises across workers.
    struct ConcurrencyProbe {
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        completed: AtomicUsize,
        gate: Notify,
        release: AtomicUsize,
    }

    impl ConcurrencyProbe {
        fn new() -> Self {
            Self {
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
                completed: AtomicUsize::new(0),
                gate: Notify::new(),
                release: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Routeable for ConcurrencyProbe {
        async fn process(&self, _update: Bytes) {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);

            // Park each call on a shared `Notify` until the test releases
            // it. `release` tracks how many notifies have been issued so a
            // late arrival can short-circuit instead of deadlocking.
            loop {
                if self.release.load(Ordering::SeqCst) > 0 {
                    self.release.fetch_sub(1, Ordering::SeqCst);
                    break;
                }
                self.gate.notified().await;
            }

            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.completed.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Serverable for ConcurrencyProbe {}
    impl Printable for ConcurrencyProbe {}

    /// `dark_threads` workers must run `process` concurrently — the new
    /// design replaces unbounded `tokio::spawn` with a fixed worker pool,
    /// so this is the test that proves the pool actually parallelises and
    /// is not, e.g., serialised by the receiver mutex.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_workers_process_in_parallel() {
        let probe = Arc::new(ConcurrencyProbe::new());
        let route_dyn: Arc<dyn RouteableComponent> = probe.clone();

        let workers_n = 4;
        let (tx, rx) = mpsc::channel::<Bytes>(UPDATE_CHANNEL_CAPACITY);
        let pool = spawn_worker_pool(
            rx,
            route_dyn,
            workers_n,
            CancellationToken::new(),
            CancellationToken::new(),
        );

        for i in 0..workers_n {
            tx.send(update_bytes(json!({ "update_id": i })))
                .await
                .unwrap();
        }

        // Spin until every worker is parked in `process` simultaneously.
        // Bound the wait so a regression to a serialising design fails
        // the test instead of hanging it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while probe.in_flight.load(Ordering::SeqCst) < workers_n {
            if std::time::Instant::now() > deadline {
                panic!(
                    "only {} workers reached `process` concurrently; expected {}",
                    probe.in_flight.load(Ordering::SeqCst),
                    workers_n
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        assert_eq!(
            probe.max_in_flight.load(Ordering::SeqCst),
            workers_n,
            "max in-flight should equal worker count"
        );

        probe.release.store(workers_n, Ordering::SeqCst);
        probe.gate.notify_waiters();
        drop(tx);
        pool.join().await;
    }

    /// End-to-end through the public dispatch surface: spawn the worker
    /// pool via a test-only helper that mirrors `run_async` and assert
    /// that all updates land at the route.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_workers_drain_all_updates_and_exit_on_close() {
        use crate::mock::routes::MockCallsRoute;

        let route = Arc::new(MockCallsRoute::new("sink"));
        let route_dyn: Arc<dyn RouteableComponent> = route.clone();

        let (tx, rx) = mpsc::channel::<Bytes>(UPDATE_CHANNEL_CAPACITY);
        let pool = spawn_worker_pool(
            rx,
            route_dyn.clone(),
            4,
            CancellationToken::new(),
            CancellationToken::new(),
        );

        for i in 0..32 {
            tx.send(update_bytes(json!({ "update_id": i })))
                .await
                .unwrap();
        }
        drop(tx);

        for w in pool.workers {
            w.await.unwrap();
        }

        assert_eq!(route.count().await, 32);
    }

    /// Capacity is small enough that a stalled worker pool backpressures
    /// the producer immediately, instead of accepting unbounded buffering.
    /// Concretely: with N workers all parked in `process`, the (N+CAP)+1th
    /// `try_send` must fail with `Full`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_bounded_capacity_surfaces_backpressure() {
        let probe = Arc::new(ConcurrencyProbe::new());
        let route_dyn: Arc<dyn RouteableComponent> = probe.clone();

        let workers_n = 2;
        let (tx, rx) = mpsc::channel::<Bytes>(UPDATE_CHANNEL_CAPACITY);
        let pool = spawn_worker_pool(
            rx,
            route_dyn,
            workers_n,
            CancellationToken::new(),
            CancellationToken::new(),
        );

        // Fill the channel: workers are blocked on `gate`, so anything we
        // send beyond `worker_count + capacity` must be rejected.
        let mut accepted = 0usize;
        loop {
            match tx.try_send(update_bytes(json!({ "update_id": accepted }))) {
                Ok(()) => accepted += 1,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => break,
                Err(e) => panic!("unexpected send error: {:?}", e),
            }
            if accepted > UPDATE_CHANNEL_CAPACITY + workers_n + 16 {
                panic!(
                    "channel never reported Full after {} sends; capacity is unbounded?",
                    accepted
                );
            }
        }

        // Channel is bounded — backpressure surfaced. The exact count is
        // `capacity + items currently held by workers`, which is at most
        // `UPDATE_CHANNEL_CAPACITY + workers_n`. We allow equality on the
        // upper bound to tolerate scheduling races on which side observed
        // the slot first.
        assert!(
            accepted <= UPDATE_CHANNEL_CAPACITY + workers_n,
            "accepted {} sends but capacity should be {} + {} workers",
            accepted,
            UPDATE_CHANNEL_CAPACITY,
            workers_n
        );
        assert!(
            accepted >= UPDATE_CHANNEL_CAPACITY,
            "accepted {} sends but capacity is {}",
            accepted,
            UPDATE_CHANNEL_CAPACITY
        );

        // Release every parked worker so the test shuts down cleanly.
        probe.release.store(accepted, Ordering::SeqCst);
        probe.gate.notify_waiters();
        drop(tx);
        pool.join().await;
    }

    /// Helper for tests that want a `Bytes` payload from a `serde_json::Value`.
    fn update_bytes(value: Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&value).unwrap())
    }

    /// A bind failure (port already in use) MUST surface as `Err` from
    /// `run_async` so `main` can exit non-zero, rather than panicking in a
    /// detached task and leaving a live process with no HTTP plane.
    #[tokio::test]
    async fn test_run_async_errors_when_port_unavailable() {
        use crate::mock::routes::MockCallsRoute;

        // Occupy 0.0.0.0:<port>; tgin binds the same addr and must fail fast.
        let occupied = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = occupied.local_addr().unwrap().port();

        let route: Arc<dyn RouteableComponent> = Arc::new(MockCallsRoute::new("sink"));
        let tgin = Tgin::new(vec![], route, 1, Some(port));

        let result = tgin.run_async().await;
        assert!(
            result.is_err(),
            "bind on an occupied port must surface as Err, not a detached panic"
        );
    }

    /// Graceful-shutdown contract for the worker pool: cancelling the
    /// shutdown token must drain whatever is already buffered and then exit
    /// every worker — even though the channel is NOT closed (a `Sender` is
    /// still alive). `run_async` relies on this: the axum router keeps
    /// `Sender` clones past its own drop, so workers can never depend on the
    /// channel closing to stop on shutdown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_worker_pool_drains_buffered_then_exits_on_shutdown() {
        use crate::mock::routes::MockCallsRoute;

        let route = Arc::new(MockCallsRoute::new("sink"));
        let route_dyn: Arc<dyn RouteableComponent> = route.clone();

        let (tx, rx) = mpsc::channel::<Bytes>(UPDATE_CHANNEL_CAPACITY);
        let shutdown = CancellationToken::new();
        let pool = spawn_worker_pool(rx, route_dyn, 4, shutdown.clone(), CancellationToken::new());

        for i in 0..16 {
            tx.send(update_bytes(json!({ "update_id": i })))
                .await
                .unwrap();
        }

        // Cancel while `tx` is still held open: proves workers stop on the
        // token, not on the channel closing.
        shutdown.cancel();

        for w in pool.workers {
            tokio::time::timeout(std::time::Duration::from_secs(5), w)
                .await
                .expect("worker did not exit within 5s of shutdown despite a live sender")
                .expect("worker task panicked");
        }

        // Every buffered update was drained before the workers exited.
        assert_eq!(route.count().await, 16);
        drop(tx);
    }

    /// At the drain deadline `stop` ends the pool even though every worker
    /// is parked in a dispatch that never completes: the dispatches in hand
    /// are aborted, nothing more is taken from the queue, and `join` counts
    /// both.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_worker_pool_stop_aborts_in_flight_and_counts_leftovers() {
        let probe = Arc::new(ConcurrencyProbe::new());
        let route_dyn: Arc<dyn RouteableComponent> = probe.clone();

        let workers_n = 2;
        let (tx, rx) = mpsc::channel::<Bytes>(UPDATE_CHANNEL_CAPACITY);
        let drain = CancellationToken::new();
        let stop = CancellationToken::new();
        let pool = spawn_worker_pool(rx, route_dyn, workers_n, drain.clone(), stop.clone());

        for i in 0..5 {
            tx.send(update_bytes(json!({ "update_id": i })))
                .await
                .unwrap();
        }
        let in_flight = probe.clone();
        wait_until(
            Duration::from_secs(2),
            "every worker to be dispatching",
            || in_flight.in_flight.load(Ordering::SeqCst) == workers_n,
        )
        .await;

        drain.cancel();
        stop.cancel();

        let left = tokio::time::timeout(Duration::from_secs(2), pool.join())
            .await
            .expect("workers kept running after stop");
        assert_eq!(
            left,
            LeftOver {
                queued: 3,
                aborted: workers_n
            }
        );
        assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
        drop(tx);
    }

    /// Build the observability sub-router with a throwaway update channel as
    /// state, exactly as `run_async` attaches it before serving.
    fn probe_app(auth_token: Option<String>, draining: Arc<AtomicBool>) -> Router {
        let (tx, _rx) = mpsc::channel::<Bytes>(1);
        observability_routes(auth_token, draining).with_state(tx)
    }

    async fn get_status_code(app: Router, path: &str) -> axum::http::StatusCode {
        use tower::ServiceExt;

        app.oneshot(
            axum::http::Request::get(path)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    }

    #[tokio::test]
    async fn test_livez_ok() {
        let draining = Arc::new(AtomicBool::new(false));
        let app = probe_app(None, draining.clone());
        assert_eq!(
            get_status_code(app.clone(), "/livez").await,
            axum::http::StatusCode::OK
        );

        // Liveness is about the process, not the drain: it stays 200 while
        // draining so an orchestrator never kills a process mid-drain.
        draining.store(true, Ordering::SeqCst);
        assert_eq!(
            get_status_code(app, "/livez").await,
            axum::http::StatusCode::OK
        );
    }

    #[tokio::test]
    async fn test_readyz_flips_to_503_when_draining() {
        let draining = Arc::new(AtomicBool::new(false));
        let app = probe_app(None, draining.clone());
        assert_eq!(
            get_status_code(app.clone(), "/readyz").await,
            axum::http::StatusCode::OK
        );

        draining.store(true, Ordering::SeqCst);
        assert_eq!(
            get_status_code(app, "/readyz").await,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
    }

    /// Probes sit outside the bearer gate (orchestrators probe without
    /// credentials) while `/status` and `/metrics` stay behind it.
    #[tokio::test]
    async fn test_probes_unauthenticated_when_auth_token_set() {
        let app = probe_app(Some("tok".to_string()), Arc::new(AtomicBool::new(false)));
        assert_eq!(
            get_status_code(app.clone(), "/readyz").await,
            axum::http::StatusCode::OK
        );
        assert_eq!(
            get_status_code(app.clone(), "/livez").await,
            axum::http::StatusCode::OK
        );
        assert_eq!(
            get_status_code(app.clone(), "/status").await,
            axum::http::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get_status_code(app, "/metrics").await,
            axum::http::StatusCode::UNAUTHORIZED
        );
    }

    /// Drain timing for end-to-end tests: no lame duck, a grace long enough
    /// that hitting it fails the `run_until` deadline checks.
    const TEST_TIMING: ShutdownTiming = ShutdownTiming {
        lame_duck: Duration::ZERO,
        grace: Duration::from_secs(5),
    };

    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Poll `cond` every 10 ms until it holds, failing after `within`.
    async fn wait_until(within: Duration, what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + within;
        while !cond() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Shutdown signals injected by a test: every `send` is one signal; with
    /// every sender dropped, no further signal ever arrives.
    struct TestSignals(mpsc::UnboundedReceiver<ShutdownCause>);

    impl ShutdownSignals for TestSignals {
        async fn recv(&mut self) -> ShutdownCause {
            match self.0.recv().await {
                Some(cause) => cause,
                None => std::future::pending().await,
            }
        }
    }

    fn test_signals() -> (mpsc::UnboundedSender<ShutdownCause>, TestSignals) {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, TestSignals(rx))
    }

    /// Signals that deliver one `CtrlC` at once, then nothing.
    fn immediate_shutdown() -> TestSignals {
        let (tx, signals) = test_signals();
        tx.send(ShutdownCause::CtrlC).unwrap();
        signals
    }

    /// A live `WebhookUpdate("/hook")` -> `LongPollRoute("/poll")` instance on
    /// a free port; every [`HookToPoll::fire`] delivers one shutdown signal.
    struct HookToPoll {
        base: String,
        client: reqwest::Client,
        route: Arc<crate::route::longpull::LongPollRoute>,
        signals: mpsc::UnboundedSender<ShutdownCause>,
        run: tokio::task::JoinHandle<std::io::Result<ShutdownOutcome>>,
    }

    impl HookToPoll {
        async fn start(timing: ShutdownTiming) -> Self {
            Self::start_with_root(timing, |poll| poll).await
        }

        /// As [`HookToPoll::start`], with the routing tree built around the
        /// `/poll` leaf by `root`.
        async fn start_with_root(
            timing: ShutdownTiming,
            root: impl FnOnce(Arc<crate::route::longpull::LongPollRoute>) -> Arc<dyn RouteableComponent>,
        ) -> Self {
            use crate::route::longpull::LongPollRoute;
            use crate::update::webhook::WebhookUpdate;

            let port = free_port();
            let route = Arc::new(LongPollRoute::new("/poll".to_string()));
            let updates: Vec<Box<dyn UpdaterComponent>> =
                vec![Box::new(WebhookUpdate::new("/hook".to_string()))];
            let tgin = Tgin::new(updates, root(route.clone()), 2, Some(port));

            let (signals, injected) = test_signals();
            let run = tokio::spawn(tgin.run_until(injected, timing));

            let base = format!("http://127.0.0.1:{port}");
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let up = client
                    .get(format!("{base}/livez"))
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success());
                if up {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "listener never came up"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            Self {
                base,
                client,
                route,
                signals,
                run,
            }
        }

        async fn post_update(&self, update_id: u64) -> reqwest::Response {
            self.client
                .post(format!("{}/hook", self.base))
                .json(&json!({ "update_id": update_id }))
                .send()
                .await
                .unwrap()
        }

        async fn poll(&self, timeout: u64) -> Value {
            self.client
                .post(format!("{}/poll", self.base))
                .json(&json!({ "timeout": timeout }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap()
        }

        /// Wait until the long-poll route buffers exactly `n` updates.
        async fn wait_buffered(&self, n: usize) {
            let route = self.route.clone();
            wait_until(Duration::from_secs(2), "updates to reach the route", || {
                route.pending() == n
            })
            .await;
        }

        /// Deliver one shutdown signal; the first starts the drain, a second
        /// forces exit.
        fn fire(&mut self) {
            self.signals.send(ShutdownCause::CtrlC).unwrap();
        }

        /// Assert `run_until` returns `Ok` within `within`, and return how
        /// it ended.
        async fn assert_exits_within(self, within: Duration) -> ShutdownOutcome {
            tokio::time::timeout(within, self.run)
                .await
                .expect("run_until did not return in time")
                .expect("run_until panicked")
                .expect("run_until returned an error")
        }

        /// Stop the instance without waiting for its drain.
        fn abort(self) {
            self.run.abort();
        }
    }

    /// An update acknowledged to Telegram (200 on the webhook) and buffered
    /// for a long-poll consumer is still served after shutdown fires: the
    /// HTTP plane stays up until the buffer is drained, then the process
    /// exits promptly instead of waiting out the grace.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_shutdown_serves_buffered_updates_before_exit() {
        let mut app = HookToPoll::start(TEST_TIMING).await;

        assert_eq!(app.post_update(1).await.status(), reqwest::StatusCode::OK);
        app.wait_buffered(1).await;

        app.fire();

        let body = app.poll(0).await;
        assert_eq!(body, json!({ "ok": true, "result": [{ "update_id": 1 }] }));

        assert_eq!(
            app.assert_exits_within(Duration::from_secs(2)).await,
            ShutdownOutcome::Completed { abandoned: 0 }
        );
    }

    /// Once ingest is cancelled, webhook ingress answers 503 + `Retry-After`
    /// so Telegram redelivers later instead of the update being acknowledged
    /// into a process that is about to exit. Every update acknowledged with
    /// 200 is still served.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_webhook_ingress_rejects_while_draining() {
        let mut app = HookToPoll::start(TEST_TIMING).await;

        assert_eq!(app.post_update(1).await.status(), reqwest::StatusCode::OK);
        app.wait_buffered(1).await;

        app.fire();

        // With no lame duck, ingest is cancelled right after the cause; an
        // update accepted before that is acknowledged and must be served.
        // The plane stays open meanwhile: update 1 is still buffered.
        let mut accepted = vec![1];
        let rejected = loop {
            let update_id = accepted.len() as u64 + 1;
            let resp = app.post_update(update_id).await;
            if resp.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                break resp;
            }
            assert_eq!(resp.status(), reqwest::StatusCode::OK);
            accepted.push(update_id);
            assert!(accepted.len() < 1000, "webhook never started rejecting");
        };
        assert_eq!(
            rejected
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .unwrap(),
            "5"
        );

        // A 200 means the update was queued before ingest stopped; wait for
        // all of them to reach the route, then fetch them in one batch.
        app.wait_buffered(accepted.len()).await;
        let body = app.poll(0).await;
        let served: Vec<u64> = body["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["update_id"].as_u64().unwrap())
            .collect();
        assert_eq!(served, accepted);

        assert_eq!(
            app.assert_exits_within(Duration::from_secs(2)).await,
            ShutdownOutcome::Completed { abandoned: 0 }
        );
    }

    /// During the lame duck `/readyz` already answers 503, but webhook
    /// ingress keeps accepting and long-poll routes keep serving: load
    /// balancers are still moving traffic away. The lame duck is long enough
    /// that the checks below never race its end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_webhook_ingress_accepts_during_lame_duck() {
        let mut app = HookToPoll::start(ShutdownTiming {
            lame_duck: Duration::from_secs(60),
            ..TEST_TIMING
        })
        .await;

        app.fire();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let status = app
                .client
                .get(format!("{}/readyz", app.base))
                .send()
                .await
                .unwrap()
                .status();
            if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "/readyz never flipped"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(app.post_update(1).await.status(), reqwest::StatusCode::OK);
        app.wait_buffered(1).await;

        let body = app.poll(0).await;
        assert_eq!(body["result"][0], json!({ "update_id": 1 }));

        app.abort();
    }

    /// A consumer long-polling an empty buffer is answered with an empty
    /// batch shortly after the drain starts, so it neither pins the shutdown
    /// for its full timeout nor sees a dropped connection.
    ///
    /// Independent of when the poll arrives: a sibling leaf under an `AllLB`
    /// holds one update nobody fetches until the end of the test, which keeps
    /// the HTTP plane open (grace 60 s). A poll that parks before the drain
    /// is woken by `begin_drain` (the usual case, given the head start
    /// below); one arriving after it is held only briefly. Either way the
    /// answer is `[]` well before the poll's 30 s timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_parked_poll_returns_empty_on_drain() {
        use crate::lb::all::AllLB;
        use crate::route::longpull::LongPollRoute;

        let keep = Arc::new(LongPollRoute::new("/keep".to_string()));
        keep.process(update_bytes(json!({ "update_id": 1 }))).await;
        let timing = ShutdownTiming {
            lame_duck: Duration::ZERO,
            grace: Duration::from_secs(60),
        };
        let mut app = HookToPoll::start_with_root(timing, |poll| {
            Arc::new(AllLB::new(vec![poll, keep.clone()]))
        })
        .await;

        let parked = {
            let client = app.client.clone();
            let url = format!("{}/poll", app.base);
            tokio::spawn(async move {
                client
                    .post(url)
                    .json(&json!({ "timeout": 30 }))
                    .send()
                    .await
                    .unwrap()
                    .json::<Value>()
                    .await
                    .unwrap()
            })
        };
        // Head start so the poll usually parks before the drain; correctness
        // does not depend on it.
        tokio::time::sleep(Duration::from_millis(100)).await;

        app.fire();

        let body = tokio::time::timeout(Duration::from_secs(10), parked)
            .await
            .expect("parked poll was held toward its 30 s timeout during the drain")
            .unwrap();
        assert_eq!(body, json!({ "ok": true, "result": [] }));

        // Fetching the last pending update ends the drain.
        let rest: Value = app
            .client
            .post(format!("{}/keep", app.base))
            .json(&json!({ "timeout": 0 }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(rest["result"], json!([{ "update_id": 1 }]));

        assert_eq!(
            app.assert_exits_within(Duration::from_secs(3)).await,
            ShutdownOutcome::Completed { abandoned: 0 }
        );
    }

    /// A downstream that accepts connections and never answers; every
    /// connection stays open until the returned task is aborted.
    async fn silent_downstream() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/bot", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((conn, _)) = listener.accept().await {
                held.push(conn);
            }
        });
        (url, task)
    }

    /// Once the HTTP plane has closed, the workers get only the rest of the
    /// grace (at least `HTTP_CLOSE_GRACE`): a webhook downstream that never
    /// answers cannot hold the process for its 60 s request timeout per
    /// update. Dispatches still in flight at the deadline are aborted and,
    /// with the updates still queued, reported as abandoned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_drain_deadline_bounds_hung_webhook_downstream() {
        use crate::route::webhook::WebhookRoute;

        const UPDATES: u64 = 5;
        let (url, downstream) = silent_downstream().await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let hung: Arc<dyn RouteableComponent> =
            Arc::new(WebhookRoute::new(url, client, Duration::from_secs(60)));
        let timing = ShutdownTiming {
            lame_duck: Duration::ZERO,
            grace: Duration::from_secs(1),
        };
        let mut app = HookToPoll::start_with_root(timing, |_poll| hung).await;

        // Each 200 means the update is in the dispatch queue; two workers
        // take one each into the hung downstream, the rest stay queued.
        for update_id in 1..=UPDATES {
            assert_eq!(
                app.post_update(update_id).await.status(),
                reqwest::StatusCode::OK
            );
        }

        app.fire();

        let outcome = app
            .assert_exits_within(timing.grace + HTTP_CLOSE_GRACE + Duration::from_secs(2))
            .await;
        assert_eq!(
            outcome,
            ShutdownOutcome::Completed {
                abandoned: UPDATES as usize
            }
        );
        downstream.abort();
    }

    /// A second signal once shutdown has started skips the rest of the drain:
    /// `run_until` returns `Forced` at once, although a buffered update nobody
    /// fetches would otherwise hold the HTTP plane open for the 60 s grace.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_second_signal_forces_exit() {
        let mut app = HookToPoll::start(ShutdownTiming {
            lame_duck: Duration::ZERO,
            grace: Duration::from_secs(60),
        })
        .await;

        assert_eq!(app.post_update(1).await.status(), reqwest::StatusCode::OK);
        app.wait_buffered(1).await;

        // Signals are read in order: the first starts the drain, the second
        // arrives while it waits for the buffer.
        app.fire();
        app.fire();

        assert_eq!(
            app.assert_exits_within(Duration::from_secs(2)).await,
            ShutdownOutcome::Forced
        );
    }

    /// Holds its sender until ingest is cancelled, like a long-poll updater
    /// with nothing to fetch.
    struct IdleUpdater;

    #[async_trait]
    impl crate::update::base::Updater for IdleUpdater {
        async fn start(&self, _tx: Sender<Bytes>, ingest: CancellationToken) {
            ingest.cancelled().await;
        }
    }

    impl Serverable for IdleUpdater {}
    impl Printable for IdleUpdater {}

    /// Without an HTTP plane there is no traffic to move away and nothing to
    /// serve buffered updates, so shutdown skips the lame duck and the
    /// pending wait even with the default timing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_shutdown_without_http_plane_is_immediate() {
        use crate::route::longpull::LongPollRoute;

        let route = Arc::new(LongPollRoute::new("/poll".to_string()));
        route.process(update_bytes(json!({ "update_id": 1 }))).await;
        let tgin = Tgin::new(vec![Box::new(IdleUpdater)], route, 1, None);

        tokio::time::timeout(
            Duration::from_secs(1),
            tgin.run_until(immediate_shutdown(), ShutdownTiming::default()),
        )
        .await
        .expect("server-less shutdown waited for the lame duck or the backlog")
        .expect("run_until returned an error");
    }

    /// With only long-poll ingress and every updater stopped on a permanent
    /// failure, nothing can deliver updates any more: the process shuts
    /// itself down (and `main` maps the counter to exit code 2) instead of
    /// idling forever behind a still-open HTTP listener.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_all_pollers_permanently_failed_triggers_exit() {
        use crate::mock::routes::MockCallsRoute;
        use crate::update::longpull::LongPollUpdate;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let telegram = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/botMYTOKEN/getUpdates"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&telegram)
            .await;

        let failures = Arc::new(AtomicUsize::new(0));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut updater = LongPollUpdate::new("MYTOKEN".to_string(), client);
        updater.set_url(format!("{}/botMYTOKEN/getUpdates", telegram.uri()));
        updater.set_health_counter(failures.clone());

        let route: Arc<dyn RouteableComponent> = Arc::new(MockCallsRoute::new("sink"));
        let mut tgin = Tgin::new(vec![Box::new(updater)], route, 2, Some(free_port()));
        tgin.set_ingress_health(failures.clone(), 1, false);

        tokio::time::timeout(
            Duration::from_secs(3),
            tgin.run_until(test_signals().1, TEST_TIMING),
        )
        .await
        .expect("run_until kept running after every ingress failed")
        .expect("run_until returned an error");

        assert_eq!(failures.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_ingress_health_all_failed() {
        let health = |failed: usize, long_poll_updaters: usize, has_webhook_ingress: bool| {
            IngressHealth {
                permanent_failures: Arc::new(AtomicUsize::new(failed)),
                long_poll_updaters,
                has_webhook_ingress,
            }
            .all_failed()
        };
        assert!(health(1, 1, false));
        assert!(health(2, 2, false));
        assert!(!health(1, 2, false), "one poller still running");
        assert!(!health(1, 1, true), "webhook ingress can still deliver");
        assert!(!health(0, 0, false), "no long-poll updaters to fail");
    }

    #[test]
    fn test_shutdown_timing_for_cause() {
        let timing = ShutdownTiming::default();
        assert_eq!(
            timing,
            ShutdownTiming {
                lame_duck: Duration::from_secs(5),
                grace: Duration::from_secs(15),
            }
        );
        assert_eq!(timing.for_cause(ShutdownCause::CtrlC), timing);
        assert_eq!(timing.for_cause(ShutdownCause::AllIngressFailed), timing);
        #[cfg(windows)]
        {
            assert_eq!(
                timing.for_cause(ShutdownCause::CtrlClose),
                ShutdownTiming {
                    lame_duck: Duration::ZERO,
                    grace: Duration::from_secs(4),
                }
            );
            let short = ShutdownTiming {
                lame_duck: Duration::from_secs(1),
                grace: Duration::from_secs(2),
            };
            assert_eq!(
                short.for_cause(ShutdownCause::CtrlClose).grace,
                Duration::from_secs(2)
            );
        }
    }

    #[test]
    fn test_drain_deadline() {
        let start = Instant::now();
        let grace = Duration::from_secs(15);
        let ms = Duration::from_millis;
        // Backlog drained early: the workers keep the rest of the grace.
        assert_eq!(
            drain_deadline(start, grace, start + ms(2000)),
            start + grace
        );
        // Grace spent: the workers get `HTTP_CLOSE_GRACE` from the close.
        assert_eq!(
            drain_deadline(start, grace, start + grace),
            start + grace + HTTP_CLOSE_GRACE
        );
        // Close shortly before the end of the grace: the floor still holds.
        assert_eq!(
            drain_deadline(start, grace, start + grace - ms(400)),
            start + grace + ms(600)
        );
    }
}
