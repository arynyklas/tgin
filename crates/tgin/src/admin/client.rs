//! `tgin admin`: the admin API from the command line.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use tgin_core::config::model::HumanDuration;
use tgin_runtime::http::{self, ClientOptions, describe_error};
use url::Url;

use super::{BotMove, ConsumerStatus, LoginStatus, MAX_MOVE_TIMEOUT, Moved, Status};
use crate::exit;

/// The admin API `tgin admin` talks to without `--url`: the default of
/// `admin.listen`.
pub const DEFAULT_URL: &str = "http://127.0.0.1:9090";
/// How long `health` waits for an answer.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long other commands wait; a reload may wait on Telegram.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one call of `login` may take: SRP and Telegram's answer.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(90);
/// How often `login` asks for the QR's current token.
const LOGIN_POLL: Duration = Duration::from_secs(1);

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
    /// `bot logout|close`: moves the bot `name` off its Bot API server,
    /// within `timeout`; without `yes`, only says what would happen.
    Bot {
        name: String,
        how: BotMove,
        timeout: Option<HumanDuration>,
        yes: bool,
    },
    /// `login`: logs the Telegram account `account` in, by QR or (with
    /// `code`) by a login code, asking for the 2FA password when needed.
    Login {
        account: String,
        code: bool,
    },
    /// `logout`: logs the account out and clears its session; without
    /// `yes`, only says what would happen.
    AccountLogout {
        account: String,
        yes: bool,
    },
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

/// Parses `bot logout|close --timeout`: a duration such as `30s`, at most
/// 1 h, without month or year units.
///
/// # Errors
///
/// Not such a duration.
pub fn parse_timeout(text: &str) -> Result<HumanDuration, String> {
    let timeout: HumanDuration = text
        .parse()
        .map_err(|err| format!("not a duration such as `30s`: {err}"))?;
    if let Some(unit) = timeout.calendar_unit() {
        return Err(format!("`{unit}` is a month or year unit"));
    }
    if timeout.get() > MAX_MOVE_TIMEOUT {
        return Err("a move's timeout is at most 1h".to_owned());
    }
    Ok(timeout)
}

/// What `logout` without `--yes` prints: the steps it would take.
fn logout_plan(account: &str) -> String {
    format!(
        "tgin admin logout would, for the account {account}:\n\
         1. park it if it runs: stop its ingest, give its RPC calls up to 10 s, close its session\n\
         2. call auth.logOut on Telegram (the account's other devices stay logged in)\n\
         3. clear its session: auth keys, login, peers and update state\n\
         Run it again with --yes to do it."
    )
}

/// What `bot logout|close` without `--yes` prints: the steps it would take.
fn move_plan(name: &str, how: BotMove, timeout: Option<&HumanDuration>) -> String {
    let call = match how {
        BotMove::LogOut => "call logOut on its Bot API server",
        BotMove::Close => "call deleteWebhook, then close, on its Bot API server",
    };
    let within = timeout.map_or("reload.drain_timeout", HumanDuration::as_str);
    format!(
        "tgin admin bot {segment} would, for the bot {name}:\n\
         1. wait until its Bot API server holds no update for it (its ingest runs on)\n\
         2. stop its ingest and park it: reloads do not start it again until one changes its upstream\n\
         3. wait until tgin has delivered the updates it holds of it\n\
         4. stop sending its calls to any server, and wait for those being sent\n\
         5. {call}\n\
         Steps 1 to 4 get {within} (at most 1h), counted from the request; if step 1 does not finish in time, nothing changes.\n\
         Run it again with --yes to do it.",
        segment = how.segment(),
    )
}

/// Runs `command` and returns the process exit code.
pub fn run(command: &Command) -> u8 {
    if let Action::Bot {
        name,
        how,
        timeout,
        yes: false,
    } = &command.action
    {
        eprintln!("{}", move_plan(name, *how, timeout.as_ref()));
        return exit::USAGE;
    }
    if let Action::AccountLogout {
        account,
        yes: false,
    } = &command.action
    {
        eprintln!("{}", logout_plan(account));
        return exit::USAGE;
    }
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
    let code = runtime.block_on(execute(command, token.as_deref()));
    // A password prompt cancelled by Ctrl+C still blocks its thread on the
    // terminal: do not wait for it.
    runtime.shutdown_background();
    code
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

/// The admin client's options: `timeout` for connecting, and for the whole
/// request unless `None`, and no proxy ever: the bearer token would
/// reach the proxy in cleartext, and the health check would depend on it.
fn client_options(connect: Duration, total: Option<Duration>) -> ClientOptions {
    ClientOptions {
        connect_timeout: connect,
        request_timeout: total,
        no_proxy: true,
        ..ClientOptions::default()
    }
}

async fn execute(command: &Command, token: Option<&str>) -> u8 {
    if let Action::Login { account, code } = &command.action {
        let mut api = HttpLogin {
            client: http::client(&client_options(REQUEST_TIMEOUT, Some(LOGIN_TIMEOUT))),
            base: command.url.clone(),
            token: token.map(str::to_owned),
            account: account.clone(),
        };
        let method = if *code { "code" } else { "qr" };
        return login(
            &mut api,
            &mut Tty,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
            method,
            LOGIN_POLL,
            async {
                // Without a signal handler, never interrupt.
                if tokio::signal::ctrl_c().await.is_err() {
                    std::future::pending::<()>().await;
                }
            },
        )
        .await;
    }
    let (connect, total) = match &command.action {
        Action::Health => (HEALTH_TIMEOUT, Some(HEALTH_TIMEOUT)),
        // tgin bounds a move itself: the timeout (the config's when none is
        // given), then its calls to the Bot API server.
        Action::Bot { timeout, .. } => (
            REQUEST_TIMEOUT,
            timeout
                .as_ref()
                .map(|t| t.get().saturating_add(REQUEST_TIMEOUT)),
        ),
        _ => (REQUEST_TIMEOUT, Some(REQUEST_TIMEOUT)),
    };
    let client = http::client(&client_options(connect, total));
    let (method, segments): (Method, Vec<&str>) = match &command.action {
        Action::Status | Action::Consumers => (Method::GET, vec!["v1", "status"]),
        Action::Reload => (Method::POST, vec!["v1", "reload"]),
        Action::Pause(name) => (Method::POST, vec!["v1", "consumers", name, "pause"]),
        Action::Resume(name) => (Method::POST, vec!["v1", "consumers", name, "resume"]),
        Action::Drain(name) => (Method::POST, vec!["v1", "consumers", name, "drain"]),
        Action::Purge(name) => (Method::POST, vec!["v1", "consumers", name, "purge"]),
        Action::Health => (Method::GET, vec!["readyz"]),
        Action::Bot { name, how, .. } => (Method::POST, vec!["v1", "bots", name, how.segment()]),
        Action::AccountLogout { account, .. } => {
            (Method::POST, vec!["v1", "accounts", account, "logout"])
        }
        // Run above.
        Action::Login { .. } => return exit::FAILURE,
    };
    let mut url = endpoint(&command.url, &segments);
    if let Action::Bot {
        timeout: Some(timeout),
        ..
    } = &command.action
    {
        url.query_pairs_mut()
            .append_pair("timeout", timeout.as_str());
    }
    let mut request = client.request(method, url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let send = async {
        let response = request.send().await?;
        let status = response.status();
        let body = response.text().await?;
        Ok::<_, reqwest::Error>(Answer { status, body })
    };
    let sent = match total {
        Some(total) => tokio::time::timeout(total, send).await.map_err(|_| total),
        None => Ok(send.await),
    };
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
        Err(total) => {
            eprintln!(
                "tgin: the admin API at {origin} did not answer within {}s",
                total.as_secs()
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

/// The server's account of a failure: its `error` (with its
/// `description`, when it sent one), its list of config errors, or the
/// body as sent.
fn failure(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return body.trim().to_owned();
    };
    if let Some(error) = value["error"].as_str() {
        return match value["description"].as_str() {
            Some(description) => format!("{error}: {description}"),
            None => error.to_owned(),
        };
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
    /// Absent from a server older than this client.
    #[serde(default)]
    upstream_changed: Vec<String>,
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
                ("upstream changed", &report.upstream_changed),
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
        Action::Health | Action::Login { .. } => body.to_owned(),
        Action::AccountLogout { account, .. } => {
            format!("{account} logged out; its session is cleared\n")
        }
        Action::Bot { how, .. } => {
            let moved: Moved = serde_json::from_str(body)?;
            let done = match how {
                BotMove::LogOut => "logged out of",
                BotMove::Close => "closed on",
            };
            let mut text = format!(
                "{} {done} its Bot API server ({}); parked until a reload changes its upstream\n",
                moved.bot, moved.upstream
            );
            if let Some(result) = &moved.result {
                let _ = writeln!(text, "  the server said: {result}");
            }
            if moved.left > 0 {
                let _ = writeln!(
                    text,
                    "  {} of its updates were still queued in tgin; they are still delivered",
                    moved.left
                );
            }
            text
        }
    })
}

fn sources_table(status: &Status) -> String {
    let rows = status
        .sources
        .iter()
        .map(|source| {
            let state = if source.parked {
                format!("{}, parked", source.state)
            } else {
                source.state.clone()
            };
            let upstream = if source.upstream.is_empty() {
                "-".to_owned()
            } else {
                source.upstream.clone()
            };
            vec![
                source.name.clone(),
                source.ingress.clone().unwrap_or_else(|| "-".to_owned()),
                upstream,
                state,
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
            "UPSTREAM",
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
                .map(|e| {
                    let runtime = if e.runtime { ", runtime" } else { "" };
                    format!("{} ({}{runtime})", e.url_redacted, e.breaker)
                })
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

/// What `login` needs of the admin API; [`HttpLogin`] in production.
trait LoginApi {
    /// `POST …/login {"method"}`.
    async fn start(&mut self, method: &str) -> Result<LoginStatus, String>;
    /// `GET …/login`.
    async fn current(&mut self) -> Result<LoginStatus, String>;
    /// `POST …/login/code` or `…/login/password`: the next state, or why to
    /// try again.
    async fn submit(&mut self, field: Secret, value: String) -> Result<Submitted, String>;
    /// `DELETE …/login`.
    async fn cancel(&mut self) -> Result<(), String>;
}

/// Which secret `submit` hands over.
#[derive(Clone, Copy)]
enum Secret {
    Code,
    Password,
}

enum Submitted {
    Next(LoginStatus),
    /// 422: wrong, try again; the server's message.
    Retry(String),
}

/// Asks the operator for a code or the 2FA password; [`Tty`] in
/// production.
trait Prompt {
    async fn ask(&mut self, prompt: &str) -> std::io::Result<String>;
}

/// The terminal, without echo. The read runs on a blocking thread, so a
/// Ctrl+C during it still cancels the login.
struct Tty;

impl Prompt for Tty {
    async fn ask(&mut self, prompt: &str) -> std::io::Result<String> {
        let prompt = prompt.to_owned();
        tokio::task::spawn_blocking(move || rpassword::prompt_password(prompt))
            .await
            .map_err(std::io::Error::other)?
    }
}

struct HttpLogin {
    client: reqwest::Client,
    base: Url,
    token: Option<String>,
    account: String,
}

impl HttpLogin {
    async fn call(
        &self,
        method: Method,
        last: &[&str],
        body: Option<String>,
    ) -> Result<Answer, String> {
        let mut segments = vec!["v1", "accounts", self.account.as_str(), "login"];
        segments.extend_from_slice(last);
        let mut request = self.client.request(method, endpoint(&self.base, &segments));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        let origin = self.base.origin().ascii_serialization();
        let response = request.send().await.map_err(|err| {
            format!(
                "the admin API at {origin} did not answer: {}",
                describe_error(&err)
            )
        })?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| format!("the admin API at {origin}: {}", describe_error(&err)))?;
        Ok(Answer { status, body })
    }

    fn state(answer: &Answer) -> Result<LoginStatus, String> {
        if !answer.status.is_success() {
            return Err(format!("{}: {}", answer.status, failure(&answer.body)));
        }
        serde_json::from_str(&answer.body)
            .map_err(|err| format!("the admin API sent an unexpected answer: {err}"))
    }
}

impl LoginApi for HttpLogin {
    async fn start(&mut self, method: &str) -> Result<LoginStatus, String> {
        let body = serde_json::json!({ "method": method }).to_string();
        Self::state(&self.call(Method::POST, &[], Some(body)).await?)
    }

    async fn current(&mut self) -> Result<LoginStatus, String> {
        Self::state(&self.call(Method::GET, &[], None).await?)
    }

    async fn submit(&mut self, field: Secret, value: String) -> Result<Submitted, String> {
        let (segment, body) = match field {
            Secret::Code => ("code", serde_json::json!({ "code": value })),
            Secret::Password => ("password", serde_json::json!({ "password": value })),
        };
        let answer = self
            .call(Method::POST, &[segment], Some(body.to_string()))
            .await?;
        if answer.status == StatusCode::UNPROCESSABLE_ENTITY {
            return Ok(Submitted::Retry(failure(&answer.body)));
        }
        Self::state(&answer).map(Submitted::Next)
    }

    async fn cancel(&mut self) -> Result<(), String> {
        let answer = self.call(Method::DELETE, &[], None).await?;
        // 404: the login had already ended.
        if answer.status.is_success() || answer.status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(format!("{}: {}", answer.status, failure(&answer.body)))
        }
    }
}

/// The QR of `url` in Unicode half blocks, light on dark, with its quiet
/// zone: two modules per character cell.
fn qr_text(url: &str) -> Option<String> {
    use qrcode::render::unicode::Dense1x2;
    let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
    Some(
        code.render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .quiet_zone(true)
            .build(),
    )
}

/// `tgin admin login`: drives the account's login through the admin API.
/// The QR goes to `out` (a fresh one each time the server renews the
/// token); prompts and messages go to `err`. The URL itself is printed
/// nowhere: only its QR.
/// `interrupt` (Ctrl+C in production) ends the login on the server too
/// (`DELETE …/login`), so it neither renews tokens nor holds the session.
async fn login(
    api: &mut impl LoginApi,
    prompt: &mut impl Prompt,
    out: &mut impl std::io::Write,
    err: &mut impl std::io::Write,
    method: &str,
    poll: Duration,
    interrupt: impl Future<Output = ()>,
) -> u8 {
    let outcome = {
        let drive = drive_login(&mut *api, prompt, out, &mut *err, method, poll);
        tokio::pin!(drive);
        tokio::pin!(interrupt);
        tokio::select! {
            biased;
            outcome = &mut drive => Some(outcome),
            () = &mut interrupt => None,
        }
    };
    match outcome {
        Some(Ok(code)) => code,
        Some(Err(message)) => {
            let _ = writeln!(err, "tgin: {message}");
            exit::FAILURE
        }
        None => {
            let _ = match api.cancel().await {
                Ok(()) => writeln!(err, "\ntgin: login cancelled"),
                Err(message) => writeln!(err, "\ntgin: cancelling the login: {message}"),
            };
            exit::FAILURE
        }
    }
}

async fn drive_login(
    api: &mut impl LoginApi,
    prompt: &mut impl Prompt,
    out: &mut impl std::io::Write,
    err: &mut impl std::io::Write,
    method: &str,
    poll: Duration,
) -> Result<u8, String> {
    let io = |e: std::io::Error| format!("the terminal: {e}");
    let mut state = api.start(method).await?;
    let mut shown: Option<super::LoginUrl> = None;
    loop {
        state = match state {
            LoginStatus::Connecting => {
                tokio::time::sleep(poll).await;
                api.current().await?
            }
            LoginStatus::Qr { url, .. } => {
                if shown.as_ref() != Some(&url) {
                    let qr = qr_text(&url.0).ok_or("the login URL does not fit a QR code")?;
                    writeln!(out, "{qr}").map_err(io)?;
                    out.flush().map_err(io)?;
                    if shown.is_none() {
                        writeln!(
                            err,
                            "Scan the QR with the Telegram app of the account: Settings > Devices > Link Desktop Device.\n\
                             A new QR follows by itself when this one expires."
                        )
                        .map_err(io)?;
                    }
                    shown = Some(url);
                }
                tokio::time::sleep(poll).await;
                api.current().await?
            }
            LoginStatus::CodeSent { kind, .. } => {
                writeln!(err, "Telegram sent a login code ({kind}).").map_err(io)?;
                let code = prompt.ask("Login code: ").await.map_err(io)?;
                submitted(api, err, Secret::Code, code).await?
            }
            LoginStatus::NeedPassword { hint } => {
                let ask = match &hint {
                    Some(hint) => format!("2FA password (hint: {hint}): "),
                    None => "2FA password: ".to_owned(),
                };
                let password = prompt.ask(&ask).await.map_err(io)?;
                submitted(api, err, Secret::Password, password).await?
            }
            LoginStatus::LoggedIn { user_id } => {
                writeln!(out, "logged in as user {user_id}").map_err(io)?;
                return Ok(exit::OK);
            }
            LoginStatus::Failed { reason, message } => {
                return Err(format!("the login failed ({reason}): {message}"));
            }
            LoginStatus::NeedEmailSetup => {
                return Err(
                    "Telegram asks to set up a login email first, in an official app".into(),
                );
            }
            LoginStatus::NeedEmailCode => {
                return Err("Telegram sent the code to the login email, which tgin does not support; log in by QR".into());
            }
            LoginStatus::PaymentRequired => {
                return Err("Telegram asks for a payment to send the code; log in by QR".into());
            }
            LoginStatus::Unknown => {
                return Err("the admin API reports a login state this tgin does not know".into());
            }
        };
    }
}

/// Hands over a code or password; on a retryable refusal, says why and
/// shows the state again (which asks again).
async fn submitted(
    api: &mut impl LoginApi,
    err: &mut impl std::io::Write,
    field: Secret,
    value: String,
) -> Result<LoginStatus, String> {
    match api.submit(field, value).await? {
        Submitted::Next(state) => Ok(state),
        Submitted::Retry(why) => {
            let _ = writeln!(err, "{why}; try again.");
            api.current().await
        }
    }
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
            ..client_options(HEALTH_TIMEOUT, Some(HEALTH_TIMEOUT))
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
    fn logout_plan_says_a_running_account_is_parked() {
        let plan = logout_plan("main");
        let park = plan.find("park it if it runs").expect("the park step");
        let log_out = plan.find("auth.logOut").expect("the logout step");
        assert!(park < log_out, "{plan}");
        assert!(!plan.contains("refuses"), "{plan}");
    }

    #[test]
    fn move_plan_lists_the_steps_in_order() {
        let timeout: HumanDuration = "5s".parse().unwrap();
        let plan = move_plan("shop", BotMove::Close, Some(&timeout));
        let steps = [
            "1. wait until its Bot API server holds no update",
            "2. stop its ingest and park it",
            "3. wait until tgin has delivered",
            "4. stop sending its calls to any server",
            "5. call deleteWebhook, then close",
            "Steps 1 to 4 get 5s (at most 1h), counted from the request",
        ];
        let mut at = 0;
        for step in steps {
            let found = plan[at..]
                .find(step)
                .unwrap_or_else(|| panic!("{step}: {plan}"));
            at += found + step.len();
        }
        let plan = move_plan("shop", BotMove::LogOut, None);
        assert!(
            plan.contains("5. call logOut on its Bot API server"),
            "{plan}"
        );
        assert!(
            plan.contains("get reload.drain_timeout (at most 1h)"),
            "{plan}"
        );
    }

    #[test]
    fn move_timeouts_are_at_most_an_hour() {
        for ok in ["30s", "1h", "60m", "0s"] {
            assert!(parse_timeout(ok).is_ok(), "{ok}");
        }
        for bad in ["61m", "1h 1s", "2h", "1M", "soon"] {
            assert!(parse_timeout(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn failure_lists_config_errors() {
        let body = r#"{"errors":[{"path":"bots[0].token","line":3,"column":12,"message":"bad","hint":"fix it"},{"path":null,"line":null,"column":null,"message":"other","hint":null}]}"#;
        assert_eq!(
            failure(body),
            "the config was rejected (2 errors); the running config is unchanged\nerror: bots[0].token (line 3, column 12): bad\n  hint: fix it\nerror: other"
        );
        assert_eq!(failure(r#"{"error":"unauthorized"}"#), "unauthorized");
        assert_eq!(
            failure(r#"{"error":"upstream refused","description":"Bad Request: no"}"#),
            "upstream refused: Bad Request: no"
        );
        assert_eq!(
            failure(
                r#"{"error":"calls in flight","description":"2 calls of the bot were still being sent"}"#
            ),
            "calls in flight: 2 calls of the bot were still being sent"
        );
        assert_eq!(failure("plain text\n"), "plain text");
    }

    /// Admin answers grow additively: fields and enum values this client
    /// does not know are ignored, not errors, and fields older servers do
    /// not send are optional.
    #[test]
    fn admin_client_ignores_unknown_fields() {
        let status = r#"{
            "version": "9.9.9", "generation": 3, "phase": "rebalancing", "shard": "b",
            "sources": [
                {"name": "a", "kind": "channel", "ingress": "mtproto", "state": "warming",
                 "updates_total": 1, "errors_total": 0, "upstream": "tba", "parked": true,
                 "lag": {"seconds": 2}},
                {"name": "b", "kind": "bot", "ingress": null, "state": "running",
                 "updates_total": 0, "errors_total": 0}
            ],
            "consumers": [
                {"name": "w", "mode": "stream", "depth": 0, "leased": 0, "in_flight": 0,
                 "paused": false, "endpoints": [{"url_redacted": "https://x", "breaker": "tripped",
                 "weight": 2}], "priority": 1}
            ],
            "tasks": [{"name": "t", "state": "sleeping", "since": 5}]
        }"#;
        let text = render(&Action::Status, status).expect("decoded");
        assert!(text.contains("UPSTREAM"), "{text}");
        let row = |name: &str| {
            text.lines()
                .find(|l| l.starts_with(name))
                .unwrap_or_else(|| panic!("no {name} row in\n{text}"))
        };
        assert!(
            row("a ").contains("tba") && row("a ").contains("warming, parked"),
            "{text}"
        );
        assert!(!row("b ").contains("parked"), "{text}");
        assert!(render(&Action::Consumers, status).is_ok());

        let reload = r#"{"generation": 2, "added": [], "removed": [], "changed": [],
            "sources_added": [], "sources_removed": [], "sources_restarted": [],
            "restart_required": [], "warnings": ["x"], "mode": "dry"}"#;
        assert!(render(&Action::Reload, reload).is_ok());
        let moved = r#"{"bot": "a", "left": 0, "upstream": "tba", "result": null, "how": "new"}"#;
        let bot = Action::Bot {
            name: "a".to_owned(),
            how: BotMove::LogOut,
            timeout: None,
            yes: true,
        };
        assert!(render(&bot, moved).is_ok());
        let purged = r#"{"purged": 3, "left": 0}"#;
        assert!(render(&Action::Purge("w".to_owned()), purged).is_ok());
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

    /// A scripted admin API for `login`.
    struct FakeApi {
        states: std::collections::VecDeque<LoginStatus>,
        submitted: Vec<(&'static str, String)>,
        /// What each submit answers.
        submits: std::collections::VecDeque<Submitted>,
        cancelled: bool,
    }

    impl LoginApi for FakeApi {
        async fn start(&mut self, method: &str) -> Result<LoginStatus, String> {
            assert_eq!(method, "qr");
            self.current().await
        }

        async fn current(&mut self) -> Result<LoginStatus, String> {
            self.states
                .pop_front()
                .ok_or_else(|| "script ended".to_owned())
        }

        async fn submit(&mut self, field: Secret, value: String) -> Result<Submitted, String> {
            let name = match field {
                Secret::Code => "code",
                Secret::Password => "password",
            };
            self.submitted.push((name, value));
            self.submits
                .pop_front()
                .ok_or_else(|| "script ended".to_owned())
        }

        async fn cancel(&mut self) -> Result<(), String> {
            self.cancelled = true;
            Ok(())
        }
    }

    struct Answers(Vec<String>, Vec<String>);

    impl Prompt for Answers {
        async fn ask(&mut self, prompt: &str) -> std::io::Result<String> {
            self.1.push(prompt.to_owned());
            Ok(self.0.remove(0))
        }
    }

    fn qr(token: &str) -> LoginStatus {
        LoginStatus::Qr {
            url: super::super::LoginUrl(format!("tg://login?token={token}")),
            expires: 1_800_000_000,
        }
    }

    /// The QR is drawn in half blocks on stdout, again when the server
    /// renews the token and not while it stays; the token is printed
    /// nowhere as text. A wrong 2FA password is asked again; the
    /// passwords never reach either stream.
    #[tokio::test]
    async fn qr_renders_without_token_in_stderr() {
        let mut api = FakeApi {
            states: [
                LoginStatus::Connecting,
                qr("FIRSTtoken"),
                qr("FIRSTtoken"),
                qr("SECONDtoken"),
                LoginStatus::NeedPassword {
                    hint: Some("pet".into()),
                },
                LoginStatus::NeedPassword {
                    hint: Some("pet".into()),
                },
            ]
            .into(),
            submitted: Vec::new(),
            submits: [
                Submitted::Retry("the 2FA password is wrong".into()),
                Submitted::Next(LoginStatus::LoggedIn { user_id: 7 }),
            ]
            .into(),
            cancelled: false,
        };
        let mut prompt = Answers(vec!["wrong-pw".into(), "right-pw".into()], Vec::new());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = login(
            &mut api,
            &mut prompt,
            &mut out,
            &mut err,
            "qr",
            Duration::ZERO,
            std::future::pending(),
        )
        .await;
        let (out, err) = (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        );
        assert_eq!(code, exit::OK, "{err}");
        let first = qr_text("tg://login?token=FIRSTtoken").unwrap();
        let second = qr_text("tg://login?token=SECONDtoken").unwrap();
        assert!(first.contains('▀') || first.contains('▄'), "{first}");
        assert_eq!(out.matches(&first).count(), 1, "drawn once while unchanged");
        assert_eq!(out.matches(&second).count(), 1, "redrawn when renewed");
        assert!(out.ends_with("logged in as user 7\n"), "{out}");
        assert!(
            err.contains("Settings > Devices > Link Desktop Device"),
            "{err}"
        );
        assert!(
            err.contains("the 2FA password is wrong; try again."),
            "{err}"
        );
        for secret in ["token", "tg://", "wrong-pw", "right-pw"] {
            assert!(!err.contains(secret), "{secret} in stderr: {err}");
            assert!(!out.contains(secret), "{secret} in stdout: {out}");
        }
        assert_eq!(
            api.submitted,
            [
                ("password", "wrong-pw".to_owned()),
                ("password", "right-pw".to_owned())
            ]
        );
        assert_eq!(prompt.1, ["2FA password (hint: pet): "; 2]);
    }

    /// A failed login exits non-zero with its reason; an unknown state
    /// from a newer server is reported, not misread.
    #[tokio::test]
    async fn login_failures_exit_non_zero() {
        for (state, said) in [
            (
                LoginStatus::Failed {
                    reason: "wrong_account".into(),
                    message: "not the configured one".into(),
                },
                "the login failed (wrong_account): not the configured one",
            ),
            (LoginStatus::Unknown, "does not know"),
        ] {
            let mut api = FakeApi {
                states: [state].into(),
                submitted: Vec::new(),
                submits: [].into(),
                cancelled: false,
            };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = login(
                &mut api,
                &mut Answers(Vec::new(), Vec::new()),
                &mut out,
                &mut err,
                "qr",
                Duration::ZERO,
                std::future::pending(),
            )
            .await;
            assert_eq!(code, exit::FAILURE);
            assert!(String::from_utf8(err).unwrap().contains(said));
        }
        let unknown: LoginStatus =
            serde_json::from_str(r#"{"state":"sms_needed","extra":1}"#).unwrap();
        assert_eq!(unknown, LoginStatus::Unknown);
        let qr: LoginStatus = serde_json::from_str(
            r#"{"state":"qr","url":"tg://login?token=x","expires":5,"new":true}"#,
        )
        .unwrap();
        assert!(matches!(qr, LoginStatus::Qr { expires: 5, .. }));
    }

    /// Ctrl+C at the QR ends the login on the server (`DELETE …/login`) and
    /// exits non-zero; nothing else is called after it.
    #[tokio::test(start_paused = true)]
    async fn ctrl_c_cancels_the_login_on_the_server() {
        let mut api = FakeApi {
            states: std::iter::repeat_with(|| qr("tok")).take(100).collect(),
            submitted: Vec::new(),
            submits: [].into(),
            cancelled: false,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = login(
            &mut api,
            &mut Answers(Vec::new(), Vec::new()),
            &mut out,
            &mut err,
            "qr",
            Duration::from_secs(1),
            tokio::time::sleep(Duration::from_millis(2500)),
        )
        .await;
        assert_eq!(code, exit::FAILURE);
        assert!(api.cancelled, "DELETE sent");
        // start + two polls before the interrupt.
        assert_eq!(api.states.len(), 97);
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("login cancelled"), "{err}");
    }
}
