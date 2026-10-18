//! Webhook ingress: the Bot API server POSTs each update to a route on the
//! shared HTTP plane, and the update is offered to a [`Sink`] before the
//! response.

use std::fmt;
use std::future::{Future, poll_fn};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, HttpBody};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::{Bytes, BytesMut};
use ipnet::IpNet;
use tgin_core::ids::SourceId;
use tgin_core::metrics::{SourceErrorReason, SourceMetrics};
use tgin_core::sink::{Admit, Sink};
use tgin_runtime::backoff::{Backoff, honor_retry_after};
use tgin_runtime::secret::{Secret, secret_eq};
use tokio_util::sync::CancellationToken;

use crate::client::{ApiError, BotClient, SetWebhook};
use crate::facts::extract;

/// The default request body cap. Telegram updates are a few KiB; the cap
/// bounds what one request can make the server buffer.
pub const DEFAULT_WEBHOOK_BODY_LIMIT: usize = 1024 * 1024;

/// `Retry-After` (seconds) on a 503: the Bot API server redelivers the
/// update later, after a shutdown by then to the replacement instance.
const RETRY_AFTER_SECS: &str = "5";

const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";
const FORWARDED_FOR: &str = "x-forwarded-for";
/// The backoff between attempts of a retried Bot API call: `(initial,
/// max)`.
const RETRY_BACKOFF: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(300));
/// The longest server-requested `retry_after` a retried call waits.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(3600);

/// The networks Telegram's cloud sends webhooks from; a self-hosted Bot API
/// server sends from wherever it runs (config validation keeps this preset
/// to `cloud` upstreams).
const TELEGRAM_NETS: [IpNet; 2] = [
    IpNet::new_assert(IpAddr::V4(Ipv4Addr::new(149, 154, 160, 0)), 20),
    IpNet::new_assert(IpAddr::V4(Ipv4Addr::new(91, 108, 4, 0)), 22),
];

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
    /// Whether `addr` may post. An IPv4-mapped IPv6 address
    /// (`::ffff:a.b.c.d`) counts as its IPv4 address.
    pub fn allows(&self, addr: IpAddr) -> bool {
        let addr = addr.to_canonical();
        match self {
            Self::None => true,
            Self::Telegram => TELEGRAM_NETS.iter().any(|n| n.contains(&addr)),
            Self::Cidrs(nets) => nets.iter().any(|n| n.contains(&addr)),
        }
    }
}

/// How a webhook route accepts updates. `Debug` does not show the path.
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
}

impl fmt::Debug for WebhookOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookOptions")
            .field("secret", &self.secret)
            .field("ip_allowlist", &self.ip_allowlist)
            .field("trusted_proxies", &self.trusted_proxies)
            .field("body_limit", &self.body_limit)
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
/// 5. 413 for a body over `opts.body_limit` (counted as a `decode` error),
///    400 for a body the client fails to send;
/// 6. 400 for a body that is not a JSON object with an integer `update_id`;
/// 7. 503 with `Retry-After: 5` when the sink pushes back, or when `ingest`
///    was cancelled by the time an update is admitted, so the Bot API
///    server delivers the update again later (the admitted copy may still
///    be delivered too: at least once);
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

/// Sends `setWebhook` once. The app calls it at start when a registration
/// is configured, then [`retry_registration`] if it failed;
/// `req.secret_token` must be the router's `secret` (for an automatic
/// secret, derive it with [`crate::webhook_secret`]).
pub async fn register(client: &BotClient, req: &SetWebhook) -> Result<(), ApiError> {
    client.set_webhook(req).await
}

/// How [`retry_registration`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Registration {
    /// A `setWebhook` succeeded.
    Registered,
    /// `stop` was cancelled first.
    Stopped,
    /// An error no retry can fix (see [`crate::is_fatal`]): the Bot API
    /// server rejected the token, or the client is misconfigured. Only a
    /// reload with a corrected config registers again.
    Rejected(ApiError),
}

/// Makes `attempt`, a `setWebhook` call, with [`retry_call`] (see there:
/// backoff, `retry_after`, final errors, cancellation), logging each
/// failure. `name` is the bot's, for logs; the caller logs a
/// [`Registration::Rejected`].
pub async fn retry_registration<F, Fut>(
    name: &str,
    stop: &CancellationToken,
    local_upstream: bool,
    failed: Option<ApiError>,
    attempt: F,
) -> Registration
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), ApiError>>,
{
    let failing = |err: &ApiError, delay: Duration| {
        tracing::warn!(source = %name, error = %err, ?delay, "setWebhook failed; the Bot API server may not deliver updates until a retry succeeds");
    };
    match retry_call(stop, local_upstream, failed, attempt, failing).await {
        Retried::Done(()) => Registration::Registered,
        Retried::Stopped => Registration::Stopped,
        Retried::Rejected(err) => Registration::Rejected(err),
    }
}

/// How [`retry_call`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Retried<T> {
    /// An attempt succeeded with this.
    Done(T),
    /// `stop` was cancelled first.
    Stopped,
    /// An error no retry can fix (see [`crate::is_fatal`]).
    Rejected(ApiError),
}

/// Makes `attempt`, a Bot API call, until one succeeds, `stop` is cancelled
/// or the server rejects it for good. `failed` is the error of an attempt
/// made just before, waited out first; without it the first attempt is
/// made at once. Between attempts it waits the backoff, 1 s doubling to
/// 5 min with full jitter, or the server's `retry_after` (up to 1 h) when
/// that is longer; `failing` hears of each error a retry follows, with the
/// wait.
///
/// An error is final when [`crate::is_fatal`] says so for the upstream:
/// on a cloud upstream a 401, 403 or 404 (the token rejected), on a
/// self-hosted one (`local_upstream`) only a 403, since its 401 or 404 may
/// be about the server's own api_id or api_hash. Cancellation, mid-wait or
/// mid-attempt, ends it at once as [`Retried::Stopped`].
pub async fn retry_call<T, F, Fut>(
    stop: &CancellationToken,
    local_upstream: bool,
    mut failed: Option<ApiError>,
    mut attempt: F,
    mut failing: impl FnMut(&ApiError, Duration),
) -> Retried<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ApiError>>,
{
    let (initial, max) = RETRY_BACKOFF;
    let mut backoff = Backoff::new(initial, max, Duration::ZERO);
    loop {
        if let Some(err) = failed.take() {
            if crate::is_fatal(&err, local_upstream) {
                return Retried::Rejected(err);
            }
            let delay = honor_retry_after(
                backoff.next_delay(&mut rand::rng()),
                err.retry_after(),
                RETRY_AFTER_CAP,
            );
            failing(&err, delay);
            if stop
                .run_until_cancelled(tokio::time::sleep(delay))
                .await
                .is_none()
            {
                return Retried::Stopped;
            }
        }
        match stop.run_until_cancelled(attempt()).await {
            None => return Retried::Stopped,
            Some(Ok(value)) => return Retried::Done(value),
            Some(Err(err)) => failed = Some(err),
        }
    }
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
        Err(BodyError::TooLarge) => {
            ingress.metrics.error(SourceErrorReason::Decode);
            tracing::debug!(source = %ingress.name, limit = ingress.body_limit, "webhook request body over the size limit");
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        Err(BodyError::Read) => {
            ingress.metrics.error(SourceErrorReason::Network);
            tracing::debug!(source = %ingress.name, "webhook request body could not be read");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let mut decoded = String::new();
    let (update_id, facts) = match extract(&body, &mut decoded) {
        Ok(read) => read,
        Err(err) => {
            ingress.metrics.error(SourceErrorReason::Decode);
            tracing::debug!(source = %ingress.name, error = %err, "unreadable webhook update");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    // Reading the body awaited; ingest may have stopped since.
    if ingress.ingest.is_cancelled() {
        return retry_later();
    }
    match ingress
        .sink
        .admit(ingress.id, update_id, body.clone(), &facts)
    {
        Admit::Admitted => {
            ingress.metrics.update();
            // Ingest may stop on another thread while `admit` commits, and
            // the shutdown coordinator may then find nothing pending and
            // close the HTTP plane. So a 200 needs `ingest` still running
            // after the commit: the commit and this check happen before
            // `cancel()` (the token's internal lock orders them), which
            // happens before the coordinator's `pending()` check, so every
            // update acknowledged here is visible to the drain. Otherwise
            // the Bot API server retries; the queued copy may still be
            // delivered, so the update may arrive twice.
            if ingress.ingest.is_cancelled() {
                return retry_later();
            }
            StatusCode::OK.into_response()
        }
        // No rule matches it, here or on any other instance: a retry cannot
        // route it.
        Admit::Unroutable => {
            ingress.metrics.update();
            StatusCode::OK.into_response()
        }
        Admit::Backpressure => {
            tracing::debug!(source = %ingress.name, update_id, "sink is full; asking the Bot API server to retry");
            retry_later()
        }
    }
}

/// 503 with `Retry-After`: the Bot API server delivers the update again later.
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
        client_ip(peer.ip(), &parts.headers, &self.trusted_proxies)
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

/// The address of the client behind a request from `peer`: `peer` itself,
/// or, when `peer` is in `trusted_proxies`, the right-most `X-Forwarded-For`
/// address not in `trusted_proxies` (the left-most if all are). An
/// IPv4-mapped IPv6 address counts as its IPv4 address. `None` when an entry
/// that has to be read is not an IP address.
pub fn client_ip(peer: IpAddr, headers: &HeaderMap, trusted_proxies: &[IpNet]) -> Option<IpAddr> {
    let trusted = |ip: IpAddr| trusted_proxies.iter().any(|n| n.contains(&ip));
    let peer = peer.to_canonical();
    if !trusted(peer) {
        return Some(peer);
    }
    forwarded_client(headers, trusted).unwrap_or(Some(peer))
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
/// from its declared length before reading anything, else while reading.
/// A body in a single frame is copied once, to its exact size: the frame is
/// a slice of the connection's read buffer, which a queued update would
/// otherwise keep alive.
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
    Ok(first.map_or_else(|| rest.freeze(), |data| Bytes::copy_from_slice(&data)))
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
    fn allowlist_cidrs_match_v4_mapped_peers() {
        let v4 = IpAllowlist::Cidrs(vec![net("149.154.160.0/20")]);
        assert!(v4.allows(ip("149.154.160.0")));
        assert!(v4.allows(ip("149.154.175.255")));
        assert!(!v4.allows(ip("149.154.176.0")));
        assert!(!v4.allows(ip("149.154.159.255")));
        assert!(v4.allows(ip("::ffff:149.154.161.1")));
        assert!(!v4.allows(ip("2001:db8::1")));

        let v6 = IpAllowlist::Cidrs(vec![net("2001:db8::/32")]);
        assert!(v6.allows(ip("2001:db8:ffff::1")));
        assert!(!v6.allows(ip("2001:db9::1")));
        assert!(!v6.allows(ip("32.1.13.184")));

        // Host bits in a configured network are ignored.
        assert!(IpAllowlist::Cidrs(vec![net("10.1.2.3/8")]).allows(ip("10.200.0.1")));
        let any = IpAllowlist::Cidrs(vec![net("0.0.0.0/0"), net("::/0")]);
        assert!(any.allows(ip("255.255.255.255")));
        assert!(any.allows(ip("::1")));
        let host = IpAllowlist::Cidrs(vec![net("10.0.0.7/32")]);
        assert!(host.allows(ip("10.0.0.7")));
        assert!(!host.allows(ip("10.0.0.8")));
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
        let trusted = |ip: IpAddr| net("10.0.0.0/8").contains(&ip);
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
    fn client_ip_believes_only_trusted_peers() {
        let proxies = [net("10.0.0.0/8")];
        let mut headers = HeaderMap::new();
        headers.insert(FORWARDED_FOR, "1.1.1.1".parse().unwrap());
        assert_eq!(
            client_ip(ip("2.2.2.2"), &headers, &proxies),
            Some(ip("2.2.2.2"))
        );
        assert_eq!(
            client_ip(ip("::ffff:10.0.0.1"), &headers, &proxies),
            Some(ip("1.1.1.1"))
        );
        assert_eq!(
            client_ip(ip("10.0.0.1"), &HeaderMap::new(), &proxies),
            Some(ip("10.0.0.1"))
        );
        headers.insert(FORWARDED_FOR, "junk".parse().unwrap());
        assert_eq!(client_ip(ip("10.0.0.1"), &headers, &proxies), None);
    }

    #[test]
    fn options_debug_hides_path_and_secret() {
        let opts = WebhookOptions {
            path: "/bot123456789:PathEmbeddedToken".to_owned(),
            secret: Some(Secret::new("hush-hush-value".to_owned())),
            ip_allowlist: IpAllowlist::Telegram,
            trusted_proxies: vec![net("10.0.0.0/8")],
            body_limit: DEFAULT_WEBHOOK_BODY_LIMIT,
        };
        let shown = format!("{opts:?}");
        assert!(!shown.contains("PathEmbeddedToken"), "{shown}");
        assert!(!shown.contains("hush"), "{shown}");
        assert!(shown.contains("10.0.0.0"), "{shown}");
    }
}
