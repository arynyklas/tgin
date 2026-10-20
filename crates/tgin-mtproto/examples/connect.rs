//! Manual check, never run in CI: `cargo run -p tgin-mtproto --example connect`.
//!
//! Opens tgin's [`Sender`] on production DC 2 with a scratch auth key that
//! never logs in, then calls `help.getNearestDc` and `help.getConfig`
//! (a basic reachability check, on tgin's own sender). The first run makes the key
//! and stores it in the scratch session; later runs reuse it, which also
//! exercises `bad_server_salt` (a stored key starts without a salt).
//!
//! - Scratch session: `$TGIN_CONNECT_SCRATCH`, else `{temp}/tgin-connect-check`. The
//!   check refuses a session with a login.
//! - `TELEGRAM_API_ID` for `initConnection` comes from `$TGIN_API_ENV`, else
//!   `~/.tgin-secrets/telegram-api.env`. It is never printed.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use grammers_tl_types::{self as tl, Deserializable, Serializable};
use tgin_mtproto::{DcOption, InitParams, RpcOutcome, Sender, SessionStore};

const DC: i32 = 2;

type AnyError = Box<dyn Error>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("connect: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), AnyError> {
    let dir = std::env::var_os("TGIN_CONNECT_SCRATCH").map_or_else(
        || std::env::temp_dir().join("tgin-connect-check"),
        PathBuf::from,
    );
    let api_env = match std::env::var_os("TGIN_API_ENV") {
        Some(path) => PathBuf::from(path),
        None => home()?.join(".tgin-secrets").join("telegram-api.env"),
    };
    let api_id = api_id(&api_env)?;

    let store = SessionStore::open(&dir)?;
    if store.login()?.is_some() {
        return Err(format!(
            "{} holds a login; this check needs an unauthorized scratch key",
            dir.display()
        )
        .into());
    }
    let dc = DcOption::production(DC).ok_or("no production DC 2")?;
    let key = store.auth_key(DC)?;
    let reused = key.is_some();
    println!(
        "connect: production DC {DC} ({}), scratch session {}",
        dc.addr.ipv4,
        dir.display()
    );

    let mut init = InitParams::new(api_id);
    init.device_model = "tgin connect check".to_owned();
    let started = Instant::now();
    // No update calls here: the pushes are dropped, so they are discarded.
    let (sender, pushes, new_key) = Sender::connect(dc, key, init).await?;
    drop(pushes);
    if let Some(key) = &new_key {
        store.set_home_key(DC, key)?;
    }
    println!(
        "auth key: {} (256 bytes, in the scratch session, not printed); connected in {} ms",
        if reused { "reused" } else { "created" },
        started.elapsed().as_millis()
    );

    let tl::enums::NearestDc::Dc(nearest) =
        call(&sender, &tl::functions::help::GetNearestDc {}).await?;
    println!(
        "help.getNearestDc: this_dc={} nearest_dc={}",
        nearest.this_dc, nearest.nearest_dc
    );
    let tl::enums::Config::Config(config) =
        call(&sender, &tl::functions::help::GetConfig {}).await?;
    println!(
        "help.getConfig: this_dc={} test_mode={} dc_options={}",
        config.this_dc,
        config.test_mode,
        config.dc_options.len()
    );
    println!("layer: {}", tl::LAYER);
    Ok(())
}

/// One request through the sender, decoded with grammers' types.
async fn call<R: Deserializable>(
    sender: &Sender,
    request: &impl Serializable,
) -> Result<R, AnyError> {
    match sender.invoke(request.to_bytes().into()).await? {
        RpcOutcome::Result(bytes) => Ok(R::from_bytes(&bytes)?),
        RpcOutcome::Error { code, message } => Err(format!("rpc_error {code} {message}").into()),
    }
}

fn home() -> Result<PathBuf, AnyError> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| "no home directory".into())
}

/// `TELEGRAM_API_ID` from a `KEY=VALUE` file. Errors name the file and the
/// key, never a value.
fn api_id(path: &Path) -> Result<i32, AnyError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let value = text
        .lines()
        .map(str::trim)
        .map(|line| line.strip_prefix("export ").unwrap_or(line))
        .find_map(|line| line.strip_prefix("TELEGRAM_API_ID="))
        .ok_or_else(|| format!("{} has no TELEGRAM_API_ID", path.display()))?;
    value
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .parse()
        .map_err(|_| format!("{} has an invalid TELEGRAM_API_ID", path.display()).into())
}
