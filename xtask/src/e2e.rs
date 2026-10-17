//! `cargo xtask e2e [--keep]`: the end-to-end suite. Real bot frameworks
//! (aiogram, python-telegram-bot, grammY, Telegraf) run against tgin and the
//! fake Telegram in `tests/e2e/compose.yaml`; every scenario is checked
//! through the fake's call journal.
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
    /// Its uploads use `Transfer-Encoding: chunked` (F§2.5).
    chunked_upload: bool,
    /// It confirms the last offset when it stops (F§2.2); aiogram does not,
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
            "final: exact counts and no downstream setWebhook/close/logOut",
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
    /// their next poll and their shutdown confirm is the only ack.
    async fn restart(&mut self, f: &'static Framework) -> Result<(), String> {
        let chat = self.chat();
        let before = format!("before restart {}", f.name);
        let allowed = if f.confirms_on_stop { 1..=1 } else { 1..=2 };
        self.inject(f.poll_token, self.text_update(chat, &before))
            .await?;
        self.echo(f.poll_token, "sendMessage", chat, &before, allowed.clone())
            .await?;
        let service = f.poll();
        compose(&["stop", "-t", "30", &service]).quiet()?;
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
    /// the last arrival at least 1.8 s after the first, and the 4th at least
    /// 0.8 s after the first and 0.8 s before the last. The arrival bounds
    /// leave room for latency differences between the messages; the exact
    /// 2 s holds only at tgin's limiter.
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
        ensure(last - first >= 1800, || {
            format!(
                "the 5th message came {} ms after the first, under 1.8 s",
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

    /// Every exact count still holds, and no downstream ever reached
    /// setWebhook, close or logOut on Telegram (tgin calls none of them).
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
        for method in ["setWebhook", "close", "logOut"] {
            let calls = self.calls("", method, "").await?;
            ensure(calls.is_empty(), || {
                format!("{method} reached Telegram: {calls:?}")
            })?;
        }
        Ok(())
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

    async fn inject(&self, token: &str, update: Value) -> Result<(), String> {
        let url = format!("{}/fake/updates/{token}", self.stack.fake);
        self.post_json(&url, &update).await.map(drop)
    }

    /// Stores a file at the fake (its own sendDocument with the bot's token)
    /// and reads its `file_path` with the fake's getFile; both happen before
    /// the scenario's journal mark.
    async fn store_file(
        &self,
        token: &str,
        chat: i64,
        filename: &str,
        data: &[u8],
    ) -> Result<StoredFile, String> {
        let url = format!("{}/bot{token}/sendDocument", self.stack.fake);
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
            .map_err(|err| format!("storing {filename}: {err}"))?;
        let body: Value = response
            .json()
            .await
            .map_err(|err| format!("storing {filename}: {err}"))?;
        let document = &body["result"]["document"];
        let (Some(file_id), Some(file_unique_id)) = (
            document["file_id"].as_str(),
            document["file_unique_id"].as_str(),
        ) else {
            return Err(format!("the fake stored no document: {body}"));
        };
        let info = self
            .post_json(
                &format!("{}/bot{token}/getFile", self.stack.fake),
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

    /// The journal entries of `token` (all bots when empty), filtered by
    /// `method` and `kind` when not empty.
    async fn calls(&self, token: &str, method: &str, kind: &str) -> Result<Vec<Call>, String> {
        let url = Url::parse_with_params(
            &format!("{}/fake/calls", self.stack.fake),
            [("token", token), ("method", method), ("kind", kind)],
        )
        .map_err(|err| format!("journal URL: {err}"))?;
        let response = self
            .stack
            .http
            .get(url)
            .send()
            .await
            .map_err(|err| format!("reading the journal: {err}"))?;
        response
            .json()
            .await
            .map_err(|err| format!("reading the journal: {err}"))
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
            .map_err(|err| format!("reading tgin's metrics: {err}"))?;
        response
            .text()
            .await
            .map_err(|err| format!("reading tgin's metrics: {err}"))
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
            .map_err(|err| format!("GET {}: {err}", redact(url)))?;
        let status = response.status();
        ensure(status.is_success(), || {
            format!("GET {} answered {status}", redact(url))
        })?;
        let body = response
            .bytes()
            .await
            .map_err(|err| format!("GET {}: {err}", redact(url)))?;
        Ok(body.to_vec())
    }

    async fn get_json(&self, url: &str) -> Result<Value, String> {
        let response = self
            .stack
            .http
            .get(url)
            .send()
            .await
            .map_err(|err| format!("GET {}: {err}", redact(url)))?;
        response
            .json()
            .await
            .map_err(|err| format!("GET {}: {err}", redact(url)))
    }

    async fn post_json(&self, url: &str, body: &Value) -> Result<Value, String> {
        let response = self
            .stack
            .http
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|err| format!("POST {}: {err}", redact(url)))?;
        let status = response.status();
        let answer: Value = response
            .json()
            .await
            .map_err(|err| format!("POST {}: {err}", redact(url)))?;
        ensure(status.is_success(), || {
            format!("POST {} answered {status}: {answer}", redact(url))
        })?;
        Ok(answer)
    }

    async fn print_journal_tail(&self) {
        match self.calls("", "", "").await {
            Ok(calls) => {
                eprintln!("\n--- the fake's journal (last 80 of {}) ---", calls.len());
                for call in calls.iter().rev().take(80).rev() {
                    let files: Vec<_> = call
                        .files
                        .iter()
                        .map(|p| (&p.name, &p.filename, p.size))
                        .collect();
                    eprintln!(
                        "#{} t={} {} {:?} status={:?} ct={:?} te={:?} args={:?} files={files:?}",
                        call.seq,
                        call.time_ms,
                        call.kind,
                        call.method,
                        call.status,
                        call.content_type,
                        call.transfer_encoding,
                        call.args,
                    );
                }
            }
            Err(err) => eprintln!("cannot read the fake's journal: {err}"),
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
        for f in &FRAMEWORKS {
            for (service, token) in [(f.poll(), f.poll_token), (f.hook(), f.hook_token)] {
                let anchor = format!("&{service}-token \"{token}\"");
                assert!(compose.contains(&anchor), "compose.yaml lacks `{anchor}`");
            }
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
