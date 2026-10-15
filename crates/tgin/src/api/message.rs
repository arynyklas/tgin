use crate::base::{RouteId, RouteableComponent};

use crate::route::longpull::LongPollRoute;

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use tokio::sync::oneshot;

use serde_json::Value;

pub enum AddRouteType {
    Longpull(Arc<LongPollRoute>),
    Webhook(Arc<dyn RouteableComponent>),
}

/// Outcome of a control-plane mutation, plumbed back to the HTTP handler so
/// callers learn the truth instead of always seeing 200.
///
/// The previous design dropped the result on the floor: handlers returned
/// `()` (always 200) and `Routeable::{add_route, remove_route}` returned
/// `Result<(), ()>` that nothing observed. The trait now returns typed
/// `AddRouteError` / `RemoveRouteError`; this enum is the wire-level
/// contract between the API loop and the HTTP layer; status-code mapping
/// lives in `IntoResponse` so the loop never touches HTTP types.
#[derive(Debug)]
pub enum ApiError {
    /// Caller-supplied data is malformed (e.g. empty `url` / `path`).
    BadRequest(String),
    /// `RmRoute` target is not present anywhere in the tree.
    NotFound(String),
    /// Mutation cannot be applied against the current tree shape (e.g. the
    /// top-level route is a leaf, not a load balancer, so it has nowhere to
    /// attach a child).
    Conflict(String),
    /// The control plane itself failed (channel closed, response dropped).
    /// Surfaces as 500 — distinct from caller errors so operators can tell
    /// "you sent garbage" from "tgin is broken".
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, message).into_response()
    }
}

/// Control-plane messages produced by the HTTP API and consumed by
/// `Tgin::run_async`.
///
/// Every mutating variant carries a `oneshot::Sender` so the handler can
/// `await` the actual outcome and translate it to a status code. Without
/// this, axum would happily return 200 for a duplicate add, a missing
/// remove, or a tree-shape mismatch — that was the bug this enum is
/// designed against.
pub enum ApiMessage {
    AddRoute {
        route: AddRouteType,
        resp: oneshot::Sender<Result<(), ApiError>>,
    },
    GetRoutes(oneshot::Sender<Value>),
    RmRoute {
        target: RouteId,
        resp: oneshot::Sender<Result<(), ApiError>>,
    },
}
