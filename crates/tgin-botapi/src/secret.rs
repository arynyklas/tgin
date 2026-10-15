//! The webhook `secret_token` derived from the bot token.

use aws_lc_rs::hmac;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use tgin_runtime::secret::Secret;

/// The HMAC message; the version suffix lets the derivation change later.
const CONTEXT: &[u8] = b"tgin-webhook-secret-v1";

/// The webhook `secret_token` for a bot: base64url without padding of
/// HMAC-SHA256 keyed with the bot token over `tgin-webhook-secret-v1`.
///
/// It is stable across restarts and replicas and fits Telegram's
/// `[A-Za-z0-9_-]{1,256}` (43 characters). Treat it as a secret.
pub fn webhook_secret(token: &Secret<String>) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, token.expose().as_bytes());
    URL_SAFE_NO_PAD.encode(hmac::sign(&key, CONTEXT))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(token: &str) -> String {
        webhook_secret(&Secret::new(token.to_owned()))
    }

    #[test]
    fn webhook_secret_is_stable_and_charset_valid() {
        let token = "123456789:AAAAexampleSecretTokenValue_-xyz";
        // Python: base64.urlsafe_b64encode(hmac.new(token, b"tgin-webhook-secret-v1",
        // hashlib.sha256).digest()).rstrip(b"=")
        assert_eq!(secret(token), "FpuzOgwgHVWly-yZ3OQ7-bNau6WWM4NpDKlTlpUIE-E");
        assert_eq!(secret(token), secret(token));
        assert_ne!(
            secret(token),
            secret("123456789:AAAAexampleSecretTokenValue_-xyZ")
        );

        for token in [token, "1:a", "", "999:ÿ"] {
            let s = secret(token);
            assert!((1..=256).contains(&s.len()), "{s}");
            assert!(
                s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "{s}"
            );
            assert!(!s.contains(token) || token.is_empty());
        }
    }
}
