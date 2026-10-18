//! The data plane's routes: webhook ingress, the Bot-API-shaped pull
//! endpoints (`/c/{consumer}/getUpdates` and each alias, served by
//! [`tgin_botapi::pull`]), and the Bot API gateway as the fallback for every
//! other path.
//!
//! The listeners serve [`DataRoutes::service`], which hands every request to
//! the router current in an `ArcSwap`, so a reload can change the routes
//! without restarting the listeners.

use std::collections::{BTreeMap, HashSet};
use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};
use parking_lot::Mutex;
use tgin_botapi::gateway::{Gateway, errors};
use tgin_botapi::pull::{PullAuth, PullRoutes};
use tgin_botapi::{DEFAULT_WEBHOOK_BODY_LIMIT, IpAllowlist, WebhookOptions};
use tgin_core::config::{self, Config, ConsumerKind, Ingress, Server};
use tgin_core::engine::Engine;
use tgin_runtime::http::{Via, refuse_loops};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

use crate::app;

/// How long a webhook request may take, body included, so a client that
/// trickles its body cannot hold a connection. Pull routes have no such
/// bound: their long polls last up to 50 s.
pub(crate) const WEBHOOK_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What a router serves of the pull consumers: who is served, with which
/// token, and the alias paths that reach them.
#[derive(Clone, Default)]
struct Served {
    auth: PullAuth,
    /// Alias path → consumer.
    aliases: BTreeMap<String, Arc<str>>,
}

/// The data plane's current routes, and the pull consumers they serve.
pub(crate) struct DataRoutes {
    routes: Arc<ArcSwap<Router>>,
    served: Mutex<Served>,
    /// Every router's fallback; rebuilds keep it.
    gateway: Arc<Gateway>,
    /// This process's `Via`: a request carrying it is one of tgin's own
    /// pushes come back, and is refused.
    via: Via,
}

impl DataRoutes {
    /// The routes of `cfg`, served by `engine`, with `gateway` serving
    /// every path they do not. Webhook routes answer 503 once `ingest` is
    /// cancelled.
    pub(crate) fn new(
        cfg: &Config,
        server: &Server,
        engine: &Arc<Engine>,
        ingest: &CancellationToken,
        gateway: Arc<Gateway>,
        via: Via,
    ) -> Self {
        let served = served(cfg, &Served::default(), engine);
        let router = router(cfg, server, engine, ingest, served.clone(), &gateway);
        Self {
            routes: Arc::new(ArcSwap::from_pointee(router)),
            served: Mutex::new(served),
            gateway,
            via,
        }
    }

    /// What the listeners serve: the current routes, swapped in by
    /// [`DataRoutes::rebuild`], behind the loop check: a request whose
    /// `Via` names this process gets `508 Loop Detected`, Telegram-shaped
    /// like every data-plane error.
    pub(crate) fn service(&self) -> Router {
        refuse_loops(
            delegating(Arc::clone(&self.routes)),
            self.via.clone(),
            "data",
            || errors::error(StatusCode::LOOP_DETECTED, "Loop Detected"),
        )
    }

    /// Builds the routes of `cfg` against the engine as it is now and serves
    /// them from the next request on; requests already running finish on
    /// the old routes. The gateway stays the fallback.
    ///
    /// A pull consumer that `cfg` no longer has is still served while the
    /// engine drains it, on `/c/<name>/getUpdates` and on its aliases (a
    /// path `cfg` gives to something else goes to the new owner), with the
    /// token it had, so its clients can fetch and ack what it holds. It
    /// leaves the routes at the first rebuild after its drain.
    pub(crate) fn rebuild(
        &self,
        cfg: &Config,
        server: &Server,
        engine: &Arc<Engine>,
        ingest: &CancellationToken,
    ) {
        let mut current = self.served.lock();
        let served = served(cfg, &current, engine);
        self.routes.store(Arc::new(router(
            cfg,
            server,
            engine,
            ingest,
            served.clone(),
            &self.gateway,
        )));
        *current = served;
    }
}

/// The pull consumers of `cfg`, with their tokens and aliases, plus those
/// of `previous` that `cfg` dropped but `engine` still drains, with the
/// token and the aliases they had, except for paths `cfg` routes itself.
fn served(cfg: &Config, previous: &Served, engine: &Engine) -> Served {
    let mut served = Served::default();
    for consumer in &cfg.consumers {
        let ConsumerKind::Pull(pull) = &consumer.kind else {
            continue;
        };
        let name: Arc<str> = Arc::from(consumer.name.as_str());
        served
            .auth
            .insert(Arc::clone(&name), pull.auth_token.clone());
        for alias in &pull.aliases {
            served
                .aliases
                .insert(alias.expose().clone(), Arc::clone(&name));
        }
    }
    let current: HashSet<Arc<str>> = served.auth.keys().cloned().collect();
    let webhooks: HashSet<&str> = cfg
        .bots
        .iter()
        .filter_map(|bot| match &bot.ingress {
            Some(Ingress::Webhook(hook)) => Some(hook.path.expose().as_str()),
            _ => None,
        })
        .collect();
    for (name, token) in &previous.auth {
        if !current.contains(name) && engine.pull_consumer(name).is_some() {
            served.auth.insert(Arc::clone(name), token.clone());
        }
    }
    for (path, name) in &previous.aliases {
        let retired = !current.contains(name) && served.auth.contains_key(name);
        if retired && !served.aliases.contains_key(path) && !webhooks.contains(path.as_str()) {
            served.aliases.insert(path.clone(), Arc::clone(name));
        }
    }
    served
}

/// The routes of `cfg` and of the pull consumers `served` lists, with
/// `gateway` as the fallback. Only webhook routes have a request timeout:
/// gateway transfers last as long as their bytes keep flowing.
fn router(
    cfg: &Config,
    server: &Server,
    engine: &Arc<Engine>,
    ingest: &CancellationToken,
    served: Served,
    gateway: &Arc<Gateway>,
) -> Router {
    let mut router = Router::new();
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
            // Deliveries come from the bot's Bot API server: Telegram's
            // networks for a cloud upstream, wherever a self-hosted one
            // runs otherwise (validation keeps `telegram` to cloud bots).
            ip_allowlist: match &hook.ip_allowlist {
                config::IpAllowlist::None => IpAllowlist::None,
                config::IpAllowlist::Telegram => IpAllowlist::Telegram,
                config::IpAllowlist::Networks(nets) => IpAllowlist::Cidrs(nets.clone()),
            },
            trusted_proxies: server.trusted_proxies.clone(),
            body_limit: DEFAULT_WEBHOOK_BODY_LIMIT,
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

    let Served { auth, aliases } = served;
    let pull = Arc::new(PullRoutes::new(
        Arc::clone(engine),
        auth,
        server.trusted_proxies.clone(),
    ));
    router = router.route(
        "/c/{consumer}/getUpdates",
        get(by_path).post(by_path).with_state(Arc::clone(&pull)),
    );
    for (path, name) in aliases {
        router = router.route(&path, alias(&pull, name));
    }
    let gateway = Arc::clone(gateway);
    router.fallback(move |req: Request| {
        let gateway = Arc::clone(&gateway);
        async move { gateway.handle(req).await }
    })
}

/// A router that hands every request to the router current in `routes`;
/// requests already running finish on the router they started on.
fn delegating(routes: Arc<ArcSwap<Router>>) -> Router {
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

#[cfg(test)]
mod tests {
    use axum::body::{Body, Bytes};
    use axum::routing::post;
    use futures_util::stream;

    use super::*;

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
