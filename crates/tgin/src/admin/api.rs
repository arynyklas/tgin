//! The admin plane's routes.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tgin_core::config::{self, Ingress};
use tgin_core::engine::{BreakerStatus, Mode};
use tgin_core::metrics::{SourceMetrics, SourceStateLabel};
use tgin_runtime::config::ConfigError;
use tgin_runtime::http::bearer_matches;
use tgin_runtime::lifecycle::Phase;
use tgin_runtime::obs::{METRICS_CONTENT_TYPE, Readiness};
use tgin_runtime::secret::Redactor;
use tgin_runtime::supervisor::TaskState;

use super::{ConsumerStatus, EndpointStatus, SourceStatus, Status, TaskStatus};
use crate::app::Shared;
use crate::cli::VERSION;
use crate::reload::ReloadError;
use crate::sources::{Kind, source_task};

/// The admin plane's routes over `shared`.
pub(crate) fn router(shared: Arc<Shared>) -> Router {
    let protected = Router::new()
        .route("/v1/status", get(status))
        .route("/v1/config", get(running_config))
        .route("/v1/reload", post(reload))
        .route("/v1/consumers/{name}/pause", post(pause))
        .route("/v1/consumers/{name}/resume", post(resume))
        .route("/v1/consumers/{name}/drain", post(drain))
        .route("/v1/consumers/{name}/purge", post(purge))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&shared),
            require_token,
        ));
    Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .merge(protected)
        .with_state(shared)
}

/// Whether `headers` carry the admin token of the running config (so a
/// rotated token applies at once). Without a token, a request is
/// authorized only while every admin listener is on loopback: a reload may
/// drop the token while a non-loopback listener stays bound until a
/// restart, and then nothing is authorized (R40).
fn authorized(shared: &Shared, headers: &HeaderMap) -> bool {
    match &shared.config.load().admin.token {
        Some(token) => bearer_matches(headers, token),
        None => !shared.admin_exposed,
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "error": message }))).into_response()
}

/// `508 Loop Detected`: the request is one of this tgin's own pushes.
pub(crate) fn loop_detected() -> Response {
    error(StatusCode::LOOP_DETECTED, "loop detected")
}

fn unauthorized() -> Response {
    let mut response = error(StatusCode::UNAUTHORIZED, "unauthorized");
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        header::HeaderValue::from_static("Bearer"),
    );
    response
}

fn unknown_consumer() -> Response {
    error(StatusCode::NOT_FOUND, "unknown consumer")
}

async fn require_token(State(shared): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    if authorized(&shared, req.headers()) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

async fn livez() -> &'static str {
    "ok"
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Starting => "starting",
        Phase::Running => "running",
        Phase::Draining => "draining",
        Phase::Stopped => "stopped",
    }
}

/// 200 while running with every component ready, else 503; the body lists
/// each component's readiness.
async fn readyz(State(shared): State<Arc<Shared>>) -> Response {
    let (components_ready, components) = shared.health.ready();
    let phase = shared.lifecycle.phase();
    let ready = phase == Phase::Running && components_ready;
    let components: serde_json::Map<String, Value> = components
        .into_iter()
        .map(|(name, readiness)| {
            let shown = match readiness {
                Readiness::Ready => "ready".to_owned(),
                Readiness::NotReady(reason) => reason,
            };
            (name, Value::from(shown))
        })
        .collect();
    let body = json!({
        "ready": ready,
        "phase": phase_name(phase),
        "components": components,
    });
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, axum::Json(body)).into_response()
}

async fn metrics(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    if !shared.config.load().admin.public_metrics && !authorized(&shared, &headers) {
        return unauthorized();
    }
    (
        [(header::CONTENT_TYPE, METRICS_CONTENT_TYPE)],
        shared.metrics.encode(),
    )
        .into_response()
}

async fn status(State(shared): State<Arc<Shared>>) -> axum::Json<Status> {
    axum::Json(status_of(&shared))
}

/// The process as `/v1/status` shows it. Task errors hold raw error text,
/// so they pass through the current redactor.
fn status_of(shared: &Shared) -> Status {
    let cfg = shared.config.load();
    let redactor = shared.redactor.load();
    let tasks = shared.supervisor.states();
    let engine_state = shared.engine.state();
    let engine = shared.engine.status();
    let task = |name: &str| {
        tasks
            .iter()
            .find(|(task, _)| &**task == name)
            .map(|(_, state)| task_state(state, &redactor))
    };

    let sources = cfg
        .bots
        .iter()
        .map(|bot| {
            let metrics = shared.engine.source_id(&bot.name).and_then(|id| {
                engine_state
                    .sources
                    .get(usize::from(id.0))
                    .and_then(Option::as_ref)
                    .map(|info| &info.metrics)
            });
            let ((state, error), ingress) = match &bot.ingress {
                Some(Ingress::LongPoll(_)) => {
                    let state = match task(&source_task(&bot.name)) {
                        // The source waits out its failures inside its task.
                        Some(("running", _))
                            if metrics.and_then(SourceMetrics::state)
                                == Some(SourceStateLabel::Backoff) =>
                        {
                            let error = shared.sources.long_poll_error(&bot.name);
                            ("backoff", error.map(|e| redactor.redact(&e).into_owned()))
                        }
                        Some(state) => state,
                        None => ("starting", None),
                    };
                    (state, Some("long_poll"))
                }
                Some(Ingress::Webhook(_)) => {
                    let serving = shared.sources.kind(&bot.name) == Some(Kind::Webhook)
                        && !shared.sources.ingest_stopped();
                    let state = match task(&source_task(&bot.name)) {
                        // Its source could not be set up.
                        Some(failed @ ("failed", _)) => failed,
                        _ if serving => ("running", None),
                        _ if shared.sources.is_starting(&bot.name) => ("starting", None),
                        _ => ("stopped", None),
                    };
                    (state, Some("webhook"))
                }
                None => (("stopped", None), None),
            };
            SourceStatus {
                name: bot.name.clone(),
                kind: "bot".to_owned(),
                ingress: ingress.map(str::to_owned),
                state: state.to_owned(),
                updates_total: metrics.map_or(0, |m| m.updates_total()),
                errors_total: metrics.map_or(0, |m| m.errors_total()),
                error,
            }
        })
        .collect();

    let consumers = engine
        .consumers
        .into_iter()
        .map(|consumer| ConsumerStatus {
            name: consumer.name,
            mode: match consumer.mode {
                Mode::Pull => "pull",
                Mode::Push => "push",
            }
            .to_owned(),
            depth: consumer.depth,
            leased: consumer.leased,
            in_flight: consumer.in_flight,
            paused: consumer.paused,
            endpoints: consumer
                .endpoints
                .unwrap_or_default()
                .into_iter()
                .map(|endpoint| EndpointStatus {
                    url_redacted: endpoint.origin,
                    breaker: match endpoint.breaker {
                        BreakerStatus::Closed => "closed",
                        BreakerStatus::Open => "open",
                        BreakerStatus::HalfOpen => "half_open",
                    }
                    .to_owned(),
                    runtime: endpoint.runtime,
                })
                .collect(),
        })
        .collect();

    let tasks = tasks
        .iter()
        .map(|(name, state)| {
            let (state, error) = task_state(state, &redactor);
            TaskStatus {
                name: name.to_string(),
                state: state.to_owned(),
                error,
            }
        })
        .collect();

    Status {
        version: VERSION.to_owned(),
        generation: engine.generation,
        phase: phase_name(shared.lifecycle.phase()).to_owned(),
        sources,
        consumers,
        tasks,
    }
}

/// A task's state name, and its error with secrets masked.
fn task_state(state: &TaskState, redactor: &Redactor) -> (&'static str, Option<String>) {
    let masked = |text: &str| Some(redactor.redact(text).into_owned());
    match state {
        TaskState::Starting => ("starting", None),
        TaskState::Running => ("running", None),
        TaskState::Backoff { last_error, .. } => ("backoff", masked(last_error)),
        TaskState::Failed { reason } => ("failed", masked(reason)),
        TaskState::Stopped => ("stopped", None),
    }
}

async fn running_config(State(shared): State<Arc<Shared>>) -> axum::Json<Value> {
    axum::Json(config::redacted_json(&shared.config.load()))
}

async fn reload(State(shared): State<Arc<Shared>>) -> Response {
    let draining = || error(StatusCode::SERVICE_UNAVAILABLE, "draining");
    if shared.lifecycle.phase() >= Phase::Draining {
        return draining();
    }
    match shared.reloads.request().await {
        Ok(report) => axum::Json(report).into_response(),
        Err(ReloadError::Draining) => draining(),
        Err(ReloadError::Rejected(errors)) => {
            let errors: Vec<Value> = errors.iter().map(error_json).collect();
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                axum::Json(json!({ "errors": errors })),
            )
                .into_response()
        }
    }
}

/// `{"path","line","column","message","hint"}`; the location fields are
/// `null` for an error without one.
fn error_json(err: &ConfigError) -> Value {
    json!({
        "path": err.at.as_ref().map(|at| &at.path),
        "line": err.at.as_ref().map(|at| at.line),
        "column": err.at.as_ref().map(|at| at.column),
        "message": err.message,
        "hint": err.hint,
    })
}

async fn pause(State(shared): State<Arc<Shared>>, Path(name): Path<String>) -> Response {
    if shared.engine.pause(&name) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        unknown_consumer()
    }
}

/// Resumes the consumer and ends a drain `POST …/drain` began; refused
/// while tgin shuts down, whose drain must not end.
async fn resume(State(shared): State<Arc<Shared>>, Path(name): Path<String>) -> Response {
    if shared.lifecycle.phase() >= Phase::Draining {
        return error(StatusCode::CONFLICT, "draining");
    }
    if shared.engine.resume(&name) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        unknown_consumer()
    }
}

async fn drain(State(shared): State<Arc<Shared>>, Path(name): Path<String>) -> Response {
    if shared.engine.drain(&name) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        unknown_consumer()
    }
}

async fn purge(State(shared): State<Arc<Shared>>, Path(name): Path<String>) -> Response {
    match shared.engine.purge(&name) {
        Some(purged) => axum::Json(json!({ "purged": purged })).into_response(),
        None => unknown_consumer(),
    }
}
