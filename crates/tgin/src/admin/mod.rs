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
//! | `POST /v1/accounts/{name}/login` `{"method":"qr"\|"code"}` | token | 200 [`LoginStatus`] |
//! | `GET /v1/accounts/{name}/login` | token | 200 [`LoginStatus`] |
//! | `POST /v1/accounts/{name}/login/code` `{"code"}`, `/login/password` `{"password"}` | token | 200 [`LoginStatus`] |
//! | `DELETE /v1/accounts/{name}/login` | token | 200 `{"cancelled":true}` |
//! | `POST /v1/accounts/{name}/logout` | token | 200 `{"logged_out":true}` |
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
//! (`30s`, `2m`…; `reload.drain_timeout` without one; at most `1h` either way),
//! counted from the request, the server runs dry of the bot's updates, the
//! bot's ingest stops, tgin delivers what it holds of the bot, and the
//! gateway stops sending the bot's calls to the server and waits for those
//! being sent; then the server is called (`logOut`, or `deleteWebhook` and
//! `close`) and the bot is parked: reloads do not start it again until one
//! changes its upstream. A move whose requester goes away before the server
//! has run dry of the bot's updates is dropped, having changed nothing;
//! after that it goes on. Failures: 400 `invalid
//! timeout`, 404 `unknown bot`, 409 `draining` during the shutdown, 409
//! `parked`, 504 `upstream not drained` when the server still held updates
//! `min(5 s, timeout/4)` before the deadline, or could not say (the bot
//! runs on unchanged; the warning logged names the server's error), 504
//! `calls in flight` when
//! calls of the bot were still being sent to the server at the deadline,
//! their count and the way out in `description` (nothing was called on the
//! server; the bot runs on), 502 `upstream
//! refused` with the server's masked text in `description` when it refused
//! the call (the bot runs on).
//!
//! `POST /v1/accounts/{name}/login` logs a Telegram account in, driving
//! the login on tgin's own MTProto sender: `qr` answers
//! `{"state":"qr","url":"tg://login?token=…","expires":…}` and renews the
//! token by itself until the QR is scanned (`GET …/login` shows the
//! current one); `code` sends a login code to the config's `phone` and
//! answers `code_sent`. A cloud password then gives `need_password`. The
//! end is `logged_in` or `failed` with a `reason` (`wrong_account` when
//! the account's phone is not the config's `phone`: that login is logged
//! out again). A repeated `POST …/login` while one runs answers its state.
//! A fifth wrong password or code ends the login (`password_attempts`,
//! `code_attempts`).
//! A QR login ends `failed` (`expired`) after 5 minutes unscanned, and any
//! login after 5 minutes without the code or password it waits for;
//! `DELETE …/login` ends one at once (`tgin admin login` sends it on
//! Ctrl+C), and a reload ends one whose account it removed or whose
//! `session_dir` or `phone` it changed.
//! The `tg://login` URL appears in these answers only; it, codes and
//! passwords are never logged. `POST …/logout` calls `auth.logOut` and
//! clears the session (keys, login, peers, update state; the directory
//! stays, under its lock). The account routes need `admin.token`: without one they answer 403
//! `admin.token required for account login` (a token-less loopback listener
//! is open to every local user and every web page), and every login
//! answer carries `Cache-Control: no-store`. Failures: 400 `invalid body`, 404 `unknown
//! account`, 404 `no login`, 409 `account running` (both refuse while the
//! account runs), 409 `logged in` (log out first), 409 `session held` (by
//! another process), 409 `session unverified` (a key left by an unfinished
//! login, which Telegram gave no clear answer about: try again, or log
//! out), 409 `login in progress` (with the other method), 422
//! with a `reason` for a wrong code or password (try again) or one the
//! login does not wait for, 502 `logout unconfirmed` when Telegram did
//! not confirm the logout (nothing is cleared).
//!
//! API v1 grows additively within a major version: answers may gain
//! fields, enum values (state and phase words) and error codes, and the
//! client ignores what it does not know.

mod accounts;
mod api;
pub mod client;

pub(crate) use accounts::{Accounts, Sessions, target_of};
pub(crate) use api::{loop_detected, router};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The longest `timeout` of a bot move: the move holds up every reload
/// while it runs.
pub const MAX_MOVE_TIMEOUT: Duration = Duration::from_secs(3600);

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

/// The `tg://login?token=…` URL of a QR login. A secret: `Debug` hides it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LoginUrl(pub String);

impl std::fmt::Debug for LoginUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoginUrl(..)")
    }
}

/// `/v1/accounts/{name}/login`: where an account's login stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LoginStatus {
    /// Connecting to Telegram.
    Connecting,
    /// Show `url` as a QR until `expires` (Telegram's unix time); `GET`
    /// gives the next one.
    Qr {
        url: LoginUrl,
        expires: i64,
    },
    /// The login code went out by `kind` (`app`, `sms`, `call`…).
    CodeSent {
        kind: String,
        timeout: Option<i32>,
    },
    /// The account has a cloud password.
    NeedPassword {
        hint: Option<String>,
    },
    /// An official app must add a login email first.
    NeedEmailSetup,
    /// The code went to the login email, which tgin does not support.
    NeedEmailCode,
    PaymentRequired,
    LoggedIn {
        user_id: i64,
    },
    /// The login ended without one; `reason` is stable, `message` for
    /// people.
    Failed {
        reason: String,
        message: String,
    },
    /// A state this client does not know.
    #[serde(other)]
    Unknown,
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
    /// The name of the bot's upstream (`api_url` for the shorthand); empty
    /// from a server older than this client.
    #[serde(default)]
    pub upstream: String,
    /// Whether the bot was moved off its Bot API server; shown only while
    /// it is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parked: bool,
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
