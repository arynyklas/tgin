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

use crate::rpc::{AccountRpc, BoxFuture, InvokeError, Route};
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

/// An account's sessions: the main one on the home DC and one media
/// session per DC, made on first use. The [`AccountRpc`] the endpoint
/// calls.
pub struct AccountSessions<M, C: Connect, K> {
    main: M,
    home_dc: i32,
    layer: i32,
    connect: C,
    keys: K,
    media: Mutex<HashMap<i32, Arc<C::Session>>>,
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
        if let Some(session) = cache.get(&dc) {
            return Ok(Arc::clone(session));
        }
        let failed = |what| InvokeError::Media { dc, what };
        let stored = self
            .keys
            .load(dc)
            .map_err(|_| failed("the session store failed"))?;
        let (session, new_key) = self.connect.connect(dc, stored).await?;
        if let Some(key) = new_key {
            self.authorize(dc, &session).await?;
            self.keys
                .save(dc, &key)
                .map_err(|_| failed("the session store failed"))?;
        }
        let session = Arc::new(session);
        cache.insert(dc, Arc::clone(&session));
        Ok(session)
    }

    /// Drops the media session to `dc` and its stored key.
    async fn evict(&self, dc: i32) -> Result<(), InvokeError> {
        self.media.lock().await.remove(&dc);
        self.keys.remove(dc).map_err(|_| InvokeError::Media {
            dc,
            what: "the session store failed",
        })
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
            return Err(InvokeError::Media {
                dc,
                what: "the DC does not accept the media session's key",
            });
        }
        Ok(outcome?)
    }

    /// `auth.exportAuthorization(dc)` on the main session, then
    /// `auth.importAuthorization` on `media`.
    async fn authorize(&self, dc: i32, media: &C::Session) -> Result<(), InvokeError> {
        let failed = |what| InvokeError::Media { dc, what };
        let export = tl::functions::auth::ExportAuthorization { dc_id: dc }.to_bytes();
        let RpcOutcome::Result(bytes) = self.main.invoke(Bytes::from(export)).await? else {
            return Err(failed("auth.exportAuthorization refused"));
        };
        let tl::enums::auth::ExportedAuthorization::Authorization(exported) =
            tl::enums::auth::ExportedAuthorization::from_bytes(&bytes)
                .map_err(|_| failed("auth.exportAuthorization answer unreadable"))?;
        let import = tl::functions::auth::ImportAuthorization {
            id: exported.id,
            bytes: exported.bytes,
        }
        .to_bytes();
        match media.invoke(Bytes::from(import)).await? {
            RpcOutcome::Result(_) => Ok(()),
            RpcOutcome::Error { .. } => Err(failed("auth.importAuthorization refused")),
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
