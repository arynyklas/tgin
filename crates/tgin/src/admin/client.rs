//! `tgin admin`: the admin API from the command line.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use tgin_runtime::http::{self, ClientOptions, describe_error};
use url::Url;

use super::{ConsumerStatus, Status};
use crate::exit;

/// The admin API `tgin admin` talks to without `--url`: the default of
/// `admin.listen`.
pub const DEFAULT_URL: &str = "http://127.0.0.1:9090";
/// How long `health` waits for an answer.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long other commands wait; a reload may wait on Telegram.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// One `tgin admin` command.
#[derive(Debug, PartialEq, Eq)]
pub struct Command {
    pub url: Url,
    /// A file holding the admin token.
    pub token_file: Option<PathBuf>,
    /// Print the API's JSON as it is instead of tables.
    pub json: bool,
    pub action: Action,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Status,
    Reload,
    Consumers,
    Pause(String),
    Resume(String),
    Drain(String),
    Purge(String),
    Health,
}

/// Parses `--url`: an `http` or `https` URL.
///
/// # Errors
///
/// Not a URL, or another scheme.
pub fn parse_url(text: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|err| format!("not a URL: {err}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("the admin URL must be http or https".to_owned());
    }
    Ok(url)
}

/// Runs `command` and returns the process exit code.
pub fn run(command: &Command) -> u8 {
    // `health` reads an unauthenticated probe: a token file it cannot read
    // (a secret the healthcheck's user may not see) must not fail it.
    let token_file = command
        .token_file
        .as_ref()
        .filter(|_| command.action != Action::Health);
    let token = match token_file {
        Some(path) => match read_token(path) {
            Ok(token) => Some(token),
            Err(err) => {
                eprintln!(
                    "tgin: cannot read the admin token file {}: {err}",
                    path.display()
                );
                return exit::FAILURE;
            }
        },
        None => None,
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("tgin: starting the async runtime failed: {err}");
            return exit::FAILURE;
        }
    };
    runtime.block_on(execute(command, token.as_deref()))
}

/// The token in `path`, without its line ending.
fn read_token(path: &Path) -> std::io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    Ok(text.trim_end_matches(['\r', '\n']).to_owned())
}

/// An answer of the admin API.
struct Answer {
    status: StatusCode,
    body: String,
}

/// The admin client's options: `timeout` for connecting and for the whole
/// request, and no proxy ever (R34): the bearer token would reach the
/// proxy in cleartext, and the health check would depend on it.
fn client_options(timeout: Duration) -> ClientOptions {
    ClientOptions {
        connect_timeout: timeout,
        request_timeout: timeout,
        no_proxy: true,
        ..ClientOptions::default()
    }
}

async fn execute(command: &Command, token: Option<&str>) -> u8 {
    let timeout = if command.action == Action::Health {
        HEALTH_TIMEOUT
    } else {
        REQUEST_TIMEOUT
    };
    let client = http::client(&client_options(timeout));
    let (method, segments): (Method, Vec<&str>) = match &command.action {
        Action::Status | Action::Consumers => (Method::GET, vec!["v1", "status"]),
        Action::Reload => (Method::POST, vec!["v1", "reload"]),
        Action::Pause(name) => (Method::POST, vec!["v1", "consumers", name, "pause"]),
        Action::Resume(name) => (Method::POST, vec!["v1", "consumers", name, "resume"]),
        Action::Drain(name) => (Method::POST, vec!["v1", "consumers", name, "drain"]),
        Action::Purge(name) => (Method::POST, vec!["v1", "consumers", name, "purge"]),
        Action::Health => (Method::GET, vec!["readyz"]),
    };
    let mut request = client.request(method, endpoint(&command.url, &segments));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let sent = tokio::time::timeout(timeout, async {
        let response = request.send().await?;
        let status = response.status();
        let body = response.text().await?;
        Ok::<_, reqwest::Error>(Answer { status, body })
    })
    .await;
    let origin = command.url.origin().ascii_serialization();
    let answer = match sent {
        Ok(Ok(answer)) => answer,
        Ok(Err(err)) => {
            eprintln!(
                "tgin: the admin API at {origin} did not answer: {}",
                describe_error(&err)
            );
            return exit::FAILURE;
        }
        Err(_elapsed) => {
            eprintln!(
                "tgin: the admin API at {origin} did not answer within {}s",
                timeout.as_secs()
            );
            return exit::FAILURE;
        }
    };

    if command.action == Action::Health {
        return health(command, &answer);
    }
    if !answer.status.is_success() {
        eprintln!("tgin: {}: {}", answer.status, failure(&answer.body));
        return exit::FAILURE;
    }
    if command.json {
        if !answer.body.is_empty() {
            println!("{}", pretty(&answer.body, &command.action));
        }
        return exit::OK;
    }
    match render(&command.action, &answer.body) {
        Ok(text) => {
            print!("{text}");
            exit::OK
        }
        Err(err) => {
            eprintln!("tgin: the admin API sent an unexpected answer: {err}");
            exit::FAILURE
        }
    }
}

/// `base` with `segments` appended to its path, each escaped as one segment.
fn endpoint(base: &Url, segments: &[&str]) -> Url {
    let mut url = base.clone();
    url.set_query(None);
    url.set_fragment(None);
    // An http(s) URL always has a path to extend.
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().extend(segments);
    }
    url
}

/// `health`: 0 when ready, else 1.
fn health(command: &Command, answer: &Answer) -> u8 {
    if command.json {
        println!("{}", answer.body);
    } else if answer.status == StatusCode::OK {
        println!("ready");
    } else {
        let phase = serde_json::from_str::<Value>(&answer.body)
            .ok()
            .and_then(|body| body["phase"].as_str().map(str::to_owned));
        match phase {
            Some(phase) => println!("not ready ({}; phase {phase})", answer.status),
            None => println!("not ready ({})", answer.status),
        }
    }
    if answer.status == StatusCode::OK {
        exit::OK
    } else {
        exit::FAILURE
    }
}

/// The server's account of a failure: its `error`, its list of config
/// errors, or the body as sent.
fn failure(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return body.trim().to_owned();
    };
    if let Some(error) = value["error"].as_str() {
        return error.to_owned();
    }
    if let Some(errors) = value["errors"].as_array() {
        let mut text = format!(
            "the config was rejected ({} error{}); the running config is unchanged",
            errors.len(),
            if errors.len() == 1 { "" } else { "s" }
        );
        for error in errors {
            text.push_str("\nerror: ");
            if let (Some(path), Some(line), Some(column)) = (
                error["path"].as_str(),
                error["line"].as_u64(),
                error["column"].as_u64(),
            ) {
                let _ = write!(text, "{path} (line {line}, column {column}): ");
            }
            text.push_str(error["message"].as_str().unwrap_or_default());
            if let Some(hint) = error["hint"].as_str() {
                let _ = write!(text, "\n  hint: {hint}");
            }
        }
        return text;
    }
    body.trim().to_owned()
}

/// The JSON of a successful answer, pretty-printed; `consumers` prints the
/// status's consumer list only.
fn pretty(body: &str, action: &Action) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(body) else {
        return body.to_owned();
    };
    if *action == Action::Consumers {
        value = value["consumers"].take();
    }
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| body.to_owned())
}

/// The reload report fields `reload` prints.
#[derive(Deserialize)]
struct ReloadSummary {
    generation: u64,
    added: Vec<String>,
    removed: Vec<String>,
    changed: Vec<String>,
    sources_added: Vec<String>,
    sources_removed: Vec<String>,
    sources_restarted: Vec<String>,
    restart_required: Vec<String>,
}

/// A successful answer as text for people.
fn render(action: &Action, body: &str) -> Result<String, serde_json::Error> {
    Ok(match action {
        Action::Status => {
            let status: Status = serde_json::from_str(body)?;
            let mut text = format!(
                "tgin {}, generation {}, {}\n\n",
                status.version, status.generation, status.phase
            );
            text.push_str(&sources_table(&status));
            text.push('\n');
            text.push_str(&consumers_table(&status.consumers));
            text
        }
        Action::Consumers => {
            let status: Status = serde_json::from_str(body)?;
            consumers_table(&status.consumers)
        }
        Action::Reload => {
            let report: ReloadSummary = serde_json::from_str(body)?;
            let mut text = format!("reloaded: generation {}\n", report.generation);
            for (label, names) in [
                ("consumers added", &report.added),
                ("consumers removed", &report.removed),
                ("consumers changed", &report.changed),
                ("sources added", &report.sources_added),
                ("sources removed", &report.sources_removed),
                ("sources restarted", &report.sources_restarted),
            ] {
                if !names.is_empty() {
                    let _ = writeln!(text, "  {label}: {}", names.join(", "));
                }
            }
            if !report.restart_required.is_empty() {
                let _ = writeln!(
                    text,
                    "  takes effect after a restart: {}",
                    report.restart_required.join(", ")
                );
            }
            text
        }
        Action::Pause(name) => format!("paused {name}\n"),
        Action::Resume(name) => format!("resumed {name}\n"),
        Action::Drain(name) => format!("draining {name}\n"),
        Action::Purge(name) => {
            #[derive(Deserialize)]
            struct Purged {
                purged: u64,
            }
            let purged: Purged = serde_json::from_str(body)?;
            format!("purged {} updates from {name}\n", purged.purged)
        }
        Action::Health => body.to_owned(),
    })
}

fn sources_table(status: &Status) -> String {
    let rows = status
        .sources
        .iter()
        .map(|source| {
            vec![
                source.name.clone(),
                source.ingress.clone().unwrap_or_else(|| "-".to_owned()),
                source.state.clone(),
                source.updates_total.to_string(),
                source.errors_total.to_string(),
                source.error.clone().unwrap_or_default(),
            ]
        })
        .collect();
    table(
        &[
            "SOURCE",
            "INGRESS",
            "STATE",
            "UPDATES",
            "ERRORS",
            "LAST ERROR",
        ],
        rows,
    )
}

fn consumers_table(consumers: &[ConsumerStatus]) -> String {
    let rows = consumers
        .iter()
        .map(|consumer| {
            let endpoints: Vec<String> = consumer
                .endpoints
                .iter()
                .map(|e| format!("{} ({})", e.url_redacted, e.breaker))
                .collect();
            vec![
                consumer.name.clone(),
                consumer.mode.clone(),
                consumer.depth.to_string(),
                consumer.leased.to_string(),
                consumer.in_flight.to_string(),
                if consumer.paused { "yes" } else { "no" }.to_owned(),
                endpoints.join(", "),
            ]
        })
        .collect();
    table(
        &[
            "CONSUMER",
            "MODE",
            "DEPTH",
            "LEASED",
            "IN FLIGHT",
            "PAUSED",
            "ENDPOINTS",
        ],
        rows,
    )
}

/// Left-aligned columns two spaces apart; trailing blanks trimmed.
fn table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    let lines =
        std::iter::once(headers.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>()).chain(rows);
    for row in lines {
        let mut line = String::new();
        for (cell, width) in row.iter().zip(&widths) {
            let _ = write!(line, "{cell:<width$}  ");
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The admin token must never pass through a proxy, nor may the health
    /// check depend on one: a proxy the environment (here: the options)
    /// names is not used.
    #[tokio::test]
    async fn admin_requests_bypass_proxies() {
        use wiremock::matchers::path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let admin = MockServer::start().await;
        let proxy = MockServer::start().await;
        for server in [&admin, &proxy] {
            Mock::given(path("/readyz"))
                .respond_with(ResponseTemplate::new(200))
                .mount(server)
                .await;
        }
        let options = ClientOptions {
            proxy: Some(reqwest::Proxy::all(proxy.uri()).expect("a proxy URL")),
            ..client_options(HEALTH_TIMEOUT)
        };
        let response = http::client(&options)
            .get(format!("{}/readyz", admin.uri()))
            .bearer_auth("admin-token")
            .send()
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::OK);
        let admin_seen = admin.received_requests().await.expect("recording on");
        let proxy_seen = proxy.received_requests().await.expect("recording on");
        assert_eq!((admin_seen.len(), proxy_seen.len()), (1, 0));
    }

    #[test]
    fn table_aligns_columns() {
        let text = table(
            &["NAME", "N"],
            vec![
                vec!["long-name".to_owned(), "1".to_owned()],
                vec!["x".to_owned(), "22".to_owned()],
            ],
        );
        assert_eq!(text, "NAME       N\nlong-name  1\nx          22\n");
    }

    #[test]
    fn endpoints_extend_the_base_path_with_escaped_segments() {
        let base = parse_url("http://127.0.0.1:9090").unwrap();
        assert_eq!(
            endpoint(&base, &["v1", "consumers", "workers", "pause"]).as_str(),
            "http://127.0.0.1:9090/v1/consumers/workers/pause"
        );
        assert_eq!(
            endpoint(&base, &["v1", "consumers", "a/b?c", "purge"]).as_str(),
            "http://127.0.0.1:9090/v1/consumers/a%2Fb%3Fc/purge"
        );
        let proxied = parse_url("https://ops.example/tgin/?x=1").unwrap();
        assert_eq!(
            endpoint(&proxied, &["readyz"]).as_str(),
            "https://ops.example/tgin/readyz"
        );
        assert!(parse_url("ftp://host").is_err());
        assert!(parse_url("not a url").is_err());
    }

    #[test]
    fn failure_lists_config_errors() {
        let body = r#"{"errors":[{"path":"bots[0].token","line":3,"column":12,"message":"bad","hint":"fix it"},{"path":null,"line":null,"column":null,"message":"other","hint":null}]}"#;
        assert_eq!(
            failure(body),
            "the config was rejected (2 errors); the running config is unchanged\nerror: bots[0].token (line 3, column 12): bad\n  hint: fix it\nerror: other"
        );
        assert_eq!(failure(r#"{"error":"unauthorized"}"#), "unauthorized");
        assert_eq!(failure("plain text\n"), "plain text");
    }

    #[test]
    fn token_file_loses_its_line_ending_only() {
        let dir = std::env::temp_dir().join(format!("tgin-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("token");
        std::fs::write(&file, " secret token \r\n").unwrap();
        assert_eq!(read_token(&file).unwrap(), " secret token ");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
