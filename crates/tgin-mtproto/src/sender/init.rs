//! The connection's init: the `initConnection` call and the latches its
//! refusals and failures set (flood waits, 4xx refusals, backoff).

use super::*;
use crate::retry::Wait;

/// What `initConnection` tells Telegram, and how the connection is used.
#[derive(Clone)]
pub struct InitParams {
    pub api_id: i32,
    pub device_model: String,
    pub system_version: String,
    pub app_version: String,
    pub system_lang_code: String,
    pub lang_pack: String,
    pub lang_code: String,
    /// A media session: every request, the init call included, is wrapped
    /// in `invokeWithoutUpdates`, and pushes are dropped.
    pub without_updates: bool,
    /// The key has a self user: a transport −404 is
    /// [`SenderError::KeyRevoked`], else [`SenderError::KeyUnknown`]. A
    /// login on the sender later calls [`Sender::mark_logged_in`].
    pub logged_in: bool,
    layer: i32,
}

impl InitParams {
    /// tgin's defaults, in grammers' compiled layer. An account serves
    /// exactly that layer.
    #[must_use]
    pub fn new(api_id: i32) -> Self {
        Self {
            api_id,
            device_model: "tgin".to_owned(),
            system_version: std::env::consts::OS.to_owned(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            system_lang_code: "en".to_owned(),
            lang_pack: String::new(),
            lang_code: "en".to_owned(),
            without_updates: false,
            logged_in: false,
            layer: tl::LAYER,
        }
    }

    /// The layer `invokeWithLayer` names: `grammers_tl_types::LAYER`.
    #[must_use]
    pub fn layer(&self) -> i32 {
        self.layer
    }

    /// The sender's own first call on each connection:
    /// `invokeWithLayer(layer, initConnection(…, help.getNearestDc))`, with
    /// the query in `invokeWithoutUpdates` on media sessions.
    pub(super) fn init_call(&self) -> Vec<u8> {
        fn wrap<Q: Serializable>(p: &InitParams, query: Q) -> Vec<u8> {
            tl::functions::InvokeWithLayer {
                layer: p.layer,
                query: tl::functions::InitConnection {
                    api_id: p.api_id,
                    device_model: p.device_model.clone(),
                    system_version: p.system_version.clone(),
                    app_version: p.app_version.clone(),
                    system_lang_code: p.system_lang_code.clone(),
                    lang_pack: p.lang_pack.clone(),
                    lang_code: p.lang_code.clone(),
                    proxy: None,
                    params: None,
                    query,
                },
            }
            .to_bytes()
        }
        let query = tl::functions::help::GetNearestDc {};
        if self.without_updates {
            wrap(self, tl::functions::InvokeWithoutUpdates { query })
        } else {
            wrap(self, query)
        }
    }
}

/// `api_id` is left out: it identifies tgin's Telegram application.
impl fmt::Debug for InitParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InitParams")
            .field("device_model", &self.device_model)
            .field("app_version", &self.app_version)
            .field("layer", &self.layer)
            .field("without_updates", &self.without_updates)
            .field("logged_in", &self.logged_in)
            .finish_non_exhaustive()
    }
}

impl Shared {
    /// The latched init refusal, while it holds.
    pub(super) fn refusal(&mut self, now: Instant) -> Option<SenderError> {
        match &self.refused {
            Some((code, message, until)) if until.is_none_or(|until| now < until) => {
                Some(SenderError::InitRefused {
                    code: *code,
                    message: message.clone(),
                })
            }
            Some(_) => {
                self.refused = None;
                None
            }
            None => None,
        }
    }

    /// What a connection's end teaches about the next one, so failures before init
    /// never loop.
    pub(super) fn learn(&mut self, cause: &Cause, init_done: bool, now: Instant) {
        if init_done {
            self.backoff.reset();
            self.retry_at = None;
        }
        match cause {
            // A 4xx refusal of initConnection holds: retrying cannot help
            // until the sender is rebuilt, except FLOOD_WAIT_X, which holds
            // for X seconds, or for good when X is past the clock's range.
            Cause::InitRefused { code, message } if (400..500).contains(code) => {
                let until = if *code == 420 {
                    now.checked_add(flood_wait(message))
                } else {
                    None
                };
                self.refused = Some((*code, message.clone(), until));
            }
            Cause::Shutdown => {}
            // Any other failure before init: back off.
            _ if !init_done => self.back_off(now),
            _ => {}
        }
    }

    /// Delays the next connection: 1 s, doubling to 60 s.
    pub(super) fn back_off(&mut self, now: Instant) {
        self.retry_at = Some(now + self.backoff.next());
    }
}

/// A 420 without a number in its message.
const DEFAULT_FLOOD_WAIT: Duration = Duration::from_secs(60);

/// The X of a 420 `…_X` message (`FLOOD_WAIT_30`).
fn flood_wait(message: &str) -> Duration {
    Wait::parse(message).map_or(DEFAULT_FLOOD_WAIT, |w| Duration::from_secs(w.seconds))
}
