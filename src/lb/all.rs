use crate::base::{Printable, RouteId, Routeable, RouteableComponent, Serverable};

use axum::Router;
use tokio::sync::mpsc::Sender;

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
    async fn process(&self, update: Arc<Value>) {
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
        for route in routes.iter() {
            let route = route.clone();
            // One ref-count bump per child instead of a deep Value clone:
            // each spawned dispatcher receives the same shared `Arc<Value>`.
            let update = Arc::clone(&update);

            tokio::spawn(async move {
                route.process(update).await;
            });
        }
    }

    async fn add_route(&self, route: AddRouteType) -> Result<(), ()> {
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

    async fn remove_route(&self, target: RouteId) -> Result<(), ()> {
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
            Err(())
        }
    }
}

#[async_trait]
impl Serverable for AllLB {
    async fn set_server(&self, mut router: Router<Sender<Value>>) -> Router<Sender<Value>> {
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

    #[tokio::test]
    async fn test_empty_routes_does_not_panic() {
        let lb = AllLB::new(vec![]);

        lb.process(Arc::new(json!({"update_id": 1}))).await;

        let json = lb.json_struct().await;
        assert_eq!(json["routes"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_all_broadcast_distribution() {
        let route1 = Arc::new(MockCallsRoute::new("A"));
        let route2 = Arc::new(MockCallsRoute::new("B"));

        let lb = AllLB::new(vec![route1.clone(), route2.clone()]);

        lb.process(Arc::new(json!({"msg": "hello"}))).await;

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

        lb.process(Arc::new(json!("m1"))).await;
        lb.process(Arc::new(json!("m2"))).await;
        lb.process(Arc::new(json!("m3"))).await;

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

        lb.process(Arc::new(json!(1))).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(r1.count().await, 1);

        let r2 = Arc::new(MockCallsRoute::new("dynamic"));
        let add_res = lb.add_route(AddRouteType::Webhook(r2.clone())).await;
        assert!(add_res.is_ok());

        let json_out = lb.json_struct().await;
        assert_eq!(json_out["routes"].as_array().unwrap().len(), 2);

        lb.process(Arc::new(json!(2))).await;
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
}
