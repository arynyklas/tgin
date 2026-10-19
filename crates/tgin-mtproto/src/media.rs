//! Media sessions (plan decision 2): a `*_MIGRATE_X` answer sends the call
//! to DC X over a second session, never over the main one.
//!
//! A media session is a [`Sender`] to DC X made `without_updates`. Its key
//! is the session store's media key for X (`media_keys`, apart from the
//! `auth_keys` a login writes); a DC without one gets a new key, authorized
//! with `auth.exportAuthorization` on the main session and
//! `auth.importAuthorization` on the new one, and stored only once the
//! import succeeded, so a stored media key is always an authorized one.
//! A key Telegram stops accepting (`AUTH_KEY_UNREGISTERED`,
//! `SESSION_REVOKED`, transport −404) is dropped, and the session is made
//! and authorized again once.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use grammers_tl_types::{self as tl, Deserializable as _, Serializable as _};
use tokio::sync::Mutex;
use tokio::time::{Instant, timeout};

use crate::rpc::{
    AccountRpc, BoxFuture, Charge, ErrorClass, InvokeError, MediaFailure, Route, charged, classify,
};
use crate::{
    AuthKey, DcOption, InitParams, RpcOutcome, Sender, SenderError, SessionError, SessionStore,
};

/// One MTProto session: anything that sends a request and gives its
/// outcome. [`Sender`] is the real one.
pub trait Session: Send + Sync {
    fn invoke(&self, body: Bytes) -> BoxFuture<'_, Result<RpcOutcome, SenderError>>;
}

impl Session for Sender {
    fn invoke(&self, body: Bytes) -> BoxFuture<'_, Result<RpcOutcome, SenderError>> {
        Box::pin(Sender::invoke(self, body))
    }
}

impl<S: Session + ?Sized> Session for Arc<S> {
    fn invoke(&self, body: Bytes) -> BoxFuture<'_, Result<RpcOutcome, SenderError>> {
        S::invoke(self, body)
    }
}

/// A new media session, and the key made for it (`None` when one was
/// given).
pub type MediaConnected<S> = (S, Option<AuthKey>);

/// Opens media sessions.
pub trait Connect: Send + Sync {
    type Session: Session + 'static;
    /// A `without_updates` session to `dc`, with `key`; with `None` it
    /// makes a key and returns it.
    fn connect(
        &self,
        dc: i32,
        key: Option<AuthKey>,
    ) -> BoxFuture<'_, Result<MediaConnected<Self::Session>, SenderError>>;
}

/// The real [`Connect`]: [`Sender::connect`] over TCP, with the account's
/// init parameters made `without_updates` and logged in.
#[derive(Debug)]
pub struct TcpConnect {
    init: InitParams,
    /// The store's DC options; ids missing here use the production table.
    options: Vec<DcOption>,
}

impl TcpConnect {
    #[must_use]
    pub fn new(mut init: InitParams, options: Vec<DcOption>) -> Self {
        init.without_updates = true;
        init.logged_in = true;
        Self { init, options }
    }
}

impl Connect for TcpConnect {
    type Session = Sender;

    fn connect(
        &self,
        dc: i32,
        key: Option<AuthKey>,
    ) -> BoxFuture<'_, Result<MediaConnected<Sender>, SenderError>> {
        Box::pin(async move {
            let option = self
                .options
                .iter()
                .find(|o| o.id == dc)
                .copied()
                .or_else(|| DcOption::production(dc))
                .ok_or(SenderError::NotSent)?;
            // A media session's pushes are discarded (its receiver yields
            // nothing anyway).
            let (sender, _pushes, new_key) =
                Sender::connect(option, key, self.init.clone()).await?;
            Ok((sender, new_key))
        })
    }
}

/// Where authorized media keys are kept, per DC. A key is saved only once
/// `auth.importAuthorization` succeeded on it, so a loaded key is
/// authorized.
pub trait KeyStore: Send + Sync {
    fn load(&self, dc: i32) -> Result<Option<AuthKey>, SessionError>;
    fn save(&self, dc: i32, key: &AuthKey) -> Result<(), SessionError>;
    fn remove(&self, dc: i32) -> Result<(), SessionError>;
}

/// The store's `media_keys` table, never `auth_keys`: that table may hold
/// keys that were never authorized (a login's first DC).
impl KeyStore for SessionStore {
    fn load(&self, dc: i32) -> Result<Option<AuthKey>, SessionError> {
        self.media_key(dc)
    }

    fn save(&self, dc: i32, key: &AuthKey) -> Result<(), SessionError> {
        self.set_media_key(dc, key)
    }

    fn remove(&self, dc: i32) -> Result<(), SessionError> {
        self.remove_media_key(dc)
    }
}

impl<K: KeyStore + ?Sized> KeyStore for Arc<K> {
    fn load(&self, dc: i32) -> Result<Option<AuthKey>, SessionError> {
        K::load(self, dc)
    }

    fn save(&self, dc: i32, key: &AuthKey) -> Result<(), SessionError> {
        K::save(self, dc, key)
    }

    fn remove(&self, dc: i32) -> Result<(), SessionError> {
        K::remove(self, dc)
    }
}

/// Telegram does not know the media session's key, or no longer accepts
/// its authorization: the call was not executed.
fn key_gone(outcome: &Result<RpcOutcome, SenderError>) -> bool {
    match outcome {
        Err(SenderError::KeyRevoked | SenderError::KeyUnknown) => true,
        Ok(RpcOutcome::Error { code: 401, message }) => {
            message == "AUTH_KEY_UNREGISTERED" || message == "SESSION_REVOKED"
        }
        _ => false,
    }
}

/// A failed media setup step.
fn failed(dc: i32, what: &'static str) -> InvokeError {
    InvokeError::Media {
        dc,
        failure: MediaFailure::Failed { what },
    }
}

/// A setup call's `rpc_error`: a flood wait keeps its seconds.
fn refused(dc: i32, what: &'static str, message: &str) -> InvokeError {
    match classify(420, message) {
        ErrorClass::FloodWait(seconds) => InvokeError::Media {
            dc,
            failure: MediaFailure::FloodWait { what, seconds },
        },
        _ => failed(dc, what),
    }
}

/// A setup call's [`SenderError`]: the client's request was not sent
/// whatever it is, so it is a media failure, never the request's outcome.
fn setup_error(dc: i32, what: &'static str, e: &SenderError) -> InvokeError {
    match e {
        SenderError::InitRefused { code: 420, message } => refused(dc, what, message),
        _ => failed(dc, what),
    }
}

/// The longest a media session's setup may take: connecting (10 s), the
/// key exchange (30 s), the export and the import.
pub const SETUP_DEADLINE: Duration = Duration::from_secs(60);
/// The first and the longest wait after a failed setup before a DC is
/// tried again; it doubles in between.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// One DC's media session. `ready` is read without waiting on a setup
/// in progress; `setup` serializes the setup of this DC only.
struct Dc<S> {
    ready: parking_lot::Mutex<Option<Arc<S>>>,
    setup: Mutex<Setup<S>>,
}

struct Setup<S> {
    /// Connected with a new key whose authorization has not succeeded yet:
    /// kept so a retry repeats only the export and import, never the key
    /// exchange.
    unauthorized: Option<(Arc<S>, AuthKey)>,
    /// After a failure: no setup before `until`; answered with `failure`.
    failed: Option<(Instant, MediaFailure)>,
    backoff: Duration,
}

impl<S> Default for Dc<S> {
    fn default() -> Self {
        Self {
            ready: parking_lot::Mutex::new(None),
            setup: Mutex::new(Setup {
                unauthorized: None,
                failed: None,
                backoff: BACKOFF_FIRST,
            }),
        }
    }
}

/// An account's sessions: the main one on the home DC and one media
/// session per DC, made on first use. The [`AccountRpc`] the endpoint
/// calls.
pub struct AccountSessions<M, C: Connect, K> {
    main: M,
    home_dc: i32,
    layer: i32,
    connect: C,
    keys: K,
    charge: Arc<dyn Charge>,
    dcs: parking_lot::Mutex<HashMap<i32, Arc<Dc<C::Session>>>>,
}

impl<M, C: Connect, K> AccountSessions<M, C, K> {
    /// `charge` is the account's request budget: a media setup's export and
    /// import, and the resend after a refused media key, are charged to it.
    pub fn new(
        main: M,
        home_dc: i32,
        layer: i32,
        connect: C,
        keys: K,
        charge: Arc<dyn Charge>,
    ) -> Self {
        Self {
            main,
            home_dc,
            layer,
            connect,
            keys,
            charge,
            dcs: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    fn dc(&self, dc: i32) -> Arc<Dc<C::Session>> {
        Arc::clone(self.dcs.lock().entry(dc).or_default())
    }
}

impl<M: Session, C: Connect, K: KeyStore> AccountSessions<M, C, K> {
    /// The media session to `dc`, made and authorized on first use. A ready
    /// session is returned at once; a setup waits only for another setup of
    /// the same DC, is bounded by [`SETUP_DEADLINE`], and after a failure
    /// the DC is not tried again for a backoff (1 s doubling to 60 s, or
    /// Telegram's flood wait).
    async fn media(&self, dc: i32) -> Result<Arc<C::Session>, InvokeError> {
        let slot = self.dc(dc);
        if let Some(session) = slot.ready.lock().clone() {
            return Ok(session);
        }
        let mut setup = slot.setup.lock().await;
        if let Some(session) = slot.ready.lock().clone() {
            return Ok(session);
        }
        if let Some((until, failure)) = setup.failed
            && Instant::now() < until
        {
            return Err(InvokeError::Media { dc, failure });
        }
        let made = match timeout(SETUP_DEADLINE, self.set_up(dc, &mut setup)).await {
            Ok(made) => made,
            Err(_) => Err(failed(dc, "the setup timed out")),
        };
        match made {
            Ok(session) => {
                setup.failed = None;
                setup.backoff = BACKOFF_FIRST;
                *slot.ready.lock() = Some(Arc::clone(&session));
                Ok(session)
            }
            Err(e) => {
                if let InvokeError::Media { failure, .. } = e {
                    let wait = match failure {
                        MediaFailure::FloodWait { seconds, .. } => {
                            Duration::from_secs(u64::from(seconds)).max(setup.backoff)
                        }
                        MediaFailure::Failed { .. } => setup.backoff,
                    };
                    setup.failed = Some((Instant::now() + wait, failure));
                    setup.backoff = (setup.backoff * 2).min(BACKOFF_MAX);
                }
                Err(e)
            }
        }
    }

    /// Connects (unless a connected, unauthorized session is kept) and
    /// authorizes a new key.
    async fn set_up(
        &self,
        dc: i32,
        setup: &mut Setup<C::Session>,
    ) -> Result<Arc<C::Session>, InvokeError> {
        let (session, key) = match setup.unauthorized.take() {
            Some(kept) => kept,
            None => {
                let stored = self
                    .keys
                    .load(dc)
                    .map_err(|_| failed(dc, "the session store failed"))?;
                let (session, new_key) = self
                    .connect
                    .connect(dc, stored)
                    .await
                    .map_err(|e| setup_error(dc, "connecting", &e))?;
                let session = Arc::new(session);
                let Some(key) = new_key else {
                    return Ok(session);
                };
                (session, key)
            }
        };
        setup.unauthorized = Some((Arc::clone(&session), key.clone()));
        // The export and the import.
        charged(self.charge.as_ref())?;
        charged(self.charge.as_ref())?;
        self.authorize(dc, session.as_ref()).await?;
        self.keys
            .save(dc, &key)
            .map_err(|_| failed(dc, "the session store failed"))?;
        setup.unauthorized = None;
        Ok(session)
    }

    /// Drops the media session to `dc` and its stored key.
    async fn evict(&self, dc: i32) -> Result<(), InvokeError> {
        let slot = self.dc(dc);
        *slot.ready.lock() = None;
        slot.setup.lock().await.unauthorized = None;
        self.keys
            .remove(dc)
            .map_err(|_| failed(dc, "the session store failed"))
    }

    /// A call on the media session to `dc`. A key Telegram no longer
    /// accepts is dropped and the session made and authorized again, once;
    /// the call was not executed, so it is sent once more. A key still
    /// refused after that is a media-session failure, never a sign that the
    /// account needs a login: the main session is not involved.
    async fn invoke_media(&self, dc: i32, body: Bytes) -> Result<RpcOutcome, InvokeError> {
        let media = self.media(dc).await?;
        let outcome = media.invoke(body.clone()).await;
        if !key_gone(&outcome) {
            return Ok(outcome?);
        }
        self.evict(dc).await?;
        let media = self.media(dc).await?;
        charged(self.charge.as_ref())?;
        let outcome = media.invoke(body).await;
        if key_gone(&outcome) {
            return Err(failed(dc, "the DC does not accept the media session's key"));
        }
        Ok(outcome?)
    }

    /// `auth.exportAuthorization(dc)` on the main session, then
    /// `auth.importAuthorization` on `media`.
    async fn authorize(&self, dc: i32, media: &C::Session) -> Result<(), InvokeError> {
        const EXPORT: &str = "auth.exportAuthorization";
        const IMPORT: &str = "auth.importAuthorization";
        let export = tl::functions::auth::ExportAuthorization { dc_id: dc }.to_bytes();
        let bytes = match self.main.invoke(Bytes::from(export)).await {
            Ok(RpcOutcome::Result(bytes)) => bytes,
            Ok(RpcOutcome::Error { message, .. }) => return Err(refused(dc, EXPORT, &message)),
            Err(e) => return Err(setup_error(dc, EXPORT, &e)),
        };
        let tl::enums::auth::ExportedAuthorization::Authorization(exported) =
            tl::enums::auth::ExportedAuthorization::from_bytes(&bytes)
                .map_err(|_| failed(dc, "auth.exportAuthorization answer unreadable"))?;
        let import = tl::functions::auth::ImportAuthorization {
            id: exported.id,
            bytes: exported.bytes,
        }
        .to_bytes();
        match media.invoke(Bytes::from(import)).await {
            Ok(RpcOutcome::Result(_)) => Ok(()),
            Ok(RpcOutcome::Error { message, .. }) => Err(refused(dc, IMPORT, &message)),
            Err(e) => Err(setup_error(dc, IMPORT, &e)),
        }
    }
}

impl<M: Session, C: Connect, K: KeyStore> AccountRpc for AccountSessions<M, C, K> {
    fn layer(&self) -> i32 {
        self.layer
    }

    fn invoke(&self, route: Route, body: Bytes) -> BoxFuture<'_, Result<RpcOutcome, InvokeError>> {
        Box::pin(async move {
            match route {
                Route::Media(dc) if dc != self.home_dc => self.invoke_media(dc, body).await,
                _ => Ok(self.main.invoke(body).await?),
            }
        })
    }
}
