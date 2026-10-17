//! Gateway request paths: the Bot API's `/bot<token>/<method>` and
//! `/file/bot<token>/<file_path>`, each optionally under `/c/<consumer>`.

use std::borrow::Cow;
use std::fmt;

use percent_encoding::percent_decode_str;
use tgin_core::config::{gateway_token, valid_name};

/// A request path the gateway serves. `Debug` hides the token.
#[derive(Clone, PartialEq, Eq)]
pub enum GatewayPath<'a> {
    /// `[/c/<consumer>]/bot<token>/<method>`.
    Method {
        consumer: Option<&'a str>,
        token: Cow<'a, str>,
        method: &'a str,
    },
    /// `[/c/<consumer>]/file/bot<token>/<file_path>`.
    File {
        consumer: Option<&'a str>,
        token: Cow<'a, str>,
        file_path: &'a str,
    },
}

impl fmt::Debug for GatewayPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Method {
                consumer, method, ..
            } => f
                .debug_struct("Method")
                .field("consumer", consumer)
                .field("token", &"[redacted]")
                .field("method", method)
                .finish(),
            Self::File {
                consumer,
                file_path,
                ..
            } => f
                .debug_struct("File")
                .field("consumer", consumer)
                .field("token", &"[redacted]")
                .field("file_path", file_path)
                .finish(),
        }
    }
}

/// `path` (a request URI's path, still percent-encoded) as a gateway path,
/// or `None` when it is none:
/// - `<token>` is percent-decoded once, then must pass
///   [`gateway_token`];
/// - `<method>` is `[A-Za-z][A-Za-z0-9_]{0,63}`, the last segment;
/// - `<file_path>` is the non-empty rest, with no `.` or `..` segment (also
///   percent-encoded, as URL parsers decode them) and no `\`;
/// - `<consumer>` is a valid name ([`valid_name`]).
pub fn parse(path: &str) -> Option<GatewayPath<'_>> {
    let rest = path.strip_prefix('/')?;
    let (consumer, rest) = match rest.strip_prefix("c/") {
        Some(rest) => {
            let (name, rest) = rest.split_once('/')?;
            (Some(valid_name(name).then_some(name)?), rest)
        }
        None => (None, rest),
    };
    match rest.strip_prefix("file/") {
        Some(rest) => {
            let (token, file_path) = rest.split_once('/')?;
            let token = token_segment(token)?;
            valid_file_path(file_path).then_some(GatewayPath::File {
                consumer,
                token,
                file_path,
            })
        }
        None => {
            let (token, method) = rest.split_once('/')?;
            let token = token_segment(token)?;
            valid_method(method).then_some(GatewayPath::Method {
                consumer,
                token,
                method,
            })
        }
    }
}

/// The token of a `bot<token>` segment.
fn token_segment(segment: &str) -> Option<Cow<'_, str>> {
    let encoded = segment.strip_prefix("bot")?;
    let token = percent_decode_str(encoded).decode_utf8().ok()?;
    gateway_token(&token).then_some(token)
}

/// `[A-Za-z][A-Za-z0-9_]{0,63}`.
fn valid_method(method: &str) -> bool {
    let bytes = method.as_bytes();
    bytes.len() <= 64
        && bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

fn valid_file_path(file_path: &str) -> bool {
    !file_path.is_empty()
        && !file_path.contains('\\')
        && file_path.split('/').all(|segment| !is_dot_segment(segment))
}

/// `.` or `..`, with any dot written as `%2e` or `%2E`.
fn is_dot_segment(segment: &str) -> bool {
    let dot = |s: &str| s == "." || s.eq_ignore_ascii_case("%2e");
    dot(segment)
        || [1, 3].into_iter().any(|split| {
            segment
                .split_at_checked(split)
                .is_some_and(|(a, b)| dot(a) && dot(b))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method<'a>(consumer: Option<&'a str>, token: &'a str, method: &'a str) -> GatewayPath<'a> {
        GatewayPath::Method {
            consumer,
            token: Cow::Borrowed(token),
            method,
        }
    }

    fn file<'a>(consumer: Option<&'a str>, token: &'a str, file_path: &'a str) -> GatewayPath<'a> {
        GatewayPath::File {
            consumer,
            token: Cow::Borrowed(token),
            file_path,
        }
    }

    #[test]
    fn path_table() {
        let accepted = [
            (
                "/bot123:AA-bb_CC/sendMessage",
                method(None, "123:AA-bb_CC", "sendMessage"),
            ),
            (
                "/file/bot123:abc/documents/file_0.txt",
                file(None, "123:abc", "documents/file_0.txt"),
            ),
            (
                "/c/workers/bot123:abc/getUpdates",
                method(Some("workers"), "123:abc", "getUpdates"),
            ),
            (
                "/c/w-2/file/bot123:abc/photos/a.jpg",
                file(Some("w-2"), "123:abc", "photos/a.jpg"),
            ),
            ("/bot123%3Aabc/getMe", method(None, "123:abc", "getMe")),
            ("/bot123%3aabc/get_me", method(None, "123:abc", "get_me")),
            (
                "/c/workers/file/bot123%3Aabc/x",
                file(Some("workers"), "123:abc", "x"),
            ),
            // Dots inside a segment are not a traversal.
            ("/file/bot1:a/a..b/.c", file(None, "1:a", "a..b/.c")),
        ];
        for (path, expected) in accepted {
            assert_eq!(parse(path), Some(expected), "{path}");
        }
        let long_method = format!("/bot1:a/a{}", "b".repeat(63));
        assert!(parse(&long_method).is_some());

        let too_long_method = format!("/bot1:a/a{}", "b".repeat(64));
        let rejected = [
            // The token: no colon, double-encoded, invalid, empty parts.
            "/bot123abc/getMe",
            "/bot123%253Aabc/getMe",
            "/bot123%3Aab%2Fc/getMe",
            "/bot123%FF:abc/getMe",
            "/bot:abc/getMe",
            "/bot123:/getMe",
            "/bot/getMe",
            // A traversal in the file path, also percent-encoded.
            "/file/bot123:abc/../secret",
            "/file/bot123:abc/a/../../x",
            "/file/bot123:abc/./x",
            "/file/bot123:abc/a/..",
            "/file/bot123:abc/%2e%2E/x",
            "/file/bot123:abc/.%2e/x",
            "/file/bot123:abc/a\\..\\x",
            "/file/bot123:abc/",
            "/file/bot123:abc",
            // The method: empty, a trailing segment, bad characters.
            "/bot123:abc/",
            "/bot123:abc",
            "/bot123:abc/sendMessage/",
            "/bot123:abc/sendMessage/extra",
            "/bot123:abc/send-message",
            "/bot123:abc/1send",
            "/bot123:abc/_send",
            "/bot123:abc/send%4Dessage",
            &too_long_method,
            // A segment other than `bot<token>`.
            "/123:abc/getMe",
            "/Bot123:abc/getMe",
            "/foo/bot123:abc/getMe",
            "/c/workers/123:abc/getMe",
            "/c/workers/files/bot123:abc/x",
            // The consumer name.
            "/c//bot123:abc/getMe",
            "/c/Workers/bot123:abc/getMe",
            "/c/-w/bot123:abc/getMe",
            "/c/w_x/bot123:abc/getMe",
            "/c/workers",
            // Not a path at all.
            "bot123:abc/getMe",
            "",
            "/",
        ];
        for path in rejected {
            assert_eq!(parse(path), None, "{path}");
        }
    }

    #[test]
    fn path_token_borrowed_unless_decoded() {
        let Some(GatewayPath::Method { token, .. }) = parse("/bot123:abc/getMe") else {
            panic!("a method path");
        };
        assert!(matches!(token, Cow::Borrowed("123:abc")));
        let Some(GatewayPath::Method { token, .. }) = parse("/bot123%3Aabc/getMe") else {
            panic!("a method path");
        };
        assert!(matches!(&token, Cow::Owned(t) if t == "123:abc"));
    }

    #[test]
    fn path_debug_hides_token() {
        for path in ["/c/w/bot123:secret/getMe", "/file/bot123:secret/a"] {
            let shown = format!("{:?}", parse(path).expect("a gateway path"));
            assert!(!shown.contains("secret"), "{shown}");
            assert!(shown.contains("[redacted]"), "{shown}");
        }
    }
}
