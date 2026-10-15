//! Webhook ingress: Telegram POSTs each update to a route on the shared HTTP
//! plane, and the update is offered to a [`Sink`] before the response.

use std::fmt;
use std::future::poll_fn;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, HttpBody};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::{Bytes, BytesMut};
use tgin_core::ids::SourceId;
use tgin_core::metrics::{SourceErrorReason, SourceMetrics};
use tgin_core::sink::{Admit, Sink};
use tgin_runtime::secret::{Secret, secret_eq};
use tokio_util::sync::CancellationToken;

use crate::client::{ApiError, BotClient, SetWebhook};
use crate::facts::extract;

/// The default request body cap. Telegram updates are a few KiB; the cap
/// bounds what one request can make the server buffer.
pub const DEFAULT_WEBHOOK_BODY_LIMIT: usize = 1024 * 1024;

/// `Retry-After` (seconds) on a 503: Telegram redelivers the update later,
/// after a shutdown by then to the replacement instance.
const RETRY_AFTER_SECS: &str = "5";

const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";
const FORWARDED_FOR: &str = "x-forwarded-for";

/// The networks Telegram sends webhooks from.
const TELEGRAM_NETS: [IpNet; 2] = [
    IpNet {
        ip: IpAddr::V4(Ipv4Addr::new(149, 154, 160, 0)),
        prefix: 20,
    },
    IpNet {
        ip: IpAddr::V4(Ipv4Addr::new(91, 108, 4, 0)),
        prefix: 22,
    },
];

/// An IP network in CIDR notation. Host bits are cleared, so `10.1.2.3/8`
/// is `10.0.0.0/8`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IpNet {
    ip: IpAddr,
    prefix: u8,
}

/// Why a string is not an [`IpNet`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IpNetError {
    #[error("expected an IP address or a CIDR network such as 10.0.0.0/8")]
    Syntax,
    #[error("prefix length {prefix} is longer than the address ({max} bits)")]
    Prefix { prefix: u8, max: u8 },
}

impl IpNet {
    pub fn new(ip: IpAddr, prefix: u8) -> Result<Self, IpNetError> {
        let max = match ip {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if prefix > max {
            return Err(IpNetError::Prefix { prefix, max });
        }
        let ip = match ip {
            IpAddr::V4(a) => IpAddr::V4(mask_v4(a.to_bits(), prefix).into()),
            IpAddr::V6(a) => IpAddr::V6(mask_v6(a.to_bits(), prefix).into()),
        };
        Ok(Self { ip, prefix })
    }

    /// The network address.
    pub fn ip(&self) -> IpAddr {
        self.ip
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// Whether `addr` is in the network. An IPv4-mapped IPv6 address
    /// (`::ffff:a.b.c.d`) counts as its IPv4 address.
    pub fn contains(&self, addr: IpAddr) -> bool {
        match (self.ip, addr) {
            (IpAddr::V4(net), IpAddr::V4(a)) => mask_v4(a.to_bits(), self.prefix) == net.to_bits(),
            (IpAddr::V4(net), IpAddr::V6(a)) => a
                .to_ipv4_mapped()
                .is_some_and(|a| mask_v4(a.to_bits(), self.prefix) == net.to_bits()),
            (IpAddr::V6(net), IpAddr::V6(a)) => mask_v6(a.to_bits(), self.prefix) == net.to_bits(),
            (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

fn mask_v4(bits: u32, prefix: u8) -> u32 {
    bits & u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
}

fn mask_v6(bits: u128, prefix: u8) -> u128 {
    bits & u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0)
}

/// `a.b.c.d/n`, `x::y/n`, or a bare address (a single-host network).
impl FromStr for IpNet {
    type Err = IpNetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (ip, prefix) = match s.split_once('/') {
            Some((ip, prefix)) => {
                // Digits only: `u8::from_str` would also take a `+` sign.
                if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(IpNetError::Syntax);
                }
                let prefix = prefix.parse().map_err(|_| IpNetError::Syntax)?;
                (ip, Some(prefix))
            }
            None => (s, None),
        };
        let ip: IpAddr = ip.parse().map_err(|_| IpNetError::Syntax)?;
        let prefix = prefix.unwrap_or(match ip {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        });
        Self::new(ip, prefix)
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.ip, self.prefix)
    }
}

/// Which client addresses may post updates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum IpAllowlist {
    /// Any address.
    #[default]
    None,
    /// Telegram's webhook networks, 149.154.160.0/20 and 91.108.4.0/22.
    Telegram,
    /// These networks; an empty list admits no one.
    Cidrs(Vec<IpNet>),
}

impl IpAllowlist {
    pub fn allows(&self, addr: IpAddr) -> bool {
        match self {
            Self::None => true,
            Self::Telegram => TELEGRAM_NETS.iter().any(|n| n.contains(addr)),
            Self::Cidrs(nets) => nets.iter().any(|n| n.contains(addr)),
        }
    }
}

/// How a webhook route accepts updates. `Debug` shows neither the path nor
/// the registration URL.
#[derive(Clone)]
pub struct WebhookOptions {
    /// The route path; it must start with `/`. It may embed a token, so it
    /// is never logged.
    pub path: String,
    /// The `X-Telegram-Bot-Api-Secret-Token` every request must carry; the
    /// one given to `setWebhook` (see [`crate::webhook_secret`]).
    pub secret: Option<Secret<String>>,
    pub ip_allowlist: IpAllowlist,
    /// Peers whose `X-Forwarded-For` is believed (reverse proxies).
    pub trusted_proxies: Vec<IpNet>,
    /// The largest accepted body, in bytes ([`DEFAULT_WEBHOOK_BODY_LIMIT`]).
    pub body_limit: usize,
    /// The registration the app sends with [`register`] at start, if any.
    /// The router does not use it.
    pub register: Option<SetWebhook>,
}

impl fmt::Debug for WebhookOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookOptions")
            .field("secret", &self.secret)
            .field("ip_allowlist", &self.ip_allowlist)
            .field("trusted_proxies", &self.trusted_proxies)
            .field("body_limit", &self.body_limit)
            .field("register", &self.register.is_some())
            .finish_non_exhaustive()
    }
}

/// One webhook route's state.
struct Ingress<S> {
    id: SourceId,
    /// For logs.
    name: Arc<str>,
    secret: Option<Secret<String>>,
    allowlist: IpAllowlist,
    trusted_proxies: Vec<IpNet>,
    body_limit: usize,
    sink: Arc<S>,
    ingest: CancellationToken,
    metrics: SourceMetrics,
}

/// A router with one `POST opts.path` route that offers each update to
/// `sink` as source `id`.
///
/// A request is answered, in order of the checks:
///
/// 1. 405 for any method but `POST`;
/// 2. 403 if the client address is not in `opts.ip_allowlist`;
/// 3. 401 without the right `X-Telegram-Bot-Api-Secret-Token`;
/// 4. 503 with `Retry-After: 5` once `ingest` is cancelled;
/// 5. 413 for a body over `opts.body_limit`;
/// 6. 400 for a body that is not a JSON object with an integer `update_id`;
/// 7. 503 with `Retry-After: 5` when the sink pushes back, so Telegram
///    delivers the update again later;
/// 8. otherwise 200 with an empty body, also when no rule matches.
///
/// Checks 1 to 4 run on the request head alone: the body is read only for
/// an authorized request while ingest runs.
///
/// The client address is the TCP peer's, read from
/// [`ConnectInfo<SocketAddr>`]: serve the app with
/// `into_make_service_with_connect_info::<SocketAddr>()`. Without it, an
/// allowlist refuses every request. When the peer is in
/// `opts.trusted_proxies`, the client is the right-most `X-Forwarded-For`
/// address not in `opts.trusted_proxies` (the left-most if all are); a
/// malformed entry before that one makes the client unknown, which the
/// allowlist refuses.
///
/// # Panics
///
/// If `opts.path` is not a valid axum route path (it must start with `/`).
pub fn webhook_router<S: Sink>(
    id: SourceId,
    name: Arc<str>,
    opts: WebhookOptions,
    sink: Arc<S>,
    ingest: CancellationToken,
    metrics: SourceMetrics,
) -> Router {
    let WebhookOptions {
        path,
        secret,
        ip_allowlist,
        trusted_proxies,
        body_limit,
        register: _,
    } = opts;
    let ingress = Arc::new(Ingress {
        id,
        name,
        secret,
        allowlist: ip_allowlist,
        trusted_proxies,
        body_limit,
        sink,
        ingest,
        metrics,
    });
    Router::new()
        .route(&path, post(handle::<S>))
        .with_state(ingress)
}

/// Sends `setWebhook`. The app calls it at start when a registration is
/// configured; `req.secret_token` must be the router's `secret` (for an
/// automatic secret, derive it with [`crate::webhook_secret`]).
pub async fn register(client: &BotClient, req: &SetWebhook) -> Result<(), ApiError> {
    client.set_webhook(req).await
}

async fn handle<S: Sink>(State(ingress): State<Arc<Ingress<S>>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    if !ingress.address_allowed(&parts) {
        ingress.metrics.error(SourceErrorReason::Unauthorized);
        tracing::debug!(source = %ingress.name, "webhook request from an address not in the allowlist");
        return StatusCode::FORBIDDEN.into_response();
    }
    if !ingress.secret_matches(&parts.headers) {
        ingress.metrics.error(SourceErrorReason::Unauthorized);
        tracing::debug!(source = %ingress.name, "webhook request without the right secret token");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Ingest stopped (shutdown past its lame duck): refuse rather than
    // acknowledge an update this process may never deliver.
    if ingress.ingest.is_cancelled() {
        return retry_later();
    }
    let body = match read_body(body, ingress.body_limit).await {
        Ok(body) => body,
        Err(BodyError::TooLarge) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(BodyError::Read) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let (update_id, facts) = match extract(&body) {
        Ok(read) => read,
        Err(err) => {
            ingress.metrics.error(SourceErrorReason::Decode);
            tracing::debug!(source = %ingress.name, error = %err, "unreadable webhook update");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    // Reading the body awaited; ingest may have stopped since. `admit` does
    // not await, so nothing can change between this check and its answer,
    // and none of its answers depends on ingest: an admitted update is
    // queued and acknowledged, an unroutable one has nowhere to go on any
    // instance, and a refused one is retried either way.
    if ingress.ingest.is_cancelled() {
        return retry_later();
    }
    match ingress
        .sink
        .admit(ingress.id, update_id, body.clone(), &facts)
    {
        Admit::Admitted | Admit::Unroutable => {
            ingress.metrics.update();
            StatusCode::OK.into_response()
        }
        Admit::Backpressure => {
            tracing::debug!(source = %ingress.name, update_id, "sink is full; asking Telegram to retry");
            retry_later()
        }
    }
}

/// 503 with `Retry-After`: Telegram delivers the update again later.
fn retry_later() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, RETRY_AFTER_SECS)],
    )
        .into_response()
}

impl<S> Ingress<S> {
    fn address_allowed(&self, parts: &Parts) -> bool {
        if self.allowlist == IpAllowlist::None {
            return true;
        }
        self.client_ip(parts)
            .is_some_and(|ip| self.allowlist.allows(ip))
    }

    /// The client's address: the TCP peer's, or the forwarded one when the
    /// peer is a trusted proxy. `None` when unknown.
    fn client_ip(&self, parts: &Parts) -> Option<IpAddr> {
        let ConnectInfo(peer) = parts.extensions.get::<ConnectInfo<SocketAddr>>()?;
        let peer = peer.ip().to_canonical();
        if !self.trusted(peer) {
            return Some(peer);
        }
        forwarded_client(&parts.headers, |ip| self.trusted(ip)).unwrap_or(Some(peer))
    }

    fn trusted(&self, ip: IpAddr) -> bool {
        self.trusted_proxies.iter().any(|n| n.contains(ip))
    }

    fn secret_matches(&self, headers: &HeaderMap) -> bool {
        let Some(expected) = &self.secret else {
            return true;
        };
        // A missing header never matches, even an empty secret.
        headers
            .get(SECRET_HEADER)
            .is_some_and(|v| secret_eq(v.as_bytes(), expected.expose().as_bytes()))
    }
}

/// The client named by `X-Forwarded-For`: the right-most address that is not
/// `trusted`, or the left-most if all are. Header lines are read as one list
/// in order. `None` without the header; `Some(None)` when an entry that has
/// to be read is not an IP address.
fn forwarded_client(
    headers: &HeaderMap,
    trusted: impl Fn(IpAddr) -> bool,
) -> Option<Option<IpAddr>> {
    let mut lines = headers.get_all(FORWARDED_FOR).iter().rev().peekable();
    lines.peek()?;
    let mut client = None;
    for line in lines {
        let Ok(line) = line.to_str() else {
            return Some(None);
        };
        for entry in line.rsplit(',') {
            let Ok(ip) = entry.trim().parse::<IpAddr>() else {
                return Some(None);
            };
            let ip = ip.to_canonical();
            if !trusted(ip) {
                return Some(Some(ip));
            }
            client = Some(ip);
        }
    }
    Some(client)
}

enum BodyError {
    TooLarge,
    /// The client failed to send the body (a broken connection or framing).
    Read,
}

/// Reads `body` whole, refusing it as soon as it is known to exceed `limit`:
/// from its declared length before reading anything, else while reading. A
/// body in a single frame is returned without a copy.
async fn read_body(mut body: Body, limit: usize) -> Result<Bytes, BodyError> {
    let hint = body.size_hint();
    if hint.lower() > limit as u64 {
        return Err(BodyError::TooLarge);
    }
    let mut first: Option<Bytes> = None;
    let mut rest = BytesMut::new();
    let mut len = 0usize;
    while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        let Ok(data) = frame.map_err(|_| BodyError::Read)?.into_data() else {
            continue; // trailers
        };
        len = len.saturating_add(data.len());
        if len > limit {
            return Err(BodyError::TooLarge);
        }
        match first.take() {
            None if rest.is_empty() => first = Some(data),
            earlier => {
                if let Some(earlier) = earlier {
                    let expected = usize::try_from(hint.lower()).unwrap_or(0).max(len);
                    rest.reserve(expected.min(limit));
                    rest.extend_from_slice(&earlier);
                }
                rest.extend_from_slice(&data);
            }
        }
    }
    Ok(first.unwrap_or_else(|| rest.freeze()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> IpNet {
        s.parse().unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn ipnet_parses_and_masks() {
        assert_eq!(net("10.1.2.3/8").to_string(), "10.0.0.0/8");
        assert_eq!(net("10.1.2.3").to_string(), "10.1.2.3/32");
        assert_eq!(net("2001:db8::1/32").to_string(), "2001:db8::/32");
        assert_eq!(net("::1").to_string(), "::1/128");
        assert_eq!(net("0.0.0.0/0").prefix(), 0);
        assert_eq!(net("192.168.0.0/16").ip(), ip("192.168.0.0"));
        assert_eq!(
            "10.0.0.0/33".parse::<IpNet>(),
            Err(IpNetError::Prefix {
                prefix: 33,
                max: 32
            })
        );
        assert_eq!(
            "::/129".parse::<IpNet>(),
            Err(IpNetError::Prefix {
                prefix: 129,
                max: 128
            })
        );
        for bad in [
            "",
            "/8",
            "10.0.0.0/",
            "10.0.0.0/+8",
            "10.0.0.0/256",
            "10.0.0/8",
            "host/8",
            " 10.0.0.0/8",
        ] {
            assert_eq!(bad.parse::<IpNet>(), Err(IpNetError::Syntax), "{bad:?}");
        }
    }

    #[test]
    fn ipnet_contains() {
        let v4 = net("149.154.160.0/20");
        assert!(v4.contains(ip("149.154.160.0")));
        assert!(v4.contains(ip("149.154.175.255")));
        assert!(!v4.contains(ip("149.154.176.0")));
        assert!(!v4.contains(ip("149.154.159.255")));
        assert!(v4.contains(ip("::ffff:149.154.161.1")));
        assert!(!v4.contains(ip("2001:db8::1")));

        let v6 = net("2001:db8::/32");
        assert!(v6.contains(ip("2001:db8:ffff::1")));
        assert!(!v6.contains(ip("2001:db9::1")));
        assert!(!v6.contains(ip("32.1.13.184")));

        assert!(net("0.0.0.0/0").contains(ip("255.255.255.255")));
        assert!(net("::/0").contains(ip("::1")));
        assert!(net("10.0.0.7/32").contains(ip("10.0.0.7")));
        assert!(!net("10.0.0.7/32").contains(ip("10.0.0.8")));
    }

    #[test]
    fn allowlist_telegram_ranges() {
        let tg = IpAllowlist::Telegram;
        for ok in [
            "149.154.160.1",
            "149.154.175.254",
            "91.108.4.0",
            "91.108.7.255",
        ] {
            assert!(tg.allows(ip(ok)), "{ok}");
        }
        for no in [
            "149.154.176.1",
            "91.108.8.0",
            "91.108.3.255",
            "127.0.0.1",
            "::1",
        ] {
            assert!(!tg.allows(ip(no)), "{no}");
        }
        assert!(IpAllowlist::None.allows(ip("127.0.0.1")));
        assert!(!IpAllowlist::Cidrs(Vec::new()).allows(ip("127.0.0.1")));
    }

    #[test]
    fn forwarded_client_picks_rightmost_untrusted() {
        let trusted = |ip: IpAddr| net("10.0.0.0/8").contains(ip);
        let pick = |lines: &[&str]| {
            let mut headers = HeaderMap::new();
            for line in lines {
                headers.append(FORWARDED_FOR, line.parse().unwrap());
            }
            forwarded_client(&headers, trusted)
        };
        assert_eq!(pick(&[]), None);
        assert_eq!(pick(&["1.1.1.1"]), Some(Some(ip("1.1.1.1"))));
        assert_eq!(
            pick(&["1.1.1.1, 2.2.2.2, 10.0.0.1"]),
            Some(Some(ip("2.2.2.2")))
        );
        assert_eq!(
            pick(&["1.1.1.1", "10.0.0.1,10.0.0.2"]),
            Some(Some(ip("1.1.1.1")))
        );
        assert_eq!(pick(&["10.0.0.3, 10.0.0.2"]), Some(Some(ip("10.0.0.3"))));
        assert_eq!(pick(&["::ffff:1.1.1.1"]), Some(Some(ip("1.1.1.1"))));
        assert_eq!(pick(&["junk, 2.2.2.2"]), Some(Some(ip("2.2.2.2"))));
        assert_eq!(pick(&["2.2.2.2, junk"]), Some(None));
        assert_eq!(pick(&["2.2.2.2,"]), Some(None));
        assert_eq!(pick(&["1.1.1.1:443"]), Some(None));
    }

    #[test]
    fn options_debug_hides_path_and_secret() {
        let opts = WebhookOptions {
            path: "/bot123456789:PathEmbeddedToken".to_owned(),
            secret: Some(Secret::new("hush-hush-value".to_owned())),
            ip_allowlist: IpAllowlist::Telegram,
            trusted_proxies: vec![net("10.0.0.0/8")],
            body_limit: DEFAULT_WEBHOOK_BODY_LIMIT,
            register: None,
        };
        let shown = format!("{opts:?}");
        assert!(!shown.contains("PathEmbeddedToken"), "{shown}");
        assert!(!shown.contains("hush"), "{shown}");
        assert!(shown.contains("10.0.0.0"), "{shown}");
    }
}
