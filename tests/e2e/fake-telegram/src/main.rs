//! `fake-telegram`: serve the fake Bot API, or check that one is healthy.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode, time::Duration};

use clap::{Parser, Subcommand};
use tgin_fake_telegram::{Options, Server};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(version, about = "A fake Telegram Bot API server for end-to-end tests")]
struct Cli {
    /// Address to listen on.
    #[arg(long, env = "FAKE_TELEGRAM_LISTEN", default_value = "0.0.0.0:8081")]
    listen: SocketAddr,
    /// Behave like `telegram-bot-api --local` (needs `--files-dir`, which
    /// must be writable): uploads go under `{files-dir}/{token}/` ('~' for
    /// ':' where the filesystem refuses ':'), `getFile` answers absolute
    /// paths, `/file…` is 404, `file:` references are accepted, and `logOut`
    /// drops pending updates and the webhook.
    #[arg(long, env = "FAKE_TELEGRAM_LOCAL_MODE")]
    local_mode: bool,
    /// The files directory (the server's `--dir`); used in local mode.
    #[arg(long, env = "FAKE_TELEGRAM_FILES_DIR")]
    files_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Exit 0 if the fake on the `--listen` port answers `/fake/health`
    /// (for container health checks).
    Health,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    match cli.command {
        Some(Command::Health) => health(cli.listen.port()).await,
        None => {
            let options = Options {
                local_mode: cli.local_mode,
                files_dir: cli.files_dir,
            };
            serve(cli.listen, options).await
        }
    }
}

async fn serve(listen: SocketAddr, options: Options) -> ExitCode {
    let listener = match TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%listen, %error, "cannot listen");
            return ExitCode::FAILURE;
        }
    };
    let server = match Server::start_with(listener, options) {
        Ok(server) => server,
        Err(error) => {
            tracing::error!(%error, "cannot start");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(addr = %server.addr(), "fake telegram listening");
    shutdown_signal().await;
    tracing::info!("shutting down");
    match server.stop().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "server failed");
            ExitCode::FAILURE
        }
    }
}

/// Ctrl-C, or SIGTERM from `docker stop` on Unix.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn health(port: u16) -> ExitCode {
    let url = format!("http://127.0.0.1:{port}/fake/health");
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()
    {
        Ok(client) => client,
        Err(_) => return ExitCode::FAILURE,
    };
    match client.get(url).send().await {
        Ok(response) if response.status().is_success() => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
