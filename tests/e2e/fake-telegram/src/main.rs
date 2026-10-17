//! `fake-telegram`: serve the fake Bot API, or check that one is healthy.

use std::{net::SocketAddr, process::ExitCode, time::Duration};

use clap::{Parser, Subcommand};
use tgin_fake_telegram::Server;
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(version, about = "A fake Telegram Bot API server for end-to-end tests")]
struct Cli {
    /// Address to listen on.
    #[arg(long, env = "FAKE_TELEGRAM_LISTEN", default_value = "0.0.0.0:8081")]
    listen: SocketAddr,
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
        None => serve(cli.listen).await,
    }
}

async fn serve(listen: SocketAddr) -> ExitCode {
    let listener = match TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%listen, %error, "cannot listen");
            return ExitCode::FAILURE;
        }
    };
    let server = match Server::start(listener) {
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
