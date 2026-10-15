//! Bot API calls over the shared `reqwest::Client`.

use std::net::IpAddr;
use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use reqwest::multipart::{Form, Part};
use reqwest::{RequestBuilder, StatusCode, Url};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tgin_runtime::http::describe_error;
use tgin_runtime::secret::Secret;

/// How much longer than its long-poll `timeout` a `getUpdates` request may
/// take before the client gives up on it.
const LONG_POLL_GRACE: Duration = Duration::from_secs(10);

/// A failed Bot API call. Errors never carry a request URL; those from
/// [`BotClient`] calls also have the bot token masked, even if a server
/// echoes it back.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// The request did not complete (connect, TLS, timeout, body read).
    #[error("Bot API request failed: {0}")]
    Network(String),
    /// The Bot API answered with an error: a non-2xx status or `"ok": false`.
    /// `status` is the response's `error_code` when present.
    #[error("Bot API error {status}: {description}")]
    Http {
        status: u16,
        description: String,
        retry_after: Option<Duration>,
    },
    /// A body could not be encoded or decoded.
    #[error("malformed Bot API response: {0}")]
    Decode(String),
    /// The API URL or token passed to [`BotClient::new`] is unusable.
    #[error("invalid Bot API client setup: {0}")]
    Config(&'static str),
}

/// The bot's own account, from `getMe`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct BotUser {
    pub id: i64,
    pub username: Option<String>,
}

/// A `setWebhook` request. `Debug` shows only the URL's origin (its path
/// and query may carry a secret) and hides the secret token.
#[derive(Clone)]
pub struct SetWebhook {
    pub url: Url,
    pub secret_token: Option<Secret<String>>,
    pub allowed_updates: Option<Vec<String>>,
    pub max_connections: Option<u32>,
    pub drop_pending_updates: bool,
    pub ip_address: Option<IpAddr>,
    /// A PEM certificate to upload, for self-signed webhook endpoints. When
    /// set, the request is sent as `multipart/form-data`.
    pub certificate: Option<Bytes>,
}

impl std::fmt::Debug for SetWebhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetWebhook")
            .field("url", &url_origin(&self.url))
            .field("secret_token", &self.secret_token)
            .field("allowed_updates", &self.allowed_updates)
            .field("max_connections", &self.max_connections)
            .field("drop_pending_updates", &self.drop_pending_updates)
            .field("ip_address", &self.ip_address)
            .field("certificate", &self.certificate)
            .finish()
    }
}

/// `scheme://host[:port]`, plus `/…` when credentials, a path, a query or a
/// fragment were left out.
fn url_origin(url: &Url) -> String {
    let mut shown = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    if let Some(port) = url.port() {
        shown.push_str(&format!(":{port}"));
    }
    let elided = !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some();
    if elided {
        shown.push_str("/…");
    }
    shown
}

/// One bot's Bot API client; clones share the connection pool. `Debug` shows
/// neither the token nor the URL.
#[derive(Clone)]
pub struct BotClient {
    http: reqwest::Client,
    /// `{api_url}/bot{token}/`.
    base: Url,
    /// Scrubbed out of error text.
    token: Secret<String>,
}

impl BotClient {
    /// A client for `{api_url}/bot{token}/`; a path on `api_url` is kept. The
    /// token must be non-empty and use only `[A-Za-z0-9:_-]`.
    pub fn new(
        http: reqwest::Client,
        api_url: &Url,
        token: &Secret<String>,
    ) -> Result<Self, ApiError> {
        let t = token.expose();
        if t.is_empty()
            || !t
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-'))
        {
            return Err(ApiError::Config(
                "the bot token must be non-empty and use only [A-Za-z0-9:_-]",
            ));
        }
        if !matches!(api_url.scheme(), "http" | "https") {
            return Err(ApiError::Config("the API URL must be http or https"));
        }
        let mut base = api_url.clone();
        base.set_query(None);
        base.set_fragment(None);
        base.path_segments_mut()
            .map_err(|()| ApiError::Config("the API URL cannot be a base URL"))?
            .pop_if_empty()
            .push(&format!("bot{t}"))
            .push("");
        Ok(Self {
            http,
            base,
            token: token.clone(),
        })
    }

    pub async fn get_me(&self) -> Result<BotUser, ApiError> {
        let body = self.send(self.http.get(self.url("getMe")?)).await?;
        self.scrubbed(parse_result(&body))
    }

    /// The raw `getUpdates` response body, for [`split_updates`]. The request
    /// may take up to `timeout` plus 10 s. An `"ok": false` body is returned
    /// as an [`ApiError::Http`] with the token masked.
    pub async fn get_updates(
        &self,
        offset: i64,
        limit: u32,
        timeout: Duration,
        allowed: Option<&[String]>,
    ) -> Result<Bytes, ApiError> {
        #[derive(Serialize)]
        struct Params<'a> {
            offset: i64,
            limit: u32,
            timeout: u64,
            #[serde(skip_serializing_if = "Option::is_none")]
            allowed_updates: Option<&'a [String]>,
        }
        let params = Params {
            offset,
            limit,
            timeout: timeout.as_secs(),
            allowed_updates: allowed,
        };
        let req = self
            .post_json("getUpdates", &params)?
            .timeout(timeout.saturating_add(LONG_POLL_GRACE));
        let body = self.send(req).await?;
        // The Bot API writes `{"ok":true` first, so a success costs no extra
        // parse. Anything else is checked for an `"ok": false` error here,
        // where the token can be masked; other malformed bodies are left to
        // `split_updates`.
        if !body.starts_with(br#"{"ok":true"#)
            && let Ok(envelope) = serde_json::from_slice::<Envelope<IgnoredAny>>(&body)
            && !envelope.ok
        {
            return Err(self.scrub(envelope.into_error(StatusCode::OK.as_u16(), None)));
        }
        Ok(body)
    }

    /// Sends JSON, or `multipart/form-data` when a certificate is set.
    pub async fn set_webhook(&self, req: &SetWebhook) -> Result<(), ApiError> {
        #[derive(Serialize)]
        struct Params<'a> {
            url: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            secret_token: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            allowed_updates: Option<&'a [String]>,
            #[serde(skip_serializing_if = "Option::is_none")]
            max_connections: Option<u32>,
            drop_pending_updates: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            ip_address: Option<IpAddr>,
        }
        let request = match &req.certificate {
            None => self.post_json(
                "setWebhook",
                &Params {
                    url: req.url.as_str(),
                    secret_token: req.secret_token.as_ref().map(|s| s.expose().as_str()),
                    allowed_updates: req.allowed_updates.as_deref(),
                    max_connections: req.max_connections,
                    drop_pending_updates: req.drop_pending_updates,
                    ip_address: req.ip_address,
                },
            )?,
            Some(certificate) => self
                .http
                .post(self.url("setWebhook")?)
                .multipart(webhook_form(req, certificate)?),
        };
        let body = self.send(request).await?;
        self.scrubbed(parse_result::<IgnoredAny>(&body).map(drop))
    }

    pub async fn delete_webhook(&self, drop_pending: bool) -> Result<(), ApiError> {
        #[derive(Serialize)]
        struct Params {
            drop_pending_updates: bool,
        }
        let params = Params {
            drop_pending_updates: drop_pending,
        };
        let body = self.send(self.post_json("deleteWebhook", &params)?).await?;
        self.scrubbed(parse_result::<IgnoredAny>(&body).map(drop))
    }

    fn url(&self, method: &str) -> Result<Url, ApiError> {
        self.base
            .join(method)
            .map_err(|_| ApiError::Config("invalid Bot API method name"))
    }

    fn post_json<T: Serialize>(
        &self,
        method: &str,
        params: &T,
    ) -> Result<RequestBuilder, ApiError> {
        let body = serde_json::to_vec(params).map_err(|_| encode_error())?;
        Ok(self
            .http
            .post(self.url(method)?)
            .header(CONTENT_TYPE, "application/json")
            .body(body))
    }

    /// The body of a 2xx response; any other status is an [`ApiError::Http`].
    async fn send(&self, req: RequestBuilder) -> Result<Bytes, ApiError> {
        let resp = req.send().await.map_err(|e| self.network(&e))?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
            .map(Duration::from_secs);
        let body = resp.bytes().await.map_err(|e| self.network(&e))?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(self.scrub(failure(status, retry_after, &body)))
        }
    }

    fn network(&self, err: &reqwest::Error) -> ApiError {
        self.scrub(ApiError::Network(describe_error(err)))
    }

    fn scrubbed<T>(&self, result: Result<T, ApiError>) -> Result<T, ApiError> {
        result.map_err(|e| self.scrub(e))
    }

    /// `err` with every copy of the token masked, in case a server echoes it.
    fn scrub(&self, err: ApiError) -> ApiError {
        let token = self.token.expose().as_str();
        let clean = |s: String| {
            if s.contains(token) {
                s.replace(token, "[redacted]")
            } else {
                s
            }
        };
        match err {
            ApiError::Network(s) => ApiError::Network(clean(s)),
            ApiError::Http {
                status,
                description,
                retry_after,
            } => ApiError::Http {
                status,
                description: clean(description),
                retry_after,
            },
            ApiError::Decode(s) => ApiError::Decode(clean(s)),
            config @ ApiError::Config(_) => config,
        }
    }
}

impl std::fmt::Debug for BotClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotClient").finish_non_exhaustive()
    }
}

fn webhook_form(req: &SetWebhook, certificate: &Bytes) -> Result<Form, ApiError> {
    let mut form = Form::new().text("url", req.url.to_string());
    if let Some(secret) = &req.secret_token {
        form = form.text("secret_token", secret.expose().clone());
    }
    if let Some(allowed) = &req.allowed_updates {
        let list = serde_json::to_string(allowed).map_err(|_| encode_error())?;
        form = form.text("allowed_updates", list);
    }
    if let Some(max) = req.max_connections {
        form = form.text("max_connections", max.to_string());
    }
    form = form.text("drop_pending_updates", req.drop_pending_updates.to_string());
    if let Some(ip) = req.ip_address {
        form = form.text("ip_address", ip.to_string());
    }
    let certificate = Part::stream_with_length(certificate.clone(), certificate.len() as u64)
        .file_name("certificate.pem");
    Ok(form.part("certificate", certificate))
}

/// Splits a `getUpdates` body into `(update_id, update)` pairs. Each update
/// is a sub-slice of `body`, so nothing is copied.
pub fn split_updates(body: &Bytes) -> Result<Vec<(i64, Bytes)>, ApiError> {
    let updates: Vec<&RawValue> = parse_result(body)?;
    updates
        .into_iter()
        .map(|raw| Ok((update_id(raw)?, body.slice_ref(raw.get().as_bytes()))))
        .collect()
}

#[derive(Deserialize)]
struct UpdateId {
    update_id: i64,
}

/// An update's `update_id`. The Bot API writes it first, which is read
/// without a second parse.
fn update_id(raw: &RawValue) -> Result<i64, ApiError> {
    let json = raw.get();
    let leading = json
        .strip_prefix(r#"{"update_id":"#)
        .and_then(|rest| rest.get(..rest.find([',', '}'])?))
        .and_then(|digits| digits.parse().ok());
    match leading {
        Some(id) => Ok(id),
        None => serde_json::from_str::<UpdateId>(json)
            .map(|u| u.update_id)
            .map_err(|e| decode_error(&e)),
    }
}

/// A Bot API response body.
#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    error_code: Option<i64>,
    parameters: Option<Parameters>,
}

#[derive(Deserialize)]
struct Parameters {
    retry_after: Option<u64>,
}

impl<T> Envelope<T> {
    /// The error an `"ok": false` response describes. `status` and
    /// `retry_after` (from the HTTP response) are fallbacks.
    fn into_error(self, status: u16, retry_after: Option<Duration>) -> ApiError {
        ApiError::Http {
            status: self
                .error_code
                .and_then(|c| u16::try_from(c).ok())
                .unwrap_or(status),
            description: self.description.unwrap_or_default(),
            retry_after: self
                .parameters
                .and_then(|p| p.retry_after)
                .map(Duration::from_secs)
                .or(retry_after),
        }
    }
}

/// The `result` of a 2xx response body.
fn parse_result<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, ApiError> {
    let envelope: Envelope<T> = serde_json::from_slice(body).map_err(|e| decode_error(&e))?;
    if !envelope.ok {
        return Err(envelope.into_error(StatusCode::OK.as_u16(), None));
    }
    envelope
        .result
        .ok_or_else(|| ApiError::Decode("the response has no result".into()))
}

/// The error a non-2xx response describes. A body that is not a Bot API
/// error (a proxy's HTML page, say) is reduced to the status's reason phrase.
fn failure(status: StatusCode, retry_after: Option<Duration>, body: &[u8]) -> ApiError {
    match serde_json::from_slice::<Envelope<IgnoredAny>>(body) {
        Ok(envelope) if !envelope.ok => envelope.into_error(status.as_u16(), retry_after),
        _ => ApiError::Http {
            status: status.as_u16(),
            description: status
                .canonical_reason()
                .unwrap_or("unexpected HTTP status")
                .to_owned(),
            retry_after,
        },
    }
}

/// Describes a JSON error by kind and position only: serde_json's messages
/// can quote the body.
fn decode_error(err: &serde_json::Error) -> ApiError {
    use serde_json::error::Category;
    let what = match err.classify() {
        Category::Io => "I/O error",
        Category::Syntax => "invalid JSON",
        Category::Data => "unexpected JSON shape",
        Category::Eof => "truncated JSON",
    };
    ApiError::Decode(format!(
        "{what} at line {} column {}",
        err.line(),
        err.column()
    ))
}

fn encode_error() -> ApiError {
    ApiError::Decode("the request body could not be encoded".into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{body_json, header, header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const TOKEN: &str = "123456789:AAAAexampleSecretTokenValue_-xyz";

    fn http() -> reqwest::Client {
        // `no_proxy`: proxy variables in the test environment must not
        // reroute requests meant for the local mock server.
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn client_for(base: &str) -> BotClient {
        BotClient::new(
            http(),
            &base.parse().unwrap(),
            &Secret::new(TOKEN.to_owned()),
        )
        .unwrap()
    }

    fn at(m: &str) -> String {
        format!("/bot{TOKEN}/{m}")
    }

    #[test]
    fn split_updates_zero_copy() {
        let body = Bytes::from_static(
            br#"{"ok":true,"result":[{"update_id":5,"message":{"text":"a"}}, {"message":{"text":"b"},"update_id":-6}]}"#,
        );
        let updates = split_updates(&body).unwrap();
        let ids: Vec<i64> = updates.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, [5, -6]);
        assert_eq!(
            &updates[0].1[..],
            br#"{"update_id":5,"message":{"text":"a"}}"#
        );
        assert_eq!(
            &updates[1].1[..],
            br#"{"message":{"text":"b"},"update_id":-6}"#
        );
        let range = body.as_ptr_range();
        for (_, update) in &updates {
            let own = update.as_ptr_range();
            assert!(range.start <= own.start && own.end <= range.end);
        }

        assert!(
            split_updates(&Bytes::from_static(br#"{"ok":true,"result":[]}"#))
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            split_updates(&Bytes::from_static(
                br#"{"ok":false,"error_code":409,"description":"Conflict"}"#
            )),
            Err(ApiError::Http { status: 409, .. })
        ));
        for bad in [
            &br#"{"ok":true,"result":[{"message":{}}]}"#[..],
            br#"{"ok":true,"result":[{"update_id":"5"}]}"#,
            br#"{"ok":true,"result":[{"update_id":1.5}]}"#,
            br#"{"ok":true,"result":{}}"#,
            br#"{"ok":true}"#,
            b"[",
        ] {
            assert!(
                matches!(
                    split_updates(&Bytes::from_static(bad)),
                    Err(ApiError::Decode(_))
                ),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[tokio::test]
    async fn get_updates_sends_params() {
        let server = MockServer::start().await;
        let body = r#"{"ok":true,"result":[{"update_id":5}]}"#;
        Mock::given(method("POST"))
            .and(path(at("getUpdates")))
            .and(header("content-type", "application/json"))
            .and(body_json(json!({
                "offset": 5,
                "limit": 100,
                "timeout": 30,
                "allowed_updates": ["message", "callback_query"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(at("getUpdates")))
            .and(body_json(json!({"offset": -1, "limit": 1, "timeout": 0})))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true,"result":[]}"#))
            .expect(1)
            .mount(&server)
            .await;

        let bot = client_for(&server.uri());
        let allowed = ["message".to_owned(), "callback_query".to_owned()];
        let got = bot
            .get_updates(5, 100, Duration::from_secs(30), Some(&allowed))
            .await
            .unwrap();
        assert_eq!(&got[..], body.as_bytes());
        bot.get_updates(-1, 1, Duration::ZERO, None).await.unwrap();
    }

    #[tokio::test]
    async fn json_methods_send_bodies() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(at("getMe")))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"ok":true,"result":{"id":42,"is_bot":true,"first_name":"B","username":"my_bot"}}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(at("deleteWebhook")))
            .and(body_json(json!({"drop_pending_updates": true})))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"ok":true,"result":true}"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(at("setWebhook")))
            .and(header("content-type", "application/json"))
            .and(body_json(json!({
                "url": "https://bot.example.com/webhook/shop",
                "secret_token": "s3cret",
                "drop_pending_updates": false,
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"ok":true,"result":true}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let bot = client_for(&server.uri());
        assert_eq!(
            bot.get_me().await.unwrap(),
            BotUser {
                id: 42,
                username: Some("my_bot".into())
            }
        );
        bot.delete_webhook(true).await.unwrap();
        bot.set_webhook(&SetWebhook {
            url: "https://bot.example.com/webhook/shop".parse().unwrap(),
            secret_token: Some(Secret::new("s3cret".into())),
            allowed_updates: None,
            max_connections: None,
            drop_pending_updates: false,
            ip_address: None,
            certificate: None,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn api_url_base_path_is_kept() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/tg/bot{TOKEN}/getMe")))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"ok":true,"result":{"id":1}}"#),
            )
            .expect(2)
            .mount(&server)
            .await;
        for base in ["/tg", "/tg/"] {
            let bot = client_for(&format!("{}{base}", server.uri()));
            assert_eq!(bot.get_me().await.unwrap().username, None);
        }
    }

    #[tokio::test]
    async fn http_error_parses_description_and_retry_after() {
        let server = MockServer::start().await;
        Mock::given(path(at("getUpdates")))
            .respond_with(ResponseTemplate::new(429).set_body_string(
                r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 7","parameters":{"retry_after":7}}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(path(at("deleteWebhook")))
            .respond_with(
                ResponseTemplate::new(502)
                    .insert_header("retry-after", "3")
                    .set_body_string("<html>bad gateway</html>"),
            )
            .mount(&server)
            .await;
        Mock::given(path(at("getMe")))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(path(at("setWebhook")))
            .respond_with(ResponseTemplate::new(409).set_body_string(
                r#"{"ok":false,"error_code":409,"description":"Conflict: terminated by other getUpdates request"}"#,
            ))
            .mount(&server)
            .await;

        let bot = client_for(&server.uri());
        assert_eq!(
            bot.get_updates(0, 100, Duration::ZERO, None).await,
            Err(ApiError::Http {
                status: 429,
                description: "Too Many Requests: retry after 7".into(),
                retry_after: Some(Duration::from_secs(7)),
            })
        );
        assert_eq!(
            bot.delete_webhook(false).await,
            Err(ApiError::Http {
                status: 502,
                description: "Bad Gateway".into(),
                retry_after: Some(Duration::from_secs(3)),
            })
        );
        assert_eq!(
            bot.get_me().await,
            Err(ApiError::Http {
                status: 401,
                description: "Unauthorized".into(),
                retry_after: None,
            })
        );
        let req = SetWebhook {
            url: "https://bot.example.com/hook".parse().unwrap(),
            secret_token: None,
            allowed_updates: None,
            max_connections: None,
            drop_pending_updates: false,
            ip_address: None,
            certificate: None,
        };
        assert_eq!(
            bot.set_webhook(&req).await,
            Err(ApiError::Http {
                status: 409,
                description: "Conflict: terminated by other getUpdates request".into(),
                retry_after: None,
            })
        );
    }

    #[tokio::test]
    async fn set_webhook_multipart_with_certificate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(at("setWebhook")))
            .and(header_regex(
                "content-type",
                "^multipart/form-data; boundary=",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"{"ok":true,"result":true,"description":"Webhook was set"}"#,
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
        let bot = client_for(&server.uri());
        bot.set_webhook(&SetWebhook {
            url: "https://203.0.113.5:8443/webhook/shop".parse().unwrap(),
            secret_token: Some(Secret::new("s3cret".into())),
            allowed_updates: Some(vec!["message".into(), "callback_query".into()]),
            max_connections: Some(40),
            drop_pending_updates: true,
            ip_address: Some("203.0.113.5".parse().unwrap()),
            certificate: Some(Bytes::from_static(pem.as_bytes())),
        })
        .await
        .unwrap();

        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&requests[0].body);
        for (name, value) in [
            ("url", "https://203.0.113.5:8443/webhook/shop"),
            ("secret_token", "s3cret"),
            ("allowed_updates", r#"["message","callback_query"]"#),
            ("max_connections", "40"),
            ("drop_pending_updates", "true"),
            ("ip_address", "203.0.113.5"),
        ] {
            let part = format!("name=\"{name}\"\r\n\r\n{value}\r\n");
            assert!(body.contains(&part), "missing {part:?} in {body}");
        }
        assert!(
            body.contains("name=\"certificate\"; filename=\"certificate.pem\""),
            "{body}"
        );
        assert!(body.contains(pem), "{body}");
    }

    #[tokio::test]
    async fn errors_never_contain_token() {
        let server = MockServer::start().await;
        // A misbehaving server that echoes the token back.
        Mock::given(path(at("getMe")))
            .respond_with(ResponseTemplate::new(400).set_body_string(format!(
                r#"{{"ok":false,"error_code":400,"description":"Bad Request: bot{TOKEN} is odd"}}"#
            )))
            .mount(&server)
            .await;
        Mock::given(path(at("deleteWebhook")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!(r#"{{"ok":true,"result":"{TOKEN}"#)),
            )
            .mount(&server)
            .await;
        Mock::given(path(at("getUpdates")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"ok":false,"error_code":409,"description":"Conflict: bot{TOKEN} polls"}}"#
            )))
            .mount(&server)
            .await;
        let bot = client_for(&server.uri());

        let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let unreachable = client_for(&format!("http://127.0.0.1:{closed_port}"));

        // A 2xx body that is not `"ok":false` is returned for `split_updates`.
        let odd_body = Bytes::from(format!(r#"{{"ok":"{TOKEN}"}}"#));
        let errors = [
            bot.get_me().await.unwrap_err(),
            bot.delete_webhook(false).await.unwrap_err(),
            bot.get_updates(0, 1, Duration::ZERO, None)
                .await
                .unwrap_err(),
            split_updates(&odd_body).unwrap_err(),
            unreachable.get_me().await.unwrap_err(),
            BotClient::new(
                http(),
                &server.uri().parse().unwrap(),
                &Secret::new(format!("{TOKEN}/x")),
            )
            .unwrap_err(),
        ];
        assert!(
            matches!(&errors[2], ApiError::Http { status: 409, description, .. }
                if description == "Conflict: bot[redacted] polls"),
            "{:?}",
            errors[2]
        );
        assert!(matches!(errors[3], ApiError::Decode(_)), "{:?}", errors[3]);
        assert!(matches!(errors[4], ApiError::Network(_)), "{:?}", errors[4]);
        for err in &errors {
            for rendered in [err.to_string(), format!("{err:?}")] {
                assert!(!rendered.contains("AAAA"), "token leaked: {rendered}");
            }
        }
        assert!(!format!("{bot:?}").contains("AAAA"));
    }

    #[test]
    fn new_rejects_bad_setup() {
        let token = Secret::new(TOKEN.to_owned());
        let url = |s: &str| s.parse::<reqwest::Url>().unwrap();
        assert!(BotClient::new(http(), &url("https://api.telegram.org"), &token).is_ok());
        for bad in ["mailto:a@b.c", "ftp://api.telegram.org"] {
            assert!(matches!(
                BotClient::new(http(), &url(bad), &token),
                Err(ApiError::Config(_))
            ));
        }
        for bad in ["", "1:a/b", "1:a?b", "1 a"] {
            assert!(matches!(
                BotClient::new(
                    http(),
                    &url("https://api.telegram.org"),
                    &Secret::new(bad.into())
                ),
                Err(ApiError::Config(_))
            ));
        }
    }

    #[test]
    fn set_webhook_debug_shows_origin_only() {
        let req = |url: &str| SetWebhook {
            url: url.parse().unwrap(),
            secret_token: Some(Secret::new("hush-hush-value".to_owned())),
            allowed_updates: Some(vec!["message".to_owned()]),
            max_connections: Some(40),
            drop_pending_updates: true,
            ip_address: Some("1.2.3.4".parse().unwrap()),
            certificate: None,
        };
        let shown = format!(
            "{:?}",
            req("https://user:pw@hook.example:8443/bot/PathSecret?q=QuerySecret#Frag")
        );
        assert!(
            shown.contains(r#"url: "https://hook.example:8443/…""#),
            "{shown}"
        );
        for hidden in ["user", "pw", "PathSecret", "QuerySecret", "Frag", "hush"] {
            assert!(!shown.contains(hidden), "{hidden} in {shown}");
        }
        assert!(shown.contains("secret_token: Some([redacted])"), "{shown}");
        assert!(
            shown.contains(r#"allowed_updates: Some(["message"])"#),
            "{shown}"
        );
        assert!(shown.contains("max_connections: Some(40)"), "{shown}");
        assert!(shown.contains("drop_pending_updates: true"), "{shown}");
        assert!(shown.contains("ip_address: Some(1.2.3.4)"), "{shown}");
        assert!(shown.contains("certificate: None"), "{shown}");

        // Nothing elided: no ellipsis, and the default port is implied.
        let shown = format!("{:?}", req("https://hook.example/"));
        assert!(shown.contains(r#"url: "https://hook.example""#), "{shown}");
        let shown = format!("{:?}", req("http://[::1]:8080"));
        assert!(shown.contains(r#"url: "http://[::1]:8080""#), "{shown}");
        let shown = format!("{:?}", req("https://hook.example/?"));
        assert!(
            shown.contains(r#"url: "https://hook.example/…""#),
            "{shown}"
        );
    }
}
