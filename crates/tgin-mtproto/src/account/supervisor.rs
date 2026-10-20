//! The supervisor and the account's lifecycle: start, runs and restarts,
//! park and close, and the registry of the process's accounts.

use super::*;

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
    pub(super) fn claim(&self, user: i64, shared: &Arc<Shared>) -> Result<(), Arc<str>> {
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
    pub(super) fn release(&self, account: &str) {
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
    pub(super) fn storage_failed(&self, e: &SessionError) -> End {
        tracing::error!(account = %self.shared.name, error = %e, "the account's session store failed");
        self.shared
            .set_state(AccountState::Failed(Failure::Storage));
        End::Retry
    }

    pub(super) fn needs_login(&self) -> End {
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

    pub(super) fn init_refused(&self, code: i32, message: &str) -> End {
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
