//! The methods the gateway answers itself, because forwarding them as a
//! downstream sent them would break tgin or the other downstreams of the
//! bot (decisions 6 and 8):
//!
//! | Method | `/bot<token>/…` | `/c/<consumer>/bot<token>/…` |
//! |---|---|---|
//! | `getUpdates` | 409 | served from the pull consumer's queue |
//! | `getMe` | the cached identity; proxied once to fill it | same |
//! | `setWebhook`, `deleteWebhook` | answered, nothing changes | applied to a push consumer with `accept_set_webhook` |
//! | `getWebhookInfo` | synthesized | the runtime endpoint and the consumer's pending count |
//! | `logOut`, `close` | answered, a warning logged | same |
//!
//! The method is matched by its label, which the case-insensitive method
//! table gives, so `GETME` is `getMe` as on Telegram. The descriptions are
//! telegram-bot-api's (`Client.cpp`). Every answer here counts as outcome
//! `local`; nothing here logs a token, a path or a URL.

use std::borrow::Cow;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use ipnet::IpNet;
use reqwest::Url;
use serde::Serialize;
use tgin_core::engine::{Consumer, Engine, Mode, TokenChanged, WebhookChange};
use tgin_runtime::secret::Secret;

use super::metrics::{MethodLabel, Outcome};
use super::{Bot, Bounded, errors, params, read_bounded};
use crate::pull;

/// The largest body of an intercepted webhook method; `setWebhook`'s
/// parameters, a certificate included, fit.
const PARAMS_LIMIT: usize = 64 * 1024;
/// The largest `getMe` answer read to fill the identity cache.
const IDENTITY_LIMIT: usize = 64 * 1024;
/// The longest `secret_token`, as telegram-bot-api checks it.
const MAX_SECRET_TOKEN_LEN: usize = 256;

/// `getUpdates` on a bot path: tgin's own source polls Telegram, so a
/// forwarded call would take its updates.
const PROXY_ONLY_GET_UPDATES: &str =
    "Conflict: tgin serves getUpdates per consumer at /c/<consumer>/bot<token>/getUpdates";
// telegram-bot-api `Client.cpp` (master e3e9dd8): `do_set_webhook` and
// `on_webhook_verified`.
const WEBHOOK_WAS_SET: &str = "Webhook was set";
const WEBHOOK_IS_ALREADY_SET: &str = "Webhook is already set";
const WEBHOOK_WAS_DELETED: &str = "Webhook was deleted";
const WEBHOOK_IS_ALREADY_DELETED: &str = "Webhook is already deleted";
const INVALID_WEBHOOK_URL: &str = "Bad Request: invalid webhook URL specified";
const SECRET_TOKEN_TOO_LONG: &str = "Bad Request: secret token is too long";
const SECRET_TOKEN_ILLEGAL: &str = "Bad Request: secret token contains illegal characters";
/// tdlib's `HttpReader` for a body over its limit.
const TOO_LARGE: &str = "Request Entity Too Large";

/// What becomes of a gateway method request.
pub(super) enum Routed {
    /// Answered here.
    Answered(Response),
    /// To Telegram as it is.
    Proxy(Request),
    /// To Telegram, and its answer fills the bot's identity cache
    /// ([`learn_identity`]).
    ProxyAndLearn(Request),
}

/// One request to a bot the gateway serves.
pub(super) struct Call<'a> {
    pub(super) engine: &'a Engine,
    pub(super) bot: &'a Bot,
    /// The consumer of a `/c/<consumer>/…` path, which the bot routes to.
    pub(super) consumer: Option<&'a str>,
    pub(super) label: MethodLabel,
    pub(super) trusted_proxies: &'a [IpNet],
}

impl Call<'_> {
    /// Answers `req` here when its method is one of the table's; otherwise
    /// hands it back for the proxy.
    pub(super) async fn route(&self, req: Request) -> Routed {
        let answer = match self.label.as_str() {
            "getUpdates" => self.get_updates(req).await,
            "getMe" => match self.bot.identity.load_full() {
                Some(identity) => {
                    self.count(Outcome::Local);
                    errors::ok_raw(&identity.raw)
                }
                None => return Routed::ProxyAndLearn(req),
            },
            "setWebhook" => self.webhook(req, true).await,
            "deleteWebhook" => self.webhook(req, false).await,
            "getWebhookInfo" => self.webhook_info(),
            "logOut" | "close" => {
                self.count(Outcome::Local);
                tracing::warn!(bot = %self.bot.name, method = self.label.as_str(), "a downstream asked to log the bot out of Telegram; answered locally, not forwarded");
                errors::ok_true(None)
            }
            _ => return Routed::Proxy(req),
        };
        Routed::Answered(answer)
    }

    fn count(&self, outcome: Outcome) {
        self.bot.metrics.method(self.label).count(outcome);
    }

    /// The request body, read up to [`PARAMS_LIMIT`] bytes within the
    /// bot's `upstream_idle_timeout`, so a client cannot hold an intercept
    /// (and the token it authenticated with) open by stalling its body. A
    /// failed or stalled read is logged and counted `client_closed`; the
    /// caller answers it with [`Abandoned::response`].
    async fn read_body(&self, body: Body) -> Result<Bounded, Abandoned> {
        let idle = self.bot.gateway.upstream_idle_timeout;
        let abandoned = match tokio::time::timeout(idle, read_bounded(body, PARAMS_LIMIT)).await {
            Ok(Ok(read)) => return Ok(read),
            Ok(Err(_)) => Abandoned::Closed,
            Err(_) => Abandoned::Stalled,
        };
        tracing::debug!(bot = %self.bot.name, method = self.label.as_str(), reason = abandoned.reason(), "gateway request abandoned");
        self.count(Outcome::ClientClosed);
        Err(abandoned)
    }

    /// A bot-scoped change found the bot's token changed since this request
    /// authenticated: the token is no longer valid, as on the next request.
    fn token_changed(&self) -> Response {
        tracing::info!(bot = %self.bot.name, method = self.label.as_str(), "refused: the bot's token changed while the request was in flight");
        self.count(Outcome::Rejected);
        errors::error(StatusCode::UNAUTHORIZED, "Unauthorized")
    }

    async fn get_updates(&self, req: Request) -> Response {
        let pull = self.consumer.map(|consumer| {
            (
                consumer,
                self.engine.pull_consumer_for(self.bot.source, consumer),
            )
        });
        let pull = match pull {
            None => {
                self.count(Outcome::Local);
                return errors::error(StatusCode::CONFLICT, PROXY_ONLY_GET_UPDATES);
            }
            Some((consumer, None)) => {
                self.count(Outcome::Local);
                return errors::error(
                    StatusCode::CONFLICT,
                    &format!("Conflict: {consumer} is not a pull consumer of this bot"),
                );
            }
            Some((_, Some(pull))) => pull,
        };
        let (parts, body) = req.into_parts();
        let body = match self.read_body(body).await {
            Ok(Bounded::Complete(body)) => body,
            // As `poll` refuses it on the consumer's own path, without
            // waiting for the rest.
            Ok(Bounded::Over(_)) => {
                self.count(Outcome::Local);
                return pull::body_unreadable();
            }
            Err(abandoned) => return abandoned.response(),
        };
        let body = Body::from(body);
        self.count(Outcome::Local);
        pull::poll(
            &pull,
            Request::from_parts(parts, body),
            self.trusted_proxies,
        )
        .await
    }

    /// `setWebhook` (`set`) or `deleteWebhook`. A push consumer that accepts
    /// it takes or drops its runtime endpoint; anything else only answers
    /// as Telegram would. `drop_pending_updates` purges the calling consumer
    /// alone, and only one that honours it. Both changes are bound to the
    /// token this request authenticated with: rotated meanwhile, they are
    /// refused with `401`.
    async fn webhook(&self, req: Request, set: bool) -> Response {
        let (parts, body) = req.into_parts();
        let body = match self.read_body(body).await {
            Ok(Bounded::Complete(body)) => body,
            Ok(Bounded::Over(_)) => {
                self.count(Outcome::Local);
                return errors::error(StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE);
            }
            Err(abandoned) => return abandoned.response(),
        };
        let content_type = parts.headers.get(header::CONTENT_TYPE);
        let param = |name| param(parts.uri.query(), content_type, &body, name);
        let drop_pending = param("drop_pending_updates").is_some_and(|v| to_bool(&v));
        // `deleteWebhook` is `setWebhook` with no URL.
        let url = if set { param("url") } else { None }.unwrap_or_default();

        let (source, token) = (self.bot.source, &self.bot.token);
        let capturing = self.consumer.filter(|&consumer| {
            self.engine
                .consumer_settings(source, consumer)
                .is_some_and(|s| s.mode == Mode::Push && s.accept_set_webhook)
        });
        let endpoint = match capturing {
            Some(_) if !url.is_empty() => {
                match runtime_endpoint(&url, param("secret_token").as_deref()) {
                    Ok(endpoint) => Some(endpoint),
                    Err(description) => {
                        self.count(Outcome::Local);
                        return errors::error(StatusCode::BAD_REQUEST, description);
                    }
                }
            }
            _ => None,
        };
        if let Some(consumer) = self.consumer
            && drop_pending
            && (!set || capturing.is_some())
        {
            match self.engine.drop_pending(source, token, consumer) {
                Ok(Some(purged)) => {
                    tracing::info!(bot = %self.bot.name, consumer = %consumer, method = self.label.as_str(), purged, "pending updates dropped by drop_pending_updates");
                }
                Ok(None) => {}
                Err(TokenChanged) => return self.token_changed(),
            }
        }
        let change = match capturing {
            Some(consumer) => match self
                .engine
                .set_runtime_endpoint(consumer, source, token, endpoint)
            {
                Ok(change) => change,
                Err(TokenChanged) => return self.token_changed(),
            },
            None => None,
        };
        self.count(Outcome::Local);
        let description = match change {
            Some(WebhookChange::Set) => WEBHOOK_WAS_SET,
            Some(WebhookChange::Unchanged) if drop_pending => WEBHOOK_WAS_SET,
            Some(WebhookChange::Unchanged) => WEBHOOK_IS_ALREADY_SET,
            Some(WebhookChange::Deleted) => WEBHOOK_WAS_DELETED,
            Some(WebhookChange::AlreadyDeleted) => WEBHOOK_IS_ALREADY_DELETED,
            None => {
                tracing::info!(bot = %self.bot.name, consumer = self.consumer, method = self.label.as_str(), "answered locally and not forwarded to Telegram; nothing changed");
                if url.is_empty() {
                    WEBHOOK_WAS_DELETED
                } else {
                    WEBHOOK_WAS_SET
                }
            }
        };
        errors::ok_true(Some(description))
    }

    /// `getWebhookInfo`: the runtime endpoint's URL, if the consumer has one,
    /// and the updates the consumer holds; on a bot path, no webhook and no
    /// pending update.
    fn webhook_info(&self) -> Response {
        self.count(Outcome::Local);
        let consumer = self
            .consumer
            .and_then(|name| self.engine.consumer_for(self.bot.source, name));
        let url = match &consumer {
            Some(Consumer::Push(push)) => push.runtime_endpoint_url(),
            _ => None,
        };
        let info = WebhookInfo {
            url: url.as_ref().map_or("", Url::as_str),
            has_custom_certificate: false,
            pending_update_count: consumer.as_ref().map_or(0, Consumer::pending),
        };
        match serde_json::to_vec(&info) {
            Ok(result) => errors::ok_raw(&result),
            Err(_) => errors::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
        }
    }
}

/// Why an intercepted request's body was not read.
#[derive(Clone, Copy)]
enum Abandoned {
    /// The client closed it early.
    Closed,
    /// No complete body within `upstream_idle_timeout`.
    Stalled,
}

impl Abandoned {
    fn reason(self) -> &'static str {
        match self {
            Abandoned::Closed => "the client closed the request body early",
            Abandoned::Stalled => "no complete request body within upstream_idle_timeout",
        }
    }

    fn response(self) -> Response {
        match self {
            Abandoned::Closed => errors::error(
                StatusCode::BAD_REQUEST,
                "Bad Request: the request body ended early",
            ),
            Abandoned::Stalled => errors::error(StatusCode::REQUEST_TIMEOUT, "Request Timeout"),
        }
    }
}

/// `WebhookInfo` with the fields telegram-bot-api always writes, in its
/// order (`Client.cpp`, `JsonWebhookInfo`).
#[derive(Serialize)]
struct WebhookInfo<'a> {
    url: &'a str,
    has_custom_certificate: bool,
    pending_update_count: usize,
}

/// Telegram's `getMe` answer for `bot`, passed on as it came; a successful
/// one fills the bot's identity cache, so later calls are answered here.
pub(super) async fn learn_identity(bot: &Bot, response: Response) -> Response {
    if response.status() != StatusCode::OK {
        return response;
    }
    let (parts, body) = response.into_parts();
    match read_bounded(body, IDENTITY_LIMIT).await {
        Ok(Bounded::Complete(body)) => {
            match crate::client::identity(&body) {
                Ok(identity) => bot.identity.store(Some(Arc::new(identity))),
                Err(_) => {
                    tracing::debug!(bot = %bot.name, "Telegram's getMe answer is no identity; not cached");
                }
            }
            Response::from_parts(parts, Body::from(body))
        }
        Ok(Bounded::Over(partial)) => Response::from_parts(parts, partial.into_body()),
        // The proxy counted and logged the failed body.
        Err(_) => errors::error(
            StatusCode::BAD_GATEWAY,
            "Bad Gateway: Telegram is unreachable",
        ),
    }
}

/// The parameter `name` as telegram-bot-api finds it: the query string's
/// first, then the body's (`HttpReader` parses the URL before the body, and
/// `Query::arg` takes the first match).
fn param<'a>(
    query: Option<&'a str>,
    content_type: Option<&HeaderValue>,
    body: &'a [u8],
    name: &str,
) -> Option<Cow<'a, str>> {
    query
        .and_then(|query| params::form_param(query.as_bytes(), name))
        .or_else(|| params::text_param(content_type, body, name))
}

/// telegram-bot-api's `Client::to_bool`: `true`, `yes` or `1`, in any case,
/// surrounding whitespace ignored.
fn to_bool(value: &str) -> bool {
    let value = value.trim();
    ["true", "yes", "1"]
        .iter()
        .any(|yes| value.eq_ignore_ascii_case(yes))
}

/// The URL and secret token of a `setWebhook` as a runtime endpoint,
/// refused as telegram-bot-api refuses them (`Client::do_set_webhook`). An
/// empty secret token is none.
fn runtime_endpoint(
    url: &str,
    secret_token: Option<&str>,
) -> Result<(Url, Option<Secret<String>>), &'static str> {
    let url = webhook_url(url).ok_or(INVALID_WEBHOOK_URL)?;
    let secret_token = secret_token.unwrap_or_default();
    if secret_token.len() > MAX_SECRET_TOKEN_LEN {
        return Err(SECRET_TOKEN_TOO_LONG);
    }
    if !secret_token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(SECRET_TOKEN_ILLEGAL);
    }
    let secret_token = (!secret_token.is_empty()).then(|| Secret::new(secret_token.to_owned()));
    Ok((url, secret_token))
}

/// A webhook URL as tdlib's `parse_url(url, Protocol::Https)` reads it: the
/// text before the first of `:/?#@[]` is the scheme when `://` follows it,
/// and must be `http` or `https` in any case; otherwise the whole text is
/// `[userinfo@]host[:port][/…]` over https. The host may not be empty.
fn webhook_url(url: &str) -> Option<Url> {
    let end = url
        .find([':', '/', '?', '#', '@', '[', ']'])
        .unwrap_or(url.len());
    let (scheme, rest) = match url[end..].strip_prefix("://") {
        Some(rest) => (url[..end].to_ascii_lowercase(), rest),
        None => ("https".to_owned(), url),
    };
    if !matches!(scheme.as_str(), "http" | "https")
        || rest.is_empty()
        || rest.starts_with(['/', '?', '#'])
    {
        return None;
    }
    Url::parse(&format!("{scheme}://{rest}"))
        .ok()
        .filter(|url| url.host().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_bool_as_telegram() {
        for yes in ["true", "TRUE", " yes ", "1", "Yes"] {
            assert!(to_bool(yes), "{yes:?}");
        }
        for no in ["", "false", "0", "2", "on", "y", "tru"] {
            assert!(!to_bool(no), "{no:?}");
        }
    }

    /// The query string's value first, as Telegram reads it and as the
    /// rate limits key a call's chat.
    #[test]
    fn params_query_first_then_body() {
        let json = HeaderValue::from_static("application/json");
        let body = br#"{"url":"https://body.example","drop_pending_updates":true}"#;
        assert_eq!(
            param(
                Some("url=https%3A%2F%2Fq.example"),
                Some(&json),
                body,
                "url"
            )
            .as_deref(),
            Some("https://q.example")
        );
        assert_eq!(
            param(Some("x=1"), Some(&json), body, "url").as_deref(),
            Some("https://body.example")
        );
        assert_eq!(
            param(None, Some(&json), body, "drop_pending_updates").as_deref(),
            Some("true")
        );
        // Present but empty in the query: empty, as `Query::arg` gives it.
        assert_eq!(
            param(Some("url="), Some(&json), body, "url").as_deref(),
            Some("")
        );
        assert_eq!(param(None, None, body, "url"), None);
    }

    #[test]
    fn runtime_endpoint_refused_like_telegram() {
        assert!(runtime_endpoint("https://a.example/hook?x=1", Some("aZ0_-")).is_ok());
        let (_, secret) = runtime_endpoint("http://127.0.0.1:8/x", Some("")).expect("valid");
        assert!(secret.is_none());
        for url in [
            "ftp://a.example/",
            "not a url",
            "/relative",
            "http://",
            "http:///x",
            "://a.example",
            "",
        ] {
            assert_eq!(
                runtime_endpoint(url, None).err(),
                Some(INVALID_WEBHOOK_URL),
                "{url}"
            );
        }
        // No scheme: https, as tdlib's `parse_url` defaults it.
        for (url, parsed) in [
            ("bot.example/hook", "https://bot.example/hook"),
            ("bot.example:8443", "https://bot.example:8443/"),
            ("HTTP://bot.example/x", "http://bot.example/x"),
            ("Https://bot.example", "https://bot.example/"),
        ] {
            let (got, _) = runtime_endpoint(url, None).expect(url);
            assert_eq!(got.as_str(), parsed, "{url}");
        }
        assert_eq!(
            runtime_endpoint("https://a.example", Some(&"a".repeat(257))).err(),
            Some(SECRET_TOKEN_TOO_LONG)
        );
        assert!(runtime_endpoint("https://a.example", Some(&"a".repeat(256))).is_ok());
        for secret in ["a b", "a=b", "é", "a/b", "a+b"] {
            assert_eq!(
                runtime_endpoint("https://a.example", Some(secret)).err(),
                Some(SECRET_TOKEN_ILLEGAL),
                "{secret}"
            );
        }
    }

    #[test]
    fn webhook_info_keys_in_telegram_order() {
        let info = WebhookInfo {
            url: "https://a.example/\"x\"",
            has_custom_certificate: false,
            pending_update_count: 3,
        };
        assert_eq!(
            serde_json::to_string(&info).expect("serialize"),
            r#"{"url":"https://a.example/\"x\"","has_custom_certificate":false,"pending_update_count":3}"#
        );
    }
}
