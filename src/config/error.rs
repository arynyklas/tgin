//! Configuration loading errors.
//!
//! Loading a `tgin.ron` runs three sequential steps:
//!
//! 1. Read the file, then substitute every `${VAR}` placeholder with its
//!    environment value. Missing vars are collected — the operator sees the
//!    full list, not the first failure.
//! 2. Parse the substituted text as RON.
//! 3. Walk the parsed tree and validate every semantic rule we can check
//!    before the runtime starts. Validation errors are collected too.
//!
//! `ConfigError` carries the failure of any step. Steps 1 and 3 collect
//! _every_ issue they can see so the operator fixes the config in one
//! editing pass instead of one error per restart.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Top-level loader error. Wraps the per-stage failure so `main` can format
/// a single human-readable message and exit non-zero.
#[derive(Debug)]
pub enum ConfigError {
    /// `fs::read_to_string` failed (missing file, permissions, …).
    FileRead { path: PathBuf, source: io::Error },
    /// One or more `${VAR}` placeholders refer to env vars that are unset.
    /// The full list is reported so the operator does not iterate one
    /// missing var per restart.
    MissingEnvVars(Vec<String>),
    /// RON failed to parse the substituted text.
    Parse { source: ron::error::SpannedError },
    /// Semantic validation found one or more issues. All issues are
    /// reported together.
    Validation(Vec<ValidationError>),
}

/// A single semantic-validation finding. Rendered with field-style location
/// plus a short remediation hint so the operator can fix it without grepping
/// the source.
#[derive(Debug, PartialEq, Eq)]
pub enum ValidationError {
    /// A load-balancer node has zero children. Updates routed there are
    /// silently dropped.
    EmptyLoadBalancer { kind: &'static str, location: String },
    /// Two HTTP-mounted endpoints (`LongPollRoute` or `WebhookUpdate`) want
    /// the same path. Whichever registers second clobbers the first in
    /// `axum::Router` / `LONGPOLL_REGISTRY`.
    DuplicatePath {
        path: String,
        first: &'static str,
        second: &'static str,
    },
    /// `WebhookRoute.url` is not a parseable absolute URL. Per-update HTTP
    /// failures would otherwise be invisible (egress errors are swallowed).
    InvalidWebhookUrl {
        url: String,
        location: String,
        reason: String,
    },
    /// `RegistrationWebhookConfig.public_ip` is not a parseable absolute URL.
    InvalidPublicIp {
        url: String,
        location: String,
        reason: String,
    },
    /// `RegistrationWebhookConfig.set_webhook_url` is not a parseable absolute URL.
    InvalidSetWebhookUrl {
        url: String,
        location: String,
        reason: String,
    },
    /// `dark_threads: 0` panics deep inside `tokio::runtime::Builder`.
    DarkThreadsZero,
    /// SSL cert / key file is unreadable. Without this check, the failure
    /// surfaces inside an `axum_server::tls_rustls` task far from the
    /// config site.
    SslFileUnreadable {
        field: &'static str,
        path: PathBuf,
        source_kind: io::ErrorKind,
        source_msg: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::FileRead { path, source } => {
                write!(f, "failed to read config file {}: {source}", path.display())
            }
            ConfigError::MissingEnvVars(vars) => {
                writeln!(
                    f,
                    "{} environment variable(s) referenced in config but not set:",
                    vars.len()
                )?;
                for v in vars {
                    writeln!(f, "  - ${{{v}}}")?;
                }
                write!(
                    f,
                    "hint: export the missing variables before starting tgin"
                )
            }
            ConfigError::Parse { source } => write!(f, "failed to parse RON config: {source}"),
            ConfigError::Validation(errs) => {
                writeln!(f, "{} configuration validation error(s):", errs.len())?;
                for (i, e) in errs.iter().enumerate() {
                    write!(f, "  {}. {e}", i + 1)?;
                    if i + 1 != errs.len() {
                        writeln!(f)?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::EmptyLoadBalancer { kind, location } => write!(
                f,
                "{location}: {kind} has no routes; updates dispatched here would be silently \
                 dropped (hint: add at least one child route or remove the load balancer)"
            ),
            ValidationError::DuplicatePath { path, first, second } => write!(
                f,
                "duplicate HTTP path {path:?}: registered by {first} and {second} \
                 (hint: each LongPollRoute / WebhookUpdate path must be unique within the process)"
            ),
            ValidationError::InvalidWebhookUrl { url, location, reason } => write!(
                f,
                "{location}: WebhookRoute.url {url:?} is not a valid absolute URL ({reason}) \
                 (hint: use an absolute http(s):// URL, e.g. \"http://127.0.0.1:8080/bot\")"
            ),
            ValidationError::InvalidPublicIp { url, location, reason } => write!(
                f,
                "{location}: registration.public_ip {url:?} is not a valid absolute URL \
                 ({reason}) (hint: use the public https:// origin Telegram will reach, \
                 e.g. \"https://example.com\")"
            ),
            ValidationError::InvalidSetWebhookUrl { url, location, reason } => write!(
                f,
                "{location}: registration.set_webhook_url {url:?} is not a valid absolute URL \
                 ({reason})"
            ),
            ValidationError::DarkThreadsZero => write!(
                f,
                "dark_threads: 0 is not supported (hint: omit the field for default 4, or set >= 1)"
            ),
            ValidationError::SslFileUnreadable {
                field,
                path,
                source_kind: _,
                source_msg,
            } => write!(
                f,
                "ssl.{field}: file {} is not readable ({source_msg}) \
                 (hint: check the path and the process user's permissions)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ConfigError {}
