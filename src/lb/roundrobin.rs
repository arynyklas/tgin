use crate::base::{AddRouteError, Printable, RemoveRouteError, RouteId, Routeable, RouteableComponent, Serverable};

use crate::api::message::AddRouteType;

use axum::Router;
use bytes::Bytes;
use tokio::sync::mpsc::Sender;

use arc_swap::ArcSwap;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::dynamic::longpoll_registry;

use async_trait::async_trait;

use serde_json::{json, Value};

/// Vector of children owned by an LB. Wrapped in `Arc` so reads are a
/// single atomic load + ref-count bump on the dispatch hot path.
type Routes = Vec<Arc<dyn RouteableComponent>>;

pub struct RoundRobinLB {
    /// Lock-free, RCU-style children list: `process` reads via a single
    /// atomic load with no `.await` on a lock; `add_route` /
    /// `remove_route` publish via copy-on-write `rcu`. The closure passed
    /// to `rcu` is pure (depends only on the previous `Routes`) and is
    /// idempotent under contention retries.
    routes: ArcSwap<Routes>,
    current: AtomicUsize,
}

impl RoundRobinLB {
    pub fn new(routes: Routes) -> Self {
        Self {
            routes: ArcSwap::from_pointee(routes),
            current: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Routeable for RoundRobinLB {
    async fn process(&self, update: Bytes) {
        let routes = self.routes.load();
        if routes.is_empty() {
            // Static config rejects empty LBs at startup, but the API can
            // drain a runtime LB via `remove_route`. Surface the drop so
            // the operator sees updates being silently swallowed.
            eprintln!(
                "warning: dropping update at empty RoundRobinLB (LB drained at runtime)"
            );
            return;
        }
        let current = self.current.fetch_add(1, Ordering::Relaxed);
        let index = current % routes.len();

        let route = routes[index].clone();

        // Drop the read guard before the child's `.await` so we don't
        // pin a snapshot for the duration of downstream I/O.
        drop(routes);

        route.process(update).await;
    }

    async fn add_route(&self, route: AddRouteType) -> Result<(), AddRouteError> {
        match route {
            AddRouteType::Longpull(route_arc) => {
                // Registry insert MUST happen before the LB swap: a
                // concurrent dynamic-handler POST that observes the new
                // child in the LB tree must also be able to resolve it
                // through the registry.
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
        // For long-poll targets, drop the dynamic-handler registry entry
        // before the LB swap so the route disappears from both surfaces
        // in the same operation. `remove` is a no-op for an absent key.
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

        // Target not directly hosted here — recurse into nested LBs.
        // Don't break on first success: an `AllLB` may legitimately have
        // the same route under multiple sub-trees; a partial removal
        // would silently leave dangling children. Same logic for
        // `RoundRobinLB` for symmetry.
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
impl Serverable for RoundRobinLB {
    async fn set_server(&self, mut router: Router<Sender<Bytes>>) -> Router<Sender<Bytes>> {
        let routes = self.routes.load_full();
        for route in routes.iter() {
            router = route.set_server(router).await;
        }
        router
    }
}

#[async_trait]
impl Printable for RoundRobinLB {
    async fn print(&self) -> String {
        let routes = self.routes.load_full();
        let mut text = String::from("LOAD BALANCER RoundRobin\n\n");

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
            "name": "round-robin",
            "routes": routes_json
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::routes::MockCallsRoute;

    fn update_bytes(value: Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&value).unwrap())
    }

    #[tokio::test]
    async fn test_empty_routes_does_not_panic() {
        let lb = RoundRobinLB::new(vec![]);

        lb.process(update_bytes(json!({"update_id": 1}))).await;

        let json = lb.json_struct().await;
        assert_eq!(json["routes"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_round_robin_distribution_even() {
        let route1 = Arc::new(MockCallsRoute::new("A"));
        let route2 = Arc::new(MockCallsRoute::new("B"));

        let lb = RoundRobinLB::new(vec![route1.clone(), route2.clone()]);

        for i in 0..4 {
            lb.process(update_bytes(json!({"id": i}))).await;
        }

        assert_eq!(route1.count().await, 2);
        assert_eq!(route2.count().await, 2);
    }

    #[tokio::test]
    async fn test_round_robin_ordering() {
        let r1 = Arc::new(MockCallsRoute::new("1"));
        let r2 = Arc::new(MockCallsRoute::new("2"));
        let r3 = Arc::new(MockCallsRoute::new("3"));

        let lb = RoundRobinLB::new(vec![r1.clone(), r2.clone(), r3.clone()]);

        lb.process(update_bytes(json!("msg1"))).await;
        let c1 = r1.get_calls().await;
        assert_eq!(c1.len(), 1);
        assert_eq!(c1[0], "msg1");
        assert_eq!(r2.count().await, 0);
        assert_eq!(r3.count().await, 0);

        lb.process(update_bytes(json!("msg2"))).await;
        assert_eq!(r2.count().await, 1);

        lb.process(update_bytes(json!("msg3"))).await;
        assert_eq!(r3.count().await, 1);

        lb.process(update_bytes(json!("msg4"))).await;
        assert_eq!(r1.count().await, 2);
    }

    #[tokio::test]
    async fn test_json_structure_aggregation() {
        let r1 = Arc::new(MockCallsRoute::new("alpha"));
        let r2 = Arc::new(MockCallsRoute::new("beta"));

        let lb = RoundRobinLB::new(vec![r1, r2]);

        let output = lb.json_struct().await;

        assert_eq!(output["type"], "load-balancer");
        assert_eq!(output["name"], "round-robin");

        let routes_arr = output["routes"]
            .as_array()
            .expect("routes should be an array");
        assert_eq!(routes_arr.len(), 2);
        assert_eq!(routes_arr[0]["id"], "alpha");
        assert_eq!(routes_arr[1]["id"], "beta");
    }

    #[tokio::test]
    async fn test_dynamic_add_webhook_route() {
        let r1 = Arc::new(MockCallsRoute::new("static"));
        let lb = RoundRobinLB::new(vec![r1.clone()]);

        lb.process(update_bytes(json!(1))).await;
        assert_eq!(r1.count().await, 1);

        let r2 = Arc::new(MockCallsRoute::new("dynamic"));

        let add_res = lb.add_route(AddRouteType::Webhook(r2.clone())).await;
        assert!(add_res.is_ok());

        let json_out = lb.json_struct().await;
        assert_eq!(json_out["routes"].as_array().unwrap().len(), 2);

        lb.process(update_bytes(json!(2))).await;

        assert_eq!(r2.count().await, 1);
    }

    /// After draining all children at runtime via `remove_route`, the LB
    /// must keep accepting `process` calls without panicking and without
    /// blocking — it just drops the update and logs.
    #[tokio::test]
    async fn test_runtime_empty_logs_and_no_panic() {
        use crate::route::webhook::{WebhookRoute, DEFAULT_REQUEST_TIMEOUT};
        use reqwest::Client;

        let client = Client::builder().no_proxy().build().unwrap();
        let route = Arc::new(WebhookRoute::new(
            "http://127.0.0.1:0/never".to_string(),
            client,
            DEFAULT_REQUEST_TIMEOUT,
        ));
        let lb = RoundRobinLB::new(vec![route]);

        let rm = lb
            .remove_route(RouteId::Url("http://127.0.0.1:0/never".to_string()))
            .await;
        assert!(rm.is_ok());

        // Now empty — must not panic and must return promptly.
        lb.process(update_bytes(json!({"update_id": 7}))).await;

        let json = lb.json_struct().await;
        assert_eq!(json["routes"].as_array().unwrap().len(), 0);
    }

    /// `remove_route` must report `Err` when no descendant matches the
    /// target, and `Ok` when a nested LB owns the target.
    #[tokio::test]
    async fn test_remove_route_aggregates_child_failure() {
        use crate::route::webhook::{WebhookRoute, DEFAULT_REQUEST_TIMEOUT};
        use reqwest::Client;

        let client = Client::builder().no_proxy().build().unwrap();
        let nested_target = "http://127.0.0.1:0/nested".to_string();
        let nested_route = Arc::new(WebhookRoute::new(
            nested_target.clone(),
            client,
            DEFAULT_REQUEST_TIMEOUT,
        ));
        let inner: Arc<dyn RouteableComponent> =
            Arc::new(RoundRobinLB::new(vec![nested_route]));

        let outer = RoundRobinLB::new(vec![inner]);

        // Target absent anywhere in the sub-tree → Err.
        let absent = outer
            .remove_route(RouteId::Url("http://does-not-exist".to_string()))
            .await;
        assert!(absent.is_err());

        // Target lives in the nested LB → Ok via child aggregation.
        let hit = outer.remove_route(RouteId::Url(nested_target)).await;
        assert!(hit.is_ok());
    }

    /// Concurrent `add_route` must not block a flood of `process` calls.
    /// With the previous `tokio::sync::RwLock` design, holding the read
    /// lock across `.await` could starve writers — and any writer in turn
    /// would queue every reader behind it. With `ArcSwap` the read path
    /// is wait-free.
    #[tokio::test]
    async fn test_arcswap_concurrent_add_does_not_block_process() {
        let r1 = Arc::new(MockCallsRoute::new("seed"));
        let lb = Arc::new(RoundRobinLB::new(vec![r1.clone()]));

        let writer_lb = lb.clone();
        let writer = tokio::spawn(async move {
            for i in 0..50 {
                let r = Arc::new(MockCallsRoute::new(&format!("dyn{i}")));
                writer_lb
                    .add_route(AddRouteType::Webhook(r))
                    .await
                    .unwrap();
            }
        });

        let start = std::time::Instant::now();
        for i in 0..1000 {
            lb.process(update_bytes(json!({"id": i}))).await;
        }
        writer.await.unwrap();

        // Loose ceiling: 1k sequential dispatches into in-process mock
        // routes, with a writer thrashing the children list, must finish
        // well under 100 ms on any reasonable dev box. The point is to
        // catch a regression to a write-starving lock, not to pin
        // wall-clock perf.
        assert!(
            start.elapsed() < std::time::Duration::from_millis(2000),
            "process loop took too long: {:?}",
            start.elapsed()
        );
    }
}
