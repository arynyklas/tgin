use crate::base::{RouteableComponent, UpdaterComponent};
use crate::config::error::{ConfigError, ValidationError};
use crate::config::schema::{RouteConfig, SslConfig, TginConfig, UpdateConfig};
use crate::lb::{all::AllLB, roundrobin::RoundRobinLB};
use crate::route::longpull::LongPollRoute;
use crate::route::webhook::WebhookRoute;
use crate::update::longpull::LongPollUpdate;
use crate::update::webhook::{RegistrationWebhookConfig as RuntimeRegistration, WebhookUpdate};

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use reqwest::Client;
use reqwest::Url;

/// Read, env-substitute, parse, and validate the config file. Returns every
/// problem we can detect before the runtime starts.
///
/// Errors short-circuit by stage — we cannot validate until we have a parsed
/// tree, and we cannot parse until env substitution succeeded — but _within_
/// each stage every issue is collected so the operator fixes the config in
/// one editing pass.
pub fn load_config(path: &str) -> Result<TginConfig, ConfigError> {
    let content = fs::read_to_string(path).map_err(|source| ConfigError::FileRead {
        path: PathBuf::from(path),
        source,
    })?;

    let processed = substitute_env_vars(&content).map_err(ConfigError::MissingEnvVars)?;

    let parsed: TginConfig =
        ron::from_str(&processed).map_err(|source| ConfigError::Parse { source })?;

    validate(&parsed).map_err(ConfigError::Validation)?;

    Ok(parsed)
}

/// Replace every `${VAR}` in the raw RON text with the value of the named env
/// variable. Collects _all_ missing variables and returns them together so a
/// config with N missing vars takes one restart to fix, not N.
///
/// This pre-parse pass is correct for the supported placeholder shape — the
/// regex only matches `\w+` between the braces, which cannot collide with RON
/// syntax. A principled per-string-field substitution after parse remains an
/// option if placeholder syntax ever needs to grow.
fn substitute_env_vars(input: &str) -> Result<String, Vec<String>> {
    let re = Regex::new(r"\$\{(\w+)\}").expect("static regex");

    // Preserve insertion order of the *first* occurrence of each missing var
    // so the operator's eye lands on the same name twice in a row in the
    // file, but we still report each name only once.
    let mut missing: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    let out = re.replace_all(input, |caps: &regex::Captures| {
        let var_name = &caps[1];
        match env::var(var_name) {
            Ok(val) => val,
            Err(_) => {
                if seen.insert(var_name.to_string()) {
                    missing.push(var_name.to_string());
                }
                // Keep a placeholder in the output so the post-substitution
                // string is still a syntactically plausible RON document.
                // Parse will fail anyway — but only after we've reported the
                // missing-var list, which is the actionable signal.
                String::new()
            }
        }
    });

    if !missing.is_empty() {
        return Err(missing);
    }

    Ok(out.into_owned())
}

/// Walk the parsed config and report every semantic issue we can detect
/// before the runtime starts. Collects every error so a multi-issue config
/// surfaces all of them in one pass.
pub fn validate(cfg: &TginConfig) -> Result<(), Vec<ValidationError>> {
    let mut errs: Vec<ValidationError> = Vec::new();

    if cfg.dark_threads == 0 {
        errs.push(ValidationError::DarkThreadsZero);
    }

    if let Some(ssl) = &cfg.ssl {
        check_ssl(ssl, &mut errs);
    }

    // Every HTTP path that lands on the shared axum router. Includes both
    // ingress (`WebhookUpdate.path`) and the long-poll _routes_ that
    // `LongPollRoute` mounts as Telegram-shaped `getUpdates` endpoints.
    // The first registration wins in axum's `Router`; collisions silently
    // shadow at request time.
    let mut paths: HashMap<String, &'static str> = HashMap::new();

    for (idx, upd) in cfg.updates.iter().enumerate() {
        match upd {
            UpdateConfig::WebhookUpdate { path, registration, .. } => {
                let location = format!("updates[{idx}].WebhookUpdate");
                if let Some(prev) = paths.insert(path.clone(), "WebhookUpdate") {
                    errs.push(ValidationError::DuplicatePath {
                        path: path.clone(),
                        first: prev,
                        second: "WebhookUpdate",
                    });
                }
                if let Some(reg) = registration {
                    check_url(
                        &reg.public_ip,
                        &format!("{location}.registration"),
                        &mut errs,
                        UrlField::PublicIp,
                    );
                    if let Some(set_url) = &reg.set_webhook_url {
                        check_url(
                            set_url,
                            &format!("{location}.registration"),
                            &mut errs,
                            UrlField::SetWebhookUrl,
                        );
                    }
                }
            }
            UpdateConfig::LongPollUpdate { .. } => {}
        }
    }

    walk_route(&cfg.route, "route", &mut errs, &mut paths);

    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

enum UrlField {
    Webhook,
    PublicIp,
    SetWebhookUrl,
}

fn check_url(raw: &str, location: &str, errs: &mut Vec<ValidationError>, field: UrlField) {
    let parsed = Url::parse(raw);
    let result = match parsed {
        Ok(u) if u.scheme() == "http" || u.scheme() == "https" => Ok(()),
        Ok(u) => Err(format!("unsupported scheme {:?}", u.scheme())),
        Err(e) => Err(e.to_string()),
    };
    if let Err(reason) = result {
        let url = raw.to_string();
        let location = location.to_string();
        errs.push(match field {
            UrlField::Webhook => ValidationError::InvalidWebhookUrl { url, location, reason },
            UrlField::PublicIp => ValidationError::InvalidPublicIp { url, location, reason },
            UrlField::SetWebhookUrl => {
                ValidationError::InvalidSetWebhookUrl { url, location, reason }
            }
        });
    }
}

fn check_ssl(ssl: &SslConfig, errs: &mut Vec<ValidationError>) {
    for (field, raw) in [("cert", &ssl.cert), ("key", &ssl.key)] {
        let path = Path::new(raw);
        match fs::metadata(path) {
            Ok(_) => {}
            Err(e) => errs.push(ValidationError::SslFileUnreadable {
                field,
                path: PathBuf::from(raw),
                source_kind: e.kind(),
                source_msg: e.to_string(),
            }),
        }
    }
}

fn walk_route(
    cfg: &RouteConfig,
    location: &str,
    errs: &mut Vec<ValidationError>,
    paths: &mut HashMap<String, &'static str>,
) {
    match cfg {
        RouteConfig::LongPollRoute { path, .. } => {
            if let Some(prev) = paths.insert(path.clone(), "LongPollRoute") {
                errs.push(ValidationError::DuplicatePath {
                    path: path.clone(),
                    first: prev,
                    second: "LongPollRoute",
                });
            }
        }
        RouteConfig::WebhookRoute { url, .. } => {
            check_url(url, location, errs, UrlField::Webhook);
        }
        RouteConfig::RoundRobinLB { routes } => {
            if routes.is_empty() {
                errs.push(ValidationError::EmptyLoadBalancer {
                    kind: "RoundRobinLB",
                    location: location.to_string(),
                });
            }
            for (i, child) in routes.iter().enumerate() {
                walk_route(child, &format!("{location}.routes[{i}]"), errs, paths);
            }
        }
        RouteConfig::AllLB { routes } => {
            if routes.is_empty() {
                errs.push(ValidationError::EmptyLoadBalancer {
                    kind: "AllLB",
                    location: location.to_string(),
                });
            }
            for (i, child) in routes.iter().enumerate() {
                walk_route(child, &format!("{location}.routes[{i}]"), errs, paths);
            }
        }
    }
}

/// Build the runtime ingress adapters. The `client` is the process-wide
/// shared `reqwest::Client`; every adapter that issues outbound HTTP gets a
/// clone (cheap — `Client` is `Arc`-internal) so they share the connection
/// pool, DNS resolver, and TLS state.
pub fn build_updates(
    configs: Vec<UpdateConfig>,
    client: &Client,
    health_counter: &Arc<AtomicUsize>,
) -> Vec<Box<dyn UpdaterComponent>> {
    let mut result: Vec<Box<dyn UpdaterComponent>> = Vec::new();

    for cfg in configs {
        match cfg {
            UpdateConfig::LongPollUpdate {
                token,
                url,
                default_timeout_sleep,
                error_timeout_sleep,
                long_poll_timeout,
                long_poll_limit,
            } => {
                let mut up = LongPollUpdate::new(token, client.clone());
                if let Some(u) = url {
                    up.set_url(u);
                }
                up.set_timeouts(default_timeout_sleep, error_timeout_sleep);
                up.set_long_poll_params(long_poll_timeout, long_poll_limit);
                up.set_health_counter(health_counter.clone());
                result.push(Box::new(up));
            }
            UpdateConfig::WebhookUpdate {
                path,
                registration,
                secret_token,
            } => {
                let mut up = WebhookUpdate::new(path);
                if let Some(reg) = registration {
                    let mut runtime =
                        RuntimeRegistration::new(reg.token, reg.public_ip, client.clone());
                    if let Some(url) = reg.set_webhook_url {
                        runtime.set_webhook_url(url);
                    }
                    up.set_registration(runtime);
                }
                if let Some(token) = secret_token {
                    up.set_secret_token(token);
                }
                result.push(Box::new(up));
            }
        }
    }
    result
}

pub fn build_route(cfg: RouteConfig, client: &Client) -> Arc<dyn RouteableComponent> {
    match cfg {
        RouteConfig::LongPollRoute { path, max_buffered_updates } => {
            let mut route = LongPollRoute::new(path);
            route.set_max_buffered_updates(max_buffered_updates);
            Arc::new(route)
        }
        RouteConfig::WebhookRoute {
            url,
            request_timeout_ms,
        } => Arc::new(WebhookRoute::new(
            url,
            client.clone(),
            Duration::from_millis(request_timeout_ms),
        )),

        RouteConfig::RoundRobinLB { routes } => {
            let built_routes: Vec<Arc<dyn RouteableComponent>> = routes
                .into_iter()
                .map(|r| build_route(r, client))
                .collect();

            Arc::new(RoundRobinLB::new(built_routes))
        }

        RouteConfig::AllLB { routes } => {
            let built_routes: Vec<Arc<dyn RouteableComponent>> = routes
                .into_iter()
                .map(|r| build_route(r, client))
                .collect();

            Arc::new(AllLB::new(built_routes))
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{ApiConfig, RegistrationWebhookConfig as SchemaRegistration};
    use crate::utils::http::build_shared_client;

    /// `build_updates` MUST forward the parsed `registration` block into the runtime
    /// `WebhookUpdate`. We can't poke at the private `registration` field from here,
    /// so we observe it through the public `Printable` impl: the boot banner contains
    /// `REGISTRATED ON <redacted-url>` iff `registration` ended up `Some`.
    #[tokio::test]
    async fn test_build_updates_wires_webhook_registration() {
        let cfg = vec![UpdateConfig::WebhookUpdate {
            path: "/bot/pull".to_string(),
            registration: Some(SchemaRegistration {
                public_ip: "https://example.invalid".to_string(),
                set_webhook_url: None,
                token: "12345678:ABCDEFGHIJKLMNOPQRSTUVWXYZ012345".to_string(),
            }),
            secret_token: None,
        }];

        let client = build_shared_client();
        let health = Arc::new(AtomicUsize::new(0));
        let built = build_updates(cfg, &client, &health);
        assert_eq!(built.len(), 1);

        let banner = built[0].print().await;
        assert!(
            banner.contains("REGISTRATED ON"),
            "registration block was discarded; banner was {banner:?}"
        );
        // The token shape matches TELEGRAM_TOKEN_REGEX, so the banner must not leak
        // the secret half of the bot token.
        assert!(
            !banner.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"),
            "token was not redacted in banner: {banner:?}"
        );
    }

    /// Mirror case: when `registration` is omitted, the runtime updater MUST NOT
    /// claim it has a registration. Guards against a future regression that defaults
    /// the field to `Some(...)`.
    #[tokio::test]
    async fn test_build_updates_no_registration_leaves_webhook_passive() {
        let cfg = vec![UpdateConfig::WebhookUpdate {
            path: "/bot/pull".to_string(),
            registration: None,
            secret_token: None,
        }];

        let client = build_shared_client();
        let health = Arc::new(AtomicUsize::new(0));
        let built = build_updates(cfg, &client, &health);
        let banner = built[0].print().await;
        assert!(
            !banner.contains("REGISTRATED ON"),
            "unexpected registration banner: {banner:?}"
        );
    }

    // ---------- substitute_env_vars ----------

    /// Collects every missing var in one pass, deduplicated, in source order.
    /// Operator workflow: read the message, export them all, restart once.
    #[test]
    fn test_substitute_env_vars_collects_all_missing() {
        // Pick names that are very unlikely to exist in any test environment.
        let key_a = "TGIN_TEST_MISSING_AAA_91237";
        let key_b = "TGIN_TEST_MISSING_BBB_91237";
        // Sanity: ensure they really are unset in this process.
        env::remove_var(key_a);
        env::remove_var(key_b);

        let input = format!("foo=${{{key_a}}} bar=${{{key_b}}} baz=${{{key_a}}}");
        let err = substitute_env_vars(&input).expect_err("two distinct missing vars");

        assert_eq!(err, vec![key_a.to_string(), key_b.to_string()]);
    }

    /// Set vars round-trip through substitution untouched. Guards against a regex
    /// change that accidentally drops the captured value.
    #[test]
    fn test_substitute_env_vars_substitutes_set_values() {
        let key = "TGIN_TEST_SET_VALUE_91237";
        env::set_var(key, "hello");
        let out = substitute_env_vars(&format!("v=${{{key}}}")).expect("set var resolves");
        env::remove_var(key);
        assert_eq!(out, "v=hello");
    }

    // ---------- validate ----------

    /// Build a `WebhookRoute` for tests. The validator does not care about
    /// `request_timeout_ms`, so every test reuses the schema default to keep
    /// fixtures focused on the field under test.
    fn wh_route(url: &str) -> RouteConfig {
        RouteConfig::WebhookRoute {
            url: url.to_string(),
            request_timeout_ms: crate::route::webhook::DEFAULT_REQUEST_TIMEOUT.as_millis() as u64,
        }
    }

    fn base_config(route: RouteConfig) -> TginConfig {
        TginConfig {
            dark_threads: 4,
            server_port: Some(3000),
            ssl: None,
            updates: vec![],
            route,
            api: None,
        }
    }

    #[test]
    fn test_validate_ok_minimal() {
        let cfg = base_config(wh_route("http://127.0.0.1:8080/bot"));
        validate(&cfg).expect("minimal config validates");
    }

    /// Empty load balancers swallow updates silently — the very symptom this
    /// validator exists to prevent. Checks both LB shapes.
    #[test]
    fn test_validate_rejects_empty_load_balancers() {
        let cfg = base_config(RouteConfig::RoundRobinLB { routes: vec![] });
        let errs = validate(&cfg).expect_err("empty RoundRobinLB rejected");
        assert!(matches!(
            errs.as_slice(),
            [ValidationError::EmptyLoadBalancer { kind: "RoundRobinLB", .. }]
        ));

        let cfg = base_config(RouteConfig::AllLB { routes: vec![] });
        let errs = validate(&cfg).expect_err("empty AllLB rejected");
        assert!(matches!(
            errs.as_slice(),
            [ValidationError::EmptyLoadBalancer { kind: "AllLB", .. }]
        ));
    }

    /// Two LongPollRoute siblings with the same `path` would clobber each other
    /// in the dynamic registry. Detect at config time.
    #[test]
    fn test_validate_rejects_duplicate_longpoll_paths() {
        let cfg = base_config(RouteConfig::RoundRobinLB {
            routes: vec![
                RouteConfig::LongPollRoute { path: "/bot/up".to_string(), max_buffered_updates: crate::route::longpull::DEFAULT_MAX_BUFFERED_UPDATES },
                RouteConfig::LongPollRoute { path: "/bot/up".to_string(), max_buffered_updates: crate::route::longpull::DEFAULT_MAX_BUFFERED_UPDATES },
            ],
        });
        let errs = validate(&cfg).expect_err("duplicate path rejected");
        assert!(errs
            .iter()
            .any(|e| matches!(e, ValidationError::DuplicatePath { path, .. } if path == "/bot/up")));
    }

    /// A `WebhookUpdate` (ingress) and a `LongPollRoute` (egress) cannot share
    /// a path on the shared axum router.
    #[test]
    fn test_validate_rejects_path_collision_across_ingress_and_egress() {
        let cfg = TginConfig {
            dark_threads: 4,
            server_port: Some(3000),
            ssl: None,
            updates: vec![UpdateConfig::WebhookUpdate {
                path: "/shared".to_string(),
                registration: None,
                secret_token: None,
            }],
            route: RouteConfig::LongPollRoute { path: "/shared".to_string(), max_buffered_updates: crate::route::longpull::DEFAULT_MAX_BUFFERED_UPDATES },
            api: None,
        };
        let errs = validate(&cfg).expect_err("ingress/egress path collision rejected");
        assert!(errs.iter().any(|e| matches!(
            e,
            ValidationError::DuplicatePath {
                path,
                first: "WebhookUpdate",
                second: "LongPollRoute",
            } if path == "/shared"
        )));
    }

    /// Catch malformed webhook URLs at startup instead of letting them surface
    /// as silently-swallowed per-update HTTP errors.
    #[test]
    fn test_validate_rejects_invalid_webhook_url() {
        let cfg = base_config(wh_route("not-a-url"));
        let errs = validate(&cfg).expect_err("non-URL rejected");
        assert!(matches!(
            errs.as_slice(),
            [ValidationError::InvalidWebhookUrl { url, .. }] if url == "not-a-url"
        ));

        let cfg = base_config(wh_route("ftp://example.com"));
        let errs = validate(&cfg).expect_err("non-http(s) scheme rejected");
        assert!(matches!(
            errs.as_slice(),
            [ValidationError::InvalidWebhookUrl { reason, .. }]
                if reason.contains("ftp")
        ));
    }

    /// `dark_threads: 0` panics deep inside `tokio::runtime::Builder` with no
    /// useful frame. Catch at config time.
    #[test]
    fn test_validate_rejects_dark_threads_zero() {
        let mut cfg = base_config(wh_route("http://127.0.0.1:8080/bot"));
        cfg.dark_threads = 0;
        let errs = validate(&cfg).expect_err("dark_threads=0 rejected");
        assert!(errs.iter().any(|e| matches!(e, ValidationError::DarkThreadsZero)));
    }

    /// Multi-issue config surfaces _every_ issue, not just the first one. The
    /// whole point of the collecting validator.
    #[test]
    fn test_validate_collects_multiple_errors() {
        let mut cfg = base_config(RouteConfig::RoundRobinLB { routes: vec![] });
        cfg.dark_threads = 0;
        let errs = validate(&cfg).expect_err("multi-error config rejected");
        // dark_threads=0 + empty LB = at least 2 distinct errors
        assert!(errs.len() >= 2, "expected >=2 errors, got {errs:?}");
        assert!(errs.iter().any(|e| matches!(e, ValidationError::DarkThreadsZero)));
        assert!(errs
            .iter()
            .any(|e| matches!(e, ValidationError::EmptyLoadBalancer { .. })));
    }

    /// SSL files that don't exist must surface at config-load time, not
    /// inside the TLS-bind path where the failure is far from the field.
    #[test]
    fn test_validate_rejects_missing_ssl_files() {
        let mut cfg = base_config(wh_route("http://127.0.0.1:8080/bot"));
        cfg.ssl = Some(SslConfig {
            cert: "/this/path/does/not/exist/cert.pem".to_string(),
            key: "/this/path/does/not/exist/key.pem".to_string(),
        });
        let errs = validate(&cfg).expect_err("missing ssl files rejected");
        // Both cert and key are missing -> 2 errors.
        let n = errs
            .iter()
            .filter(|e| matches!(e, ValidationError::SslFileUnreadable { .. }))
            .count();
        assert_eq!(n, 2, "expected one error per ssl file, got {errs:?}");
    }

    /// Registration URLs are validated too — a typo in `public_ip` would
    /// otherwise surface only when `setWebhook` runs against Telegram.
    #[test]
    fn test_validate_rejects_invalid_public_ip() {
        let cfg = TginConfig {
            dark_threads: 4,
            server_port: Some(3000),
            ssl: None,
            updates: vec![UpdateConfig::WebhookUpdate {
                path: "/bot".to_string(),
                registration: Some(SchemaRegistration {
                    public_ip: "not a url".to_string(),
                    set_webhook_url: None,
                    token: "12345678:ABCDEFGHIJKLMNOPQRSTUVWXYZ012345".to_string(),
                }),
                secret_token: None,
            }],
            route: wh_route("http://127.0.0.1:8080/bot"),
            api: None,
        };
        let errs = validate(&cfg).expect_err("bad public_ip rejected");
        assert!(errs
            .iter()
            .any(|e| matches!(e, ValidationError::InvalidPublicIp { .. })));
    }

    /// API present + valid config: regression guard that ApiConfig doesn't
    /// trip validation.
    #[test]
    fn test_validate_ok_with_api() {
        let mut cfg = base_config(wh_route("http://127.0.0.1:8080/bot"));
        cfg.api = Some(ApiConfig { base_path: "/api".to_string() });
        validate(&cfg).expect("api config validates");
    }
}
