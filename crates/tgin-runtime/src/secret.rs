//! Secret values and log redaction.

use std::borrow::Cow;
use std::fmt;
use std::sync::LazyLock;

use aho_corasick::{AhoCorasick, MatchKind};
use regex::Regex;
use serde::{Serialize, Serializer};
use subtle::ConstantTimeEq;

/// Text that replaces every secret in output.
const REDACTED: &str = "[redacted]";
/// Text that [`Redactor`] substitutes for each match.
const MASK: &str = "#####";
/// Registered secrets shorter than this are not masked: they would match
/// ordinary words.
const MIN_SECRET_LEN: usize = 8;

static TELEGRAM_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{8,15}):([a-zA-Z0-9_-]{30,50})").expect("token regex is valid")
});

/// A value that never shows up in `Debug`, `Display` or serialized output.
#[derive(Clone)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(v: T) -> Self {
        Self(v)
    }

    /// The wrapped value; callers own keeping it out of logs.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl<T> Serialize for Secret<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(REDACTED)
    }
}

impl PartialEq for Secret<String> {
    fn eq(&self, other: &Self) -> bool {
        secret_eq(self.0.as_bytes(), other.0.as_bytes())
    }
}

impl Eq for Secret<String> {}

/// Constant-time equality: the running time depends only on `expected`'s
/// length, never on where the inputs differ.
pub fn secret_eq(provided: &[u8], expected: &[u8]) -> bool {
    let len_eq = provided.len() == expected.len();
    let mut padded = vec![0u8; expected.len()];
    let copy_len = provided.len().min(expected.len());
    padded[..copy_len].copy_from_slice(&provided[..copy_len]);
    let bytes_eq: bool = padded.ct_eq(expected).into();
    len_eq && bytes_eq
}

/// Masks registered secrets and Telegram bot tokens in text bound for logs.
pub struct Redactor {
    secrets: Option<AhoCorasick>,
}

impl Redactor {
    /// Secrets shorter than 8 bytes are ignored.
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let patterns: Vec<String> = secrets
            .into_iter()
            .filter(|s| s.len() >= MIN_SECRET_LEN)
            .collect();
        let secrets = (!patterns.is_empty()).then(|| {
            AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostLongest)
                .build(&patterns)
                .expect("secret patterns fit the automaton size limits")
        });
        Self { secrets }
    }

    /// `s` with every match replaced by `#####`; borrowed when nothing matched.
    pub fn redact<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let masked = match &self.secrets {
            Some(ac) if ac.is_match(s) => {
                let mut out = String::with_capacity(s.len());
                ac.replace_all_with(s, &mut out, |_, _, dst| {
                    dst.push_str(MASK);
                    true
                });
                Cow::Owned(out)
            }
            _ => Cow::Borrowed(s),
        };
        match masked {
            Cow::Borrowed(b) => TELEGRAM_TOKEN_RE.replace_all(b, MASK),
            Cow::Owned(o) => Cow::Owned(TELEGRAM_TOKEN_RE.replace_all(&o, MASK).into_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_display_serialize_redact() {
        let s = Secret::new("hunter2-secret".to_string());
        assert_eq!(format!("{s:?}"), "[redacted]");
        assert_eq!(format!("{s}"), "[redacted]");
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"[redacted]\"");
        assert_eq!(s.expose(), "hunter2-secret");
        assert_eq!(s.clone(), s);
        assert_ne!(s, Secret::new("hunter2-secreT".to_string()));
    }

    #[test]
    fn secret_eq_matches_only_exact() {
        assert!(secret_eq(b"hunter2", b"hunter2"));
        assert!(!secret_eq(b"hunter2", b"hunter3"));
        assert!(!secret_eq(b"hunter", b"hunter2"));
        assert!(!secret_eq(b"hunter234", b"hunter2"));
        assert!(!secret_eq(b"", b"hunter2"));
    }

    #[test]
    fn redactor_masks_registered_secret_and_token_pattern() {
        let r = Redactor::new(["s3cr3t-value-123".to_string()]);
        let input = "x 123456789:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA y s3cr3t-value-123";
        assert_eq!(r.redact(input), "x ##### y #####");
    }

    #[test]
    fn redactor_ignores_short_secrets() {
        let r = Redactor::new(["short".to_string()]);
        let out = r.redact("a short line");
        assert_eq!(out, "a short line");
        assert!(matches!(out, Cow::Borrowed(_)));
    }
}
