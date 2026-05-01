use crate::base::{RouteableComponent, UpdaterComponent};
use crate::config::schema::{RouteConfig, TginConfig, UpdateConfig};
use crate::lb::{all::AllLB, roundrobin::RoundRobinLB};
use crate::route::longpull::LongPollRoute;
use crate::route::webhook::WebhookRoute;
use crate::update::longpull::LongPollUpdate;
use crate::update::webhook::{RegistrationWebhookConfig as RuntimeRegistration, WebhookUpdate};

use std::fs;
use std::sync::Arc;

use regex::Regex;
use reqwest::Client;
use std::env;

pub fn load_config(path: &str) -> TginConfig {
    let content = fs::read_to_string(path).expect("Failed to read config file");
    let processed_content = substitute_env_vars(&content);

    ron::from_str(&processed_content).expect("Failed to parse RON config")
}

fn substitute_env_vars(input: &str) -> String {
    let re = Regex::new(r"\$\{(\w+)\}").unwrap();

    re.replace_all(input, |caps: &regex::Captures| {
        let var_name = &caps[1];

        match env::var(var_name) {
            Ok(val) => val,
            Err(_) => {
                panic!("Environment variable '${}' is not set", var_name);
            }
        }
    })
    .to_string()
}

/// Build the runtime ingress adapters. The `client` is the process-wide
/// shared `reqwest::Client`; every adapter that issues outbound HTTP gets a
/// clone (cheap — `Client` is `Arc`-internal) so they share the connection
/// pool, DNS resolver, and TLS state.
pub fn build_updates(configs: Vec<UpdateConfig>, client: &Client) -> Vec<Box<dyn UpdaterComponent>> {
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
        RouteConfig::LongPollRoute { path } => Arc::new(LongPollRoute::new(path)),
        RouteConfig::WebhookRoute { url } => Arc::new(WebhookRoute::new(url, client.clone())),

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
    use crate::config::schema::RegistrationWebhookConfig as SchemaRegistration;
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
        let built = build_updates(cfg, &client);
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
        let built = build_updates(cfg, &client);
        let banner = built[0].print().await;
        assert!(
            !banner.contains("REGISTRATED ON"),
            "unexpected registration banner: {banner:?}"
        );
    }
}
