//! `cargo xtask e2e [--keep]`: the end-to-end suite. Real bot frameworks
//! (aiogram, python-telegram-bot, grammY, Telegraf) run against tgin and the
//! fake Telegram in `tests/e2e/compose.yaml`; every scenario is checked
//! through the fake's call journal. The self-hosted scenario has a fake of
//! its own in local mode (`fake-local`) and its own journal.
//!
//! The runner builds the images, starts the stack with
//! `docker compose up -d --wait`, and runs the scenarios in order. Each check
//! polls the journal (or tgin's metrics) every 100 ms for at most 30 s; there
//! are no fixed sleeps. A count that must stay exact ("exactly one
//! sendMessage") is checked when the scenario sees it and once more after
//! the last scenario, so a late duplicate fails the run too. On failure,
//! Ctrl-C or a panic, the compose logs (and, when the stack is up, the
//! journal's tail) are printed, and the stack is taken down with
//! `docker compose down -v`, unless `--keep`.
//!
//! The compose project is `tgin-e2e`, or `TGIN_E2E_PROJECT` when set; the
//! containers, network and images are named after it.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::{Client, Url, multipart};
use serde::Deserialize;
use serde_json::{Value, json};

/// The compose project unless [`PROJECT_VAR`] names another.
const DEFAULT_PROJECT: &str = "tgin-e2e";
/// The environment variable naming the compose project; compose.yaml reads
/// it too, for the image names.
const PROJECT_VAR: &str = "TGIN_E2E_PROJECT";
/// How often a check polls.
const POLL: Duration = Duration::from_millis(100);
/// How long a check polls before it fails.
const BOUND: Duration = Duration::from_secs(30);
/// How long `up --wait` may take, seconds (the images are built before).
const UP_TIMEOUT: &str = "300";
/// The size of the document the file scenario sends: over the gateway's
/// 64 KiB buffering threshold, so the upload streams.
const DOCUMENT_SIZE: usize = 100_000;

/// One framework: a polling bot and a webhook bot. The tokens are the fake
/// values in `compose.yaml` (a unit test keeps them in step).
struct Framework {
    name: &'static str,
    poll_token: &'static str,
    hook_token: &'static str,
    /// Its uploads use `Transfer-Encoding: chunked`.
    chunked_upload: bool,
    /// It confirms the last offset when it stops; aiogram does not,
    /// so a redelivery of the last update is tolerated for it.
    confirms_on_stop: bool,
    /// Its webhook bot registers with setWebhook on the scoped base; else
    /// tgin pushes to the endpoint configured in `tgin.yaml`.
    registers_hook: bool,
}

const FRAMEWORKS: [Framework; 4] = [
    Framework {
        name: "aiogram",
        poll_token: "700001:e2e_fake_aiogram_poll",
        hook_token: "700002:e2e_fake_aiogram_hook",
        chunked_upload: true,
        confirms_on_stop: false,
        registers_hook: true,
    },
    Framework {
        name: "ptb",
        poll_token: "700003:e2e_fake_ptb_poll",
        hook_token: "700004:e2e_fake_ptb_hook",
        chunked_upload: false,
        confirms_on_stop: true,
        registers_hook: true,
    },
    Framework {
        name: "grammy",
        poll_token: "700005:e2e_fake_grammy_poll",
        hook_token: "700006:e2e_fake_grammy_hook",
        chunked_upload: true,
        confirms_on_stop: true,
        registers_hook: true,
    },
    Framework {
        name: "telegraf",
        poll_token: "700007:e2e_fake_telegraf_poll",
        hook_token: "700008:e2e_fake_telegraf_hook",
        chunked_upload: true,
        confirms_on_stop: true,
        registers_hook: false,
    },
];

impl Framework {
    /// Its polling bot: the compose service, the tgin bot and the consumer.
    fn poll(&self) -> String {
        format!("{}-poll", self.name)
    }

    /// Its webhook bot: the compose service, the tgin bot and the consumer.
    fn hook(&self) -> String {
        format!("{}-hook", self.name)
    }
}

fn framework(name: &str) -> &'static Framework {
    FRAMEWORKS
        .iter()
        .find(|f| f.name == name)
        .unwrap_or(&FRAMEWORKS[0])
}

/// The self-hosted scenario's polling bot (python-telegram-bot): the compose
/// service, the tgin bot and the consumer, on tgin's `fake-local` upstream.
const LOCAL_POLL: &str = "local-poll";
/// Its token, as in `compose.yaml`. Realistic in shape: the local fake names
/// the bot's files directory after it.
const LOCAL_POLL_TOKEN: &str = "8100000001:AAHe2eFakeLocalPollBotTokenx7Kq9Zr2";
/// The self-hosted scenario's webhook bot (grammY), as [`LOCAL_POLL`].
const LOCAL_HOOK: &str = "local-hook";
const LOCAL_HOOK_TOKEN: &str = "8100000002:AAHe2eFakeLocalHookBotTokenm4Tp8Wd5";
/// The URL tgin registers at the local fake for [`LOCAL_HOOK`]: its data
/// plane over plain http, at the bot's default webhook path.
const LOCAL_HOOK_URL: &str = "http://tgin:8080/webhook/local-hook";
/// The local fake's files directory, mounted read-only in tgin at the same
/// path.
const LOCAL_FILES_DIR: &str = "/srv/tba";
/// A local file reference the gateway must refuse.
const LOCAL_FILE_REFERENCE: &str = "file:///etc/passwd";
/// tgin's answer to a local file reference (`local_files.rs`).
const LOCAL_FILE_REFUSED: &str = "Bad Request: local file uploads are disabled by tgin";
/// tgin's answer to a downstream `logOut` or `close` (`intercept.rs`).
const LOG_OUT_DISABLED: &str = "Bad Request: logOut and close are disabled by tgin; operators move bots with tgin admin bot logout|close";

/// Whether `token` is a bot of the local fake.
fn is_local(token: &str) -> bool {
    token == LOCAL_POLL_TOKEN || token == LOCAL_HOOK_TOKEN
}

pub(crate) fn run(keep: bool) -> ExitCode {
    let started = Instant::now();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("e2e: cannot start a tokio runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        // From here on Ctrl-C no longer kills the runner: a compose child it
        // interrupts fails its step, and the scenarios stop at the signal,
        // so either way the logs are printed and the stack is taken down.
        let mut interrupt = pin!(tokio::signal::ctrl_c());
        listen(interrupt.as_mut()).await?;
        // A stack left by an earlier `--keep` run would carry its state over.
        compose(&["down", "-v", "--remove-orphans"])
            .quiet()
            .map_err(|err| format!("cannot clear an earlier stack: {err}"))?;
        let teardown = Teardown { keep };
        let result = build_and_test(interrupt.as_mut()).await;
        if let Err(err) = &result {
            eprintln!("\ne2e FAILED: {err}");
            print_logs();
        }
        drop(teardown);
        result
    });
    let elapsed = started.elapsed().as_secs_f64();
    match result {
        Ok(()) => {
            println!("e2e: all scenarios passed in {elapsed:.1} s");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("e2e: failed after {elapsed:.1} s: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The pending Ctrl-C wait.
type Interrupt<'a> = Pin<&'a mut (dyn Future<Output = io::Result<()>> + 'a)>;

/// Polls the Ctrl-C wait once, which installs the handler.
async fn listen(interrupt: Interrupt<'_>) -> Result<(), String> {
    // A Unix signal reaches the wait only once the driver turns.
    tokio::task::yield_now().await;
    tokio::select! {
        biased;
        result = interrupt => match result {
            Ok(()) => Err("interrupted (Ctrl-C)".to_owned()),
            Err(err) => Err(format!("cannot listen for Ctrl-C: {err}")),
        },
        () = std::future::ready(()) => Ok(()),
    }
}

/// Takes the stack down when dropped, also while a panic unwinds (then it
/// prints the compose logs first), unless `--keep`.
struct Teardown {
    keep: bool,
}

impl Drop for Teardown {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("\ne2e: the runner panicked");
            print_logs();
        }
        if self.keep {
            println!(
                "e2e: --keep: the stack stays up; remove it with `docker compose -p {} down -v`",
                project()
            );
        } else if let Err(err) = compose(&["down", "-v", "--remove-orphans"]).quiet() {
            eprintln!("e2e: docker compose down failed: {err}");
        }
    }
}

fn print_logs() {
    eprintln!("\n--- docker compose logs ---");
    let _ = compose(&["logs", "--no-color", "--timestamps"]).status();
}

async fn build_and_test(mut interrupt: Interrupt<'_>) -> Result<(), String> {
    let build = Instant::now();
    compose(&["build"]).checked()?;
    println!(
        "e2e: images built in {:.1} s",
        build.elapsed().as_secs_f64()
    );
    listen(interrupt.as_mut()).await?;
    let up = Instant::now();
    compose(&["up", "-d", "--wait", "--wait-timeout", UP_TIMEOUT]).checked()?;
    println!("e2e: stack up in {:.1} s", up.elapsed().as_secs_f64());
    listen(interrupt.as_mut()).await?;
    let mut suite = Suite::new(Stack::discover()?);
    let outcome = tokio::select! {
        outcome = suite.scenarios() => outcome,
        _ = interrupt => Err("interrupted (Ctrl-C)".to_owned()),
    };
    if outcome.is_err() {
        suite.print_journal_tail().await;
    }
    outcome
}

/// The compose project: [`PROJECT_VAR`] if set, else [`DEFAULT_PROJECT`].
fn project() -> String {
    std::env::var(PROJECT_VAR)
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_PROJECT.to_owned())
}

fn compose_file() -> PathBuf {
    crate::repo_path(&["tests", "e2e", "compose.yaml"])
}

/// `docker compose -f tests/e2e/compose.yaml -p <project> <args>`, with
/// [`PROJECT_VAR`] set for compose.yaml's image names.
fn compose(args: &[&str]) -> Command {
    let project = project();
    let mut command = Command::new("docker");
    command
        .arg("compose")
        .arg("-f")
        .arg(compose_file())
        .args(["-p", &project])
        .args(args)
        .env(PROJECT_VAR, &project);
    command
}

trait Run {
    /// Runs with inherited output; fails on a non-zero exit.
    fn checked(&mut self) -> Result<(), String>;
    /// Runs with the output captured, shown only on failure.
    fn quiet(&mut self) -> Result<String, String>;
}

impl Run for Command {
    fn checked(&mut self) -> Result<(), String> {
        let status = self
            .status()
            .map_err(|err| format!("cannot run {self:?}: {err}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{self:?} exited with {status}"))
        }
    }

    fn quiet(&mut self) -> Result<String, String> {
        let output = self
            .output()
            .map_err(|err| format!("cannot run {self:?}: {err}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            Ok(stdout)
        } else {
            Err(format!(
                "{self:?} exited with {}: {}{}",
                output.status,
                stdout,
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }
}

/// The stack's host-side addresses.
struct Stack {
    http: Client,
    /// The fake's control API and Bot API.
    fake: String,
    /// The local-mode fake's control API and Bot API.
    fake_local: String,
    /// tgin's data plane (the gateway).
    gateway: String,
    /// tgin's admin plane (public metrics).
    admin: String,
}

impl Stack {
    fn discover() -> Result<Self, String> {
        let http = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|err| format!("cannot build the HTTP client: {err}"))?;
        Ok(Self {
            http,
            fake: published("fake", 8081)?,
            fake_local: published("fake-local", 8081)?,
            gateway: published("tgin", 8080)?,
            admin: published("tgin", 9090)?,
        })
    }
}

/// `http://<host:port>` a service's container port is published on.
fn published(service: &str, port: u16) -> Result<String, String> {
    let out = compose(&["port", service, &port.to_string()]).quiet()?;
    let address = out.trim();
    if address.is_empty() {
        return Err(format!("{service}:{port} is not published"));
    }
    Ok(format!("http://{address}"))
}

/// One journal entry of the fake (`GET /fake/calls`; field names from the
/// fake's `journal.rs`).
#[derive(Debug, Clone, Deserialize)]
struct Call {
    seq: u64,
    time_ms: u64,
    kind: String,
    method: String,
    content_type: Option<String>,
    transfer_encoding: Option<String>,
    #[serde(default)]
    args: BTreeMap<String, String>,
    #[serde(default)]
    files: Vec<FilePart>,
    status: Option<u16>,
    /// The update a webhook delivery carried.
    update_id: Option<i64>,
    /// A parameter named a local file (`file:…`).
    #[serde(default)]
    local_file: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct FilePart {
    name: String,
    filename: String,
    size: usize,
    fnv1a64: String,
}

/// A file stored at the fake for the file scenario.
struct StoredFile {
    file_id: String,
    file_unique_id: String,
    file_path: String,
}

impl Call {
    fn arg(&self, name: &str) -> Option<&str> {
        self.args.get(name).map(String::as_str)
    }

    /// A message to `chat` with `text`.
    fn is_text(&self, chat: i64, text: &str) -> bool {
        self.arg("chat_id") == Some(chat.to_string().as_str()) && self.arg("text") == Some(text)
    }

    fn ok(&self) -> bool {
        self.status == Some(200)
    }
}

/// A count checked when the scenario runs and again after the last one.
struct Exact {
    what: String,
    token: &'static str,
    method: &'static str,
    matches: Box<dyn Fn(&Call) -> bool>,
    allowed: RangeInclusive<usize>,
}

struct Suite {
    stack: Stack,
    exact: Vec<Exact>,
    next_chat: i64,
    next_message: Cell<i64>,
}

/// Waits for `probe` to give `Some`, polling every [`POLL`] for at most
/// [`BOUND`]; `what` names the awaited state in the error.
async fn eventually<T, F, Fut>(what: &str, mut probe: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>, String>>,
{
    let deadline = Instant::now() + BOUND;
    loop {
        if let Some(value) = probe().await? {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(format!("{what}: not within {} s", BOUND.as_secs()));
        }
        tokio::time::sleep(POLL).await;
    }
}

fn ensure(condition: bool, message: impl FnOnce() -> String) -> Result<(), String> {
    if condition { Ok(()) } else { Err(message()) }
}

/// The value of a metric series, summed over the series whose labels
/// include every pair of `labels` (0 when there is none).
fn metric(text: &str, name: &str, labels: &[(&str, &str)]) -> f64 {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let (series, value) = line.rsplit_once(' ')?;
            let (series_name, series_labels) = series.split_once('{').unwrap_or((series, ""));
            let wanted = series_name == name
                && labels
                    .iter()
                    .all(|(key, value)| series_labels.contains(&format!("{key}=\"{value}\"")));
            if wanted {
                value.parse::<f64>().ok()
            } else {
                None
            }
        })
        .sum()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The document the file scenario sends: deterministic, not all one byte.
fn document_bytes() -> Vec<u8> {
    (0..DOCUMENT_SIZE).map(|i| (i * 31 % 251) as u8).collect()
}

/// The 64-bit FNV-1a hash of `data` as 16 lowercase hex digits, as the
/// fake journals it for each file part (`fnv1a64`).
fn fnv1a64(data: &[u8]) -> String {
    let hash = data.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}

impl Suite {
    fn new(stack: Stack) -> Self {
        Self {
            stack,
            exact: Vec::new(),
            next_chat: 10_000,
            next_message: Cell::new(1),
        }
    }

    async fn scenarios(&mut self) -> Result<(), String> {
        let total = Instant::now();
        macro_rules! scenario {
            ($name:expr, $body:expr) => {{
                let started = Instant::now();
                let name = $name;
                match $body.await {
                    Ok(()) => println!("ok   {name} ({:.2} s)", started.elapsed().as_secs_f64()),
                    Err(err) => {
                        println!("FAIL {name} ({:.2} s)", started.elapsed().as_secs_f64());
                        return Err(format!("{name}: {err}"));
                    }
                }
            }};
        }
        for f in &FRAMEWORKS {
            scenario!(format!("1 polling echo: {}", f.name), self.polling_echo(f));
        }
        for f in &FRAMEWORKS {
            scenario!(format!("2 restart confirm: {}", f.name), self.restart(f));
        }
        for f in &FRAMEWORKS {
            scenario!(format!("3 files: {}", f.name), self.files(f));
        }
        for f in &FRAMEWORKS {
            scenario!(
                format!("4 webhook delivery: {}", f.name),
                self.webhook_delivery(f)
            );
        }
        scenario!(
            "5 webhook reply: aiogram multipart",
            self.reply(framework("aiogram"))
        );
        scenario!(
            "5 webhook reply: grammY JSON",
            self.reply(framework("grammy"))
        );
        scenario!(
            "5 webhook reply: Telegraf sendChatAction",
            self.reply(framework("telegraf"))
        );
        scenario!("5 webhook reply: getMe refused", self.refused_reply());
        for f in &FRAMEWORKS {
            scenario!(format!("6 429 smoothed: {}", f.name), self.flood(f));
        }
        scenario!("7 shaping: 5 messages to one chat", self.shaping());
        scenario!(
            "8 self-hosted: files from disk (python-telegram-bot)",
            self.local_files()
        );
        scenario!(
            "8 self-hosted: webhook over http (grammY)",
            self.local_webhook()
        );
        scenario!(
            "8 self-hosted: file: references refused",
            self.local_refused()
        );
        scenario!(
            "final: exact counts, close/logOut refused, no downstream setWebhook/close/logOut",
            self.finally()
        );
        println!("e2e: scenarios took {:.1} s", total.elapsed().as_secs_f64());
        Ok(())
    }

    // --- scenarios -------------------------------------------------------

    /// Scenario 1: an injected text update gives exactly one `sendMessage` with the
    /// same chat and text; nothing the framework calls to set up reaches
    /// Telegram (setWebhook/close/logOut never; deleteWebhook only tgin's
    /// own one at start).
    async fn polling_echo(&mut self, f: &'static Framework) -> Result<(), String> {
        let chat = self.chat();
        let text = format!("echo {}", f.name);
        self.inject(f.poll_token, self.text_update(chat, &text))
            .await?;
        self.echo(f.poll_token, "sendMessage", chat, &text, 1..=1)
            .await?;
        for method in ["setWebhook", "close", "logOut"] {
            let calls = self.calls(f.poll_token, method, "").await?;
            ensure(calls.is_empty(), || {
                format!("{method} reached Telegram: {calls:?}")
            })?;
        }
        let deletes = self.calls(f.poll_token, "deleteWebhook", "").await?;
        ensure(deletes.len() <= 1, || {
            format!("deleteWebhook reached Telegram beyond tgin's own start-up call: {deletes:?}")
        })
    }

    /// Scenario 2: a polling bot stopped and started again: no update it handled
    /// before the stop is echoed twice (aiogram, which does not confirm on
    /// stop, may get its last batch again). grammY and Telegraf hold the
    /// `before restart` handler 3 s after its echo, so the stop lands before
    /// their next poll and their shutdown confirm (`getUpdates {offset,
    /// limit: 1}`) is the only ack; that confirm must return at once, so
    /// their stop is timed. python-telegram-bot polls again as soon as a
    /// batch arrives, so its shutdown confirm (`timeout=0`) is never the
    /// only ack here; only the in-process test
    /// `get_updates_consumer_scoped_serves_queue` covers it.
    async fn restart(&mut self, f: &'static Framework) -> Result<(), String> {
        let chat = self.chat();
        let before = format!("before restart {}", f.name);
        let allowed = if f.confirms_on_stop { 1..=1 } else { 1..=2 };
        self.inject(f.poll_token, self.text_update(chat, &before))
            .await?;
        self.echo(f.poll_token, "sendMessage", chat, &before, allowed.clone())
            .await?;
        let service = f.poll();
        let stopping = std::time::Instant::now();
        compose(&["stop", "-t", "30", &service]).quiet()?;
        let stopped = stopping.elapsed();
        // The 3 s hold plus margin; a shutdown getUpdates that tgin parked
        // instead of answering at once runs into the 30 s kill.
        ensure(
            !matches!(f.name, "grammy" | "telegraf")
                || stopped < std::time::Duration::from_secs(15),
            || {
                format!(
                    "{service} took {stopped:?} to stop: its shutdown getUpdates did not return at once"
                )
            },
        )?;
        compose(&["start", &service]).quiet()?;
        let after = format!("after restart {}", f.name);
        self.inject(f.poll_token, self.text_update(chat, &after))
            .await?;
        self.echo(f.poll_token, "sendMessage", chat, &after, 1..=1)
            .await?;
        // Any redelivery of `before` comes with or ahead of `after`.
        let again = self.texts(f.poll_token, chat, &before).await?;
        ensure(allowed.contains(&again.len()), || {
            format!(
                "`{before}` was echoed {} times after the restart",
                again.len()
            )
        })
    }

    /// Scenario 3: a document update: the bot calls getFile, downloads through the
    /// gateway's file route (PTB with `%3A` for the token's colon) and sends
    /// the bytes back with sendDocument, as multipart, chunked where the
    /// framework chunks. The echoed part's checksum equals the original's, and
    /// the runner's own download through the file route is byte-exact.
    async fn files(&mut self, f: &'static Framework) -> Result<(), String> {
        let chat = self.chat();
        let filename = format!("e2e-{}.bin", f.name);
        let data = document_bytes();
        let checksum = fnv1a64(&data);
        let stored = self
            .store_file(f.poll_token, chat, &filename, &data)
            .await?;
        let (file_id, file_unique_id) = (stored.file_id.clone(), stored.file_unique_id);
        let mark = self.mark(f.poll_token).await?;
        let mut update = self.message(chat);
        update["message"]["document"] = json!({
            "file_id": file_id,
            "file_unique_id": file_unique_id,
            "file_name": filename,
            "file_size": data.len(),
        });
        self.inject(f.poll_token, update).await?;

        let echo_name = format!("echo-{filename}");
        let size = data.len();
        let sent = {
            let echo_name = echo_name.clone();
            move |call: &Call| {
                call.ok()
                    && call.arg("chat_id") == Some(chat.to_string().as_str())
                    && call.files.iter().any(|part| part.filename == echo_name)
            }
        };
        let uploads = self
            .exactly(
                format!("sendDocument of {echo_name}"),
                f.poll_token,
                "sendDocument",
                sent,
                1..=1,
            )
            .await?;
        let upload = &uploads[0];
        let part = upload.files.iter().find(|p| p.filename == echo_name);
        ensure(
            part.is_some_and(|p| p.size == size && p.fnv1a64 == checksum),
            || format!("sendDocument carried {part:?}, not {size} bytes with fnv1a64 {checksum}"),
        )?;
        ensure(
            upload
                .content_type
                .as_deref()
                .is_some_and(|ct| ct.starts_with("multipart/form-data")),
            || format!("sendDocument is not multipart: {upload:?}"),
        )?;
        let chunked = upload.transfer_encoding.as_deref() == Some("chunked");
        ensure(chunked == f.chunked_upload, || {
            format!(
                "sendDocument transfer-encoding {:?}, expected chunked: {}",
                upload.transfer_encoding, f.chunked_upload
            )
        })?;

        let get_files = self.calls(f.poll_token, "getFile", "").await?;
        ensure(
            get_files
                .iter()
                .any(|c| c.seq > mark && c.ok() && c.arg("file_id") == Some(file_id.as_str())),
            || format!("no getFile for {file_id}: {get_files:?}"),
        )?;
        let downloads = self.calls(f.poll_token, "", "file").await?;
        ensure(downloads.iter().any(|c| c.seq > mark && c.ok()), || {
            format!("no download after the update: {downloads:?}")
        })?;
        let bot = f.poll();
        let through_gateway = self
            .metric(
                "tgin_gateway_requests_total",
                &[("bot", &bot), ("method", "file"), ("outcome", "ok")],
            )
            .await?;
        ensure(through_gateway >= 1.0, || {
            format!("tgin counted no file download for {bot}")
        })?;
        if f.name == "ptb" {
            let logs = compose(&["logs", "--no-color", &bot]).quiet()?;
            ensure(
                logs.contains("encodes the token's colon as %3A: True"),
                || "PTB's download path did not carry %3A".to_owned(),
            )?;
        }
        let url = format!(
            "{}/c/{bot}/file/bot{}/{}",
            self.stack.gateway, f.poll_token, stored.file_path
        );
        let fetched = self.get_bytes(&url).await?;
        ensure(fetched == data, || {
            format!(
                "the file route served {} bytes (fnv1a64 {}), not the {size} stored",
                fetched.len(),
                fnv1a64(&fetched)
            )
        })
    }

    /// Scenario 4: updates reach the webhook bots. aiogram, PTB and grammY registered
    /// their endpoint with setWebhook on the scoped base (the call never
    /// reached Telegram); Telegraf gets tgin's configured endpoint. Each bot
    /// checks the secret header (the frameworks answer 401/403 without it)
    /// and, except PTB, tgin's `Via` (400 without it), so an echo proves
    /// both arrived.
    async fn webhook_delivery(&mut self, f: &'static Framework) -> Result<(), String> {
        let hook = f.hook();
        let expected = if f.registers_hook {
            format!("http://{hook}:8081/hook")
        } else {
            String::new()
        };
        let info_url = format!(
            "{}/c/{hook}/bot{}/getWebhookInfo",
            self.stack.gateway, f.hook_token
        );
        let url = eventually(
            &format!("{hook}'s getWebhookInfo url {expected:?}"),
            || async {
                let info = self.get_json(&info_url).await?;
                let url = info["result"]["url"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                Ok((url == expected).then_some(url))
            },
        )
        .await?;
        ensure(url == expected, || format!("getWebhookInfo url {url:?}"))?;
        let set = self.calls(f.hook_token, "setWebhook", "").await?;
        ensure(set.is_empty(), || {
            format!("setWebhook reached Telegram: {set:?}")
        })?;

        let chat = self.chat();
        let text = format!("hook {}", f.name);
        self.inject(f.hook_token, self.text_update(chat, &text))
            .await?;
        self.echo(f.hook_token, "sendMessage", chat, &text, 1..=1)
            .await?;
        let endpoint = if f.registers_hook { "runtime" } else { "0" };
        // tgin counts the delivery once the reply ran and the update is
        // acked, which can trail the echo.
        eventually(
            &format!("a delivery counted on {hook}'s {endpoint} endpoint"),
            || async {
                let delivered = self
                    .metric(
                        "tgin_delivery_attempts_total",
                        &[
                            ("consumer", &hook),
                            ("endpoint", endpoint),
                            ("outcome", "delivered"),
                        ],
                    )
                    .await?;
                Ok((delivered >= 1.0).then_some(()))
            },
        )
        .await
    }

    /// Scenario 5: a webhook reply is executed once: aiogram's as multipart, grammY's
    /// as JSON, Telegraf's sendChatAction (its echo goes over HTTP).
    async fn reply(&mut self, f: &'static Framework) -> Result<(), String> {
        let hook = f.hook();
        let before = self.replies(&hook, "executed").await?;
        let chat = self.chat();
        let text = format!("reply {}", f.name);
        self.inject(f.hook_token, self.text_update(chat, &text))
            .await?;
        if f.name == "telegraf" {
            let action = self
                .exactly(
                    format!("sendChatAction typing to {chat}"),
                    f.hook_token,
                    "sendChatAction",
                    move |c: &Call| {
                        c.ok()
                            && c.arg("chat_id") == Some(chat.to_string().as_str())
                            && c.arg("action") == Some("typing")
                    },
                    1..=1,
                )
                .await?;
            ensure(action[0].arg("method") == Some("sendChatAction"), || {
                format!("sendChatAction did not come from a reply: {:?}", action[0])
            })?;
            let echo = self
                .echo(f.hook_token, "sendMessage", chat, &text, 1..=1)
                .await?;
            ensure(echo[0].arg("method").is_none(), || {
                format!("Telegraf's sendMessage came from a reply: {:?}", echo[0])
            })?;
        } else {
            let echo = self
                .echo(f.hook_token, "sendMessage", chat, &text, 1..=1)
                .await?;
            let call = &echo[0];
            ensure(call.arg("method") == Some("sendMessage"), || {
                format!("the sendMessage did not come from a reply: {call:?}")
            })?;
            let content_type = call.content_type.as_deref().unwrap_or_default();
            let expected = if f.name == "aiogram" {
                "multipart/form-data"
            } else {
                "application/json"
            };
            ensure(content_type.starts_with(expected), || {
                format!("the reply's content type is {content_type:?}, not {expected}")
            })?;
        }
        eventually(&format!("{hook} counts the reply executed"), || async {
            Ok((self.replies(&hook, "executed").await? > before).then_some(()))
        })
        .await
    }

    /// Scenario 5: a webhook reply naming getMe is refused: aiogram sends a control
    /// message over HTTP, then replies with getMe.
    async fn refused_reply(&mut self) -> Result<(), String> {
        let f = framework("aiogram");
        let hook = f.hook();
        let before = self.replies(&hook, "refused").await?;
        let mark = self.mark(f.hook_token).await?;
        let chat = self.chat();
        self.inject(f.hook_token, self.text_update(chat, "getme"))
            .await?;
        self.echo(f.hook_token, "sendMessage", chat, "getme-control", 1..=1)
            .await?;
        eventually(
            &format!("{hook} counts the getMe reply refused"),
            || async { Ok((self.replies(&hook, "refused").await? > before).then_some(())) },
        )
        .await?;
        let get_me: Vec<Call> = self
            .calls(f.hook_token, "getMe", "")
            .await?
            .into_iter()
            .filter(|c| c.seq > mark)
            .collect();
        ensure(get_me.is_empty(), || {
            format!("the refused getMe reached Telegram: {get_me:?}")
        })
    }

    /// Scenario 6: one injected 429 (`retry_after` 1) on sendMessage: tgin waits it
    /// out and retries once, so exactly one sendMessage succeeds, ≥ 1 s after
    /// the refused one, and the framework never sees the 429.
    async fn flood(&mut self, f: &'static Framework) -> Result<(), String> {
        let bot = f.poll();
        let retries = self
            .metric("tgin_gateway_retries_total", &[("bot", &bot)])
            .await?;
        self.post_json(
            &format!("{}/fake/faults", self.stack.fake),
            &json!({
                "token": f.poll_token,
                "method": "sendMessage",
                "status": 429,
                "retry_after": 1,
                "count": 1,
            }),
        )
        .await?;
        let chat = self.chat();
        let text = format!("flood {}", f.name);
        self.inject(f.poll_token, self.text_update(chat, &text))
            .await?;
        self.echo(f.poll_token, "sendMessage", chat, &text, 1..=1)
            .await?;
        let attempts = self.texts(f.poll_token, chat, &text).await?;
        let statuses: Vec<Option<u16>> = attempts.iter().map(|c| c.status).collect();
        ensure(statuses == [Some(429), Some(200)], || {
            format!("sendMessage attempts answered {statuses:?}, not [429, 200]")
        })?;
        let total = text.clone();
        self.expect_exact(
            format!("sendMessage `{text}` attempts"),
            f.poll_token,
            "sendMessage",
            move |c: &Call| c.is_text(chat, &total),
            2..=2,
        );
        let gap = attempts[1].time_ms.saturating_sub(attempts[0].time_ms);
        ensure(gap >= 1000, || {
            format!("the retry came {gap} ms after the 429, before its retry_after")
        })?;
        let after = self
            .metric("tgin_gateway_retries_total", &[("bot", &bot)])
            .await?;
        ensure(after > retries, || {
            format!("tgin counted no retry for {bot}")
        })
    }

    /// Scenario 7: shaping: grammY sends 5 messages to one chat at once. Under
    /// the default per-chat limit (burst 3, 1/s) tgin delays exactly the 4th
    /// and 5th (the gate: its own count), and the fake sees the 1/s pacing:
    /// the last arrival 1.8 s to 3 s after the first (2 s at tgin's limiter,
    /// so an over-delay fails too), and the 4th at least 0.8 s after the
    /// first and 0.8 s before the last. The arrival bounds leave room for
    /// latency differences between the messages.
    async fn shaping(&mut self) -> Result<(), String> {
        let f = framework("grammy");
        let bot = f.poll();
        let delayed = [
            ("bot", bot.as_str()),
            ("scope", "chat"),
            ("action", "delayed"),
        ];
        let before = self
            .metric("tgin_gateway_ratelimited_total", &delayed)
            .await?;
        let chat = self.chat();
        self.inject(f.poll_token, self.text_update(chat, "burst"))
            .await?;
        let burst = move |c: &Call| {
            c.ok()
                && c.arg("chat_id") == Some(chat.to_string().as_str())
                && c.arg("text").is_some_and(|t| t.starts_with("burst "))
        };
        let sent = self
            .exactly(
                format!("burst to {chat}"),
                f.poll_token,
                "sendMessage",
                burst,
                5..=5,
            )
            .await?;
        let after = self
            .metric("tgin_gateway_ratelimited_total", &delayed)
            .await?;
        ensure(after - before == 2.0, || {
            format!("tgin delayed {} of the 5 messages, not 2", after - before)
        })?;
        let mut arrivals: Vec<u64> = sent.iter().map(|c| c.time_ms).collect();
        arrivals.sort_unstable();
        let (first, fourth, last) = (arrivals[0], arrivals[3], arrivals[4]);
        println!(
            "     burst arrivals +{} +{} +{} +{} ms",
            arrivals[1] - first,
            arrivals[2] - first,
            fourth - first,
            last - first
        );
        ensure((1800..=3000).contains(&(last - first)), || {
            format!(
                "the 5th message came {} ms after the first, not within 1.8 s to 3 s",
                last - first
            )
        })?;
        ensure(fourth - first >= 800 && last - fourth >= 800, || {
            format!(
                "the 4th message came {} ms after the first and {} ms before the 5th, not 1/s",
                fourth - first,
                last - fourth
            )
        })
    }

    /// Scenario 8, files: a document stored at the local fake (written under
    /// /srv/tba/<token>/, its getFile answering the absolute path) reaches
    /// the polling bot. tgin's getFile answers the path relative to the
    /// bot's directory, so the bot downloads from tgin's file route (PTB
    /// with `%3A` for the token's colon) and sends the bytes back with
    /// sendDocument. tgin serves the file from the shared volume: it never
    /// times a file request against the server
    /// (`tgin_gateway_upstream_seconds`, which every proxied request
    /// observes), and the local fake's file route (404 in local mode) sees
    /// no request. The runner's own getFile and downloads, on the
    /// consumer-scoped and the proxy-only base, give the relative path and
    /// the exact bytes.
    async fn local_files(&mut self) -> Result<(), String> {
        let (bot, token) = (LOCAL_POLL, LOCAL_POLL_TOKEN);
        let chat = self.chat();
        let filename = "e2e-local.bin";
        let data = document_bytes();
        let checksum = fnv1a64(&data);
        let stored = self.store_file(token, chat, filename, &data).await?;
        // The fake's paths hold the token: checked, never printed. On
        // Linux the bot's directory keeps the token's ':'.
        let relative = [token.to_owned(), token.replace(':', "~")]
            .iter()
            .find_map(|dir| {
                stored
                    .file_path
                    .strip_prefix(&format!("{LOCAL_FILES_DIR}/{dir}/"))
                    .map(str::to_owned)
            })
            .ok_or_else(|| {
                format!("the local fake's getFile path is not under the bot's directory in {LOCAL_FILES_DIR}")
            })?;
        let mark = self.mark(token).await?;
        let mut update = self.message(chat);
        update["message"]["document"] = json!({
            "file_id": stored.file_id,
            "file_unique_id": stored.file_unique_id,
            "file_name": filename,
            "file_size": data.len(),
        });
        self.inject(token, update).await?;

        let echo_name = format!("echo-{filename}");
        let size = data.len();
        let sent = {
            let echo_name = echo_name.clone();
            move |call: &Call| {
                call.ok()
                    && call.arg("chat_id") == Some(chat.to_string().as_str())
                    && call.files.iter().any(|part| part.filename == echo_name)
            }
        };
        let uploads = self
            .exactly(
                format!("sendDocument of {echo_name}"),
                token,
                "sendDocument",
                sent,
                1..=1,
            )
            .await?;
        let part = uploads[0].files.iter().find(|p| p.filename == echo_name);
        ensure(
            part.is_some_and(|p| p.size == size && p.fnv1a64 == checksum),
            || format!("sendDocument carried {part:?}, not {size} bytes with fnv1a64 {checksum}"),
        )?;
        let file_id = stored.file_id.as_str();
        let get_files = self.calls(token, "getFile", "").await?;
        ensure(
            get_files
                .iter()
                .any(|c| c.seq > mark && c.ok() && c.arg("file_id") == Some(file_id)),
            || format!("no getFile for {file_id}: {get_files:?}"),
        )?;
        let logs = compose(&["logs", "--no-color", bot]).quiet()?;
        ensure(
            logs.contains("encodes the token's colon as %3A: True"),
            || "PTB's download path did not carry %3A".to_owned(),
        )?;

        let gateway = self.stack.gateway.clone();
        for (base, scope) in [
            (format!("{gateway}/c/{bot}"), "consumer-scoped"),
            (gateway, "proxy-only"),
        ] {
            let info = self
                .post_json(
                    &format!("{base}/bot{token}/getFile"),
                    &json!({"file_id": file_id}),
                )
                .await?;
            ensure(
                info["result"]["file_path"].as_str() == Some(relative.as_str()),
                || {
                    format!(
                        "getFile on the {scope} base did not answer the path relative to the bot's directory"
                    )
                },
            )?;
            let fetched = self
                .get_bytes(&format!("{base}/file/bot{token}/{relative}"))
                .await?;
            ensure(fnv1a64(&fetched) == checksum && fetched == data, || {
                format!(
                    "the file route on the {scope} base served {} bytes (fnv1a64 {}), not the {size} stored",
                    fetched.len(),
                    fnv1a64(&fetched)
                )
            })?;
        }
        // One snapshot for both series: the 3 downloads counted, and not
        // one of them timed against the server.
        let (from_disk, upstream) =
            eventually(&format!("tgin counts {bot}'s 3 file downloads"), || async {
                let text = self.metrics().await?;
                let ok = metric(
                    &text,
                    "tgin_gateway_requests_total",
                    &[("bot", bot), ("method", "file"), ("outcome", "ok")],
                );
                let upstream = metric(
                    &text,
                    "tgin_gateway_upstream_seconds_count",
                    &[("bot", bot), ("method", "file")],
                );
                Ok((ok >= 3.0).then_some((ok, upstream)))
            })
            .await?;
        ensure(upstream == 0.0, || {
            format!(
                "tgin timed {upstream} of {bot}'s {from_disk} file downloads against the Bot API server instead of serving them from disk"
            )
        })?;
        // File paths hold the token: counted, never printed.
        let proxied = self.calls(token, "", "file").await?.len();
        ensure(proxied == 0, || {
            format!(
                "{proxied} downloads reached the local fake's file route instead of tgin's disk"
            )
        })
    }

    /// Scenario 8, webhook: tgin registered its own data plane at the local
    /// fake over plain http (`http://tgin:8080/webhook/local-hook`, with a
    /// secret), and grammY's setWebhook stayed in tgin as the consumer's
    /// runtime endpoint. An update injected at the local fake is POSTed to
    /// tgin over http (the fake journals the delivery answered 200), pushed
    /// to grammY, and its webhook reply (JSON) is executed against the local
    /// fake.
    async fn local_webhook(&mut self) -> Result<(), String> {
        let (hook, token) = (LOCAL_HOOK, LOCAL_HOOK_TOKEN);
        let registered = eventually("tgin's setWebhook at the local fake", || async {
            Ok(self
                .calls(token, "setWebhook", "")
                .await?
                .into_iter()
                .find(Call::ok))
        })
        .await?;
        ensure(registered.arg("url") == Some(LOCAL_HOOK_URL), || {
            format!(
                "tgin registered the webhook URL {:?}, not {LOCAL_HOOK_URL}",
                registered.arg("url")
            )
        })?;
        ensure(
            registered
                .arg("secret_token")
                .is_some_and(|s| !s.is_empty()),
            || "tgin registered its webhook without a secret_token".to_owned(),
        )?;
        let info_url = format!("{}/c/{hook}/bot{token}/getWebhookInfo", self.stack.gateway);
        let expected = format!("http://{hook}:8081/hook");
        eventually(
            &format!("{hook}'s getWebhookInfo url {expected:?}"),
            || async {
                let info = self.get_json(&info_url).await?;
                Ok((info["result"]["url"].as_str() == Some(expected.as_str())).then_some(()))
            },
        )
        .await?;

        let chat = self.chat();
        let text = "hook local";
        let update_id = self.inject(token, self.text_update(chat, text)).await?;
        let echo = self.echo(token, "sendMessage", chat, text, 1..=1).await?;
        ensure(echo[0].arg("method") == Some("sendMessage"), || {
            format!(
                "grammY's sendMessage did not come from a webhook reply: {:?}",
                echo[0]
            )
        })?;
        let delivered = eventually(
            &format!("the local fake's delivery of update {update_id} answered 200"),
            || async {
                let deliveries: Vec<Call> = self
                    .calls(token, "", "webhook")
                    .await?
                    .into_iter()
                    .filter(|c| c.update_id == Some(update_id) && c.status.is_some())
                    .collect();
                Ok((!deliveries.is_empty()).then_some(deliveries))
            },
        )
        .await?;
        let statuses: Vec<Option<u16>> = delivered.iter().map(|c| c.status).collect();
        ensure(statuses == [Some(200)], || {
            format!(
                "the local fake's deliveries of update {update_id} were answered {statuses:?}, not once 200"
            )
        })?;
        eventually(
            &format!("a delivery counted on {hook}'s runtime endpoint"),
            || async {
                let delivered = self
                    .metric(
                        "tgin_delivery_attempts_total",
                        &[
                            ("consumer", hook),
                            ("endpoint", "runtime"),
                            ("outcome", "delivered"),
                        ],
                    )
                    .await?;
                Ok((delivered >= 1.0).then_some(()))
            },
        )
        .await?;
        eventually(&format!("{hook} counts the reply executed"), || async {
            Ok((self.replies(hook, "executed").await? >= 1.0).then_some(()))
        })
        .await
    }

    /// Scenario 8, `file:` references: the runner's sendDocument naming a
    /// local file, as JSON on the consumer-scoped base and as a multipart
    /// text field on the proxy-only base, gets tgin's own 400; the local
    /// fake (which would accept it) journals no call naming a local file
    /// and no sendDocument to the chat.
    async fn local_refused(&mut self) -> Result<(), String> {
        let (bot, token) = (LOCAL_POLL, LOCAL_POLL_TOKEN);
        let chat = self.chat();
        let gateway = self.stack.gateway.clone();
        let json_body = self
            .stack
            .http
            .post(format!("{gateway}/c/{bot}/bot{token}/sendDocument"))
            .json(&json!({"chat_id": chat, "document": LOCAL_FILE_REFERENCE}));
        self.refused(
            "a JSON sendDocument naming a local file (consumer-scoped base)",
            json_body,
            LOCAL_FILE_REFUSED,
        )
        .await?;
        let form = multipart::Form::new()
            .text("chat_id", chat.to_string())
            .text("document", LOCAL_FILE_REFERENCE);
        let multipart_body = self
            .stack
            .http
            .post(format!("{gateway}/bot{token}/sendDocument"))
            .multipart(form);
        self.refused(
            "a multipart sendDocument naming a local file (proxy-only base)",
            multipart_body,
            LOCAL_FILE_REFUSED,
        )
        .await?;
        let named = self
            .local_calls("", "", "")
            .await?
            .into_iter()
            .filter(|c| c.local_file)
            .count();
        ensure(named == 0, || {
            format!("{named} calls named a local file at the local fake")
        })?;
        let to_chat = move |c: &Call| c.arg("chat_id") == Some(chat.to_string().as_str());
        let reached = self
            .calls(token, "sendDocument", "")
            .await?
            .into_iter()
            .filter(to_chat)
            .count();
        ensure(reached == 0, || {
            format!("{reached} sendDocument calls to {chat} reached the local fake")
        })?;
        self.expect_exact(
            format!("sendDocument to {chat} at the local fake"),
            token,
            "sendDocument",
            to_chat,
            0..=0,
        );
        Ok(())
    }

    /// Every exact count still holds. The runner sends close and logOut for
    /// a bot of each fake on a consumer-scoped base and on the proxy-only
    /// base, and tgin answers each itself with its 400. No downstream ever
    /// reached setWebhook, close or logOut on either fake: tgin calls none
    /// of them, except its own setWebhook for the local webhook bot, the
    /// local fake's only one. Each bot's only getMe and deleteWebhook are
    /// tgin's own, made once at start (a webhook bot gets no deleteWebhook):
    /// the frameworks' startup calls, restarts included, stay local. The
    /// local fake never saw a local file reference.
    async fn finally(&mut self) -> Result<(), String> {
        for exact in &self.exact {
            let n = self
                .calls(exact.token, exact.method, "")
                .await?
                .iter()
                .filter(|c| (exact.matches)(c))
                .count();
            ensure(exact.allowed.contains(&n), || {
                format!(
                    "{}: {n} at the end, expected {:?}",
                    exact.what, exact.allowed
                )
            })?;
        }
        let gateway = self.stack.gateway.clone();
        let f = &FRAMEWORKS[0];
        for (bot, token) in [
            (f.poll(), f.poll_token),
            (LOCAL_POLL.to_owned(), LOCAL_POLL_TOKEN),
        ] {
            for (base, scope) in [
                (format!("{gateway}/c/{bot}"), "consumer-scoped"),
                (gateway.clone(), "proxy-only"),
            ] {
                for method in ["close", "logOut"] {
                    let request = self.stack.http.post(format!("{base}/bot{token}/{method}"));
                    self.refused(
                        &format!("{method} for {bot} on the {scope} base"),
                        request,
                        LOG_OUT_DISABLED,
                    )
                    .await?;
                }
            }
        }
        for method in ["setWebhook", "close", "logOut"] {
            let calls = self.calls("", method, "").await?;
            ensure(calls.is_empty(), || {
                format!("{method} reached Telegram: {calls:?}")
            })?;
        }
        for method in ["close", "logOut"] {
            let calls = self.local_calls("", method, "").await?;
            ensure(calls.is_empty(), || {
                format!("{method} reached the local fake: {calls:?}")
            })?;
        }
        let set = self.local_calls("", "setWebhook", "").await?.len();
        let own = self.calls(LOCAL_HOOK_TOKEN, "setWebhook", "").await?;
        ensure(
            set == 1 && own.len() == 1 && own[0].arg("url") == Some(LOCAL_HOOK_URL),
            || format!("the local fake saw {set} setWebhook calls, not only tgin's own"),
        )?;
        for f in &FRAMEWORKS {
            for (bot, token) in [(f.poll(), f.poll_token), (f.hook(), f.hook_token)] {
                for method in ["getMe", "deleteWebhook"] {
                    let n = self.calls(token, method, "").await?.len();
                    ensure(n == 1, || {
                        format!("{bot}: {n} {method} calls reached Telegram, not only tgin's own")
                    })?;
                }
            }
        }
        for (bot, token, deletes) in [
            (LOCAL_POLL, LOCAL_POLL_TOKEN, 1),
            (LOCAL_HOOK, LOCAL_HOOK_TOKEN, 0),
        ] {
            for (method, expected) in [("getMe", 1), ("deleteWebhook", deletes)] {
                let n = self.calls(token, method, "").await?.len();
                ensure(n == expected, || {
                    format!(
                        "{bot}: {n} {method} calls reached the local fake, not tgin's own {expected}"
                    )
                })?;
            }
        }
        let named = self
            .local_calls("", "", "")
            .await?
            .into_iter()
            .filter(|c| c.local_file)
            .count();
        ensure(named == 0, || {
            format!("{named} calls named a local file at the local fake")
        })
    }

    // --- helpers ---------------------------------------------------------

    fn chat(&mut self) -> i64 {
        self.next_chat += 1;
        self.next_chat
    }

    /// A private-chat message update without content.
    fn message(&self, chat: i64) -> Value {
        self.next_message.set(self.next_message.get() + 1);
        json!({
            "message": {
                "message_id": self.next_message.get(),
                "date": unix_now(),
                "chat": {"id": chat, "type": "private", "first_name": "E2E"},
                "from": {"id": chat, "is_bot": false, "first_name": "E2E"},
            }
        })
    }

    fn text_update(&self, chat: i64, text: &str) -> Value {
        let mut update = self.message(chat);
        update["message"]["text"] = json!(text);
        update
    }

    /// The fake that serves `token`'s bot: the local one for the
    /// self-hosted scenario's bots.
    fn fake_of(&self, token: &str) -> &str {
        if is_local(token) {
            &self.stack.fake_local
        } else {
            &self.stack.fake
        }
    }

    /// Queues `update` at the bot's fake; returns its `update_id`.
    async fn inject(&self, token: &str, update: Value) -> Result<i64, String> {
        let url = format!("{}/fake/updates/{token}", self.fake_of(token));
        let answer = self.post_json(&url, &update).await?;
        answer["update_id"]
            .as_i64()
            .ok_or_else(|| format!("the fake assigned no update_id: {answer}"))
    }

    /// Stores a file at the bot's fake (its own sendDocument with the bot's
    /// token) and reads its `file_path` with the fake's getFile; both happen
    /// before the scenario's journal mark.
    async fn store_file(
        &self,
        token: &str,
        chat: i64,
        filename: &str,
        data: &[u8],
    ) -> Result<StoredFile, String> {
        let fake = self.fake_of(token);
        let url = format!("{fake}/bot{token}/sendDocument");
        let part = multipart::Part::bytes(data.to_vec()).file_name(filename.to_owned());
        let form = multipart::Form::new()
            .text("chat_id", chat.to_string())
            .part("document", part);
        let response = self
            .stack
            .http
            .post(&url)
            .multipart(form)
            .send()
            .await
            .map_err(|err| format!("storing {filename}: {}", err.without_url()))?;
        let body: Value = response
            .json()
            .await
            .map_err(|err| format!("storing {filename}: {}", err.without_url()))?;
        let document = &body["result"]["document"];
        let (Some(file_id), Some(file_unique_id)) = (
            document["file_id"].as_str(),
            document["file_unique_id"].as_str(),
        ) else {
            return Err(format!("the fake stored no document: {body}"));
        };
        let info = self
            .post_json(
                &format!("{fake}/bot{token}/getFile"),
                &json!({"file_id": file_id}),
            )
            .await?;
        let Some(file_path) = info["result"]["file_path"].as_str() else {
            return Err(format!("the fake's getFile gave no file_path: {info}"));
        };
        Ok(StoredFile {
            file_id: file_id.to_owned(),
            file_unique_id: file_unique_id.to_owned(),
            file_path: file_path.to_owned(),
        })
    }

    /// The journal entries of `token` (all bots when empty) at its fake (the
    /// cloud one when empty), filtered by `method` and `kind` when not
    /// empty.
    async fn calls(&self, token: &str, method: &str, kind: &str) -> Result<Vec<Call>, String> {
        self.journal(self.fake_of(token), token, method, kind).await
    }

    /// As [`Self::calls`], at the local fake.
    async fn local_calls(
        &self,
        token: &str,
        method: &str,
        kind: &str,
    ) -> Result<Vec<Call>, String> {
        self.journal(&self.stack.fake_local, token, method, kind)
            .await
    }

    async fn journal(
        &self,
        fake: &str,
        token: &str,
        method: &str,
        kind: &str,
    ) -> Result<Vec<Call>, String> {
        let url = Url::parse_with_params(
            &format!("{fake}/fake/calls"),
            [("token", token), ("method", method), ("kind", kind)],
        )
        .map_err(|err| format!("journal URL: {err}"))?;
        let response = self
            .stack
            .http
            .get(url)
            .send()
            .await
            .map_err(|err| format!("reading the journal: {}", err.without_url()))?;
        response
            .json()
            .await
            .map_err(|err| format!("reading the journal: {}", err.without_url()))
    }

    /// The highest journal sequence number of `token` so far.
    async fn mark(&self, token: &str) -> Result<u64, String> {
        Ok(self
            .calls(token, "", "")
            .await?
            .iter()
            .map(|c| c.seq)
            .max()
            .unwrap_or_default())
    }

    /// The sendMessage calls of `token` to `chat` with `text`, any status.
    async fn texts(&self, token: &str, chat: i64, text: &str) -> Result<Vec<Call>, String> {
        Ok(self
            .calls(token, "sendMessage", "")
            .await?
            .into_iter()
            .filter(|c| c.is_text(chat, text))
            .collect())
    }

    /// Waits for `method` calls of `token` with `chat` and `text` answered
    /// 200, and records that their count stays within `allowed`.
    async fn echo(
        &mut self,
        token: &'static str,
        method: &'static str,
        chat: i64,
        text: &str,
        allowed: RangeInclusive<usize>,
    ) -> Result<Vec<Call>, String> {
        let wanted = text.to_owned();
        self.exactly(
            format!("{method} `{text}` to {chat}"),
            token,
            method,
            move |c: &Call| c.ok() && c.is_text(chat, &wanted),
            allowed,
        )
        .await
    }

    /// Waits until the `method` calls of `token` that `matches` reach the
    /// start of `allowed`, fails if they are past its end, and records the
    /// count for the final check.
    async fn exactly<M>(
        &mut self,
        what: String,
        token: &'static str,
        method: &'static str,
        matches: M,
        allowed: RangeInclusive<usize>,
    ) -> Result<Vec<Call>, String>
    where
        M: Fn(&Call) -> bool + Clone + 'static,
    {
        let least = *allowed.start();
        let this = &*self;
        let found = eventually(&format!("{what} ({least})"), || {
            let matches = matches.clone();
            async move {
                let found: Vec<Call> = this
                    .calls(token, method, "")
                    .await?
                    .into_iter()
                    .filter(|c| matches(c))
                    .collect();
                Ok((found.len() >= least).then_some(found))
            }
        })
        .await?;
        ensure(allowed.contains(&found.len()), || {
            format!(
                "{what}: {} calls, expected {allowed:?}: {found:?}",
                found.len()
            )
        })?;
        self.expect_exact(what, token, method, matches, allowed);
        Ok(found)
    }

    fn expect_exact(
        &mut self,
        what: String,
        token: &'static str,
        method: &'static str,
        matches: impl Fn(&Call) -> bool + 'static,
        allowed: RangeInclusive<usize>,
    ) {
        self.exact.push(Exact {
            what,
            token,
            method,
            matches: Box::new(matches),
            allowed,
        });
    }

    async fn metrics(&self) -> Result<String, String> {
        let url = format!("{}/metrics", self.stack.admin);
        let response = self
            .stack
            .http
            .get(url)
            .send()
            .await
            .map_err(|err| format!("reading tgin's metrics: {}", err.without_url()))?;
        response
            .text()
            .await
            .map_err(|err| format!("reading tgin's metrics: {}", err.without_url()))
    }

    async fn metric(&self, name: &str, labels: &[(&str, &str)]) -> Result<f64, String> {
        Ok(metric(&self.metrics().await?, name, labels))
    }

    /// tgin's count of `consumer`'s webhook replies with `outcome`.
    async fn replies(&self, consumer: &str, outcome: &str) -> Result<f64, String> {
        self.metric(
            "tgin_webhook_replies_total",
            &[("consumer", consumer), ("outcome", outcome)],
        )
        .await
    }

    /// The body of a 200 answer to `GET url`.
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        let response = self
            .stack
            .http
            .get(url)
            .send()
            .await
            .map_err(|err| format!("GET {}: {}", redact(url), err.without_url()))?;
        let status = response.status();
        ensure(status.is_success(), || {
            format!("GET {} answered {status}", redact(url))
        })?;
        let body = response
            .bytes()
            .await
            .map_err(|err| format!("GET {}: {}", redact(url), err.without_url()))?;
        Ok(body.to_vec())
    }

    async fn get_json(&self, url: &str) -> Result<Value, String> {
        let response = self
            .stack
            .http
            .get(url)
            .send()
            .await
            .map_err(|err| format!("GET {}: {}", redact(url), err.without_url()))?;
        response
            .json()
            .await
            .map_err(|err| format!("GET {}: {}", redact(url), err.without_url()))
    }

    async fn post_json(&self, url: &str, body: &Value) -> Result<Value, String> {
        let response = self
            .stack
            .http
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|err| format!("POST {}: {}", redact(url), err.without_url()))?;
        let status = response.status();
        let answer: Value = response
            .json()
            .await
            .map_err(|err| format!("POST {}: {}", redact(url), err.without_url()))?;
        ensure(status.is_success(), || {
            format!("POST {} answered {status}: {answer}", redact(url))
        })?;
        Ok(answer)
    }

    /// Sends `request` and checks that tgin answered it itself with its
    /// Telegram-shaped 400 and exactly `description`; `what` names the
    /// request in errors (its URL holds the token).
    async fn refused(
        &self,
        what: &str,
        request: reqwest::RequestBuilder,
        description: &str,
    ) -> Result<(), String> {
        let response = request
            .send()
            .await
            .map_err(|err| format!("{what}: {}", err.without_url()))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|err| format!("{what}: {}", err.without_url()))?;
        let expected = json!({"ok": false, "error_code": 400, "description": description});
        ensure(
            status == reqwest::StatusCode::BAD_REQUEST && body == expected,
            || format!("{what} answered {status} {body}, not 400 {expected}"),
        )
    }

    async fn print_journal_tail(&self) {
        for (name, fake) in [
            ("the fake's", &self.stack.fake),
            ("the local fake's", &self.stack.fake_local),
        ] {
            match self.journal(fake, "", "", "").await {
                Ok(calls) => {
                    eprintln!("\n--- {name} journal (last 80 of {}) ---", calls.len());
                    for call in calls.iter().rev().take(80).rev() {
                        let files: Vec<_> = call
                            .files
                            .iter()
                            .map(|p| (&p.name, &p.filename, p.size))
                            .collect();
                        // tgin's derived webhook secret stays out of the output.
                        let mut args = call.args.clone();
                        if let Some(secret) = args.get_mut("secret_token") {
                            "<redacted>".clone_into(secret);
                        }
                        eprintln!(
                            "#{} t={} {} {:?} status={:?} ct={:?} te={:?} args={:?} files={files:?}",
                            call.seq,
                            call.time_ms,
                            call.kind,
                            call.method,
                            call.status,
                            call.content_type,
                            call.transfer_encoding,
                            args,
                        );
                    }
                }
                Err(err) => eprintln!("cannot read {name} journal: {err}"),
            }
        }
    }
}

/// The URL without its path: paths hold fake tokens, which still should not
/// reach logs.
fn redact(url: &str) -> String {
    Url::parse(url).map_or_else(|_| "<url>".to_owned(), |u| u.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runner's tokens are the ones compose.yaml gives the bots.
    #[test]
    fn tokens_match_compose() {
        let compose = std::fs::read_to_string(compose_file()).expect("compose.yaml reads");
        let local = [
            (LOCAL_POLL, LOCAL_POLL_TOKEN),
            (LOCAL_HOOK, LOCAL_HOOK_TOKEN),
        ];
        let all = FRAMEWORKS
            .iter()
            .flat_map(|f| [(f.poll(), f.poll_token), (f.hook(), f.hook_token)])
            .chain(local.map(|(service, token)| (service.to_owned(), token)));
        for (service, token) in all {
            let anchor = format!("&{service}-token \"{token}\"");
            assert!(compose.contains(&anchor), "compose.yaml lacks `{anchor}`");
        }
    }

    /// The local fake's bots have tokens of Telegram's shape:
    /// `<digits>:<35 characters>`.
    #[test]
    fn local_tokens_are_realistic() {
        for token in [LOCAL_POLL_TOKEN, LOCAL_HOOK_TOKEN] {
            let (id, secret) = token.split_once(':').expect("a token has a ':'");
            assert!(!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()));
            assert_eq!(secret.len(), 35);
            assert!(
                secret
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            );
            assert!(is_local(token));
        }
    }

    /// The fake's published FNV-1a 64 vectors (`journal::fnv1a64`).
    #[test]
    fn fnv1a64_matches_the_fake() {
        assert_eq!(fnv1a64(b""), "cbf29ce484222325");
        assert_eq!(fnv1a64(b"a"), "af63dc4c8601ec8c");
        assert_eq!(fnv1a64(b"hello"), "a430d84680aabd0b");
    }

    #[test]
    fn metric_sums_matching_series() {
        let text = "# TYPE x counter\n\
            x_total{bot=\"a\",method=\"file\",outcome=\"ok\"} 2\n\
            x_total{bot=\"a-b\",method=\"file\",outcome=\"ok\"} 5\n\
            x_total{bot=\"a\",method=\"getFile\",outcome=\"ok\"} 1\n\
            # EOF\n";
        assert_eq!(
            metric(text, "x_total", &[("bot", "a"), ("method", "file")]),
            2.0
        );
        assert_eq!(metric(text, "x_total", &[("bot", "a")]), 3.0);
        assert_eq!(metric(text, "x_total", &[("bot", "c")]), 0.0);
        assert_eq!(metric(text, "y_total", &[]), 0.0);
    }
}
