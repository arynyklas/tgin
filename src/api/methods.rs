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
            validate_longpoll_path(&route.path, "Longpull route")?;
            let mut update = LongPollRoute::new(route.path);
            if let Some(cap) = route.max_buffered_updates {
                update.set_max_buffered_updates(cap);
            }
            AddRouteType::Longpull(Arc::new(update))
        }
        AddRouteTypeSch::Webhook(route) => {
            validate_webhook_url(&route.url, "Webhook route")?;
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
            validate_longpoll_path(&r.path, "Longpull removal")?;
            RouteId::Path(r.path)
        }
        RmRouteTypeSch::Webhook(r) => {
            validate_webhook_url(&r.url, "Webhook removal")?;
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

/// Reject a long-poll path that the routing layer cannot serve.
///
/// `axum::Router::route` requires paths that begin with `/`; anything else
/// silently fails to match at request time and the operator sees a 404 with
/// no clear cause. Catching it here turns a runtime symptom into a 400 with
/// an actionable message. `kind` distinguishes "add" vs "remove" in the
/// error so a misconfigured `DELETE` body says "removal" and not "route".
fn validate_longpoll_path(path: &str, kind: &str) -> Result<(), ApiError> {
    if path.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "{kind} requires a non-empty `path`"
        )));
    }
    if !path.starts_with('/') {
        return Err(ApiError::BadRequest(format!(
            "{kind} `path` must start with `/`, got `{path}`"
        )));
    }
    Ok(())
}

/// Reject a webhook URL that `reqwest` will fail to dispatch to.
///
/// `reqwest::Url::parse` catches malformed URLs (missing scheme, illegal
/// characters, IDN issues). The scheme guard catches structurally-valid
/// URLs whose scheme `WebhookRoute` cannot POST to (`ftp://`, `file://`,
/// `data:`, etc.). Without the guard, the route would be added, every
/// dispatched update would fail at request time, and the operator would
/// see a stream of best-effort-logged failures with no upfront warning.
fn validate_webhook_url(url: &str, kind: &str) -> Result<(), ApiError> {
    if url.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "{kind} requires a non-empty `url`"
        )));
    }
    let parsed = reqwest::Url::parse(url).map_err(|err| {
        ApiError::BadRequest(format!(
            "{kind} `url` is not a valid URL ({err}): `{url}`"
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::BadRequest(format!(
            "{kind} `url` must use http or https, got `{}`",
            parsed.scheme()
        )));
    }
    Ok(())
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
            typee: AddRouteTypeSch::Longpull(AddLongpullRouteSch { path: String::new(), max_buffered_updates: None }),
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

    /// `add_route` MUST reject a long-poll path that doesn't start with `/`.
    /// Without this, the route is added but `axum::Router::route` won't match
    /// any incoming request and the operator sees an opaque 404.
    #[tokio::test]
    async fn add_longpull_path_without_leading_slash_returns_400() {
        let (tx, mut rx) = mpsc::channel::<ApiMessage>(8);
        let body = AddRouteSch {
            typee: AddRouteTypeSch::Longpull(AddLongpullRouteSch { path: "bot/updates".into(), max_buffered_updates: None }),
        };

        let res = add_route(State(tx), Extension(test_client()), Json(body)).await;
        match res {
            Err(ApiError::BadRequest(msg)) => {
                assert!(
                    msg.contains("must start with `/`"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected BadRequest, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    /// `remove_route` MUST apply the same path-shape check as `add_route` so
    /// callers can't half-trick the API into deleting a path that could not
    /// have been added through it.
    #[tokio::test]
    async fn remove_longpull_path_without_leading_slash_returns_400() {
        let (tx, mut rx) = mpsc::channel::<ApiMessage>(8);
        let body = RmRouteSch {
            typee: RmRouteTypeSch::Longpull(RmLongpullRouteSch {
                path: "bot/updates".into(),
            }),
        };

        let res = remove_route(State(tx), Json(body)).await;
        match res {
            Err(ApiError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    /// `add_route` MUST reject a webhook URL that `reqwest::Url::parse`
    /// can't parse. The route would otherwise be added, every dispatched
    /// update would fail at request time, and the failure would only show
    /// up in `eprintln!` from `WebhookRoute`.
    #[tokio::test]
    async fn add_webhook_malformed_url_returns_400() {
        let (tx, mut rx) = mpsc::channel::<ApiMessage>(8);
        let body = AddRouteSch {
            typee: AddRouteTypeSch::Webhook(AddWebhookRouteSch {
                url: "not a url".into(),
                request_timeout_ms: None,
            }),
        };

        let res = add_route(State(tx), Extension(test_client()), Json(body)).await;
        match res {
            Err(ApiError::BadRequest(msg)) => {
                assert!(
                    msg.contains("not a valid URL"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected BadRequest, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    /// `add_route` MUST reject schemes `WebhookRoute` cannot POST to.
    /// `reqwest::Client::post` rejects non-http/https at request time
    /// (`UnsupportedScheme`), so allowing `ftp://` here would just defer
    /// the failure to the dispatcher loop.
    #[tokio::test]
    async fn add_webhook_non_http_scheme_returns_400() {
        let cases = ["ftp://example.com/bot", "file:///etc/passwd", "data:,hi"];
        for url in cases {
            let (tx, mut rx) = mpsc::channel::<ApiMessage>(8);
            let body = AddRouteSch {
                typee: AddRouteTypeSch::Webhook(AddWebhookRouteSch {
                    url: url.into(),
                    request_timeout_ms: None,
                }),
            };

            let res = add_route(State(tx), Extension(test_client()), Json(body)).await;
            match res {
                Err(ApiError::BadRequest(msg)) => {
                    assert!(
                        msg.contains("http or https"),
                        "{url}: unexpected error message: {msg}"
                    );
                }
                other => panic!("{url}: expected BadRequest, got {:?}", other),
            }
            assert!(rx.try_recv().is_err(), "{url}: leaked into channel");
        }
    }

}
