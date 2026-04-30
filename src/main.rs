mod base;
mod config;
mod dynamic;
mod lb;
mod route;
mod tgin;
mod update;
mod utils;

mod api;

use crate::config::setup::{build_route, build_updates, load_config};
use crate::tgin::Tgin;
use crate::utils::http::build_shared_client;

use clap::{Arg, Command};

#[cfg(test)]
mod mock;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Command::new("tgin")
        .about("tgin is a telegram bot routing layer")
        .arg(
            Arg::new("file")
                .short('f')
                .long("file")
                .value_name("FILE")
                .help("Path to the configuration file")
                .default_value("tgin.ron"),
        );

    let matches = cli.get_matches();

    let config_path: &str = matches
        .get_one::<String>("file")
        .map(|s| s.as_str())
        .unwrap();

    let conf = load_config(config_path);

    // One process-wide HTTP client. `Client::clone()` is `Arc`-internal, so
    // every adapter that needs HTTP egress (LongPollUpdate, WebhookRoute,
    // RegistrationWebhookConfig, dynamically added routes via the API) shares
    // the same connection pool, DNS resolver, and TLS state.
    let http_client = build_shared_client();

    let inputs = build_updates(conf.updates, &http_client);
    let lb = build_route(conf.route, &http_client);

    let mut tgin = Tgin::new(inputs, lb, conf.dark_threads, conf.server_port);

    if let Some(api) = conf.api {
        let api = api::router::Api::new(api.base_path, http_client.clone());
        tgin.set_api(api);
    }

    if let Some(ssl) = conf.ssl {
        tgin.set_ssl(ssl.cert, ssl.key);
    }

    tgin.run();

    Ok(())
}
