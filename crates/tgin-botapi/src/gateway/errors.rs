//! Bot API responses as Telegram writes them: `ok`, then `result` or
//! `error_code` and `description`, then `parameters` (telegram-bot-api's
//! `Query.cpp`, `JsonQueryOk` and `JsonQueryError`). The bytes are written by
//! hand: serde_json would sort the keys.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;

/// `{"ok":true,"result":<result_json>}`; `result_json` is written as is.
pub fn ok_raw(result_json: &[u8]) -> Response {
    let mut body = Vec::with_capacity(22 + result_json.len());
    body.extend_from_slice(br#"{"ok":true,"result":"#);
    body.extend_from_slice(result_json);
    body.push(b'}');
    json(StatusCode::OK, body)
}

/// `{"ok":true,"result":true}`, with `"description"` after `result` when
/// given, as Telegram answers `setWebhook`.
pub fn ok_true(description: Option<&str>) -> Response {
    let mut body = br#"{"ok":true,"result":true"#.to_vec();
    if let Some(description) = description {
        body.extend_from_slice(br#","description":"#);
        push_string(&mut body, description);
    }
    body.push(b'}');
    json(StatusCode::OK, body)
}

/// `{"ok":false,"error_code":<status>,"description":<description>}` with
/// that status.
pub fn error(status: StatusCode, description: &str) -> Response {
    let mut body = error_head(status, description);
    body.push(b'}');
    json(status, body)
}

/// Telegram's 429: `Too Many Requests: retry after <n>`, with
/// `parameters.retry_after` and a `Retry-After` header.
pub fn too_many_requests(retry_after_secs: u64) -> Response {
    let status = StatusCode::TOO_MANY_REQUESTS;
    let description = format!("Too Many Requests: retry after {retry_after_secs}");
    let mut body = error_head(status, &description);
    body.extend_from_slice(
        format!(r#","parameters":{{"retry_after":{retry_after_secs}}}}}"#).as_bytes(),
    );
    let mut response = json(status, body);
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs));
    response
}

/// `{"ok":false,"error_code":…,"description":…`, unclosed.
fn error_head(status: StatusCode, description: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(64 + description.len());
    body.extend_from_slice(br#"{"ok":false,"error_code":"#);
    body.extend_from_slice(status.as_str().as_bytes());
    body.extend_from_slice(br#","description":"#);
    push_string(&mut body, description);
    body
}

/// `s` as a JSON string.
fn push_string(out: &mut Vec<u8>, s: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(b'"');
    for &b in s.as_bytes() {
        match b {
            b'"' => out.extend_from_slice(br#"\""#),
            b'\\' => out.extend_from_slice(br"\\"),
            b'\n' => out.extend_from_slice(br"\n"),
            b'\r' => out.extend_from_slice(br"\r"),
            b'\t' => out.extend_from_slice(br"\t"),
            0..=0x1f => out.extend_from_slice(&[
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[usize::from(b >> 4)],
                HEX[usize::from(b & 0xf)],
            ]),
            _ => out.push(b),
        }
    }
    out.push(b'"');
}

fn json(status: StatusCode, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parts(response: Response) -> (StatusCode, Option<String>, Option<String>, String) {
        let header = |name| {
            response
                .headers()
                .get(name)
                .map(|v: &HeaderValue| v.to_str().unwrap().to_owned())
        };
        let content_type = header(header::CONTENT_TYPE);
        let retry_after = header(header::RETRY_AFTER);
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            content_type,
            retry_after,
            String::from_utf8(body.to_vec()).unwrap(),
        )
    }

    #[tokio::test]
    async fn error_bytes_exact() {
        let (status, content_type, retry_after, body) = parts(too_many_requests(3)).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(content_type.as_deref(), Some("application/json"));
        assert_eq!(retry_after.as_deref(), Some("3"));
        assert_eq!(
            body,
            r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 3","parameters":{"retry_after":3}}"#
        );

        let (status, content_type, retry_after, body) =
            parts(error(StatusCode::NOT_FOUND, "Not Found: method not found")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(content_type.as_deref(), Some("application/json"));
        assert_eq!(retry_after, None);
        assert_eq!(
            body,
            r#"{"ok":false,"error_code":404,"description":"Not Found: method not found"}"#
        );

        // The description is escaped as a JSON string.
        let (_, _, _, body) = parts(error(
            StatusCode::BAD_REQUEST,
            "Bad Request: \"x\\y\"\n\t\u{1}é",
        ))
        .await;
        assert_eq!(
            body,
            r#"{"ok":false,"error_code":400,"description":"Bad Request: \"x\\y\"\n\t\u0001é"}"#
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["description"], "Bad Request: \"x\\y\"\n\t\u{1}é");
    }

    #[tokio::test]
    async fn ok_bytes_exact() {
        let (status, content_type, _, body) =
            parts(ok_raw(br#"{"id":42,"is_bot":true,"first_name":"B"}"#)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("application/json"));
        assert_eq!(
            body,
            r#"{"ok":true,"result":{"id":42,"is_bot":true,"first_name":"B"}}"#
        );

        let (status, _, _, body) = parts(ok_true(None)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"ok":true,"result":true}"#);

        let (_, _, _, body) = parts(ok_true(Some("Webhook was set"))).await;
        assert_eq!(
            body,
            r#"{"ok":true,"result":true,"description":"Webhook was set"}"#
        );
    }
}
