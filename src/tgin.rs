use crate::api::message::{ApiError, ApiMessage};
use crate::api::router::Api;
use crate::base::{AddRouteError, RemoveRouteError, RouteableComponent, Serverable, UpdaterComponent};

use axum::Router;
use axum::routing::get;
use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;

use axum_server::tls_rustls::RustlsConfig;
use axum_server::Handle;
use std::net::SocketAddr;
use std::time::Duration;
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
        }
    }

    pub fn set_api(&mut self, api: Api) {
        self.api = Some(api);
    }

    pub fn set_ssl(&mut self, ssl_cert: String, ssl_key: String) {
        self.ssl_cert = Some(ssl_cert);
        self.ssl_key = Some(ssl_key);
    }

    pub fn run(self) -> std::io::Result<()> {
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

        runtime.block_on(self.run_async())
    }

    pub async fn run_async(self) -> std::io::Result<()> {
        // Bounded buffer: a small queue is flow control. The previous
        // 1_000_000-slot channel was not — it was a runway for OOM that
        // hid downstream slowness from producers until memory ran out.
        // With a bounded buffer, a slow downstream backpressures producers
        // (LongPoll stops calling `getUpdates`; webhook handlers hold open)
        // and overload becomes immediately visible.
        let (tx, rx) = mpsc::channel::<Bytes>(UPDATE_CHANNEL_CAPACITY);

        crate::observe::set_route(self.route.clone());

        let api = self.api;

        let shutdown = CancellationToken::new();
        // Released only after the HTTP server has finished its graceful drain,
        // so the worker pool and the control-plane loop keep consuming updates
        // produced by in-flight webhook handlers right up until the server is
        // done — then drain the backlog and exit.
        let drain = CancellationToken::new();

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

                // Always-on observability endpoints, independent of the
                // management API. Explicit routes take precedence over the
                // `dynamic_handler` fallback, and take no `State`, so they are
                // valid on `Router<Sender<Bytes>>`.
                router = router
                    .route("/status", get(crate::observe::status_handler))
                    .route("/metrics", get(crate::observe::metrics_handler));

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

        let (server_handle, server_task) = match server {
            Some((handle, task)) => (Some(handle), Some(task)),
            None => (None, None),
        };

        for provider in self.updates {
            if let Some(m) = provider.metrics() {
                crate::observe::register_updater(m);
            }
            let tx_clone = tx.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                provider.start(tx_clone, shutdown).await;
            });
        }

        // Graceful shutdown coordinator. On SIGTERM / Ctrl-C:
        //   1. cancel `shutdown` so long-poll ingest stops pulling new updates
        //      from Telegram;
        //   2. start the HTTP server's bounded graceful drain and wait for it
        //      to finish, so in-flight requests complete and no further
        //      webhook updates are produced;
        //   3. cancel `drain` so the worker pool and control-plane loop finish
        //      the buffered backlog and exit, letting `run_async` return.
        // The worker pool deliberately outlives the HTTP drain so updates
        // pushed by draining webhook handlers are still consumed, not dropped.
        {
            let shutdown = shutdown.clone();
            let drain = drain.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                tracing::info!("shutdown signal received; draining");
                shutdown.cancel();
                if let Some(handle) = server_handle {
                    handle.graceful_shutdown(Some(SHUTDOWN_GRACE));
                }
                if let Some(task) = server_task {
                    let _ = task.await;
                }
                drain.cancel();
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
        // backlog and exit. They never wait on every `Sender` clone dropping,
        // which the axum router does not guarantee on shutdown.
        let workers = spawn_worker_pool(rx, self.route.clone(), self.dark_threads, drain.clone());

        match api {
            None => {
                // Workers exit when the channel closes (every ingress
                // producer dropped its `Sender` clone). Wait for clean
                // drain so the runtime doesn't tear down mid-dispatch.
                for w in workers {
                    let _ = w.await;
                }
            }

            Some(api) => {
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
                            let _ = tx_response.send(self.route.json_struct().await);
                        }

                        // Run synchronously in the API loop and plumb the
                        // result back through the oneshot. Mutation is
                        // sub-millisecond (`arc_swap::rcu` over a `Vec`) so
                        // there is no latency case for spawning, and
                        // synchronous handling is what lets the trait-level
                        // `AddRouteError` surface as an honest 4xx instead of
                        // a silent 200.
                        ApiMessage::AddRoute { route, resp } => {
                            let result = self.route.add_route(route).await.map_err(|err| {
                                match err {
                                    AddRouteError::Conflict => ApiError::Conflict(
                                        "top-level route does not accept dynamic children"
                                            .into(),
                                    ),
                                }
                            });
                            let _ = resp.send(result);
                        }

                        // Symmetric with AddRoute: the previous
                        // `tokio::spawn` made `add` then `rm` racy and
                        // hid "target absent" behind a 200. Running in
                        // the loop preserves caller ordering and lets
                        // `RemoveRouteError::NotFound` surface as a 404.
                        ApiMessage::RmRoute { target, resp } => {
                            let result = self.route.remove_route(target).await.map_err(|err| {
                                match err {
                                    RemoveRouteError::NotFound => ApiError::NotFound(
                                        "no route in the tree matched the supplied target"
                                            .into(),
                                    ),
                                }
                            });
                            let _ = resp.send(result);
                        }
                    }
                }

                for w in workers {
                    let _ = w.await;
                }
            }
        }

        Ok(())
    }
}

/// Bounded capacity for the update mpsc. Sized to absorb short bursts
/// without giving slow downstreams a runway to OOM. The previous
/// 1_000_000-slot buffer was a buffer, not flow control; with this cap,
/// producers see backpressure immediately when the worker pool falls
/// behind.
const UPDATE_CHANNEL_CAPACITY: usize = 1024;

/// Hard deadline for draining in-flight HTTP connections on graceful
/// shutdown. Bounds the wait so a parked long-poll downstream connection or
/// a wedged request cannot block exit indefinitely, while leaving headroom
/// under the common 30 s orchestrator (k8s/systemd) kill window.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Resolves on the first SIGTERM (Unix) or Ctrl-C (all platforms). On
/// Windows there is no SIGTERM, so only Ctrl-C arms.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Spawn `worker_count` (min 1) consumers over a shared receiver. Each worker
/// pulls one update at a time and runs `route.process` to completion before
/// taking the next, so in-flight work is capped at `worker_count`.
///
/// Termination is deterministic and does not rely on every `Sender` clone
/// being dropped (the axum router retains state-sender clones past its own
/// drop): a worker stops when the channel closes (clean no-signal path) or
/// when `shutdown` is cancelled. On cancellation the biased `recv` still wins
/// whenever an update is ready, so a backlog keeps draining; the worker exits
/// only once the buffer is empty.
fn spawn_worker_pool(
    rx: mpsc::Receiver<Bytes>,
    route: Arc<dyn RouteableComponent>,
    worker_count: usize,
    shutdown: CancellationToken,
) -> Vec<tokio::task::JoinHandle<()>> {
    let worker_count = worker_count.max(1);
    // Shared via a `tokio::sync::Mutex`: the guard is held only across the
    // pick (`recv`), never across `route.process`, so N-1 workers can be
    // processing while one is parked waiting for the next update.
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let rx = rx.clone();
        let route = route.clone();
        let shutdown = shutdown.clone();
        workers.push(tokio::spawn(async move {
            loop {
                let next = {
                    let mut guard = rx.lock().await;
                    tokio::select! {
                        biased;
                        msg = guard.recv() => msg,
                        // Cancelled: drain whatever is buffered (one per
                        // iteration via the biased `recv` above); `try_recv`
                        // yields `None` once empty, ending the worker.
                        _ = shutdown.cancelled() => guard.try_recv().ok(),
                    }
                };
                match next {
                    // `Bytes::clone` inside the routing tree is one atomic op;
                    // no `Value` allocation, no re-serialization on egress.
                    Some(update) => {
                        let span = crate::observe::dispatch_span(&update);
                        route.process(update).instrument(span).await;
                    }
                    None => return,
                }
            }
        }));
    }
    workers
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
        let workers = spawn_worker_pool(rx, route_dyn, workers_n, CancellationToken::new());

        for i in 0..workers_n {
            tx.send(update_bytes(json!({ "update_id": i }))).await.unwrap();
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
        for w in workers {
            let _ = w.await;
        }
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
        let workers = spawn_worker_pool(rx, route_dyn.clone(), 4, CancellationToken::new());

        for i in 0..32 {
            tx.send(update_bytes(json!({ "update_id": i }))).await.unwrap();
        }
        drop(tx);

        for w in workers {
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
        let workers = spawn_worker_pool(rx, route_dyn, workers_n, CancellationToken::new());

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
        for w in workers {
            let _ = w.await;
        }
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
        let workers = spawn_worker_pool(rx, route_dyn, 4, shutdown.clone());

        for i in 0..16 {
            tx.send(update_bytes(json!({ "update_id": i }))).await.unwrap();
        }

        // Cancel while `tx` is still held open: proves workers stop on the
        // token, not on the channel closing.
        shutdown.cancel();

        for w in workers {
            tokio::time::timeout(std::time::Duration::from_secs(5), w)
                .await
                .expect("worker did not exit within 5s of shutdown despite a live sender")
                .expect("worker task panicked");
        }

        // Every buffered update was drained before the workers exited.
        assert_eq!(route.count().await, 16);
        drop(tx);
    }
}
