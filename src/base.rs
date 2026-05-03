use async_trait::async_trait;
use bytes::Bytes;
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

/// Reasons `Routeable::add_route` can fail.
///
/// Encoded at the type level so the API loop in `tgin.rs` does not have to
/// invent a meaning for an opaque `Err(())`. The previous shape
/// (`Result<(), ()>`) forced the loop to map the unit error to a different
/// `ApiError` variant per operation (Add → Conflict, Rm → NotFound), which
/// silently broke if a future LB impl ever wanted to distinguish causes.
/// Splitting the trait surface into per-method error enums lets each
/// implementor say exactly which failures it can produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddRouteError {
    /// This node cannot accept a child. Today the only producer is the
    /// default trait impl on a leaf (`LongPollRoute`, `WebhookRoute`,
    /// `MockCallsRoute`): a leaf has no children list to push into. The
    /// API maps this to `409 Conflict`.
    Conflict,
}

/// Reasons `Routeable::remove_route` can fail.
///
/// See [`AddRouteError`] for the rationale on per-method error types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveRouteError {
    /// Target is not present anywhere in the subtree rooted at this node.
    /// Both LB impls produce this when neither their direct children nor
    /// any nested LB owns the target; the default trait impl on a leaf
    /// produces it unconditionally (a leaf trivially has no descendants).
    /// The API maps this to `404 Not Found`.
    NotFound,
}

#[async_trait]
pub trait Routeable: Send + Sync {
    /// Dispatch one update through this node.
    ///
    /// `update` MUST be a single, syntactically valid JSON value. The
    /// router never re-parses or re-serializes it: a `LongPollRoute`
    /// concatenates the bytes verbatim into a `getUpdates` envelope, a
    /// `WebhookRoute` POSTs them as the request body. Pushing non-JSON
    /// bytes here corrupts the consuming bot's response and silently
    /// drops every update batched alongside the bad one.
    ///
    /// Enforcement lives at the ingress boundary, not here:
    ///   * `LongPollUpdate` deserializes the upstream `result` array as
    ///     `Vec<Box<RawValue>>` and walks each element, so anything it
    ///     forwards is by construction a valid JSON value.
    ///   * `WebhookUpdate` validates the request body with
    ///     `serde_json::from_slice::<&RawValue>` and rejects malformed
    ///     bodies with `400 Bad Request` before they enter the channel.
    /// Any new ingress adapter MUST do the same before sending bytes
    /// into the routing tree.
    ///
    /// Carrying the update as `Bytes` (not `serde_json::Value`) keeps
    /// cloning to one atomic ref-count bump, so a broadcast load
    /// balancer hands the same payload to N children with no per-child
    /// copy and no `Value` allocation.
    async fn process(&self, update: Bytes);

    /// Identity of this route, or `None` for inner nodes (load balancers).
    fn id(&self) -> Option<RouteId> {
        None
    }

    /// Attach a child route at runtime. The default returns
    /// [`AddRouteError::Conflict`] — leaves cannot accept children.
    /// Load balancers override this to push into their children list.
    async fn add_route(&self, route: AddRouteType) -> Result<(), AddRouteError> {
        drop(route);
        Err(AddRouteError::Conflict)
    }

    /// Detach a previously-added route at runtime. The default returns
    /// [`RemoveRouteError::NotFound`] — leaves have no children to search.
    /// Load balancers override this to walk the children list and recurse
    /// into nested LBs.
    async fn remove_route(&self, target: RouteId) -> Result<(), RemoveRouteError> {
        drop(target);
        Err(RemoveRouteError::NotFound)
    }
}
#[async_trait]
pub trait Serverable {
    async fn set_server(&self, server: Router<Sender<Bytes>>) -> Router<Sender<Bytes>> {
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

    /// The default `Routeable::add_route` impl MUST return
    /// `AddRouteError::Conflict` so the API loop can map it to 409 without
    /// inventing a meaning for an opaque `Err(())`. This is the contract
    /// the leaf routes (`LongPollRoute`, `WebhookRoute`, `MockCallsRoute`)
    /// rely on by inheriting the default; if the default ever changes,
    /// the API loop's mapping in `tgin.rs` would silently lie.
    #[tokio::test]
    async fn test_default_add_route_returns_conflict() {
        use crate::mock::routes::MockCallsRoute;
        use crate::route::longpull::LongPollRoute;
        use std::sync::Arc;

        let leaf = MockCallsRoute::new("leaf");
        let child = Arc::new(LongPollRoute::new("/dyn".to_string()));
        let res = leaf.add_route(AddRouteType::Longpull(child)).await;
        assert_eq!(res, Err(AddRouteError::Conflict));
    }

    /// The default `Routeable::remove_route` impl MUST return
    /// `RemoveRouteError::NotFound`. Same rationale as
    /// `test_default_add_route_returns_conflict`.
    #[tokio::test]
    async fn test_default_remove_route_returns_not_found() {
        use crate::mock::routes::MockCallsRoute;

        let leaf = MockCallsRoute::new("leaf");
        let res = leaf.remove_route(RouteId::Path("/anything".to_string())).await;
        assert_eq!(res, Err(RemoveRouteError::NotFound));
    }
}
