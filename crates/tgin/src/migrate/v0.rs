//! The tgin 0.2 RON config file, as 0.2 read it.

use serde::Deserialize;

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct TginConfig {
    #[serde(default = "default_workers")]
    pub dark_threads: usize,
    pub server_port: Option<u16>,
    #[serde(default)]
    pub ssl: Option<SslConfig>,
    pub updates: Vec<UpdateConfig>,
    pub route: RouteConfig,
    pub api: Option<ApiConfig>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub log_format: LogFormat,
    /// Shared secret for the control / observability plane. When set,
    /// `/status`, `/metrics`, and the management API (`/<api.base_path>/*`)
    /// require `Authorization: Bearer <auth_token>`. Required (non-blank)
    /// whenever `api` is set; without `api` it may be omitted to leave
    /// `/status` and `/metrics` open, relying on network-layer isolation.
    #[serde(default)]
    pub auth_token: Option<String>,
}

fn default_workers() -> usize {
    4
}

fn default_log_level() -> String {
    "info".to_string()
}

/// Subscriber output format selected by the `log_format` config field.
/// `Compact` is human-readable single-line; `Json` emits one JSON object
/// per line for log shippers.
#[derive(Deserialize, Debug, Default, Clone, Copy)]
pub enum LogFormat {
    #[default]
    Compact,
    Json,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct SslConfig {
    pub cert: String,
    pub key: String,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct ApiConfig {
    pub base_path: String,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
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
    0
}

fn default_long_poll_timeout() -> u64 {
    30
}

fn default_long_poll_limit() -> u64 {
    100
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct RegistrationWebhookConfig {
    pub public_ip: String,
    pub set_webhook_url: Option<String>,
    pub token: String,
}
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub enum RouteConfig {
    LongPollRoute {
        path: String,
        /// Cap on per-route in-memory buffer depth. When reached, oldest
        /// updates are evicted FIFO and lost: tgin has already confirmed them
        /// to Telegram with its next `getUpdates`, so Telegram cannot
        /// redeliver them.
        /// Default: 10 000.
        #[serde(default = "default_max_buffered_updates")]
        max_buffered_updates: usize,
    },
    WebhookRoute {
        url: String,
        /// Per-request deadline (in milliseconds) for forwarding an update to
        /// this downstream. Default: 10 000 (10 s).
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

/// 0.2's `WebhookRoute` request timeout.
fn default_webhook_request_timeout_ms() -> u64 {
    10_000
}

/// 0.2's `LongPollRoute` buffer cap.
fn default_max_buffered_updates() -> usize {
    10_000
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// The 0.2 configs still in the repository parse with these types.
    #[test]
    fn repository_ron_configs_parse() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = vec![root.join("examples/simple/tgin.ron")];
        let perf = root.join("tests/performance/cfgs");
        for entry in std::fs::read_dir(&perf).expect("perf configs") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|ext| ext == "ron") {
                files.push(path);
            }
        }
        assert!(files.len() > 1, "no RON config under {}", perf.display());
        for file in files {
            let text = std::fs::read_to_string(&file).expect("readable");
            if let Err(err) = ron::from_str::<TginConfig>(&text) {
                panic!("{}: {err}", file.display());
            }
        }
    }

    #[test]
    fn defaults_are_0_2_defaults() {
        let config: TginConfig = ron::from_str(
            r#"(server_port: None, updates: [], api: None, route: AllLB(routes: [
                LongPollRoute(path: "/a"),
                WebhookRoute(url: "http://b/"),
            ]))"#,
        )
        .expect("parses");
        assert_eq!(config.dark_threads, 4);
        assert_eq!(config.log_level, "info");
        let RouteConfig::AllLB { routes } = config.route else {
            panic!("not AllLB");
        };
        assert!(matches!(
            routes[0],
            RouteConfig::LongPollRoute {
                max_buffered_updates: 10_000,
                ..
            }
        ));
        assert!(matches!(
            routes[1],
            RouteConfig::WebhookRoute {
                request_timeout_ms: 10_000,
                ..
            }
        ));
    }
}
