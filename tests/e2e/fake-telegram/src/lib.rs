//! A fake Telegram Bot API server for end-to-end tests.
//!
//! It answers `/bot{token}/{method}` and `/file/bot{token}/{path}` the way the
//! official server does (error texts, `getUpdates` offsets and 409 conflicts,
//! webhook delivery and webhook replies), keeps per-token state, and has a
//! control API under `/fake/`:
//!
//! - `POST /fake/updates/{token}`: queue an Update (JSON object); assigns
//!   `update_id` when absent; answers `{"update_id":N}`.
//! - `GET /fake/calls?token=&method=&kind=`: the journal (calls, downloads,
//!   webhook deliveries), oldest first. `DELETE /fake/calls` clears it.
//! - `POST /fake/faults`: `{token, method, status, retry_after?, count,
//!   delay_ms?, description?}`; the next `count` calls answer `status`.
//! - `GET /fake/health`.

mod api;
mod journal;
mod params;
mod state;
mod webhook;

use std::{io, net::SocketAddr, sync::Arc};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    journal::Filter,
    state::{Fault, Shared, bot_id},
};

/// A running fake server.
pub struct Server {
    addr: SocketAddr,
    cancel: CancellationToken,
    task: JoinHandle<io::Result<()>>,
}

impl Server {
    /// Serve on `listener` until [`Server::stop`].
    pub fn start(listener: TcpListener) -> io::Result<Self> {
        let addr = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let shared = Arc::new(Shared::new(cancel.clone()).map_err(io::Error::other)?);
        let stop = cancel.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router(shared))
                .with_graceful_shutdown(async move { stop.cancelled().await })
                .await
        });
        Ok(Self { addr, cancel, task })
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop serving: parked polls return, webhook delivery stops, and the
    /// server task is awaited.
    pub async fn stop(self) -> io::Result<()> {
        self.cancel.cancel();
        self.task.await.map_err(io::Error::other)?
    }
}

fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/fake/health", get(health))
        .route("/fake/updates/{token}", post(inject))
        .route("/fake/calls", get(calls).delete(clear_calls))
        .route("/fake/faults", post(add_fault))
        .fallback(fallback)
        .with_state(shared)
}

async fn fallback(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    api::handle(shared, request).await
}

fn control_error(description: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"ok": false, "description": description})),
    )
        .into_response()
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"ok": true}))
}

async fn inject(
    State(shared): State<Arc<Shared>>,
    Path(token): Path<String>,
    body: Bytes,
) -> Response {
    let Some(id) = bot_id(&token) else {
        return control_error("invalid token");
    };
    match shared.bot(&token, id).inject(&body) {
        Some(update_id) => Json(json!({"update_id": update_id})).into_response(),
        None => control_error("the update must be a JSON object"),
    }
}

async fn calls(State(shared): State<Arc<Shared>>, Query(filter): Query<Filter>) -> Response {
    Json(shared.journal.list(&filter)).into_response()
}

async fn clear_calls(State(shared): State<Arc<Shared>>) -> Json<serde_json::Value> {
    shared.journal.clear();
    Json(json!({"ok": true}))
}

async fn add_fault(State(shared): State<Arc<Shared>>, body: Bytes) -> Response {
    let fault: Fault = match serde_json::from_slice(&body) {
        Ok(fault) => fault,
        Err(error) => return control_error(&error.to_string()),
    };
    if let Err(problem) = fault.validate() {
        return control_error(problem);
    }
    shared.add_fault(fault);
    Json(json!({"ok": true})).into_response()
}
