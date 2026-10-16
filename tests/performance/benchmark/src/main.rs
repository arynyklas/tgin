//! `tgin-bench`: an open-loop load generator that also plays the Telegram
//! Bot API for the system under test.
//!
//! `tgin-bench serve` runs for the life of the bench container, so the fake
//! Bot API is up before tgin and the bots start polling it. `tgin-bench run`
//! asks that server for one measured run: updates go out at
//! `start + i / rps` whatever the system under test does, a bot echoes each
//! update's text back through `sendMessage`, and latency runs from the
//! update's scheduled send time to that reply.

mod bot;
mod load;
mod mock;
mod report;
mod send;
mod server;
mod track;

use std::{net::Ipv4Addr, process::ExitCode, sync::Arc};

use clap::{Parser, Subcommand, ValueEnum};
use reqwest::Client;
use tokio::net::TcpListener;

use crate::{
    report::Report,
    server::{RunOptions, Server},
};

const DEFAULT_SERVER: &str = "http://127.0.0.1:8090";

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Serve the fake Bot API and accept runs.
    Serve {
        #[arg(short, long, default_value_t = 8090)]
        port: u16,
    },
    /// Ask the server for one measured run and print its report.
    Run {
        #[command(flatten)]
        options: RunOptions,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
    },
    /// Exit 0 when `<server><path>` answers with a success status.
    Health {
        #[arg(long, default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long, default_value = "/bench/health")]
        path: String,
    },
    /// Print the CSV header matching `run --format csv-row`.
    CsvHeader,
    /// Run a fast downstream bot that echoes every update via `sendMessage`.
    Bot {
        #[command(flatten)]
        options: bot::BotOptions,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Json,
    /// One CSV row on stdout; the counters it leaves out go to stderr.
    CsvRow,
}

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Serve { port } => serve(port).await,
        Command::Run {
            options,
            format,
            server,
        } => run(&options, format, &server).await,
        Command::Health { server, path } => health(&server, &path).await,
        Command::Bot { options } => match bot::run(options).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        Command::CsvHeader => {
            println!("{}", report::csv_header());
            ExitCode::SUCCESS
        }
    }
}

async fn serve(port: u16) -> ExitCode {
    let listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot listen on port {port}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let app = server::router(Arc::new(Server::new()));
    match axum::serve(listener, app).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("server failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(options: &RunOptions, format: OutputFormat, server: &str) -> ExitCode {
    let response = match Client::new()
        .post(format!("{server}/bench/run"))
        .json(options)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            eprintln!("cannot reach the bench server at {server}: {error}");
            return ExitCode::FAILURE;
        }
    };
    if !response.status().is_success() {
        let status = response.status();
        let reason = response.text().await.unwrap_or_default();
        eprintln!("run failed ({status}): {reason}");
        return ExitCode::FAILURE;
    }
    let report: Report = match response.json().await {
        Ok(report) => report,
        Err(error) => {
            eprintln!("unreadable report: {error}");
            return ExitCode::FAILURE;
        }
    };
    match format {
        OutputFormat::Text => println!("{}", report.text()),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string(&report).expect("the report serializes")
        ),
        OutputFormat::CsvRow => {
            println!("{}", report.csv_row());
            eprintln!("{}", report.summary_line());
        }
    }
    ExitCode::SUCCESS
}

async fn health(server: &str, path: &str) -> ExitCode {
    match Client::new().get(format!("{server}{path}")).send().await {
        Ok(response) if response.status().is_success() => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
