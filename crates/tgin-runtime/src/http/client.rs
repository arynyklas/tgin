//! Outbound HTTP client and URL-free error rendering.

use std::error::Error as _;
use std::fmt::Write as _;
use std::time::Duration;

/// Timeouts for [`client`].
#[derive(Clone, Debug)]
pub struct ClientOptions {
    /// Bound on establishing the TCP connection and TLS session.
    pub connect_timeout: Duration,
    /// Bound on a whole request, from connecting to the end of the response
    /// body; a request can override it with `RequestBuilder::timeout`.
    pub request_timeout: Duration,
    /// How long an idle pooled connection is kept for reuse.
    pub pool_idle_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(10),
            pool_idle_timeout: Duration::from_secs(90),
        }
    }
}

/// Builds a `reqwest::Client` (rustls with aws-lc-rs, HTTP/1.1 and HTTP/2).
/// The client is cheap to clone; clones share one connection pool.
pub fn client(opts: &ClientOptions) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(opts.connect_timeout)
        .timeout(opts.request_timeout)
        .pool_idle_timeout(opts.pool_idle_timeout)
        .build()
        .expect("a reqwest client with only timeouts configured always builds")
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

    #[tokio::test]
    async fn describe_error_reports_timeout_kind() {
        // The kernel completes the handshake, but nothing ever answers.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/bot1:SECRET/getMe", silent.local_addr().unwrap());

        let err = client(&ClientOptions::default())
            .get(&url)
            .timeout(Duration::from_millis(100))
            .send()
            .await
            .expect_err("the server never responds");

        let described = describe_error(&err);
        assert!(described.contains("timeout: true"), "{described}");
        assert!(!described.contains("SECRET"), "{described}");
    }
}
