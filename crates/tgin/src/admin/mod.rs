//! The admin plane, a listener of its own (`admin.listen`): probes, metrics
//! and the management API v1, and `tgin admin`, its command-line client.
//!
//! | Method and path | Auth | Answer |
//! |---|---|---|
//! | `GET /livez` | none | 200 `ok` |
//! | `GET /readyz` | none | 200 or 503 `{"ready","phase","components":{…}}` |
//! | `GET /metrics` | token unless `public_metrics` | OpenMetrics text |
//! | `GET /v1/status` | token | [`Status`] |
//! | `GET /v1/config` | token | the running config, secrets redacted |
//! | `POST /v1/reload` | token | 200 reload report; 422 `{"errors":[…]}` |
//! | `POST /v1/consumers/{name}/pause`, `/drain` | token | 204 |
//! | `POST /v1/consumers/{name}/resume` | token | 204: unpaused, and an admin drain ended |
//! | `POST /v1/consumers/{name}/purge` | token | 200 `{"purged":N}` |
//! | `POST /v1/bots/{name}/logout`, `/close` | token | 200 [`Moved`] |
//!
//! The token is `Authorization: Bearer <admin.token>`; without an
//! `admin.token` (loopback listeners only) the API needs none. After a
//! reload removes the token while a non-loopback listener is bound, the
//! protected endpoints answer 401 until a restart. Failures
//! answer `{"error":"…"}`: 401 `unauthorized`, 404 `unknown consumer`, 503
//! `draining` for a reload during the shutdown, 409 `draining` for a resume
//! during it.
//!
//! `POST /v1/bots/{name}/logout|close[?timeout=<duration>]` moves a bot off
//! its Bot API server, serialized with the reloads. Within the timeout
//! (`30s`, `2m`…; `reload.drain_timeout` without one) the server runs dry
//! of the bot's updates, the bot's ingest stops, and tgin delivers what it
//! holds of the bot; then the server is called (`logOut`, or
//! `deleteWebhook` and `close`) and the bot is parked: reloads do not start
//! it again until one changes its upstream. Failures: 400 `invalid
//! timeout`, 404 `unknown bot`, 409 `draining` during the shutdown, 409
//! `parked`, 504 `upstream not drained` when the server still held updates
//! at the deadline, or could not say (the bot runs on unchanged; the
//! warning logged names the server's error), 502 with the server's
//! description when it refused the call (the bot runs on).

mod api;
pub mod client;

pub(crate) use api::{loop_detected, router};
use serde::{Deserialize, Serialize};

/// How `POST /v1/bots/{name}/…` moves a bot off its Bot API server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BotMove {
    /// `logout`: the server's `logOut`.
    LogOut,
    /// `close`: the server's `deleteWebhook`, then `close`.
    Close,
}

impl BotMove {
    /// The endpoint's last path segment.
    pub fn segment(self) -> &'static str {
        match self {
            Self::LogOut => "logout",
            Self::Close => "close",
        }
    }

    /// The Bot API method that logs the bot out.
    pub fn method(self) -> &'static str {
        match self {
            Self::LogOut => "logOut",
            Self::Close => "close",
        }
    }
}

/// `POST /v1/bots/{name}/logout|close`: a bot moved off its server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moved {
    pub bot: String,
    /// The bot's updates tgin still held (queued or leased) when the
    /// server was called.
    pub left: usize,
    /// The name of the upstream the bot was logged out of (`api_url` for
    /// the shorthand).
    pub upstream: String,
    /// The server's description of its answer, if any, secrets masked.
    pub result: Option<String>,
}

/// `GET /v1/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    /// The config generation in effect; 1 at start, one more per reload.
    pub generation: u64,
    /// `starting`, `running`, `draining` or `stopped`.
    pub phase: String,
    pub sources: Vec<SourceStatus>,
    pub consumers: Vec<ConsumerStatus>,
    /// Every supervised task: sources, push dispatchers, background calls.
    pub tasks: Vec<TaskStatus>,
}

/// One bot's source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub name: String,
    /// `bot`.
    pub kind: String,
    /// `long_poll`, `webhook`, or `null` for a bot without ingress.
    pub ingress: Option<String>,
    /// `starting`, `running`, `backoff`, `failed` or `stopped`.
    pub state: String,
    pub updates_total: u64,
    pub errors_total: u64,
    /// Why the source backs off or failed, secrets masked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One consumer of the running config.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumerStatus {
    pub name: String,
    /// `pull` or `push`.
    pub mode: String,
    /// Updates waiting to be leased.
    pub depth: usize,
    pub leased: usize,
    pub in_flight: usize,
    pub paused: bool,
    /// A push consumer's endpoints, in config order, then the one a bot's
    /// `setWebhook` set; empty for pull.
    pub endpoints: Vec<EndpointStatus>,
}

/// One push endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointStatus {
    /// `scheme://host[:port]`: the URL's path, query and credentials are
    /// never shown.
    pub url_redacted: String,
    /// `closed`, `open` or `half_open`.
    pub breaker: String,
    /// Whether a bot's `setWebhook` set it (absent from older servers).
    #[serde(default)]
    pub runtime: bool,
}

/// One supervised task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStatus {
    pub name: String,
    /// `starting`, `running`, `backoff`, `failed` or `stopped`.
    pub state: String,
    /// The last error, secrets masked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
