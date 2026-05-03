use crate::base::{AddRouteError, Printable, RemoveRouteError, RouteId, Routeable, RouteableComponent, Serverable};

use axum::Router;
use bytes::Bytes;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinSet;

use arc_swap::ArcSwap;

use std::sync::Arc;

use crate::api::message::AddRouteType;
use crate::dynamic::longpoll_registry;

use async_trait::async_trait;

use serde_json::{json, Value};

/// See [`crate::lb::roundrobin::Routes`].
type Routes = Vec<Arc<dyn RouteableComponent>>;

pub struct AllLB {
    /// Lock-free children list — see `RoundRobinLB::routes`.
    routes: ArcSwap<Routes>,
}

impl AllLB {
    pub fn new(routes: Routes) -> Self {
        Self {
            routes: ArcSwap::from_pointee(routes),
        }
    }
}

#[async_trait]
impl Routeable for AllLB {
    async fn process(&self, update: Bytes) {
        let routes = self.routes.load();
        if routes.is_empty() {
            // Static config rejects empty LBs at startup, but the API can
            // drain a runtime LB via `remove_route`. Surface the drop so
            // the operator sees updates being silently swallowed.
            eprintln!(
                "warning: dropping update at empty AllLB (LB drained at runtime)"
            );
            return;
        }
        // Snapshot the children so the `ArcSwap` read guard is released
        // before any downstream I/O. `Bytes::clone` is one atomic ref-count
        // bump per child: every spawned dispatcher receives the same
        // shared payload with no per-child copy.
        let children: Vec<Arc<dyn RouteableComponent>> =
            routes.iter().cloned().collect();
        drop(routes);

        let mut set: JoinSet<()> = JoinSet::new();
        for route in children {
            let update = update.clone();
            set.spawn(async move {
                route.process(update).await;
            });
        }

        // Drain before returning. The calling worker stays parked here
        // until every child completes, so the worker-pool bound
        // (`dark_threads` in `Tgin::run_async`) caps the number of
        // concurrent fanouts. Panics in children surface as `JoinError`
        // and MUST NOT take down sibling fanouts or the calling worker —
        // log and continue draining.
        while let Some(res) = set.join_next().await {
            if let Err(e) = res {
                if e.is_panic() {
                    eprintln!("AllLB child panicked during fanout: {e}");
                }
                // `JoinError::is_cancelled` only fires when the `JoinSet`
                // is dropped mid-flight, which we don't do during drain.
            }
        }
    }

    async fn add_route(&self, route: AddRouteType) -> Result<(), AddRouteError> {
        match route {
            AddRouteType::Longpull(route_arc) => {
                longpoll_registry::insert(route_arc.path.clone(), route_arc.clone());
                self.routes.rcu(|cur| {
                    let mut next: Routes = (**cur).clone();
                    next.push(route_arc.clone());
                    Arc::new(next)
                });
                Ok(())
            }
            AddRouteType::Webhook(route) => {
                self.routes.rcu(|cur| {
                    let mut next: Routes = (**cur).clone();
                    next.push(route.clone());
                    Arc::new(next)
                });
                Ok(())
            }
        }
    }

    async fn remove_route(&self, target: RouteId) -> Result<(), RemoveRouteError> {
        if let RouteId::Path(path) = &target {
            longpoll_registry::remove(path);
        }

        let mut removed_locally = false;
        self.routes.rcu(|cur| {
            let mut next: Routes = (**cur).clone();
            let before = next.len();
            next.retain(|r| r.id().as_ref() != Some(&target));
            removed_locally = next.len() < before;
            Arc::new(next)
        });

        if removed_locally {
            return Ok(());
        }

        // Recurse into nested LBs. `AllLB` may legitimately host the same
        // route under multiple sub-trees (broadcast semantics), so we do
        // NOT break on first hit — every owner must be told to drop it.
        let routes = self.routes.load_full();
        let mut child_removed = false;
        for child in routes.iter().filter(|r| r.id().is_none()) {
            if child.remove_route(target.clone()).await.is_ok() {
                child_removed = true;
            }
        }

        if child_removed {
            Ok(())
        } else {
            Err(RemoveRouteError::NotFound)
        }
    }
}

#[async_trait]
impl Serverable for AllLB {
    async fn set_server(&self, mut router: Router<Sender<Bytes>>) -> Router<Sender<Bytes>> {
        let routes = self.routes.load_full();
        for route in routes.iter() {
            router = route.set_server(router).await;
        }
        router
    }
}

#[async_trait]
impl Printable for AllLB {
    async fn print(&self) -> String {
        let routes = self.routes.load_full();
        let mut text = String::from("LOAD BALANCER All\n\n");

        for route in routes.iter() {
            text.push_str(&format!("{}\n\n", route.print().await));
        }
        text
    }

    async fn json_struct(&self) -> Value {
        let routes = self.routes.load_full();
        let mut routes_json: Vec<Value> = Vec::new();
        for route in routes.iter() {
            routes_json.push(route.json_struct().await);
        }

        json!({
            "type": "load-balancer",
            "name": "all",
            "routes": routes_json
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::routes::MockCallsRoute;
    use std::time::Duration;

    fn update_bytes(value: Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&value).unwrap())
    }

    #[tokio::test]
    async fn test_empty_routes_does_not_panic() {
        let lb = AllLB::new(vec![]);

        lb.process(update_bytes(json!({"update_id": 1}))).await;

        let json = lb.json_struct().await;
        assert_eq!(json["routes"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_all_broadcast_distribution() {
        let route1 = Arc::new(MockCallsRoute::new("A"));
        let route2 = Arc::new(MockCallsRoute::new("B"));

        let lb = AllLB::new(vec![route1.clone(), route2.clone()]);

        lb.process(update_bytes(json!({"msg": "hello"}))).await;

        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(route1.count().await, 1);
        assert_eq!(route2.count().await, 1);
        let c1 = route1.get_calls().await;
        let c2 = route2.get_calls().await;
        assert_eq!(c1[0]["msg"], "hello");
        assert_eq!(c2[0]["msg"], "hello");
    }

    #[tokio::test]
    async fn test_multiple_messages_broadcast() {
        let r1 = Arc::new(MockCallsRoute::new("1"));
        let r2 = Arc::new(MockCallsRoute::new("2"));
        let r3 = Arc::new(MockCallsRoute::new("3"));

        let lb = AllLB::new(vec![r1.clone(), r2.clone(), r3.clone()]);

        lb.process(update_bytes(json!("m1"))).await;
        lb.process(update_bytes(json!("m2"))).await;
        lb.process(update_bytes(json!("m3"))).await;

        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(r1.count().await, 3);
        assert_eq!(r2.count().await, 3);
        assert_eq!(r3.count().await, 3);
    }

    #[tokio::test]
    async fn test_json_structure() {
        let r1 = Arc::new(MockCallsRoute::new("x"));
        let lb = AllLB::new(vec![r1]);

        let output = lb.json_struct().await;

        assert_eq!(output["type"], "load-balancer");
        assert_eq!(output["name"], "all");
        assert_eq!(output["routes"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_dynamic_add_route_broadcast() {
        let r1 = Arc::new(MockCallsRoute::new("static"));
        let lb = AllLB::new(vec![r1.clone()]);

        lb.process(update_bytes(json!(1))).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(r1.count().await, 1);

        let r2 = Arc::new(MockCallsRoute::new("dynamic"));
        let add_res = lb.add_route(AddRouteType::Webhook(r2.clone())).await;
        assert!(add_res.is_ok());

        let json_out = lb.json_struct().await;
        assert_eq!(json_out["routes"].as_array().unwrap().len(), 2);

        lb.process(update_bytes(json!(2))).await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(r1.count().await, 2);
        assert_eq!(r2.count().await, 1);
    }

    /// `remove_route` must report `Err` when no descendant matches the
    /// target, and `Ok` when a nested LB owns the target. Mirrors the
    /// `RoundRobinLB` test for symmetry.
    #[tokio::test]
    async fn test_remove_route_aggregates_child_failure() {
        use crate::lb::roundrobin::RoundRobinLB;
        use crate::route::webhook::{WebhookRoute, DEFAULT_REQUEST_TIMEOUT};
        use reqwest::Client;

        let client = Client::builder().no_proxy().build().unwrap();
        let nested_target = "http://127.0.0.1:0/nested-all".to_string();
        let nested_route = Arc::new(WebhookRoute::new(
            nested_target.clone(),
            client,
            DEFAULT_REQUEST_TIMEOUT,
        ));
        let inner: Arc<dyn RouteableComponent> =
            Arc::new(RoundRobinLB::new(vec![nested_route]));

        let outer = AllLB::new(vec![inner]);

        let absent = outer
            .remove_route(RouteId::Url("http://does-not-exist-all".to_string()))
            .await;
        assert!(absent.is_err());

        let hit = outer.remove_route(RouteId::Url(nested_target)).await;
        assert!(hit.is_ok());
    }

    /// `process` MUST NOT return until every child has completed. With
    /// the previous `tokio::spawn` design, `process` returned immediately
    /// and children finished asynchronously — callers had to `sleep` to
    /// observe results. After the fix, no sleep is needed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_alllb_process_awaits_all_children() {
        let r1 = Arc::new(MockCallsRoute::new("a"));
        let r2 = Arc::new(MockCallsRoute::new("b"));
        let r3 = Arc::new(MockCallsRoute::new("c"));
        let lb = AllLB::new(vec![r1.clone(), r2.clone(), r3.clone()]);

        lb.process(update_bytes(json!({"k": 1}))).await;

        // No `sleep` — `process` MUST have awaited all children.
        assert_eq!(r1.count().await, 1);
        assert_eq!(r2.count().await, 1);
        assert_eq!(r3.count().await, 1);
    }

    /// A child panic MUST surface in logs but MUST NOT propagate to the
    /// caller or abort sibling fanouts. The calling worker survives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_alllb_panicking_child_does_not_kill_siblings() {
        struct PanickingRoute;
        #[async_trait]
        impl Routeable for PanickingRoute {
            async fn process(&self, _update: Bytes) {
                panic!("intentional test panic");
            }
        }
        impl Serverable for PanickingRoute {}
        impl Printable for PanickingRoute {}

        let survivor = Arc::new(MockCallsRoute::new("survivor"));
        let lb: AllLB = AllLB::new(vec![
            Arc::new(PanickingRoute) as Arc<dyn RouteableComponent>,
            survivor.clone(),
        ]);

        // MUST NOT propagate the child panic to the caller.
        lb.process(update_bytes(json!({"k": 1}))).await;

        // Sibling MUST have been delivered to.
        assert_eq!(survivor.count().await, 1);
    }

    /// Headline test: prove the worker-pool bound holds. Drive `AllLB`
    /// via the same shape used by `run_async` (a bounded mpsc fed by a
    /// fixed pool of workers calling `lb.process(...)`), with parking
    /// children, and assert that no probe ever sees more than
    /// `worker_count` concurrent calls — even when many more updates are
    /// queued. The OLD `tokio::spawn` design violates this: every queued
    /// update would spawn a child immediately, pushing per-probe
    /// in-flight up to `queued_updates`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_alllb_caps_concurrent_fanouts_at_worker_pool() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::{mpsc, Notify};

        struct ParkingProbe {
            in_flight: AtomicUsize,
            max_in_flight: AtomicUsize,
            gate: Notify,
            release: AtomicUsize,
        }
        #[async_trait]
        impl Routeable for ParkingProbe {
            async fn process(&self, _u: Bytes) {
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                loop {
                    if self.release.load(Ordering::SeqCst) > 0 {
                        self.release.fetch_sub(1, Ordering::SeqCst);
                        break;
                    }
                    self.gate.notified().await;
                }
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
            }
        }
        impl Serverable for ParkingProbe {}
        impl Printable for ParkingProbe {}

        let k = 3usize;
        let workers_n = 2usize;
        let queued_updates = 8usize;

        let probes: Vec<Arc<ParkingProbe>> = (0..k)
            .map(|_| {
                Arc::new(ParkingProbe {
                    in_flight: AtomicUsize::new(0),
                    max_in_flight: AtomicUsize::new(0),
                    gate: Notify::new(),
                    release: AtomicUsize::new(0),
                })
            })
            .collect();

        let children: Vec<Arc<dyn RouteableComponent>> = probes
            .iter()
            .cloned()
            .map(|p| p as Arc<dyn RouteableComponent>)
            .collect();
        let lb: Arc<dyn RouteableComponent> = Arc::new(AllLB::new(children));

        // Mirror run_async's worker pool.
        let (tx, rx) = mpsc::channel::<Bytes>(1024);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let mut workers = Vec::new();
        for _ in 0..workers_n {
            let rx = rx.clone();
            let lb = lb.clone();
            workers.push(tokio::spawn(async move {
                loop {
                    let next = { rx.lock().await.recv().await };
                    match next {
                        Some(u) => lb.process(u).await,
                        None => return,
                    }
                }
            }));
        }

        for i in 0..queued_updates {
            tx.send(update_bytes(json!({ "i": i }))).await.unwrap();
        }

        // Wait until the workers reach steady state: every worker is in
        // a fanout, so each probe has `workers_n` concurrent calls.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let peak: usize = probes
                .iter()
                .map(|p| p.in_flight.load(Ordering::SeqCst))
                .max()
                .unwrap();
            if peak >= workers_n {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("workers never reached steady state");
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // The bound: at most `workers_n` fanouts in flight, so each probe
        // sees at most `workers_n` concurrent calls.
        for p in &probes {
            let peak = p.max_in_flight.load(Ordering::SeqCst);
            assert!(
                peak <= workers_n,
                "probe peaked at {} concurrent calls, worker-pool bound is {}",
                peak,
                workers_n
            );
        }

        // Drain.
        for p in &probes {
            p.release.store(queued_updates, Ordering::SeqCst);
            p.gate.notify_waiters();
        }
        drop(tx);
        for w in workers {
            let _ = w.await;
        }
    }
}
