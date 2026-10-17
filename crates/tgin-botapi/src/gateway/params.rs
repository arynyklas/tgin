//! Request parameters from a complete request body, read as the Bot API
//! reads them (tdlib's `HttpReader`): `multipart/form-data`, then
//! `application/x-www-form-urlencoded`, then `application/json`, matched as
//! substrings of the lowercased `Content-Type`; any other body has none. The
//! query string is the caller's.

use std::borrow::Cow;
use std::convert::Infallible;
use std::fmt;

use axum::http::HeaderValue;
use bytes::Bytes;
use futures_util::FutureExt as _;
use percent_encoding::percent_decode;
use serde::de::{self, DeserializeSeed, Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::value::RawValue;

/// The most bytes of a multipart text field [`text_param`] reads.
pub const MULTIPART_FIELD_LIMIT: usize = 64 * 1024;

/// The top-level parameter `name` of `body`, as text:
/// - JSON: a string as is, a number or a boolean as written; `null`, objects
///   and arrays are not text;
/// - urlencoded: the decoded value, which must be UTF-8;
/// - multipart: a text field of at most [`MULTIPART_FIELD_LIMIT`] bytes, in
///   any order; file parts are skipped.
///
/// The first occurrence wins. `None` when the body has no such parameter or
/// cannot be read.
pub fn text_param<'a>(
    content_type: Option<&HeaderValue>,
    body: &'a [u8],
    name: &str,
) -> Option<Cow<'a, str>> {
    match encoding(content_type?)? {
        Encoding::Multipart => {
            let content_type = content_type?.to_str().ok()?;
            multipart_param(content_type, body, name).map(Cow::Owned)
        }
        Encoding::Json => json_param(body, name),
        Encoding::Form => form_param(body, name),
    }
}

/// How a request body carries its parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    Multipart,
    Json,
    Form,
}

/// The encoding `content_type` names, if it is one the Bot API reads.
pub(crate) fn encoding(content_type: &HeaderValue) -> Option<Encoding> {
    let content_type = content_type.as_bytes();
    [
        (&b"multipart/form-data"[..], Encoding::Multipart),
        (b"application/x-www-form-urlencoded", Encoding::Form),
        (b"application/json", Encoding::Json),
    ]
    .into_iter()
    .find(|(name, _)| {
        content_type
            .windows(name.len())
            .any(|window| window.eq_ignore_ascii_case(name))
    })
    .map(|(_, encoding)| encoding)
}

/// The value of the first pair named `name` of an urlencoded body: pairs
/// split on `&`, name and value on the first `=`. `None` when that value is
/// not UTF-8 once decoded; a pair whose name is not matches nothing.
pub(crate) fn form_param<'a>(body: &'a [u8], name: &str) -> Option<Cow<'a, str>> {
    let value = body
        .split(|&b| b == b'&')
        .filter(|pair| !pair.is_empty())
        .find_map(|pair| {
            let mut parts = pair.splitn(2, |&b| b == b'=');
            let key = parts.next().unwrap_or_default();
            (form_decode(key).as_deref() == Some(name)).then(|| parts.next().unwrap_or_default())
        })?;
    form_decode(value)
}

/// `+` as a space, then percent-decoded; `None` unless UTF-8. Borrowed
/// unless something was decoded.
fn form_decode(encoded: &[u8]) -> Option<Cow<'_, str>> {
    if !encoded.contains(&b'+') {
        return percent_decode(encoded).decode_utf8().ok();
    }
    let spaced: Vec<u8> = encoded
        .iter()
        .map(|&b| if b == b'+' { b' ' } else { b })
        .collect();
    let decoded = percent_decode(&spaced).decode_utf8().ok()?;
    Some(Cow::Owned(decoded.into_owned()))
}

/// The top-level member `name` of a JSON object, as text.
fn json_param<'a>(body: &'a [u8], name: &str) -> Option<Cow<'a, str>> {
    let mut de = serde_json::Deserializer::from_slice(body);
    let value = de.deserialize_map(FindMember { name }).ok()?;
    de.end().ok()?;
    let value = value?.get();
    match value.as_bytes().first()? {
        b'"' => serde_json::Deserializer::from_str(value)
            .deserialize_str(Text)
            .ok(),
        b'-' | b'0'..=b'9' | b't' | b'f' => Some(Cow::Borrowed(value)),
        _ => None,
    }
}

/// The value of an object's first member named `name`, unparsed.
struct FindMember<'n> {
    name: &'n str,
}

impl<'de> Visitor<'de> for FindMember<'_> {
    type Value = Option<&'de RawValue>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut found = None;
        while let Some(named) = map.next_key_seed(IsKey(self.name))? {
            if named && found.is_none() {
                found = Some(map.next_value()?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(found)
    }
}

/// Whether an object key is the given name, without allocating unless it
/// has escapes.
struct IsKey<'n>(&'n str);

impl<'de> DeserializeSeed<'de> for IsKey<'_> {
    type Value = bool;

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<bool, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl Visitor<'_> for IsKey<'_> {
    type Value = bool;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object key")
    }

    fn visit_str<E: de::Error>(self, key: &str) -> Result<bool, E> {
        Ok(key == self.0)
    }
}

/// A JSON string, borrowed unless it has escapes.
struct Text;

impl<'de> Visitor<'de> for Text {
    type Value = Cow<'de, str>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string")
    }

    fn visit_borrowed_str<E: de::Error>(self, s: &'de str) -> Result<Self::Value, E> {
        Ok(Cow::Borrowed(s))
    }

    fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
        Ok(Cow::Owned(s.to_owned()))
    }

    fn visit_string<E: de::Error>(self, s: String) -> Result<Self::Value, E> {
        Ok(Cow::Owned(s))
    }
}

/// Bytes of the body multer is fed at a time.
const MULTIPART_CHUNK: usize = 64 * 1024;

/// The first text field `name` of a multipart body. multer is
/// asynchronous, but over a body already in memory every step is ready at
/// once, so its future is polled to completion in one go.
fn multipart_param(content_type: &str, body: &[u8], name: &str) -> Option<String> {
    let boundary = multer::parse_boundary(content_type).ok()?;
    let chunks = body
        .chunks(MULTIPART_CHUNK)
        .map(|chunk| Ok::<_, Infallible>(Bytes::copy_from_slice(chunk)));
    let mut multipart = multer::Multipart::new(futures_util::stream::iter(chunks), boundary);
    let read = async move {
        while let Some(mut field) = multipart.next_field().await.ok()? {
            if field.file_name().is_some() || field.name() != Some(name) {
                continue;
            }
            let mut text = Vec::new();
            while let Some(chunk) = field.chunk().await.ok()? {
                if text.len() + chunk.len() > MULTIPART_FIELD_LIMIT {
                    return None;
                }
                text.extend_from_slice(&chunk);
            }
            return String::from_utf8(text).ok();
        }
        None
    };
    read.now_or_never().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = "application/json";
    const FORM: &str = "application/x-www-form-urlencoded";
    const BOUNDARY: &str = "XyZ-boundary";

    fn get<'a>(content_type: &str, body: &'a [u8], name: &str) -> Option<Cow<'a, str>> {
        text_param(
            Some(&HeaderValue::from_str(content_type).unwrap()),
            body,
            name,
        )
    }

    fn multipart_type() -> String {
        format!("multipart/form-data; boundary={BOUNDARY}")
    }

    /// A multipart body of `(name, file name, content)` parts.
    fn multipart(parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, file_name, content) in parts {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            let disposition = match file_name {
                Some(file) => format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{file}\"\r\nContent-Type: application/octet-stream\r\n"
                ),
                None => format!("Content-Disposition: form-data; name=\"{name}\"\r\n"),
            };
            body.extend_from_slice(disposition.as_bytes());
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(content);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    #[test]
    fn text_param_forms() {
        // JSON: strings (escaped or not), numbers and booleans.
        let body = br#"{"chat_id":-100123,"url":"https:\/\/a.example\/hook","secret_token":"s3","drop_pending_updates":true,"ratio":1.5e3,"off":false}"#;
        assert_eq!(get(JSON, body, "chat_id").as_deref(), Some("-100123"));
        assert_eq!(
            get(JSON, body, "url").as_deref(),
            Some("https://a.example/hook")
        );
        assert!(matches!(
            get(JSON, body, "secret_token"),
            Some(Cow::Borrowed("s3"))
        ));
        assert_eq!(
            get(JSON, body, "drop_pending_updates").as_deref(),
            Some("true")
        );
        assert_eq!(get(JSON, body, "off").as_deref(), Some("false"));
        assert_eq!(get(JSON, body, "ratio").as_deref(), Some("1.5e3"));
        assert_eq!(
            get("Application/JSON; charset=utf-8", body, "chat_id").as_deref(),
            Some("-100123")
        );
        // Whitespace around values, an escaped key, the first duplicate.
        let spaced = b" { \"a\" : [1, {\"b\": 2}] , \"ch\\u0061t_id\" : \"@x\" , \"chat_id\": 9 } ";
        assert_eq!(get(JSON, spaced, "chat_id").as_deref(), Some("@x"));

        // urlencoded.
        let form = b"url=https%3A%2F%2Fa.example%2Fhook&chat_id=%40chan&drop_pending_updates=true&text=a+b&chat_id=2";
        assert_eq!(
            get(FORM, form, "url").as_deref(),
            Some("https://a.example/hook")
        );
        assert_eq!(get(FORM, form, "chat_id").as_deref(), Some("@chan"));
        assert!(matches!(
            get(FORM, form, "drop_pending_updates"),
            Some(Cow::Borrowed("true"))
        ));
        assert_eq!(get(FORM, form, "text").as_deref(), Some("a b"));

        // multipart: the field after a file part; a file part with the
        // field's name is skipped.
        let body = multipart(&[
            (
                "certificate",
                Some("cert.pem"),
                b"url=evil\r\n--not-a-boundary",
            ),
            ("url", Some("url.txt"), b"https://file.example"),
            ("url", None, b"https://a.example/hook"),
            ("secret_token", None, "s\u{e9}cret".as_bytes()),
        ]);
        let ct = multipart_type();
        assert_eq!(
            get(&ct, &body, "url").as_deref(),
            Some("https://a.example/hook")
        );
        assert_eq!(
            get(&ct, &body, "secret_token").as_deref(),
            Some("s\u{e9}cret")
        );
        assert_eq!(get(&ct, &body, "certificate"), None);
        // The field cap.
        let at_cap = "a".repeat(MULTIPART_FIELD_LIMIT);
        let over_cap = "a".repeat(MULTIPART_FIELD_LIMIT + 1);
        let body = multipart(&[
            ("big", None, at_cap.as_bytes()),
            ("bigger", None, over_cap.as_bytes()),
        ]);
        assert_eq!(get(&ct, &body, "big").as_deref(), Some(at_cap.as_str()));
        assert_eq!(get(&ct, &body, "bigger"), None);

        // Missing.
        assert_eq!(get(JSON, br#"{"text":"a"}"#, "chat_id"), None);
        assert_eq!(get(FORM, b"text=a", "chat_id"), None);
        assert_eq!(get(FORM, b"", "chat_id"), None);
        assert_eq!(
            get(&ct, &multipart(&[("text", None, b"a")]), "chat_id"),
            None
        );
        assert_eq!(text_param(None, br#"{"chat_id":1}"#, "chat_id"), None);
        assert_eq!(get("text/plain", br#"{"chat_id":1}"#, "chat_id"), None);

        // Nested and non-text values are ignored.
        for body in [
            &br#"{"reply_parameters":{"chat_id":5},"text":"a"}"#[..],
            br#"{"x":[{"chat_id":5}]}"#,
            br#"{"chat_id":null}"#,
            br#"{"chat_id":[1]}"#,
            br#"{"chat_id":{"id":1}}"#,
        ] {
            assert_eq!(
                get(JSON, body, "chat_id"),
                None,
                "{}",
                String::from_utf8_lossy(body)
            );
        }

        // Unreadable bodies.
        for body in [
            &b"[1]"[..],
            b"\"chat_id\"",
            br#"{"chat_id":1"#,
            br#"{"chat_id":1} x"#,
            b"",
        ] {
            assert_eq!(get(JSON, body, "chat_id"), None);
        }
        assert_eq!(get("multipart/form-data", &body, "big"), None);
        assert_eq!(get(&ct, b"--XyZ-boundary\r\ngarbage", "big"), None);
    }

    /// A `Content-Type` naming both is a form, as the Bot API server reads it
    /// (tdlib `HttpReader.cpp:217-221`).
    #[test]
    fn text_param_form_wins_over_json() {
        let ct = "application/x-www-form-urlencoded; profile=application/json";
        assert_eq!(get(ct, b"chat_id=5", "chat_id").as_deref(), Some("5"));
        assert_eq!(get(ct, br#"{"chat_id":5}"#, "chat_id"), None);
        let ct = "application/json; profile=application/x-www-form-urlencoded";
        assert_eq!(get(ct, b"chat_id=5", "chat_id").as_deref(), Some("5"));
    }

    /// Form values are UTF-8 like every other parameter; one that is not is
    /// absent.
    #[test]
    fn text_param_form_utf8() {
        assert_eq!(get(FORM, b"secret_token=%FF", "secret_token"), None);
        assert_eq!(get(FORM, b"secret_token=\xFF", "secret_token"), None);
        assert_eq!(get(FORM, b"secret_token=a%E2%82", "secret_token"), None);
        // The first occurrence wins, even when it cannot be read.
        assert_eq!(get(FORM, b"text=%FF&text=b", "text"), None);
        assert_eq!(
            get(FORM, b"text=%E2%82%AC", "text").as_deref(),
            Some("\u{20ac}")
        );
        assert_eq!(
            get(FORM, "text=\u{20ac}".as_bytes(), "text").as_deref(),
            Some("\u{20ac}")
        );
        assert_eq!(get(FORM, b"text=a+b%2B", "text").as_deref(), Some("a b+"));
        // Keys are decoded too; a key that is not UTF-8 matches nothing.
        assert_eq!(get(FORM, b"chat%5Fid=5", "chat_id").as_deref(), Some("5"));
        assert_eq!(get(FORM, b"%FF=1&a=2", "a").as_deref(), Some("2"));
        // No value, empty pairs.
        assert_eq!(get(FORM, b"&&flag&a=", "flag").as_deref(), Some(""));
        assert_eq!(get(FORM, b"&&flag&a=", "a").as_deref(), Some(""));
        assert_eq!(get(FORM, b"a=b=c", "a").as_deref(), Some("b=c"));
        // Borrowed unless something was decoded.
        assert!(matches!(
            get(FORM, b"x=1&url=https://a.example/hook", "url"),
            Some(Cow::Borrowed("https://a.example/hook"))
        ));
        assert!(matches!(
            get(FORM, b"text=a+b", "text"),
            Some(Cow::Owned(_))
        ));
    }
}
