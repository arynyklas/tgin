mod base;
mod config;
mod dynamic;
mod lb;
mod observe;
mod route;
mod tgin;
mod update;
mod utils;

mod api;

use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::config::schema::UpdateConfig;
use crate::config::setup::{build_route, build_updates, effective_auth_token, load_config};
use crate::tgin::Tgin;
use crate::utils::http::build_shared_client;

use clap::{Arg, Command};

#[cfg(test)]
mod mock;

/// Top-level entry. Operators see a single rendered error line (or block) on
/// stderr and a non-zero exit code — never a Rust stack trace from a
/// startup-time panic. Stack traces are not for operators.
fn main() -> ExitCode {
    match run() {
        Ok(RunOutcome::Clean) => ExitCode::SUCCESS,
        Ok(RunOutcome::PermanentUpdaterFailure(n)) => {
            // Distinct from the generic exit-1 so an orchestrator can
            // tell "tgin failed to start" from "a configured bot's
            // token / URL is wrong". Both are operator-fixable, but only
            // the latter has been running long enough to handle traffic.
            tracing::error!(
                updaters = n,
                "updater(s) terminated with permanent failure; check earlier long-poll permanent-failure logs"
            );
            ExitCode::from(2)
        }
        Err(err) => {
            eprintln!("tgin: {err}");
            ExitCode::from(1)
        }
    }
}

/// Outcome of a successful boot, distinguished so `main` picks the right
/// exit code without parsing error strings.
enum RunOutcome {
    /// All updaters terminated cleanly (channel closed, normal shutdown).
    Clean,
    /// At least one updater hit a 401 / 403 / 404 and stopped retrying.
    PermanentUpdaterFailure(usize),
}

/// The command-line interface the binary parses. Shared with tests so they
/// exercise the exact `Command` `run()` uses.
fn cli() -> Command {
    Command::new("tgin")
        .version(env!("CARGO_PKG_VERSION"))
        .about("tgin is a telegram bot routing layer")
        .arg(
            Arg::new("file")
                .short('f')
                .long("file")
                .value_name("FILE")
                .help("Path to the configuration file")
                .default_value("tgin.ron"),
        )
}

fn run() -> Result<RunOutcome, Box<dyn std::error::Error>> {
    let matches = cli().get_matches();

    let config_path: &str = matches
        .get_one::<String>("file")
        .map(|s| s.as_str())
        .expect("clap default guarantees a value");

    let conf = load_config(config_path)?;

    crate::observe::init(&conf.log_level, conf.log_format);

    // An unsigned webhook ingress accepts any POST as a Telegram update.
    // Allowed (a reverse proxy may guard it), but never silently.
    for upd in &conf.updates {
        if let UpdateConfig::WebhookUpdate {
            path,
            secret_token: None,
            ..
        } = upd
        {
            tracing::warn!(
                "WebhookUpdate has no secret_token: anyone who can reach {path} can inject \
                 updates; set secret_token"
            );
        }
    }

    // One process-wide HTTP client. `Client::clone()` is `Arc`-internal, so
    // every adapter that needs HTTP egress (LongPollUpdate, WebhookRoute,
    // RegistrationWebhookConfig, dynamically added routes via the API) shares
    // the same connection pool, DNS resolver, and TLS state.
    let http_client = build_shared_client();

    // Process-wide permanent-failure counter shared by every long-poll
    // updater. A non-zero value at shutdown means at least one updater
    // hit a 401 / 403 / 404 (revoked token, malformed URL, ...) and
    // stopped retrying. We surface that as a distinct non-zero exit
    // code so an orchestrator (systemd, Docker, k8s) can alert/restart
    // instead of treating it as a clean shutdown.
    let health_counter = Arc::new(AtomicUsize::new(0));

    let inputs = build_updates(conf.updates, &http_client, &health_counter);
    let lb = build_route(conf.route, &http_client);

    let mut tgin = Tgin::new(inputs, lb, conf.dark_threads, conf.server_port);

    let (auth_token, blank_token) = effective_auth_token(conf.auth_token);
    if blank_token {
        tracing::warn!(
            "auth_token is set but blank; ignoring it — /status and /metrics are \
             UNAUTHENTICATED"
        );
    }
    tgin.set_auth_token(auth_token.clone());

    if let Some(api) = conf.api {
        let api = api::router::Api::new(api.base_path, http_client.clone(), auth_token.clone());
        tgin.set_api(api);
    }

    if let Some(ssl) = conf.ssl {
        tgin.set_ssl(ssl.cert, ssl.key);
    }

    tgin.run()?;

    // `tgin.run()` returns once every producer has terminated and the
    // worker pool drained. If any updater stopped because of a permanent
    // failure, propagate that distinction to `main` so the exit code
    // tells the truth.
    let permanent = health_counter.load(Ordering::Relaxed);
    if permanent > 0 {
        Ok(RunOutcome::PermanentUpdaterFailure(permanent))
    } else {
        Ok(RunOutcome::Clean)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_version_flag() {
        let err = cli()
            .try_get_matches_from(["tgin", "--version"])
            .expect_err("--version short-circuits parsing");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        let rendered = err.to_string();
        assert!(
            rendered.contains(env!("CARGO_PKG_VERSION")),
            "version output missing crate version: {rendered:?}"
        );
    }
}
