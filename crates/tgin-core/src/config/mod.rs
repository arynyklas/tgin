//! The configuration file, version 1: its serde model ([`model`]),
//! validation, resolution into a [`Config`], and JSON Schema.

mod acl;
mod defaults;
pub mod model;
mod resolve;
mod schema;
mod sensitive;
mod validate;

use std::path::Path;
use std::time::Duration;

use tgin_runtime::config::{ConfigError, Env, parse_yaml};

pub use self::acl::{
    ACCOUNT_CHANGING, ACCOUNT_READ_ONLY, GLOB_NAMESPACES, IRREVERSIBLE, MethodAcl, MethodGlob,
    account_changing, exact_only, glob_namespace,
};
pub use self::model::RawConfig;
pub use self::resolve::{
    Account, Admin, Balance, Bot, Config, Consumer, ConsumerKind, Endpoint, Files, Gateway,
    Ingress, IpAllowlist, Log, LogFormat, LogLevel, LongPoll, Pull, Push, Rate, RateLimit,
    RateMode, Reload, Render, Retry, Rpc, RpcClient, Runtime, Server, Tls, Upstream, UpstreamKind,
    Webhook, WebhookReply, WebhookRules, WebhookSecret,
};
pub use self::schema::schema;
pub use self::sensitive::{Sensitive, SensitivePath, SensitiveRoute, SensitiveUrl};

/// A push endpoint's `timeout` when the config gives none; a runtime
/// endpoint from `setWebhook` always has it.
pub const DEFAULT_ENDPOINT_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether `token` can be sent as a bot token: non-empty, and only
/// `[A-Za-z0-9:_-]`. The loader rejects any other token, and the Bot API
/// client refuses it, by this one rule.
pub fn valid_bot_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-'))
}

/// Whether the Bot API gateway can serve `token`: `<digits>:<secret>`,
/// `^[0-9]+:[A-Za-z0-9_-]{1,128}$`, as Telegram issues them. The loader
/// warns about any other bot token, and the gateway path parser applies
/// this one rule to the token segment after percent-decoding it.
pub fn gateway_token(token: &str) -> bool {
    let Some((id, secret)) = token.split_once(':') else {
        return false;
    };
    !id.is_empty()
        && id.bytes().all(|b| b.is_ascii_digit())
        && (1..=128).contains(&secret.len())
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// Whether `name` is a valid bot or consumer name: 1 to 63 lowercase
/// letters, digits and `-`, starting with a letter or digit. The loader
/// checks every name, and the gateway path parser every `/c/<name>`
/// segment, by this one rule.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let allowed = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    bytes.len() <= 63
        && bytes.first().is_some_and(allowed)
        && bytes.iter().all(|c| allowed(c) || *c == b'-')
}

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
/// - URL fields (`api_url`, an upstream's `url` and `files.url`,
///   `public_url`, endpoint `url`) show only the resolved URL's origin,
///   `scheme://host[:port]`, followed by `/…` when a path, query or
///   fragment was elided; their `${…}` references are not shown. A bot
///   with neither `api_url` nor `upstream` shows the default `api_url`.
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
    let upstreams = json.get_mut("upstreams").and_then(Value::as_object_mut);
    for out in upstreams.into_iter().flatten().map(|(_, u)| u) {
        redact(out.get_mut("url"));
        redact(out.pointer_mut("/files/url"));
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
    for upstream in &cfg.upstreams {
        let Some(out) = json
            .get_mut("upstreams")
            .and_then(|upstreams| upstreams.get_mut(&*upstream.name))
        else {
            continue;
        };
        out["url"] = Value::from(upstream.url.masked());
        if let Some(Files::Url(url)) = &upstream.files {
            out["files"]["url"] = Value::from(url.masked());
        }
    }
    // A loaded config resolved every bot and consumer, in document order.
    for (bot, out) in cfg
        .bots
        .iter()
        .zip(json["bots"].as_array_mut().into_iter().flatten())
    {
        if *bot.upstream.name == *Upstream::SHORTHAND {
            out["api_url"] = Value::from(bot.upstream.url.masked());
        }
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
