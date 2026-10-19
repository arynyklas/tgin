//! Envelope v1: one account update as the JSON object pull and push
//! consumers receive.
//!
//! ```json
//! {"update_id":1,"account":"main","layer":230,"constructor":"updateNewMessage",
//!  "received_at":"2026-10-19T12:00:00.000Z","tl":"<base64>","users":"<base64>",
//!  "chats":"<base64>"}
//! ```
//!
//! `tl` is the update's own TL bytes, `users` and `chats` the raw
//! `Vector<User>` and `Vector<Chat>` of the push or difference it came
//! with, all in standard base64 with padding. [`rendered`] adds
//! `"update"`, the update as TL-JSON. Both are written straight into one
//! buffer sized up front: one allocation for [`raw`], and the TL-JSON text
//! besides for [`rendered`].

use std::io::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;

use crate::schema::TlError;
use crate::tl_json;
use crate::updates::UpdateBytes;

/// The fields of an envelope that are not the update's bytes.
#[derive(Clone, Copy, Debug)]
pub struct Meta<'a> {
    pub account: &'a str,
    pub layer: i32,
    /// Strictly increasing per account, not contiguous.
    pub update_id: i64,
    /// When tgin admitted the update; written as RFC 3339 UTC with
    /// milliseconds.
    pub received_at: SystemTime,
}

/// The envelope of `update`, with the batch's raw `users` and `chats`.
#[must_use]
pub fn raw(meta: &Meta<'_>, update: &UpdateBytes, users: &[u8], chats: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(size(meta, update, users, chats, 0));
    write_fields(&mut out, meta, update, users, chats);
    out.push(b'}');
    Bytes::from(out)
}

/// The envelope of `update` with `"update"`, its TL-JSON, as a consumer
/// with `render: tl_json` receives it.
///
/// # Errors
///
/// The TL-JSON codec refused the update's bytes.
pub fn rendered(
    meta: &Meta<'_>,
    update: &UpdateBytes,
    users: &[u8],
    chats: &[u8],
) -> Result<Bytes, TlError> {
    let mut json = String::new();
    tl_json::to_json(update.bytes(), &mut json)?;
    let extra = UPDATE_KEY.len() + json.len();
    let mut out = Vec::with_capacity(size(meta, update, users, chats, extra));
    write_fields(&mut out, meta, update, users, chats);
    out.extend_from_slice(UPDATE_KEY);
    out.extend_from_slice(json.as_bytes());
    out.push(b'}');
    Ok(Bytes::from(out))
}

const UPDATE_KEY: &[u8] = b",\"update\":";

/// The envelope's length, `extra` bytes before the closing brace included;
/// an upper bound for the numbers (the capacity is a hint, so a year past
/// 999 999 only costs a reallocation).
fn size(meta: &Meta<'_>, update: &UpdateBytes, users: &[u8], chats: &[u8], extra: usize) -> usize {
    const FIXED: usize = br#"{"update_id":,"account":"","layer":,"constructor":"","received_at":"","tl":"","users":"","chats":""}"#.len();
    // i64 and i32 in decimal, and `YYYY…-MM-DDTHH:MM:SS.mmmZ` with up to
    // a 6-digit year.
    const NUMBERS: usize = 20 + 11 + 26;
    let b64 = |n: usize| n.div_ceil(3) * 4;
    FIXED
        + NUMBERS
        + escaped_len(meta.account)
        + escaped_len(update.facts().constructor)
        + b64(update.bytes().len())
        + b64(users.len())
        + b64(chats.len())
        + extra
}

fn write_fields(
    out: &mut Vec<u8>,
    meta: &Meta<'_>,
    update: &UpdateBytes,
    users: &[u8],
    chats: &[u8],
) {
    // Writing to a `Vec` cannot fail.
    let _ = write!(out, "{{\"update_id\":{},\"account\":\"", meta.update_id);
    escape_into(out, meta.account);
    let _ = write!(out, "\",\"layer\":{},\"constructor\":\"", meta.layer);
    escape_into(out, update.facts().constructor);
    out.extend_from_slice(b"\",\"received_at\":\"");
    timestamp_into(out, meta.received_at);
    out.extend_from_slice(b"\",\"tl\":\"");
    base64_into(out, update.bytes());
    out.extend_from_slice(b"\",\"users\":\"");
    base64_into(out, users);
    out.extend_from_slice(b"\",\"chats\":\"");
    base64_into(out, chats);
    out.push(b'"');
}

fn base64_into(out: &mut Vec<u8>, bytes: &[u8]) {
    let start = out.len();
    out.resize(start + bytes.len().div_ceil(3) * 4, 0);
    let written = STANDARD
        .encode_slice(bytes, &mut out[start..])
        .unwrap_or_default();
    out.truncate(start + written);
}

/// Bytes `s` takes as JSON string content.
fn escaped_len(s: &str) -> usize {
    s.bytes()
        .map(|b| match b {
            b'"' | b'\\' => 2,
            0..0x20 => 6,
            _ => 1,
        })
        .sum()
}

/// `s` as JSON string content.
fn escape_into(out: &mut Vec<u8>, s: &str) {
    for b in s.bytes() {
        match b {
            b'"' | b'\\' => out.extend_from_slice(&[b'\\', b]),
            0..0x20 => {
                let _ = write!(out, "\\u{b:04x}");
            }
            _ => out.push(b),
        }
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`, UTC; a time before 1970 is written as the
/// epoch.
fn timestamp_into(out: &mut Vec<u8>, at: SystemTime) {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (year, month, day) = civil(days);
    let _ = write!(
        out,
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since.subsec_millis()
    );
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Value;

    use super::*;
    use crate::tl_json::from_json;

    fn tl(json: &str) -> Vec<u8> {
        let mut out = Vec::new();
        from_json(json, &mut out).expect("valid TL-JSON");
        out
    }

    /// A boxed `Vector` of the objects.
    fn vector(items: &[&str]) -> Vec<u8> {
        let mut out = 0x1cb5_c415_u32.to_le_bytes().to_vec();
        out.extend_from_slice(&u32::try_from(items.len()).expect("few").to_le_bytes());
        for item in items {
            out.extend_from_slice(&tl(item));
        }
        out
    }

    fn update() -> UpdateBytes {
        UpdateBytes::new(Bytes::from(tl(
            r#"{"_":"updateNewMessage","pts":10,"pts_count":1,"message":{"_":"message","id":5,
            "peer_id":{"_":"peerUser","user_id":"3"},"date":0,"message":"hi \"there\""}}"#,
        )))
        .expect("an update")
    }

    fn meta(update_id: i64) -> Meta<'static> {
        Meta {
            account: "main",
            layer: 230,
            update_id,
            // 2026-10-19T12:34:56.789Z
            received_at: UNIX_EPOCH + Duration::from_millis(1_792_413_296_789),
        }
    }

    fn b64(v: &Value) -> Vec<u8> {
        STANDARD
            .decode(v.as_str().expect("a string"))
            .expect("standard base64")
    }

    #[test]
    fn envelope_fields_and_base64() {
        let u = update();
        let users = vector(&[r#"{"_":"userEmpty","id":"3"}"#]);
        let chats = vector(&[]);
        let bytes = raw(&meta(42), &u, &users, &chats);
        let text = std::str::from_utf8(&bytes).expect("UTF-8");
        let v: Value = serde_json::from_str(text).expect("one JSON object");
        let keys: Vec<&str> = v
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected = [
            "update_id",
            "account",
            "layer",
            "constructor",
            "received_at",
            "tl",
            "users",
            "chats",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected, "exactly the v1 fields, no `update`");
        assert!(text.starts_with(r#"{"update_id":42,"account":"main","layer":230,"constructor":"updateNewMessage","received_at":"2026-10-19T12:34:56.789Z","tl":""#), "{text}");
        assert_eq!(b64(&v["tl"]), u.bytes().to_vec());
        assert_eq!(b64(&v["users"]), users);
        assert_eq!(b64(&v["chats"]), chats);
        assert_eq!(
            bytes.len(),
            size(&meta(42), &u, &users, &chats, 0) - (20 + 11 + 26) + 2 + 3 + 24,
            "sized exactly but for the numbers"
        );
    }

    #[test]
    fn rendered_adds_the_update_as_tl_json() {
        let u = update();
        let empty = vector(&[]);
        let bytes = rendered(&meta(7), &u, &empty, &empty).expect("renders");
        let v: Value = serde_json::from_slice(&bytes).expect("one JSON object");
        let mut json = String::new();
        tl_json::to_json(u.bytes(), &mut json).expect("renders");
        let expected: Value = serde_json::from_str(&json).expect("TL-JSON");
        assert_eq!(v["update"], expected);
        assert_eq!(v["update"]["message"]["message"], "hi \"there\"");
        assert_eq!(b64(&v["tl"]), u.bytes().to_vec(), "the raw bytes stay");
        let plain = raw(&meta(7), &u, &empty, &empty);
        assert_eq!(&bytes[..plain.len() - 1], &plain[..plain.len() - 1]);
    }

    #[test]
    fn names_are_escaped() {
        let u = update();
        let empty = vector(&[]);
        let m = Meta {
            account: "a\"b\\c\u{1}",
            ..meta(1)
        };
        let v: Value = serde_json::from_slice(&raw(&m, &u, &empty, &empty)).expect("valid JSON");
        assert_eq!(v["account"], "a\"b\\c\u{1}");
    }

    #[test]
    fn timestamps() {
        let at = |ms: u64| {
            let mut out = Vec::new();
            timestamp_into(&mut out, UNIX_EPOCH + Duration::from_millis(ms));
            String::from_utf8(out).expect("ASCII")
        };
        assert_eq!(at(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(951_782_400_001), "2000-02-29T00:00:00.001Z");
        assert_eq!(at(1_792_413_296_789), "2026-10-19T12:34:56.789Z");
        assert_eq!(at(253_402_300_799_999), "9999-12-31T23:59:59.999Z");
    }
}
