//! The account's RPC port: calls on the sessions of the moment, media
//! routing, what an answer tells of the account, and affected pts.

use super::*;

/// One RPC call on the account's sessions while it lives.
struct RpcCall<'a>(&'a Shared);

impl<'a> RpcCall<'a> {
    pub(super) fn new(shared: &'a Shared) -> Self {
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
pub(super) struct Unlimited;

impl Charge for Unlimited {
    fn charge(&self) -> Result<(), Duration> {
        Ok(())
    }
}

/// Media sessions through the account's network, to the session's DC
/// options (the production table for the others).
pub(super) struct MediaVia {
    pub(super) net: Arc<dyn Network>,
    pub(super) options: Vec<DcOption>,
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
pub(super) struct MediaKeys(pub(super) Weak<SessionStore>);

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

pub(super) fn dc_option(options: &[DcOption], dc: i32) -> Option<DcOption> {
    options
        .iter()
        .find(|o| o.id == dc)
        .copied()
        .or_else(|| DcOption::production(dc))
}

/// The account's [`AccountRpc`]: the sessions of the moment.
pub(super) struct RpcPort {
    pub(super) shared: Arc<Shared>,
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
pub(super) fn observe(
    shared: &Shared,
    route: Route,
    home: i32,
    outcome: &Result<RpcOutcome, InvokeError>,
) {
    let main = match route {
        Route::Main => true,
        Route::Media(dc) => dc == home,
    };
    match outcome {
        Err(InvokeError::Sender(e)) if main && crate::authorization::sender_gone(e) => {
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
pub(super) struct AffectedPort(pub(super) mpsc::Sender<(BoxKind, i32, i32)>);

impl AffectedSink for AffectedPort {
    fn feed_affected(&self, box_kind: BoxKind, pts: i32, pts_count: i32) {
        // Full: the gap these pts leave is fetched instead.
        let _ = self.0.try_send((box_kind, pts, pts_count));
    }
}
