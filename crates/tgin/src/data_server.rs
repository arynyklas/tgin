//! The data plane's routes: webhook ingress, the Bot-API-shaped pull
//! endpoints (`/c/{consumer}/getUpdates` and each alias) and, until the admin
//! plane takes them, `/livez` and `/readyz`.
//!
//! The listeners serve [`delegating`], which hands every request to the
//! router current in an `ArcSwap`, so routes can change without restarting
//! the listeners.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, MethodRouter};
use axum::Router;
use http_body::{Frame, SizeHint};
use serde_json::{json, Value};
use tgin_botapi::{IpAllowlist, IpNet, WebhookOptions, DEFAULT_WEBHOOK_BODY_LIMIT};
use tgin_core::config::{self, Config, ConsumerKind, Ingress, Server};
use tgin_core::consumer::pull::PollRequest;
use tgin_core::engine::Engine;
use tgin_runtime::lifecycle::{Lifecycle, Phase};
use tgin_runtime::obs::{Health, Readiness};
use tgin_runtime::queue::HolderId;
use tgin_runtime::secret::{secret_eq, Secret};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

use crate::app;

/// How long a webhook request may take, body included, so a client that
/// trickles its body cannot hold a connection. Pull routes have no such
/// bound: their long polls last up to 50 s.
pub(crate) const WEBHOOK_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest `getUpdates` request body; its parameters fit in a few bytes.
const PULL_BODY_LIMIT: usize = 64 * 1024;
/// `getUpdates`'s default and largest `limit`.
const MAX_LIMIT: i64 = 100;
/// The longest `getUpdates` `timeout`, in seconds.
const MAX_TIMEOUT_SECS: i64 = 50;
/// The header a downstream instance names itself with.
const INSTANCE_HEADER: &str = "x-tgin-instance";
/// The longest instance name.
const MAX_INSTANCE_LEN: usize = 64;

/// What the probes report on.
#[derive(Clone)]
pub(crate) struct Probes {
    pub(crate) lifecycle: Arc<Lifecycle>,
    pub(crate) health: Arc<Health>,
}

/// The data plane's routes for `cfg`, served by `engine`. Webhook routes
/// answer 503 once `ingest` is cancelled.
pub(crate) fn router(
    cfg: &Config,
    server: &Server,
    engine: &Arc<Engine>,
    ingest: &CancellationToken,
    probes: Probes,
) -> Router {
    let mut router = Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz).with_state(probes));

    for bot in &cfg.bots {
        let Some(Ingress::Webhook(hook)) = &bot.ingress else {
            continue;
        };
        let source = engine.source_id(&bot.name).and_then(|id| {
            let state = engine.state();
            let info = state.sources.get(usize::from(id.0))?.as_ref()?;
            Some((id, info.metrics.clone()))
        });
        let Some((id, metrics)) = source else {
            tracing::error!(source = %bot.name, "the engine has no source for this webhook bot; its route is not served");
            continue;
        };
        let opts = WebhookOptions {
            path: hook.path.expose().clone(),
            secret: app::webhook_secret(bot, hook),
            ip_allowlist: match &hook.ip_allowlist {
                config::IpAllowlist::None => IpAllowlist::None,
                config::IpAllowlist::Telegram => IpAllowlist::Telegram,
                config::IpAllowlist::Networks(nets) => IpAllowlist::Cidrs(nets.clone()),
            },
            trusted_proxies: server.trusted_proxies.clone(),
            body_limit: DEFAULT_WEBHOOK_BODY_LIMIT,
            register: None,
        };
        let webhook = tgin_botapi::webhook_router(
            id,
            Arc::from(bot.name.as_str()),
            opts,
            Arc::clone(engine),
            ingest.clone(),
            metrics,
        );
        router = router.merge(with_timeout(webhook, WEBHOOK_REQUEST_TIMEOUT));
    }

    let mut auth = HashMap::new();
    let mut aliases = Vec::new();
    for consumer in &cfg.consumers {
        let ConsumerKind::Pull(pull) = &consumer.kind else {
            continue;
        };
        let name: Arc<str> = Arc::from(consumer.name.as_str());
        auth.insert(Arc::clone(&name), pull.auth_token.clone());
        for alias in &pull.aliases {
            aliases.push((alias.expose().clone(), Arc::clone(&name)));
        }
    }
    let pull = Arc::new(PullRoutes {
        engine: Arc::clone(engine),
        auth,
        trusted_proxies: server.trusted_proxies.clone(),
    });
    router = router.route(
        "/c/{consumer}/getUpdates",
        get(by_path).post(by_path).with_state(Arc::clone(&pull)),
    );
    for (path, name) in aliases {
        router = router.route(&path, alias(&pull, name));
    }
    router
}

/// A router that hands every request to the router current in `routes`;
/// requests already running finish on the router they started on.
pub(crate) fn delegating(routes: Arc<ArcSwap<Router>>) -> Router {
    Router::new().fallback_service(Delegate(routes))
}

#[derive(Clone)]
struct Delegate(Arc<ArcSwap<Router>>);

impl tower::Service<Request> for Delegate {
    type Response = Response;
    type Error = Infallible;
    type Future = tower::util::Oneshot<Router, Request>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        Router::clone(&self.0.load()).oneshot(req)
    }
}

/// Answers 408 for a request still running after `timeout`.
fn with_timeout(router: Router, timeout: Duration) -> Router {
    router.layer(middleware::from_fn_with_state(timeout, request_timeout))
}

async fn request_timeout(State(timeout): State<Duration>, req: Request, next: Next) -> Response {
    match tokio::time::timeout(timeout, next.run(req)).await {
        Ok(response) => response,
        Err(_elapsed) => StatusCode::REQUEST_TIMEOUT.into_response(),
    }
}

async fn livez() -> &'static str {
    "ok"
}

/// 200 while running with every component ready, else 503; the body lists
/// each component's readiness.
async fn readyz(State(probes): State<Probes>) -> Response {
    let (components_ready, components) = probes.health.ready();
    let phase = probes.lifecycle.phase();
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
        "phase": format!("{phase:?}").to_lowercase(),
        "components": components,
    });
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, axum::Json(body)).into_response()
}

/// The pull consumers the data plane serves.
struct PullRoutes {
    engine: Arc<Engine>,
    /// Every pull consumer of the config, with its bearer token if it has
    /// one. A consumer not listed is not served.
    auth: HashMap<Arc<str>, Option<Secret<String>>>,
    trusted_proxies: Vec<IpNet>,
}

async fn by_path(
    State(pull): State<Arc<PullRoutes>>,
    Path(consumer): Path<String>,
    req: Request,
) -> Response {
    pull.get_updates(&consumer, req).await
}

fn alias(pull: &Arc<PullRoutes>, name: Arc<str>) -> MethodRouter {
    let handler = move |State(pull): State<Arc<PullRoutes>>, req: Request| {
        let name = Arc::clone(&name);
        async move { pull.get_updates(&name, req).await }
    };
    get(handler.clone())
        .post(handler)
        .with_state(Arc::clone(pull))
}

impl PullRoutes {
    /// One `getUpdates` poll of the consumer `name`.
    async fn get_updates(&self, name: &str, req: Request) -> Response {
        let Some(token) = self.auth.get(name) else {
            return telegram_error(StatusCode::NOT_FOUND, "Not Found");
        };
        let (parts, body) = req.into_parts();
        if let Some(expected) = token {
            if !bearer_matches(&parts.headers, expected) {
                return telegram_error(StatusCode::UNAUTHORIZED, "Unauthorized");
            }
        }
        let Some(consumer) = self.engine.pull_consumer(name) else {
            return telegram_error(StatusCode::NOT_FOUND, "Not Found");
        };
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(peer)| *peer);
        let instance = match instance(&parts.headers, peer, &self.trusted_proxies) {
            Ok(instance) => instance,
            Err(description) => return bad_request(description),
        };
        let body = match axum::body::to_bytes(body, PULL_BODY_LIMIT).await {
            Ok(body) => body,
            Err(_) => return bad_request("the request body could not be read"),
        };
        let content_type = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        let params = match Params::parse(parts.uri.query(), content_type, &body) {
            Ok(params) => params,
            Err(description) => return bad_request(&description),
        };

        // Cancelled when the client goes away: hyper drops this future,
        // and with it the guard.
        let cancel = CancellationToken::new();
        let _disconnect = cancel.clone().drop_guard();
        let polled = consumer
            .poll(
                PollRequest {
                    instance,
                    offset: params.offset,
                    limit: params.limit,
                    timeout: params.timeout,
                },
                &cancel,
            )
            .await;
        batch_response(polled.items)
    }
}

/// Whether `headers` carry `Authorization: Bearer <expected>`.
fn bearer_matches(headers: &HeaderMap, expected: &Secret<String>) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let value = value.as_bytes();
    let scheme = b"bearer ";
    value.len() > scheme.len()
        && value[..scheme.len()].eq_ignore_ascii_case(scheme)
        && secret_eq(&value[scheme.len()..], expected.expose().as_bytes())
}

/// The polling instance: `X-Tgin-Instance` if sent, else the client
/// address (forwarded only by a trusted proxy).
fn instance(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted_proxies: &[IpNet],
) -> Result<HolderId, &'static str> {
    if let Some(value) = headers.get(INSTANCE_HEADER) {
        return instance_name(value).map(|name| HolderId(Arc::from(name)));
    }
    peer.and_then(|peer| tgin_botapi::client_ip(peer.ip(), headers, trusted_proxies))
        .map(|ip| HolderId(Arc::from(ip.to_string())))
        .ok_or("the client address is unknown; send X-Tgin-Instance")
}

fn instance_name(value: &HeaderValue) -> Result<&str, &'static str> {
    const INVALID: &str = "X-Tgin-Instance must be 1 to 64 visible ASCII characters";
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_INSTANCE_LEN || !bytes.iter().all(u8::is_ascii_graphic)
    {
        return Err(INVALID);
    }
    value.to_str().map_err(|_| INVALID)
}

/// `getUpdates` parameters, clamped as Telegram clamps them.
#[derive(Debug, PartialEq, Eq)]
struct Params {
    offset: i64,
    /// 1 to 100; 100 when not given.
    limit: usize,
    /// 0 to 50 s; 0 when not given.
    timeout: Duration,
}

/// Parameters as sent, before defaults and clamping.
#[derive(Default)]
struct RawParams {
    offset: Option<i64>,
    limit: Option<i64>,
    timeout: Option<i64>,
}

impl RawParams {
    fn set(&mut self, key: &str, value: Result<i64, ()>) -> Result<(), String> {
        let slot = match key {
            "offset" => &mut self.offset,
            "limit" => &mut self.limit,
            "timeout" => &mut self.timeout,
            // `allowed_updates` and anything else: routing is the config's.
            _ => return Ok(()),
        };
        *slot = Some(value.map_err(|()| format!("{key} must be an integer"))?);
        Ok(())
    }

    fn form(&mut self, bytes: &[u8]) -> Result<(), String> {
        for (key, value) in url::form_urlencoded::parse(bytes) {
            self.set(&key, value.trim().parse().map_err(|_| ()))?;
        }
        Ok(())
    }

    fn json(&mut self, body: &[u8]) -> Result<(), String> {
        let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
            return Err("the body must be a JSON object".to_owned());
        };
        for (key, value) in &fields {
            let value = match value {
                Value::Number(n) => n.as_i64().ok_or(()),
                Value::String(s) => s.trim().parse().map_err(|_| ()),
                _ => Err(()),
            };
            self.set(key, value)?;
        }
        Ok(())
    }
}

impl Params {
    /// From the query string, then the body, whose values win: JSON when
    /// the content type says so, otherwise a form.
    fn parse(query: Option<&str>, content_type: Option<&str>, body: &[u8]) -> Result<Self, String> {
        let mut raw = RawParams::default();
        if let Some(query) = query {
            raw.form(query.as_bytes())?;
        }
        if !body.is_empty() {
            let media_type = content_type
                .and_then(|ct| ct.split(';').next())
                .map(str::trim)
                .unwrap_or_default();
            if media_type.eq_ignore_ascii_case("application/json") {
                raw.json(body)?;
            } else if media_type.eq_ignore_ascii_case("multipart/form-data") {
                return Err("send the parameters as a query string, a form or JSON".to_owned());
            } else {
                raw.form(body)?;
            }
        }
        let limit = raw.limit.map_or(MAX_LIMIT, |n| n.clamp(1, MAX_LIMIT));
        let timeout = raw.timeout.map_or(0, |n| n.clamp(0, MAX_TIMEOUT_SECS));
        Ok(Self {
            offset: raw.offset.unwrap_or(0),
            limit: usize::try_from(limit).unwrap_or(1),
            timeout: Duration::from_secs(u64::try_from(timeout).unwrap_or(0)),
        })
    }
}

/// `{"ok":false,"error_code":…,"description":…}` with the same HTTP status,
/// as the Bot API answers.
fn telegram_error(status: StatusCode, description: &str) -> Response {
    let body = json!({
        "ok": false,
        "error_code": status.as_u16(),
        "description": description,
    });
    (status, axum::Json(body)).into_response()
}

fn bad_request(detail: &str) -> Response {
    telegram_error(StatusCode::BAD_REQUEST, &format!("Bad Request: {detail}"))
}

/// `{"ok":true,"result":[…]}` around the payloads, which are sent as they
/// are, never copied: the body is the sequence of chunks itself.
fn batch_response(items: Vec<Bytes>) -> Response {
    let mut chunks = VecDeque::with_capacity(items.len() * 2 + 1);
    chunks.push_back(Bytes::from_static(b"{\"ok\":true,\"result\":["));
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            chunks.push_back(Bytes::from_static(b","));
        }
        chunks.push_back(item);
    }
    chunks.push_back(Bytes::from_static(b"]}"));
    let remaining = chunks.iter().map(|c| c.len() as u64).sum();
    (
        [(header::CONTENT_TYPE, "application/json")],
        Body::new(Chunks { chunks, remaining }),
    )
        .into_response()
}

/// A body of ready chunks with an exact length, so it goes out with a
/// `Content-Length` and each chunk is written as it is.
struct Chunks {
    chunks: VecDeque<Bytes>,
    remaining: u64,
}

impl HttpBody for Chunks {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        Poll::Ready(this.chunks.pop_front().map(|chunk| {
            this.remaining -= chunk.len() as u64;
            Ok(Frame::data(chunk))
        }))
    }

    fn is_end_stream(&self) -> bool {
        self.chunks.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

#[cfg(test)]
mod tests {
    use axum::routing::post;
    use futures_util::stream;

    use super::*;

    fn params(query: Option<&str>, content_type: Option<&str>, body: &str) -> Params {
        Params::parse(query, content_type, body.as_bytes()).expect("valid parameters")
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn params_default_and_clamp() {
        assert_eq!(
            params(None, None, ""),
            Params {
                offset: 0,
                limit: 100,
                timeout: secs(0)
            }
        );
        assert_eq!(
            params(Some("offset=-1&limit=0&timeout=-5"), None, ""),
            Params {
                offset: -1,
                limit: 1,
                timeout: secs(0)
            }
        );
        assert_eq!(
            params(Some("limit=1000&timeout=600&allowed_updates=[]"), None, ""),
            Params {
                offset: 0,
                limit: 100,
                timeout: secs(50)
            }
        );
    }

    #[test]
    fn params_from_form_and_json_bodies_win_over_the_query() {
        let query = Some("offset=1&limit=2&timeout=3");
        assert_eq!(
            params(
                query,
                Some("application/x-www-form-urlencoded"),
                "offset=10&timeout=30"
            ),
            Params {
                offset: 10,
                limit: 2,
                timeout: secs(30)
            }
        );
        assert_eq!(
            params(
                query,
                Some("application/json; charset=utf-8"),
                r#"{"offset":"11","limit":5,"allowed_updates":["message"]}"#
            ),
            Params {
                offset: 11,
                limit: 5,
                timeout: secs(3)
            }
        );
        // Without a content type a body is read as a form.
        assert_eq!(params(None, None, "limit=7").limit, 7);
    }

    #[test]
    fn params_reject_malformed_values() {
        for (query, content_type, body) in [
            (Some("offset=abc"), None, ""),
            (None, Some("application/json"), "[1]"),
            (None, Some("application/json"), r#"{"limit":1.5}"#),
            (None, Some("application/json"), "{"),
            (None, Some("multipart/form-data; boundary=x"), "--x--"),
        ] {
            assert!(
                Params::parse(query, content_type, body.as_bytes()).is_err(),
                "{query:?} {content_type:?} {body:?}"
            );
        }
    }

    #[test]
    fn instance_from_header_or_address() {
        let peer = Some(SocketAddr::from(([10, 0, 0, 1], 5000)));
        let proxies: Vec<IpNet> = vec!["10.0.0.0/8".parse().unwrap()];
        let mut headers = HeaderMap::new();
        assert_eq!(
            instance(&headers, peer, &[]).unwrap(),
            HolderId(Arc::from("10.0.0.1"))
        );
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        assert_eq!(
            instance(&headers, peer, &[]).unwrap(),
            HolderId(Arc::from("10.0.0.1"))
        );
        assert_eq!(
            instance(&headers, peer, &proxies).unwrap(),
            HolderId(Arc::from("1.2.3.4"))
        );
        assert!(instance(&HeaderMap::new(), None, &[]).is_err());

        headers.insert(INSTANCE_HEADER, HeaderValue::from_static("worker-1"));
        assert_eq!(
            instance(&headers, None, &[]).unwrap(),
            HolderId(Arc::from("worker-1"))
        );
        let longest = "x".repeat(64);
        headers.insert(INSTANCE_HEADER, HeaderValue::from_str(&longest).unwrap());
        assert!(instance(&headers, None, &[]).is_ok());
        for bad in ["", "two words", &"x".repeat(65)] {
            headers.insert(INSTANCE_HEADER, HeaderValue::from_str(bad).unwrap());
            assert!(instance(&headers, peer, &[]).is_err(), "{bad:?}");
        }
        headers.insert(
            INSTANCE_HEADER,
            HeaderValue::from_bytes("é".as_bytes()).unwrap(),
        );
        assert!(instance(&headers, peer, &[]).is_err());
    }

    #[test]
    fn bearer_needs_scheme_and_exact_token() {
        let expected = Secret::new("pull-secret-123".to_owned());
        let check = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, HeaderValue::from_static(value));
            bearer_matches(&headers, &expected)
        };
        assert!(check("Bearer pull-secret-123"));
        assert!(check("bearer pull-secret-123"));
        assert!(!check("Bearer pull-secret-12"));
        assert!(!check("Bearer pull-secret-1234"));
        assert!(!check("Basic pull-secret-123"));
        assert!(!check("pull-secret-123"));
        assert!(!bearer_matches(&HeaderMap::new(), &expected));
    }

    #[tokio::test]
    async fn batch_body_is_exact_and_uncopied() {
        let first = Bytes::from_static(br#"{"update_id":1}"#);
        let second = Bytes::from_static(br#"{"update_id":2}"#);
        let response = batch_response(vec![first.clone(), second.clone()]);
        let mut body = response.into_body();
        assert_eq!(
            body.size_hint().exact(),
            Some((21 + first.len() + 1 + second.len() + 2) as u64)
        );
        let mut chunks = Vec::new();
        while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
        {
            chunks.push(frame.unwrap().into_data().unwrap());
        }
        let joined: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
        assert_eq!(
            joined,
            br#"{"ok":true,"result":[{"update_id":1},{"update_id":2}]}"#
        );
        assert_eq!(chunks[1].as_ptr(), first.as_ptr());
        assert_eq!(chunks[3].as_ptr(), second.as_ptr());

        let empty = axum::body::to_bytes(batch_response(Vec::new()).into_body(), 64)
            .await
            .unwrap();
        assert_eq!(&empty[..], br#"{"ok":true,"result":[]}"#);
    }

    #[tokio::test(start_paused = true)]
    async fn trickled_request_times_out() {
        let router = with_timeout(
            Router::new().route(
                "/hook",
                post(|body: Bytes| async move { body.len().to_string() }),
            ),
            WEBHOOK_REQUEST_TIMEOUT,
        );
        let never_ends = stream::pending::<Result<Bytes, Infallible>>();
        let request = Request::post("/hook")
            .body(Body::from_stream(never_ends))
            .unwrap();
        let started = tokio::time::Instant::now();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(started.elapsed(), WEBHOOK_REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn delegating_serves_the_current_router() {
        let routes = Arc::new(ArcSwap::from_pointee(
            Router::new().route("/", get(|| async { "first" })),
        ));
        let served = delegating(Arc::clone(&routes));
        let text = |response: Response| async move {
            let body = axum::body::to_bytes(response.into_body(), 64)
                .await
                .unwrap();
            String::from_utf8(body.to_vec()).unwrap()
        };
        let request = || Request::get("/").body(Body::empty()).unwrap();
        assert_eq!(
            text(served.clone().oneshot(request()).await.unwrap()).await,
            "first"
        );
        routes.store(Arc::new(
            Router::new().route("/", get(|| async { "second" })),
        ));
        assert_eq!(
            text(served.oneshot(request()).await.unwrap()).await,
            "second"
        );
    }
}
