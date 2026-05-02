//! Shared `reqwest::Client` factory.
//!
//! All ingress / egress / management code paths use a single process-wide HTTP
//! client. Sharing the client preserves the connection pool, DNS resolver, and
//! TLS state across N bots / N webhook routes — building one client per
//! `LongPollUpdate` / `WebhookRoute` / `RegistrationWebhookConfig` would
//! duplicate that state and prevent TCP/TLS reuse.
//!
//! No global request `timeout` is configured: the longest legitimate request
//! is a Telegram long-poll, which can hold the connection up to ~50 s. Each
//! call site sets its own per-request `RequestBuilder::timeout` so the
//! deadline matches the operation, while `connect_timeout` on the client
//! still bounds the TCP/TLS handshake.

use std::time::Duration;

use reqwest::Client;

const POOL_MAX_IDLE_PER_HOST: usize = 32;
/// Evict idle keep-alive connections before the typical server-side keep-alive
/// timeout (axum/hyper default ~75 s, many proxies 60 s). Without this we
/// occasionally pick a connection the peer has already closed, hyper surfaces
/// it as `IncompleteMessage`, and the call site treats it as a network error
/// — contributing to long-poll tail latency on otherwise-healthy upstreams.
/// 30 s is well under any reasonable server timeout while still amortising TCP
/// handshakes across the long-poll cycle.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);
const HTTP2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
const HTTP2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the shared `reqwest::Client` used by every HTTP component in the
/// process. `Client` is cheaply `Clone` (it is `Arc`-internal); call sites
/// receive a clone and share the underlying pool.
pub fn build_shared_client() -> Client {
    Client::builder()
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .http2_keep_alive_interval(HTTP2_KEEP_ALIVE_INTERVAL)
        .http2_keep_alive_timeout(HTTP2_KEEP_ALIVE_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .expect("Failed to build shared reqwest::Client")
}
