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
use serde::Deserialize;
use serde_json::json;
use tgin_core::config::Config;
use tgin_mtproto::login::{
    self, LoginError, LoginHandle, LoginParams, LoginState, LogoutError, Method, StartError,
    TcpDial,
};
use tgin_mtproto::{InitParams, SessionError, SessionStore};
use tokio::sync::Mutex;

use super::{LoginStatus, LoginUrl};

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
    /// Whether the account runs. Login and logout refuse while it does.
    fn running(&self, account: &str) -> bool;
    /// Opens the account's session and starts a login on it.
    fn start_login(
        &self,
        target: &Target,
        method: Method,
    ) -> BoxFuture<'static, Result<LoginHandle, Refusal>>;
    /// `auth.logOut`, then the session cleared.
    fn log_out(&self, target: &Target) -> BoxFuture<'static, Result<(), Refusal>>;
}

/// The production handle: Telegram's DCs over TCP. No account runs yet:
/// the supervisor that runs them (M3 Part 2, Task 7) answers `running`.
pub(crate) struct Sessions;

impl AccountHandle for Sessions {
    fn running(&self, _account: &str) -> bool {
        false
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
    /// A `tokio` mutex: starting a login may ask Telegram about a leftover
    /// key while it holds the account's entry.
    logins: Mutex<HashMap<String, Login>>,
}

impl Accounts {
    pub(crate) fn new(handle: Arc<dyn AccountHandle>) -> Self {
        Self {
            handle,
            logins: Mutex::new(HashMap::new()),
        }
    }

    async fn login(&self, name: &str) -> Option<Arc<LoginHandle>> {
        self.logins
            .lock()
            .await
            .get(name)
            .map(|login| Arc::clone(&login.handle))
    }

    /// After a reload: closes every login whose account is gone, or whose
    /// `session_dir` or `phone` changed (`target` gives the new config's).
    /// Closing waits until the login has released its session.
    pub(crate) async fn reconcile(&self, target: impl Fn(&str) -> Option<Target>) {
        let stale: Vec<(String, Login)> = {
            let mut logins = self.logins.lock().await;
            let names: Vec<String> = logins
                .iter()
                .filter(|(name, login)| {
                    !target(name).is_some_and(|new| login.target.same_login(&new))
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
        }
    }
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
    let handle = {
        let mut logins = accounts.logins.lock().await;
        match logins.get(&name) {
            Some(login) if !login.handle.state().is_final() => {
                if login.method != method {
                    return error(StatusCode::CONFLICT, "login in progress");
                }
                Arc::clone(&login.handle)
            }
            _ => {
                // An ended login has released the session.
                logins.remove(&name);
                match accounts.handle.start_login(&target, method).await {
                    Ok(h) => {
                        let h = Arc::new(h);
                        logins.insert(
                            name.clone(),
                            Login {
                                method,
                                target: target.clone(),
                                handle: Arc::clone(&h),
                            },
                        );
                        h
                    }
                    Err(refusal) => return refused(&name, &refusal),
                }
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
    match state.accounts().login(&name).await {
        Some(h) => login_status(&h.state()),
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
    let Some(h) = state.accounts().login(name).await else {
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
/// on Ctrl+C) and waits until it has released the session.
async fn cancel<S: AccountsState>(
    _: TokenConfigured,
    State(state): State<S>,
    Path(name): Path<String>,
) -> Response {
    if state.target(&name).is_none() {
        return unknown_account();
    }
    let login = state.accounts().logins.lock().await.remove(&name);
    match login {
        Some(login) => {
            login.close().await;
            tracing::info!(account = %name, "account login cancelled");
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
    // A login in progress ends first, releasing the session.
    let login = accounts.logins.lock().await.remove(&name);
    if let Some(login) = login {
        login.close().await;
    }
    match accounts.handle.log_out(&target).await {
        Ok(()) => {
            tracing::info!(account = %name, "account logged out");
            axum::Json(json!({ "logged_out": true })).into_response()
        }
        Err(refusal) => refused(&name, &refusal),
    }
}

#[cfg(test)]
mod tests;
