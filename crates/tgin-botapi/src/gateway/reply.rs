//! Webhook replies: the method call a push endpoint answered an update
//! with, made to the bot's Bot API server the way Telegram makes a
//! webhook's (decision 3, `WebhookActor.cpp`):
//! - the method is the body's `method` parameter (JSON, urlencoded or a
//!   multipart text field); an empty or missing one, any `get…` method,
//!   `setWebhook`, `deleteWebhook`, `close` and `logOut` are refused, in
//!   any case;
//! - to a bot whose server in local mode must not get a local file
//!   reference, a reply carrying one is refused (`local_files`);
//! - the bot's rate limits apply as to a gateway request;
//! - the body goes to `{url}/bot{token}/{method}` unchanged, with its
//!   `Content-Type`, once: nothing is re-encoded, multipart files included,
//!   and nothing is retried. Nobody sees the result, as with Telegram.

use std::borrow::Cow;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::body::Body;
use axum::http::header::{CONTENT_TYPE, VIA};
use serde::Deserialize;
use tgin_core::ids::SourceId;
use tgin_core::reply::{MAX_REPLY_BYTES, Reply, ReplyExecutor};
use tgin_runtime::http::describe_error;
use tgin_runtime::secret::Redactor;
use tokio::time::{Instant, sleep_until};

use super::chat_key::chat_key;
use super::local_files::{Verdict, body_verdict};
use super::metrics::{MethodLabel, Outcome, RateAction, ReplyMetrics, ReplyOutcome};
use super::params::text_param;
use super::proxy::{deadline, outcome, url};
use super::{Bot, Bounded, Gateway, read_bounded};

/// The longest bound on a reply's call, from connecting to the end of the
/// response; the resolved upstream's `response_timeout`, when shorter,
/// bounds it instead. Not `Bot::response_timeout`: a reply's v1 bound was
/// always 30 s, and the `api_url` shorthand resolves to 60 s, which keeps
/// it. The body is in memory, so no upload can be slow.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest method name executed; Bot API names are far shorter.
const MAX_METHOD_LEN: usize = 64;
/// The longest error body of the server's read for its `description`.
const REFUSAL_LIMIT: usize = 64 * 1024;
/// The most bytes of the server's `description` a log line carries.
const MAX_DESCRIPTION_LEN: usize = 256;

impl Gateway {
    /// The executor push consumers hand webhook replies to: this gateway,
    /// held weakly, so that the engine's consumers never keep the gateway
    /// (and the engine it holds) alive. Once the gateway is gone, replies
    /// are dropped.
    pub fn reply_executor(self: &Arc<Self>) -> Arc<dyn ReplyExecutor> {
        Arc::new(Replies(Arc::downgrade(self)))
    }

    /// The `tgin_webhook_replies_total` handles of `consumer`, created on
    /// its first reply.
    fn reply_metrics(&self, consumer: &str) -> ReplyMetrics {
        let mut replies = self.replies.lock();
        if let Some(metrics) = replies.get(consumer) {
            return metrics.clone();
        }
        let metrics = self.metrics.replies(consumer);
        replies.insert(Arc::from(consumer), metrics.clone());
        metrics
    }

    /// Forgets the reply series of consumers the engine no longer has, a
    /// removed one's once its drain has ended (see
    /// [`ReplyExecutor::forget`]).
    pub(super) fn retain_reply_metrics(&self) {
        let mut replies = self.replies.lock();
        replies.retain(|consumer, _| {
            let kept = self.engine.knows_consumer(consumer);
            if !kept {
                self.metrics.remove_replies(consumer);
            }
            kept
        });
    }

    /// Makes the call of `reply`, which the push consumer `consumer` got
    /// for an update of the bot `source`, and counts its outcome. A reply
    /// from a runtime endpoint runs only with the token that set the
    /// endpoint, checked once the limits let it go: a reload that rotated
    /// the token since refuses it.
    async fn run_reply(&self, consumer: &str, source: SourceId, reply: Reply) {
        let counts = self.reply_metrics(consumer);
        let table = self.table.load_full();
        let Some(bot) = table.bots.iter().find(|bot| bot.source == source) else {
            counts.count(ReplyOutcome::Failed);
            tracing::warn!(consumer = %consumer, "webhook reply not executed: the gateway does not serve its bot");
            return;
        };
        let method = text_param(Some(&reply.content_type), &reply.body, "method");
        let Some(method) = method.filter(|method| executable(method)) else {
            counts.count(ReplyOutcome::Refused);
            tracing::debug!(bot = %bot.name, consumer = %consumer, "webhook reply refused: no method, or one the Bot API server refuses in a webhook reply");
            return;
        };
        let label = MethodLabel::of(&method);
        if bot.refuses_local_files
            && let Verdict::Refused(refusal) = body_verdict(Some(&reply.content_type), &reply.body)
        {
            counts.count(ReplyOutcome::Refused);
            tracing::warn!(bot = %bot.name, consumer = %consumer, method = label.as_str(), reason = refusal.reason(), "webhook reply refused: it carries a local file reference, or parameters tgin cannot check, for a Bot API server in local mode");
            return;
        }
        if bot.limiter.applies(&method) && !admit(bot, consumer, label, &reply, &counts).await {
            return;
        }
        if let Some(binding) = reply.bound_to
            && !self.engine.holds_token(binding, &bot.token)
        {
            counts.count(ReplyOutcome::Refused);
            tracing::warn!(bot = %bot.name, consumer = %consumer, method = label.as_str(), "webhook reply refused: the token of the bot whose setWebhook set the endpoint changed");
            return;
        }
        let call = Call {
            bot,
            consumer,
            label,
            counts: &counts,
        };
        call.send(&method, &reply).await;
    }
}

/// Books the reply on the bot's limits and waits out the delay they give;
/// `false` when they refuse it (counted and logged). A multipart body's
/// chat is not read: only the global quota applies, as for a streamed
/// gateway request.
async fn admit(
    bot: &Bot,
    consumer: &str,
    label: MethodLabel,
    reply: &Reply,
    counts: &ReplyMetrics,
) -> bool {
    let chat = chat_key(Some(&reply.content_type), &reply.body);
    let now = Instant::now();
    match bot.limiter.acquire(chat.as_ref(), now) {
        Ok(admitted) => {
            if !admitted.delay.is_zero() {
                bot.metrics.ratelimited(admitted.scope, RateAction::Delayed);
                tracing::debug!(bot = %bot.name, consumer = %consumer, method = label.as_str(), scope = admitted.scope.as_str(), delay = ?admitted.delay, "webhook reply delayed by the rate limits");
                sleep_until(deadline(now, admitted.delay)).await;
            }
            true
        }
        Err(limited) => {
            bot.metrics.ratelimited(limited.scope, RateAction::Rejected);
            counts.count(ReplyOutcome::RateLimited);
            tracing::warn!(bot = %bot.name, consumer = %consumer, method = label.as_str(), scope = limited.scope.as_str(), retry_after = ?limited.retry_after, "webhook reply refused by the rate limits; it is not executed");
            false
        }
    }
}

/// Whether a webhook reply may call `method`: a Bot API method name of at
/// most [`MAX_METHOD_LEN`] ASCII letters, digits and `_` (so it is one
/// clean path segment), that the Bot API server does not refuse in a reply.
fn executable(method: &str) -> bool {
    let refused = method
        .get(..3)
        .is_some_and(|get| get.eq_ignore_ascii_case("get"))
        || ["setwebhook", "deletewebhook", "close", "logout"]
            .iter()
            .any(|name| method.eq_ignore_ascii_case(name));
    !method.is_empty()
        && method.len() <= MAX_METHOD_LEN
        && method
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !refused
}

/// One reply's call to the bot's Bot API server.
struct Call<'a> {
    bot: &'a Bot,
    consumer: &'a str,
    label: MethodLabel,
    counts: &'a ReplyMetrics,
}

impl Call<'_> {
    /// POSTs the reply's body as it came, with its `Content-Type`, to
    /// `method`, and counts the answer: in `tgin_webhook_replies_total`
    /// and, as every upstream call, in `tgin_gateway_requests_total`. No
    /// answer after the body was sent, the call may have been executed.
    async fn send(&self, method: &str, reply: &Reply) {
        let bot = self.bot;
        let handles = bot.metrics.method(self.label);
        let started = Instant::now();
        let sent = bot
            .client
            .post(url(&bot.method_base, method, None))
            .timeout(bot.upstream.response_timeout.min(REPLY_TIMEOUT))
            .header(CONTENT_TYPE, reply.content_type.clone())
            .header(VIA, bot.via.clone())
            .body(reply.body.clone())
            .send()
            .await;
        let response = match sent {
            Ok(response) => response,
            Err(err) => {
                bot.upstream_metrics.up().unreachable();
                handles.count(Outcome::Unreachable);
                self.counts.count(ReplyOutcome::Failed);
                if err.is_connect() {
                    tracing::warn!(bot = %bot.name, consumer = %self.consumer, method = self.label.as_str(), error = %describe_error(&err), "webhook reply failed: the Bot API server is unreachable");
                } else {
                    tracing::warn!(bot = %bot.name, consumer = %self.consumer, method = self.label.as_str(), error = %describe_error(&err), "webhook reply failed: the Bot API server did not answer; the call may have been executed");
                }
                return;
            }
        };
        bot.upstream_metrics.up().answered();
        handles.upstream_seconds(started.elapsed().as_secs_f64());
        let status = response.status();
        handles.count(outcome(status));
        if status.is_success() {
            self.counts.count(ReplyOutcome::Executed);
            tracing::debug!(bot = %bot.name, consumer = %self.consumer, method = self.label.as_str(), "webhook reply executed");
            return;
        }
        self.counts.count(ReplyOutcome::Failed);
        // Bounded by the request's total timeout, as the whole call is.
        let body = Body::new(axum::http::Response::<reqwest::Body>::from(response).into_body());
        let body = match read_bounded(body, REFUSAL_LIMIT).await {
            Ok(Bounded::Complete(body)) => Some(body),
            Ok(Bounded::Over(_)) | Err(_) => None,
        };
        let description = body
            .as_deref()
            .and_then(description)
            .map(|description| redacted(&description, bot.token.expose()))
            .unwrap_or_default();
        tracing::warn!(bot = %bot.name, consumer = %self.consumer, method = self.label.as_str(), status = status.as_u16(), description = %description, "webhook reply failed: the Bot API server refused it");
    }
}

/// The `description` of a Bot API error body.
fn description(body: &[u8]) -> Option<Cow<'_, str>> {
    #[derive(Deserialize)]
    struct Refusal<'a> {
        #[serde(borrow)]
        description: Option<Cow<'a, str>>,
    }
    serde_json::from_slice::<Refusal<'_>>(body)
        .ok()?
        .description
}

/// `description` fit for a log line: the bot's token and anything shaped
/// like a token masked, then cut to [`MAX_DESCRIPTION_LEN`] bytes.
fn redacted(description: &str, token: &str) -> String {
    let own = if !token.is_empty() && description.contains(token) {
        Cow::Owned(description.replace(token, "[redacted]"))
    } else {
        Cow::Borrowed(description)
    };
    let mut masked = Redactor::new(std::iter::empty::<String>())
        .redact(&own)
        .into_owned();
    if masked.len() > MAX_DESCRIPTION_LEN {
        let mut end = MAX_DESCRIPTION_LEN;
        while !masked.is_char_boundary(end) {
            end -= 1;
        }
        masked.truncate(end);
    }
    masked
}

impl ReplyExecutor for Gateway {
    fn execute<'a>(
        &'a self,
        consumer: &'a str,
        source: SourceId,
        reply: Reply,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(self.run_reply(consumer, source, reply))
    }

    fn too_large(&self, consumer: &str, source: SourceId) {
        self.reply_metrics(consumer).count(ReplyOutcome::TooLarge);
        let table = self.table.load();
        let bot = table
            .bots
            .iter()
            .find(|bot| bot.source == source)
            .map_or("?", |bot| &*bot.name);
        tracing::warn!(bot = %bot, consumer = %consumer, limit = MAX_REPLY_BYTES, "webhook reply over the size limit; it is not executed");
    }

    fn forget(&self, consumer: &str) {
        if self.replies.lock().remove(consumer).is_some() {
            self.metrics.remove_replies(consumer);
        }
    }
}

/// [`Gateway::reply_executor`]: the gateway, held weakly.
struct Replies(Weak<Gateway>);

impl ReplyExecutor for Replies {
    fn execute<'a>(
        &'a self,
        consumer: &'a str,
        source: SourceId,
        reply: Reply,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if let Some(gateway) = self.0.upgrade() {
                gateway.run_reply(consumer, source, reply).await;
            }
        })
    }

    fn too_large(&self, consumer: &str, source: SourceId) {
        if let Some(gateway) = self.0.upgrade() {
            gateway.too_large(consumer, source);
        }
    }

    fn forget(&self, consumer: &str) {
        if let Some(gateway) = self.0.upgrade() {
            gateway.forget(consumer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_rule_as_telegram() {
        for method in [
            "sendMessage",
            "SENDMESSAGE",
            "answerCallbackQuery",
            "sendChatAction",
            "deleteMessage",
            "setMyCommands",
            "ge",
            "closeForumTopic",
            "logOutX",
            "a_1",
        ] {
            assert!(executable(method), "{method}");
        }
        for method in [
            "",
            "get",
            "getMe",
            "GetChat",
            "GETUPDATES",
            "setWebhook",
            "SETWEBHOOK",
            "deleteWebhook",
            "close",
            "CLOSE",
            "logOut",
            "logout",
        ] {
            assert!(!executable(method), "{method}");
        }
    }

    #[test]
    fn method_must_be_one_short_segment() {
        assert!(executable(&"a".repeat(MAX_METHOD_LEN)));
        for method in [
            "a".repeat(MAX_METHOD_LEN + 1),
            "send/../getMe".to_owned(),
            "sendMessage?x=1".to_owned(),
            "send%2FMessage".to_owned(),
            "send Message".to_owned(),
            "http:x".to_owned(),
            "sendMessagé".to_owned(),
        ] {
            assert!(!executable(&method), "{method}");
        }
    }

    #[test]
    fn description_masked_and_capped() {
        let token = "123456789:AAAAexampleSecretTokenValue_-xyz";
        let other = "987654321:BBBBanotherSecretTokenValue_-xyz";
        let text = format!("Bad Request: {token} and {other}");
        assert_eq!(redacted(&text, token), "Bad Request: [redacted] and #####");
        let long = format!("{}é{}", "a".repeat(255), "b".repeat(10));
        let capped = redacted(&long, token);
        assert_eq!(capped, "a".repeat(255), "cut on a char boundary");
        assert_eq!(redacted("short", ""), "short");
    }

    #[test]
    fn description_of_error_bodies() {
        assert_eq!(
            description(br#"{"ok":false,"error_code":400,"description":"Bad Request: x\u00e9"}"#)
                .as_deref(),
            Some("Bad Request: xé")
        );
        assert_eq!(description(br#"{"ok":false}"#), None);
        assert_eq!(description(b"<html>"), None);
    }
}
