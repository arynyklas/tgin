use once_cell::sync::Lazy;
use regex::Regex;

pub const TELEGRAM_TOKEN_REGEX: &str = r"(\d{8,15}):([a-zA-Z0-9_-]{30,50})";

/// Compiled once and shared by every token-redaction call site. Compiling this
/// static pattern per `LongPollUpdate` / `RegistrationWebhookConfig`
/// construction was pure waste.
pub static TELEGRAM_TOKEN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(TELEGRAM_TOKEN_REGEX).unwrap());
