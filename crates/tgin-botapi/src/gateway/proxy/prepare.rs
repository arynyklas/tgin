//! Request bodies on their way upstream: read whole when small, for the
//! rate limits' chat and a resend after a 429, or scanned for local file
//! references, read whole or streamed through a [`ScanGuard`].

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue};
use http_body::Body as HttpBody;
use reqwest::Url;
use tokio::sync::oneshot;

use super::super::chat_key::{ChatKey, chat_key};
use super::super::local_files::{
    MAX_PARAMETERS, Refusal, ScanGuard, Verdict, boundary, form_has_local_file, json_has_local_file,
};
use super::super::params::{Encoding, encoding};
use super::super::{Bounded, read_bounded};
use super::Failed;
use super::transfer::{Bounds, Progress, Upload, Waited, await_headers};

/// The longest request body of a limited method read for its chat and
/// kept to be sent again after a 429.
const BUFFER_LIMIT: usize = 64 * 1024;

/// A request body as it goes upstream.
pub(super) enum Payload {
    /// Read whole: it can be sent again.
    Buffered(Bytes),
    /// Streamed: it goes once.
    Streamed(Body),
}

/// The body of a request to buffer, and, when the limits apply (`limited`),
/// the chat it targets:
/// - a JSON or urlencoded body (or an empty one) is read up to
///   [`BUFFER_LIMIT`], waiting at most `idle` for each frame; whole, it is
///   buffered;
/// - longer, what was read and the rest stream, as does any other body,
///   multipart included, unread.
///
/// The chat is the query string's `chat_id`, else the buffered body's, as
/// Telegram reads it (`HttpReader` parses the URL before the body, and
/// `Query::arg` takes the first match). A streamed body's `chat_id` cannot
/// be read, so the query string's is the only chat tgin can know there;
/// without one only the global quota applies.
pub(super) async fn prepare(
    headers: &HeaderMap,
    query: Option<&str>,
    body: Body,
    idle: Duration,
    limited: bool,
) -> Result<(Payload, Option<ChatKey>), Failed> {
    let content_type = headers.get(CONTENT_TYPE);
    let readable = matches!(
        content_type.and_then(encoding),
        Some(Encoding::Json | Encoding::Form)
    ) || body.size_hint().exact() == Some(0);
    if !readable {
        return Ok((
            Payload::Streamed(body),
            limited.then(|| query_chat(query)).flatten(),
        ));
    }
    Ok(match read_within(body, BUFFER_LIMIT, idle).await? {
        Bounded::Complete(bytes) => {
            let chat = limited
                .then(|| query_chat(query).or_else(|| chat_key(content_type, &bytes)))
                .flatten();
            (Payload::Buffered(bytes), chat)
        }
        Bounded::Over(partial) => (
            Payload::Streamed(partial.into_body()),
            limited.then(|| query_chat(query)).flatten(),
        ),
    })
}

/// The query string's chat.
fn query_chat(query: Option<&str>) -> Option<ChatKey> {
    const FORM: HeaderValue = HeaderValue::from_static("application/x-www-form-urlencoded");
    query.and_then(|query| chat_key(Some(&FORM), query.as_bytes()))
}

/// `body` read up to `limit` bytes, waiting at most `idle` for each frame.
async fn read_within(body: Body, limit: usize, idle: Duration) -> Result<Bounded, Failed> {
    let progress = Arc::new(Progress::new());
    let stamped = Body::new(Upload::new(body, Arc::clone(&progress)));
    // A body that has ended completes the read at once, so only `idle`
    // ever runs out here.
    let bounds = Bounds {
        idle,
        response: idle,
    };
    match await_headers(read_bounded(stamped, limit), &progress, bounds).await {
        Waited::Ready(Ok(read)) => Ok(read),
        Waited::Ready(Err(_)) => Err(Failed::ClientClosed),
        Waited::Idle | Waited::NoAnswer => Err(Failed::Idle),
    }
}

/// For a bot whose server must not get a local file reference: refuses a
/// query string the server would read one from, and a request with more
/// than one `Content-Type` (which one the server reads is unclear);
/// otherwise the encoding of a body whose parameters are to be scanned.
pub(super) fn scanned_encoding(
    url: &Url,
    headers: &HeaderMap,
) -> Result<Option<Encoding>, Refusal> {
    if url
        .query()
        .is_some_and(|query| form_has_local_file(query.as_bytes()))
    {
        return Err(Refusal::LocalFile);
    }
    let mut content_types = headers.get_all(CONTENT_TYPE).iter();
    let content_type = content_types.next();
    if content_types.next().is_some() {
        return Err(Refusal::Unreadable);
    }
    Ok(content_type.and_then(encoding))
}

/// [`prepare`] for a body whose parameters are scanned:
/// - JSON or urlencoded, it is read up to [`BUFFER_LIMIT`]; whole, it is
///   refused when it carries a local file reference, else buffered, its
///   chat read from it, as in [`prepare`]. Longer, it streams through a
///   [`ScanGuard`], so tgin holds no more of it than an unscanned body;
///   one that announces more than [`MAX_PARAMETERS`] is refused as too
///   large at once, and one that only turns out longer, while it streams;
/// - multipart, it streams through a [`ScanGuard`]; an empty one, with no
///   parameters, goes as it is.
///
/// A streamed body's verdict comes back with it.
pub(super) async fn prepare_scanned(
    scanned: Encoding,
    content_type: Option<&HeaderValue>,
    query: Option<&str>,
    body: Body,
    idle: Duration,
    limited: bool,
) -> Result<(Payload, Option<ChatKey>, Option<oneshot::Receiver<Verdict>>), Failed> {
    let chat_of_query = || limited.then(|| query_chat(query)).flatten();
    if scanned == Encoding::Multipart {
        // An empty body carries no parameters; wrapped, its end would be
        // hidden until the guard read it, which hyper never does for a
        // zero length.
        if body.is_end_stream() {
            return Ok((Payload::Streamed(body), chat_of_query(), None));
        }
        let Some(boundary) =
            content_type.and_then(|content_type| boundary(content_type.as_bytes()))
        else {
            return Err(Failed::Refused(Refusal::Unreadable));
        };
        let (guard, verdict) = ScanGuard::multipart(body, boundary);
        return Ok((
            Payload::Streamed(Body::new(guard)),
            chat_of_query(),
            Some(verdict),
        ));
    }
    let partial = match read_within(body, BUFFER_LIMIT, idle).await? {
        Bounded::Complete(bytes) => {
            let found = match scanned {
                Encoding::Json => json_has_local_file(&bytes),
                Encoding::Form | Encoding::Multipart => form_has_local_file(&bytes),
            };
            if found {
                return Err(Failed::Refused(Refusal::LocalFile));
            }
            let chat = limited
                .then(|| query_chat(query).or_else(|| chat_key(content_type, &bytes)))
                .flatten();
            return Ok((Payload::Buffered(bytes), chat, None));
        }
        Bounded::Over(partial) => partial,
    };
    let body = partial.into_body();
    let max = u64::try_from(MAX_PARAMETERS).unwrap_or(u64::MAX);
    if body.size_hint().lower() > max {
        return Err(Failed::Refused(Refusal::TooLarge));
    }
    let (guard, verdict) = ScanGuard::params(body, scanned);
    Ok((
        Payload::Streamed(Body::new(guard)),
        chat_of_query(),
        Some(verdict),
    ))
}

/// `failed`, or the refusal that made it fail: a [`ScanGuard`] fails the
/// upload it refuses.
pub(super) fn refused_by(failed: Failed, verdict: Option<oneshot::Receiver<Verdict>>) -> Failed {
    match verdict.and_then(|mut verdict| verdict.try_recv().ok()) {
        Some(Verdict::Refused(refusal)) => Failed::Refused(refusal),
        _ => failed,
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use tokio::time::Instant;

    use super::*;

    /// A body tgin buffers is read with the same idle bound: a client that
    /// stops sending gets `Idle` after exactly `idle` without a frame, and
    /// one that goes away is told apart.
    #[tokio::test(start_paused = true)]
    async fn buffered_read_waits_at_most_idle_per_frame() {
        let idle = Duration::from_secs(5);
        let headers = json_headers();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let feeder = tokio::spawn(async move {
            for chunk in [&b"{\"chat_id\""[..], b":-5,"] {
                tokio::time::sleep(Duration::from_secs(4)).await;
                tx.send(Ok(Bytes::from_static(chunk))).await.expect("open");
            }
            // Stalls, keeping the body open.
            pending::<()>().await;
        });
        let started = Instant::now();
        let read = prepare(&headers, None, Body::from_stream(stream), idle, true).await;
        assert!(matches!(read, Err(Failed::Idle)));
        assert_eq!(started.elapsed(), Duration::from_secs(13));
        feeder.abort();

        let failing = futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other("gone"))]);
        let read = prepare(&headers, None, Body::from_stream(failing), idle, true).await;
        assert!(matches!(read, Err(Failed::ClientClosed)));
    }

    fn json_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers
    }

    /// The chat of a buffered body is the query string's `chat_id`, as
    /// Telegram reads it (`HttpReader` parses the URL before the body, and
    /// `Query::arg` takes the first match), else the body's own; a streamed
    /// body's is the query string's, the only one tgin can know. Without
    /// limits, none is read.
    #[tokio::test]
    async fn chat_from_query_before_body() {
        let idle = Duration::from_secs(5);
        let headers = json_headers();
        let query = Some("chat_id=%40Chan");
        let body = || Body::from(r#"{"chat_id":-5}"#);

        let Ok((Payload::Buffered(bytes), chat)) =
            prepare(&headers, query, body(), idle, true).await
        else {
            panic!("buffered");
        };
        assert_eq!(&bytes[..], br#"{"chat_id":-5}"#);
        assert_eq!(
            chat,
            Some(ChatKey::Username("chan".into())),
            "the query comes first"
        );

        let Ok((Payload::Buffered(_), chat)) =
            prepare(&headers, Some("x=1"), body(), idle, true).await
        else {
            panic!("buffered");
        };
        assert_eq!(chat, Some(ChatKey::Id(-5)), "else the body's");

        let Ok((Payload::Buffered(_), chat)) =
            prepare(&headers, query, Body::from("{}"), idle, true).await
        else {
            panic!("buffered");
        };
        assert_eq!(chat, Some(ChatKey::Username("chan".into())));

        let long = format!(r#"{{"chat_id":-5,"text":"{}"}}"#, "x".repeat(BUFFER_LIMIT));
        let Ok((Payload::Streamed(_), chat)) =
            prepare(&headers, query, Body::from(long), idle, true).await
        else {
            panic!("streamed");
        };
        assert_eq!(chat, Some(ChatKey::Username("chan".into())));

        let mut multipart = HeaderMap::new();
        multipart.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("multipart/form-data; boundary=b"),
        );
        let Ok((Payload::Streamed(_), chat)) = prepare(&multipart, None, body(), idle, true).await
        else {
            panic!("streamed");
        };
        assert_eq!(chat, None);
        let Ok((Payload::Streamed(_), chat)) = prepare(&multipart, query, body(), idle, true).await
        else {
            panic!("streamed");
        };
        assert_eq!(
            chat,
            Some(ChatKey::Username("chan".into())),
            "a multipart body's chat is the query's"
        );

        let Ok((Payload::Buffered(_), chat)) = prepare(&headers, query, body(), idle, false).await
        else {
            panic!("buffered");
        };
        assert_eq!(chat, None, "no limits, no chat");
    }
}
