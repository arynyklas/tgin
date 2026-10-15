//! Telegram Bot API client and update facts extraction for tgin.

mod client;
mod facts;
mod secret;

pub use client::{ApiError, BotClient, BotUser, SetWebhook, split_updates};
pub use facts::{FactsError, extract};
pub use secret::webhook_secret;
