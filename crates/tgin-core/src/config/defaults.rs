//! Default values of optional config fields.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use super::model::{
    Admin, At, Balance, Breaker, ByteSize, ChatRate, Endpoint, Gateway, GlobalRate, GroupRate,
    HumanDuration, IpAllowlist, Log, LogFormat, LogLevel, LongPoll, OnFull, Ordering, Pull,
    RateLimit, RateMode, Reload, Retry, Retry429, Runtime, Server, Template, Webhook, WebhookReply,
    WebhookSecret, WorkerThreads,
};

/// The Telegram Bot API.
pub(super) const API_URL: &str = "https://api.telegram.org";
/// A `cloud` upstream's `response_timeout`.
pub(super) const CLOUD_RESPONSE_TIMEOUT: Duration = secs(60);
/// A `local` upstream's `response_timeout`: a local server takes uploads of
/// up to 2 GB and may answer long after the body was sent.
pub(super) const LOCAL_RESPONSE_TIMEOUT: Duration = secs(600);
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

pub(super) fn local_mode() -> At<bool> {
    At::new(false)
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
    duration(super::DEFAULT_ENDPOINT_TIMEOUT)
}

pub(super) fn endpoints() -> At<Vec<Endpoint>> {
    At::new(Vec::new())
}

pub(super) fn webhook_reply() -> WebhookReply {
    WebhookReply::Execute
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
            ip_allowlist: At::new(IpAllowlist::None),
        }
    }
}

impl Default for Gateway {
    fn default() -> Self {
        Self {
            upstream_idle_timeout: duration(secs(60)),
            rate_limit: RateLimit::default(),
            retry_429: Retry429::default(),
        }
    }
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            mode: RateMode::Shape,
            max_delay: duration(secs(5)),
            methods: At::new(
                ["send*", "copyMessage*", "forwardMessage*"]
                    .map(|method| At::new(method.to_owned()))
                    .into(),
            ),
            global: GlobalRate::default(),
            per_chat: ChatRate::default(),
            per_group: GroupRate::default(),
        }
    }
}

impl Default for GlobalRate {
    fn default() -> Self {
        Self {
            count: At::new(30),
            per: duration(secs(1)),
            burst: At::new(30),
        }
    }
}

impl Default for ChatRate {
    fn default() -> Self {
        Self {
            count: At::new(1),
            per: duration(secs(1)),
            burst: At::new(3),
        }
    }
}

impl Default for GroupRate {
    fn default() -> Self {
        Self {
            count: At::new(20),
            per: duration(secs(60)),
            burst: At::new(20),
        }
    }
}

impl Default for Retry429 {
    fn default() -> Self {
        Self {
            max_wait: duration(secs(10)),
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
            honor_drop_pending: false,
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
