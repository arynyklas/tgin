//! Observability subsystem: tracing subscriber init, per-component metrics,
//! the process-global metrics registry, the `dispatch` correlation span, and
//! the always-on `GET /status` (JSON) + `GET /metrics` (Prometheus text)
//! handlers.
//!
//! The pure functions — [`collect_leaves`], [`render_prometheus`],
//! [`build_status`] — take their inputs explicitly and touch no globals, so
//! they are unit-testable in isolation. The only shared state is the
//! [`struct@REGISTRY`] (set once at startup, read-only after) and the
//! [`struct@LB_DROPPED_EMPTY`] counter.
//!
//! Token redaction ([`TELEGRAM_TOKEN_RE`]) is applied to every metric label
//! and `source` identity so a misconfigured route URL carrying a bot token
//! never leaks through `/status` or `/metrics`.

use std::borrow::Cow;
use std::fmt::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

use arc_swap::ArcSwap;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use serde_json::{json, Value};

use crate::base::RouteableComponent;
use crate::config::schema::LogFormat;
use crate::utils::defaults::TELEGRAM_TOKEN_RE;

/// Install the process-global `tracing` subscriber. Called exactly once from
/// `main::run` after config validation; calling it twice panics inside
/// `.init()`, which is intentional — a double-init is a wiring bug.
///
/// Writer is the `fmt` default (stdout), unifying what used to be a mixed
/// stdout-banner / stderr-error split. `RUST_LOG` overrides the configured
/// level via `from_env_lossy`.
pub fn init(log_level: &str, log_format: LogFormat) {
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::EnvFilter;

    // `log_level` is validated by `config::setup::validate`, so the parse
    // cannot realistically fail; fall back to INFO defensively rather than
    // panic on an unexpected value.
    let level: LevelFilter = log_level.parse().unwrap_or(LevelFilter::INFO);
    let filter = EnvFilter::builder()
        .with_default_directive(level.into())
        .from_env_lossy();

    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match log_format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Compact => builder.compact().init(),
    }
}

/// Counters for one ingress adapter (updater). Shared as an `Arc` between the
/// updater task that increments it and the registry that reads it.
pub struct UpdaterMetrics {
    kind: &'static str,
    /// Redacted identity: poll URL for long-poll, ingress path for webhook.
    source: String,
    updates_received: AtomicU64,
    /// Long-poll transient poll failures; stays 0 for webhook ingress.
    poll_failures: AtomicU64,
    /// Set once a long-poll updater hits a 401/403/404 and stops; webhook
    /// ingress never sets it.
    permanent_failure: AtomicBool,
}

impl UpdaterMetrics {
    pub fn new(kind: &'static str, source: String) -> Arc<Self> {
        Arc::new(Self {
            kind,
            source,
            updates_received: AtomicU64::new(0),
            poll_failures: AtomicU64::new(0),
            permanent_failure: AtomicBool::new(false),
        })
    }

    pub fn record_update(&self) {
        self.updates_received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_poll_failure(&self) {
        self.poll_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn mark_permanent_failure(&self) {
        self.permanent_failure.store(true, Ordering::Relaxed);
    }

    fn json(&self) -> Value {
        json!({
            "type": self.kind,
            "source": self.source,
            "metrics": {
                "updates_received_total": self.updates_received.load(Ordering::Relaxed),
                "poll_failures_total": self.poll_failures.load(Ordering::Relaxed),
                "permanent_failure": self.permanent_failure.load(Ordering::Relaxed),
            }
        })
    }
}

/// Process-global metrics state. `route` is set once at startup and read-only
/// thereafter; `updaters` grows once per ingress adapter during boot.
struct Registry {
    route: OnceLock<Arc<dyn RouteableComponent>>,
    updaters: ArcSwap<Vec<Arc<UpdaterMetrics>>>,
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(|| Registry {
    route: OnceLock::new(),
    updaters: ArcSwap::from_pointee(Vec::new()),
});

/// Updates dropped at a load balancer that was drained empty at runtime.
/// Process-global because the drop happens deep inside `process` with no
/// handle back to per-LB state, and the count is small operator signal.
static LB_DROPPED_EMPTY: AtomicU64 = AtomicU64::new(0);

/// Record the routing tree root so `/status` and `/metrics` can introspect
/// it. Set once at startup; later calls are ignored.
pub fn set_route(route: Arc<dyn RouteableComponent>) {
    let _ = REGISTRY.route.set(route);
}

/// Register one updater's metrics handle so it surfaces on `/status` and
/// `/metrics`. Copy-on-write append, matching the `ArcSwap` idiom used for LB
/// children lists.
pub fn register_updater(m: Arc<UpdaterMetrics>) {
    REGISTRY.updaters.rcu(|cur| {
        let mut next = (**cur).clone();
        next.push(m.clone());
        Arc::new(next)
    });
}

pub fn record_lb_drop() {
    LB_DROPPED_EMPTY.fetch_add(1, Ordering::Relaxed);
}

fn lb_dropped() -> u64 {
    LB_DROPPED_EMPTY.load(Ordering::Relaxed)
}

#[cfg(test)]
pub fn lb_dropped_for_test() -> u64 {
    lb_dropped()
}

/// Redact any Telegram token embedded in `s`.
fn redact(s: &str) -> Cow<'_, str> {
    TELEGRAM_TOKEN_RE.replace_all(s, "#####")
}

/// Redact, then escape for a Prometheus label value (`\`, `"`, newline).
fn sanitize_label(s: &str) -> String {
    redact(s)
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// A leaf route's metrics, extracted from a `json_struct` tree.
enum LeafSample {
    LongPoll {
        label: String,
        queue_depth: u64,
        queue_dropped: u64,
        served: u64,
    },
    Webhook {
        label: String,
        dispatched: u64,
        success: u64,
        failure: u64,
        error: u64,
    },
}

fn num(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// Recursively collect leaf-route metrics from a `json_struct` tree.
/// Pure (no globals) so it is unit-testable against a hand-built tree.
fn collect_leaves(node: &Value, out: &mut Vec<LeafSample>) {
    match node.get("type").and_then(Value::as_str) {
        Some("load-balancer") => {
            if let Some(arr) = node.get("routes").and_then(Value::as_array) {
                for c in arr {
                    collect_leaves(c, out);
                }
            }
        }
        Some("longpoll") => {
            let label = sanitize_label(node["options"]["path"].as_str().unwrap_or(""));
            let m = &node["metrics"];
            out.push(LeafSample::LongPoll {
                label,
                queue_depth: num(m, "queue_depth"),
                queue_dropped: num(m, "queue_dropped_total"),
                served: num(m, "served_total"),
            });
        }
        Some("webhook") => {
            let label = sanitize_label(node["options"]["url"].as_str().unwrap_or(""));
            let m = &node["metrics"];
            out.push(LeafSample::Webhook {
                label,
                dispatched: num(m, "dispatched_total"),
                success: num(m, "http_success_total"),
                failure: num(m, "http_failure_total"),
                error: num(m, "http_error_total"),
            });
        }
        // "mock"/unknown leaves carry no metrics.
        _ => {}
    }
}

/// Build the `/status` JSON document from explicit inputs (no globals).
fn build_status(
    updaters: &[Arc<UpdaterMetrics>],
    route_tree: Option<&Value>,
    lb_dropped: u64,
) -> Value {
    json!({
        "updaters": updaters.iter().map(|m| m.json()).collect::<Vec<_>>(),
        "routes": route_tree.cloned().unwrap_or(Value::Null),
        "load_balancer": { "dropped_empty_total": lb_dropped },
    })
}

/// Render the Prometheus text exposition from explicit inputs (no globals).
fn render_prometheus(
    updaters: &[Arc<UpdaterMetrics>],
    route_tree: Option<&Value>,
    lb_dropped: u64,
) -> String {
    let mut leaves = Vec::new();
    if let Some(t) = route_tree {
        collect_leaves(t, &mut leaves);
    }

    let mut o = String::new();

    // --- updaters ---
    let _ = writeln!(o, "# TYPE tgin_updater_updates_received_total counter");
    for m in updaters {
        let _ = writeln!(
            o,
            "tgin_updater_updates_received_total{{source=\"{}\"}} {}",
            sanitize_label(&m.source),
            m.updates_received.load(Ordering::Relaxed)
        );
    }
    let _ = writeln!(o, "# TYPE tgin_updater_poll_failures_total counter");
    for m in updaters {
        let _ = writeln!(
            o,
            "tgin_updater_poll_failures_total{{source=\"{}\"}} {}",
            sanitize_label(&m.source),
            m.poll_failures.load(Ordering::Relaxed)
        );
    }
    let _ = writeln!(o, "# TYPE tgin_updater_permanent_failure gauge");
    for m in updaters {
        let _ = writeln!(
            o,
            "tgin_updater_permanent_failure{{source=\"{}\"}} {}",
            sanitize_label(&m.source),
            m.permanent_failure.load(Ordering::Relaxed) as u8
        );
    }

    // --- long-poll routes ---
    let _ = writeln!(o, "# TYPE tgin_route_queue_depth gauge");
    for l in &leaves {
        if let LeafSample::LongPoll { label, queue_depth, .. } = l {
            let _ = writeln!(o, "tgin_route_queue_depth{{route=\"{label}\"}} {queue_depth}");
        }
    }
    let _ = writeln!(o, "# TYPE tgin_route_queue_dropped_total counter");
    for l in &leaves {
        if let LeafSample::LongPoll { label, queue_dropped, .. } = l {
            let _ = writeln!(
                o,
                "tgin_route_queue_dropped_total{{route=\"{label}\"}} {queue_dropped}"
            );
        }
    }
    let _ = writeln!(o, "# TYPE tgin_route_served_total counter");
    for l in &leaves {
        if let LeafSample::LongPoll { label, served, .. } = l {
            let _ = writeln!(o, "tgin_route_served_total{{route=\"{label}\"}} {served}");
        }
    }

    // --- webhook routes ---
    let _ = writeln!(o, "# TYPE tgin_route_dispatched_total counter");
    for l in &leaves {
        if let LeafSample::Webhook { label, dispatched, .. } = l {
            let _ = writeln!(
                o,
                "tgin_route_dispatched_total{{route=\"{label}\"}} {dispatched}"
            );
        }
    }
    let _ = writeln!(o, "# TYPE tgin_route_http_responses_total counter");
    for l in &leaves {
        if let LeafSample::Webhook { label, success, failure, error, .. } = l {
            let _ = writeln!(
                o,
                "tgin_route_http_responses_total{{route=\"{label}\",result=\"success\"}} {success}"
            );
            let _ = writeln!(
                o,
                "tgin_route_http_responses_total{{route=\"{label}\",result=\"failure\"}} {failure}"
            );
            let _ = writeln!(
                o,
                "tgin_route_http_responses_total{{route=\"{label}\",result=\"error\"}} {error}"
            );
        }
    }

    // --- load balancers ---
    let _ = writeln!(o, "# TYPE tgin_lb_dropped_empty_total counter");
    let _ = writeln!(o, "tgin_lb_dropped_empty_total {lb_dropped}");

    o
}

/// Snapshot the routing tree via `json_struct`, or `None` if not yet set.
async fn route_tree() -> Option<Value> {
    match REGISTRY.route.get() {
        Some(r) => Some(r.json_struct().await),
        None => None,
    }
}

pub async fn status_json() -> Value {
    let updaters = REGISTRY.updaters.load_full();
    build_status(&updaters, route_tree().await.as_ref(), lb_dropped())
}

pub async fn prometheus_text() -> String {
    let updaters = REGISTRY.updaters.load_full();
    render_prometheus(&updaters, route_tree().await.as_ref(), lb_dropped())
}

pub async fn status_handler() -> Json<Value> {
    Json(status_json().await)
}

pub async fn metrics_handler() -> Response {
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4")],
        prometheus_text().await,
    )
        .into_response()
}

/// Span correlating one update's hops through the routing tree. `update_id`
/// is parsed off the wire bytes only when an info-level span would actually
/// record, so the dispatch hot path pays nothing when info logging is off.
pub fn dispatch_span(update: &Bytes) -> tracing::Span {
    let span = tracing::info_span!("dispatch", update_id = tracing::field::Empty);
    if !span.is_disabled() {
        #[derive(serde::Deserialize)]
        struct Peek {
            update_id: i64,
        }
        if let Ok(p) = serde_json::from_slice::<Peek>(update) {
            span.record("update_id", p.update_id);
        }
    }
    span
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tree() -> Value {
        json!({
            "type": "load-balancer",
            "name": "round-robin",
            "routes": [
                {
                    "type": "longpoll",
                    "options": { "path": "/p/getUpdates" },
                    "metrics": { "queue_depth": 3, "queue_dropped_total": 2, "served_total": 5 }
                },
                {
                    "type": "webhook",
                    "options": { "url": "http://d/bot" },
                    "metrics": {
                        "dispatched_total": 4,
                        "http_success_total": 3,
                        "http_failure_total": 1,
                        "http_error_total": 0
                    }
                }
            ]
        })
    }

    #[test]
    fn test_collect_leaves_extracts_metrics() {
        let mut leaves = Vec::new();
        collect_leaves(&sample_tree(), &mut leaves);
        assert_eq!(leaves.len(), 2);

        match &leaves[0] {
            LeafSample::LongPoll { label, queue_depth, queue_dropped, served } => {
                assert_eq!(label, "/p/getUpdates");
                assert_eq!(*queue_depth, 3);
                assert_eq!(*queue_dropped, 2);
                assert_eq!(*served, 5);
            }
            _ => panic!("expected first leaf to be long-poll"),
        }
        match &leaves[1] {
            LeafSample::Webhook { label, dispatched, success, failure, error } => {
                assert_eq!(label, "http://d/bot");
                assert_eq!(*dispatched, 4);
                assert_eq!(*success, 3);
                assert_eq!(*failure, 1);
                assert_eq!(*error, 0);
            }
            _ => panic!("expected second leaf to be webhook"),
        }
    }

    #[test]
    fn test_render_prometheus_emits_and_redacts() {
        let token_source =
            "https://api.telegram.org/bot12345678:AAAAbbbbccccddddeeeeffffgggghhhhiii/getUpdates";
        let updaters = vec![UpdaterMetrics::new("longpoll", token_source.into())];
        let tree = sample_tree();
        let out = render_prometheus(&updaters, Some(&tree), 7);

        assert!(out.contains("tgin_lb_dropped_empty_total 7"));
        assert!(out.contains("tgin_route_served_total{route=\"/p/getUpdates\"} 5"));
        assert!(out.contains(
            "tgin_route_http_responses_total{route=\"http://d/bot\",result=\"failure\"} 1"
        ));
        assert!(out.contains("# TYPE tgin_updater_updates_received_total counter"));

        // Redaction: neither the token id nor its secret may appear.
        assert!(!out.contains("12345678"));
        assert!(!out.contains("AAAAbbbbccccddddeeeeffffgggghhhhiii"));
        assert!(out.contains("#####"));
    }

    #[test]
    fn test_build_status_shape() {
        let updaters = vec![UpdaterMetrics::new("longpoll", "src".into())];
        let tree = sample_tree();
        let v = build_status(&updaters, Some(&tree), 7);

        assert_eq!(v["routes"]["type"], "load-balancer");
        assert_eq!(v["updaters"][0]["type"], "longpoll");
        assert_eq!(v["load_balancer"]["dropped_empty_total"], 7);
    }

    #[test]
    fn test_updater_metrics_counters() {
        let m = UpdaterMetrics::new("webhook", "0.0.0.0/in".into());
        m.record_update();
        m.record_update();
        m.record_poll_failure();
        m.mark_permanent_failure();

        let j = m.json();
        assert_eq!(j["metrics"]["updates_received_total"], 2);
        assert_eq!(j["metrics"]["poll_failures_total"], 1);
        assert_eq!(j["metrics"]["permanent_failure"], true);
    }

    #[test]
    fn test_sanitize_label_escapes() {
        assert_eq!(sanitize_label("a\"b"), "a\\\"b");
        assert_eq!(sanitize_label("a\\b"), "a\\\\b");
        assert_eq!(sanitize_label("a\nb"), "a\\nb");
    }
}
