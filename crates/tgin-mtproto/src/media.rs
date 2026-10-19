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

use bytes::Bytes;
use grammers_tl_types::{self as tl, Deserializable as _, Serializable as _};
use tokio::sync::Mutex;

use crate::rpc::{AccountRpc, BoxFuture, ErrorClass, InvokeError, MediaFailure, Route, classify};
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

/// One DC's media session: made and authorized, or connected with a new
/// key whose authorization has not succeeded yet. The latter is kept so a
/// retry repeats only the export and import, never the key exchange.
enum Slot<S> {
    Ready(Arc<S>),
    Unauthorized(Arc<S>, Box<AuthKey>),
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
    media: Mutex<HashMap<i32, Slot<C::Session>>>,
}

impl<M, C: Connect, K> AccountSessions<M, C, K> {
    pub fn new(main: M, home_dc: i32, layer: i32, connect: C, keys: K) -> Self {
        Self {
            main,
            home_dc,
            layer,
            connect,
            keys,
            media: Mutex::new(HashMap::new()),
        }
    }
}

impl<M: Session, C: Connect, K: KeyStore> AccountSessions<M, C, K> {
    /// The media session to `dc`, made and authorized on first use. Made
    /// under the cache's lock, so two calls never authorize one DC twice.
    async fn media(&self, dc: i32) -> Result<Arc<C::Session>, InvokeError> {
        let mut cache = self.media.lock().await;
        let (session, key) = match cache.get(&dc) {
            Some(Slot::Ready(session)) => return Ok(Arc::clone(session)),
            Some(Slot::Unauthorized(session, key)) => (Arc::clone(session), AuthKey::clone(key)),
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
                    cache.insert(dc, Slot::Ready(Arc::clone(&session)));
                    return Ok(session);
                };
                cache.insert(
                    dc,
                    Slot::Unauthorized(Arc::clone(&session), Box::new(key.clone())),
                );
                (session, key)
            }
        };
        self.authorize(dc, session.as_ref()).await?;
        self.keys
            .save(dc, &key)
            .map_err(|_| failed(dc, "the session store failed"))?;
        cache.insert(dc, Slot::Ready(Arc::clone(&session)));
        Ok(session)
    }

    /// Drops the media session to `dc` and its stored key.
    async fn evict(&self, dc: i32) -> Result<(), InvokeError> {
        self.media.lock().await.remove(&dc);
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
