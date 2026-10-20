//! Where an account stands: its states, why it failed, its status, and
//! how the shared state changes.

use super::*;

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

impl Shared {
    pub(super) fn new(name: Arc<str>) -> Self {
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

    pub(super) fn set_state(&self, state: AccountState) {
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
    pub(super) fn connected(&self) {
        if self.frozen.load(Ordering::Relaxed) {
            self.set_state(AccountState::Frozen);
        } else {
            self.set_state(AccountState::Running);
        }
    }

    /// Records that Telegram reports the account frozen; the ingest's next
    /// turn shows it (a flood wait first, while it lasts).
    pub(super) fn mark_frozen(&self) {
        if !self.frozen.swap(true, Ordering::Relaxed) {
            tracing::warn!(account = %self.name, "Telegram reports the account frozen: it is read-only");
        }
    }

    pub(super) fn main_key_gone(&self) {
        self.key_gone.store(true, Ordering::SeqCst);
        self.signal.notify_one();
    }

    /// The account's session ended.
    pub(super) fn disconnected(&self) {
        *self.sessions.write() = None;
        self.common.store(false, Ordering::Relaxed);
        self.channels.store(0, Ordering::Relaxed);
    }
}
