//! Login on raw TL: QR (<https://core.telegram.org/api/qr-login>), the
//! code flow, and the shared 2FA SRP step (<https://core.telegram.org/api/srp>),
//! over tgin's own [`Sender`].
//!
//! Beyond the plain flows, it guards the account:
//! - **The identity gate.** With a `phone` in the config, the logged-in
//!   user's phone must match it. On a mismatch the new key is logged out
//!   (`auth.logOut`). Once Telegram confirms it, the session is reset and
//!   the login ends [`Failed`](LoginState::Failed)`(`[`LoginError::WrongAccount`]`)`;
//!   without a confirmation the key stays home, with no login, and the end
//!   is [`LoginError::WrongAccountNotLoggedOut`].
//! - **The login marker** (self user and `login_unix`) is written in one
//!   transaction ([`SessionStore::record_login`]), and only after the gate.
//! - **SRP** runs only once `p`/`g` pass grammers' safety check, `srp_B`
//!   and `secure_random` are at most 256 bytes (grammers pads them to 256
//!   and would panic on more) and `1 < g_b < p − 1`. Its secret `a` comes
//!   from the OS CSPRNG, `secure_random` only mixed in.
//! - **A probe after an I/O failure.** When `auth.checkPassword`,
//!   `auth.importLoginToken` or `auth.signIn` was written and no answer came,
//!   the login may have happened: the flow asks `users.getUsers([inputUserSelf])`
//!   on a fresh connection before it reports anything.
//!
//! The spike's local QR page and its delivery through a bot are not part of
//! tgin: the QR reaches the operator through the admin API and `tgin admin
//! login` only.
//!
//! # Secrets
//!
//! The `tg://login` URL ([`LoginUrl`]), login codes and passwords never
//! appear in `Debug`, `Display`, errors or logs. The phone is only
//! compared.
//!
//! # Pushes
//!
//! A login sender has no update manager. Its pushes, the sender's
//! synthetic `updatesTooLong` included, are drained and dropped on a task
//! of their own; only `updateLoginToken` (the QR was scanned) is noticed.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use bytes::Bytes;
use grammers_crypto::two_factor_auth::{calculate_2fa, check_p_and_g};
use grammers_tl_types::{self as tl, Deserializable, Identifiable as _, RemoteCall};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::authorization;
use crate::retry::{Wait, WaitKind};
use crate::{
    AuthKey, DcOption, InitParams, Peer, PeerKind, Pushes, RpcOutcome, Sender, SenderError,
    SessionError, SessionStore,
};

#[cfg(test)]
mod tests;

/// The DC a login starts on, as Telegram's own apps do; Telegram moves it
/// to the account's home DC when needed.
pub const FIRST_DC: i32 = 2;
/// The longest a QR token is shown before it is exported again.
const MAX_TOKEN_TTL: i64 = 30;
/// The longest `start` asks Telegram about a key an unfinished login left.
pub const LEFTOVER_DEADLINE: Duration = Duration::from_secs(20);
/// Wrong passwords, or wrong codes, that end a login.
pub const MAX_ATTEMPTS: u32 = 5;
/// The longest a QR login runs unscanned; then it ends `Failed(expired)`.
pub const QR_LIFETIME: Duration = Duration::from_secs(300);
/// The longest a login waits for a code or a password; then it ends
/// `Failed(expired)`.
pub const INPUT_WAIT: Duration = Duration::from_secs(300);
/// SRP's numbers are 2048 bits; grammers pads each to 256 bytes.
const SRP_MAX: usize = 256;

/// One connection a login talks over: [`Sender`] in production.
pub trait Link: Send + Sync + 'static {
    /// Sends one serialized TL function; see [`Sender::invoke`].
    fn invoke(&self, body: Bytes) -> impl Future<Output = Result<RpcOutcome, SenderError>> + Send;
    /// The key now has a self user; see [`Sender::mark_logged_in`].
    fn mark_logged_in(&self);
}

impl Link for Sender {
    fn invoke(&self, body: Bytes) -> impl Future<Output = Result<RpcOutcome, SenderError>> + Send {
        Sender::invoke(self, body)
    }

    fn mark_logged_in(&self) {
        Sender::mark_logged_in(self);
    }
}

/// Opens a [`Link`] to a DC: [`TcpDial`] in production.
pub trait Dial: Send + Sync + 'static {
    type Link: Link;
    /// A connection to `dc`. With `key: None` it makes a new auth key and
    /// returns it, for the caller to store; `logged_in` is
    /// [`InitParams::logged_in`].
    fn dial(
        &self,
        dc: i32,
        key: Option<AuthKey>,
        logged_in: bool,
    ) -> impl Future<Output = Result<(Self::Link, Pushes, Option<AuthKey>), SenderError>> + Send;
}

/// Telegram's production DCs over TCP.
#[derive(Clone, Debug)]
pub struct TcpDial {
    init: InitParams,
}

impl TcpDial {
    #[must_use]
    pub fn new(init: InitParams) -> Self {
        Self { init }
    }
}

impl Dial for TcpDial {
    type Link = Sender;

    async fn dial(
        &self,
        dc: i32,
        key: Option<AuthKey>,
        logged_in: bool,
    ) -> Result<(Sender, Pushes, Option<AuthKey>), SenderError> {
        let option = DcOption::production(dc).ok_or_else(|| {
            SenderError::Connect(crate::TransportError::Handshake(format!(
                "Telegram named DC {dc}, which tgin does not know"
            )))
        })?;
        let mut init = self.init.clone();
        init.logged_in = logged_in;
        Sender::connect(option, key, init).await
    }
}

/// The `tg://login?token=…` URL a QR shows. A secret: whoever has it can
/// log in as the account once it is scanned. `Debug` and `Display` hide it.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginUrl(String);

impl LoginUrl {
    fn new(token: &[u8]) -> Self {
        Self(format!(
            "tg://login?token={}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token)
        ))
    }

    /// The URL itself, for the QR and the admin API's answer only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LoginUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LoginUrl(..)")
    }
}

/// Where Telegram sent the login code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeKind {
    App,
    Sms,
    Call,
    FlashCall,
    MissedCall,
    FragmentSms,
    FirebaseSms,
    SmsWord,
    SmsPhrase,
}

impl CodeKind {
    /// The admin API's name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Sms => "sms",
            Self::Call => "call",
            Self::FlashCall => "flash_call",
            Self::MissedCall => "missed_call",
            Self::FragmentSms => "fragment_sms",
            Self::FirebaseSms => "firebase_sms",
            Self::SmsWord => "sms_word",
            Self::SmsPhrase => "sms_phrase",
        }
    }
}

/// Where a login stands. Nothing in a login panics: every way it ends is
/// one of these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginState {
    /// Connecting to Telegram, or waiting for its first answer.
    Connecting,
    /// Show `url` as a QR until `expires` (Telegram's unix time); a fresh
    /// one follows by itself.
    Qr {
        url: LoginUrl,
        expires: i32,
    },
    /// Waiting for [`LoginHandle::submit_code`].
    CodeSent {
        kind: CodeKind,
        timeout: Option<i32>,
    },
    /// The account has a cloud password: waiting for
    /// [`LoginHandle::submit_password`].
    NeedPassword {
        hint: Option<String>,
    },
    /// `auth.sentCodeTypeSetUpEmailRequired`: an official app must add a
    /// login email first.
    NeedEmailSetup,
    /// `auth.sentCodeTypeEmailCode`: the code went to the login email,
    /// which tgin does not support.
    NeedEmailCode,
    /// `auth.sentCodePaymentRequired`.
    PaymentRequired,
    /// Logged in, checked and recorded.
    LoggedIn {
        user_id: i64,
    },
    Failed(LoginError),
}

impl LoginState {
    /// The login has ended: nothing follows.
    #[must_use]
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            Self::NeedEmailSetup
                | Self::NeedEmailCode
                | Self::PaymentRequired
                | Self::LoggedIn { .. }
                | Self::Failed(_)
        )
    }

    /// The state's admin API name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Qr { .. } => "qr",
            Self::CodeSent { .. } => "code_sent",
            Self::NeedPassword { .. } => "need_password",
            Self::NeedEmailSetup => "need_email_setup",
            Self::NeedEmailCode => "need_email_code",
            Self::PaymentRequired => "payment_required",
            Self::LoggedIn { .. } => "logged_in",
            Self::Failed(_) => "failed",
        }
    }
}

/// Why a login step failed. No variant holds a secret.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
    /// An RPC error the flow does not handle.
    #[error("Telegram refused: {code} {name}")]
    Rpc { code: i32, name: String },
    #[error("Telegram asks to wait {seconds} s (FLOOD_WAIT)")]
    FloodWait { seconds: u32 },
    /// `PASSWORD_HASH_INVALID`; the password may be tried again.
    #[error("the 2FA password is wrong")]
    PasswordInvalid,
    /// `PHONE_CODE_INVALID`; the code may be tried again.
    #[error("the login code is wrong")]
    CodeInvalid,
    /// `PHONE_CODE_EXPIRED`.
    #[error("the login code expired")]
    CodeExpired,
    /// `auth.authorizationSignUpRequired`: the phone has no account.
    #[error("the phone number has no Telegram account")]
    SignUpRequired,
    /// A second `loginTokenMigrateTo` after an import.
    #[error("Telegram moved the login to DC {dc} again after the import")]
    MigrateLoop { dc: i32 },
    /// `account.password` without what SRP needs, or with unsafe values.
    #[error("unusable 2FA parameters from Telegram: {0}")]
    PasswordParams(&'static str),
    /// [`MAX_ATTEMPTS`] wrong 2FA passwords: the login ended.
    #[error("too many wrong 2FA passwords; start the login again")]
    PasswordAttempts,
    /// [`MAX_ATTEMPTS`] wrong login codes: the login ended.
    #[error("too many wrong login codes; start the login again")]
    CodeAttempts,
    /// The QR was not scanned within [`QR_LIFETIME`], or no code or
    /// password came within [`INPUT_WAIT`].
    #[error("the login expired; start it again")]
    Expired,
    /// The operating system's random source failed: no SRP secret.
    #[error("the operating system's random source failed")]
    NoRandomness,
    /// The identity gate: the logged-in account's phone is not the
    /// config's. The new authorization was logged out again.
    #[error("the logged-in account is not the configured one; it was logged out again")]
    WrongAccount,
    /// The identity gate found another account, and Telegram did not
    /// confirm `auth.logOut` of it. The key stays in the session (home DC,
    /// no login), so the logout can be retried.
    #[error(
        "the logged-in account is not the configured one, and Telegram did not confirm its logout: run `tgin admin logout <account> --yes`, or end the session in that account's Settings > Devices"
    )]
    WrongAccountNotLoggedOut,
    /// The code flow needs the account's `phone` in the config.
    #[error("the account has no `phone` in the config; the code login needs one")]
    NoPhone,
    /// A code or password came while the login does not wait for one.
    #[error("the login does not wait for that now")]
    NotExpected,
    /// Telegram's answer did not decode as the expected type.
    #[error("Telegram's answer did not decode")]
    Decode,
    /// The request had no answer (see [`SenderError`]).
    #[error("{0}")]
    Sender(String),
    #[error("session store: {0}")]
    Storage(String),
}

impl LoginError {
    /// A short stable name, for the admin API.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Rpc { .. } => "rpc_error",
            Self::FloodWait { .. } => "flood_wait",
            Self::PasswordInvalid => "password_invalid",
            Self::CodeInvalid => "code_invalid",
            Self::CodeExpired => "code_expired",
            Self::SignUpRequired => "sign_up_required",
            Self::MigrateLoop { .. } => "migrate_loop",
            Self::PasswordParams(_) => "password_params",
            Self::Expired => "expired",
            Self::PasswordAttempts => "password_attempts",
            Self::CodeAttempts => "code_attempts",
            Self::NoRandomness => "no_randomness",
            Self::WrongAccount => "wrong_account",
            Self::WrongAccountNotLoggedOut => "wrong_account_not_logged_out",
            Self::NoPhone => "no_phone",
            Self::NotExpected => "not_expected",
            Self::Decode => "decode",
            Self::Sender(_) => "no_answer",
            Self::Storage(_) => "storage",
        }
    }

    /// The step may be tried again: the login goes on.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::PasswordInvalid | Self::CodeInvalid | Self::NotExpected
        )
    }

    fn rpc(code: i32, message: &str) -> Self {
        if message.starts_with("FLOOD_WAIT_") {
            let seconds = Wait::parse(message)
                .filter(|w| w.kind == WaitKind::Flood)
                .and_then(|w| u32::try_from(w.seconds).ok());
            return Self::FloodWait {
                seconds: seconds.unwrap_or(0),
            };
        }
        match message {
            "PASSWORD_HASH_INVALID" => Self::PasswordInvalid,
            "PHONE_CODE_INVALID" => Self::CodeInvalid,
            "PHONE_CODE_EXPIRED" => Self::CodeExpired,
            _ => Self::Rpc {
                code,
                name: message.to_owned(),
            },
        }
    }
}

impl From<SessionError> for LoginError {
    fn from(e: SessionError) -> Self {
        Self::Storage(e.to_string())
    }
}

/// How a call ended, before the flow decides what it means.
enum Fail {
    Rpc {
        code: i32,
        message: String,
    },
    /// Written, no answer: it may have been executed.
    NoAnswer,
    Sender(SenderError),
    Decode,
}

impl From<Fail> for LoginError {
    fn from(f: Fail) -> Self {
        match f {
            Fail::Rpc { code, message } => Self::rpc(code, &message),
            Fail::NoAnswer => Self::Sender(SenderError::NoAnswer.to_string()),
            Fail::Sender(e) => Self::Sender(e.to_string()),
            Fail::Decode => Self::Decode,
        }
    }
}

/// Sends `request` and decodes its answer.
async fn call<L: Link, R: RemoteCall>(link: &L, request: &R) -> Result<R::Return, Fail> {
    match link.invoke(Bytes::from(request.to_bytes())).await {
        Ok(RpcOutcome::Result(bytes)) => R::Return::from_bytes(&bytes).map_err(|_| Fail::Decode),
        Ok(RpcOutcome::Error { code, message }) => Err(Fail::Rpc { code, message }),
        Err(SenderError::NoAnswer) => Err(Fail::NoAnswer),
        Err(e) => Err(Fail::Sender(e)),
    }
}

fn is_password_needed(f: &Fail) -> bool {
    matches!(f, Fail::Rpc { message, .. } if message == "SESSION_PASSWORD_NEEDED")
}

/// The logged-in user, as the authorization names it. `Debug` hides the
/// phone.
#[derive(Clone)]
struct Me {
    id: i64,
    access_hash: Option<i64>,
    bot: bool,
    phone: Option<String>,
}

impl fmt::Debug for Me {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Me")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Me {
    fn from_user(user: tl::enums::User) -> Option<Self> {
        match user {
            tl::enums::User::User(u) => Some(Self {
                id: u.id,
                access_hash: u.access_hash,
                bot: u.bot,
                phone: u.phone,
            }),
            tl::enums::User::Empty(_) => None,
        }
    }

    fn peer(&self) -> Peer {
        Peer {
            id: self.id,
            kind: PeerKind::User {
                bot: Some(self.bot),
                is_self: Some(true),
            },
            access_hash: self.access_hash,
        }
    }
}

/// One step's result. `Debug` leaves out the QR token and the
/// phone_code_hash.
enum Step {
    Token {
        token: Vec<u8>,
        expires: i32,
    },
    Migrate {
        dc: i32,
        token: Vec<u8>,
    },
    NeedPassword {
        hint: Option<String>,
    },
    CodeSent {
        kind: CodeKind,
        timeout: Option<i32>,
        hash: String,
    },
    /// `303 *_MIGRATE_X`: the request belongs on DC `dc`; it was not
    /// executed here.
    Moved {
        dc: i32,
    },
    /// A final state that is not a login (email, payment).
    Stop(LoginState),
    Authorized(Me),
}

impl fmt::Debug for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Token { expires, .. } => f
                .debug_struct("Token")
                .field("expires", expires)
                .finish_non_exhaustive(),
            Self::Migrate { dc, .. } => f
                .debug_struct("Migrate")
                .field("dc", dc)
                .finish_non_exhaustive(),
            Self::NeedPassword { hint } => {
                f.debug_struct("NeedPassword").field("hint", hint).finish()
            }
            Self::CodeSent { kind, timeout, .. } => f
                .debug_struct("CodeSent")
                .field("kind", kind)
                .field("timeout", timeout)
                .finish_non_exhaustive(),
            Self::Moved { dc } => f.debug_struct("Moved").field("dc", dc).finish(),
            Self::Stop(state) => f.debug_tuple("Stop").field(state).finish(),
            Self::Authorized(me) => f.debug_tuple("Authorized").field(me).finish(),
        }
    }
}

fn authorized(auth: tl::enums::auth::Authorization) -> Result<Step, LoginError> {
    match auth {
        tl::enums::auth::Authorization::Authorization(a) => Me::from_user(a.user)
            .map(Step::Authorized)
            .ok_or(LoginError::Decode),
        tl::enums::auth::Authorization::SignUpRequired(_) => Err(LoginError::SignUpRequired),
    }
}

/// `account.getPassword`, for its hint.
async fn need_password<L: Link>(link: &L) -> Result<Step, LoginError> {
    let tl::enums::account::Password::Password(p) =
        call(link, &tl::functions::account::GetPassword {}).await?;
    Ok(Step::NeedPassword { hint: p.hint })
}

/// Who the link's key is logged in as: `users.getUsers([inputUserSelf])`.
/// `Ok(None)` when Telegram says the key holds no login
/// ([`authorization::no_login`], [`authorization::sender_gone`]); a request
/// that was never written is sent once more.
async fn whoami<L: Link>(link: &L) -> Result<Option<Me>, Fail> {
    let request = tl::functions::users::GetUsers {
        id: vec![tl::enums::InputUser::UserSelf],
    };
    let mut answer = call(link, &request).await;
    if matches!(answer, Err(Fail::Sender(SenderError::NotSent))) {
        answer = call(link, &request).await;
    }
    match answer {
        Ok(users) => Ok(users.into_iter().next().and_then(Me::from_user)),
        Err(f) if holds_no_login(&f) => Ok(None),
        Err(f) => Err(f),
    }
}

/// Telegram says the key holds no login: its authorization is gone, or a
/// login on it never got past the 2FA step.
fn holds_no_login(f: &Fail) -> bool {
    match f {
        Fail::Rpc { code, message } => authorization::no_login(*code, message),
        Fail::Sender(e) => authorization::sender_gone(e),
        Fail::NoAnswer | Fail::Decode => false,
    }
}

/// After a login-producing call was written without an answer: did it log
/// in? [`whoami`] says, so a lost answer neither
/// double-logs in nor resets a live key.
async fn probe<L: Link>(link: &L) -> Result<Step, LoginError> {
    match whoami(link).await? {
        Some(me) => Ok(Step::Authorized(me)),
        // Not logged in: the call had not been executed, or failed.
        None => Err(LoginError::Sender(SenderError::NoAnswer.to_string())),
    }
}

/// A login-producing call's failure: a probe after no answer, the 2FA
/// step on `SESSION_PASSWORD_NEEDED`.
async fn after_auth_failure<L: Link>(link: &L, f: Fail) -> Result<Step, LoginError> {
    match f {
        Fail::NoAnswer => probe(link).await,
        f if is_password_needed(&f) => need_password(link).await,
        f => Err(f.into()),
    }
}

fn login_token(token: tl::enums::auth::LoginToken, imported: bool) -> Result<Step, LoginError> {
    use tl::enums::auth::LoginToken;
    match token {
        LoginToken::Token(t) => Ok(Step::Token {
            token: t.token,
            expires: t.expires,
        }),
        LoginToken::MigrateTo(m) if imported => Err(LoginError::MigrateLoop { dc: m.dc_id }),
        LoginToken::MigrateTo(m) => Ok(Step::Migrate {
            dc: m.dc_id,
            token: m.token,
        }),
        LoginToken::Success(s) => authorized(s.authorization),
    }
}

/// `auth.exportLoginToken`: a fresh token, or the scan's result.
async fn export<L: Link>(link: &L, api_id: i32, api_hash: &str) -> Result<Step, LoginError> {
    let request = tl::functions::auth::ExportLoginToken {
        api_id,
        api_hash: api_hash.to_owned(),
        except_ids: Vec::new(),
    };
    match call(link, &request).await {
        Ok(token) => login_token(token, false),
        // After a scan this export is what fetches the login: probe.
        Err(f) => after_auth_failure(link, f).await,
    }
}

/// `auth.importLoginToken` on the DC the token migrated to.
async fn import<L: Link>(link: &L, token: Vec<u8>) -> Result<Step, LoginError> {
    match call(link, &tl::functions::auth::ImportLoginToken { token }).await {
        Ok(token) => login_token(token, true),
        Err(f) => after_auth_failure(link, f).await,
    }
}

/// `auth.sendCode`.
async fn send_code<L: Link>(
    link: &L,
    phone: &str,
    api_id: i32,
    api_hash: &str,
) -> Result<Step, LoginError> {
    let request = tl::functions::auth::SendCode {
        phone_number: phone.to_owned(),
        api_id,
        api_hash: api_hash.to_owned(),
        settings: tl::enums::CodeSettings::Settings(tl::types::CodeSettings {
            allow_flashcall: false,
            current_number: false,
            allow_app_hash: false,
            allow_missed_call: false,
            allow_firebase: false,
            unknown_number: false,
            logout_tokens: None,
            token: None,
            app_sandbox: None,
        }),
    };
    use tl::enums::auth::SentCode;
    match call(link, &request).await {
        Ok(SentCode::Code(code)) => Ok(sent_code(code.r#type, code.timeout, code.phone_code_hash)),
        Ok(SentCode::Success(s)) => authorized(s.authorization),
        Ok(SentCode::PaymentRequired(_)) => Ok(Step::Stop(LoginState::PaymentRequired)),
        Err(f) => match moved_to(&f) {
            Some(dc) => Ok(Step::Moved { dc }),
            None => Err(f.into()),
        },
    }
}

/// The DC a `303 PHONE_MIGRATE_X`, `NETWORK_MIGRATE_X` or `USER_MIGRATE_X`
/// names: the phone's account lives there, and the request belongs there.
fn moved_to(f: &Fail) -> Option<i32> {
    let Fail::Rpc { code: 303, message } = f else {
        return None;
    };
    ["PHONE_MIGRATE_", "NETWORK_MIGRATE_", "USER_MIGRATE_"]
        .iter()
        .find_map(|prefix| message.strip_prefix(prefix))
        .and_then(|dc| dc.parse().ok())
}

fn sent_code(kind: tl::enums::auth::SentCodeType, timeout: Option<i32>, hash: String) -> Step {
    use tl::enums::auth::SentCodeType as T;
    let kind = match kind {
        T::App(_) => CodeKind::App,
        T::Sms(_) => CodeKind::Sms,
        T::Call(_) => CodeKind::Call,
        T::FlashCall(_) => CodeKind::FlashCall,
        T::MissedCall(_) => CodeKind::MissedCall,
        T::FragmentSms(_) => CodeKind::FragmentSms,
        T::FirebaseSms(_) => CodeKind::FirebaseSms,
        T::SmsWord(_) => CodeKind::SmsWord,
        T::SmsPhrase(_) => CodeKind::SmsPhrase,
        T::EmailCode(_) => return Step::Stop(LoginState::NeedEmailCode),
        T::SetUpEmailRequired(_) => return Step::Stop(LoginState::NeedEmailSetup),
    };
    Step::CodeSent {
        kind,
        timeout,
        hash,
    }
}

/// `auth.signIn` with the code.
async fn sign_in<L: Link>(
    link: &L,
    phone: &str,
    hash: &str,
    code: &str,
) -> Result<Step, LoginError> {
    let request = tl::functions::auth::SignIn {
        phone_number: phone.to_owned(),
        phone_code_hash: hash.to_owned(),
        phone_code: Some(code.to_owned()),
        email_verification: None,
    };
    match call(link, &request).await {
        Ok(auth) => authorized(auth),
        Err(f) => match moved_to(&f) {
            Some(dc) => Ok(Step::Moved { dc }),
            None => after_auth_failure(link, f).await,
        },
    }
}

/// SRP's inputs, checked: nothing grammers is given can panic it.
struct Srp {
    salt1: Vec<u8>,
    salt2: Vec<u8>,
    p: Vec<u8>,
    g: i32,
    srp_b: Vec<u8>,
    secure_random: Vec<u8>,
    srp_id: i64,
}

fn srp_inputs(info: tl::types::account::Password) -> Result<Srp, LoginError> {
    if !info.has_password {
        return Err(LoginError::PasswordParams("has_password is false"));
    }
    let algo = match info.current_algo {
        Some(tl::enums::PasswordKdfAlgo::Sha256Sha256Pbkdf2Hmacsha512iter100000Sha256ModPow(a)) => {
            a
        }
        Some(tl::enums::PasswordKdfAlgo::Unknown) => {
            return Err(LoginError::PasswordParams("unknown KDF"));
        }
        None => return Err(LoginError::PasswordParams("no current_algo")),
    };
    let srp_b = info.srp_b.ok_or(LoginError::PasswordParams("no srp_B"))?;
    let srp_id = info.srp_id.ok_or(LoginError::PasswordParams("no srp_id"))?;
    // grammers pads both to 256 bytes and panics on more.
    if srp_b.len() > SRP_MAX {
        return Err(LoginError::PasswordParams("srp_B longer than 256 bytes"));
    }
    if info.secure_random.len() > SRP_MAX {
        return Err(LoginError::PasswordParams(
            "secure_random longer than 256 bytes",
        ));
    }
    // grammers' check_p_and_g panics on g outside 2..=7.
    if !(2..=7).contains(&algo.g) {
        return Err(LoginError::PasswordParams("g outside 2..=7"));
    }
    // A g_b of 0, 1 or p − 1 (or beyond p) pins the shared secret.
    if !in_group(&srp_b, &algo.p) {
        return Err(LoginError::PasswordParams("srp_B outside 1 < g_b < p - 1"));
    }
    Ok(Srp {
        salt1: algo.salt1,
        salt2: algo.salt2,
        p: algo.p,
        g: algo.g,
        srp_b,
        secure_random: info.secure_random,
        srp_id,
    })
}

/// Whether `1 < g_b < p − 1`, both big-endian.
fn in_group(g_b: &[u8], p: &[u8]) -> bool {
    fn strip(n: &[u8]) -> &[u8] {
        let zeros = n.iter().take_while(|&&b| b == 0).count();
        &n[zeros..]
    }
    // `p − 1`; p is odd once it passes the safe-prime check, and a p that
    // is not fails that check anyway.
    let mut p_minus_1 = strip(p).to_vec();
    for byte in p_minus_1.iter_mut().rev() {
        let (v, borrow) = byte.overflowing_sub(1);
        *byte = v;
        if !borrow {
            break;
        }
    }
    let less = |a: &[u8], b: &[u8]| {
        let (a, b) = (strip(a), strip(b));
        a.len() < b.len() || (a.len() == b.len() && a < b)
    };
    less(&[1], g_b) && less(g_b, &p_minus_1)
}

/// SRP's secret exponent `a`: 256 bytes from the OS CSPRNG, with
/// Telegram's `secure_random` XORed in as extra entropy. Never Telegram's
/// value alone: whoever knew `a` could test password guesses offline
/// against a decrypted login.
fn srp_secret(secure_random: &[u8]) -> Result<Vec<u8>, LoginError> {
    use rand::TryRng as _;
    let mut a = vec![0u8; SRP_MAX];
    rand::rngs::SysRng
        .try_fill_bytes(&mut a)
        .map_err(|_| LoginError::NoRandomness)?;
    for (byte, extra) in a.iter_mut().zip(secure_random) {
        *byte ^= extra;
    }
    Ok(a)
}

/// SRP over a fresh `account.getPassword`, then `auth.checkPassword`. The
/// password is used for the proof only.
async fn check_password<L: Link>(link: &L, password: String) -> Result<Step, LoginError> {
    let tl::enums::account::Password::Password(info) =
        call(link, &tl::functions::account::GetPassword {}).await?;
    let srp = srp_inputs(info)?;
    let a = srp_secret(&srp.secure_random)?;
    let srp_id = srp.srp_id;
    // The safe-prime check and PBKDF2 (100 000 rounds) take a while.
    let proof = tokio::task::spawn_blocking(move || {
        if !check_p_and_g(&srp.p, &srp.g) {
            return None;
        }
        Some(calculate_2fa(
            &srp.salt1, &srp.salt2, &srp.p, &srp.g, srp.srp_b, a, password,
        ))
    })
    .await
    .map_err(|_| LoginError::PasswordParams("the SRP computation failed"))?;
    let (m1, g_a) = proof.ok_or(LoginError::PasswordParams("p/g failed the safety check"))?;
    let request = tl::functions::auth::CheckPassword {
        password: tl::enums::InputCheckPasswordSrp::Srp(tl::types::InputCheckPasswordSrp {
            srp_id,
            a: g_a.to_vec(),
            m1: m1.to_vec(),
        }),
    };
    match call(link, &request).await {
        Ok(auth) => authorized(auth),
        Err(Fail::NoAnswer) => probe(link).await,
        Err(f) => Err(f.into()),
    }
}

/// `auth.logOut` on `link`'s key: `Ok` once Telegram confirmed that the
/// key is logged out, by answering it, or by saying the key holds no login
/// (a lost authorization, a duplicated key, transport −404).
async fn log_out_key<L: Link>(link: &L) -> Result<(), Fail> {
    match call(link, &tl::functions::auth::LogOut {}).await {
        Ok(_) => Ok(()),
        Err(f) if holds_no_login(&f) => Ok(()),
        Err(f) => Err(f),
    }
}

/// Whether the configured `want` and Telegram's `got` are one number:
/// digits only, so `+1 555 0100` matches `15550100`.
fn same_phone(want: &str, got: Option<&str>) -> bool {
    let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
    let want = digits(want);
    !want.is_empty() && got.is_some_and(|got| digits(got) == want)
}

/// What a login needs besides the session.
#[derive(Clone)]
pub struct LoginParams {
    pub api_id: i32,
    pub api_hash: String,
    /// The config's phone: the identity gate's reference, and the code
    /// flow's number.
    pub phone: Option<String>,
}

impl fmt::Debug for LoginParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginParams")
            .field("has_phone", &self.phone.is_some())
            .finish_non_exhaustive()
    }
}

/// Which flow a login runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Qr,
    Code,
}

/// Why a login could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// A login is recorded: log out first.
    #[error("the account is logged in; log it out first")]
    LoggedIn,
    /// A key from an unfinished login is stored, and Telegram did not say
    /// whether it is logged in. Nothing was changed.
    #[error("a key from an unfinished login could not be checked with Telegram: {0}")]
    Unverified(String),
    #[error(transparent)]
    Storage(#[from] SessionError),
}

enum Input {
    Code(String),
    Password(String),
}

struct Command {
    input: Input,
    reply: oneshot::Sender<Result<LoginState, LoginError>>,
}

/// Leaves out the code or password.
impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.input {
            Input::Code(_) => "Command(code)",
            Input::Password(_) => "Command(password)",
        })
    }
}

/// A login in progress. Dropping it ends the login and closes its
/// connection; the session is released once the login's task ends.
#[derive(Debug)]
pub struct LoginHandle {
    state: watch::Receiver<LoginState>,
    commands: mpsc::Sender<Command>,
    task: Option<JoinHandle<()>>,
}

impl Drop for LoginHandle {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl LoginHandle {
    /// The current state.
    #[must_use]
    pub fn state(&self) -> LoginState {
        self.state.borrow().clone()
    }

    /// The login's states as they change, without holding the login: the
    /// receiver sees the final state, published once the session is
    /// released, or the sender gone when the login was aborted.
    #[must_use]
    pub fn watch(&self) -> watch::Receiver<LoginState> {
        self.state.clone()
    }

    /// The state once it is no longer [`LoginState::Connecting`].
    pub async fn settled(&self) -> LoginState {
        let mut state = self.state.clone();
        let settled = state
            .wait_for(|s| !matches!(s, LoginState::Connecting))
            .await
            .map(|state| state.clone());
        settled.unwrap_or_else(|_| self.state())
    }

    /// The final state, once the login has ended.
    pub async fn finished(&self) -> LoginState {
        let mut state = self.state.clone();
        let end = state
            .wait_for(LoginState::is_final)
            .await
            .map(|state| state.clone());
        end.unwrap_or_else(|_| self.state())
    }

    /// The next state after the current one.
    pub async fn changed(&self) -> LoginState {
        let mut state = self.state.clone();
        state.mark_unchanged();
        let _ = state.changed().await;
        self.state()
    }

    /// Ends the login without waiting for it.
    pub fn abort(&self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }

    /// Ends the login and waits until it has released the session.
    pub async fn close(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }

    /// Hands the login code to a login in [`LoginState::CodeSent`]. Returns
    /// the state that follows; a [retryable](LoginError::is_retryable)
    /// error leaves the state as it was.
    pub async fn submit_code(&self, code: String) -> Result<LoginState, LoginError> {
        self.submit(Input::Code(code)).await
    }

    /// Hands the 2FA password to a login in [`LoginState::NeedPassword`].
    pub async fn submit_password(&self, password: String) -> Result<LoginState, LoginError> {
        self.submit(Input::Password(password)).await
    }

    async fn submit(&self, input: Input) -> Result<LoginState, LoginError> {
        let (reply, answer) = oneshot::channel();
        if self.commands.send(Command { input, reply }).await.is_err() {
            return Err(LoginError::NotExpected);
        }
        answer.await.unwrap_or(Err(LoginError::NotExpected))
    }
}

/// Starts a login on `store`, which it owns until the login ends. Must run
/// inside a Tokio runtime.
///
/// A recorded login is refused ([`StartError::LoggedIn`]). A home key left
/// without one (a login whose answer was lost, say) may still be logged
/// in, so it is asked first ([`whoami`]), within [`LEFTOVER_DEADLINE`]:
/// - logged in, with a `phone` configured: it goes through the identity
///   gate and, when it matches, is recorded;
/// - logged in, with no `phone` to check it against: it is logged out
///   (`auth.logOut`), since nothing says it is the right account, and the
///   login starts fresh. So a crash between a scan and the login marker
///   costs one more scan when no `phone` is configured;
/// - unknown to Telegram: the session is reset and the login starts on
///   [`FIRST_DC`] with a new key;
/// - no clear answer in time, or an unconfirmed logout: refused
///   ([`StartError::Unverified`]), the session unchanged.
pub async fn start<D: Dial>(
    store: SessionStore,
    dial: D,
    params: LoginParams,
    method: Method,
) -> Result<LoginHandle, StartError> {
    if store.login()?.is_some() {
        return Err(StartError::LoggedIn);
    }
    let begin = match leftover(&store)? {
        None => Begin::Fresh(method),
        Some((dc, key)) => {
            let asked = tokio::time::timeout(
                LEFTOVER_DEADLINE,
                ask_leftover(&dial, dc, key, &params, method),
            )
            .await;
            match asked {
                Ok(begin) => begin?,
                Err(_) => {
                    return Err(StartError::Unverified(format!(
                        "Telegram did not answer within {} s",
                        LEFTOVER_DEADLINE.as_secs()
                    )));
                }
            }
        }
    };
    if matches!(begin, Begin::Fresh(_)) {
        store.reset_for_login()?;
    }
    let (state_tx, state) = watch::channel(LoginState::Connecting);
    let (commands_tx, commands) = mpsc::channel(4);
    let flow = Flow {
        store,
        dial,
        params,
        state: state_tx,
        commands,
        wrong_passwords: 0,
        wrong_codes: 0,
        moved: false,
    };
    let task = tokio::spawn(flow.run(begin));
    Ok(LoginHandle {
        state,
        commands: commands_tx,
        task: Some(task),
    })
}

/// What a leftover home key means for the login (see [`start`]).
async fn ask_leftover<D: Dial>(
    dial: &D,
    dc: i32,
    key: AuthKey,
    params: &LoginParams,
    method: Method,
) -> Result<Begin<D::Link>, StartError> {
    let unverified = |e: String| StartError::Unverified(e);
    let (link, pushes) = match dial.dial(dc, Some(key), false).await {
        Ok((link, pushes, _)) => (link, pushes),
        Err(e) if authorization::sender_gone(&e) => return Ok(Begin::Fresh(method)),
        Err(e) => return Err(unverified(e.to_string())),
    };
    let conn = Conn::new(link, pushes);
    match whoami(&conn.link).await {
        Ok(None) => Ok(Begin::Fresh(method)),
        Ok(Some(me)) if params.phone.is_some() => Ok(Begin::Recovered(conn, me)),
        // Logged in, and nothing to check the account against: never
        // adopt it.
        Ok(Some(_)) => match log_out_key(&conn.link).await {
            Ok(()) => {
                tracing::info!("login: a key left logged in by an unfinished login was logged out");
                Ok(Begin::Fresh(method))
            }
            Err(f) => Err(unverified(LoginError::from(f).to_string())),
        },
        Err(f) => Err(unverified(LoginError::from(f).to_string())),
    }
}

/// The home DC and its key, when a key is stored.
fn leftover(store: &SessionStore) -> Result<Option<(i32, AuthKey)>, SessionError> {
    let Some(dc) = store.home_dc()? else {
        return Ok(None);
    };
    Ok(store.auth_key(dc)?.map(|key| (dc, key)))
}

/// How a login's task begins.
enum Begin<L> {
    Fresh(Method),
    /// A leftover key turned out to be logged in as `Me`.
    Recovered(Conn<L>, Me),
}

/// Why a logout did not complete.
#[derive(Debug, thiserror::Error)]
pub enum LogoutError {
    /// Telegram did not confirm `auth.logOut`; nothing was deleted.
    #[error("Telegram did not confirm the logout: {0}")]
    Unconfirmed(String),
    #[error(transparent)]
    Storage(#[from] SessionError),
}

/// `auth.logOut` on the session's home key, then
/// [`SessionStore::reset_for_login`]. A key Telegram no longer knows (−404,
/// 401) counts as logged out. Any other failure deletes nothing.
pub async fn log_out<D: Dial>(store: &SessionStore, dial: &D) -> Result<(), LogoutError> {
    let home = store.home_dc()?;
    if let Some(dc) = home
        && let Some(key) = store.auth_key(dc)?
    {
        let logged_in = store.login()?.is_some();
        match dial.dial(dc, Some(key), logged_in).await {
            Ok((link, pushes, _)) => {
                drop(pushes);
                if let Err(f) = log_out_key(&link).await {
                    return Err(LogoutError::Unconfirmed(LoginError::from(f).to_string()));
                }
            }
            Err(e) if authorization::sender_gone(&e) => {}
            Err(e) => return Err(LogoutError::Unconfirmed(e.to_string())),
        }
    }
    store.reset_for_login()?;
    Ok(())
}

/// Wakes the flow when the QR was scanned; drops every other push.
async fn drain(mut pushes: Pushes, scanned: Arc<Notify>) {
    while let Some(push) = pushes.recv().await {
        if is_login_token(&push) {
            scanned.notify_one();
        }
    }
}

/// `updateShort { update: updateLoginToken }`, or `updates` whose updates
/// hold one.
fn is_login_token(push: &[u8]) -> bool {
    let word = |at: usize| {
        push.get(at..at + 4)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_le_bytes)
    };
    let token = tl::types::UpdateLoginToken::CONSTRUCTOR_ID;
    match word(0) {
        Some(id) if id == tl::types::UpdateShort::CONSTRUCTOR_ID => word(4) == Some(token),
        Some(id) if id == tl::types::Updates::CONSTRUCTOR_ID => {
            matches!(tl::enums::Updates::from_bytes(push),
                Ok(tl::enums::Updates::Updates(u)) if u.updates.iter().any(|u| matches!(u, tl::enums::Update::LoginToken)))
        }
        _ => false,
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The login's task.
struct Flow<D: Dial> {
    store: SessionStore,
    dial: D,
    params: LoginParams,
    state: watch::Sender<LoginState>,
    commands: mpsc::Receiver<Command>,
    /// Wrong passwords and codes so far: each is spent on the real account.
    wrong_passwords: u32,
    wrong_codes: u32,
    /// The code login followed a `*_MIGRATE_X` to another DC.
    moved: bool,
}

/// One connection of the flow, its pushes drained.
struct Conn<L> {
    link: L,
    scanned: Arc<Notify>,
    drain: JoinHandle<()>,
}

impl<L> Conn<L> {
    /// `link`, its pushes drained on a task of their own.
    fn new(link: L, pushes: Pushes) -> Self {
        let scanned = Arc::new(Notify::new());
        let drain = tokio::spawn(drain(pushes, Arc::clone(&scanned)));
        Self {
            link,
            scanned,
            drain,
        }
    }
}

impl<L> Drop for Conn<L> {
    fn drop(&mut self) {
        self.drain.abort();
    }
}

/// What the flow waits for.
enum Wanted {
    Code,
    Password,
}

impl<D: Dial> Flow<D> {
    async fn run(self, begin: Begin<D::Link>) {
        let mut flow = self;
        let ended = match begin {
            Begin::Fresh(method) => flow.drive(method).await,
            Begin::Recovered(conn, me) => flow.conclude(&conn.link, &me).await,
        };
        let end = match ended {
            Ok(state) => state,
            Err(e) => LoginState::Failed(e),
        };
        tracing::info!(state = end.name(), "login ended");
        // The session is released before anyone can see the end: a caller
        // that logs out (or in) right after it must not find it held.
        let Flow {
            store,
            state,
            mut commands,
            ..
        } = flow;
        drop(store);
        // A late command gets no state.
        commands.close();
        state.send_replace(end);
    }

    /// A new key on `dc`, stored as home before it is used.
    async fn connect(&self, dc: i32) -> Result<Conn<D::Link>, LoginError> {
        let (link, pushes, key) = self
            .dial
            .dial(dc, None, false)
            .await
            .map_err(|e| LoginError::Sender(e.to_string()))?;
        let key = key.ok_or_else(|| LoginError::Sender("the connection made no key".to_owned()))?;
        self.store.set_home_key(dc, &key)?;
        Ok(Conn::new(link, pushes))
    }

    /// A request was moved to DC `dc` (`303 *_MIGRATE_X`): a new key there,
    /// stored as home. A login follows one move; a second is a loop.
    async fn follow(&mut self, dc: i32) -> Result<Conn<D::Link>, LoginError> {
        if self.moved {
            return Err(LoginError::MigrateLoop { dc });
        }
        self.moved = true;
        tracing::info!(dc, "login: the account lives on another DC");
        self.connect(dc).await
    }

    async fn drive(&mut self, method: Method) -> Result<LoginState, LoginError> {
        let qr_deadline = Instant::now() + QR_LIFETIME;
        let mut conn = self.connect(FIRST_DC).await?;
        let (api_id, api_hash) = (self.params.api_id, self.params.api_hash.clone());
        let mut step = match method {
            Method::Qr => export(&conn.link, api_id, &api_hash).await,
            Method::Code => {
                let phone = self.params.phone.clone().ok_or(LoginError::NoPhone)?;
                match send_code(&conn.link, &phone, api_id, &api_hash).await {
                    Ok(Step::Moved { dc }) => {
                        conn = self.follow(dc).await?;
                        send_code(&conn.link, &phone, api_id, &api_hash).await
                    }
                    step => step,
                }
            }
        };
        let mut imported = false;
        loop {
            match step? {
                Step::Token { token, expires } => {
                    self.publish(LoginState::Qr {
                        url: LoginUrl::new(&token),
                        expires,
                    });
                    let ttl = (i64::from(expires) - now_unix()).clamp(1, MAX_TOKEN_TTL);
                    let renew = Instant::now() + Duration::from_secs(ttl.unsigned_abs());
                    let scanned = self.wait_scan(&conn.scanned, renew.min(qr_deadline)).await;
                    if !scanned && Instant::now() >= qr_deadline {
                        return Err(LoginError::Expired);
                    }
                    step = export(&conn.link, api_id, &api_hash).await;
                }
                Step::Migrate { dc, token } => {
                    if imported {
                        return Err(LoginError::MigrateLoop { dc });
                    }
                    imported = true;
                    tracing::info!(dc, "login: the account lives on another DC");
                    conn = self.connect(dc).await?;
                    step = import(&conn.link, token).await;
                }
                Step::NeedPassword { hint } => {
                    self.publish(LoginState::NeedPassword { hint });
                    step = self.answer(&mut conn, &Wanted::Password, None).await;
                }
                Step::CodeSent {
                    kind,
                    timeout,
                    hash,
                } => {
                    self.publish(LoginState::CodeSent { kind, timeout });
                    step = self.answer(&mut conn, &Wanted::Code, Some(&hash)).await;
                }
                // A request moved once already was moved again.
                Step::Moved { dc } => return Err(LoginError::MigrateLoop { dc }),
                Step::Stop(state) => return Ok(state),
                Step::Authorized(me) => return self.conclude(&conn.link, &me).await,
            }
        }
    }

    fn publish(&self, state: LoginState) {
        tracing::info!(state = state.name(), "login");
        self.state.send_replace(state);
    }

    /// Until the QR is scanned or `ttl` passes. A code or password meanwhile
    /// is refused.
    /// Whether it was scanned.
    async fn wait_scan(&mut self, scanned: &Notify, until: Instant) -> bool {
        let deadline = tokio::time::sleep_until(until);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => return false,
                () = scanned.notified() => return true,
                Some(command) = self.commands.recv() => {
                    let _ = command.reply.send(Err(LoginError::NotExpected));
                }
            }
        }
    }

    /// Waits for the code or password the state asks for and tries it; a
    /// retryable failure is answered and the wait goes on.
    async fn answer(
        &mut self,
        conn: &mut Conn<D::Link>,
        wanted: &Wanted,
        hash: Option<&str>,
    ) -> Result<Step, LoginError> {
        let deadline = Instant::now() + INPUT_WAIT;
        loop {
            let Ok(command) = tokio::time::timeout_at(deadline, self.commands.recv()).await else {
                return Err(LoginError::Expired);
            };
            let Some(command) = command else {
                // Every handle is gone: nobody can answer.
                return Err(LoginError::NotExpected);
            };
            let step = match (&command.input, wanted) {
                (Input::Password(pw), Wanted::Password) => {
                    check_password(&conn.link, pw.clone()).await
                }
                (Input::Code(code), Wanted::Code) => {
                    let phone = self.params.phone.clone().unwrap_or_default();
                    let hash = hash.unwrap_or_default();
                    match sign_in(&conn.link, &phone, hash, code).await {
                        Ok(Step::Moved { dc }) => match self.follow(dc).await {
                            Ok(moved) => {
                                *conn = moved;
                                sign_in(&conn.link, &phone, hash, code).await
                            }
                            Err(e) => Err(e),
                        },
                        step => step,
                    }
                }
                _ => {
                    let _ = command.reply.send(Err(LoginError::NotExpected));
                    continue;
                }
            };
            let step = match step {
                Err(LoginError::PasswordInvalid) => {
                    self.wrong_passwords += 1;
                    tracing::warn!(attempt = self.wrong_passwords, "login: wrong 2FA password");
                    if self.wrong_passwords >= MAX_ATTEMPTS {
                        Err(LoginError::PasswordAttempts)
                    } else {
                        Err(LoginError::PasswordInvalid)
                    }
                }
                Err(LoginError::CodeInvalid) => {
                    self.wrong_codes += 1;
                    tracing::warn!(attempt = self.wrong_codes, "login: wrong login code");
                    if self.wrong_codes >= MAX_ATTEMPTS {
                        Err(LoginError::CodeAttempts)
                    } else {
                        Err(LoginError::CodeInvalid)
                    }
                }
                step => step,
            };
            match step {
                Err(e) if e.is_retryable() => {
                    tracing::info!(error = e.code(), "login: try again");
                    let _ = command.reply.send(Err(e));
                }
                step => {
                    // The reply carries the state that follows.
                    let mut watch = self.state.subscribe();
                    watch.mark_unchanged();
                    tokio::spawn(async move {
                        let next = match watch.changed().await {
                            Ok(()) => watch.borrow().clone(),
                            Err(_) => LoginState::Failed(LoginError::NotExpected),
                        };
                        let _ = command.reply.send(Ok(next));
                    });
                    return step;
                }
            }
        }
    }

    /// The identity gate, then the login marker in one transaction.
    async fn conclude(&self, link: &D::Link, me: &Me) -> Result<LoginState, LoginError> {
        if let Some(want) = &self.params.phone
            && !same_phone(want, me.phone.as_deref())
        {
            tracing::warn!(
                "login: the logged-in account is not the configured one; logging it out"
            );
            if log_out_key(link).await.is_err() {
                // The key is the only way to retry the logout: keep it.
                tracing::warn!(
                    "login: auth.logOut of the wrong account got no confirmation; the key is kept for `tgin admin logout`"
                );
                return Err(LoginError::WrongAccountNotLoggedOut);
            }
            self.store.reset_for_login()?;
            return Err(LoginError::WrongAccount);
        }
        let login = self.store.record_login(me.peer())?;
        link.mark_logged_in();
        Ok(LoginState::LoggedIn {
            user_id: login.self_user_id,
        })
    }
}
