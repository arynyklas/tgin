//! The Bot API gateway: request paths, the Bot API method table, request
//! parameters and chat keys, Telegram-shaped responses, and [`Gateway`],
//! which checks each request's bot token and streams it to Telegram.
//!
//! Gateway request paths and upstream URLs carry bot tokens: nothing here
//! logs, traces or reports them. Logs and metrics name the bot and the
//! method label instead.

pub mod chat_key;
pub mod errors;
mod intercept;
pub mod methods;
pub mod metrics;
pub mod params;
pub mod path;
mod proxy;

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use bytes::BytesMut;
use http_body::{Body as HttpBody, Frame, SizeHint};
use ipnet::IpNet;
use reqwest::Url;
use tgin_core::config;
use tgin_core::engine::Engine;
use tgin_core::ids::SourceId;
use tgin_runtime::http::{ClientOptions, client};
use tgin_runtime::secret::{Secret, secret_eq};

pub use self::metrics::GatewayMetrics;
use self::metrics::{BotMetrics, MethodLabel, Outcome};
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
    pub api_url: Url,
    pub gateway: config::Gateway,
}

/// The Bot API gateway: the data plane's fallback for paths no configured
/// route serves.
pub struct Gateway {
    engine: Arc<Engine>,
    metrics: GatewayMetrics,
    table: ArcSwap<Table>,
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
    api_url: Url,
    gateway: config::Gateway,
    /// `{api_url}/bot{token}/`, the `api_url` path kept.
    method_base: Url,
    /// `{api_url}/file/bot{token}/`.
    file_base: Url,
    /// The gateway profile: no total timeout, the connect timeout above;
    /// the proxy bounds idleness with `gateway.upstream_idle_timeout`.
    client: reqwest::Client,
    metrics: BotMetrics,
    /// Shared with the entries that replace this one while the token stays
    /// the same, so an identity learned meanwhile is never lost.
    identity: Arc<ArcSwapOption<Identity>>,
}

impl Bot {
    fn new(
        bot: GatewayBot,
        metrics: BotMetrics,
        identity: Arc<ArcSwapOption<Identity>>,
    ) -> Option<Bot> {
        let token = bot.token.expose();
        let method_base = base(&bot.api_url, &[&format!("bot{token}")])?;
        let file_base = base(&bot.api_url, &["file", &format!("bot{token}")])?;
        let client = client(&ClientOptions {
            connect_timeout: CONNECT_TIMEOUT,
            request_timeout: None,
            ..ClientOptions::default()
        });
        Some(Bot {
            name: bot.name,
            source: bot.source,
            token: bot.token,
            api_url: bot.api_url,
            gateway: bot.gateway,
            method_base,
            file_base,
            client,
            metrics,
            identity,
        })
    }

    /// Whether `bot` describes this entry unchanged.
    fn same(&self, bot: &GatewayBot) -> bool {
        self.token.expose() == bot.token.expose()
            && self.api_url == bot.api_url
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
    /// answers which consumers a bot routes to.
    pub fn new(engine: Arc<Engine>, metrics: GatewayMetrics) -> Gateway {
        Gateway {
            engine,
            metrics,
            table: ArcSwap::from_pointee(Table::default()),
        }
    }

    /// Replaces the bot table. An entry whose token, `api_url` and gateway
    /// config are unchanged keeps its state (client, identity); a bot whose
    /// token stays keeps its identity. A removed bot's metric series go.
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
            let identity = previous
                .filter(|previous| previous.token.expose() == bot.token.expose())
                .map_or_else(Arc::default, |previous| Arc::clone(&previous.identity));
            let name = Arc::clone(&bot.name);
            match Bot::new(bot, metrics, identity) {
                Some(entry) => entries.push(Arc::new(entry)),
                None => {
                    tracing::error!(bot = %name, "the bot's api_url cannot be a base URL; the gateway does not serve it")
                }
            }
        }
        for gone in &old.bots {
            if !entries.iter().any(|entry| entry.name == gone.name) {
                self.metrics.remove_bot(&gone.metrics);
            }
        }
        self.table.store(Arc::new(Table {
            bots: entries,
            trusted_proxies,
        }));
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
                intercept::Routed::Proxy(req) => proxy::forward(bot, upstream, label, req).await,
                intercept::Routed::ProxyAndLearn(req) => {
                    let response = proxy::forward(bot, upstream, label, req).await;
                    intercept::learn_identity(bot, response).await
                }
            };
        }
        proxy::forward(bot, upstream, label, req).await
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
            api_url: bot.api_url.expose().clone(),
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
        let engine = Engine::new(
            &cfg,
            CoreMetrics::register(&mut reg),
            reqwest::Client::new(),
            EngineRuntime {
                supervisor: Arc::new(Supervisor::new()),
                stop: CancellationToken::new(),
                force: CancellationToken::new(),
            },
        )
        .expect("engine");
        let gateway = Gateway::new(Arc::new(engine), GatewayMetrics::register(&mut reg));
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
