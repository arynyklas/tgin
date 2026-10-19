//! `/v1/accounts/{name}/login|logout`: logging a Telegram account in and
//! out through the admin API (see the [module table](super)).
//!
//! A login runs on its own task, on the account's session, which it holds
//! (under the session's lock) until the login ends. The routes reach the
//! account through an [`AccountHandle`]: whether it runs, and the session
//! and Telegram behind it, so the tests put a scripted Telegram there.
//!
//! The `tg://login` URL leaves tgin only in a `qr` answer. Codes and
//! passwords are parsed from the body and handed to the login; no log
//! line, error or status carries any of them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use tgin_core::config::Config;
use tgin_mtproto::login::{
    self, LoginError, LoginHandle, LoginParams, LoginState, LogoutError, Method, StartError,
    TcpDial,
};
use tgin_mtproto::{InitParams, SessionError, SessionStore};

use super::{LoginStatus, LoginUrl};
use crate::accounts::AccountSources;

/// How long `POST …/login` waits for Telegram's first answer before it
/// answers `connecting`.
const SETTLE: Duration = Duration::from_secs(30);

/// What a login needs of an account's config.
#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub(crate) session_dir: PathBuf,
    pub(crate) params: LoginParams,
}

impl Target {
    /// Whether a login started for `self` still fits `other`: the same
    /// session, and the same phone for the identity gate.
    fn same_login(&self, other: &Self) -> bool {
        self.session_dir == other.session_dir && self.params.phone == other.params.phone
    }
}

/// The account `name` of `cfg`, as a login needs it.
pub(crate) fn target_of(cfg: &Config, name: &str) -> Option<Target> {
    let account = cfg.accounts.iter().find(|a| a.name == name)?;
    Some(Target {
        session_dir: account.session_dir.expose().clone(),
        params: LoginParams {
            api_id: account.api_id,
            api_hash: account.api_hash.expose().clone(),
            phone: account.phone.as_ref().map(|p| p.expose().clone()),
        },
    })
}

/// Why an account refused a login or logout.
#[derive(Debug)]
pub(crate) enum Refusal {
    /// A login is recorded: log out first.
    LoggedIn,
    /// Another process holds the session.
    Held,
    /// Telegram did not confirm the logout; nothing was cleared.
    Unconfirmed,
    /// A key from an unfinished login is stored, and Telegram did not say
    /// whether it is logged in; nothing was changed.
    Unverified,
    /// The session store failed.
    Store(SessionError),
}

impl From<SessionError> for Refusal {
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::Held { .. } => Self::Held,
            e => Self::Store(e),
        }
    }
}

/// The account behind the routes.
pub(crate) trait AccountHandle: Send + Sync + 'static {
    /// Whether the account runs (holds, or will take again on its own, its
    /// session). Login and logout refuse while it does.
    fn running(&self, account: &str) -> bool;
    /// Opens the account's session and starts a login on it.
    fn start_login(
        &self,
        target: &Target,
        method: Method,
    ) -> BoxFuture<'static, Result<LoginHandle, Refusal>>;
    /// `auth.logOut`, then the session cleared.
    fn log_out(&self, target: &Target) -> BoxFuture<'static, Result<(), Refusal>>;
    /// A login or logout of the account ended, or a login start was given
    /// up: the account looks at its session again (a logged-out account
    /// that is now logged in starts).
    fn changed(&self, account: &str);
}

/// The production handle: Telegram's DCs over TCP, and the accounts'
/// supervisors.
pub(crate) struct Sessions(pub(crate) Arc<AccountSources>);

impl AccountHandle for Sessions {
    fn running(&self, account: &str) -> bool {
        self.0.running(account)
    }

    fn start_login(
        &self,
        target: &Target,
        method: Method,
    ) -> BoxFuture<'static, Result<LoginHandle, Refusal>> {
        let dir = target.session_dir.clone();
        let params = target.params.clone();
        let dial = TcpDial::new(InitParams::new(params.api_id));
        Box::pin(async move {
            let store = SessionStore::open(&dir)?;
            login::start(store, dial, params, method)
                .await
                .map_err(start_refusal)
        })
    }

    fn log_out(&self, target: &Target) -> BoxFuture<'static, Result<(), Refusal>> {
        let dir = target.session_dir.clone();
        let dial = TcpDial::new(InitParams::new(target.params.api_id));
        Box::pin(async move {
            let store = SessionStore::open(&dir)?;
            login::log_out(&store, &dial).await.map_err(logout_refusal)
        })
    }

    fn changed(&self, account: &str) {
        self.0.wake(account);
    }
}

pub(crate) fn start_refusal(e: StartError) -> Refusal {
    match e {
        StartError::LoggedIn => Refusal::LoggedIn,
        StartError::Unverified(_) => Refusal::Unverified,
        StartError::Storage(e) => e.into(),
    }
}

pub(crate) fn logout_refusal(e: LogoutError) -> Refusal {
    match e {
        LogoutError::Unconfirmed(_) => Refusal::Unconfirmed,
        LogoutError::Storage(e) => e.into(),
    }
}

/// The logins in progress or ended, by account.
pub(crate) struct Accounts {
    handle: Arc<dyn AccountHandle>,
    /// Never held across an await: starting a login may wait on Telegram,
    /// so a start reserves its name with [`Entry::Starting`] and installs
    /// the login after.
    logins: Mutex<HashMap<String, Entry>>,
    /// Tells one start's reservation from a later one's.
    starts: AtomicU64,
}

/// An account's place in [`Accounts`].
enum Entry {
    /// A start is in progress: it may be asking Telegram about a leftover
    /// key, for up to `login::LEFTOVER_DEADLINE`.
    Starting {
        id: u64,
        target: Target,
    },
    Running(Login),
}

impl Entry {
    fn target(&self) -> &Target {
        match self {
            Self::Starting { target, .. } => target,
            Self::Running(login) => &login.target,
        }
    }

    /// Ends it: a running login is closed (see [`Login::close`]); a start
    /// in progress finds its reservation gone and closes what it started.
    async fn close(self) {
        if let Self::Running(login) = self {
            login.close().await;
        }
    }
}

impl Accounts {
    pub(crate) fn new(handle: Arc<dyn AccountHandle>) -> Self {
        Self {
            handle,
            logins: Mutex::new(HashMap::new()),
            starts: AtomicU64::new(0),
        }
    }

    /// The account's login: `Ok(None)` while one is starting.
    fn login(&self, name: &str) -> Option<Option<Arc<LoginHandle>>> {
        self.logins.lock().get(name).map(|entry| match entry {
            Entry::Starting { .. } => None,
            Entry::Running(login) => Some(Arc::clone(&login.handle)),
        })
    }

    /// After a reload: closes every login whose account is gone, or whose
    /// `session_dir` or `phone` changed (`target` gives the new config's).
    /// Closing waits until the login has released its session.
    pub(crate) async fn reconcile(&self, target: impl Fn(&str) -> Option<Target>) {
        let stale: Vec<(String, Entry)> = {
            let mut logins = self.logins.lock();
            let names: Vec<String> = logins
                .iter()
                .filter(|(name, entry)| {
                    !target(name).is_some_and(|new| entry.target().same_login(&new))
                })
                .map(|(name, _)| name.clone())
                .collect();
            names
                .into_iter()
                .filter_map(|name| logins.remove(&name).map(|login| (name, login)))
                .collect()
        };
        for (name, login) in stale {
            tracing::info!(account = %name, "account login closed: the account changed in the config");
            login.close().await;
            self.handle.changed(&name);
        }
    }

    /// Whether a login of the account `name` is starting or running: it
    /// holds, or is about to hold, the account's session.
    pub(crate) fn login_in_progress(&self, name: &str) -> bool {
        self.logins
            .lock()
            .get(name)
            .is_some_and(|entry| match entry {
                Entry::Starting { .. } => true,
                Entry::Running(login) => !login.handle.state().is_final(),
            })
    }
}

/// Tells the account when `login` ends (it may have logged it in). Holds
/// only the login's state, never the login.
fn watch_end(handle: Arc<dyn AccountHandle>, name: String, login: &LoginHandle) {
    let mut state = login.watch();
    tokio::spawn(async move {
        // An aborted login drops its sender: it ended too.
        let _ = state.wait_for(LoginState::is_final).await;
        handle.changed(&name);
    });
}

/// A login of the map, with the config it was started for.
struct Login {
    method: Method,
    target: Target,
    handle: Arc<LoginHandle>,
}

impl Login {
    /// Ends it and waits for its task when this is the last handle; other
    /// holders (a request in flight) see it end.
    async fn close(self) {
        match Arc::try_unwrap(self.handle) {
            Ok(handle) => handle.close().await,
            Err(shared) => shared.abort(),
        }
    }
}

/// A start's [`Entry::Starting`] reservation. Dropping it removes the
/// entry if it is still this start's reservation; once the login is
/// installed the entry is `Running`, so there is nothing to remove.
struct Reservation<'a> {
    accounts: &'a Accounts,
    name: &'a str,
    id: u64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let removed = {
            let mut logins = self.accounts.logins.lock();
            let ours = matches!(
                logins.get(self.name),
                Some(Entry::Starting { id, .. }) if *id == self.id
            );
            ours && logins.remove(self.name).is_some()
        };
        if removed {
            self.accounts.handle.changed(self.name);
        }
    }
}

/// What the routes need of their state.
pub(crate) trait AccountsState: Clone + Send + Sync + 'static {
    fn accounts(&self) -> &Accounts;
    /// The account `name` of the running config.
    fn target(&self, name: &str) -> Option<Target>;
    /// Whether the running config has an `admin.token`. Without one the
    /// account routes refuse (403): on a token-less loopback listener every
    /// local user and every web page (cross-site or rebound) could reach
    /// them.
    fn token_configured(&self) -> bool;
}

/// The extractor every account route takes first: the 403 without an
/// `admin.token`, before the account, Telegram or the session is touched.
pub(crate) struct TokenConfigured;

impl<S: AccountsState> FromRequestParts<S> for TokenConfigured {
    type Rejection = Response;

    async fn from_request_parts(_parts: &mut Parts, state: &S) -> Result<Self, Response> {
        if state.token_configured() {
            Ok(Self)
        } else {
            Err(error(
                StatusCode::FORBIDDEN,
                "admin.token required for account login",
            ))
        }
    }
}

/// The account routes, to be mounted behind the admin token.
pub(crate) fn routes<S: AccountsState>() -> Router<S> {
    Router::new()
        .route(
            "/v1/accounts/{name}/login",
            post(start::<S>).get(current::<S>),
        )
        .route("/v1/accounts/{name}/login/code", post(code::<S>))
        .route("/v1/accounts/{name}/login/password", post(password::<S>))
        .route("/v1/accounts/{name}/logout", post(logout::<S>))
        .route(
            "/v1/accounts/{name}/login",
            axum::routing::delete(cancel::<S>),
        )
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "error": message }))).into_response()
}

fn unknown_account() -> Response {
    error(StatusCode::NOT_FOUND, "unknown account")
}

fn no_login() -> Response {
    error(StatusCode::NOT_FOUND, "no login")
}

fn running() -> Response {
    error(StatusCode::CONFLICT, "account running")
}

/// A body that is not the expected JSON. The text is never echoed: it may
/// hold a password.
fn invalid_body() -> Response {
    error(StatusCode::BAD_REQUEST, "invalid body")
}

fn refused(account: &str, refusal: &Refusal) -> Response {
    match refusal {
        Refusal::LoggedIn => error(StatusCode::CONFLICT, "logged in"),
        Refusal::Held => error(StatusCode::CONFLICT, "session held"),
        Refusal::Unconfirmed => error(StatusCode::BAD_GATEWAY, "logout unconfirmed"),
        Refusal::Unverified => (
            StatusCode::CONFLICT,
            axum::Json(json!({
                "error": "session unverified",
                "description": "a key from an unfinished login is stored and Telegram did not say whether it is logged in; try again, or run `tgin admin logout <account> --yes`",
            })),
        )
            .into_response(),
        Refusal::Store(e) => {
            tracing::warn!(account, error = %e, "account session store failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "session store failed")
        }
    }
}

/// The admin API's view of `state`.
pub(crate) fn status(state: &LoginState) -> LoginStatus {
    match state {
        LoginState::Connecting => LoginStatus::Connecting,
        LoginState::Qr { url, expires } => LoginStatus::Qr {
            url: LoginUrl(url.expose().to_owned()),
            expires: i64::from(*expires),
        },
        LoginState::CodeSent { kind, timeout } => LoginStatus::CodeSent {
            kind: kind.as_str().to_owned(),
            timeout: *timeout,
        },
        LoginState::NeedPassword { hint } => LoginStatus::NeedPassword { hint: hint.clone() },
        LoginState::NeedEmailSetup => LoginStatus::NeedEmailSetup,
        LoginState::NeedEmailCode => LoginStatus::NeedEmailCode,
        LoginState::PaymentRequired => LoginStatus::PaymentRequired,
        LoginState::LoggedIn { user_id } => LoginStatus::LoggedIn { user_id: *user_id },
        LoginState::Failed(e) => LoginStatus::Failed {
            reason: e.code().to_owned(),
            message: e.to_string(),
        },
    }
}

fn answer(account: &str, state: &LoginState) -> Response {
    tracing::info!(account, state = state.name(), "account login");
    login_status(state)
}

/// A login state's answer. It may carry the `tg://login` URL, a secret, so
/// no cache keeps it.
fn login_status(state: &LoginState) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(status(state)),
    )
        .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum MethodName {
    Qr,
    Code,
}

#[derive(Deserialize)]
struct StartBody {
    method: MethodName,
}

async fn start<S: AccountsState>(
    _: TokenConfigured,
    State(state): State<S>,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let Some(target) = state.target(&name) else {
        return unknown_account();
    };
    let Ok(StartBody { method }) = serde_json::from_slice(&body) else {
        return invalid_body();
    };
    let method = match method {
        MethodName::Qr => Method::Qr,
        MethodName::Code => Method::Code,
    };
    let accounts = state.accounts();
    if accounts.handle.running(&name) {
        return running();
    }
    let id = accounts.starts.fetch_add(1, Ordering::Relaxed);
    let running = {
        let mut logins = accounts.logins.lock();
        match logins.get(&name) {
            Some(Entry::Starting { .. }) => {
                return error(StatusCode::CONFLICT, "login starting");
            }
            Some(Entry::Running(login)) if !login.handle.state().is_final() => {
                if login.method != method {
                    return error(StatusCode::CONFLICT, "login in progress");
                }
                Some(Arc::clone(&login.handle))
            }
            _ => {
                // An ended login has released the session.
                logins.insert(
                    name.clone(),
                    Entry::Starting {
                        id,
                        target: target.clone(),
                    },
                );
                None
            }
        }
    };
    let handle = match running {
        Some(handle) => handle,
        None => {
            // Removes the reservation on every way out (a refusal, a
            // client that goes away, a panic) unless the login is installed.
            let reservation = Reservation {
                accounts,
                name: &name,
                id,
            };
            let started = accounts.handle.start_login(&target, method).await;
            // No await while the map is locked.
            let installed = {
                let mut logins = accounts.logins.lock();
                let ours = matches!(
                    logins.get(&name),
                    Some(Entry::Starting { id: mine, .. }) if *mine == id
                );
                match started {
                    Ok(h) if ours => {
                        let h = Arc::new(h);
                        watch_end(Arc::clone(&accounts.handle), name.clone(), &h);
                        logins.insert(
                            name.clone(),
                            Entry::Running(Login {
                                method,
                                target,
                                handle: Arc::clone(&h),
                            }),
                        );
                        Ok(h)
                    }
                    Ok(h) => Err(Err(h)),
                    Err(refusal) => Err(Ok(refusal)),
                }
            };
            drop(reservation);
            match installed {
                Ok(h) => h,
                // Cancelled meanwhile (DELETE, or a reload changed the
                // account): the new login ends at once.
                Err(Err(h)) => {
                    h.close().await;
                    return error(StatusCode::CONFLICT, "login cancelled");
                }
                Err(Ok(refusal)) => return refused(&name, &refusal),
            }
        }
    };
    let state = tokio::time::timeout(SETTLE, handle.settled())
        .await
        .unwrap_or_else(|_| handle.state());
    answer(&name, &state)
}

async fn current<S: AccountsState>(
    _: TokenConfigured,
    State(state): State<S>,
    Path(name): Path<String>,
) -> Response {
    if state.target(&name).is_none() {
        return unknown_account();
    }
    match state.accounts().login(&name) {
        Some(Some(h)) => login_status(&h.state()),
        Some(None) => login_status(&LoginState::Connecting),
        None => no_login(),
    }
}

#[derive(Deserialize)]
struct CodeBody {
    code: String,
}

#[derive(Deserialize)]
struct PasswordBody {
    password: String,
}

async fn code<S: AccountsState>(
    _: TokenConfigured,
    State(state): State<S>,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let Ok(CodeBody { code }) = serde_json::from_slice(&body) else {
        return invalid_body();
    };
    submit(&state, &name, async |h: Arc<LoginHandle>| {
        h.submit_code(code).await
    })
    .await
}

async fn password<S: AccountsState>(
    _: TokenConfigured,
    State(state): State<S>,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let Ok(PasswordBody { password }) = serde_json::from_slice(&body) else {
        return invalid_body();
    };
    submit(&state, &name, async |h: Arc<LoginHandle>| {
        h.submit_password(password).await
    })
    .await
}

/// Hands a code or password to the account's login.
async fn submit<S: AccountsState>(
    state: &S,
    name: &str,
    hand: impl AsyncFnOnce(Arc<LoginHandle>) -> Result<LoginState, LoginError>,
) -> Response {
    if state.target(name).is_none() {
        return unknown_account();
    }
    let Some(Some(h)) = state.accounts().login(name) else {
        return no_login();
    };
    match hand(h).await {
        Ok(next) => answer(name, &next),
        Err(e) => {
            // A wrong password or code is spent on the real account.
            tracing::warn!(
                account = name,
                reason = e.code(),
                "account login: try again"
            );
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                axum::Json(json!({ "error": e.to_string(), "reason": e.code() })),
            )
                .into_response()
        }
    }
}

/// `DELETE …/login`: ends the account's login (`tgin admin login` sends it
/// on Ctrl+C). A running login is closed and has released the session when
/// the answer comes. A login still starting is cancelled at once, but its
/// start may hold the session a little longer, up to
/// `login::LEFTOVER_DEADLINE` (20 s) while it asks Telegram about a
/// leftover key; it then ends without being installed.
async fn cancel<S: AccountsState>(
    _: TokenConfigured,
    State(state): State<S>,
    Path(name): Path<String>,
) -> Response {
    if state.target(&name).is_none() {
        return unknown_account();
    }
    let login = state.accounts().logins.lock().remove(&name);
    match login {
        Some(login) => {
            login.close().await;
            tracing::info!(account = %name, "account login cancelled");
            state.accounts().handle.changed(&name);
            axum::Json(json!({ "cancelled": true })).into_response()
        }
        None => no_login(),
    }
}

async fn logout<S: AccountsState>(
    _: TokenConfigured,
    State(state): State<S>,
    Path(name): Path<String>,
) -> Response {
    let Some(target) = state.target(&name) else {
        return unknown_account();
    };
    let accounts = state.accounts();
    if accounts.handle.running(&name) {
        return running();
    }
    // A login in progress ends first, releasing the session; one still
    // starting holds the session until it is installed.
    let login = {
        let mut logins = accounts.logins.lock();
        if let Some(Entry::Starting { .. }) = logins.get(&name) {
            return error(StatusCode::CONFLICT, "login starting");
        }
        logins.remove(&name)
    };
    if let Some(login) = login {
        login.close().await;
    }
    match accounts.handle.log_out(&target).await {
        Ok(()) => {
            tracing::info!(account = %name, "account logged out");
            accounts.handle.changed(&name);
            axum::Json(json!({ "logged_out": true })).into_response()
        }
        Err(refusal) => refused(&name, &refusal),
    }
}

#[cfg(test)]
mod tests;
