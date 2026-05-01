use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use tokio::sync::mpsc::Sender;

use axum::Router;

use crate::update::base::Updater;

use crate::api::message::AddRouteType;

/// Identity of a leaf route inside the routing tree.
///
/// Used by the management API to address a specific destination for removal,
/// and by `RoundRobinLB` / `AllLB` to compare children to that target. Inner
/// nodes (load balancers) have no identity and return `None` from
/// [`Routeable::id`].
///
/// `Path` and `Url` are intentionally distinct variants — a long-poll path
/// `/foo` and a webhook URL `/foo` are unrelated entities and must never
/// alias.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RouteId {
    /// HTTP path served by a `LongPollRoute` (e.g. `/bot1/getUpdates`).
    Path(String),
    /// Downstream URL targeted by a `WebhookRoute`.
    Url(String),
}

#[async_trait]
pub trait Routeable: Send + Sync {
    /// Dispatch one update through this node.
    ///
    /// The update is shared via `Arc` so a broadcast load balancer can hand
    /// the same value to N children with one `Arc` clone per child instead
    /// of one deep `Value` clone per child. Single-consumer leaves (e.g.
    /// `LongPollRoute` reached through a `RoundRobinLB`) can recover an
    /// owned `Value` via `Arc::try_unwrap` with no clone.
    async fn process(&self, update: Arc<Value>);

    /// Identity of this route, or `None` for inner nodes (load balancers).
    fn id(&self) -> Option<RouteId> {
        None
    }

    async fn add_route(&self, route: AddRouteType) -> Result<(), ()> {
        drop(route);
        Err(())
    }

    async fn remove_route(&self, target: RouteId) -> Result<(), ()> {
        drop(target);
        Err(())
    }
}
#[async_trait]
pub trait Serverable {
    async fn set_server(&self, server: Router<Sender<Value>>) -> Router<Sender<Value>> {
        server
    }
}
#[async_trait]
pub trait Printable {
    async fn print(&self) -> String {
        "".into()
    }

    async fn json_struct(&self) -> Value {
        json!({})
    }
}

pub trait UpdaterComponent: Updater + Serverable + Printable + Send + Sync {}
impl<T: Updater + Serverable + Printable> UpdaterComponent for T {}

pub trait RouteableComponent: Routeable + Serverable + Printable + Send + Sync {}
impl<T: Routeable + Serverable + Printable> RouteableComponent for T {}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RouteId::Path("/x")` and `RouteId::Url("/x")` are unrelated entities
    /// and must compare unequal — even though their inner string is the
    /// same. Guards against accidentally collapsing the two variants back
    /// into a single `Option<&str>` representation.
    #[test]
    fn test_route_id_distinguishes_path_and_url() {
        let path = RouteId::Path("/x".to_string());
        let url = RouteId::Url("/x".to_string());

        assert_ne!(path, url);
        assert_eq!(path, RouteId::Path("/x".to_string()));
        assert_eq!(url, RouteId::Url("/x".to_string()));
    }
}
