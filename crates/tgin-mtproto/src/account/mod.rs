//! An account's supervisor: one task per account owns its session store
//! (and so its lock), its main session, its update manager and its ingest,
//! and serves its RPC.
//!
//! # Running an account
//!
//! [`Account::start`] spawns the account's task on the current Tokio
//! runtime. The task:
//! 1. opens the session store; with no login recorded it reports
//!    [`AccountState::LoggedOut`], releases the store and waits for
//!    [`Account::wake`] (a login of this process ended);
//! 2. claims the logged-in user in the [`Registry`]: a second account of
//!    the same user fails with [`Failure::DuplicateAccount`] before it
//!    connects;
//! 3. connects the main session to the home DC, logged in, and serves RPC
//!    calls on it ([`Account::rpc`], with media sessions for other DCs);
//! 4. feeds every push to the [`UpdateManager`], runs its fetches
//!    (`updates.getState`, `getDifference`, `getChannelDifference`, on the
//!    main session only) and sleeps until its [`deadline`](UpdateManager::deadline);
//! 5. hands each call's batches to the [`BatchSink`]: first the marks and
//!    the peers of all of them in one write
//!    ([`SessionStore::mark_channels_behind`]), then, in order, `admit`,
//!    and the state `committed` or `refused` returns, persisted (the only
//!    write after admission).
//!
//! A connection that drops makes the sender push its connection-end
//! signal, and a `new_session_created` an `updatesTooLong`; the manager
//! answers either with `getDifference`, which reconnects (only the second
//! cuts a backed-off retry short). A fetch that fails is reported back to the manager
//! (`fetch_failed`, or `fetch_failed_permanently` for a refusal asking
//! again cannot fix), never dropped.
//!
//! A panic of the task is caught and the task restarts after 1 s, doubling
//! to 60 s; transient failures (the store, the network) do the same.
//!
//! # States
//!
//! - A transport −404 (`KeyRevoked`, `KeyUnknown`), a 401 or
//!   `AUTH_KEY_DUPLICATED` on the main session: [`AccountState::NeedsLogin`];
//!   the account releases its session and waits for a new login. A media
//!   session's refused key never does this: the main session still works.
//! - A 420 refusal of the main session's init call:
//!   [`AccountState::FloodWait`] until Telegram's time is up.
//! - Another 4xx refusal of the init call:
//!   [`Failure::InitRefused`], until the account is started again.
//! - A `FROZEN_*` answer: [`AccountState::Frozen`] (the account is
//!   read-only; ingest goes on).
//!
//! # Stopping
//!
//! [`Account::stop_ingest`] stops the ingest between two manager calls:
//! every batch admitted by then is committed, since admission and commit
//! run together. The main session keeps serving RPC calls until
//! [`Account::close`], which releases the session.
//!
//! Logs carry the account's name, constructor and box names, counts and
//! pts, never update content.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use grammers_tl_types::{self as tl, Serializable as _};
use parking_lot::{Mutex, RwLock};
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::{AbortHandle, JoinError, JoinHandle, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::authorization::gone as lost_authorization;
use crate::decode::BoxKind;
use crate::media::{AccountSessions, Connect, KeyStore, MediaConnected, Session};
use crate::retry::DEFAULT_FLOOD_WAIT;
use crate::rpc::{
    AccountRpc, AffectedSink, BoxFuture, Charge, ErrorClass, InvokeError, Route, classify,
};
use crate::schema::API;
use crate::updates::CatchingUp;
use crate::{
    Admission, AuthKey, Batch, BatchSink, DcOption, Dropped, Fetch, InitParams, Pushes, RpcOutcome,
    Sender, SenderError, SessionError, SessionStore, UpdateIds, UpdateManager,
};

mod ingest;
mod port;
mod state;
mod supervisor;

use ingest::Ingest;
#[cfg(test)]
use port::observe;
use port::{AffectedPort, MediaKeys, MediaVia, RpcPort, Unlimited, dc_option};
pub use state::{AccountState, AccountStatus, Failure, SessionUse};
pub use supervisor::{Account, Registry};

#[cfg(test)]
mod tests;

/// Messages of a channel difference per call (Telegram's most for users).
const CHANNEL_DIFFERENCE_LIMIT: i32 = 100;
/// `messages.affected*` pts waiting for the ingest; beyond it they are
/// dropped, and the gap they leave is fetched.
const AFFECTED_QUEUE: usize = 1024;

/// What an account runs from.
pub struct AccountSpec {
    /// Its name in logs, metrics and the status.
    pub name: String,
    /// Its session directory.
    pub session_dir: PathBuf,
    /// The request budget a media session's setup (its export and import)
    /// is charged to; `None`: unlimited.
    pub charge: Option<Arc<dyn Charge>>,
}

impl AccountSpec {
    #[must_use]
    pub fn new(name: impl Into<String>, session_dir: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            session_dir: session_dir.into(),
            charge: None,
        }
    }
}

/// An opened session: the session, its pushes (a media session's yield
/// nothing), and the key made for it when none was given.
pub struct Connection {
    pub session: Arc<dyn Session>,
    pub pushes: Pushes,
    pub new_key: Option<AuthKey>,
}

/// How an account reaches Telegram: [`TcpNetwork`] in production.
pub trait Network: Send + Sync + 'static {
    /// A logged-in session to `dc`: the main session, or with `media` a
    /// media session (every request in `invokeWithoutUpdates`). With
    /// `key: None` it makes a new key and returns it.
    fn connect(
        &self,
        dc: DcOption,
        key: Option<AuthKey>,
        media: bool,
    ) -> BoxFuture<'_, Result<Connection, SenderError>>;
}

/// Telegram over TCP: a [`Sender`] per session.
#[derive(Clone, Debug)]
pub struct TcpNetwork {
    init: InitParams,
}

impl TcpNetwork {
    /// Sessions opened with `init` (its `logged_in` and `without_updates`
    /// are set per session).
    #[must_use]
    pub fn new(init: InitParams) -> Self {
        Self { init }
    }
}

impl Network for TcpNetwork {
    fn connect(
        &self,
        dc: DcOption,
        key: Option<AuthKey>,
        media: bool,
    ) -> BoxFuture<'_, Result<Connection, SenderError>> {
        Box::pin(async move {
            let mut init = self.init.clone();
            init.logged_in = true;
            init.without_updates = media;
            let (sender, pushes, new_key) = Sender::connect(dc, key, init).await?;
            Ok(Connection {
                session: Arc::new(sender),
                pushes,
                new_key,
            })
        })
    }
}

/// What the account's handles and its task share.
struct Shared {
    name: Arc<str>,
    state: watch::Sender<AccountState>,
    /// 0 until the session was read.
    home_dc: AtomicI32,
    common: AtomicBool,
    channels: AtomicUsize,
    updates: AtomicU64,
    undecodable: AtomicU64,
    full: AtomicU64,
    /// The sessions RPC calls go to, while connected.
    sessions: RwLock<Option<Arc<Sessions>>>,
    /// A `FROZEN_*` answer came: the state shows `frozen` while connected,
    /// until the account restarts, logs out or needs a new login.
    frozen: AtomicBool,
    /// An RPC call on the main session found the key gone.
    key_gone: AtomicBool,
    signal: Notify,
    wake: Notify,
    /// RPC calls on the sessions now, and a wake when the last one ends.
    rpc_calls: AtomicUsize,
    rpc_idle: Notify,
}

type Sessions = AccountSessions<Arc<dyn Session>, MediaVia, MediaKeys>;

/// What the account's task runs with.
struct Ctx {
    shared: Arc<Shared>,
    dir: PathBuf,
    charge: Arc<dyn Charge>,
    sink: Arc<dyn BatchSink>,
    net: Arc<dyn Network>,
    registry: Arc<Registry>,
    ingest: CancellationToken,
    close: CancellationToken,
    affected: tokio::sync::Mutex<mpsc::Receiver<(BoxKind, i32, i32)>>,
}

/// How one run of the account ended.
#[derive(Debug, PartialEq, Eq)]
enum End {
    /// Stopped: no restart.
    Stopped,
    /// Start again after the backoff.
    Retry,
    /// Start again then.
    Until(Instant),
    /// Start again on [`Account::wake`].
    Wait,
}

impl Ctx {
    fn storage_failed(&self, e: &SessionError) -> End {
        tracing::error!(account = %self.shared.name, error = %e, "the account's session store failed");
        self.shared
            .set_state(AccountState::Failed(Failure::Storage));
        End::Retry
    }

    fn needs_login(&self) -> End {
        tracing::warn!(account = %self.shared.name, "Telegram no longer accepts the account's authorization; log it out and in again");
        self.registry.release(&self.shared.name);
        // A new login may be another account, or the same one unfrozen.
        self.shared.frozen.store(false, Ordering::Relaxed);
        self.shared.set_state(AccountState::NeedsLogin);
        End::Wait
    }

    /// The main session failed for good, or for a while.
    fn main_failed(&self, e: &SenderError) -> End {
        match e {
            SenderError::KeyRevoked | SenderError::KeyUnknown => self.needs_login(),
            SenderError::InitRefused { code, message } => self.init_refused(*code, message),
            e => {
                tracing::warn!(account = %self.shared.name, error = %e, "the account could not reach Telegram");
                self.shared
                    .set_state(AccountState::Failed(Failure::Connect));
                End::Retry
            }
        }
    }

    fn init_refused(&self, code: i32, message: &str) -> End {
        let name = &self.shared.name;
        if code == 420 {
            let wait = match classify(code, message) {
                ErrorClass::FloodWait(x) => Duration::from_secs(u64::from(x)),
                _ => DEFAULT_FLOOD_WAIT,
            };
            tracing::warn!(account = %name, seconds = wait.as_secs(), "Telegram asks the account to wait before it connects");
            self.shared.set_state(AccountState::FloodWait {
                until: SystemTime::now() + wait,
            });
            return End::Until(Instant::now() + wait);
        }
        if lost_authorization(code, message) {
            return self.needs_login();
        }
        if (400..500).contains(&code) {
            tracing::error!(account = %name, code, error = message, "Telegram refused the account's session");
            self.shared
                .set_state(AccountState::Failed(Failure::InitRefused));
            return End::Wait;
        }
        tracing::warn!(account = %name, code, error = message, "Telegram could not open the account's session");
        self.shared
            .set_state(AccountState::Failed(Failure::Connect));
        End::Retry
    }
}
