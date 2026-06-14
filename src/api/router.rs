use axum::{
    routing::{get, post},
    Extension, Router,
};
use bytes::Bytes;
use reqwest::Client;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::api::message::ApiMessage;
use crate::base::Serverable;

use crate::api::methods;

use async_trait::async_trait;

pub struct Api {
    base_path: String,
    tx: Sender<ApiMessage>,
    pub rx: Receiver<ApiMessage>,
    /// Shared `reqwest::Client` injected into routes built by `add_route`.
    /// Stored here (rather than constructed at handler time) so dynamically
    /// added `WebhookRoute`s share the process-wide connection pool / TLS
    /// state with statically configured ones.
    client: Client,
    /// Optional shared secret gating this control plane. `None` leaves it open.
    auth_token: Option<String>,
}

impl Api {
    pub fn new(base_path: String, client: Client, auth_token: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel::<ApiMessage>(100);
        Self {
            base_path,
            tx,
            rx,
            client,
            auth_token,
        }
    }
}

#[async_trait]
impl Serverable for Api {
    async fn set_server(&self, main_router: Router<Sender<Bytes>>) -> Router<Sender<Bytes>> {
        let router = Router::new()
            .route("/routes", get(methods::get_routes))
            .route(
                "/route",
                post(methods::add_route).delete(methods::remove_route),
            )
            .layer(Extension(self.client.clone()))
            .with_state(self.tx.clone());

        // Optional bearer gate on the mutating control plane. Applied after
        // `with_state` so it wraps the finished sub-router and runs ahead of
        // every handler; `None` leaves it open (network-isolation default).
        let router = crate::utils::auth::guard(router, self.auth_token.clone());

        main_router.nest(&self.base_path, router)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_client() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    /// With `auth_token` set, the management API rejects an unauthenticated or
    /// wrong-credential request with 401 — and does so in the gate layer,
    /// before the handler ever reaches the control channel (so this test does
    /// not need the main loop running to consume `ApiMessage`s).
    #[tokio::test]
    async fn test_api_rejects_without_bearer_when_token_set() {
        let api = Api::new("/api".to_string(), test_client(), Some("s3cret".to_string()));
        let app = api.set_server(Router::new()).await.with_state(mpsc::channel::<Bytes>(1).0);

        let missing = app
            .clone()
            .oneshot(Request::get("/api/routes").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED, "missing bearer");

        let wrong = app
            .oneshot(
                Request::get("/api/routes")
                    .header("authorization", "Bearer nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED, "wrong bearer");
    }
}
