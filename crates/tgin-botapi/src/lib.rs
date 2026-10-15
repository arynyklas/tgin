//! Telegram Bot API client, update facts extraction, and the long-poll and
//! webhook ingress sources for tgin.

mod client;
mod facts;
mod longpoll;
mod secret;
mod webhook;

pub use client::{ApiError, BotClient, BotUser, SetWebhook, split_updates};
pub use facts::{FactsError, extract};
pub use longpoll::{LongPollOptions, LongPollSource, OnIdentity};
pub use secret::webhook_secret;
pub use webhook::{
    DEFAULT_WEBHOOK_BODY_LIMIT, IpAllowlist, IpNet, IpNetError, WebhookOptions, register,
    webhook_router,
};
