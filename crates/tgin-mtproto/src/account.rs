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

use crate::decode::BoxKind;
use crate::media::{AccountSessions, Connect, KeyStore, MediaConnected, Session};
use crate::rpc::{
    AccountRpc, AffectedSink, BoxFuture, Charge, ErrorClass, InvokeError, Route, classify,
};
use crate::schema::API;
use crate::updates::CatchingUp;
use crate::{
    Admission, AuthKey, Batch, BatchSink, DcOption, Dropped, Fetch, InitParams, Pushes, RpcOutcome,
    Sender, SenderError, SessionError, SessionStore, UpdateIds, UpdateManager,
};

#[cfg(test)]
mod tests;

/// The first wait before a failed account starts again; it doubles per
/// failure up to [`BACKOFF_MAX`]. A run longer than that resets it.
pub const BACKOFF_FIRST: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A 420 without a number in its message.
const DEFAULT_FLOOD_WAIT: Duration = Duration::from_secs(60);
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

/// Why an account failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Another account of this process is logged in as the same Telegram
    /// user: running one user twice would make the sessions fight over the
    /// update stream. Not retried.
    DuplicateAccount,
    /// Telegram refused the main session's init call (a 4xx other than a
    /// flood wait or a lost authorization, `API_ID_INVALID` say). Not
    /// retried.
    InitRefused,
    /// Another process holds the session directory.
    SessionHeld,
    /// The session store failed.
    Storage,
    /// Telegram could not be reached.
    Connect,
    /// The account's task panicked.
    Panicked,
}

impl Failure {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateAccount => "duplicate_account",
            Self::InitRefused => "init_refused",
            Self::SessionHeld => "session_held",
            Self::Storage => "storage",
            Self::Connect => "connect",
            Self::Panicked => "panicked",
        }
    }

    /// Whether the account tries again on its own (after a backoff).
    #[must_use]
    pub fn retried(self) -> bool {
        !matches!(self, Self::DuplicateAccount | Self::InitRefused)
    }
}

/// An operation of this process that holds an account's session, so the
/// account waits for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionUse {
    Login,
    Logout,
}

impl SessionUse {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Logout => "logout",
        }
    }
}

/// Where an account stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountState {
    /// No login is recorded, or an operation of this process (`held_by`)
    /// holds the session: the account starts once it ends.
    LoggedOut {
        held_by: Option<SessionUse>,
    },
    /// Opening the session, or the main session is not connected.
    Connecting,
    Running,
    /// Telegram refused the main session's init call with a flood wait.
    FloodWait {
        until: SystemTime,
    },
    /// Telegram answered `FROZEN_*`: the account is read-only. It stays so
    /// until the account restarts, is logged out or needs a new login.
    Frozen,
    /// Telegram no longer accepts the account's authorization: log it out
    /// and in again.
    NeedsLogin,
    /// See [`Failure::retried`].
    Failed(Failure),
}

impl AccountState {
    /// Every state's name, the `state` label's values.
    pub const NAMES: [&'static str; 7] = [
        "logged_out",
        "connecting",
        "running",
        "flood_wait",
        "frozen",
        "needs_login",
        "failed",
    ];

    #[must_use]
    pub fn name(&self) -> &'static str {
        Self::NAMES[self.index()]
    }

    /// The position of [`name`](Self::name) in [`NAMES`](Self::NAMES).
    #[must_use]
    pub fn index(&self) -> usize {
        match self {
            Self::LoggedOut { .. } => 0,
            Self::Connecting => 1,
            Self::Running => 2,
            Self::FloodWait { .. } => 3,
            Self::Frozen => 4,
            Self::NeedsLogin => 5,
            Self::Failed(_) => 6,
        }
    }

    /// Whether the account holds, or will take again on its own, its
    /// session: logins and logouts must wait.
    #[must_use]
    pub fn active(&self) -> bool {
        match self {
            Self::LoggedOut { .. } | Self::NeedsLogin => false,
            Self::Failed(failure) => failure.retried(),
            Self::Connecting | Self::Running | Self::FloodWait { .. } | Self::Frozen => true,
        }
    }
}

/// An account's status: no secret, no content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountStatus {
    pub name: Arc<str>,
    pub state: AccountState,
    /// The one layer the account serves.
    pub layer: i32,
    /// Once the session was read.
    pub home_dc: Option<i32>,
    pub catching_up: CatchingUp,
    /// Updates admitted.
    pub updates: u64,
    /// Updates dropped, by reason.
    pub dropped: Dropped,
}

/// Says which operation of this process, if any, holds an account's
/// session.
type LoginCheck = Box<dyn Fn(&str) -> Option<SessionUse> + Send + Sync>;

/// The accounts of one process: which Telegram user each runs as, and
/// which have a login under way.
#[derive(Default)]
pub struct Registry {
    users: Mutex<HashMap<i64, Arc<str>>>,
    logins: Option<LoginCheck>,
    /// Accounts refused as duplicates, woken when a user is released.
    duplicates: Mutex<Vec<Weak<Shared>>>,
}

impl Registry {
    /// `logins(account)` says whether a login of this process holds, or is
    /// about to hold, the account's session.
    #[must_use]
    pub fn with_logins(
        logins: impl Fn(&str) -> Option<SessionUse> + Send + Sync + 'static,
    ) -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
            logins: Some(Box::new(logins)),
            duplicates: Mutex::new(Vec::new()),
        }
    }

    /// Claims `user` for `shared`'s account; the account already holding it
    /// otherwise, and then `shared` is woken when a user is released. The
    /// waiter is registered under the same lock as the claim, so a release
    /// cannot fall between the refusal and the registration.
    fn claim(&self, user: i64, shared: &Arc<Shared>) -> Result<(), Arc<str>> {
        let account = &shared.name;
        let mut users = self.users.lock();
        users.retain(|_, holder| holder != account);
        match users.get(&user) {
            Some(holder) => {
                // Lock order: `users`, then `duplicates`.
                self.duplicates.lock().push(Arc::downgrade(shared));
                Err(Arc::clone(holder))
            }
            None => {
                users.insert(user, Arc::clone(account));
                Ok(())
            }
        }
    }

    /// Forgets `account`'s user; when it held one, the accounts refused as
    /// duplicates look again.
    fn release(&self, account: &str) {
        let parked = {
            let mut users = self.users.lock();
            let before = users.len();
            users.retain(|_, holder| &**holder != account);
            if users.len() == before {
                return;
            }
            std::mem::take(&mut *self.duplicates.lock())
        };
        for shared in parked.iter().filter_map(Weak::upgrade) {
            shared.wake.notify_one();
        }
    }

    fn held_by(&self, account: &str) -> Option<SessionUse> {
        self.logins.as_ref().and_then(|logins| logins(account))
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("users", &self.users.lock().len())
            .finish_non_exhaustive()
    }
}

/// A running account. Clones share it; the last one dropped stops it.
#[derive(Clone)]
pub struct Account(Arc<Inner>);

struct Inner {
    shared: Arc<Shared>,
    ingest: CancellationToken,
    close: CancellationToken,
    /// Behind an async lock: a close cut short leaves it in place.
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    abort: AbortHandle,
    rpc: Arc<RpcPort>,
    affected: Arc<AffectedPort>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.ingest.cancel();
        self.close.cancel();
    }
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("name", &self.0.shared.name)
            .field("state", &*self.0.shared.state.borrow())
            .finish()
    }
}

impl Account {
    /// Starts the account's task on the current runtime. `sink` admits its
    /// batches; `network` reaches Telegram; `registry` is shared by every
    /// account of the process.
    #[must_use]
    pub fn start(
        spec: AccountSpec,
        sink: Arc<dyn BatchSink>,
        network: Arc<dyn Network>,
        registry: Arc<Registry>,
    ) -> Self {
        let shared = Arc::new(Shared::new(Arc::from(spec.name)));
        let (affected_tx, affected_rx) = mpsc::channel(AFFECTED_QUEUE);
        let ingest = CancellationToken::new();
        let close = CancellationToken::new();
        let ctx = Arc::new(Ctx {
            shared: Arc::clone(&shared),
            dir: spec.session_dir,
            charge: spec.charge.unwrap_or_else(|| Arc::new(Unlimited)),
            sink,
            net: network,
            registry,
            ingest: ingest.clone(),
            close: close.clone(),
            affected: tokio::sync::Mutex::new(affected_rx),
        });
        let task = tokio::spawn(supervise(ctx));
        Self(Arc::new(Inner {
            rpc: Arc::new(RpcPort {
                shared: Arc::clone(&shared),
            }),
            affected: Arc::new(AffectedPort(affected_tx)),
            shared,
            ingest,
            close,
            abort: task.abort_handle(),
            task: tokio::sync::Mutex::new(Some(task)),
        }))
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.0.shared.name
    }

    /// Whether `other` is a handle of this same account (not another start
    /// of the same name).
    #[must_use]
    pub fn same(&self, other: &Account) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    #[must_use]
    pub fn state(&self) -> AccountState {
        self.0.shared.state.borrow().clone()
    }

    /// The state as it changes.
    #[must_use]
    pub fn watch(&self) -> watch::Receiver<AccountState> {
        self.0.shared.state.subscribe()
    }

    #[must_use]
    pub fn status(&self) -> AccountStatus {
        let s = &self.0.shared;
        let home = s.home_dc.load(Ordering::Relaxed);
        AccountStatus {
            name: Arc::clone(&s.name),
            state: s.state.borrow().clone(),
            layer: API.layer,
            home_dc: (home != 0).then_some(home),
            catching_up: CatchingUp {
                common: s.common.load(Ordering::Relaxed),
                channels: s.channels.load(Ordering::Relaxed),
            },
            updates: s.updates.load(Ordering::Relaxed),
            dropped: Dropped {
                undecodable: s.undecodable.load(Ordering::Relaxed),
                full: s.full.load(Ordering::Relaxed),
            },
        }
    }

    /// The account's RPC, for the endpoint: calls go to the session of the
    /// moment; while there is none they are not sent
    /// ([`SenderError::KeyRevoked`] when the account needs a login,
    /// [`SenderError::NotSent`] otherwise).
    #[must_use]
    pub fn rpc(&self) -> Arc<dyn AccountRpc> {
        Arc::clone(&self.0.rpc) as Arc<dyn AccountRpc>
    }

    /// Where the endpoint reports the pts of `messages.affected*` answers:
    /// the ingest feeds them to the update manager.
    #[must_use]
    pub fn affected(&self) -> Arc<dyn AffectedSink> {
        Arc::clone(&self.0.affected) as Arc<dyn AffectedSink>
    }

    /// The account's session changed outside it (a login or a logout of
    /// this process ended): a logged-out or failed account looks again.
    pub fn wake(&self) {
        self.0.shared.wake.notify_one();
    }

    /// Stops the ingest; RPC calls are served until [`close`](Self::close).
    pub fn stop_ingest(&self) {
        self.0.ingest.cancel();
    }

    /// Stops the account and waits until it released its session. True
    /// when its task ended by itself, false when [`abort`](Self::abort)
    /// ended it. A close cut short (a timeout) leaves the task reachable,
    /// so a later `abort` still ends it.
    pub async fn close(&self) -> bool {
        self.0.ingest.cancel();
        self.0.close.cancel();
        let mut task = self.0.task.lock().await;
        let Some(handle) = task.as_mut() else {
            return true;
        };
        let ended = handle.await.is_ok();
        *task = None;
        ended
    }

    /// Ends the account's task at once, without waiting.
    pub fn abort(&self) {
        self.0.ingest.cancel();
        self.0.close.cancel();
        self.0.abort.abort();
    }

    /// Parks the account so another holder (a logout) can use its session:
    /// stops the ingest (everything admitted is committed), waits up to
    /// `rpc_wait` for its RPC calls on their way to Telegram, then closes
    /// it, releasing the session. Start a new [`Account`] to run it again.
    pub async fn park(&self, rpc_wait: Duration) -> bool {
        self.0.ingest.cancel();
        let shared = &self.0.shared;
        let idle = async {
            loop {
                let notified = shared.rpc_idle.notified();
                if shared.rpc_calls.load(Ordering::SeqCst) == 0 {
                    return;
                }
                notified.await;
            }
        };
        if tokio::time::timeout(rpc_wait, idle).await.is_err() {
            tracing::warn!(account = %shared.name, "RPC calls still running when the account was parked");
        }
        self.close().await
    }
}

/// One RPC call on the account's sessions while it lives.
struct RpcCall<'a>(&'a Shared);

impl<'a> RpcCall<'a> {
    fn new(shared: &'a Shared) -> Self {
        shared.rpc_calls.fetch_add(1, Ordering::SeqCst);
        Self(shared)
    }
}

impl Drop for RpcCall<'_> {
    fn drop(&mut self) {
        if self.0.rpc_calls.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.rpc_idle.notify_waiters();
        }
    }
}

/// The budget when the spec gives none.
struct Unlimited;

impl Charge for Unlimited {
    fn charge(&self) -> Result<(), Duration> {
        Ok(())
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

impl Shared {
    fn new(name: Arc<str>) -> Self {
        Self {
            name,
            state: watch::Sender::new(AccountState::Connecting),
            home_dc: AtomicI32::new(0),
            common: AtomicBool::new(false),
            channels: AtomicUsize::new(0),
            updates: AtomicU64::new(0),
            undecodable: AtomicU64::new(0),
            full: AtomicU64::new(0),
            sessions: RwLock::new(None),
            frozen: AtomicBool::new(false),
            key_gone: AtomicBool::new(false),
            signal: Notify::new(),
            wake: Notify::new(),
            rpc_calls: AtomicUsize::new(0),
            rpc_idle: Notify::new(),
        }
    }

    fn set_state(&self, state: AccountState) {
        let name = state.name();
        let reason = match &state {
            AccountState::Failed(failure) => Some(failure.as_str()),
            _ => None,
        };
        if self.state.send_if_modified(|current| {
            let changed = *current != state;
            *current = state;
            changed
        }) {
            tracing::info!(account = %self.name, state = name, reason, "account state");
        }
    }

    /// Telegram answered on the main session.
    fn connected(&self) {
        if self.frozen.load(Ordering::Relaxed) {
            self.set_state(AccountState::Frozen);
        } else {
            self.set_state(AccountState::Running);
        }
    }

    /// Records that Telegram reports the account frozen; the ingest's next
    /// turn shows it (a flood wait first, while it lasts).
    fn mark_frozen(&self) {
        if !self.frozen.swap(true, Ordering::Relaxed) {
            tracing::warn!(account = %self.name, "Telegram reports the account frozen: it is read-only");
        }
    }

    fn main_key_gone(&self) {
        self.key_gone.store(true, Ordering::SeqCst);
        self.signal.notify_one();
    }

    /// The account's session ended.
    fn disconnected(&self) {
        *self.sessions.write() = None;
        self.common.store(false, Ordering::Relaxed);
        self.channels.store(0, Ordering::Relaxed);
    }
}

type Sessions = AccountSessions<Arc<dyn Session>, MediaVia, MediaKeys>;

/// Media sessions through the account's network, to the session's DC
/// options (the production table for the others).
struct MediaVia {
    net: Arc<dyn Network>,
    options: Vec<DcOption>,
}

impl Connect for MediaVia {
    type Session = Arc<dyn Session>;

    fn connect(
        &self,
        dc: i32,
        key: Option<AuthKey>,
    ) -> BoxFuture<'_, Result<MediaConnected<Arc<dyn Session>>, SenderError>> {
        Box::pin(async move {
            let option = dc_option(&self.options, dc).ok_or(SenderError::NotSent)?;
            // A media session's pushes are discarded.
            let c = self.net.connect(option, key, true).await?;
            Ok((c.session, c.new_key))
        })
    }
}

/// The store's media keys, while the account holds the store: an RPC call
/// still running when the account stops does not keep the session (and
/// its lock).
struct MediaKeys(Weak<SessionStore>);

impl MediaKeys {
    fn store(&self) -> Result<Arc<SessionStore>, SessionError> {
        self.0.upgrade().ok_or_else(|| {
            SessionError::Io(std::io::Error::other("the account's session is closed"))
        })
    }
}

impl KeyStore for MediaKeys {
    fn load(&self, dc: i32) -> Result<Option<AuthKey>, SessionError> {
        self.store()?.load(dc)
    }

    fn save(&self, dc: i32, key: &AuthKey) -> Result<(), SessionError> {
        self.store()?.save(dc, key)
    }

    fn remove(&self, dc: i32) -> Result<(), SessionError> {
        self.store()?.remove(dc)
    }
}

fn dc_option(options: &[DcOption], dc: i32) -> Option<DcOption> {
    options
        .iter()
        .find(|o| o.id == dc)
        .copied()
        .or_else(|| DcOption::production(dc))
}

/// The account's [`AccountRpc`]: the sessions of the moment.
struct RpcPort {
    shared: Arc<Shared>,
}

impl AccountRpc for RpcPort {
    fn layer(&self) -> i32 {
        API.layer
    }

    fn invoke(&self, route: Route, body: Bytes) -> BoxFuture<'_, Result<RpcOutcome, InvokeError>> {
        Box::pin(async move {
            let sessions = self.shared.sessions.read().clone();
            let Some(sessions) = sessions else {
                // An account that will not take its session again on its
                // own (logged out, needing a login, failed for good) is
                // unavailable; one on its way back is only not there yet.
                let unavailable = !self.shared.state.borrow().active();
                return Err(InvokeError::Sender(if unavailable {
                    SenderError::KeyRevoked
                } else {
                    SenderError::NotSent
                }));
            };
            let home = self.shared.home_dc.load(Ordering::Relaxed);
            let outcome = {
                let _call = RpcCall::new(&self.shared);
                sessions.invoke(route, body).await
            };
            observe(&self.shared, route, home, &outcome);
            outcome
        })
    }
}

/// What an RPC call's outcome tells of the account: the main session's key
/// gone, or the account frozen. A media session's refused key says nothing
/// about the main one.
fn observe(shared: &Shared, route: Route, home: i32, outcome: &Result<RpcOutcome, InvokeError>) {
    let main = match route {
        Route::Main => true,
        Route::Media(dc) => dc == home,
    };
    match outcome {
        Err(InvokeError::Sender(SenderError::KeyRevoked | SenderError::KeyUnknown)) if main => {
            shared.main_key_gone();
        }
        Ok(RpcOutcome::Error { code, message }) if main && lost_authorization(*code, message) => {
            shared.main_key_gone();
        }
        Ok(RpcOutcome::Error { code, message })
            if classify(*code, message) == ErrorClass::Frozen =>
        {
            // The ingest decides the state: woken, it shows `frozen` unless
            // a flood wait holds.
            shared.mark_frozen();
            shared.signal.notify_one();
        }
        _ => {}
    }
}

/// The account's [`AffectedSink`]: the pts go to the ingest, which owns
/// the update manager.
struct AffectedPort(mpsc::Sender<(BoxKind, i32, i32)>);

impl AffectedSink for AffectedPort {
    fn feed_affected(&self, box_kind: BoxKind, pts: i32, pts_count: i32) {
        // Full: the gap these pts leave is fetched instead.
        let _ = self.0.try_send((box_kind, pts, pts_count));
    }
}

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

/// Aborts a task when dropped: an aborted supervisor takes its run along.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn supervise(ctx: Arc<Ctx>) {
    let mut backoff = BACKOFF_FIRST;
    loop {
        if ctx.ingest.is_cancelled() {
            break;
        }
        let started = Instant::now();
        ctx.shared.key_gone.store(false, Ordering::SeqCst);
        let run = tokio::spawn(run(Arc::clone(&ctx)));
        let _abort = AbortOnDrop(run.abort_handle());
        let end = ended(&ctx, run.await);
        ctx.shared.disconnected();
        let at = match end {
            End::Stopped => break,
            End::Retry => {
                if started.elapsed() > BACKOFF_MAX {
                    backoff = BACKOFF_FIRST;
                }
                let at = Instant::now() + backoff;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                Some(at)
            }
            End::Until(at) => Some(at),
            End::Wait => {
                backoff = BACKOFF_FIRST;
                None
            }
        };
        tokio::select! {
            biased;
            () = ctx.ingest.cancelled() => break,
            () = ctx.shared.wake.notified(), if at.is_none() => {}
            () = sleep_until(at) => {}
        }
    }
    ctx.registry.release(&ctx.shared.name);
}

/// Sleeps until `at`; for ever without one.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn ended(ctx: &Ctx, joined: Result<End, JoinError>) -> End {
    match joined {
        Ok(end) => end,
        Err(e) if e.is_panic() => {
            tracing::error!(account = %ctx.shared.name, "the account's task panicked; it starts again");
            ctx.shared
                .set_state(AccountState::Failed(Failure::Panicked));
            End::Retry
        }
        Err(_) => End::Stopped,
    }
}

/// What the store says of the account.
enum Known {
    LoggedOut,
    /// A login is recorded but its home key is missing.
    Broken,
    LoggedIn {
        user: i64,
        dc: i32,
        key: Box<AuthKey>,
    },
}

fn known(store: &SessionStore) -> Result<Known, SessionError> {
    let Some(login) = store.login()? else {
        return Ok(Known::LoggedOut);
    };
    let Some(dc) = store.home_dc()? else {
        return Ok(Known::Broken);
    };
    Ok(match store.auth_key(dc)? {
        Some(key) => Known::LoggedIn {
            user: login.self_user_id,
            dc,
            key: Box::new(key),
        },
        None => Known::Broken,
    })
}

/// One run: open the session, connect, ingest until stopped or failed.
async fn run(ctx: Arc<Ctx>) -> End {
    let shared = &ctx.shared;
    let name = &shared.name;
    if let Some(held_by) = ctx.registry.held_by(name) {
        shared.set_state(AccountState::LoggedOut {
            held_by: Some(held_by),
        });
        return End::Wait;
    }
    shared.set_state(AccountState::Connecting);
    let store = match SessionStore::open(&ctx.dir) {
        Ok(store) => Arc::new(store),
        Err(SessionError::Held { .. }) => {
            if let Some(held_by) = ctx.registry.held_by(name) {
                shared.set_state(AccountState::LoggedOut {
                    held_by: Some(held_by),
                });
                return End::Wait;
            }
            tracing::warn!(account = %name, "the account's session is held by another process");
            shared.set_state(AccountState::Failed(Failure::SessionHeld));
            return End::Retry;
        }
        Err(e) => return ctx.storage_failed(&e),
    };
    let (user, dc, key) = match known(&store) {
        Ok(Known::LoggedIn { user, dc, key }) => (user, dc, *key),
        Ok(Known::LoggedOut) => {
            ctx.registry.release(name);
            shared.frozen.store(false, Ordering::Relaxed);
            shared.set_state(AccountState::LoggedOut { held_by: None });
            return End::Wait;
        }
        Ok(Known::Broken) => return ctx.needs_login(),
        Err(e) => return ctx.storage_failed(&e),
    };
    if let Err(holder) = ctx.registry.claim(user, shared) {
        tracing::error!(account = %name, other = %holder, "the account is logged in as the same Telegram user as another account; it does not start");
        shared.set_state(AccountState::Failed(Failure::DuplicateAccount));
        // `claim` registered it: woken when an account releases its user.
        return End::Wait;
    }
    shared.home_dc.store(dc, Ordering::Relaxed);
    let options: Vec<DcOption> = match store.dc_options() {
        Ok(options) => options
            .into_iter()
            .map(|(id, addr)| DcOption { id, addr })
            .collect(),
        Err(e) => return ctx.storage_failed(&e),
    };
    let Some(home) = dc_option(&options, dc) else {
        tracing::error!(account = %name, dc, "the account's home DC has no known address");
        shared.set_state(AccountState::Failed(Failure::Connect));
        return End::Retry;
    };
    let connected = tokio::select! {
        biased;
        () = ctx.ingest.cancelled() => return End::Stopped,
        connected = ctx.net.connect(home, Some(key), false) => connected,
    };
    let conn = match connected {
        Ok(conn) => conn,
        Err(e) => return ctx.main_failed(&e),
    };
    let state = match store.stored_state() {
        Ok(state) => state,
        Err(e) => return ctx.storage_failed(&e),
    };
    let ids = match UpdateIds::load(&store) {
        Ok(ids) => ids,
        Err(e) => return ctx.storage_failed(&e),
    };
    let sessions = Arc::new(AccountSessions::new(
        Arc::clone(&conn.session),
        dc,
        API.layer,
        MediaVia {
            net: Arc::clone(&ctx.net),
            options,
        },
        MediaKeys(Arc::downgrade(&store)),
        Arc::clone(&ctx.charge),
    ));
    *shared.sessions.write() = Some(sessions);
    tracing::info!(account = %name, dc, "account session open");
    let end = {
        let mut affected = ctx.affected.lock().await;
        let mut ingest = Ingest {
            ctx: &ctx,
            store: &store,
            session: conn.session,
            manager: UpdateManager::new(state).with_user_lookup({
                // Weak: a manager that outlives the run never holds the
                // session. A failed lookup counts as unknown, which only
                // asks one difference more.
                let store = Arc::downgrade(&store);
                move |user| {
                    store
                        .upgrade()
                        .is_some_and(|store| store.user_known(user).unwrap_or(false))
                }
            }),
            ids,
            fetches: JoinSet::new(),
            dropped: Dropped::default(),
            flood: None,
            reached: None,
            frozen: shared.frozen.load(Ordering::Relaxed),
        };
        ingest.run(conn.pushes, &mut affected).await
    };
    if end == End::Stopped {
        // Everything admitted is committed; the session serves RPC calls
        // until the account closes.
        tracing::info!(account = %name, "account ingest stopped");
        ctx.close.cancelled().await;
    }
    shared.disconnected();
    end
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

/// A 401 (`AUTH_KEY_UNREGISTERED`, `SESSION_REVOKED`…) or
/// `AUTH_KEY_DUPLICATED`: the authorization is gone. `SESSION_PASSWORD_NEEDED`
/// is a 401 of a login, never of a logged-in session.
fn lost_authorization(code: i32, message: &str) -> bool {
    (code == 401 && message != "SESSION_PASSWORD_NEEDED") || message == "AUTH_KEY_DUPLICATED"
}

/// Refusals of a channel's difference that say the account can no longer
/// read the channel.
const CHANNEL_GONE: [&str; 3] = [
    "CHANNEL_PRIVATE",
    "CHANNEL_INVALID",
    "CHANNEL_PUBLIC_GROUP_NA",
];

type Fetched = (Fetch, Result<RpcOutcome, SenderError>);

/// One run's ingest: the manager, its fetches and the batches it gives.
struct Ingest<'a> {
    ctx: &'a Ctx,
    store: &'a SessionStore,
    session: Arc<dyn Session>,
    manager: UpdateManager,
    ids: UpdateIds,
    fetches: JoinSet<Fetched>,
    /// What [`UpdateManager::dropped`] said last.
    dropped: Dropped,
    /// What [`UpdateManager::flood_wait`] said last.
    flood: Option<(Instant, SystemTime)>,
    /// Whether this turn's catch-up answers reached Telegram, for
    /// [`publish`](Self::publish).
    reached: Option<bool>,
    /// The account's frozen flag as [`publish`](Self::publish) last saw it.
    frozen: bool,
}

impl Ingest<'_> {
    async fn run(
        &mut self,
        mut pushes: Pushes,
        affected: &mut mpsc::Receiver<(BoxKind, i32, i32)>,
    ) -> End {
        loop {
            if let Err(end) = self.issue() {
                return end;
            }
            self.publish();
            let deadline = self.manager.deadline();
            let shared = &self.ctx.shared;
            let handled = tokio::select! {
                biased;
                () = self.ctx.ingest.cancelled() => return End::Stopped,
                () = shared.signal.notified() => {
                    if shared.key_gone.swap(false, Ordering::SeqCst) {
                        return self.ctx.needs_login();
                    }
                    Ok(())
                }
                push = pushes.recv() => match push {
                    Some(push) => {
                        let batches = self.manager.feed_push(push);
                        self.handle(&batches)
                    }
                    None => {
                        tracing::warn!(account = %shared.name, "the account's main session ended");
                        return End::Retry;
                    }
                },
                Some((box_kind, pts, count)) = affected.recv() => {
                    let batches = self.manager.feed_affected(box_kind, pts, count);
                    self.handle(&batches)
                }
                Some(done) = self.fetches.join_next(), if !self.fetches.is_empty() => {
                    match done {
                        Ok(fetched) => self.answer(fetched),
                        // A fetch lost its task: the manager would wait for
                        // it for ever, so this run ends.
                        Err(_) => Err(End::Retry),
                    }
                }
                () = tokio::time::sleep_until(deadline) => Ok(()),
            };
            if let Err(end) = handled {
                return end;
            }
        }
    }

    /// Runs every fetch the manager wants now.
    fn issue(&mut self) -> Result<(), End> {
        while let Some(fetch) = self.manager.next_action() {
            match fetch_body(fetch, self.store) {
                Ok(Some(body)) => {
                    let session = Arc::clone(&self.session);
                    self.fetches
                        .spawn(async move { (fetch, session.invoke(body).await) });
                }
                Ok(None) => {
                    tracing::debug!(account = %self.ctx.shared.name, "no access hash for a channel to catch up; its state is dropped");
                    let batches = self.manager.fetch_failed_permanently(fetch);
                    self.handle(&batches)?;
                }
                Err(e) => return Err(self.ctx.storage_failed(&e)),
            }
        }
        Ok(())
    }

    /// A fetch's outcome, back to the manager.
    fn answer(&mut self, (fetch, outcome): Fetched) -> Result<(), End> {
        let name = &self.ctx.shared.name;
        match outcome {
            Ok(RpcOutcome::Result(bytes)) => {
                self.reached = Some(true);
                let batches = match fetch {
                    Fetch::GetState => self.manager.feed_state(bytes),
                    Fetch::GetDifference { .. } => self.manager.feed_difference(bytes),
                    Fetch::GetChannelDifference { channel, .. } => {
                        self.manager.feed_channel_difference(channel, bytes)
                    }
                };
                self.handle(&batches)
            }
            Ok(RpcOutcome::Error { code, message }) => {
                self.reached = Some(true);
                tracing::debug!(account = %name, fetch = fetch_name(fetch), code, error = %message, "a catch-up call was refused");
                if lost_authorization(code, &message) {
                    return Err(self.ctx.needs_login());
                }
                match (classify(code, &message), fetch) {
                    (ErrorClass::Frozen, _) => {
                        self.ctx.shared.mark_frozen();
                        self.manager.fetch_failed(fetch);
                    }
                    (ErrorClass::FloodWait(x) | ErrorClass::SlowMode(x), _) => {
                        tracing::warn!(account = %name, fetch = fetch_name(fetch), seconds = x, "Telegram asks the account to wait before it catches up again");
                        self.manager
                            .fetch_flood_wait(fetch, Duration::from_secs(u64::from(x)));
                    }
                    (_, Fetch::GetChannelDifference { channel, .. })
                        if CHANNEL_GONE.contains(&message.as_str()) =>
                    {
                        self.manager.forget_channel(channel);
                    }
                    _ if (400..500).contains(&code) => {
                        let batches = self.manager.fetch_failed_permanently(fetch);
                        self.handle(&batches)?;
                    }
                    _ => self.manager.fetch_failed(fetch),
                }
                Ok(())
            }
            Err(e) => match e {
                SenderError::KeyRevoked | SenderError::KeyUnknown => Err(self.ctx.needs_login()),
                SenderError::InitRefused { code, message } if (400..500).contains(&code) => {
                    Err(self.ctx.init_refused(code, &message))
                }
                SenderError::BadRequest => {
                    let batches = self.manager.fetch_failed_permanently(fetch);
                    self.handle(&batches)
                }
                e => {
                    tracing::debug!(account = %name, fetch = fetch_name(fetch), error = %e, "a catch-up call failed; it is asked again");
                    self.reached = Some(false);
                    self.manager.fetch_failed(fetch);
                    Ok(())
                }
            },
        }
    }

    /// One manager call's batches: their marks and peers in one write,
    /// then each admitted or refused, in order, and the state that gives
    /// persisted.
    fn handle(&mut self, batches: &[Batch]) -> Result<(), End> {
        if batches.is_empty() {
            return Ok(());
        }
        let ctx = self.ctx;
        let stored = |r: Result<(), SessionError>| r.map_err(|e| ctx.storage_failed(&e));
        stored(self.store.mark_channels_behind(batches))?;
        for batch in batches {
            let admission = match ctx.sink.admit(batch, &mut self.ids, self.store) {
                Ok(admission) => admission,
                Err(e) => {
                    tracing::error!(account = %ctx.shared.name, error = %e, "the account's update ids could not be persisted");
                    ctx.shared.set_state(AccountState::Failed(Failure::Storage));
                    return Err(End::Retry);
                }
            };
            let delta = match admission {
                Admission::Admitted => {
                    let n = u64::try_from(batch.updates.len()).unwrap_or(u64::MAX);
                    ctx.shared.updates.fetch_add(n, Ordering::Relaxed);
                    self.manager.committed(batch.id)
                }
                Admission::Refused => self.manager.refused(batch.id),
            };
            if let Some(delta) = delta {
                stored(self.store.commit_state(&delta))?;
            }
        }
        Ok(())
    }

    /// The status the manager gives: boxes catching up, drops.
    fn publish(&mut self) {
        let shared = &self.ctx.shared;
        let catching = self.manager.catching_up();
        shared.common.store(catching.common, Ordering::Relaxed);
        shared.channels.store(catching.channels, Ordering::Relaxed);
        let dropped = self.manager.dropped();
        shared.undecodable.fetch_add(
            dropped.undecodable.saturating_sub(self.dropped.undecodable),
            Ordering::Relaxed,
        );
        shared.full.fetch_add(
            dropped.full.saturating_sub(self.dropped.full),
            Ordering::Relaxed,
        );
        self.dropped = dropped;
        // The state, decided once per turn: the common box waiting out
        // Telegram's flood wait shows it until then, whatever answered
        // meanwhile (a channel's fetch, an RPC call); otherwise whether the
        // last catch-up call reached Telegram.
        let flood = self.manager.flood_wait();
        if flood != self.flood.map(|(at, _)| at) {
            self.flood = flood.map(|at| {
                let until = SystemTime::now() + at.saturating_duration_since(Instant::now());
                (at, until)
            });
            if flood.is_none() && self.reached.is_none() {
                self.reached = Some(true);
            }
        }
        let frozen = shared.frozen.load(Ordering::Relaxed);
        let newly_frozen = frozen && !std::mem::replace(&mut self.frozen, frozen);
        match (self.flood, self.reached.take()) {
            (Some((_, until)), _) => shared.set_state(AccountState::FloodWait { until }),
            (None, Some(true)) => shared.connected(),
            (None, Some(false)) => shared.set_state(AccountState::Connecting),
            // An RPC answer reported the account frozen: Telegram answered.
            (None, None) if newly_frozen => shared.connected(),
            (None, None) => {}
        }
    }
}

fn fetch_name(fetch: Fetch) -> &'static str {
    match fetch {
        Fetch::GetState => "updates.getState",
        Fetch::GetDifference { .. } => "updates.getDifference",
        Fetch::GetChannelDifference { .. } => "updates.getChannelDifference",
    }
}

/// The request for `fetch`; `None` for a channel whose access hash is not
/// cached.
fn fetch_body(fetch: Fetch, store: &SessionStore) -> Result<Option<Bytes>, SessionError> {
    let body = match fetch {
        Fetch::GetState => tl::functions::updates::GetState {}.to_bytes(),
        Fetch::GetDifference { pts, qts, date } => tl::functions::updates::GetDifference {
            pts,
            pts_limit: None,
            pts_total_limit: None,
            date,
            qts,
            qts_limit: None,
        }
        .to_bytes(),
        Fetch::GetChannelDifference { channel, pts } => {
            let Some(access_hash) = store
                .peer(crate::decode::channel_dialog_id(channel))?
                .and_then(|peer| peer.access_hash)
            else {
                return Ok(None);
            };
            tl::functions::updates::GetChannelDifference {
                force: false,
                channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                    channel_id: channel,
                    access_hash,
                }),
                filter: tl::enums::ChannelMessagesFilter::Empty,
                pts,
                limit: CHANNEL_DIFFERENCE_LIMIT,
            }
            .to_bytes()
        }
    };
    Ok(Some(Bytes::from(body)))
}
