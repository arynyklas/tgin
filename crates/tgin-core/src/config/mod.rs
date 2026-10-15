//! The configuration file, version 1: its serde model ([`model`]),
//! validation, resolution into a [`Config`], and JSON Schema.

mod defaults;
pub mod model;
mod resolve;
mod schema;
mod validate;

use std::path::Path;

use tgin_runtime::config::{ConfigError, Env, parse_yaml};

pub use self::model::RawConfig;
pub use self::resolve::{
    Admin, Balance, Bot, Config, Consumer, ConsumerKind, Endpoint, Ingress, IpAllowlist, Log,
    LogFormat, LogLevel, LongPoll, Pull, Push, Reload, Retry, Runtime, Server, Tls, Webhook,
    WebhookSecret,
};
pub use self::schema::schema;

/// Parses, validates and resolves a config file. `origin` names the file in
/// messages, and relative `${file:…}` references resolve against its
/// directory.
///
/// On success the config's rules compile, and [`Config::warnings`] holds
/// problems that do not reject it.
///
/// # Errors
///
/// A YAML or shape error alone, or else every validation error, ordered by
/// position.
pub fn load(text: &str, origin: &Path, env: &dyn Env) -> Result<Config, Vec<ConfigError>> {
    let raw: RawConfig = parse_yaml(text, origin).map_err(|err| vec![err])?;
    let mut diag = validate::Diagnostics::default();
    let rules_ok = validate::validate(&raw, &mut diag);
    let base_dir = origin.parent().unwrap_or(Path::new(""));
    let mut config = resolve::resolve(raw, env, base_dir, &rules_ok, &mut diag);
    if !diag.errors.is_empty() {
        diag.errors.sort_by_key(|err| {
            err.at
                .as_ref()
                .map_or((u32::MAX, u32::MAX), |at| (at.line, at.column))
        });
        return Err(diag.errors);
    }
    config.warnings = diag.warnings;
    Ok(config)
}

/// The config as written, for `/v1/config`: defaults filled in, `${…}`
/// references unexpanded and every secret `"[redacted]"`.
pub fn redacted_json(cfg: &Config) -> serde_json::Value {
    // Serializing to a `Value` fails only on non-string map keys, which the
    // model does not have.
    serde_json::to_value(&cfg.raw).unwrap_or(serde_json::Value::Null)
}
