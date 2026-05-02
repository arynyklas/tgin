use crate::api::message::{ApiError, ApiMessage};
use crate::api::router::Api;
use crate::base::{RouteableComponent, Serverable, UpdaterComponent};

use axum::Router;
use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;

use axum_server::tls_rustls::RustlsConfig;
use std::net::SocketAddr;

use tokio::runtime::Builder;

use crate::dynamic::handler::dynamic_handler;

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

    pub fn run(self) {
        let runtime = Builder::new_multi_thread()
            .worker_threads(self.dark_threads)
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime");
        runtime.block_on(async {
            println!("STARTED TGIN with {} worker threads\n", &self.dark_threads);

            println!("CATCH UPDATES FROM\n");

            for update in &self.updates {
                println!("{}\n", update.print().await);
            }

            println!("\nRUTE TO\n");

            println!("{}", &self.route.print().await);
        });

        runtime.block_on(self.run_async());
    }

    pub async fn run_async(self) {
        // Bounded buffer: a small queue is flow control. The previous
        // 1_000_000-slot channel was not — it was a runway for OOM that
        // hid downstream slowness from producers until memory ran out.
        // With a bounded buffer, a slow downstream backpressures producers
        // (LongPoll stops calling `getUpdates`; webhook handlers hold open)
        // and overload becomes immediately visible.
        let (tx, rx) = mpsc::channel::<Bytes>(UPDATE_CHANNEL_CAPACITY);

        let api = self.api;

        if let Some(port) = self.server_port {
            let mut router: Router<Sender<Bytes>> = Router::new();

            for provider in &self.updates {
                router = provider.set_server(router).await;
            }

            router = self.route.set_server(router).await;

            if let Some(ref api) = api {
                router = api.set_server(router).await;
            }

            let app = router.with_state(tx.clone());

            let app = if api.is_some() {
                app.fallback(dynamic_handler)
            } else {
                app
            };

            let addr = SocketAddr::from(([0, 0, 0, 0], port));

            match (self.ssl_cert.clone(), self.ssl_key.clone()) {
                (Some(cert_path), Some(key_path)) => {
                    let config = RustlsConfig::from_pem_file(cert_path, key_path)
                        .await
                        .expect("Failed to load SSL certificates");

                    tokio::spawn(async move {
                        axum_server::bind_rustls(addr, config)
                            .serve(app.into_make_service())
                            .await
                            .unwrap();
                    });
                }
                _ => {
                    tokio::spawn(async move {
                        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
                        axum::serve(listener, app).await.unwrap();
                    });
                }
            }
        }

        for provider in self.updates {
            let tx_clone = tx.clone();
            tokio::spawn(async move {
                provider.start(tx_clone).await;
            });
        }

        // Drop the dispatcher's own sender so the channel closes cleanly
        // once every producer task ends — that signals workers to drain and
        // exit.
        drop(tx);

        // N long-lived consumers replace the previous `spawn`-per-update
        // pattern. Each worker loops on `rx.recv()` and `await`s
        // `route.process(...)` directly: the routing tree is the unit of
        // concurrency, not the individual update. This caps in-flight work
        // at `dark_threads` regardless of arrival rate, so a slow downstream
        // can no longer grow the task pool unboundedly.
        //
        // The receiver is shared via a `tokio::sync::Mutex`. The lock is
        // held only across `recv().await` (a single tokio primitive op);
        // contention is brief and bounded by N. While one worker is parked
        // in `recv`, the other N-1 can be processing — the mutex serialises
        // _picking_ items, not _doing_ work.
        let worker_count = self.dark_threads.max(1);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let rx = rx.clone();
            let route = self.route.clone();
            workers.push(tokio::spawn(async move {
                loop {
                    let next = {
                        let mut guard = rx.lock().await;
                        guard.recv().await
                    };
                    match next {
                        Some(update) => {
                            // `Bytes::clone` inside the routing tree is
                            // one atomic op; no `Value` allocation, no
                            // re-serialization on egress.
                            route.process(update).await;
                        }
                        None => return,
                    }
                }
            }));
        }

        match api {
            None => {
                // Workers exit when the channel closes (every ingress
                // producer dropped its `Sender` clone). Wait for clean
                // drain so the runtime doesn't tear down mid-dispatch.
                for w in workers {
                    let _ = w.await;
                }
            }

            Some(mut api) => {
                // Control plane runs alongside the worker pool. Updates do
                // not flow through this loop anymore — workers own the
                // update channel.
                while let Some(api_msg) = api.rx.recv().await {
                    match api_msg {
                        ApiMessage::GetRoutes(tx_response) => {
                            let _ = tx_response.send(self.route.json_struct().await);
                        }

                        // Run synchronously in the API loop and plumb the
                        // result back through the oneshot. Mutation is
                        // sub-millisecond (`arc_swap::rcu` over a `Vec`) so
                        // there is no latency case for spawning, and
                        // synchronous handling is what lets the HTTP
                        // handler turn the trait-level `Result<(), ()>`
                        // into an honest 200/4xx/5xx.
                        ApiMessage::AddRoute { route, resp } => {
                            let result = self
                                .route
                                .add_route(route)
                                .await
                                .map_err(|()| {
                                    ApiError::Conflict(
                                        "top-level route does not accept dynamic children"
                                            .into(),
                                    )
                                });
                            let _ = resp.send(result);
                        }

                        // Symmetric with AddRoute: the previous
                        // `tokio::spawn` made `add` then `rm` racy and
                        // hid "target absent" behind a 200. Running in
                        // the loop preserves caller ordering and lets
                        // `Routeable::remove_route`'s `Err(())` surface
                        // as a 404.
                        ApiMessage::RmRoute { target, resp } => {
                            let result = self
                                .route
                                .remove_route(target)
                                .await
                                .map_err(|()| {
                                    ApiError::NotFound(
                                        "no route in the tree matched the supplied target"
                                            .into(),
                                    )
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
    }
}

/// Bounded capacity for the update mpsc. Sized to absorb short bursts
/// without giving slow downstreams a runway to OOM. The previous
/// 1_000_000-slot buffer was a buffer, not flow control; with this cap,
/// producers see backpressure immediately when the worker pool falls
/// behind.
const UPDATE_CHANNEL_CAPACITY: usize = 1024;

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
        let workers = spawn_workers(rx, route_dyn, workers_n);

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
        let workers = spawn_workers(rx, route_dyn.clone(), 4);

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
        let workers = spawn_workers(rx, route_dyn, workers_n);

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

    /// Test-only mirror of the worker-spawning logic in `run_async`.
    /// Exists so unit tests can drive the dispatch surface without
    /// standing up a full `Tgin` (which would also require ingress
    /// adapters, an axum router, and a port).
    fn spawn_workers(
        rx: mpsc::Receiver<Bytes>,
        route: Arc<dyn RouteableComponent>,
        worker_count: usize,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        (0..worker_count.max(1))
            .map(|_| {
                let rx = rx.clone();
                let route = route.clone();
                tokio::spawn(async move {
                    loop {
                        let next = {
                            let mut guard = rx.lock().await;
                            guard.recv().await
                        };
                        match next {
                            Some(update) => {
                                route.process(update).await;
                            }
                            None => return,
                        }
                    }
                })
            })
            .collect()
    }

    /// Helper for tests that want a `Bytes` payload from a `serde_json::Value`.
    fn update_bytes(value: Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&value).unwrap())
    }
}
