use axum::{extract::State, http, response::IntoResponse, Extension, Json};
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;
use tokio;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

use crate::api::message::{AddRouteType, ApiMessage};
use crate::api::schemas::{AddRouteSch, AddRouteTypeSch, RmRouteSch, RmRouteTypeSch};
use crate::base::RouteId;

use crate::route::longpull::LongPollRoute;
use crate::route::webhook::{WebhookRoute, DEFAULT_REQUEST_TIMEOUT};
use std::time::Duration;

pub async fn add_route(
    State(tx): State<Sender<ApiMessage>>,
    Extension(client): Extension<Client>,
    Json(data): Json<AddRouteSch>,
) {
    let route = match data.typee {
        AddRouteTypeSch::Longpull(route) => {
            let update = LongPollRoute::new(route.path);
            AddRouteType::Longpull(Arc::new(update))
        }
        AddRouteTypeSch::Webhook(route) => {
            let timeout = route
                .request_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_REQUEST_TIMEOUT);
            let update = WebhookRoute::new(route.url, client, timeout);
            AddRouteType::Webhook(Arc::new(update))
        }
    };

    let _ = tx
        .send(ApiMessage::AddRoute {
            sublevel: data.sublevel,
            route,
        })
        .await;
}

pub async fn get_routes(
    State(tx): State<Sender<ApiMessage>>,
) -> Result<Json<Value>, impl IntoResponse> {
    let (tx_response, rx_response) = oneshot::channel();

    let _ = tx.send(ApiMessage::GetRoutes(tx_response)).await;

    match rx_response.await {
        Ok(json) => Ok(Json::from(json)),
        Err(_) => Err(http::StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn remove_route(State(tx): State<Sender<ApiMessage>>, Json(data): Json<RmRouteSch>) {
    let target = match data.typee {
        RmRouteTypeSch::Longpull(r) => RouteId::Path(r.path),
        RmRouteTypeSch::Webhook(r) => RouteId::Url(r.url),
    };

    let _ = tx.send(ApiMessage::RmRoute(target)).await;
}
