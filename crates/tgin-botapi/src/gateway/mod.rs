//! The Bot API gateway: request paths, the Bot API method table, request
//! parameters and chat keys, Telegram-shaped responses, and [`Gateway`],
//! which checks each request's bot token and streams it to Telegram, and
//! makes the method calls push endpoints answer updates with (`reply`).
//!
//! Gateway request paths and upstream URLs carry bot tokens: nothing here
//! logs, traces or reports them. Logs and metrics name the bot and the
//! method label instead.

pub mod chat_key;
pub mod errors;
mod intercept;
pub mod limit;
pub mod methods;
pub mod metrics;
pub mod params;
pub mod path;
mod proxy;
mod reply;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::BytesMut;
use http_body::{Body as HttpBody, Frame, SizeHint};
use ipnet::IpNet;
use parking_lot::Mutex;
use reqwest::Url;
use tgin_core::config;
use tgin_core::engine::Engine;
use tgin_core::ids::SourceId;
use tgin_runtime::http::{ClientOptions, Via, client};
use tgin_runtime::secret::{Secret, secret_eq};
use tokio::time::Instant;

use self::limit::BotLimiter;
use self::metrics::{BotMetrics, MethodLabel, Outcome, ReplyMetrics, UpstreamMetrics};
pub use self::metrics::{GatewayMetrics, UpstreamUp};
use self::path::GatewayPath;
use self::proxy::Upstream;
use crate::client::Identity;

/// The connect timeout of the gateway's upstream requests.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A configured bot as the gateway serves it.
pub struct GatewayBot {
    pub name: Arc<str>,
    pub source: SourceId,
    pub token: Secret<String>,
    pub upstream: Arc<config::Upstream>,
    pub gateway: config::Gateway,
}

/// The Bot API gateway: the data plane's fallback for paths no configured
/// route serves, and the executor of push consumers' webhook replies.
pub struct Gateway {
    engine: Arc<Engine>,
    metrics: GatewayMetrics,
    /// This process's `Via`, which every upstream request carries.
    via: HeaderValue,
    table: ArcSwap<Table>,
    /// `tgin_webhook_replies_total` handles by push consumer.
    replies: Mutex<HashMap<Arc<str>, ReplyMetrics>>,
    /// The requests forwarded to Telegram, for the shutdown's drain.
    upstreams: Arc<Upstreams>,
}

/// The gateway's requests forwarded to Telegram: how many are in flight,
/// and when one last started or ended (`None` before the first).
#[derive(Default)]
struct Upstreams {
    in_flight: AtomicUsize,
    last: Mutex<Option<Instant>>,
}

/// One request forwarded to Telegram, counted in [`Upstreams`] until it is
/// dropped: with its response body, which may stream long after the
/// headers, or with the request when no body is relayed.
pub(super) struct Forwarding(Arc<Upstreams>);

impl Forwarding {
    fn new(upstreams: &Arc<Upstreams>) -> Forwarding {
        upstreams.in_flight.fetch_add(1, Ordering::SeqCst);
        *upstreams.last.lock() = Some(Instant::now());
        Forwarding(Arc::clone(upstreams))
    }
}

impl Drop for Forwarding {
    fn drop(&mut self) {
        // Stamped first: whoever sees nothing in flight sees this end.
        *self.0.last.lock() = Some(Instant::now());
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The bots the gateway serves, swapped whole by [`Gateway::set_bots`].
#[derive(Default)]
struct Table {
    bots: Vec<Arc<Bot>>,
    trusted_proxies: Vec<IpNet>,
}

impl Table {
    /// The bot whose token is `token`, compared in constant time with every
    /// configured token, without stopping at a match.
    fn find(&self, token: &str) -> Option<&Arc<Bot>> {
        let mut found = None;
        for bot in &self.bots {
            if secret_eq(token.as_bytes(), bot.token.expose().as_bytes()) && found.is_none() {
                found = Some(bot);
            }
        }
        found
    }

    fn named(&self, name: &str) -> Option<&Arc<Bot>> {
        self.bots.iter().find(|bot| *bot.name == *name)
    }
}

/// One bot's entry: its upstream, its own client and its state.
struct Bot {
    name: Arc<str>,
    source: SourceId,
    token: Secret<String>,
    upstream: Arc<config::Upstream>,
    gateway: config::Gateway,
    /// `{url}/bot{token}/`, the upstream URL's path kept.
    method_base: Url,
    /// `{url}/file/bot{token}/`.
    file_base: Url,
    /// The gateway profile: no total timeout, the connect timeout above;
    /// the proxy bounds idleness with `gateway.upstream_idle_timeout`.
    client: reqwest::Client,
    /// This process's `Via` (see [`Gateway::new`]).
    via: HeaderValue,
    metrics: BotMetrics,
    /// `tgin_bot_info` and `tgin_upstream_up` for `upstream`: kept while
    /// the bot's upstream stays the same, replaced when it changes.
    upstream_metrics: UpstreamMetrics,
    /// Shared with the entries that replace this one while the token and
    /// the rate limits stay the same, so a reload never resets a quota.
    limiter: Arc<BotLimiter>,
    /// Shared with the entries that replace this one while the token stays
    /// the same, so an identity learned meanwhile is never lost.
    identity: Arc<ArcSwapOption<Identity>>,
}

impl Bot {
    fn new(
        bot: GatewayBot,
        via: HeaderValue,
        metrics: BotMetrics,
        upstream_metrics: UpstreamMetrics,
        identity: Arc<ArcSwapOption<Identity>>,
        limiter: Arc<BotLimiter>,
    ) -> Result<Bot, UpstreamMetrics> {
        let token = bot.token.expose();
        let url = bot.upstream.url.expose();
        let (Some(method_base), Some(file_base)) = (
            base(url, &[&format!("bot{token}")]),
            base(url, &["file", &format!("bot{token}")]),
        ) else {
            return Err(upstream_metrics);
        };
        let client = client(&ClientOptions {
            connect_timeout: CONNECT_TIMEOUT,
            request_timeout: None,
            ..ClientOptions::default()
        });
        Ok(Bot {
            name: bot.name,
            source: bot.source,
            token: bot.token,
            upstream: bot.upstream,
            gateway: bot.gateway,
            method_base,
            file_base,
            client,
            via,
            metrics,
            upstream_metrics,
            limiter,
            identity,
        })
    }

    /// Whether `bot` describes this entry unchanged.
    fn same(&self, bot: &GatewayBot) -> bool {
        self.token.expose() == bot.token.expose()
            && self.upstream == bot.upstream
            && self.gateway == bot.gateway
            && self.source == bot.source
    }
}

/// `api_url` with `segments` and a trailing slash appended to its path, as
/// `BotClient::new` builds its base; query and fragment dropped. `None` for
/// a URL that cannot be a base (the config only allows http and https).
fn base(api_url: &Url, segments: &[&str]) -> Option<Url> {
    let mut url = api_url.clone();
    url.set_query(None);
    url.set_fragment(None);
    {
        let mut path = url.path_segments_mut().ok()?;
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
        path.push("");
    }
    Some(url)
}

impl Gateway {
    /// A gateway serving no bot until [`Gateway::set_bots`]; `engine`
    /// answers which consumers a bot routes to. Every request it sends
    /// upstream, a webhook reply's included, carries `via`, this process's
    /// `Via`: an `api_url` that points back at tgin is refused with `508`
    /// instead of forwarding to itself without end.
    pub fn new(engine: Arc<Engine>, metrics: GatewayMetrics, via: &Via) -> Gateway {
        Gateway {
            engine,
            metrics,
            via: via.header_value().clone(),
            table: ArcSwap::from_pointee(Table::default()),
            replies: Mutex::new(HashMap::new()),
            upstreams: Arc::default(),
        }
    }

    /// Replaces the bot table. An entry whose token, upstream and gateway
    /// config are unchanged keeps its state (client, identity, limiter); a
    /// bot whose token stays keeps its identity, and its limiter too while
    /// its rate limits stay. A bot moved to another server (its upstream's
    /// name or URL changed) gets a new `tgin_bot_info` label pair and its
    /// `tgin_upstream_up` starts absent again; other upstream edits keep
    /// both. A removed bot's metric series go, and so do the
    /// webhook-reply series of consumers the engine no longer has.
    pub fn set_bots(&self, bots: Vec<GatewayBot>, trusted_proxies: Vec<IpNet>) {
        let old = self.table.load_full();
        let mut entries = Vec::with_capacity(bots.len());
        for bot in bots {
            let previous = old.named(&bot.name);
            if let Some(previous) = previous.filter(|previous| previous.same(&bot)) {
                entries.push(Arc::clone(previous));
                continue;
            }
            let metrics = previous.map_or_else(
                || self.metrics.bot(&bot.name),
                |previous| previous.metrics.clone(),
            );
            let same_token = |previous: &&Arc<Bot>| previous.token.expose() == bot.token.expose();
            let identity = previous
                .filter(same_token)
                .map_or_else(Arc::default, |previous| Arc::clone(&previous.identity));
            let kept_limiter = previous
                .filter(same_token)
                .filter(|previous| previous.gateway.rate_limit == bot.gateway.rate_limit)
                .map(|previous| Arc::clone(&previous.limiter));
            let name = Arc::clone(&bot.name);
            let limiter = match kept_limiter {
                Some(limiter) => limiter,
                None => match BotLimiter::new(&bot.gateway.rate_limit) {
                    Ok(limiter) => Arc::new(limiter),
                    Err(err) => {
                        tracing::error!(bot = %name, error = %err, "the bot's rate limits are invalid; the gateway does not serve it");
                        continue;
                    }
                },
            };
            // Kept while the bot stays on the same server: its running
            // source holds this handle until a reload that moves it, and
            // only such a reload restarts the source for its upstream.
            let upstream_metrics = match previous {
                Some(previous) if previous.upstream.same_server(&bot.upstream) => {
                    previous.upstream_metrics.clone()
                }
                _ => {
                    // The old series go first: the new ones may carry the
                    // same labels (the upstream's URL changed, not its name).
                    if let Some(previous) = previous {
                        self.metrics.remove_upstream(&previous.upstream_metrics);
                    }
                    self.metrics.upstream(&bot.name, &bot.upstream.name)
                }
            };
            match Bot::new(
                bot,
                self.via.clone(),
                metrics,
                upstream_metrics,
                identity,
                limiter,
            ) {
                Ok(entry) => entries.push(Arc::new(entry)),
                Err(upstream_metrics) => {
                    self.metrics.remove_upstream(&upstream_metrics);
                    tracing::error!(bot = %name, "the bot's upstream URL cannot be a base URL; the gateway does not serve it");
                }
            }
        }
        for gone in &old.bots {
            if !entries.iter().any(|entry| entry.name == gone.name) {
                self.metrics.remove_bot(&gone.metrics);
                self.metrics.remove_upstream(&gone.upstream_metrics);
            }
        }
        self.table.store(Arc::new(Table {
            bots: entries,
            trusted_proxies,
        }));
        self.retain_reply_metrics();
    }

    /// The `tgin_upstream_up` handle of the bot `bot` on its current
    /// upstream, for the bot's own Bot API calls (long polls, `getMe`,
    /// `setWebhook` and the like).
    pub fn upstream_up(&self, bot: &str) -> Option<UpstreamUp> {
        let table = self.table.load();
        Some(table.named(bot)?.upstream_metrics.up().clone())
    }

    /// Records the identity `getMe` returned for the bot `bot`.
    pub fn set_identity(&self, bot: &str, identity: &Identity) {
        if let Some(entry) = self.table.load().named(bot) {
            entry.identity.store(Some(Arc::new(identity.clone())));
        }
    }

    /// The identity last recorded for the bot `bot`.
    pub fn identity(&self, bot: &str) -> Option<Arc<Identity>> {
        self.table.load().named(bot)?.identity.load_full()
    }

    /// The proxies whose forwarding headers the gateway trusts.
    pub fn trusted_proxies(&self) -> Vec<IpNet> {
        self.table.load().trusted_proxies.clone()
    }

    /// The data router's fallback. A path that is no gateway path answers
    /// Telegram's `404 Not Found`; a token that matches no configured bot
    /// `401 Unauthorized`; a consumer the bot does not route to `404`; a
    /// method other than GET or POST `405`. `getUpdates`, `getMe`, the
    /// webhook methods, `logOut` and `close` are answered here (see
    /// `intercept`). Everything else streams to Telegram.
    pub async fn handle(&self, req: Request) -> Response {
        let uri = req.uri().clone();
        let Some(target) = path::parse(uri.path()) else {
            return errors::error(StatusCode::NOT_FOUND, "Not Found");
        };
        let table = self.table.load_full();
        let (consumer, token, upstream, label) = match &target {
            GatewayPath::Method {
                consumer,
                token,
                method,
            } => (
                *consumer,
                token,
                Upstream::Method(method),
                MethodLabel::of(method),
            ),
            GatewayPath::File {
                consumer,
                token,
                file_path,
            } => (
                *consumer,
                token,
                Upstream::File(file_path),
                MethodLabel::File,
            ),
        };
        let Some(bot) = table.find(token) else {
            self.metrics.unauthorized();
            tracing::debug!("gateway request with an unknown bot token refused");
            return errors::error(StatusCode::UNAUTHORIZED, "Unauthorized");
        };
        let handles = bot.metrics.method(label);
        if let Some(consumer) = consumer
            && !self.engine.routes_to(bot.source, consumer)
        {
            handles.count(Outcome::Rejected);
            return errors::error(StatusCode::NOT_FOUND, "Not Found: consumer not found");
        }
        if req.method() != Method::GET && req.method() != Method::POST {
            handles.count(Outcome::Rejected);
            return errors::error(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
        }
        if let Upstream::Method(_) = upstream {
            let call = intercept::Call {
                engine: &self.engine,
                bot,
                consumer,
                label,
                trusted_proxies: &table.trusted_proxies,
            };
            return match call.route(req).await {
                intercept::Routed::Answered(response) => response,
                intercept::Routed::Proxy(req) => {
                    let forwarding = Forwarding::new(&self.upstreams);
                    proxy::forward(bot, upstream, label, req, forwarding).await
                }
                intercept::Routed::ProxyAndLearn(req) => {
                    let forwarding = Forwarding::new(&self.upstreams);
                    let response = proxy::forward(bot, upstream, label, req, forwarding).await;
                    intercept::learn_identity(bot, response).await
                }
            };
        }
        let forwarding = Forwarding::new(&self.upstreams);
        proxy::forward(bot, upstream, label, req, forwarding).await
    }

    /// When the gateway last forwarded a request to Telegram: `now` while
    /// one is in flight, else when the last one started or ended, `None`
    /// before the first. Requests the gateway answers itself (`getUpdates`
    /// and the other intercepts) do not count, so pollers never hold the
    /// shutdown's drain open.
    pub fn upstream_busy_at(&self, now: Instant) -> Option<Instant> {
        if self.upstreams.in_flight.load(Ordering::SeqCst) > 0 {
            return Some(now);
        }
        *self.upstreams.last.lock()
    }
}

/// A request body read up to a limit by [`read_bounded`].
pub enum Bounded {
    /// The whole body, no longer than the limit.
    Complete(Bytes),
    /// The body is longer than the limit.
    Over(Partial),
}

/// A body longer than a limit: what was read of it, and the rest unread.
pub struct Partial {
    /// The frames read before the limit was passed, the last one included;
    /// empty when the body announced a length over the limit.
    pub prefix: Bytes,
    /// The body after `prefix`.
    pub rest: Body,
}

impl Partial {
    /// The whole body again, `prefix` then `rest`, streamed; its length is
    /// known when `rest`'s is.
    pub fn into_body(self) -> Body {
        Body::new(Prefixed {
            prefix: (!self.prefix.is_empty()).then_some(self.prefix),
            rest: self.rest,
        })
    }
}

/// Reads `body` while it fits in `limit` bytes. A body that announces a
/// longer length is not read at all. Trailers are dropped.
///
/// # Errors
///
/// Reading the body failed.
pub async fn read_bounded(mut body: Body, limit: usize) -> Result<Bounded, axum::Error> {
    let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
    if body.size_hint().lower() > limit_u64 {
        return Ok(Bounded::Over(Partial {
            prefix: Bytes::new(),
            rest: body,
        }));
    }
    let mut read = Collected::default();
    while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        let Ok(data) = frame?.into_data() else {
            continue;
        };
        read.push(data);
        if read.len > limit {
            return Ok(Bounded::Over(Partial {
                prefix: read.into_bytes(),
                rest: body,
            }));
        }
    }
    Ok(Bounded::Complete(read.into_bytes()))
}

/// Frames read so far; a single frame is kept without copying.
#[derive(Default)]
struct Collected {
    first: Bytes,
    more: Option<BytesMut>,
    len: usize,
}

impl Collected {
    fn push(&mut self, data: Bytes) {
        self.len += data.len();
        if let Some(more) = &mut self.more {
            more.extend_from_slice(&data);
        } else if self.first.is_empty() {
            self.first = data;
        } else {
            let mut more = BytesMut::with_capacity(self.len);
            more.extend_from_slice(&self.first);
            more.extend_from_slice(&data);
            self.more = Some(more);
        }
    }

    fn into_bytes(self) -> Bytes {
        match self.more {
            Some(more) => more.freeze(),
            None => self.first,
        }
    }
}

/// `prefix`, then `rest`.
struct Prefixed {
    prefix: Option<Bytes>,
    rest: Body,
}

impl HttpBody for Prefixed {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        if let Some(prefix) = this.prefix.take() {
            return Poll::Ready(Some(Ok(Frame::data(prefix))));
        }
        Pin::new(&mut this.rest).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.prefix.is_none() && self.rest.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let rest = self.rest.size_hint();
        let prefix = self
            .prefix
            .as_ref()
            .map_or(0, |p| u64::try_from(p.len()).unwrap_or(u64::MAX));
        let mut hint = SizeHint::new();
        hint.set_lower(rest.lower().saturating_add(prefix));
        if let Some(upper) = rest.upper() {
            hint.set_upper(upper.saturating_add(prefix));
        }
        hint
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body of `chunks`, streamed frame by frame, with no length.
    fn chunked(chunks: &[&'static str]) -> Body {
        let chunks: Vec<Result<Bytes, std::io::Error>> = chunks
            .iter()
            .map(|c| Ok(Bytes::from_static(c.as_bytes())))
            .collect();
        Body::from_stream(futures_util::stream::iter(chunks))
    }

    async fn all(body: Body) -> Bytes {
        axum::body::to_bytes(body, usize::MAX).await.expect("body")
    }

    /// Bot `a` of `cfg` with `token`, as `set_bots` takes it.
    fn gateway_bot(cfg: &config::Config, token: &str) -> GatewayBot {
        let bot = &cfg.bots[0];
        GatewayBot {
            name: Arc::from(bot.name.as_str()),
            source: SourceId(0),
            token: Secret::new(token.to_owned()),
            upstream: Arc::clone(&bot.upstream),
            gateway: bot.gateway.clone(),
        }
    }

    /// An unchanged bot keeps its entry; a bot whose token stays keeps its
    /// identity through other changes; a new token forgets it; a removed
    /// bot's series go.
    #[tokio::test]
    async fn set_bots_keeps_state_of_unchanged_bots() {
        use tgin_core::engine::EngineRuntime;
        use tgin_core::metrics::CoreMetrics;
        use tgin_runtime::config::MapEnv;
        use tgin_runtime::supervisor::Supervisor;
        use tokio_util::sync::CancellationToken;

        let load = |yaml: &str| {
            config::load(yaml, std::path::Path::new("tgin.yaml"), &MapEnv::default())
                .unwrap_or_else(|_| panic!("valid config"))
        };
        let cfg = load("version: 1\nbots:\n  - { name: a, token: \"1:a\" }\n");
        let slower = load(
            "version: 1\nbots:\n  - { name: a, token: \"1:a\", gateway: { upstream_idle_timeout: 90s } }\n",
        );
        let mut reg = prometheus_client::registry::Registry::with_prefix("tgin");
        let via = tgin_runtime::http::Via::generate();
        let engine = Engine::new(
            &cfg,
            CoreMetrics::register(&mut reg),
            reqwest::Client::new(),
            EngineRuntime {
                supervisor: Arc::new(Supervisor::new()),
                stop: CancellationToken::new(),
                force: CancellationToken::new(),
                via: via.clone(),
                reply: Default::default(),
            },
        )
        .expect("engine");
        let gateway = Gateway::new(Arc::new(engine), GatewayMetrics::register(&mut reg), &via);
        let proxies: Vec<IpNet> = vec!["10.0.0.0/8".parse().expect("net")];
        gateway.set_bots(vec![gateway_bot(&cfg, "1:a")], proxies.clone());
        assert_eq!(gateway.trusted_proxies(), proxies);
        assert!(gateway.identity("a").is_none());
        let identity = Identity {
            user: crate::BotUser {
                id: 1,
                username: Some("a_bot".to_owned()),
            },
            raw: Bytes::from_static(br#"{"id":1,"username":"a_bot"}"#),
        };
        gateway.set_identity("a", &identity);
        gateway.set_identity("nobody", &identity);
        let first = Arc::clone(&gateway.table.load().bots[0]);

        gateway.set_bots(vec![gateway_bot(&cfg, "1:a")], Vec::new());
        assert!(Arc::ptr_eq(&first, &gateway.table.load().bots[0]), "kept");
        assert!(gateway.trusted_proxies().is_empty());

        gateway.set_bots(vec![gateway_bot(&slower, "1:a")], Vec::new());
        assert!(
            !Arc::ptr_eq(&first, &gateway.table.load().bots[0]),
            "rebuilt"
        );
        assert_eq!(gateway.identity("a").as_deref(), Some(&identity));

        gateway.set_bots(vec![gateway_bot(&slower, "1:b")], Vec::new());
        assert!(
            gateway.identity("a").is_none(),
            "another token, maybe another bot"
        );

        gateway.table.load().bots[0]
            .metrics
            .method(MethodLabel::of("getMe"))
            .count(Outcome::Ok);
        gateway.set_bots(Vec::new(), Vec::new());
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &reg).expect("encode");
        assert!(!out.contains(r#"bot="a""#), "{out}");
    }

    #[tokio::test]
    async fn read_bounded_returns_whole_body_within_limit() {
        let Ok(Bounded::Complete(body)) = read_bounded(chunked(&["ab", "cd"]), 4).await else {
            panic!("within the limit");
        };
        assert_eq!(&body[..], b"abcd");
        let Ok(Bounded::Complete(single)) = read_bounded(Body::from("xyz"), 3).await else {
            panic!("within the limit");
        };
        assert_eq!(&single[..], b"xyz");
    }

    #[tokio::test]
    async fn read_bounded_hands_back_prefix_and_rest() {
        let Ok(Bounded::Over(partial)) = read_bounded(chunked(&["ab", "cd", "ef"]), 3).await else {
            panic!("over the limit");
        };
        assert_eq!(&partial.prefix[..], b"abcd");
        assert_eq!(&all(partial.into_body()).await[..], b"abcdef");
    }

    /// A body announcing a length over the limit is not read; the rebuilt
    /// body keeps its exact length.
    #[tokio::test]
    async fn read_bounded_skips_announced_oversize_body() {
        let Ok(Bounded::Over(partial)) = read_bounded(Body::from("0123456789"), 4).await else {
            panic!("over the limit");
        };
        assert!(partial.prefix.is_empty());
        let body = partial.into_body();
        assert_eq!(body.size_hint().exact(), Some(10));
        assert_eq!(&all(body).await[..], b"0123456789");

        let Ok(Bounded::Over(partial)) = read_bounded(chunked(&["0123", "45"]), 3).await else {
            panic!("over the limit");
        };
        assert_eq!(&partial.prefix[..], b"0123");
    }
}
