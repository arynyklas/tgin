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
}

impl Api {
    pub fn new(base_path: String, client: Client) -> Self {
        let (tx, rx) = mpsc::channel::<ApiMessage>(100);
        Self {
            base_path,
            tx,
            rx,
            client,
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

        main_router.nest(&self.base_path, router)
    }
}
