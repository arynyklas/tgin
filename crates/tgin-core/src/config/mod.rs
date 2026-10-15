//! The configuration file, version 1: its serde model ([`model`]),
//! validation, resolution into a [`Config`], and JSON Schema.

mod defaults;
pub mod model;
mod resolve;
mod schema;
mod sensitive;
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
pub use self::sensitive::{Sensitive, SensitivePath, SensitiveRoute, SensitiveUrl};

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
    let raw: RawConfig = parse_yaml(text).map_err(|err| vec![err])?;
    let mut diag = validate::Diagnostics::default();
    validate::validate(&raw, &mut diag);
    let base_dir = origin.parent().unwrap_or(Path::new(""));
    let mut config = resolve::resolve(raw, env, base_dir, &mut diag);
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

/// The config as written, for `/v1/config`, with defaults filled in.
///
/// - Every secret is `"[redacted]"`.
/// - URL fields (`api_url`, `public_url`, endpoint `url`) show only the
///   resolved URL's origin, `scheme://host[:port]`, followed by `/…` when a
///   path, query or fragment was elided; their `${…}` references are not
///   shown.
/// - Every other string is as written, `${…}` references unexpanded, with
///   bot tokens and the config's secrets masked as `#####`.
pub fn redacted_json(cfg: &Config) -> serde_json::Value {
    use serde_json::Value;

    // Serializing to a `Value` fails only on non-string map keys, which the
    // model does not have.
    let mut json = serde_json::to_value(&cfg.raw).unwrap_or(Value::Null);
    mask_strings(&mut json, &cfg.masker);
    // Every URL field starts redacted, so one without a resolved counterpart
    // never shows as written.
    fn redact(value: Option<&mut Value>) {
        if let Some(value) = value.filter(|v| !v.is_null()) {
            *value = Value::from("[redacted]");
        }
    }
    let bots = json.get_mut("bots").and_then(Value::as_array_mut);
    for out in bots.into_iter().flatten() {
        redact(out.get_mut("api_url"));
        redact(out.pointer_mut("/ingress/webhook/public_url"));
    }
    let consumers = json.get_mut("consumers").and_then(Value::as_object_mut);
    for consumer in consumers.into_iter().flatten().map(|(_, c)| c) {
        let endpoints = consumer
            .pointer_mut("/push/endpoints")
            .and_then(Value::as_array_mut);
        for out in endpoints.into_iter().flatten() {
            redact(out.get_mut("url"));
        }
    }
    // A loaded config resolved every bot and consumer, in document order.
    for (bot, out) in cfg
        .bots
        .iter()
        .zip(json["bots"].as_array_mut().into_iter().flatten())
    {
        out["api_url"] = Value::from(bot.api_url.masked());
        if let Some(Ingress::Webhook(hook)) = &bot.ingress
            && let Some(url) = &hook.public_url
        {
            out["ingress"]["webhook"]["public_url"] = Value::from(url.masked());
        }
    }
    for consumer in &cfg.consumers {
        let ConsumerKind::Push(push) = &consumer.kind else {
            continue;
        };
        let endpoints = &mut json["consumers"][consumer.name.as_str()]["push"]["endpoints"];
        for (endpoint, out) in push
            .endpoints
            .iter()
            .zip(endpoints.as_array_mut().into_iter().flatten())
        {
            out["url"] = Value::from(endpoint.url.masked());
        }
    }
    json
}

fn mask_strings(value: &mut serde_json::Value, masker: &sensitive::Masker) {
    match value {
        serde_json::Value::String(text) => *text = masker.mask(text),
        serde_json::Value::Array(items) => {
            for item in items {
                mask_strings(item, masker);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                mask_strings(item, masker);
            }
        }
        _ => {}
    }
}
