//! `getUpdates` served from a pull consumer's queue, as the Bot API answers
//! it: on `/c/<consumer>/getUpdates` and each alias ([`PullRoutes`], with
//! the consumer's bearer token), and on the gateway path
//! `/c/<consumer>/bot<token>/getUpdates` ([`poll`], the bot token being the
//! authority there).

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::fmt;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body::{Body as HttpBody, Frame, SizeHint};
use ipnet::IpNet;
use serde::de::{
    self, DeserializeSeed, Deserializer as _, IgnoredAny, MapAccess, SeqAccess, Visitor,
};
use tgin_core::consumer::PullConsumer;
use tgin_core::consumer::pull::PollRequest;
use tgin_core::engine::Engine;
use tgin_runtime::http::bearer_matches;
use tgin_runtime::queue::HolderId;
use tgin_runtime::secret::Secret;
use tokio_util::sync::CancellationToken;

use crate::gateway::errors;

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

/// The pull consumers a router serves, by name, each with its bearer token
/// if it has one.
pub type PullAuth = HashMap<Arc<str>, Option<Secret<String>>>;

/// The pull consumers the data plane serves on their own paths.
pub struct PullRoutes {
    engine: Arc<Engine>,
    /// The pull consumers served, each with its bearer token if it has one;
    /// a consumer not listed is not served.
    auth: PullAuth,
    trusted_proxies: Vec<IpNet>,
}

impl PullRoutes {
    /// Serves the consumers `auth` lists from `engine`; `trusted_proxies`
    /// may name the client address of a poll without `X-Tgin-Instance`.
    pub fn new(engine: Arc<Engine>, auth: PullAuth, trusted_proxies: Vec<IpNet>) -> Self {
        Self {
            engine,
            auth,
            trusted_proxies,
        }
    }

    /// One `getUpdates` poll of the consumer `name`: `404` unless it is
    /// served, `401` without its bearer token when it has one.
    pub async fn get_updates(&self, name: &str, req: Request) -> Response {
        let Some(token) = self.auth.get(name) else {
            return errors::error(StatusCode::NOT_FOUND, "Not Found");
        };
        if let Some(expected) = token
            && !bearer_matches(req.headers(), expected)
        {
            return errors::error(StatusCode::UNAUTHORIZED, "Unauthorized");
        }
        let Some(consumer) = self.engine.pull_consumer(name) else {
            return errors::error(StatusCode::NOT_FOUND, "Not Found");
        };
        poll(&consumer, req, &self.trusted_proxies).await
    }
}

/// One `getUpdates` poll of `consumer`, which the caller has authorized.
/// The parameters come from the query string, then a JSON or urlencoded
/// body, whose values win; a multipart body gets `400`. The polling
/// instance is `X-Tgin-Instance` if sent, else the client address (forwarded
/// only by one of `trusted_proxies`). The poll ends when the client goes
/// away.
pub async fn poll(consumer: &PullConsumer, req: Request, trusted_proxies: &[IpNet]) -> Response {
    let (parts, body) = req.into_parts();
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    let instance = match instance(&parts.headers, peer, trusted_proxies) {
        Ok(instance) => instance,
        Err(description) => return bad_request(description),
    };
    let Ok(body) = axum::body::to_bytes(body, PULL_BODY_LIMIT).await else {
        return body_unreadable();
    };
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let params = match Params::parse(parts.uri.query(), content_type, &body) {
        Ok(params) => params,
        Err(description) => return bad_request(&description),
    };

    // Cancelled when the client goes away: hyper drops this future, and
    // with it the guard.
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
    peer.and_then(|peer| crate::client_ip(peer.ip(), headers, trusted_proxies))
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

/// The parameters `getUpdates` reads.
#[derive(Clone, Copy)]
enum Key {
    Offset,
    Limit,
    Timeout,
}

impl Key {
    /// `allowed_updates` and anything else: routing is the config's.
    fn of(key: &str) -> Option<Key> {
        match key {
            "offset" => Some(Key::Offset),
            "limit" => Some(Key::Limit),
            "timeout" => Some(Key::Timeout),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Key::Offset => "offset",
            Key::Limit => "limit",
            Key::Timeout => "timeout",
        }
    }
}

/// Parameters as sent, before defaults and clamping.
#[derive(Default)]
struct RawParams {
    offset: Option<i64>,
    limit: Option<i64>,
    timeout: Option<i64>,
}

/// A value as sent: an integer, or `Err` for anything that is none.
type Sent = Result<i64, ()>;

impl RawParams {
    fn set(&mut self, key: Key, value: Sent) -> Result<(), String> {
        let slot = match key {
            Key::Offset => &mut self.offset,
            Key::Limit => &mut self.limit,
            Key::Timeout => &mut self.timeout,
        };
        *slot = Some(value.map_err(|()| format!("{} must be an integer", key.name()))?);
        Ok(())
    }

    fn form(&mut self, bytes: &[u8]) -> Result<(), String> {
        for (key, value) in form_urlencoded::parse(bytes) {
            if let Some(key) = Key::of(&key) {
                self.set(key, value.trim().parse().map_err(|_| ()))?;
            }
        }
        Ok(())
    }

    /// A JSON object's members: an integer, or a string holding one. A key
    /// sent twice takes its last value; the keys are checked in name order.
    fn json(&mut self, body: &[u8]) -> Result<(), String> {
        let mut de = serde_json::Deserializer::from_slice(body);
        let members = de
            .deserialize_map(Members::default())
            .and_then(|members| de.end().map(|()| members))
            .map_err(|_| "the body must be a JSON object".to_owned())?;
        for (key, value) in [
            (Key::Limit, members.limit),
            (Key::Offset, members.offset),
            (Key::Timeout, members.timeout),
        ] {
            if let Some(value) = value {
                self.set(key, value)?;
            }
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

/// The members of a JSON object that `getUpdates` reads, each the last one
/// of its name.
#[derive(Default)]
struct Members {
    offset: Option<Sent>,
    limit: Option<Sent>,
    timeout: Option<Sent>,
}

impl<'de> Visitor<'de> for Members {
    type Value = Members;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> Result<Members, A::Error> {
        while let Some(key) = map.next_key_seed(KeySeed)? {
            let slot = match key {
                Some(Key::Offset) => &mut self.offset,
                Some(Key::Limit) => &mut self.limit,
                Some(Key::Timeout) => &mut self.timeout,
                None => {
                    map.next_value::<IgnoredAny>()?;
                    continue;
                }
            };
            *slot = Some(map.next_value_seed(Integer)?);
        }
        Ok(self)
    }
}

/// An object key, as the [`Key`] it names, without allocating unless it
/// has escapes.
struct KeySeed;

impl<'de> DeserializeSeed<'de> for KeySeed {
    type Value = Option<Key>;

    fn deserialize<D: de::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl Visitor<'_> for KeySeed {
    type Value = Option<Key>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object key")
    }

    fn visit_str<E: de::Error>(self, key: &str) -> Result<Self::Value, E> {
        Ok(Key::of(key))
    }
}

/// A member's value as an integer: a JSON integer that fits `i64`, or a
/// string holding one (surrounding whitespace allowed). Anything else is
/// read whole and is `Err(())`.
struct Integer;

impl<'de> DeserializeSeed<'de> for Integer {
    type Value = Sent;

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Sent, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Integer {
    type Value = Sent;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Sent, E> {
        Ok(Ok(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Sent, E> {
        Ok(i64::try_from(v).map_err(|_| ()))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Sent, E> {
        Ok(Err(()))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Sent, E> {
        Ok(v.trim().parse().map_err(|_| ()))
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Sent, E> {
        Ok(Err(()))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Sent, E> {
        Ok(Err(()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Sent, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Err(()))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Sent, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Err(()))
    }
}

fn bad_request(detail: &str) -> Response {
    errors::error(StatusCode::BAD_REQUEST, &format!("Bad Request: {detail}"))
}

/// `400 Bad Request: the request body could not be read`: a body that
/// failed or is over the 64 KiB limit.
pub(crate) fn body_unreadable() -> Response {
    bad_request("the request body could not be read")
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

    /// The JSON reader decides as reading the body into a `serde_json`
    /// value would: the object's last value of a key counts, keys are
    /// checked in name order, other members may hold anything.
    #[test]
    fn params_json_matches_a_parsed_object() {
        let json = |body: &str| Params::parse(None, Some("application/json"), body.as_bytes());
        assert_eq!(
            json(r#"{"limit":1,"limit":" 7 ","x":{"a":[1,{"b":null}]},"y":[true],"z":1e300}"#)
                .expect("valid")
                .limit,
            7
        );
        assert_eq!(
            json(r#"{"offset":-9223372036854775808}"#)
                .expect("valid")
                .offset,
            i64::MIN
        );
        for (body, error) in [
            (
                r#"{"offset":9223372036854775808}"#,
                "offset must be an integer",
            ),
            (r#"{"timeout":true}"#, "timeout must be an integer"),
            (r#"{"timeout":null}"#, "timeout must be an integer"),
            (r#"{"timeout":[1]}"#, "timeout must be an integer"),
            (r#"{"timeout":{"a":1}}"#, "timeout must be an integer"),
            (r#"{"timeout":"x","limit":"y"}"#, "limit must be an integer"),
            (r#"{"limit":"x","limit":2}"#, ""),
            (r#"{"limit":1} 2"#, "the body must be a JSON object"),
            ("1", "the body must be a JSON object"),
            ("null", "the body must be a JSON object"),
        ] {
            match json(body) {
                Ok(_) => assert_eq!(error, "", "{body}"),
                Err(got) => assert_eq!(got, error, "{body}"),
            }
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
}
