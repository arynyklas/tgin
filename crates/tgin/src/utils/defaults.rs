use regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;

const TELEGRAM_TOKEN_REGEX: &str = r"(\d{8,15}):([a-zA-Z0-9_-]{30,50})";

/// Compiled once on first use; [`redact`] is its only user.
static TELEGRAM_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(TELEGRAM_TOKEN_REGEX).unwrap());

/// `s` with every Telegram bot token replaced by `#####`; borrowed unchanged
/// when it carries none. Use it wherever a configured URL or path is logged,
/// printed or returned: webhook paths and long-poll route paths may embed a
/// token too.
pub fn redact(s: &str) -> Cow<'_, str> {
    TELEGRAM_TOKEN_RE.replace_all(s, "#####")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact() {
        assert!(matches!(redact("/bot/getUpdates"), Cow::Borrowed(_)));
        assert_eq!(
            redact("/bot123456789:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA/hook"),
            "/bot#####/hook"
        );
    }
}
