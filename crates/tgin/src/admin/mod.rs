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
//!
//! The token is `Authorization: Bearer <admin.token>`; without an
//! `admin.token` (loopback listeners only) the API needs none. After a
//! reload removes the token while a non-loopback listener is bound, the
//! protected endpoints answer 401 until a restart. Failures
//! answer `{"error":"…"}`: 401 `unauthorized`, 404 `unknown consumer`, 503
//! `draining` for a reload during the shutdown, 409 `draining` for a resume
//! during it.

mod api;
pub mod client;

pub(crate) use api::router;
use serde::{Deserialize, Serialize};

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
