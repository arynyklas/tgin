//! Telegram Bot API client and update facts extraction for tgin.

mod client;
mod facts;
mod longpoll;
mod secret;

pub use client::{ApiError, BotClient, BotUser, SetWebhook, split_updates};
pub use facts::{FactsError, extract};
pub use longpoll::{LongPollOptions, LongPollSource, OnIdentity};
pub use secret::webhook_secret;
