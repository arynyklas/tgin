//! Default values of optional config fields.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use super::model::{
    Admin, At, Balance, Breaker, ByteSize, HumanDuration, IpAllowlist, Log, LogFormat, LogLevel,
    LongPoll, OnFull, Ordering, Pull, Reload, Retry, Runtime, Server, Template, Webhook,
    WebhookSecret, WorkerThreads,
};

/// The Telegram Bot API.
const API_URL: &str = "https://api.telegram.org";
/// The data-plane listener.
const SERVER_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080);
/// The admin listener.
const ADMIN_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9090);

fn duration(value: Duration) -> At<HumanDuration> {
    At::new(HumanDuration::from(value))
}

const fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

pub(super) fn api_url() -> Template {
    Template(At::new(API_URL.to_owned()))
}

pub(super) fn balance() -> Balance {
    Balance::RoundRobin
}

pub(super) fn max_in_flight() -> At<usize> {
    At::new(32)
}

pub(super) fn ordering() -> Ordering {
    Ordering::None
}

pub(super) fn capacity() -> At<usize> {
    At::new(10_000)
}

pub(super) fn capacity_bytes() -> At<ByteSize> {
    At::new(ByteSize(64 << 20))
}

pub(super) fn on_full() -> OnFull {
    OnFull::Backpressure
}

pub(super) fn endpoint_timeout() -> At<HumanDuration> {
    duration(secs(10))
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            worker_threads: WorkerThreads::Auto,
            lame_duck: duration(secs(5)),
            shutdown_grace: duration(secs(15)),
        }
    }
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            format: LogFormat::Text,
        }
    }
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen: vec![At::new(SERVER_LISTEN)],
            tls: None,
            trusted_proxies: Vec::new(),
            max_connections: At::new(10_000),
            header_read_timeout: duration(secs(10)),
        }
    }
}

impl Default for Admin {
    fn default() -> Self {
        Self {
            listen: vec![At::new(ADMIN_LISTEN)],
            token: None,
            public_metrics: false,
        }
    }
}

impl Default for Reload {
    fn default() -> Self {
        Self {
            watch: true,
            debounce: duration(Duration::from_millis(500)),
            drain_timeout: duration(secs(30)),
        }
    }
}

impl Default for LongPoll {
    fn default() -> Self {
        Self {
            timeout: duration(secs(30)),
            limit: At::new(100),
            allowed_updates: None,
            delete_webhook_on_start: true,
        }
    }
}

impl Default for Webhook {
    fn default() -> Self {
        Self {
            path: None,
            public_url: None,
            register: At::new(false),
            secret_token: WebhookSecret::Auto,
            allowed_updates: None,
            max_connections: None,
            drop_pending_updates: false,
            ip_address: None,
            certificate_file: None,
            ip_allowlist: IpAllowlist::None,
        }
    }
}

impl Default for Pull {
    fn default() -> Self {
        Self {
            capacity: capacity(),
            capacity_bytes: capacity_bytes(),
            on_full: on_full(),
            lease_timeout: duration(secs(120)),
            ordering: ordering(),
            auth_token: None,
            aliases: Vec::new(),
        }
    }
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            initial: duration(secs(1)),
            max: duration(secs(60)),
            max_attempts: At::new(10),
            max_age: duration(secs(3600)),
        }
    }
}

impl Default for Breaker {
    fn default() -> Self {
        Self {
            failure_threshold: At::new(5),
            open_for: duration(secs(30)),
        }
    }
}
