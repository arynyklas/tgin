//! Outbound HTTP client and URL-free error rendering.

use std::error::Error as _;
use std::fmt::Write as _;
use std::time::Duration;

/// Timeouts and redirect handling for [`client`].
#[derive(Clone, Debug)]
pub struct ClientOptions {
    /// Bound on establishing the TCP connection and TLS session.
    pub connect_timeout: Duration,
    /// Bound on a whole request, from connecting to the end of the response
    /// body; a request can override it with `RequestBuilder::timeout`.
    pub request_timeout: Duration,
    /// How long an idle pooled connection is kept for reuse.
    pub pool_idle_timeout: Duration,
    /// Follow up to 10 redirects. Off by default: tgin never needs them, and
    /// following one turns a POST into a GET and can carry credential
    /// headers to another host.
    pub follow_redirects: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(10),
            pool_idle_timeout: Duration::from_secs(90),
            follow_redirects: false,
        }
    }
}

/// Builds a `reqwest::Client` (rustls with aws-lc-rs, HTTP/1.1 and HTTP/2).
/// The client is cheap to clone; clones share one connection pool.
///
/// # Panics
///
/// Panics if the host has no system CA certificate store: the client verifies
/// servers against the platform's trust roots and cannot be built without
/// them.
pub fn client(opts: &ClientOptions) -> reqwest::Client {
    client_with(opts, false)
}

/// [`client`]'s builder; `no_proxy` lets tests ignore proxy variables in
/// their environment.
fn client_with(opts: &ClientOptions, no_proxy: bool) -> reqwest::Client {
    let builder = reqwest::Client::builder()
        .connect_timeout(opts.connect_timeout)
        .timeout(opts.request_timeout)
        .pool_idle_timeout(opts.pool_idle_timeout)
        .redirect(if opts.follow_redirects {
            reqwest::redirect::Policy::limited(10)
        } else {
            reqwest::redirect::Policy::none()
        });
    let builder = if no_proxy {
        builder.no_proxy()
    } else {
        builder
    };
    builder
        .build()
        .expect("a system CA certificate store is required to build the HTTP client")
}

/// Renders a `reqwest` error as its kind plus the `Debug` form of its source
/// chain, without the request URL. Bot API URLs carry the bot token, and
/// `reqwest::Error`'s own `Debug` and `Display` print the URL.
pub fn describe_error(err: &reqwest::Error) -> String {
    let kind = if err.is_builder() {
        "builder"
    } else if err.is_redirect() {
        "redirect"
    } else if err.is_status() {
        "status"
    } else if err.is_body() {
        "body"
    } else if err.is_decode() {
        "decode"
    } else if err.is_upgrade() {
        "upgrade"
    } else {
        "request"
    };

    let mut out = format!("reqwest::Error {{ kind: {kind}");
    if let Some(status) = err.status() {
        let _ = write!(out, ", status: {}", status.as_u16());
    }
    if err.is_connect() {
        out.push_str(", connect: true");
    }
    if err.is_timeout() {
        out.push_str(", timeout: true");
    }
    // The `Debug` forms of hyper, hyper-util and io errors include their own
    // causes, so the direct source already renders the whole chain.
    if let Some(source) = err.source() {
        let _ = write!(out, ", source: {source:?}");
    }
    out.push_str(" }");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds() {
        // Building sends nothing, so proxy variables cannot affect it.
        let _ = client(&ClientOptions::default());
    }

    /// A server whose `/from` redirects to its `/to`.
    async fn redirecting() -> wiremock::MockServer {
        use wiremock::matchers::path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(path("/from"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/to", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(path("/to"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn client_does_not_follow_redirects_by_default() {
        let server = redirecting().await;
        let response = client_with(&ClientOptions::default(), true)
            .post(format!("{}/from", server.uri()))
            .send()
            .await
            .expect("response");
        assert_eq!(response.status().as_u16(), 302);
        let seen = server.received_requests().await.expect("recording on");
        assert_eq!(seen.len(), 1, "the redirect was followed");
    }

    #[tokio::test]
    async fn client_follows_redirects_when_asked() {
        let server = redirecting().await;
        let opts = ClientOptions {
            follow_redirects: true,
            ..ClientOptions::default()
        };
        let response = client_with(&opts, true)
            .post(format!("{}/from", server.uri()))
            .send()
            .await
            .expect("response");
        assert_eq!(response.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn describe_error_reports_timeout_kind() {
        // The kernel completes the handshake, but nothing ever answers.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/bot1:SECRET/getMe", silent.local_addr().unwrap());

        // `no_proxy` so an HTTP_PROXY or ALL_PROXY in the test environment
        // cannot reroute the request.
        let err = client_with(&ClientOptions::default(), true)
            .get(&url)
            .timeout(Duration::from_millis(100))
            .send()
            .await
            .expect_err("the server never responds");

        let described = describe_error(&err);
        assert!(described.contains("timeout: true"), "{described}");
        assert!(!described.contains("SECRET"), "{described}");
    }

    #[tokio::test]
    async fn describe_error_has_no_url() {
        let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{closed_port}/bot123456789:AAAAexampleSecretToken/x");

        let err = client_with(&ClientOptions::default(), true)
            .get(&url)
            .send()
            .await
            .expect_err("nothing listens on the port");
        assert!(
            format!("{err:?}").contains("AAAA"),
            "precondition: reqwest's own Debug output carries the URL"
        );

        let described = describe_error(&err);
        assert!(!described.contains("AAAA"), "token leaked: {described}");
        assert!(
            !described.contains("bot123456789"),
            "URL leaked: {described}"
        );
        assert!(described.contains("connect"), "kind missing: {described}");
    }
}
