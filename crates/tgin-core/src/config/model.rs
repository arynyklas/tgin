//! The serde model of `tgin.yaml`, version 1, as written.
//!
//! Every config error must keep its line and column, so the shapes are
//! restricted: no `#[serde(untagged)]`, no internally tagged enums and no
//! `flatten`, all of which make serde-saphyr buffer the input and lose
//! locations. Fields that accept a string or an object have a hand-written
//! `Deserialize`; every other enum is externally tagged. Their JSON Schemas
//! are in `schema.rs`.

use std::borrow::Borrow;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::time::Duration;

use indexmap::IndexMap;
use ipnet::IpNet;
use schemars::JsonSchema;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_saphyr::{Location, Spanned};

use super::defaults;

/// Text that replaces secrets in `Debug` and serialized output.
pub(super) const REDACTED: &str = "[redacted]";

/// The configuration file, version 1.
///
/// Only secrets (`token`, `secret_token`, `auth_token`, `admin.token`) and
/// fields holding URLs or file paths expand `${env:NAME}` and `${file:PATH}`
/// references; every other value is literal.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[schemars(title = "tgin configuration, version 1")]
pub struct RawConfig {
    /// The config format version; must be 1.
    #[schemars(extend("const" = 1))]
    pub version: At<u32>,
    #[serde(default)]
    pub runtime: Runtime,
    #[serde(default)]
    pub log: Log,
    /// The data plane: webhook ingress and pull consumers. Omitted, tgin
    /// serves neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Server>,
    #[serde(default)]
    pub admin: Admin,
    #[serde(default)]
    pub reload: Reload,
    /// The Telegram bots updates come from.
    #[serde(default)]
    pub bots: Vec<Bot>,
    /// Where updates go, by name.
    // A located key would hide the name from error paths, so a consumer's
    // errors point at its value instead.
    #[serde(default)]
    pub consumers: IndexMap<Name, At<Consumer>>,
}

/// Process settings. `worker_threads` takes effect after a restart; a
/// shutdown uses the `lame_duck` and `shutdown_grace` of the config in
/// effect when it begins.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct Runtime {
    /// Tokio worker threads: `auto` (one per CPU) or a count.
    pub worker_threads: WorkerThreads,
    /// How long, once shutdown begins, tgin keeps serving while readiness
    /// reports "not ready", before it stops ingesting; skipped without a
    /// `server` block.
    pub lame_duck: At<HumanDuration>,
    /// How long shutdown waits for queues to drain.
    pub shutdown_grace: At<HumanDuration>,
}

/// Logging, to stdout.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct Log {
    /// The most verbose level logged; a reload applies it. A non-empty
    /// `RUST_LOG` overrides it.
    pub level: LogLevel,
    /// Log line format; a change takes effect after a restart.
    pub format: LogFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    Text,
    Json,
}

/// The data-plane HTTP server.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct Server {
    /// Addresses to listen on, as `ip:port`.
    pub listen: Vec<At<SocketAddr>>,
    /// Serve HTTPS with these files. Turning TLS on or off takes effect
    /// after a restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<Tls>,
    /// Proxies whose `X-Forwarded-For` is trusted, as networks.
    pub trusted_proxies: Vec<Cidr>,
    /// Most open connections.
    #[schemars(range(min = 1))]
    pub max_connections: At<usize>,
    /// How long a client may take to send the request headers; above 0.
    pub header_read_timeout: At<HumanDuration>,
}

/// The data plane's certificate. Every config reload reads both files
/// again and swaps the certificate in if they changed.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Tls {
    /// PEM certificate chain file; a relative path is relative to the config
    /// file's directory. Supports `${env:…}` and `${file:…}`.
    pub cert_file: Template,
    /// PEM private key file; a relative path is relative to the config
    /// file's directory. Supports `${env:…}` and `${file:…}`.
    pub key_file: Template,
}

/// The admin HTTP server: health, metrics and the management API.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct Admin {
    /// Addresses to listen on, as `ip:port`. A non-loopback address needs
    /// `token`.
    pub listen: Vec<At<SocketAddr>>,
    /// Bearer token for the admin API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<SecretText>,
    /// Serve `/metrics` without the token.
    pub public_metrics: bool,
}

/// Hot reload of the config file.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct Reload {
    /// Reload when the config file changes.
    pub watch: bool,
    /// Quiet time after a change before reloading; at least 10ms.
    pub debounce: At<HumanDuration>,
    /// How long a removed consumer drains before it is dropped.
    pub drain_timeout: At<HumanDuration>,
}

/// One Telegram bot.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Bot {
    /// Unique bot name, used in metrics and logs.
    pub name: At<Name>,
    /// The bot token: only ASCII letters, digits, `:`, `_` and `-`. The Bot
    /// API gateway serves only tokens shaped `<digits>:<secret>`.
    pub token: SecretText,
    /// Bot API base URL. Supports `${env:…}` and `${file:…}`.
    #[serde(default = "defaults::api_url")]
    pub api_url: Template,
    /// How updates arrive; at most one. Omitted, the bot receives none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<At<Ingress>>,
    /// The Bot API gateway for this bot's downstream clients. Changes apply
    /// live.
    #[serde(default)]
    pub gateway: Gateway,
    /// Rules in order; the first match wins.
    #[serde(default)]
    pub routes: Vec<At<Route>>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[expect(
    clippy::large_enum_variant,
    reason = "built once per config load; boxing would only complicate the model"
)]
pub enum Ingress {
    /// Poll Telegram with `getUpdates`.
    LongPoll(LongPoll),
    /// Receive updates on the data-plane server.
    Webhook(Webhook),
}

/// Long polling: tgin calls `getUpdates` on the Bot API.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct LongPoll {
    /// `getUpdates` long-poll timeout; at least 1 s.
    pub timeout: At<HumanDuration>,
    /// Updates per `getUpdates` call, 1 to 100.
    #[schemars(range(min = 1, max = 100))]
    pub limit: At<u32>,
    /// Update types to receive, sent with every `getUpdates`, so the config
    /// decides what arrives. Omitted, tgin sends `[]`: Telegram's default,
    /// every type except `chat_member`, `message_reaction` and
    /// `message_reaction_count`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_updates: Option<Vec<String>>,
    /// Call `deleteWebhook` (keeping pending updates) before polling, and
    /// again when a poll finds a webhook set. Off, an active webhook stops
    /// the polling until something else deletes it.
    pub delete_webhook_on_start: bool,
}

/// Telegram posts updates to the data plane.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct Webhook {
    /// HTTP path updates are posted to. Omitted, `/webhook/<bot name>`. It
    /// is matched as written, so it must be what a URL sends: no `?`, `#`,
    /// spaces, control or non-ASCII characters, `.`/`..` segments or `\`.
    /// Supports `${env:…}` and `${file:…}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Template>,
    /// The public HTTPS base URL Telegram reaches tgin at; `setWebhook`
    /// registers it followed by `path`. Supports `${env:…}` and `${file:…}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<Template>,
    /// Call `setWebhook` at start; needs `public_url`.
    pub register: At<bool>,
    /// `auto` (derived from the bot token), `none`, or the secret Telegram
    /// must send: 1 to 256 ASCII letters, digits, `_` or `-`.
    pub secret_token: WebhookSecret,
    /// Update types to receive, sent with `setWebhook`, so the config decides
    /// what arrives. Omitted, tgin sends `[]`: Telegram's default, every type
    /// except `chat_member`, `message_reaction` and `message_reaction_count`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_updates: Option<Vec<String>>,
    /// Telegram's `max_connections`, 1 to 100.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100))]
    pub max_connections: Option<At<u32>>,
    /// Telegram's `drop_pending_updates` for `setWebhook`.
    pub drop_pending_updates: bool,
    /// Telegram's `ip_address` for `setWebhook`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_address: Option<IpAddr>,
    /// Self-signed certificate to upload with `setWebhook`; needs
    /// `register: true`. A relative path is relative to the config file's
    /// directory. Supports `${env:…}` and `${file:…}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certificate_file: Option<Template>,
    /// Who may post updates: `none` (anyone), `telegram` (Telegram's
    /// published networks) or a list of networks.
    pub ip_allowlist: IpAllowlist,
}

/// The Bot API gateway: how tgin forwards the bot's requests from
/// downstream clients to the Bot API.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct Gateway {
    /// Longest wait for the Bot API to accept or send the next bytes of a
    /// request; at least 1s. A transfer that keeps moving is never cut.
    pub upstream_idle_timeout: At<HumanDuration>,
    pub rate_limit: RateLimit,
    pub retry_429: Retry429,
}

/// Telegram's flood limits, applied before a request goes upstream.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct RateLimit {
    pub mode: RateMode,
    /// Longest delay `shape` adds before it answers 429 instead; at most
    /// 1m. `0s` behaves like `reject`.
    pub max_delay: At<HumanDuration>,
    /// The methods limited, ignoring case: a method name, or a prefix
    /// followed by `*`. At least one unless `mode: off`.
    #[schemars(inner(pattern(super::schema::METHOD_PATTERN)))]
    pub methods: At<Vec<At<String>>>,
    pub global: GlobalRate,
    pub per_chat: ChatRate,
    pub per_group: GroupRate,
}

/// What a request beyond the limits gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateMode {
    /// Delayed until the limits admit it, up to `max_delay`; a longer wait
    /// is answered with 429 at once.
    Shape,
    /// Answered with 429 at once.
    Reject,
    /// No limits.
    Off,
}

/// One rate block; each has its own defaults.
macro_rules! rate_block {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
        #[serde(default, deny_unknown_fields, rename_all = "snake_case")]
        pub struct $name {
            /// Requests per `per`; at least 1.
            #[schemars(range(min = 1))]
            pub count: At<u32>,
            /// The period `count` requests are spread over; above 0.
            pub per: At<HumanDuration>,
            /// Requests let through at once before the rate applies; at
            /// least 1.
            #[schemars(range(min = 1))]
            pub burst: At<u32>,
        }

        impl $name {
            /// The block's fields as one shape.
            pub(super) fn rate(&self) -> Rate<'_> {
                Rate {
                    count: &self.count,
                    per: &self.per,
                    burst: &self.burst,
                }
            }
        }
    };
}

rate_block! {
    /// Requests of the bot, over all chats.
    GlobalRate
}

rate_block! {
    /// Requests per chat.
    ChatRate
}

rate_block! {
    /// Requests per group, supergroup or channel (a negative `chat_id` or an
    /// `@username`), on top of `per_chat`.
    GroupRate
}

/// The fields of any rate block.
pub(super) struct Rate<'a> {
    pub(super) count: &'a At<u32>,
    pub(super) per: &'a At<HumanDuration>,
    pub(super) burst: &'a At<u32>,
}

/// The transparent retry of a request the Bot API answered with 429.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct Retry429 {
    /// Longest `retry_after` waited out before the one retry; at most 1m.
    /// A longer one passes the 429 through; `0s` turns the retry off.
    pub max_wait: At<HumanDuration>,
}

/// One routing rule.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Route {
    /// When the rule applies. Omitted, it matches every update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#match: Option<Match>,
    /// Where matching updates go; every entry receives the update.
    pub to: Vec<At<Target>>,
}

/// Rule predicates: values within a list are alternatives, fields must all
/// hold, and an empty list matches anything.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct Match {
    /// Update types such as `message` or `callback_query`.
    pub update_types: Vec<String>,
    /// Chat ids.
    pub chat_ids: Vec<i64>,
    /// Chat types.
    pub chat_types: Vec<ChatType>,
    /// Sender user ids.
    pub user_ids: Vec<i64>,
    /// Commands, with or without the leading `/`: 1 to 32 ASCII letters,
    /// digits or `_`, with no `@bot` suffix.
    #[schemars(inner(regex(pattern = "^/?[A-Za-z0-9_]{1,32}$")))]
    pub commands: Vec<At<String>>,
    /// Message text or caption.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextMatch>,
    /// Callback query data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<TextMatch>,
}

impl Match {
    /// Whether every update satisfies these predicates.
    pub fn is_empty(&self) -> bool {
        self.update_types.is_empty()
            && self.chat_ids.is_empty()
            && self.chat_types.is_empty()
            && self.user_ids.is_empty()
            && self.commands.is_empty()
            && self.text.is_none()
            && self.callback_data.is_none()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatType {
    Private,
    Group,
    Supergroup,
    Channel,
}

/// A text match; each list is non-empty.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum TextMatch {
    /// Starts with any of these (case-sensitive).
    Prefix(Vec<String>),
    /// Any of these regexes matches somewhere in the text.
    Regex(Vec<String>),
    /// Equals any of these exactly.
    Equals(Vec<String>),
}

/// A route target: a consumer name, `{split: …}` or `{all: […]}`.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Consumer(String),
    /// Exactly one child receives the update.
    Split(Split),
    /// Every child receives the update.
    All(Vec<At<Target>>),
}

/// Exactly one child of `among` receives each update.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Split {
    pub strategy: At<SplitStrategy>,
    /// The children: consumer names or nested targets.
    pub among: Vec<At<Target>>,
    /// One weight per `among` entry, each above 0; only with `weighted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(inner(range(min = 1)))]
    pub weights: Option<At<Vec<At<u32>>>>,
}

/// How a split picks its child.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SplitStrategy {
    /// Each child in turn.
    RoundRobin,
    /// Sticky by chat (or user); round robin without either.
    HashChat,
    /// Smooth weighted round robin over `weights`.
    Weighted,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[expect(
    clippy::large_enum_variant,
    reason = "built once per config load; boxing would only complicate the model"
)]
pub enum Consumer {
    /// Bots fetch updates from tgin with `getUpdates`.
    Pull(Pull),
    /// tgin posts updates to HTTP endpoints.
    Push(Push),
}

/// Bots fetch updates from tgin with `getUpdates` on
/// `/c/<consumer>/getUpdates` and on each alias.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct Pull {
    /// Most queued updates.
    #[schemars(range(min = 1))]
    pub capacity: At<usize>,
    /// Most queued bytes.
    pub capacity_bytes: At<ByteSize>,
    pub on_full: OnFull,
    /// How long a fetched update waits for its acknowledgement before
    /// another poll may receive it; at least 1s.
    pub lease_timeout: At<HumanDuration>,
    pub ordering: Ordering,
    /// Bearer token pollers must send.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<SecretText>,
    /// Extra HTTP paths serving this consumer, matched as written, with the
    /// same rules as a webhook `path`. Each supports `${env:…}` and
    /// `${file:…}`.
    pub aliases: Vec<Template>,
    /// Let `deleteWebhook` with `drop_pending_updates: true`, sent on this
    /// consumer's gateway path, empty the consumer's queue. Off, the flag
    /// is ignored.
    pub honor_drop_pending: bool,
}

/// tgin posts each update to HTTP endpoints, with retries and a circuit
/// breaker per endpoint.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Push {
    /// Where updates are posted. Empty only with `accept_set_webhook: true`;
    /// updates then queue until a `setWebhook` gives an endpoint.
    #[serde(default = "defaults::endpoints")]
    pub endpoints: At<Vec<Endpoint>>,
    #[serde(default = "defaults::balance")]
    pub balance: Balance,
    /// Most deliveries at once over all endpoints, counting those waiting
    /// to retry.
    #[serde(default = "defaults::max_in_flight")]
    #[schemars(range(min = 1))]
    pub max_in_flight: At<usize>,
    #[serde(default = "defaults::ordering")]
    pub ordering: Ordering,
    /// Most queued updates.
    #[serde(default = "defaults::capacity")]
    #[schemars(range(min = 1))]
    pub capacity: At<usize>,
    /// Most queued bytes.
    #[serde(default = "defaults::capacity_bytes")]
    pub capacity_bytes: At<ByteSize>,
    #[serde(default = "defaults::on_full")]
    pub on_full: OnFull,
    #[serde(default)]
    pub retry: Retry,
    #[serde(default)]
    pub breaker: Breaker,
    /// Let `setWebhook`, sent on this consumer's gateway path, add its URL
    /// as an endpoint until tgin restarts, instead of answering it without
    /// effect. Only for a consumer fed by one bot.
    #[serde(default)]
    pub accept_set_webhook: bool,
    #[serde(default = "defaults::webhook_reply")]
    pub webhook_reply: WebhookReply,
    /// Let `deleteWebhook` with `drop_pending_updates: true`, sent on this
    /// consumer's gateway path, empty the consumer's queue. Off, the flag
    /// is ignored.
    #[serde(default)]
    pub honor_drop_pending: bool,
}

/// What tgin does with a Bot API method call in an endpoint's response
/// body, the way Telegram handles a webhook reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebhookReply {
    /// Call the method through the gateway, once, before the update is
    /// acknowledged.
    Execute,
    /// Ignore the response body.
    Ignore,
}

/// One downstream URL.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Endpoint {
    /// `http` or `https` URL. Supports `${env:…}` and `${file:…}`.
    pub url: Template,
    /// Sent as `X-Telegram-Bot-Api-Secret-Token`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_token: Option<SecretText>,
    /// Per-request timeout; above 0.
    #[serde(default = "defaults::endpoint_timeout")]
    pub timeout: At<HumanDuration>,
}

/// How a push consumer picks the endpoint for each attempt, among those
/// whose circuit breaker admits a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Balance {
    /// Each endpoint in turn.
    RoundRobin,
    /// Sticky by chat (or user); round robin without either.
    HashChat,
    /// The endpoint with the fewest requests in flight.
    LeastInFlight,
}

/// What a full queue does with a new update.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnFull {
    /// Refuse it; the source retries later.
    Backpressure,
    /// Evict the oldest queued updates.
    DropOldest,
}

/// The order a consumer hands out updates in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Ordering {
    /// Queue order, any number per chat at once.
    None,
    /// One update per chat (or user) in flight at a time; updates with
    /// neither are never held back.
    PerChat,
}

/// Retries of failed deliveries.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct Retry {
    /// First backoff; above 0.
    pub initial: At<HumanDuration>,
    /// Longest backoff; above 0.
    pub max: At<HumanDuration>,
    /// Attempts before the update is dead-lettered; at least 1.
    #[schemars(range(min = 1))]
    pub max_attempts: At<u32>,
    /// Age after which the update is dead-lettered, counted from when it
    /// was queued; above 0.
    pub max_age: At<HumanDuration>,
}

/// Per-endpoint circuit breaker.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct Breaker {
    /// Consecutive transient failures (408, 429, 5xx, timeouts, network
    /// errors) that open the breaker; at least 1.
    #[schemars(range(min = 1))]
    pub failure_threshold: At<u32>,
    /// How long the breaker stays open; above 0.
    pub open_for: At<HumanDuration>,
}

/// A value and where it was written; equality and hashing ignore the place.
#[derive(Clone, Debug)]
pub struct At<T> {
    pub value: T,
    pub location: Location,
}

impl<T> At<T> {
    /// A value that was not written in the document, such as a default.
    pub fn new(value: T) -> Self {
        Self {
            value,
            location: Location::UNKNOWN,
        }
    }
}

impl<T: PartialEq> PartialEq for At<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: Eq> Eq for At<T> {}

impl<T: Hash> Hash for At<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for At<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Spanned::<T>::deserialize(deserializer).map(|spanned| Self {
            value: spanned.value,
            location: spanned.referenced,
        })
    }
}

impl<T: Serialize> Serialize for At<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

/// A bot or consumer name: `^[a-z0-9][a-z0-9-]{0,62}$`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Name(pub String);

impl Name {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Lets `consumers` be looked up by `&str`.
impl Borrow<str> for Name {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// A secret as written, before `${env:…}`/`${file:…}` expansion. It is
/// redacted in `Debug` and serialized output.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct SecretText(pub At<String>);

impl fmt::Debug for SecretText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl Serialize for SecretText {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(REDACTED)
    }
}

/// A URL or path as written, before `${env:…}`/`${file:…}` expansion.
/// `Debug` is opaque: the text may hold credentials, and only a resolved
/// [`Config`](super::Config) knows the secrets to mask in it.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Template(pub At<String>);

impl fmt::Debug for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Template(..)")
    }
}

/// A duration such as `30s`, `5m` or `1h 30m`, keeping its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HumanDuration {
    value: Duration,
    text: String,
}

/// humantime's month and year units: `5M` would silently mean five months.
const CALENDAR_UNITS: [&str; 8] = ["M", "month", "months", "y", "year", "years", "yr", "yrs"];

impl HumanDuration {
    pub fn get(&self) -> Duration {
        self.value
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The month or year unit the text uses, if any.
    pub fn calendar_unit(&self) -> Option<&str> {
        self.text
            .split(|c: char| c.is_ascii_digit() || c.is_whitespace())
            .find(|unit| CALENDAR_UNITS.contains(unit))
    }
}

impl From<Duration> for HumanDuration {
    fn from(value: Duration) -> Self {
        Self {
            value,
            text: humantime::format_duration(value).to_string(),
        }
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DurationVisitor;

        impl Visitor<'_> for DurationVisitor {
            type Value = HumanDuration;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration such as `30s`, or 0")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<HumanDuration, E> {
                match humantime::parse_duration(text) {
                    Ok(value) => Ok(HumanDuration {
                        value,
                        text: text.to_owned(),
                    }),
                    Err(err) => Err(E::custom(format_args!("invalid duration `{text}`: {err}"))),
                }
            }

            /// An unquoted `0`; any other number needs a unit.
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<HumanDuration, E> {
                if n == 0 {
                    self.visit_str("0")
                } else {
                    Err(E::custom(format_args!(
                        "invalid duration `{n}`: add a unit, such as `{n}s`"
                    )))
                }
            }

            fn visit_i64<E: de::Error>(self, n: i64) -> Result<HumanDuration, E> {
                match u64::try_from(n) {
                    Ok(n) => self.visit_u64(n),
                    Err(_) => Err(E::invalid_value(de::Unexpected::Signed(n), &self)),
                }
            }
        }

        deserializer.deserialize_any(DurationVisitor)
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

/// A size in bytes: an integer, or a string such as `512KiB` or `64MiB`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ByteSize(pub u64);

const SIZE_UNITS: [(&str, u64); 4] = [
    ("GiB", 1 << 30),
    ("MiB", 1 << 20),
    ("KiB", 1 << 10),
    ("B", 1),
];

impl ByteSize {
    fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let digits = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        let (number, unit) = text.split_at(digits);
        let unit = unit.trim_start();
        let multiplier = if unit.is_empty() {
            Some(1)
        } else {
            SIZE_UNITS
                .iter()
                .find(|(name, _)| *name == unit)
                .map(|(_, m)| *m)
        };
        let bad =
            || format!("invalid size `{text}`; use bytes or a number with B, KiB, MiB or GiB");
        let number: u64 = number.parse().map_err(|_| bad())?;
        multiplier
            .ok_or_else(bad)?
            .checked_mul(number)
            .map(Self)
            .ok_or_else(|| format!("size `{text}` is too large"))
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SizeVisitor;

        impl Visitor<'_> for SizeVisitor {
            type Value = ByteSize;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a size in bytes, such as 1048576 or 64MiB")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<ByteSize, E> {
                Ok(ByteSize(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<ByteSize, E> {
                u64::try_from(v)
                    .map(ByteSize)
                    .map_err(|_| E::invalid_value(de::Unexpected::Signed(v), &self))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<ByteSize, E> {
                ByteSize::parse(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(SizeVisitor)
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let unit = SIZE_UNITS
            .iter()
            .find(|(_, m)| *m > 1 && self.0 != 0 && self.0.is_multiple_of(*m));
        match unit {
            Some((name, m)) => serializer.collect_str(&format_args!("{}{name}", self.0 / m)),
            None => serializer.serialize_u64(self.0),
        }
    }
}

/// An IP network such as `10.0.0.0/8`, or a single address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr(pub IpNet);

impl<'de> Deserialize<'de> for Cidr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse::<IpNet>()
            .or_else(|_| text.parse::<IpAddr>().map(IpNet::from))
            .map(Cidr)
            .map_err(|_| {
                de::Error::custom(format_args!(
                    "invalid network `{text}`; expected an address or a CIDR such as 10.0.0.0/8"
                ))
            })
    }
}

impl Serialize for Cidr {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

/// `auto` or a thread count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkerThreads {
    /// One per CPU.
    #[default]
    Auto,
    Count(NonZeroUsize),
}

impl<'de> Deserialize<'de> for WorkerThreads {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ThreadsVisitor;

        impl Visitor<'_> for ThreadsVisitor {
            type Value = WorkerThreads;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("`auto` or a thread count of at least 1")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<WorkerThreads, E> {
                usize::try_from(v)
                    .ok()
                    .and_then(NonZeroUsize::new)
                    .map(WorkerThreads::Count)
                    .ok_or_else(|| E::invalid_value(de::Unexpected::Unsigned(v), &self))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<WorkerThreads, E> {
                match u64::try_from(v) {
                    Ok(v) => self.visit_u64(v),
                    Err(_) => Err(E::invalid_value(de::Unexpected::Signed(v), &self)),
                }
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<WorkerThreads, E> {
                if v == "auto" {
                    Ok(WorkerThreads::Auto)
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(v), &self))
                }
            }
        }

        deserializer.deserialize_any(ThreadsVisitor)
    }
}

impl Serialize for WorkerThreads {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Count(n) => serializer.serialize_u64(n.get() as u64),
        }
    }
}

/// A webhook's `secret_token`: `auto`, `none` or the secret itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WebhookSecret {
    /// Derived from the bot token.
    #[default]
    Auto,
    /// Telegram sends no secret and none is checked.
    None,
    Secret(SecretText),
}

impl<'de> Deserialize<'de> for WebhookSecret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = At::<String>::deserialize(deserializer)?;
        Ok(match text.value.as_str() {
            "auto" => Self::Auto,
            "none" => Self::None,
            _ => Self::Secret(SecretText(text)),
        })
    }
}

impl Serialize for WebhookSecret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::None => serializer.serialize_str("none"),
            Self::Secret(secret) => secret.serialize(serializer),
        }
    }
}

/// A webhook's `ip_allowlist`: `none`, `telegram` or a list of networks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum IpAllowlist {
    /// Any client may post.
    #[default]
    None,
    /// Telegram's published webhook networks.
    Telegram,
    Networks(Vec<Cidr>),
}

impl<'de> Deserialize<'de> for IpAllowlist {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AllowlistVisitor;

        impl<'de> Visitor<'de> for AllowlistVisitor {
            type Value = IpAllowlist;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("`none`, `telegram` or a list of networks")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<IpAllowlist, E> {
                match v {
                    "none" => Ok(IpAllowlist::None),
                    "telegram" => Ok(IpAllowlist::Telegram),
                    _ => Err(E::invalid_value(de::Unexpected::Str(v), &self)),
                }
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<IpAllowlist, A::Error> {
                let mut networks = Vec::new();
                while let Some(network) = seq.next_element()? {
                    networks.push(network);
                }
                Ok(IpAllowlist::Networks(networks))
            }
        }

        deserializer.deserialize_any(AllowlistVisitor)
    }
}

impl Serialize for IpAllowlist {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::None => serializer.serialize_str("none"),
            Self::Telegram => serializer.serialize_str("telegram"),
            Self::Networks(networks) => networks.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TargetVisitor;

        impl<'de> Visitor<'de> for TargetVisitor {
            type Value = Target;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a consumer name, `{split: …}` or `{all: […]}`")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Target, E> {
                Ok(Target::Consumer(v.to_owned()))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Target, A::Error> {
                let Some(key) = map.next_key::<String>()? else {
                    return Err(de::Error::invalid_length(0, &self));
                };
                let target = match key.as_str() {
                    "split" => Target::Split(map.next_value()?),
                    "all" => Target::All(map.next_value()?),
                    other => return Err(de::Error::unknown_variant(other, &["split", "all"])),
                };
                if let Some(extra) = map.next_key::<String>()? {
                    return Err(de::Error::custom(format_args!(
                        "a target has exactly one key, `split` or `all`; found `{key}` and `{extra}`"
                    )));
                }
                Ok(target)
            }
        }

        deserializer.deserialize_any(TargetVisitor)
    }
}

impl Serialize for Target {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Consumer(name) => serializer.serialize_str(name),
            Self::Split(split) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("split", split)?;
                map.end()
            }
            Self::All(children) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("all", children)?;
                map.end()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_size_parses_units() {
        assert_eq!(ByteSize::parse("512KiB"), Ok(ByteSize(512 << 10)));
        assert_eq!(ByteSize::parse("64MiB"), Ok(ByteSize(64 << 20)));
        assert_eq!(ByteSize::parse("2 GiB"), Ok(ByteSize(2 << 30)));
        assert_eq!(ByteSize::parse("10B"), Ok(ByteSize(10)));
        assert_eq!(ByteSize::parse("10"), Ok(ByteSize(10)));
        for bad in ["", "MiB", "64MB", "1.5MiB", "-1", "99999999999999999999GiB"] {
            assert!(ByteSize::parse(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn byte_size_serializes_largest_exact_unit() {
        let text = |n| serde_json::to_string(&ByteSize(n)).unwrap();
        assert_eq!(text(64 << 20), r#""64MiB""#);
        assert_eq!(text(3 << 10), r#""3KiB""#);
        assert_eq!(text(1536), "1536");
        assert_eq!(text(0), "0");
    }

    #[test]
    fn duration_detects_calendar_units() {
        let d = |text: &str| HumanDuration {
            value: Duration::ZERO,
            text: text.to_owned(),
        };
        assert_eq!(d("5M").calendar_unit(), Some("M"));
        assert_eq!(d("1y").calendar_unit(), Some("y"));
        assert_eq!(d("1h 2months").calendar_unit(), Some("months"));
        assert_eq!(d("5m").calendar_unit(), None);
        assert_eq!(d("1h30m").calendar_unit(), None);
    }
}
