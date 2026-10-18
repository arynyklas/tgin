//! A fake Telegram Bot API server for end-to-end tests.
//!
//! It answers `/bot{token}/{method}` and `/file/bot{token}/{path}` the way the
//! official server does (error texts, `getUpdates` offsets and 409 conflicts,
//! webhook delivery and webhook replies), keeps per-token state, and has a
//! control API under `/fake/`:
//!
//! - `POST /fake/updates/{token}`: queue an Update (JSON object); assigns
//!   `update_id` when absent; answers `{"update_id":N}`.
//! - `GET /fake/calls?token=&method=&kind=`: the journal (calls, downloads,
//!   webhook deliveries), oldest first; empty filter values are ignored. A
//!   call's `status` is `null` while it is pending and 499 when its client
//!   went away first. `DELETE /fake/calls` clears it.
//! - `POST /fake/faults`: `{token, method, status, retry_after?, count,
//!   delay_ms?, description?}`; the next `count` calls answer `status`.
//! - `GET /fake/health`.
//!
//! Journal entries carry `local_file: true` when an `InputFile` parameter
//! (or a string anywhere in JSON carried by a parameter) starts with
//! `file:`, in either mode.
//!
//! [`Options::local_mode`] (`--local-mode` / `FAKE_TELEGRAM_LOCAL_MODE`,
//! with `--files-dir` / `FAKE_TELEGRAM_FILES_DIR`) imitates
//! `telegram-bot-api --local`: at start the files directory must accept a
//! new file (else start fails, as the server exits); uploads are written to
//! `{files_dir}/{bot dir}/documents/file_N[.ext]` and `getFile` answers that
//! absolute path. The bot dir is the token itself, or the token with ':'
//! replaced by '~' when the filesystem refuses ':' in a name (the server's
//! `mkdtemp("1:a…")` probe). Every `/file…` path answers 404; `sendDocument`
//! accepts a `file:` reference; `logOut` drops the pending updates and the
//! webhook (`close` only answers `true`). In the default mode `logOut` and
//! `close` only answer `true`.

mod api;
mod journal;
mod params;
mod state;
mod webhook;

use std::{io, net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    journal::Filter,
    state::{Fault, LocalFiles, Shared, bot_id},
};

/// How the fake behaves; the default is the cloud Bot API.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Behave like `telegram-bot-api --local`: uploads are written under
    /// `{files_dir}/{token}/` (`{token with ':' → '~'}/` where the
    /// filesystem refuses ':'), `getFile` answers absolute
    /// paths, `/file/bot…` is 404, `file:` references are accepted, and
    /// `logOut` drops the pending updates and the webhook.
    pub local_mode: bool,
    /// The server's `--dir`; required in local mode.
    pub files_dir: Option<PathBuf>,
}

/// A running fake server.
pub struct Server {
    addr: SocketAddr,
    cancel: CancellationToken,
    task: JoinHandle<io::Result<()>>,
}

impl Server {
    /// Serve on `listener` with the default (cloud) behaviour until
    /// [`Server::stop`].
    pub fn start(listener: TcpListener) -> io::Result<Self> {
        Self::start_with(listener, Options::default())
    }

    /// Serve on `listener` with `options` until [`Server::stop`].
    pub fn start_with(listener: TcpListener, options: Options) -> io::Result<Self> {
        let addr = listener.local_addr()?;
        let local = if options.local_mode {
            let dir = options.files_dir.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "local mode needs a files directory (--files-dir)",
                )
            })?;
            Some(local_files(&dir)?)
        } else {
            None
        };
        let cancel = CancellationToken::new();
        let shared = Arc::new(Shared::new(cancel.clone(), local).map_err(io::Error::other)?);
        let stop = cancel.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router(shared))
                .with_graceful_shutdown(async move { stop.cancelled().await })
                .await
        });
        Ok(Self { addr, cancel, task })
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop serving: parked polls return, webhook delivery stops, and the
    /// server task is awaited.
    pub async fn stop(self) -> io::Result<()> {
        self.cancel.cancel();
        self.task.await.map_err(io::Error::other)?
    }
}

/// Probe the files directory as `telegram-bot-api` does at startup: it must
/// accept a new file, and a bot's directory keeps the token's ':' only if
/// the filesystem accepts a ':' in a name.
fn local_files(dir: &std::path::Path) -> io::Result<LocalFiles> {
    std::fs::create_dir_all(dir)?;
    let root = std::path::absolute(dir)?;
    let file = root.join(format!(".fake-telegram-probe-{}", probe_suffix()));
    std::fs::File::create_new(&file)
        .and_then(|_| std::fs::remove_file(&file))
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "can't create files in the directory {}: {error}; use --files-dir to specify a writable files directory",
                    root.display()
                ),
            )
        })?;
    let colon = colon_allowed(&root.join(format!("1:a-fake-telegram-probe-{}", probe_suffix())))?;
    Ok(LocalFiles { root, colon })
}

/// A random probe-name suffix, so that neither a concurrent start nor a
/// probe left behind by a killed one is ever hit.
fn probe_suffix() -> String {
    format!("{:016x}", rand::random::<u64>())
}

/// Whether a directory named `path` (whose name holds a ':') can exist. One
/// that already does proves it, and is left alone.
fn colon_allowed(path: &std::path::Path) -> io::Result<bool> {
    match std::fs::create_dir(path) {
        Ok(()) => std::fs::remove_dir(path).map(|()| true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(true),
        Err(_) => Ok(false),
    }
}

fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/fake/health", get(health))
        .route("/fake/updates/{token}", post(inject))
        .route("/fake/calls", get(calls).delete(clear_calls))
        .route("/fake/faults", post(add_fault))
        .fallback(fallback)
        .with_state(shared)
}

async fn fallback(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    api::handle(shared, request).await
}

fn control_error(description: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"ok": false, "description": description})),
    )
        .into_response()
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"ok": true}))
}

async fn inject(
    State(shared): State<Arc<Shared>>,
    Path(token): Path<String>,
    body: Bytes,
) -> Response {
    let Some(id) = bot_id(&token) else {
        return control_error("invalid token");
    };
    match shared.bot(&token, id).inject(&body) {
        Some(update_id) => Json(json!({"update_id": update_id})).into_response(),
        None => control_error("the update must be a JSON object"),
    }
}

async fn calls(State(shared): State<Arc<Shared>>, Query(filter): Query<Filter>) -> Response {
    Json(shared.journal.list(&filter)).into_response()
}

async fn clear_calls(State(shared): State<Arc<Shared>>) -> Json<serde_json::Value> {
    shared.journal.clear();
    Json(json!({"ok": true}))
}

async fn add_fault(State(shared): State<Arc<Shared>>, body: Bytes) -> Response {
    let fault: Fault = match serde_json::from_slice(&body) {
        Ok(fault) => fault,
        Err(error) => return control_error(&error.to_string()),
    };
    if let Err(problem) = fault.validate() {
        return control_error(problem);
    }
    shared.add_fault(fault);
    Json(json!({"ok": true})).into_response()
}

#[cfg(test)]
mod tests {
    use super::{colon_allowed, local_files, probe_suffix};

    /// Probes left behind by killed runs (the old `{pid}-{n}` names and an
    /// existing probe path) never flip the bot directories to '~'.
    #[test]
    fn leftover_probe_keeps_the_colon_form() {
        let dir = tempfile::tempdir().unwrap();
        let leftover = dir.path().join("1:a-leftover");
        if std::fs::create_dir(&leftover).is_err() {
            // This filesystem refuses ':' in names: there is no leftover
            // to trip over, and the '~' form is right.
            assert!(!local_files(dir.path()).unwrap().colon);
            return;
        }
        // An existing probe path reads as "':' allowed" and is left alone.
        assert!(colon_allowed(&leftover).unwrap());
        assert!(leftover.is_dir());
        for n in 0..256 {
            let old = format!("1:a-fake-telegram-probe-{}-{n}", std::process::id());
            std::fs::create_dir(dir.path().join(old)).unwrap();
        }
        assert!(local_files(dir.path()).unwrap().colon);
    }

    #[test]
    fn probes_in_one_directory_do_not_collide() {
        assert_ne!(probe_suffix(), probe_suffix());
        let dir = tempfile::tempdir().unwrap();
        let first = local_files(dir.path()).unwrap();
        let second = local_files(dir.path()).unwrap();
        assert_eq!(first.colon, second.colon);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
