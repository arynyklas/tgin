//! Telegram Bot API client, update facts extraction, the long-poll and
//! webhook ingress sources, `getUpdates` served from pull consumers, and the
//! Bot API gateway for tgin.

mod client;
mod facts;
pub mod gateway;
mod longpoll;
pub mod pull;
mod secret;
mod webhook;

pub use client::{ApiError, BotClient, BotUser, Identity, SetWebhook, split_updates};
pub use facts::{FactsError, extract};
/// The network type of [`IpAllowlist`] and [`WebhookOptions::trusted_proxies`].
pub use ipnet::IpNet;
pub use longpoll::{LongPollOptions, LongPollSource, OnIdentity, is_fatal};
pub use secret::webhook_secret;
pub use webhook::{
    DEFAULT_WEBHOOK_BODY_LIMIT, IpAllowlist, Registration, WebhookOptions, client_ip, register,
    retry_registration, webhook_router,
};
