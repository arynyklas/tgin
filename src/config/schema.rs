use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct TginConfig {
    #[serde(default = "default_workers")]
    pub dark_threads: usize,
    pub server_port: Option<u16>,
    #[serde(default)]
    pub ssl: Option<SslConfig>,
    pub updates: Vec<UpdateConfig>,
    pub route: RouteConfig,
    pub api: Option<ApiConfig>,
}

fn default_workers() -> usize {
    4
}

#[derive(Deserialize, Debug)]
pub struct SslConfig {
    pub cert: String,
    pub key: String,
}

#[derive(Deserialize, Debug)]
pub struct ApiConfig {
    pub base_path: String,
}

#[derive(Deserialize, Debug)]
pub enum UpdateConfig {
    LongPollUpdate {
        token: String,
        url: Option<String>,
        #[serde(default = "default_timeout")]
        default_timeout_sleep: u64,
        #[serde(default = "default_timeout")]
        error_timeout_sleep: u64,
        /// Server-side hold for `getUpdates` (Telegram caps at 50). Default: 30.
        #[serde(default = "default_long_poll_timeout")]
        long_poll_timeout: u64,
        /// Max updates per `getUpdates` response (Telegram caps at 100). Default: 100.
        #[serde(default = "default_long_poll_limit")]
        long_poll_limit: u64,
    },
    WebhookUpdate {
        path: String,
        registration: Option<RegistrationWebhookConfig>,
        #[serde(default)]
        secret_token: Option<String>,
    },
}

fn default_timeout() -> u64 {
    100
}

fn default_long_poll_timeout() -> u64 {
    30
}

fn default_long_poll_limit() -> u64 {
    100
}

#[derive(Deserialize, Debug)]
pub struct RegistrationWebhookConfig {
    pub public_ip: String,
    pub set_webhook_url: Option<String>,
    pub token: String,
}
#[derive(Deserialize, Debug)]
pub enum RouteConfig {
    LongPollRoute {
        path: String,
    },
    WebhookRoute {
        url: String,
        /// Per-request deadline (in milliseconds) for forwarding an update to
        /// this downstream. Defaults to `WebhookRoute`'s `DEFAULT_REQUEST_TIMEOUT`.
        #[serde(default = "default_webhook_request_timeout_ms")]
        request_timeout_ms: u64,
    },

    RoundRobinLB {
        routes: Vec<RouteConfig>,
    },
    AllLB {
        routes: Vec<RouteConfig>,
    },
}

fn default_webhook_request_timeout_ms() -> u64 {
    crate::route::webhook::DEFAULT_REQUEST_TIMEOUT.as_millis() as u64
}
