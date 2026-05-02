use axum::{extract::State, http::StatusCode, Extension, Json};
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

use crate::api::message::{AddRouteType, ApiError, ApiMessage};
use crate::api::schemas::{AddRouteSch, AddRouteTypeSch, RmRouteSch, RmRouteTypeSch};
use crate::base::RouteId;

use crate::route::longpull::LongPollRoute;
use crate::route::webhook::{WebhookRoute, DEFAULT_REQUEST_TIMEOUT};
use std::time::Duration;

/// Add a route at runtime.
///
/// Validates input shape (non-empty `url` / `path`) before crossing the
/// channel boundary, then forwards the build route to the API loop and
/// awaits its verdict via a `oneshot`. The previous implementation
/// returned `()` (always 200) and dropped the `Routeable::add_route`
/// result — operators had no way to learn that the mutation was rejected.
pub async fn add_route(
    State(tx): State<Sender<ApiMessage>>,
    Extension(client): Extension<Client>,
    Json(data): Json<AddRouteSch>,
) -> Result<StatusCode, ApiError> {
    let route = match data.typee {
        AddRouteTypeSch::Longpull(route) => {
            if route.path.is_empty() {
                return Err(ApiError::BadRequest(
                    "Longpull route requires a non-empty `path`".into(),
                ));
            }
            let update = LongPollRoute::new(route.path);
            AddRouteType::Longpull(Arc::new(update))
        }
        AddRouteTypeSch::Webhook(route) => {
            if route.url.is_empty() {
                return Err(ApiError::BadRequest(
                    "Webhook route requires a non-empty `url`".into(),
                ));
            }
            let timeout = route
                .request_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_REQUEST_TIMEOUT);
            let update = WebhookRoute::new(route.url, client, timeout);
            AddRouteType::Webhook(Arc::new(update))
        }
    };

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(ApiMessage::AddRoute {
        route,
        resp: resp_tx,
    })
    .await
    .map_err(|_| ApiError::Internal("control plane channel closed".into()))?;

    resp_rx
        .await
        .map_err(|_| ApiError::Internal("control plane dropped response".into()))??;

    Ok(StatusCode::OK)
}

pub async fn get_routes(
    State(tx): State<Sender<ApiMessage>>,
) -> Result<Json<Value>, ApiError> {
    let (tx_response, rx_response) = oneshot::channel();

    tx.send(ApiMessage::GetRoutes(tx_response))
        .await
        .map_err(|_| ApiError::Internal("control plane channel closed".into()))?;

    rx_response
        .await
        .map(Json)
        .map_err(|_| ApiError::Internal("control plane dropped response".into()))
}

/// Remove a previously-added route.
///
/// Symmetric with `add_route`: validates, forwards, awaits the verdict,
/// returns 404 when the target is absent, 200 when removed. The previous
/// implementation spawned a detached task — a caller who issued
/// `add` then `rm` could not reason about ordering, and a missing target
/// produced a 200.
pub async fn remove_route(
    State(tx): State<Sender<ApiMessage>>,
    Json(data): Json<RmRouteSch>,
) -> Result<StatusCode, ApiError> {
    let target = match data.typee {
        RmRouteTypeSch::Longpull(r) => {
            if r.path.is_empty() {
                return Err(ApiError::BadRequest(
                    "Longpull removal requires a non-empty `path`".into(),
                ));
            }
            RouteId::Path(r.path)
        }
        RmRouteTypeSch::Webhook(r) => {
            if r.url.is_empty() {
                return Err(ApiError::BadRequest(
                    "Webhook removal requires a non-empty `url`".into(),
                ));
            }
            RouteId::Url(r.url)
        }
    };

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(ApiMessage::RmRoute {
        target,
        resp: resp_tx,
    })
    .await
    .map_err(|_| ApiError::Internal("control plane channel closed".into()))?;

    resp_rx
        .await
        .map_err(|_| ApiError::Internal("control plane dropped response".into()))??;

    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schemas::{
        AddLongpullRouteSch, AddWebhookRouteSch, RmLongpullRouteSch, RmWebhookRouteSch,
    };
    use tokio::sync::mpsc;

    fn test_client() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    /// `add_route` MUST reject an empty webhook URL with 400 and MUST NOT
    /// dispatch anything to the control plane: a well-formed channel that
    /// receives garbage is a downstream bug we own; rejecting at the edge
    /// means the API loop never sees it.
    #[tokio::test]
    async fn add_webhook_empty_url_returns_400_and_does_not_send() {
        let (tx, mut rx) = mpsc::channel::<ApiMessage>(8);
        let body = AddRouteSch {
            typee: AddRouteTypeSch::Webhook(AddWebhookRouteSch {
                url: String::new(),
                request_timeout_ms: None,
            }),
        };

        let res = add_route(State(tx), Extension(test_client()), Json(body)).await;

        match res {
            Err(ApiError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {:?}", other),
        }
        assert!(
            rx.try_recv().is_err(),
            "no message must reach the control plane on input validation failure"
        );
    }

    /// `add_route` MUST reject an empty long-poll path with 400.
    #[tokio::test]
    async fn add_longpull_empty_path_returns_400() {
        let (tx, mut rx) = mpsc::channel::<ApiMessage>(8);
        let body = AddRouteSch {
            typee: AddRouteTypeSch::Longpull(AddLongpullRouteSch {
                path: String::new(),
            }),
        };

        let res = add_route(State(tx), Extension(test_client()), Json(body)).await;

        match res {
            Err(ApiError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    /// `remove_route` MUST reject empty identifiers with 400.
    #[tokio::test]
    async fn remove_empty_identifier_returns_400() {
        let (tx, mut rx) = mpsc::channel::<ApiMessage>(8);
        let body = RmRouteSch {
            typee: RmRouteTypeSch::Webhook(RmWebhookRouteSch {
                url: String::new(),
            }),
        };

        let res = remove_route(State(tx.clone()), Json(body)).await;
        match res {
            Err(ApiError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {:?}", other),
        }

        let body = RmRouteSch {
            typee: RmRouteTypeSch::Longpull(RmLongpullRouteSch {
                path: String::new(),
            }),
        };
        let res = remove_route(State(tx), Json(body)).await;
        match res {
            Err(ApiError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    /// When the control plane reports `NotFound`, the handler MUST surface
    /// it instead of converting to 200. This is the regression the original
    /// detached `tokio::spawn(remove_route)` made impossible to test.
    #[tokio::test]
    async fn remove_not_found_returns_404() {
        let (tx, mut rx) = mpsc::channel::<ApiMessage>(8);

        let loop_handle = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let ApiMessage::RmRoute { resp, .. } = msg {
                    let _ = resp.send(Err(ApiError::NotFound("route not found".into())));
                }
            }
        });

        let body = RmRouteSch {
            typee: RmRouteTypeSch::Webhook(RmWebhookRouteSch {
                url: "http://does-not-exist".into(),
            }),
        };

        let res = remove_route(State(tx), Json(body)).await;
        match res {
            Err(ApiError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {:?}", other),
        }

        loop_handle.abort();
    }

    /// Healthy round-trip: the API loop responds Ok and the handler returns
    /// 200. Pairs with the negative tests above so we know the success
    /// path still works after the contract change.
    #[tokio::test]
    async fn add_route_ok_returns_200() {
        let (tx, mut rx) = mpsc::channel::<ApiMessage>(8);

        let loop_handle = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let ApiMessage::AddRoute { resp, .. } = msg {
                    let _ = resp.send(Ok(()));
                }
            }
        });

        let body = AddRouteSch {
            typee: AddRouteTypeSch::Webhook(AddWebhookRouteSch {
                url: "http://downstream.invalid/bot".into(),
                request_timeout_ms: None,
            }),
        };

        let res = add_route(State(tx), Extension(test_client()), Json(body)).await;
        match res {
            Ok(StatusCode::OK) => {}
            other => panic!("expected Ok(200), got {:?}", other),
        }

        loop_handle.abort();
    }

    /// If the API loop is gone (channel dropped), the handler MUST return
    /// 500 — distinct from caller errors so operators can tell apart
    /// "you sent garbage" and "tgin is broken".
    #[tokio::test]
    async fn add_route_internal_when_channel_closed() {
        let (tx, rx) = mpsc::channel::<ApiMessage>(8);
        drop(rx);

        let body = AddRouteSch {
            typee: AddRouteTypeSch::Webhook(AddWebhookRouteSch {
                url: "http://downstream.invalid/bot".into(),
                request_timeout_ms: None,
            }),
        };

        let res = add_route(State(tx), Extension(test_client()), Json(body)).await;
        match res {
            Err(ApiError::Internal(_)) => {}
            other => panic!("expected Internal, got {:?}", other),
        }
    }
}
