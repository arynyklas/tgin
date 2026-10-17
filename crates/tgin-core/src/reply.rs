//! Webhook replies: a Bot API method call that a push endpoint puts in the
//! body of its 2xx answer, as Telegram lets a webhook do. The push consumer
//! reads the reply and hands it to a [`ReplyExecutor`] before it
//! acknowledges the update; the executor (the gateway) makes the call.

use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use reqwest::header::HeaderValue;

use crate::ids::SourceId;

/// The longest reply body read; a longer one is not executed.
pub const MAX_REPLY_BYTES: usize = 1 << 20;

/// A push endpoint's answer to an update, carrying a method call.
#[derive(Clone, Debug)]
pub struct Reply {
    /// The answer's `Content-Type`: JSON, urlencoded or multipart.
    pub content_type: HeaderValue,
    /// The whole body, at most [`MAX_REPLY_BYTES`].
    pub body: Bytes,
    /// Set when the runtime endpoint answered: the bot whose `setWebhook`
    /// set it and the token it did so with. The executor runs the call
    /// only while that token is still the bot's (see
    /// [`crate::engine::Engine::holds_token`]).
    pub bound_to: Option<Binding>,
}

/// The bot and token a runtime endpoint was set with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub source: SourceId,
    /// The engine's keyed fingerprint of the token, never the token.
    pub token_fingerprint: u64,
}

/// Makes the method call of a webhook reply.
pub trait ReplyExecutor: Send + Sync + 'static {
    /// Makes the call `reply` carries for the bot `source`, on behalf of
    /// the push consumer `consumer`. It never fails the delivery: the
    /// downstream has processed the update.
    fn execute<'a>(
        &'a self,
        consumer: &'a str,
        source: SourceId,
        reply: Reply,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// The endpoint answered `consumer`'s update with a reply body longer
    /// than [`MAX_REPLY_BYTES`], which is not executed.
    fn too_large(&self, consumer: &str, source: SourceId);

    /// The consumer `consumer` is gone, its drain included: whatever the
    /// executor keeps for it (its reply metric series) can go too.
    fn forget(&self, consumer: &str);
}

/// The executor push consumers use, set once when it exists: the gateway
/// that executes replies is built after the engine whose consumers read
/// them. Until it is set, no reply is read.
pub type ReplyHandle = Arc<OnceLock<Arc<dyn ReplyExecutor>>>;

/// Whether `content_type` names a body the Bot API reads parameters from:
/// it contains `application/json`, `application/x-www-form-urlencoded` or
/// `multipart/form-data`, ignoring ASCII case, as Telegram matches a webhook
/// answer's type.
pub fn carries_parameters(content_type: &HeaderValue) -> bool {
    let content_type = content_type.as_bytes();
    [
        &b"application/json"[..],
        b"application/x-www-form-urlencoded",
        b"multipart/form-data",
    ]
    .iter()
    .any(|name| {
        content_type
            .windows(name.len())
            .any(|window| window.eq_ignore_ascii_case(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_types_match_as_substrings_in_any_case() {
        for yes in [
            "application/json",
            "Application/JSON; charset=utf-8",
            "application/x-www-form-urlencoded",
            "multipart/form-data; boundary=x",
            "text/plain, application/json",
        ] {
            assert!(carries_parameters(&HeaderValue::from_static(yes)), "{yes}");
        }
        for no in ["", "text/plain", "application/jso", "application/xml"] {
            assert!(!carries_parameters(&HeaderValue::from_static(no)), "{no}");
        }
    }
}
