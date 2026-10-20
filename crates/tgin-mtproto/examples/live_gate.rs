//! Manual live gate, never run in CI: it drives a `tgin-mtproto`
//! [`Account`] (supervisor, sender, update manager) on the owner's real
//! Telegram account. It is a dev harness, not part of tgin.
//!
//! `cargo run --release -p tgin-mtproto --example live_gate -- <command> [--key value]…`
//!
//! | Command | What it does |
//! |---|---|
//! | `import` | Copies a logged-in session (`--from`) into a new store (`--session`), row by row through [`SessionStore`]: DC options, auth keys, home DC, self user. No update state, no channel pts. |
//! | `whoami` | Starts the account, asks `users.getUsers([inputUserSelf])`: user id, whether the phone matches, home DC, layer. |
//! | `kill-runs` | Up to 5 runs of up to 30 probes (bot private chat and test channel). Each run kills the account process once right after admitting a probe batch (before its state is committed) and once on a timer armed at admission (inside the commit), restarts it, and checks every probe was admitted at least once. |
//! | `backpressure` | Refuses every batch while the bot sends up to 40 probes, then admits again: the probes come back through gap recovery; the refused unsequenced updates are counted as dropped. |
//! | `catch-up` | Stores the pts and access hash of every channel in the chat list, marks them all behind, restarts and times a live probe while the channels catch up; then restarts once more with every channel caught up. |
//! | `soak` | The account for the multi-day soak: counts, footprint, heartbeat and received-id records under `--data`. |
//! | `heartbeat` | The soak's bot: a silent heartbeat every 5 min to the private chat and the test channel; deletes each once `soak` saw it. |
//! | `baseline` | The same runtime with no account, for the footprint comparison. |
//! | `soak-report` | Counts and timings of a soak window, and the received message ids. |
//!
//! Rules this harness keeps (the owner's main account):
//! - test traffic comes only from the test bot, always silent, and is
//!   deleted once the account has seen it; a run sends at most 30 messages;
//! - content (probe tags, message ids) is recorded only for the bot's
//!   private chat and the test channel; every other chat yields counts,
//!   constructor names, box kinds and pts;
//! - nothing prints the api hash, the bot token or any part of it, the
//!   phone, or a key; a Bot API error never carries the request URL.
//!
//! Secrets (defaults under `~/.tgin-secrets`): `--api-env`
//! (`TELEGRAM_API_ID`), `--token-file` (the test bot's token),
//! `--account-env` (`TGIN_SPIKE_PHONE`, for `whoami`). The account's user
//! id and the test channel come from `TGIN_GATE_USER` and
//! `TGIN_GATE_CHANNEL` (Bot API id), from the environment or else
//! `--gate-env` (`m3-gate.env`); they are never in the repository.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use grammers_tl_types::{self as tl, Deserializable, Serializable};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tgin_mtproto::account::{Account, AccountSpec, AccountState, Registry, TcpNetwork};
use tgin_mtproto::decode::{BoxKind, PeerId};
use tgin_mtproto::rpc::{ErrorClass, Route, classify};
use tgin_mtproto::{
    Admission, Batch, BatchSink, ChannelKind, IdsError, InitParams, Peer, PeerKind, RpcOutcome,
    SessionError, SessionStore, StateDelta, UpdateBytes, UpdateIds, UpdateState,
};
use tokio::io::AsyncBufReadExt as _;
use tokio::sync::mpsc;

type AnyError = Box<dyn Error + Send + Sync>;

/// The test identifiers, read once at start ([`Gate::load`]).
static GATE: std::sync::OnceLock<Gate> = std::sync::OnceLock::new();
const HOME_DC: i32 = 2;
const LAYER: i32 = 230;
const ACCOUNT: &str = "main";
/// Volume caps on the owner's account.
const MAX_RUNS: usize = 5;
const MAX_KILL_PROBES: usize = 30;
const MAX_BACKPRESSURE_PROBES: usize = 40;
/// The soak's heartbeat text starts with this.
const HEARTBEAT: &str = "lg hb ";

const USAGE: &str = "usage: live_gate <import|whoami|kill-runs|backpressure|catch-up|soak|heartbeat|baseline|soak-report> [--key value]…";

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("live_gate: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("live_gate: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode, AnyError> {
    let args = Args::parse()?;
    let paths = Paths::new(&args)?;
    // Panic payloads could carry content: only the location is printed.
    std::panic::set_hook(Box::new(|info| {
        let at = info
            .location()
            .map_or_else(|| "?".to_owned(), |l| format!("{}:{}", l.file(), l.line()));
        eprintln!("panic at {at}");
    }));
    if matches!(args.cmd.as_str(), "whoami" | "kill-child" | "soak") {
        log_stderr();
    }
    if !matches!(args.cmd.as_str(), "import" | "baseline" | "soak-report") {
        let gate = Gate::load(&paths.gate_env)?;
        let _ = GATE.set(gate);
    }
    match args.cmd.as_str() {
        "import" => import(&args, &paths).map(|()| ExitCode::SUCCESS),
        "whoami" => whoami(&args, &paths).await,
        "kill-runs" => kill_runs(&args, &paths).await,
        "kill-child" => kill_child(&args, &paths).await,
        "backpressure" => backpressure(&args, &paths).await,
        "catch-up" => catch_up(&args, &paths).await,
        "soak" => soak(&args, &paths).await,
        "heartbeat" => heartbeat(&args, &paths).await,
        "baseline" => baseline(&args, &paths).await,
        "soak-report" => soak_report(&args, &paths).map(|()| ExitCode::SUCCESS),
        other => Err(format!("unknown command `{other}`; {USAGE}").into()),
    }
}

// ---------------------------------------------------------------------------
// Arguments, paths, secrets

struct Args {
    cmd: String,
    opts: HashMap<String, String>,
}

impl Args {
    fn parse() -> Result<Self, AnyError> {
        let mut it = std::env::args().skip(1);
        let cmd = it.next().ok_or(USAGE)?;
        let mut opts = HashMap::new();
        while let Some(key) = it.next() {
            let key = key
                .strip_prefix("--")
                .ok_or_else(|| format!("unexpected argument `{key}`; {USAGE}"))?
                .to_owned();
            let value = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
            opts.insert(key, value);
        }
        Ok(Self { cmd, opts })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.opts.get(key).map(String::as_str)
    }

    fn path(&self, key: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
        self.get(key).map_or_else(default, PathBuf::from)
    }

    fn num<T: FromStr>(&self, key: &str, default: T) -> Result<T, AnyError> {
        match self.get(key) {
            Some(v) => v
                .parse()
                .map_err(|_| format!("--{key}: not a number").into()),
            None => Ok(default),
        }
    }
}

struct Paths {
    session: PathBuf,
    out: PathBuf,
    api_env: PathBuf,
    token_file: PathBuf,
    account_env: PathBuf,
    gate_env: PathBuf,
    secrets: PathBuf,
}

impl Paths {
    fn new(args: &Args) -> Result<Self, AnyError> {
        let secrets = args.path("secrets", || {
            home()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".tgin-secrets")
        });
        Ok(Self {
            session: args.path("session", || secrets.join("m3-account").join("session")),
            out: args.path("out", || std::env::temp_dir().join("tgin-live-gate")),
            api_env: args.path("api-env", || secrets.join("telegram-api.env")),
            token_file: args.path("token-file", || secrets.join("bot-token")),
            account_env: args.path("account-env", || secrets.join("m3-account.env")),
            gate_env: args.path("gate-env", || secrets.join("m3-gate.env")),
            secrets,
        })
    }

    fn api_id(&self) -> Result<i32, AnyError> {
        env_value(&self.api_env, "TELEGRAM_API_ID")?
            .parse()
            .map_err(|_| {
                format!("{} has an invalid TELEGRAM_API_ID", self.api_env.display()).into()
            })
    }

    fn token(&self) -> Result<BotToken, AnyError> {
        BotToken::read(&self.token_file)
    }

    fn out_dir(&self, name: &str) -> Result<PathBuf, AnyError> {
        let dir = self.out.join(name);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// `key` from a `KEY=VALUE` file. Errors name the file and the key, never a
/// value.
fn env_value(path: &Path, key: &str) -> Result<String, AnyError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let prefix = format!("{key}=");
    let value = text
        .lines()
        .map(str::trim)
        .map(|line| line.strip_prefix("export ").unwrap_or(line))
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .ok_or_else(|| format!("{} has no {key}", path.display()))?;
    Ok(value
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .to_owned())
}

/// The account's user id (also the Bot API chat id of its private chat
/// with the test bot) and the test channel's Bot API id. Personal
/// identifiers: never in the repository. Read from the environment, else
/// from `--gate-env` (default `~/.tgin-secrets/m3-gate.env`).
struct Gate {
    user: i64,
    channel: i64,
}

const GATE_USER: &str = "TGIN_GATE_USER";
const GATE_CHANNEL: &str = "TGIN_GATE_CHANNEL";
/// The Bot API offset of a channel's id (`-100…`).
const CHANNEL_OFFSET: i64 = 1_000_000_000_000;

impl Gate {
    fn load(file: &Path) -> Result<Self, AnyError> {
        let read = |key: &str| -> Result<i64, AnyError> {
            let value = match std::env::var(key) {
                Ok(v) => v,
                Err(_) => env_value(file, key)?,
            };
            value
                .trim()
                .parse()
                .map_err(|_| format!("{key} is not a number").into())
        };
        let gate = Self {
            user: read(GATE_USER)?,
            channel: read(GATE_CHANNEL)?,
        };
        if gate.user <= 0 || gate.channel > -CHANNEL_OFFSET {
            return Err(format!(
                "{GATE_USER} must be a user id and {GATE_CHANNEL} a channel's Bot API id (-100…)"
            )
            .into());
        }
        Ok(gate)
    }
}

fn gate() -> &'static Gate {
    GATE.get().expect("the gate ids are loaded at start")
}

/// The account's user id: the Bot API chat id of the bot's private chat.
fn owner() -> i64 {
    gate().user
}

/// The test channel's Bot API id.
fn test_channel() -> i64 {
    gate().channel
}

/// The test channel's bare id.
fn test_channel_id() -> i64 {
    -CHANNEL_OFFSET - gate().channel
}

/// The test bot's token. Never printed, in part or whole.
struct BotToken(String);

impl BotToken {
    fn read(path: &Path) -> Result<Self, AnyError> {
        let token = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?
            .trim()
            .to_owned();
        if token.split_once(':').is_none() {
            return Err(format!("{} does not hold a bot token", path.display()).into());
        }
        Ok(Self(token))
    }

    /// The bot's user id: the peer of the private chat as the account sees it.
    fn bot_id(&self) -> Result<i64, AnyError> {
        self.0
            .split_once(':')
            .and_then(|(id, _)| id.parse().ok())
            .ok_or_else(|| "the bot token has no numeric bot id".into())
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn utc() -> String {
    let secs = unix_ms() / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil(i64::try_from(days).unwrap_or(0));
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn say(line: impl AsRef<str>) {
    println!("[{}] {}", utc(), line.as_ref());
}

fn secs(d: Duration) -> String {
    format!("{:.2} s", d.as_secs_f64())
}

/// Sends `tgin-mtproto`'s logs (names, counts, error codes; never content)
/// to `path`.
fn log_to(path: &Path) -> Result<(), AnyError> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .try_init()
        .map_err(|e| format!("log: {e}").into())
}

/// The same logs to stderr.
fn log_stderr() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
}

// ---------------------------------------------------------------------------
// The account

fn start_account(paths: &Paths, api_id: i32, sink: Arc<dyn BatchSink>) -> Account {
    Account::start(
        AccountSpec::new(ACCOUNT, &paths.session),
        sink,
        Arc::new(TcpNetwork::new(InitParams::new(api_id))),
        Arc::new(Registry::default()),
    )
}

/// Waits for `running`; a state that does not get there on its own is an
/// error. A flood wait is an environment note, waited out.
async fn wait_running(account: &Account, limit: Duration) -> Result<(), AnyError> {
    let deadline = tokio::time::Instant::now() + limit;
    let mut states = account.watch();
    let mut noted = false;
    loop {
        let state = states.borrow_and_update().clone();
        match &state {
            AccountState::Running => return Ok(()),
            AccountState::LoggedOut { .. } | AccountState::NeedsLogin | AccountState::Frozen => {
                return Err(format!("the account is {}", state.name()).into());
            }
            AccountState::Failed(f) if !f.retried() => {
                return Err(format!("the account failed: {}", f.as_str()).into());
            }
            AccountState::FloodWait { until } if !noted => {
                noted = true;
                let left = until.duration_since(SystemTime::now()).unwrap_or_default();
                say(format!(
                    "the account waits out a FLOOD_WAIT of {} (environment note)",
                    secs(left)
                ));
            }
            _ => {}
        }
        tokio::time::timeout_at(deadline, states.changed())
            .await
            .map_err(|_| {
                format!(
                    "the account is not running after {} (state {})",
                    secs(limit),
                    account.state().name()
                )
            })?
            .map_err(|_| "the account's task ended")?;
    }
}

/// Waits until the start-up difference (the common box) is over.
async fn wait_common_caught_up(account: &Account, limit: Duration) -> Result<(), AnyError> {
    let deadline = Instant::now() + limit;
    while account.status().catching_up.common || account.state() != AccountState::Running {
        if Instant::now() > deadline {
            return Err(format!("the common box still catches up after {}", secs(limit)).into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

/// One call on the account's main session. FLOOD_WAIT is honoured.
async fn call<R: Deserializable>(
    account: &Account,
    request: &impl Serializable,
) -> Result<R, AnyError> {
    let rpc = account.rpc();
    loop {
        let outcome = rpc
            .invoke(Route::Main, Bytes::from(request.to_bytes()))
            .await
            .map_err(|e| e.to_string())?;
        match outcome {
            RpcOutcome::Result(bytes) => {
                return R::from_bytes(&bytes).map_err(|e| format!("answer: {e}").into());
            }
            RpcOutcome::Error { code, message } => match classify(code, &message) {
                ErrorClass::FloodWait(x) => {
                    say(format!(
                        "FLOOD_WAIT {x} s on a call (environment note); waiting"
                    ));
                    tokio::time::sleep(Duration::from_secs(u64::from(x) + 1)).await;
                }
                _ => return Err(format!("rpc_error {code} {message}").into()),
            },
        }
    }
}

/// Admits everything, counting.
#[derive(Default)]
struct CountSink {
    updates: AtomicU64,
}

impl BatchSink for CountSink {
    fn admit(
        &self,
        batch: &Batch,
        ids: &mut UpdateIds,
        store: &SessionStore,
    ) -> Result<Admission, IdsError> {
        if !batch.updates.is_empty() {
            ids.take(batch.updates.len(), store)?;
        }
        self.updates
            .fetch_add(batch.updates.len() as u64, Ordering::Relaxed);
        Ok(Admission::Admitted)
    }
}

/// The test chat an update belongs to, if any.
fn test_chat(update: &UpdateBytes, bot: i64) -> Option<&'static str> {
    match update.facts().peer {
        Some(PeerId::User(id)) if id == bot => Some("private"),
        Some(PeerId::Channel(id)) if id == test_channel_id() => Some("channel"),
        _ => None,
    }
}

fn chat_id(chat: &str) -> i64 {
    if chat == "channel" {
        test_channel()
    } else {
        owner()
    }
}

/// Whether the update has a sequence (a box and a non-zero pts or qts).
fn sequenced(update: &UpdateBytes) -> bool {
    let f = update.facts();
    let seq = match f.box_kind {
        BoxKind::Common | BoxKind::Channel(_) => f.pts,
        BoxKind::Qts => f.qts,
        BoxKind::None => None,
    };
    seq.is_some_and(|s| s != 0)
}

/// A new message's id, for the test chats only.
fn message_id(update: &UpdateBytes) -> Option<i32> {
    let bytes = update.bytes();
    match update.facts().constructor {
        "updateShortMessage" => match tl::enums::Updates::from_bytes(bytes).ok()? {
            tl::enums::Updates::UpdateShortMessage(m) => Some(m.id),
            _ => None,
        },
        "updateNewMessage" | "updateNewChannelMessage" => {
            let message = match tl::enums::Update::from_bytes(bytes).ok()? {
                tl::enums::Update::NewMessage(u) => u.message,
                tl::enums::Update::NewChannelMessage(u) => u.message,
                _ => return None,
            };
            Some(match message {
                tl::enums::Message::Message(m) => m.id,
                tl::enums::Message::Service(m) => m.id,
                tl::enums::Message::Empty(m) => m.id,
            })
        }
        _ => None,
    }
}

/// Composed from a difference (pts_count 0) or pushed.
fn origin(update: &UpdateBytes) -> &'static str {
    let f = update.facts();
    if f.pts_count == Some(0)
        && matches!(
            f.constructor,
            "updateNewMessage" | "updateNewChannelMessage"
        )
    {
        "difference"
    } else {
        "push"
    }
}

// ---------------------------------------------------------------------------
// The Bot API (the test bot only)

#[derive(Clone)]
struct Bot {
    http: reqwest::Client,
    base: Arc<str>,
}

impl Bot {
    fn new(token: &BotToken) -> Result<Self, AnyError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("Bot API client: {}", e.without_url()))?;
        Ok(Self {
            http,
            base: Arc::from(format!("https://api.telegram.org/bot{}/", token.0)),
        })
    }

    /// One method call; a 429 waits `retry_after` and asks again (an
    /// environment note). Errors carry the method and Telegram's
    /// description, never the URL.
    async fn call(&self, method: &'static str, body: &Value) -> Result<Value, AnyError> {
        loop {
            let response = self
                .http
                .post(format!("{}{method}", self.base))
                .json(body)
                .send()
                .await
                .map_err(|e| format!("Bot API {method}: {}", e.without_url()))?;
            let answer: Value = response
                .json()
                .await
                .map_err(|e| format!("Bot API {method}: {}", e.without_url()))?;
            if answer["ok"] == true {
                return Ok(answer["result"].clone());
            }
            if answer["error_code"] == 429 {
                let wait = answer["parameters"]["retry_after"].as_u64().unwrap_or(5);
                say(format!(
                    "bot: {method} FLOOD_WAIT {wait} s (environment note)"
                ));
                tokio::time::sleep(Duration::from_secs(wait + 1)).await;
                continue;
            }
            return Err(format!(
                "Bot API {method} failed ({}): {}",
                answer["error_code"],
                answer["description"].as_str().unwrap_or("no description")
            )
            .into());
        }
    }

    /// A silent message; its id.
    async fn send(&self, chat: i64, text: &str) -> Result<i32, AnyError> {
        let body = json!({"chat_id": chat, "text": text, "disable_notification": true});
        let message = self.call("sendMessage", &body).await?;
        message["message_id"]
            .as_i64()
            .and_then(|id| i32::try_from(id).ok())
            .ok_or_else(|| "sendMessage: no message_id".into())
    }

    async fn delete(&self, chat: i64, ids: &[i32]) -> Result<(), AnyError> {
        for chunk in ids.chunks(100) {
            let body = json!({"chat_id": chat, "message_ids": chunk});
            self.call("deleteMessages", &body).await?;
        }
        Ok(())
    }

    /// `sendChatAction typing`: reaches the account as an unsequenced
    /// `updateUserTyping`.
    async fn typing(&self, chat: i64) -> Result<(), AnyError> {
        let body = json!({"chat_id": chat, "action": "typing"});
        self.call("sendChatAction", &body).await.map(|_| ())
    }
}

/// Probes sent by one check, by chat.
#[derive(Default)]
struct Sent {
    probes: Vec<(&'static str, String, i32)>,
}

impl Sent {
    fn tags(&self) -> BTreeSet<String> {
        self.probes.iter().map(|(_, tag, _)| tag.clone()).collect()
    }

    fn count(&self, chat: &str) -> usize {
        self.probes.iter().filter(|(c, _, _)| *c == chat).count()
    }

    /// Deletes every probe; (deleted, failed).
    async fn delete_all(&self, bot: &Bot) -> (usize, usize) {
        let (mut ok, mut failed) = (0, 0);
        for chat in ["private", "channel"] {
            let ids: Vec<i32> = self
                .probes
                .iter()
                .filter(|(c, _, _)| *c == chat)
                .map(|(_, _, id)| *id)
                .collect();
            if ids.is_empty() {
                continue;
            }
            match bot.delete(chat_id(chat), &ids).await {
                Ok(()) => ok += ids.len(),
                Err(e) => {
                    say(format!("delete in the {chat} chat failed: {e}"));
                    failed += ids.len();
                }
            }
        }
        (ok, failed)
    }
}

/// Sends `n` probes `{prefix}p{i}` one per `every`, alternating the private
/// chat and the test channel.
async fn send_probes(bot: Bot, prefix: String, n: usize, every: Duration, sent: Arc<Mutex<Sent>>) {
    let start = tokio::time::Instant::now();
    for i in 1..=n {
        let chat = if i % 2 == 1 { "private" } else { "channel" };
        let tag = format!("{prefix}p{i:02}");
        match bot.send(chat_id(chat), &tag).await {
            Ok(id) => sent.lock().probes.push((chat, tag, id)),
            Err(e) => say(format!("probe {i} not sent: {e}")),
        }
        tokio::time::sleep_until(start + every * u32::try_from(i).unwrap_or(u32::MAX)).await;
    }
}

// ---------------------------------------------------------------------------
// import

fn import(args: &Args, paths: &Paths) -> Result<(), AnyError> {
    let from = args.path("from", || paths.secrets.join("m3-spike").join("session"));
    if !from.join("session.redb").is_file() {
        return Err(format!("{} holds no session.redb", from.display()).into());
    }
    let src = SessionStore::open(&from)?;
    let login = src.login()?.ok_or("the source session records no login")?;
    // A store written through grammers' session keeps no home DC row until
    // a migration sets one: its home is grammers' default, DC 2.
    let stored_home = src.home_dc()?;
    let home = stored_home.unwrap_or(HOME_DC);
    let me = src
        .peer(login.self_user_id)?
        .ok_or("the source session has no row for its self user")?;
    let options = src.dc_options()?;
    let mut dcs: BTreeSet<i32> = options.iter().map(|(id, _)| *id).collect();
    dcs.insert(home);
    dcs.extend(1..=5);
    let mut keys = Vec::new();
    for dc in dcs {
        if let Some(key) = src.auth_key(dc)? {
            keys.push((dc, key));
        }
    }
    let home_key = keys
        .iter()
        .find(|(dc, _)| *dc == home)
        .map(|(_, key)| key.clone())
        .ok_or("the source session has no key for its home DC")?;
    // Only the home key is used; media DCs get keys of their own, authorized
    // on first use.
    keys.retain(|(dc, _)| *dc == home);
    let left_state = src.update_state()?;
    let left_channels = src.channel_pts()?.len();
    drop(src);

    let dst = SessionStore::open(&paths.session)?;
    if dst.login()?.is_some() || dst.home_dc()?.is_some() {
        return Err(format!(
            "{} already holds an authorization; nothing copied",
            paths.session.display()
        )
        .into());
    }
    for (dc, addr) in &options {
        dst.set_dc_option(*dc, *addr)?;
    }
    dst.set_home_key(home, &home_key)?;
    let recorded = dst.record_login(me)?;

    // Read back.
    let ok = dst.dc_options()? == options
        && dst.home_dc()? == Some(home)
        && keys
            .iter()
            .all(|(dc, key)| dst.auth_key(*dc).ok().flatten().as_ref() == Some(key))
        && dst.login()?.map(|l| l.self_user_id) == Some(login.self_user_id)
        && dst.peer(login.self_user_id)?.map(|p| p.id) == Some(me.id)
        && dst.update_state()? == UpdateState::default()
        && dst.channel_pts()?.is_empty();
    let list =
        |it: &mut dyn Iterator<Item = i32>| it.map(|d| d.to_string()).collect::<Vec<_>>().join(",");
    say(format!(
        "import: {} → {}",
        from.display(),
        paths.session.display()
    ));
    say(format!(
        "import: dc options {} (DCs {}); auth keys {} (DCs {}; 256 bytes each, not printed); home DC {home} ({}); self user {} (login time recorded as the import, unix {})",
        options.len(),
        list(&mut options.iter().map(|(d, _)| *d)),
        keys.len(),
        list(&mut keys.iter().map(|(d, _)| *d)),
        if stored_home.is_some() {
            "stored"
        } else {
            "no row in the source: grammers' default"
        },
        login.self_user_id,
        recorded.login_unix,
    ));
    say(format!(
        "import: not copied: the update state (source pts {} qts {} seq {}) and {left_channels} channel pts rows; the target starts with none",
        left_state.pts, left_state.qts, left_state.seq
    ));
    say(format!(
        "import: read back {}",
        if ok { "identical" } else { "DIFFERENT" }
    ));
    if ok {
        Ok(())
    } else {
        Err("the imported rows do not read back as copied".into())
    }
}

// ---------------------------------------------------------------------------
// whoami

async fn whoami(_args: &Args, paths: &Paths) -> Result<ExitCode, AnyError> {
    let api_id = paths.api_id()?;
    let phone = env_value(&paths.account_env, "TGIN_SPIKE_PHONE")?;
    let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
    let started = Instant::now();
    let account = start_account(paths, api_id, Arc::new(CountSink::default()));
    let result = async {
        wait_running(&account, Duration::from_secs(90)).await?;
        let running = started.elapsed();
        let users: Vec<tl::enums::User> = call(
            &account,
            &tl::functions::users::GetUsers {
                id: vec![tl::enums::InputUser::UserSelf],
            },
        )
        .await?;
        let Some(tl::enums::User::User(user)) = users.into_iter().next() else {
            return Err::<_, AnyError>("users.getUsers answered no user".into());
        };
        let matches = !digits(&phone).is_empty()
            && user.phone.as_deref().map(digits) == Some(digits(&phone));
        let status = account.status();
        say(format!(
            "whoami: user_id={} / phone matches: {} / home_dc={} layer={} (compiled layer {}) / running {} after start",
            user.id,
            if matches { "yes" } else { "no" },
            status.home_dc.map_or_else(|| "-".to_owned(), |d| d.to_string()),
            status.layer,
            tl::LAYER,
            secs(running),
        ));
        Ok(user.id == owner() && matches && status.home_dc == Some(HOME_DC) && status.layer == LAYER)
    }
    .await;
    account.close().await;
    if result? {
        Ok(ExitCode::SUCCESS)
    } else {
        say("whoami: does not match the expected account");
        Ok(ExitCode::from(2))
    }
}

// ---------------------------------------------------------------------------
// The probe sink: records admitted probes in a journal, fsynced before the
// batch counts as admitted.

#[derive(Clone, Copy, PartialEq, Eq)]
enum KillKind {
    /// Abort right after the batch is admitted, before its state is
    /// committed.
    AfterAdmit,
    /// Arm a timer at admission that aborts the process `delay` later,
    /// while the ingest commits the batch's state.
    InCommit,
}

impl KillKind {
    fn name(self) -> &'static str {
        match self {
            Self::AfterAdmit => "after-admit",
            Self::InCommit => "in-commit",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "after-admit" => Some(Self::AfterAdmit),
            "in-commit" => Some(Self::InCommit),
            _ => None,
        }
    }
}

struct Seen {
    tag: String,
    at: Instant,
}

#[derive(Default)]
struct ProbeStats {
    batches: u64,
    updates: u64,
    refused_batches: u64,
    refused_updates: u64,
    refused_unsequenced: u64,
    by_origin: BTreeMap<&'static str, u64>,
}

struct ProbeSink {
    bot: i64,
    prefix: String,
    child: u32,
    journal: Mutex<File>,
    seen: mpsc::UnboundedSender<Seen>,
    refuse: AtomicBool,
    /// The kill and the instant it is armed from.
    kill: Mutex<Option<(KillKind, Instant)>>,
    /// The in-commit timer's longest delay.
    commit_window: Duration,
    stats: Mutex<ProbeStats>,
}

impl ProbeSink {
    fn new(
        bot: i64,
        prefix: String,
        child: u32,
        journal: &Path,
        seen: mpsc::UnboundedSender<Seen>,
    ) -> Result<Self, AnyError> {
        let journal = OpenOptions::new().create(true).append(true).open(journal)?;
        Ok(Self {
            bot,
            prefix,
            child,
            journal: Mutex::new(journal),
            seen,
            refuse: AtomicBool::new(false),
            kill: Mutex::new(None),
            commit_window: Duration::ZERO,
            stats: Mutex::new(ProbeStats::default()),
        })
    }

    fn journal(&self, lines: &[String]) -> Result<(), IdsError> {
        let mut j = self.journal.lock();
        for line in lines {
            writeln!(j, "{line}").map_err(|e| IdsError::Store(SessionError::Io(e)))?;
        }
        j.sync_data()
            .map_err(|e| IdsError::Store(SessionError::Io(e)))
    }
}

impl BatchSink for ProbeSink {
    fn admit(
        &self,
        batch: &Batch,
        ids: &mut UpdateIds,
        store: &SessionStore,
    ) -> Result<Admission, IdsError> {
        if self.refuse.load(Ordering::SeqCst) {
            let mut stats = self.stats.lock();
            stats.refused_batches += 1;
            stats.refused_updates += batch.updates.len() as u64;
            stats.refused_unsequenced +=
                batch.updates.iter().filter(|u| !sequenced(u)).count() as u64;
            return Ok(Admission::Refused);
        }
        if !batch.updates.is_empty() {
            ids.take(batch.updates.len(), store)?;
        }
        let mut lines = Vec::new();
        let mut tags = Vec::new();
        {
            let mut stats = self.stats.lock();
            stats.batches += 1;
            stats.updates += batch.updates.len() as u64;
            for update in &batch.updates {
                let Some(chat) = test_chat(update, self.bot) else {
                    continue;
                };
                let Some(text) = update.facts().text.filter(|t| t.starts_with(&self.prefix)) else {
                    continue;
                };
                let from = origin(update);
                *stats.by_origin.entry(from).or_default() += 1;
                lines.push(format!(
                    "A {} {} {chat} {} {from}",
                    self.child,
                    text.replace(' ', "_"),
                    update.facts().constructor
                ));
                tags.push(text.to_owned());
            }
        }
        if lines.is_empty() {
            return Ok(Admission::Admitted);
        }
        let kill = (*self.kill.lock())
            .filter(|(_, at)| Instant::now() >= *at)
            .map(|(kind, _)| kind);
        if let Some(kind) = kill {
            let common = batch
                .commit
                .pts
                .map_or_else(|| "-".to_owned(), |p| p.to_string());
            let channel = batch
                .commit
                .channels
                .iter()
                .find(|(c, _)| *c == test_channel_id())
                .map_or_else(|| "-".to_owned(), |(_, p)| p.to_string());
            let delay = match kind {
                KillKind::AfterAdmit => Duration::ZERO,
                KillKind::InCommit => {
                    let max = u64::try_from(self.commit_window.as_micros())
                        .unwrap_or(1)
                        .max(1);
                    Duration::from_micros(rand::random::<u64>() % max)
                }
            };
            lines.push(format!(
                "K {} {} {} {common} {channel} {}",
                self.child,
                kind.name(),
                batch.id,
                delay.as_micros()
            ));
            self.journal(&lines)?;
            match kind {
                KillKind::AfterAdmit => std::process::abort(),
                KillKind::InCommit => {
                    std::thread::spawn(move || {
                        std::thread::sleep(delay);
                        std::process::abort();
                    });
                }
            }
        } else {
            self.journal(&lines)?;
        }
        let at = Instant::now();
        for tag in tags {
            let _ = self.seen.send(Seen { tag, at });
        }
        Ok(Admission::Admitted)
    }
}

/// How long one state write takes on this disk: the median of 15
/// `commit_state` calls on a scratch store next to `near`.
fn commit_time(near: &Path) -> Result<Duration, AnyError> {
    let dir = near.join(format!("commit-probe-{}", std::process::id()));
    let mut times = Vec::new();
    {
        let store = SessionStore::open(&dir)?;
        for pts in 1..=15 {
            let delta = StateDelta {
                pts: Some(pts),
                ..StateDelta::default()
            };
            let t = Instant::now();
            store.commit_state(&delta)?;
            times.push(t.elapsed());
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    times.sort();
    Ok(times[times.len() / 2])
}

/// A probe admission in a journal.
struct Admitted {
    tag: String,
    origin: String,
}

/// A kill in a journal: the batch and the state it moves to.
#[derive(Clone, Copy)]
struct KillMark {
    child: u32,
    batch: u64,
    common: Option<i32>,
    channel: Option<i32>,
    delay_us: u64,
}

/// What one kill run's children wrote to their journal.
struct Journal {
    admitted: Vec<Admitted>,
    kills: Vec<KillMark>,
}

impl Journal {
    fn read(path: &Path) -> Result<Self, AnyError> {
        let mut out = Self {
            admitted: Vec::new(),
            kills: Vec::new(),
        };
        let Ok(file) = File::open(path) else {
            return Ok(out);
        };
        for line in BufReader::new(file).lines() {
            let line = line?;
            let f: Vec<&str> = line.split(' ').collect();
            match f.as_slice() {
                ["A", _child, tag, _chat, _ctor, from] => out.admitted.push(Admitted {
                    tag: tag.replace('_', " "),
                    origin: (*from).to_owned(),
                }),
                ["K", child, _kind, batch, common, channel, delay] => out.kills.push(KillMark {
                    child: child.parse()?,
                    batch: batch.parse()?,
                    common: common.parse().ok(),
                    channel: channel.parse().ok(),
                    delay_us: delay.parse()?,
                }),
                _ => {}
            }
        }
        Ok(out)
    }

    fn tags(&self) -> BTreeSet<String> {
        self.admitted.iter().map(|a| a.tag.clone()).collect()
    }
}

// ---------------------------------------------------------------------------
// kill-runs

/// One child of a kill run: runs the account until killed or told to stop
/// (the file `--stop-file` appears).
async fn kill_child(args: &Args, paths: &Paths) -> Result<ExitCode, AnyError> {
    let api_id = paths.api_id()?;
    let bot = paths.token()?.bot_id()?;
    let run: u32 = args.num("run", 0)?;
    let child: u32 = args.num("child", 0)?;
    let journal = PathBuf::from(args.get("journal").ok_or("--journal")?);
    let stop = PathBuf::from(args.get("stop-file").ok_or("--stop-file")?);
    let kill = args.get("kill").and_then(KillKind::parse);
    let arm = Duration::from_secs(args.num("arm-secs", 0)?);
    let window = Duration::from_micros(args.num("commit-window-us", 0)?);
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut sink = ProbeSink::new(bot, format!("lg kill r{run} "), child, &journal, tx)?;
    sink.commit_window = window;
    let sink = Arc::new(sink);
    let account = start_account(paths, api_id, Arc::clone(&sink) as Arc<dyn BatchSink>);
    wait_running(&account, Duration::from_secs(90)).await?;
    println!("RUNNING");
    if let Some(kind) = kill {
        *sink.kill.lock() = Some((kind, Instant::now() + arm));
    }
    while !stop.exists() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    account.stop_ingest();
    account.close().await;
    let stats = sink.stats.lock();
    println!(
        "STOPPED batches {} updates {} dropped {:?}",
        stats.batches,
        stats.updates,
        account.status().dropped
    );
    Ok(ExitCode::SUCCESS)
}

struct KillReport {
    pass: bool,
    sent: usize,
    admitted: usize,
    duplicates: usize,
    kills: Vec<String>,
}

async fn kill_runs(args: &Args, paths: &Paths) -> Result<ExitCode, AnyError> {
    let runs: usize = args.num("runs", MAX_RUNS)?;
    let probes: usize = args.num("probes", MAX_KILL_PROBES)?;
    if runs > MAX_RUNS || probes > MAX_KILL_PROBES {
        return Err(format!("at most {MAX_RUNS} runs of {MAX_KILL_PROBES} probes").into());
    }
    let first: usize = args.num("first-run", 1)?;
    let dir = paths.out_dir("kill-runs")?;
    let bot = Bot::new(&paths.token()?)?;
    let window = commit_time(&dir)?;
    // commit_state is the one write after admission (the peers go with the
    // marks, before it); 2.5 × its median leaves room for a slow write.
    let window = window * 5 / 2;
    say(format!(
        "kill-runs: {runs} runs × {probes} probes (private chat and test channel, alternating, 1/s); in-commit timer uniform in [0, {} µs) (2.5 × the median state write on this disk)",
        window.as_micros()
    ));
    let mut all_pass = true;
    let (mut total_sent, mut total_admitted, mut total_dup) = (0, 0, 0);
    for run in first..first + runs {
        let report = kill_run(run, probes, paths, &dir, &bot, window).await?;
        all_pass &= report.pass;
        total_sent += report.sent;
        total_admitted += report.admitted;
        total_dup += report.duplicates;
        for kill in &report.kills {
            say(format!("  run {run} {kill}"));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    say(format!(
        "kill-runs: {} | probes {total_admitted}/{total_sent} admitted at least once | duplicates {total_dup}",
        if all_pass { "PASS" } else { "FAIL" }
    ));
    Ok(if all_pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[allow(clippy::too_many_lines)]
async fn kill_run(
    run: usize,
    probes: usize,
    paths: &Paths,
    dir: &Path,
    bot: &Bot,
    window: Duration,
) -> Result<KillReport, AnyError> {
    let journal = dir.join(format!("run{run}.journal"));
    let _ = std::fs::remove_file(&journal);
    let stop = dir.join(format!("run{run}.stop"));
    let _ = std::fs::remove_file(&stop);
    let exe = std::env::current_exe()?;
    let sent = Arc::new(Mutex::new(Sent::default()));
    let mut burst: Option<tokio::task::JoinHandle<()>> = None;
    let plan = [
        (Some(KillKind::AfterAdmit), 8),
        (Some(KillKind::InCommit), 6),
        (None, 0),
    ];
    let run_start = Instant::now();
    let mut kills = Vec::new();
    let mut children = 0;
    for (k, (kill, arm)) in plan.into_iter().enumerate() {
        let k = u32::try_from(k + 1).unwrap_or(0);
        children += 1;
        let log = File::create(dir.join(format!("run{run}-child{k}.log")))?;
        let mut cmd = tokio::process::Command::new(&exe);
        cmd.arg("kill-child")
            .env(GATE_USER, owner().to_string())
            .env(GATE_CHANNEL, test_channel().to_string())
            .args(["--run", &run.to_string(), "--child", &k.to_string()])
            .arg("--journal")
            .arg(&journal)
            .arg("--stop-file")
            .arg(&stop)
            .arg("--session")
            .arg(&paths.session)
            .arg("--api-env")
            .arg(&paths.api_env)
            .arg("--token-file")
            .arg(&paths.token_file)
            .args(["--arm-secs", &arm.to_string()])
            .args(["--commit-window-us", &window.as_micros().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .kill_on_drop(true);
        if let Some(kind) = kill {
            cmd.args(["--kill", kind.name()]);
        }
        let mut child = cmd.spawn()?;
        let mut out = tokio::io::BufReader::new(child.stdout.take().ok_or("child stdout")?).lines();
        let started = Instant::now();
        let running = tokio::time::timeout(Duration::from_secs(120), async {
            while let Ok(Some(line)) = out.next_line().await {
                if line == "RUNNING" {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        if !running {
            let _ = child.kill().await;
            return Err(format!("run {run} child {k} did not start; see its log").into());
        }
        if burst.is_none() {
            burst = Some(tokio::spawn(send_probes(
                bot.clone(),
                format!("lg kill r{run} "),
                probes,
                Duration::from_secs(1),
                Arc::clone(&sent),
            )));
        }
        if let Some(kind) = kill {
            match tokio::time::timeout(Duration::from_secs(60), child.wait()).await {
                Ok(status) => {
                    let code = status?.code();
                    let j = Journal::read(&journal)?;
                    let landed = match j.kills.iter().rev().find(|x| x.child == k) {
                        Some(&KillMark {
                            batch,
                            common,
                            channel,
                            delay_us: delay,
                            ..
                        }) => {
                            let store = open_when_free(&paths.session).await?;
                            let state = store.update_state()?;
                            let stored_channel = store
                                .channel_pts()?
                                .into_iter()
                                .find(|(c, _)| *c == test_channel_id())
                                .map(|(_, p)| p);
                            drop(store);
                            let committed = common.is_some_and(|p| state.pts >= p)
                                || channel.is_some_and(|p| stored_channel.is_some_and(|s| s >= p));
                            format!(
                                "batch {batch}, timer {delay} µs; target common {} channel {}; stored common {} channel {} → the abort landed {} the state commit",
                                common.map_or_else(|| "-".to_owned(), |p| p.to_string()),
                                channel.map_or_else(|| "-".to_owned(), |p| p.to_string()),
                                state.pts,
                                stored_channel.map_or_else(|| "-".to_owned(), |p| p.to_string()),
                                if committed { "after" } else { "before" },
                            )
                        }
                        None => "no kill marker (the process ended otherwise)".to_owned(),
                    };
                    kills.push(format!(
                        "child {k}: {} kill at {} into the run (child up {}), exit {:?}; {landed}",
                        kind.name(),
                        secs(run_start.elapsed()),
                        secs(started.elapsed()),
                        code
                    ));
                }
                Err(_) => {
                    let _ = child.kill().await;
                    kills.push(format!(
                        "child {k}: {} kill did not fire within 60 s; killed by the supervisor",
                        kind.name()
                    ));
                }
            }
        } else {
            if let Some(b) = burst.take() {
                b.await?;
            }
            let want = sent.lock().tags();
            let deadline = Instant::now() + Duration::from_secs(120);
            while Instant::now() < deadline && !want.is_subset(&Journal::read(&journal)?.tags()) {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            File::create(&stop)?;
            let _ = tokio::time::timeout(Duration::from_secs(30), child.wait()).await;
            let _ = child.kill().await;
        }
    }
    let j = Journal::read(&journal)?;
    let sent = std::mem::take(&mut *sent.lock());
    let admitted = j.tags();
    let missing: Vec<String> = sent.tags().difference(&admitted).cloned().collect();
    let duplicates = j.admitted.len().saturating_sub(admitted.len());
    let by_chat = |chat: &str| {
        sent.probes
            .iter()
            .filter(|(c, tag, _)| *c == chat && admitted.contains(tag))
            .count()
    };
    let mut origins: BTreeMap<&str, usize> = BTreeMap::new();
    for a in &j.admitted {
        *origins.entry(a.origin.as_str()).or_default() += 1;
    }
    let (deleted, failed) = sent.delete_all(bot).await;
    let pass = missing.is_empty() && !sent.probes.is_empty();
    say(format!(
        "kill-runs run {run}: {} | sent {} (private {}, channel {}) | children {children} | probes admitted {}/{} missing {missing:?} | by box: common {}/{} channel {}/{} | duplicates {duplicates} | origins {origins:?} | deleted {deleted} failed {failed}",
        if pass { "PASS" } else { "FAIL" },
        sent.probes.len(),
        sent.count("private"),
        sent.count("channel"),
        sent.probes.len() - missing.len(),
        sent.probes.len(),
        by_chat("private"),
        sent.count("private"),
        by_chat("channel"),
        sent.count("channel"),
    ));
    Ok(KillReport {
        pass,
        sent: sent.probes.len(),
        admitted: sent.probes.len() - missing.len(),
        duplicates,
        kills,
    })
}

/// Opens the session once the killed process's lock is gone.
async fn open_when_free(dir: &Path) -> Result<SessionStore, AnyError> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match SessionStore::open(dir) {
            Ok(store) => return Ok(store),
            Err(SessionError::Held { .. }) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// backpressure

async fn backpressure(args: &Args, paths: &Paths) -> Result<ExitCode, AnyError> {
    let probes: usize = args.num("probes", MAX_BACKPRESSURE_PROBES)?;
    if probes > MAX_BACKPRESSURE_PROBES {
        return Err(format!("at most {MAX_BACKPRESSURE_PROBES} probes").into());
    }
    let dir = paths.out_dir("backpressure")?;
    log_to(&dir.join("account.log"))?;
    let api_id = paths.api_id()?;
    let token = paths.token()?;
    let bot = Bot::new(&token)?;
    let journal = dir.join("probes.journal");
    let _ = std::fs::remove_file(&journal);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = Arc::new(ProbeSink::new(
        token.bot_id()?,
        "lg bp ".to_owned(),
        0,
        &journal,
        tx,
    )?);
    let account = start_account(paths, api_id, Arc::clone(&sink) as Arc<dyn BatchSink>);
    let result = async {
        wait_running(&account, Duration::from_secs(90)).await?;
        wait_common_caught_up(&account, Duration::from_secs(120)).await?;
        let before = account.status().dropped;
        sink.refuse.store(true, Ordering::SeqCst);
        let paused = Instant::now();
        say(format!("backpressure: refusing every batch while the bot sends {probes} probes, 1/s"));
        let sent = Arc::new(Mutex::new(Sent::default()));
        let burst = tokio::spawn(send_probes(
            bot.clone(),
            "lg bp ".to_owned(),
            probes,
            Duration::from_secs(1),
            Arc::clone(&sent),
        ));
        // Unsequenced probes: `typing` in the private chat every 5 s.
        let mut typing = 0;
        while !burst.is_finished() {
            if bot.typing(owner()).await.is_ok() {
                typing += 1;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        burst.await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        sink.refuse.store(false, Ordering::SeqCst);
        let resumed = Instant::now();
        let refused = {
            let s = sink.stats.lock();
            (s.refused_batches, s.refused_updates, s.refused_unsequenced)
        };
        say(format!(
            "backpressure: resumed after {}; refused {} batches, {} updates ({} unsequenced); typing actions sent {typing}",
            secs(paused.elapsed()),
            refused.0,
            refused.1,
            refused.2
        ));
        let want = sent.lock().tags();
        let mut got: HashMap<String, Instant> = HashMap::new();
        let deadline = Instant::now() + Duration::from_secs(240);
        while !want.iter().all(|t| got.contains_key(t)) && Instant::now() < deadline {
            if let Ok(Some(seen)) =
                tokio::time::timeout(Duration::from_millis(500), rx.recv()).await
            {
                got.entry(seen.tag).or_insert(seen.at);
            }
        }
        let last = got.values().max().map(|at| at.saturating_duration_since(resumed));
        let after = account.status().dropped;
        let full = after.full - before.full;
        let missing: Vec<&String> = want.iter().filter(|t| !got.contains_key(*t)).collect();
        let origins = sink.stats.lock().by_origin.clone();
        let sent = std::mem::take(&mut *sent.lock());
        let by_chat = |chat: &str| {
            sent.probes
                .iter()
                .filter(|(c, tag, _)| *c == chat && got.contains_key(tag))
                .count()
        };
        let pass = missing.is_empty() && !want.is_empty() && full > 0 && full == refused.2;
        say(format!(
            "backpressure: {} | probes recovered {}/{} missing {missing:?} | by box: common {}/{} channel {}/{} | last probe {} after resuming | origins {origins:?} | dropped full +{full} (the refused batches' unsequenced updates: {}) undecodable +{}",
            if pass { "PASS" } else { "FAIL" },
            want.len() - missing.len(),
            want.len(),
            by_chat("private"),
            sent.count("private"),
            by_chat("channel"),
            sent.count("channel"),
            last.map_or_else(|| "-".to_owned(), secs),
            refused.2,
            after.undecodable - before.undecodable,
        ));
        let (deleted, failed) = sent.delete_all(&bot).await;
        say(format!("backpressure: probes deleted {deleted}, not deleted {failed}"));
        Ok::<_, AnyError>(pass)
    }
    .await;
    account.close().await;
    Ok(if result? {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

// ---------------------------------------------------------------------------
// catch-up

#[derive(Default)]
struct DialogWalk {
    main: usize,
    archive: usize,
    users: usize,
    bots: usize,
    groups: usize,
    supergroups: usize,
    channels: usize,
    forbidden: usize,
    other: usize,
    /// Channels and supergroups with a dialog pts and an access hash.
    states: Vec<(Peer, i32)>,
}

/// `messages.getDialogs` over the main list and the archive.
#[allow(clippy::too_many_lines)]
async fn walk_dialogs(account: &Account) -> Result<DialogWalk, AnyError> {
    use tl::enums::{Chat, Dialog, InputPeer, Message, Peer as TlPeer, User};
    let mut walk = DialogWalk::default();
    let mut seen: HashSet<(u8, i64)> = HashSet::new();
    for folder in [0, 1] {
        let (mut offset_date, mut offset_id, mut offset_peer) = (0, 0, InputPeer::Empty);
        for _page in 0..200 {
            let request = tl::functions::messages::GetDialogs {
                exclude_pinned: false,
                folder_id: Some(folder),
                offset_date,
                offset_id,
                offset_peer: offset_peer.clone(),
                limit: 100,
                hash: 0,
            };
            let (dialogs, messages, chats, users, total) =
                match call::<tl::enums::messages::Dialogs>(account, &request).await? {
                    tl::enums::messages::Dialogs::Dialogs(d) => {
                        (d.dialogs, d.messages, d.chats, d.users, None)
                    }
                    tl::enums::messages::Dialogs::Slice(d) => {
                        (d.dialogs, d.messages, d.chats, d.users, Some(d.count))
                    }
                    tl::enums::messages::Dialogs::NotModified(_) => break,
                };
            let chats: HashMap<i64, &Chat> = chats
                .iter()
                .filter_map(|c| match c {
                    Chat::Chat(x) => Some((x.id, c)),
                    Chat::Forbidden(x) => Some((x.id, c)),
                    Chat::Channel(x) => Some((x.id, c)),
                    Chat::ChannelForbidden(x) => Some((x.id, c)),
                    _ => None,
                })
                .collect();
            let users: HashMap<i64, &tl::types::User> = users
                .iter()
                .filter_map(|u| match u {
                    User::User(x) => Some((x.id, x)),
                    User::Empty(_) => None,
                })
                .collect();
            let page_len = dialogs.len();
            let mut last = None;
            for d in &dialogs {
                let Dialog::Dialog(d) = d else {
                    walk.other += 1;
                    continue;
                };
                last = Some(d);
                let key = match &d.peer {
                    TlPeer::User(p) => (0, p.user_id),
                    TlPeer::Chat(p) => (1, p.chat_id),
                    TlPeer::Channel(p) => (2, p.channel_id),
                };
                if !seen.insert(key) {
                    continue;
                }
                if folder == 0 {
                    walk.main += 1;
                } else {
                    walk.archive += 1;
                }
                match &d.peer {
                    TlPeer::User(p) => match users.get(&p.user_id) {
                        Some(u) if u.bot => walk.bots += 1,
                        _ => walk.users += 1,
                    },
                    TlPeer::Chat(_) => walk.groups += 1,
                    TlPeer::Channel(p) => match chats.get(&p.channel_id) {
                        Some(Chat::Channel(c)) => {
                            let kind = if c.broadcast {
                                walk.channels += 1;
                                ChannelKind::Broadcast
                            } else if c.gigagroup {
                                walk.supergroups += 1;
                                ChannelKind::Gigagroup
                            } else {
                                walk.supergroups += 1;
                                ChannelKind::Megagroup
                            };
                            if let (Some(pts), Some(hash)) = (d.pts, c.access_hash) {
                                walk.states.push((
                                    Peer {
                                        id: c.id,
                                        kind: PeerKind::Channel { kind: Some(kind) },
                                        access_hash: Some(hash),
                                    },
                                    pts,
                                ));
                            }
                        }
                        _ => walk.forbidden += 1,
                    },
                }
            }
            let done = page_len < 100
                || total.is_some_and(|t| usize::try_from(t).is_ok_and(|t| seen.len() >= t));
            let Some(last) = last else { break };
            if done {
                break;
            }
            let date = messages.iter().find_map(|m| match m {
                Message::Message(m) if m.id == last.top_message && m.peer_id == last.peer => {
                    Some(m.date)
                }
                Message::Service(m) if m.id == last.top_message && m.peer_id == last.peer => {
                    Some(m.date)
                }
                _ => None,
            });
            let next_peer = match &last.peer {
                TlPeer::User(p) => InputPeer::User(tl::types::InputPeerUser {
                    user_id: p.user_id,
                    access_hash: users
                        .get(&p.user_id)
                        .and_then(|u| u.access_hash)
                        .unwrap_or(0),
                }),
                TlPeer::Chat(p) => InputPeer::Chat(tl::types::InputPeerChat { chat_id: p.chat_id }),
                TlPeer::Channel(p) => InputPeer::Channel(tl::types::InputPeerChannel {
                    channel_id: p.channel_id,
                    access_hash: match chats.get(&p.channel_id) {
                        Some(Chat::Channel(c)) => c.access_hash.unwrap_or(0),
                        Some(Chat::ChannelForbidden(c)) => c.access_hash,
                        _ => 0,
                    },
                }),
            };
            let next = (date.unwrap_or(0), last.top_message);
            if next == (offset_date, offset_id) {
                break;
            }
            (offset_date, offset_id, offset_peer) = (next.0, next.1, next_peer);
        }
    }
    Ok(walk)
}

struct Restart {
    pass: bool,
    channels_done: bool,
}

/// A restart from the stored state: the probe goes to the private chat
/// once the start-up difference is over, so it arrives as a live push while
/// the channels catch up.
async fn timed_restart(
    label: &str,
    paths: &Paths,
    api_id: i32,
    token: &BotToken,
    bot: &Bot,
    dir: &Path,
    cap: Duration,
) -> Result<Restart, AnyError> {
    let (stored, behind) = {
        let store = SessionStore::open(&paths.session)?;
        let s = store.stored_state()?;
        (s.channels.len(), s.behind.len())
    };
    let journal = dir.join(format!("{label}.journal"));
    let _ = std::fs::remove_file(&journal);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let prefix = "lg cu ".to_owned();
    let sink = Arc::new(ProbeSink::new(
        token.bot_id()?,
        prefix.clone(),
        0,
        &journal,
        tx,
    )?);
    let t0 = Instant::now();
    let account = start_account(paths, api_id, Arc::clone(&sink) as Arc<dyn BatchSink>);
    let result = async {
        wait_running(&account, Duration::from_secs(90)).await?;
        wait_common_caught_up(&account, Duration::from_secs(120)).await?;
        let common_done = t0.elapsed();
        let at_send = account.status().catching_up.channels;
        let tag = format!("{prefix}{label}");
        let send_start = t0.elapsed();
        let id = bot.send(owner(), &tag).await?;
        let sent_at = Instant::now();
        let seen = tokio::time::timeout(Duration::from_secs(600), async {
            while let Some(s) = rx.recv().await {
                if s.tag == tag {
                    return Some(s.at);
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        let at_seen = account.status().catching_up.channels;
        let _ = bot.delete(owner(), &[id]).await;
        let Some(seen) = seen else {
            say(format!("catch-up {label}: probe NOT received within 600 s"));
            return Ok::<_, AnyError>(Restart {
                pass: false,
                channels_done: false,
            });
        };
        let to_probe = seen.saturating_duration_since(t0);
        let after_send = seen.saturating_duration_since(sent_at);
        // Then the channels' catch-up, sampled every 5 s.
        let mut samples = Vec::new();
        let done_by = Instant::now() + cap;
        let finished = loop {
            let c = account.status().catching_up.channels;
            samples.push(c);
            if c == 0 {
                break Some(t0.elapsed());
            }
            if Instant::now() > done_by {
                break None;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        };
        let status = account.status();
        let pass = to_probe <= Duration::from_secs(10) && (behind == 0 || at_seen > 0);
        say(format!(
            "catch-up {label}: channels in the stored state {stored}, behind at start {behind}; start-up difference over at {}; probe sent at {} with {at_send} channels catching up; received {} after the restart ({} after the send returned) with {at_seen} channels still catching up; channel catch-up {}; updates admitted {}; dropped {:?}",
            secs(common_done),
            secs(send_start),
            secs(to_probe),
            secs(after_send),
            finished.map_or_else(
                || format!("not over after {} more ({} left)", secs(cap), samples.last().copied().unwrap_or(0)),
                |t| format!("over at {}", secs(t))
            ),
            status.updates,
            status.dropped,
        ));
        let step = (samples.len() / 12).max(1);
        say(format!(
            "catch-up {label}: channels catching up every {} s after the probe: {:?}",
            5 * step,
            samples.iter().step_by(step).collect::<Vec<_>>()
        ));
        Ok(Restart {
            pass,
            channels_done: finished.is_some(),
        })
    }
    .await;
    account.close().await;
    result
}

async fn catch_up(args: &Args, paths: &Paths) -> Result<ExitCode, AnyError> {
    let dir = paths.out_dir("catch-up")?;
    let log = dir.join("account.log");
    log_to(&log)?;
    let api_id = paths.api_id()?;
    let token = paths.token()?;
    let bot = Bot::new(&token)?;
    let cap = Duration::from_secs(args.num("cap-secs", 1800)?);

    // The chat list, through the account.
    let account = start_account(paths, api_id, Arc::new(CountSink::default()));
    let walked = async {
        wait_running(&account, Duration::from_secs(90)).await?;
        let t = Instant::now();
        let walk = walk_dialogs(&account).await?;
        Ok::<_, AnyError>((walk, t.elapsed()))
    }
    .await;
    account.close().await;
    let (walk, took) = walked?;
    say(format!(
        "catch-up: chat list {} (main {} / archive {}): users {}, bots {}, groups {}, supergroups {}, channels {}, inaccessible {}, other entries {}; {} channels and supergroups with a dialog pts; walked in {}",
        walk.main + walk.archive,
        walk.main,
        walk.archive,
        walk.users,
        walk.bots,
        walk.groups,
        walk.supergroups,
        walk.channels,
        walk.forbidden,
        walk.other,
        walk.states.len(),
        secs(took)
    ));

    // Every such channel: its peer, a pts (the stored one if any, else the
    // dialog's), and marked behind, so it catches up at start.
    {
        let store = SessionStore::open(&paths.session)?;
        let known: HashMap<i64, i32> = store.channel_pts()?.into_iter().collect();
        let peers: Vec<Peer> = walk.states.iter().map(|(p, _)| *p).collect();
        store.put_peers(&peers)?;
        let mut seeded = 0;
        for (peer, pts) in &walk.states {
            if !known.contains_key(&peer.id) {
                store.set_channel_pts(peer.id, *pts)?;
                seeded += 1;
            }
        }
        let mark = Batch {
            id: 1,
            updates: Vec::new(),
            users: Bytes::new(),
            chats: Bytes::new(),
            peers: Vec::new(),
            commit: StateDelta::default(),
            ahead: walk.states.iter().map(|(p, _)| p.id).collect(),
        };
        store.mark_channels_behind(std::slice::from_ref(&mark))?;
        let s = store.stored_state()?;
        say(format!(
            "catch-up: stored state now: {} channels with a pts ({seeded} from the chat list, {} kept), {} marked behind",
            s.channels.len(),
            walk.states.len() - seeded,
            s.behind.len()
        ));
    }

    let a = timed_restart("A", paths, api_id, &token, &bot, &dir, cap).await?;
    let b = timed_restart(
        "B",
        paths,
        api_id,
        &token,
        &bot,
        &dir,
        Duration::from_secs(60),
    )
    .await?;
    let floods = std::fs::read_to_string(&log)
        .map(|t| t.lines().filter(|l| l.contains("wait before")).count())
        .unwrap_or(0);
    say(format!(
        "catch-up: {} | restart with every channel behind: {}; restart after their catch-up ({}): {}; flood waits logged by the account: {floods}",
        if a.pass && b.pass { "PASS" } else { "FAIL" },
        if a.pass { "pass" } else { "fail" },
        if a.channels_done {
            "complete"
        } else {
            "incomplete"
        },
        if b.pass { "pass" } else { "fail" },
    ));
    Ok(if a.pass && b.pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

// ---------------------------------------------------------------------------
// The soak

#[derive(Default)]
struct Footprint {
    rss_kb: u64,
    anon_kb: u64,
    threads: u64,
    cpu_ms: u64,
    fds: u64,
    sockets: u64,
}

impl Footprint {
    /// This process, from `/proc/self` (Linux only).
    fn sample() -> Option<Self> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let field = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let after = stat.rsplit_once(')')?.1;
        let f: Vec<&str> = after.split_whitespace().collect();
        let ticks = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let mut fds = 0;
        let mut sockets = 0;
        for entry in std::fs::read_dir("/proc/self/fd").ok()?.flatten() {
            fds += 1;
            if std::fs::read_link(entry.path())
                .is_ok_and(|t| t.to_string_lossy().starts_with("socket:"))
            {
                sockets += 1;
            }
        }
        Some(Self {
            rss_kb: field("VmRSS:"),
            anon_kb: field("RssAnon:"),
            threads: field("Threads:"),
            // utime and stime, in clock ticks of 10 ms.
            cpu_ms: (ticks(11) + ticks(12)) * 10,
            fds,
            sockets,
        })
    }

    fn csv(&self) -> String {
        format!(
            "{},{},{},{},{},{}",
            self.rss_kb, self.anon_kb, self.threads, self.cpu_ms, self.fds, self.sockets
        )
    }
}

/// Appends a line and syncs it.
fn append(path: &Path, line: &str) {
    let write = || -> std::io::Result<()> {
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(f, "{line}")?;
        f.sync_data()
    };
    if let Err(e) = write() {
        eprintln!("cannot append to {}: {e}", path.display());
    }
}

#[derive(Default)]
struct SoakCounts {
    batches: AtomicU64,
    updates: AtomicU64,
    common: AtomicU64,
    qts: AtomicU64,
    channel: AtomicU64,
    unsequenced: AtomicU64,
    heartbeats: AtomicU64,
    private_ids: AtomicU64,
    channel_ids: AtomicU64,
}

/// The soak's sink: counts by box; for the two test chats, the received
/// message ids (`ids.csv`) and the heartbeats seen (`seen.csv`).
struct SoakSink {
    bot: i64,
    data: PathBuf,
    counts: SoakCounts,
    last_heartbeat: Mutex<Instant>,
}

impl BatchSink for SoakSink {
    fn admit(
        &self,
        batch: &Batch,
        ids: &mut UpdateIds,
        store: &SessionStore,
    ) -> Result<Admission, IdsError> {
        if !batch.updates.is_empty() {
            ids.take(batch.updates.len(), store)?;
        }
        let c = &self.counts;
        c.batches.fetch_add(1, Ordering::Relaxed);
        c.updates
            .fetch_add(batch.updates.len() as u64, Ordering::Relaxed);
        let now = unix_ms();
        let mut id_lines = Vec::new();
        let mut seen_lines = Vec::new();
        for update in &batch.updates {
            let f = update.facts();
            let counter = match f.box_kind {
                _ if !sequenced(update) => &c.unsequenced,
                BoxKind::Common => &c.common,
                BoxKind::Qts => &c.qts,
                BoxKind::Channel(_) | BoxKind::None => &c.channel,
            };
            counter.fetch_add(1, Ordering::Relaxed);
            let Some(chat) = test_chat(update, self.bot) else {
                continue;
            };
            let Some(id) = message_id(update) else {
                continue;
            };
            if chat == "private" {
                c.private_ids.fetch_add(1, Ordering::Relaxed);
            } else {
                c.channel_ids.fetch_add(1, Ordering::Relaxed);
            }
            id_lines.push(format!(
                "{now},{chat},{id},{},{},{}",
                f.constructor,
                f.pts.unwrap_or(0),
                origin(update)
            ));
            if let Some((seq, sent)) = f
                .text
                .and_then(|t| t.strip_prefix(HEARTBEAT))
                .and_then(|t| t.split_once(' '))
                .and_then(|(seq, sent)| Some((seq.parse::<u64>().ok()?, sent.parse::<u64>().ok()?)))
            {
                c.heartbeats.fetch_add(1, Ordering::Relaxed);
                *self.last_heartbeat.lock() = Instant::now();
                seen_lines.push(format!(
                    "{now},{chat},{id},{seq},{}",
                    now.saturating_sub(sent)
                ));
            }
        }
        for line in &id_lines {
            append(&self.data.join("ids.csv"), line);
        }
        for line in &seen_lines {
            append(&self.data.join("seen.csv"), line);
        }
        Ok(Admission::Admitted)
    }
}

async fn soak(args: &Args, paths: &Paths) -> Result<ExitCode, AnyError> {
    let data = args.path("data", || PathBuf::from("/data"));
    std::fs::create_dir_all(&data)?;
    let api_id = paths.api_id()?;
    let bot = paths.token()?.bot_id()?;
    let wedge = Duration::from_secs(args.num("wedge-secs", 1200)?);
    let events = data.join("events.csv");
    let event =
        |what: &str, detail: &str| append(&events, &format!("{},{what},{detail}", unix_ms()));
    event("start", &format!("pid {}", std::process::id()));
    let started = Instant::now();
    let sink = Arc::new(SoakSink {
        bot,
        data: data.clone(),
        counts: SoakCounts::default(),
        last_heartbeat: Mutex::new(Instant::now()),
    });
    let account = start_account(paths, api_id, Arc::clone(&sink) as Arc<dyn BatchSink>);
    let mut states = account.watch();
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let terminated = async move { term.recv().await };
    #[cfg(not(unix))]
    let terminated = async { tokio::signal::ctrl_c().await.ok() };
    tokio::pin!(terminated);
    let code = loop {
        tokio::select! {
            _ = &mut terminated => {
                event("stop", "SIGTERM");
                break 0;
            }
            changed = states.changed() => {
                if changed.is_err() {
                    event("exit", "the account's task ended");
                    break 4;
                }
                let state = states.borrow_and_update().clone();
                event("state", state.name());
            }
            _ = tick.tick() => {
                if let Some(f) = Footprint::sample() {
                    append(&data.join("footprint.csv"), &format!("{},{}", unix_ms(), f.csv()));
                }
                let s = account.status();
                let c = &sink.counts;
                append(&data.join("status.csv"), &format!(
                    "{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                    unix_ms(),
                    s.state.name(),
                    s.updates,
                    s.dropped.undecodable,
                    s.dropped.full,
                    s.catching_up.common,
                    s.catching_up.channels,
                    c.batches.load(Ordering::Relaxed),
                    c.common.load(Ordering::Relaxed),
                    c.qts.load(Ordering::Relaxed),
                    c.channel.load(Ordering::Relaxed),
                    c.unsequenced.load(Ordering::Relaxed),
                    c.heartbeats.load(Ordering::Relaxed),
                    c.private_ids.load(Ordering::Relaxed) + c.channel_ids.load(Ordering::Relaxed),
                ));
                if started.elapsed() > wedge && sink.last_heartbeat.lock().elapsed() > wedge {
                    event("wedge-exit", &format!("no heartbeat seen for {} s", wedge.as_secs()));
                    break 3;
                }
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(8), account.close()).await;
    event("closed", &code.to_string());
    Ok(ExitCode::from(code))
}

async fn heartbeat(args: &Args, paths: &Paths) -> Result<ExitCode, AnyError> {
    let data = args.path("data", || PathBuf::from("/data"));
    std::fs::create_dir_all(&data)?;
    let every = Duration::from_secs(args.num("interval-secs", 300)?);
    let expire_ms = args.num::<u64>("expire-secs", 6 * 3600)? * 1000;
    let bot = Bot::new(&paths.token()?)?;
    let sent_csv = data.join("sent.csv");
    let deleted_csv = data.join("deleted.csv");
    // (chat, id) → (sent unix ms, seq); the sequence goes on from the last
    // run. A heartbeat is matched to the account's sighting by its
    // sequence: in the private chat the account's message ids are not the
    // bot's.
    let mut pending: BTreeMap<(String, i32), (u64, u64)> = BTreeMap::new();
    let mut seq = 0u64;
    let mut last_sent = 0u64;
    for row in read_csv(&sent_csv) {
        if let [at, chat, id, s] = row.as_slice()
            && let (Ok(at), Ok(id), Ok(s)) = (at.parse::<u64>(), id.parse(), s.parse::<u64>())
        {
            pending.insert((chat.clone(), id), (at, s));
            seq = seq.max(s);
            last_sent = last_sent.max(at);
        }
    }
    for row in read_csv(&deleted_csv) {
        if let [_, chat, id, _] = row.as_slice()
            && let Ok(id) = id.parse()
        {
            pending.remove(&(chat.clone(), id));
        }
    }
    let every_ms = u64::try_from(every.as_millis()).unwrap_or(300_000);
    let mut next = last_sent + every_ms;
    loop {
        let now = unix_ms();
        if now >= next {
            seq += 1;
            for chat in ["private", "channel"] {
                let at = unix_ms();
                match bot
                    .send(chat_id(chat), &format!("{HEARTBEAT}{seq} {at}"))
                    .await
                {
                    Ok(id) => {
                        append(&sent_csv, &format!("{at},{chat},{id},{seq}"));
                        pending.insert((chat.to_owned(), id), (at, seq));
                    }
                    Err(e) => append(
                        &data.join("heartbeat-errors.csv"),
                        &format!("{at},{chat},{seq},{}", e.to_string().replace(',', ";")),
                    ),
                }
            }
            next = now.max(next) + every_ms;
        }
        // Delete what the account saw; give up waiting after `expire`.
        let seen: HashSet<(String, u64)> = read_csv(&data.join("seen.csv"))
            .into_iter()
            .filter_map(|r| Some((r.get(1)?.clone(), r.get(3)?.parse().ok()?)))
            .collect();
        let now = unix_ms();
        let due: Vec<((String, i32), bool)> = pending
            .iter()
            .filter_map(|(key, (at, seq))| {
                let was_seen = seen.contains(&(key.0.clone(), *seq));
                (was_seen || now.saturating_sub(*at) > expire_ms).then(|| (key.clone(), was_seen))
            })
            .collect();
        for ((chat, id), was_seen) in due {
            if bot.delete(chat_id(&chat), &[id]).await.is_ok() {
                append(
                    &deleted_csv,
                    &format!(
                        "{},{chat},{id},{}",
                        unix_ms(),
                        if was_seen { "seen" } else { "expired" }
                    ),
                );
                pending.remove(&(chat, id));
            }
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn baseline(args: &Args, _paths: &Paths) -> Result<ExitCode, AnyError> {
    let data = args.path("data", || PathBuf::from("/data"));
    std::fs::create_dir_all(&data)?;
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        if let Some(f) = Footprint::sample() {
            append(
                &data.join("baseline.csv"),
                &format!("{},{}", unix_ms(), f.csv()),
            );
        }
    }
}

fn read_csv(path: &Path) -> Vec<Vec<String>> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .map(|l| l.split(',').map(str::to_owned).collect())
        .collect()
}

// ---------------------------------------------------------------------------
// soak-report

/// Footprint rows of one process: (unix ms, rss kB, anon kB, threads, cpu
/// ms, fds, sockets).
type Row = (u64, u64, u64, u64, u64, u64, u64);

fn footprint_rows(path: &Path, from: u64, to: u64) -> Vec<Row> {
    read_csv(path)
        .into_iter()
        .filter_map(|r| {
            let n: Vec<u64> = r.iter().filter_map(|v| v.parse().ok()).collect();
            (n.len() == 7 && n[0] >= from && n[0] < to)
                .then(|| (n[0], n[1], n[2], n[3], n[4], n[5], n[6]))
        })
        .collect()
}

fn footprint_summary(name: &str, rows: &[Row]) -> String {
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return format!("{name}: no samples");
    };
    let max = |f: fn(&Row) -> u64| rows.iter().map(f).max().unwrap_or(0);
    // CPU time restarts with the process: sum each run's growth.
    let mut cpu = 0;
    for w in rows.windows(2) {
        cpu += if w[1].4 >= w[0].4 {
            w[1].4 - w[0].4
        } else {
            w[1].4
        };
    }
    let span = last.0.saturating_sub(first.0).max(1);
    #[allow(clippy::cast_precision_loss)]
    let pct = cpu as f64 * 100.0 / span as f64;
    let mb = |kb: u64| format!("{:.1}", kb as f64 / 1024.0);
    format!(
        "{name}: samples {}; resident MB first {} last {} max {}; private (anon) MB first {} last {} max {}; threads last {} max {}; sockets last {} max {}; fds max {}; CPU {} s over {:.1} h ({pct:.3} % of one core)",
        rows.len(),
        mb(first.1),
        mb(last.1),
        mb(max(|r| r.1)),
        mb(first.2),
        mb(last.2),
        mb(max(|r| r.2)),
        last.3,
        max(|r| r.3),
        last.6,
        max(|r| r.6),
        max(|r| r.5),
        cpu / 1000,
        span as f64 / 3_600_000.0,
    )
}

#[allow(clippy::too_many_lines)]
fn soak_report(args: &Args, _paths: &Paths) -> Result<(), AnyError> {
    let data = args.path("data", || PathBuf::from("/data"));
    let events = read_csv(&data.join("events.csv"));
    let first_start = events
        .iter()
        .find(|r| r.get(1).is_some_and(|e| e == "start"))
        .and_then(|r| r[0].parse::<u64>().ok())
        .ok_or("no start event in events.csv")?;
    let from: u64 = args.num("from-ms", first_start)?;
    let hours: f64 = args.num("hours", 24.0)?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let to = from + (hours * 3_600_000.0) as u64;
    let now = unix_ms();
    let in_window = |r: &Vec<String>| {
        r.first()
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|t| t >= from && t < to)
    };
    println!(
        "window: {from} → {to} ms ({hours} h); now {now}{}",
        if now < to { " (window still open)" } else { "" }
    );

    let sent: Vec<Vec<String>> = read_csv(&data.join("sent.csv"))
        .into_iter()
        .filter(in_window)
        .collect();
    let seen: Vec<Vec<String>> = read_csv(&data.join("seen.csv"))
        .into_iter()
        .filter(|r| {
            r.first()
                .and_then(|v| v.parse::<u64>().ok())
                .is_some_and(|t| t >= from)
        })
        .collect();
    // By (chat, sequence): the private chat's ids differ between the bot
    // and the account.
    let seen_seqs: HashSet<(String, String)> = seen
        .iter()
        .filter_map(|r| Some((r.get(1)?.clone(), r.get(3)?.clone())))
        .collect();
    for chat in ["private", "channel"] {
        let mine: Vec<&Vec<String>> = sent.iter().filter(|r| r[1] == chat).collect();
        // A heartbeat sent in the last 10 minutes may still be on its way.
        let settled: Vec<&&Vec<String>> = mine
            .iter()
            .filter(|r| {
                r[0].parse::<u64>()
                    .is_ok_and(|t| t + 600_000 < now.min(to + 600_000))
            })
            .collect();
        let missing: Vec<String> = settled
            .iter()
            .filter(|r| !seen_seqs.contains(&(r[1].clone(), r[3].clone())))
            .map(|r| r[3].clone())
            .collect();
        let mut lat: Vec<u64> = seen
            .iter()
            .filter(|r| r[1] == chat && in_window(r))
            .filter_map(|r| r.get(4)?.parse().ok())
            .collect();
        lat.sort_unstable();
        let pick = |q: usize| {
            lat.get((lat.len() * q / 100).min(lat.len().saturating_sub(1)))
                .copied()
        };
        let fmt = |v: Option<u64>| {
            v.map_or_else(
                || "-".to_owned(),
                |ms| format!("{:.1} s", ms as f64 / 1000.0),
            )
        };
        println!(
            "heartbeats {chat}: sent {} (settled {}), seen {}, missing {} (seq {:?}); latency p50 {} p95 {} max {}",
            mine.len(),
            settled.len(),
            lat.len(),
            missing.len(),
            missing,
            fmt(pick(50)),
            fmt(pick(95)),
            fmt(lat.last().copied()),
        );
    }
    let deleted = read_csv(&data.join("deleted.csv"));
    let deleted_in: Vec<&Vec<String>> = deleted.iter().filter(|r| in_window(r)).collect();
    println!(
        "deleted: {} once seen, {} expired unseen",
        deleted_in
            .iter()
            .filter(|r| r.get(3).is_some_and(|v| v == "seen"))
            .count(),
        deleted_in
            .iter()
            .filter(|r| r.get(3).is_some_and(|v| v == "expired"))
            .count(),
    );
    let errors = read_csv(&data.join("heartbeat-errors.csv"))
        .into_iter()
        .filter(in_window)
        .count();
    println!("heartbeat send errors: {errors}");

    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut states: BTreeMap<String, usize> = BTreeMap::new();
    for r in events.iter().filter(|r| in_window(r)) {
        let kind = r.get(1).cloned().unwrap_or_default();
        if kind == "state" {
            *states
                .entry(r.get(2).cloned().unwrap_or_default())
                .or_default() += 1;
        }
        *kinds.entry(kind).or_default() += 1;
    }
    println!("account events: {kinds:?}; states entered: {states:?}");
    let chaos: Vec<Vec<String>> = read_csv(&data.join("chaos.csv"))
        .into_iter()
        .filter(|r| {
            r.first()
                .and_then(|v| v.parse::<u64>().ok())
                .is_some_and(|t| t * 1000 >= from && t * 1000 < to)
        })
        .collect();
    let mut chaos_kinds: BTreeMap<String, usize> = BTreeMap::new();
    for r in &chaos {
        *chaos_kinds
            .entry(r.get(1).cloned().unwrap_or_default())
            .or_default() += 1;
    }
    println!("chaos events: {chaos_kinds:?}");
    for r in &chaos {
        if r.get(1).is_some_and(|k| k != "poll") {
            println!("  chaos {}", r.join(","));
        }
    }

    let status: Vec<Vec<String>> = read_csv(&data.join("status.csv"))
        .into_iter()
        .filter(in_window)
        .collect();
    if let Some(last) = status.last() {
        println!(
            "last status row: state {} updates {} dropped undecodable {} full {} catching up common {} channels {}",
            last.get(1).map_or("-", String::as_str),
            last.get(2).map_or("-", String::as_str),
            last.get(3).map_or("-", String::as_str),
            last.get(4).map_or("-", String::as_str),
            last.get(5).map_or("-", String::as_str),
            last.get(6).map_or("-", String::as_str),
        );
    }
    println!(
        "{}",
        footprint_summary(
            "account",
            &footprint_rows(&data.join("footprint.csv"), from, to)
        )
    );
    println!(
        "{}",
        footprint_summary(
            "baseline",
            &footprint_rows(&data.join("baseline.csv"), from, to)
        )
    );

    let ids: Vec<Vec<String>> = read_csv(&data.join("ids.csv"))
        .into_iter()
        .filter(in_window)
        .collect();
    let mut unique: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
    for r in &ids {
        if let (Some(chat), Some(id)) = (r.get(1), r.get(2).and_then(|v| v.parse().ok())) {
            unique
                .entry(if chat == "private" {
                    "private"
                } else {
                    "channel"
                })
                .or_default()
                .insert(id);
        }
    }
    println!(
        "received message ids: private {} unique ({} rows), channel {} unique",
        unique.get("private").map_or(0, BTreeSet::len),
        ids.iter()
            .filter(|r| r.get(1).is_some_and(|c| c == "private"))
            .count(),
        unique.get("channel").map_or(0, BTreeSet::len),
    );
    if let Some(out) = args.get("ids-out") {
        let mut f = File::create(out)?;
        for (chat, set) in &unique {
            for id in set {
                writeln!(f, "{chat} {id}")?;
            }
        }
        println!("ids written to {out}");
    }
    Ok(())
}
