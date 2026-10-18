//! The file route of self-hosted Bot API servers and local mode's `getFile`
//! paths (plan decision 3):
//!
//! | Upstream | `/file/bot<token>/<rel>` | `getFile` |
//! |---|---|---|
//! | `cloud` | proxied to `{url}/file/bot{token}/` | verbatim |
//! | `local`, `files.dir` | served from the bot's directory under `dir` | local mode: `{server_dir}/{token dir}/X` → `X` |
//! | `local`, `files.url` | proxied to `{files.url}/{token}/` | local mode: `…/{token dir}/X` → `X` |
//! | `local`, no `files` | 404 `files are not served for this upstream` | verbatim |
//!
//! A bot's directory on the server is named after its token: the token as
//! it is, or with ':' → '~' where the server's filesystem refuses ':'
//! ([`Upstream::token_dirs`]). Both names are secrets: nothing here logs a
//! path, a directory or a URL, only the bot's name.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, LAST_MODIFIED, RANGE,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::BytesMut;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use reqwest::Url;
use serde::Deserialize;
use serde_json::value::RawValue;
use tgin_core::config::{Files, Upstream, UpstreamKind};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use super::proxy::NO_ANSWER;
use super::{Bot, Bounded, base, errors, read_bounded};

/// A file the bot's directory does not hold.
pub(super) const FILE_NOT_FOUND: &str = "Not Found: file not found";
/// The file route of a `local` upstream without `files`.
pub(super) const NOT_SERVED: &str = "Not Found: files are not served for this upstream";
/// A path that leaves the bot's directory, or that no file origin is
/// given.
pub(super) const NOT_FOUND: &str = "Not Found";
/// The largest `getFile` answer read for its rewrite (ruling R3); a larger
/// one passes verbatim.
const GET_FILE_LIMIT: usize = 64 * 1024;
/// The read size of a served file.
const CHUNK: usize = 64 * 1024;
/// A token in a URL path: everything but `[A-Za-z0-9._~-]` percent-encoded,
/// so its ':' goes as `%3A`.
const TOKEN_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Where a bot's `/file/bot<token>/<rel>` route goes.
pub(super) enum FileTarget {
    /// The cloud: the raw path proxied below `{url}/file/bot{token}/`.
    Proxy(Url),
    /// A file origin: the path checked by [`origin_path`] and proxied below
    /// `{files.url}/{token}/`.
    Origin(Url),
    /// Served from the bot's directory under `root`, the server's
    /// directory as mounted in tgin.
    Disk { root: PathBuf },
    /// Answered with [`NOT_SERVED`].
    NotServed,
}

/// Which local-mode `getFile` paths are the bot's own, to be rewritten to
/// the relative path under its directory.
pub(super) enum Rewrite {
    /// Not local mode, or no `files`: every answer verbatim.
    Off,
    /// `{server_dir}/{token dir}/…`: the server's directory as it sees it.
    Under(String),
    /// An absolute path through `/{token dir}/` (ruling R9: a file origin
    /// has no `server_dir`).
    AtBotDir,
}

impl FileTarget {
    /// The file route of a bot with `token` on `upstream`; `None` for a URL
    /// that cannot be a base.
    pub(super) fn new(upstream: &Upstream, token: &str) -> Option<FileTarget> {
        Some(match &upstream.files {
            None if upstream.kind == UpstreamKind::Cloud => FileTarget::Proxy(base(
                upstream.url.expose(),
                &["file", &format!("bot{token}")],
            )?),
            None => FileTarget::NotServed,
            Some(Files::Dir { dir, .. }) => FileTarget::Disk {
                root: dir.expose().clone(),
            },
            Some(Files::Url(url)) => FileTarget::Origin(origin_base(url.expose(), token)?),
        })
    }
}

impl Rewrite {
    /// The rewrite of local-mode `getFile` answers from `upstream`. A
    /// `server_dir` that is not UTF-8 can match no JSON string: off.
    pub(super) fn new(upstream: &Upstream) -> Rewrite {
        if !upstream.local_mode {
            return Rewrite::Off;
        }
        match &upstream.files {
            None => Rewrite::Off,
            Some(Files::Url(_)) => Rewrite::AtBotDir,
            Some(Files::Dir { server_dir, .. }) => match server_dir.expose().to_str() {
                Some(root) => Rewrite::Under(root.to_owned()),
                None => {
                    tracing::warn!(upstream = %upstream.name, "files.server_dir is not UTF-8; local getFile paths stay absolute");
                    Rewrite::Off
                }
            },
        }
    }

    pub(super) fn is_on(&self) -> bool {
        !matches!(self, Rewrite::Off)
    }

    /// The root `rewrite_get_file` matches under, if any.
    fn server_root(&self) -> Option<&str> {
        match self {
            Rewrite::Under(root) => Some(root),
            Rewrite::Off | Rewrite::AtBotDir => None,
        }
    }
}

/// `{files.url}/{token}/`, the token percent-encoded; query and fragment
/// dropped.
fn origin_base(files_url: &Url, token: &str) -> Option<Url> {
    if files_url.cannot_be_a_base() {
        return None;
    }
    let mut url = files_url.clone();
    url.set_query(None);
    url.set_fragment(None);
    let mut path = url.path().to_owned();
    if !path.ends_with('/') {
        path.push('/');
    }
    path.extend(utf8_percent_encode(token, TOKEN_SEGMENT));
    path.push('/');
    url.set_path(&path);
    Some(url)
}

/// The bot's directory under `root`: the first of [`Upstream::token_dirs`]
/// that is a directory. The colon form cannot exist on Windows, where ':'
/// would name an alternate data stream, so it is not tried there.
pub(super) async fn bot_dir(root: &Path, token: &str) -> Option<PathBuf> {
    for form in Upstream::token_dirs(token) {
        if cfg!(windows) && form.contains(':') {
            continue;
        }
        let dir = root.join(form);
        if tokio::fs::metadata(&dir)
            .await
            .is_ok_and(|meta| meta.is_dir())
        {
            return Some(dir);
        }
    }
    None
}

/// Serves `rel` (a request's file path, still percent-encoded) from the
/// bot's directory `dir`:
/// - a path that is no plain relative path, or whose canonical form (links
///   followed) leaves `dir`'s, answers 404 `Not Found`;
/// - a file that is not there 404 `Not Found: file not found`;
/// - otherwise the file streams as `application/octet-stream` with its
///   length, a weak `ETag` from its size and modification time and its
///   `Last-Modified`; `If-None-Match` (else `If-Modified-Since`) answers
///   304; a single satisfiable byte range (unless a stale `If-Range`)
///   answers 206, an unsatisfiable one 416; several ranges get the whole
///   file.
pub async fn serve(dir: &Path, rel: &str, req_headers: &HeaderMap) -> Response {
    let Some(rel) = relative(rel) else {
        return errors::error(StatusCode::NOT_FOUND, NOT_FOUND);
    };
    let file_not_found = || errors::error(StatusCode::NOT_FOUND, FILE_NOT_FOUND);
    let Ok(base) = tokio::fs::canonicalize(dir).await else {
        return file_not_found();
    };
    let Ok(target) = tokio::fs::canonicalize(base.join(rel)).await else {
        return file_not_found();
    };
    if !target.starts_with(&base) {
        tracing::debug!("gateway file request refused: the path leaves the bot's directory");
        return errors::error(StatusCode::NOT_FOUND, NOT_FOUND);
    }
    let Ok(mut file) = tokio::fs::File::open(&target).await else {
        return file_not_found();
    };
    let meta = match file.metadata().await {
        Ok(meta) if meta.is_file() => meta,
        _ => return file_not_found(),
    };
    let len = meta.len();
    let mtime = meta.modified().ok();
    let etag = etag(len, mtime);
    let modified = mtime.and_then(whole_seconds);
    let last_modified = modified.map(httpdate::fmt_http_date);

    let mut validators = HeaderMap::new();
    validators.insert(ETAG, header_value(&etag));
    if let Some(last_modified) = &last_modified {
        validators.insert(LAST_MODIFIED, header_value(last_modified));
    }
    if not_modified(req_headers, &etag, modified) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        *response.headers_mut() = validators;
        return response;
    }

    let range = req_headers
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .filter(|_| if_range_holds(req_headers, last_modified.as_deref()))
        .map_or(Range::Whole, |value| range(value, len));
    let (status, start, count) = match range {
        Range::Whole => (StatusCode::OK, 0, len),
        Range::Part { start, end } => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        Range::Unsatisfiable => {
            let mut response = errors::error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "Requested Range Not Satisfiable",
            );
            response
                .headers_mut()
                .insert(CONTENT_RANGE, header_value(&format!("bytes */{len}")));
            return response;
        }
    };
    if start > 0 && file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return file_not_found();
    }
    let stream = ReaderStream::with_capacity(file.take(count), CHUNK);
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    *headers = validators;
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(CONTENT_LENGTH, HeaderValue::from(count));
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if status == StatusCode::PARTIAL_CONTENT {
        let end = start + count - 1;
        headers.insert(
            CONTENT_RANGE,
            header_value(&format!("bytes {start}-{end}/{len}")),
        );
    }
    response
}

/// `rel` percent-decoded once, when it is plain relative segments: `None`
/// for invalid UTF-8, a NUL, a `\`, or an empty (so an absolute path too),
/// `.` or `..` segment.
fn checked(rel: &str) -> Option<Cow<'_, str>> {
    let rel = percent_decode_str(rel).decode_utf8().ok()?;
    let plain = !rel.contains(['\0', '\\'])
        && rel
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    plain.then_some(rel)
}

/// `rel` as a relative path of plain components ([`checked`]), refused
/// on Windows also for a ':' (a drive or an alternate data stream).
fn relative(rel: &str) -> Option<PathBuf> {
    let rel = checked(rel)?;
    if cfg!(windows) && rel.contains(':') {
        return None;
    }
    let mut path = PathBuf::new();
    for segment in rel.split('/') {
        let mut components = Path::new(segment).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) if name == segment => path.push(name),
            _ => return None,
        }
    }
    Some(path)
}

/// `rel` for a file origin: its [`checked`] segments, each percent-encoded
/// again with the strict unreserved set, so an origin that decodes once
/// sees exactly the path checked here, whatever the request's encoding.
/// The rules do not depend on tgin's OS: the origin is remote.
pub(super) fn origin_path(rel: &str) -> Option<String> {
    let rel = checked(rel)?;
    let mut path = String::with_capacity(rel.len());
    for (i, segment) in rel.split('/').enumerate() {
        if i > 0 {
            path.push('/');
        }
        path.extend(utf8_percent_encode(segment, TOKEN_SEGMENT));
    }
    Some(path)
}

/// `time` cut to whole seconds, as HTTP dates carry it; `None` before
/// 1970.
fn whole_seconds(time: SystemTime) -> Option<SystemTime> {
    let since = time.duration_since(UNIX_EPOCH).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(since.as_secs()))
}

/// A weak validator: the file's size and full-precision modification time.
fn etag(len: u64, modified: Option<SystemTime>) -> String {
    match modified.and_then(|time| time.duration_since(UNIX_EPOCH).ok()) {
        Some(since) => format!("W/\"{len:x}-{:x}\"", since.as_nanos()),
        None => format!("W/\"{len:x}\""),
    }
}

/// `text` as a header value; every caller's text is visible ASCII.
fn header_value(text: &str) -> HeaderValue {
    HeaderValue::from_str(text).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Whether the request's validators match the file's: `If-None-Match`
/// (weak comparison, `*` matches) when sent, else `If-Modified-Since`.
fn not_modified(headers: &HeaderMap, etag: &str, modified: Option<SystemTime>) -> bool {
    let opaque = |tag: &str| tag.trim().trim_start_matches("W/").to_owned();
    let ours = opaque(etag);
    if headers.contains_key(IF_NONE_MATCH) {
        return headers
            .get_all(IF_NONE_MATCH)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|tag| tag.trim() == "*" || opaque(tag) == ours);
    }
    let since = headers
        .get(IF_MODIFIED_SINCE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| httpdate::parse_http_date(value).ok());
    matches!((modified, since), (Some(modified), Some(since)) if modified <= since)
}

/// Whether a `Range` applies under `If-Range`: always without one; with
/// one, only when it is the file's `Last-Modified` (the `ETag` is weak, and
/// `If-Range` needs a strong match).
fn if_range_holds(headers: &HeaderMap, last_modified: Option<&str>) -> bool {
    match headers.get(IF_RANGE) {
        None => true,
        Some(value) => last_modified.is_some_and(|date| value.as_bytes() == date.as_bytes()),
    }
}

/// What a `Range` header asks of a file of `len` bytes.
#[derive(Debug, PartialEq, Eq)]
enum Range {
    /// No usable range: several, another unit, or one that does not parse.
    Whole,
    /// Bytes `start..=end`, within the file.
    Part { start: u64, end: u64 },
    /// A range wholly past the end, or an empty suffix.
    Unsatisfiable,
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` (RFC 9110 §14.1.2).
fn range(value: &str, len: u64) -> Range {
    let Some((unit, spec)) = value.split_once('=') else {
        return Range::Whole;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") || spec.contains(',') {
        return Range::Whole;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return Range::Whole;
    };
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        return match digits(last) {
            None => Range::Whole,
            Some(0) => Range::Unsatisfiable,
            Some(_) if len == 0 => Range::Unsatisfiable,
            Some(suffix) => Range::Part {
                start: len.saturating_sub(suffix),
                end: len - 1,
            },
        };
    }
    let Some(start) = digits(first) else {
        return Range::Whole;
    };
    let end = if last.is_empty() {
        u64::MAX
    } else {
        match digits(last) {
            Some(end) if end >= start => end,
            _ => return Range::Whole,
        }
    };
    if start >= len {
        return Range::Unsatisfiable;
    }
    Range::Part {
        start,
        end: end.min(len - 1),
    }
}

/// A non-empty run of ASCII digits as a number.
fn digits(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// A local-mode `getFile` answer from `bot`'s upstream with its own
/// absolute `file_path` made relative (ruling R3): a 2xx answer without
/// `Content-Encoding` of at most [`GET_FILE_LIMIT`] bytes is read and
/// rewritten by [`rewrite_get_file`], `Content-Length` adjusted; any other
/// answer passes verbatim.
pub(super) async fn rewrite_response(bot: &Bot, response: Response) -> Response {
    let encoded = response
        .headers()
        .get(CONTENT_ENCODING)
        .is_some_and(|value| value != "identity");
    if !response.status().is_success() || encoded {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    match read_bounded(body, GET_FILE_LIMIT).await {
        Ok(Bounded::Complete(body)) => {
            match rewrite_get_file(&body, bot.token.expose(), bot.rewrite.server_root()) {
                Some(rewritten) => {
                    parts
                        .headers
                        .insert(CONTENT_LENGTH, HeaderValue::from(rewritten.len()));
                    Response::from_parts(parts, Body::from(rewritten))
                }
                None => {
                    tracing::debug!(bot = %bot.name, "getFile answer passed verbatim: no file path under the bot's own directory");
                    Response::from_parts(parts, Body::from(body))
                }
            }
        }
        Ok(Bounded::Over(partial)) => {
            tracing::debug!(bot = %bot.name, "getFile answer over 64 KiB passed verbatim");
            Response::from_parts(parts, partial.into_body())
        }
        // The proxy counted and logged the failed body; the call went out.
        Err(_) => errors::error(StatusCode::BAD_GATEWAY, NO_ANSWER),
    }
}

/// `body`, a successful `getFile` answer, with `result.file_path` replaced
/// by its remainder under the directory of the bot with `token` (see
/// [`own_relative`]); every other byte as it was. `None` when the body is
/// no such answer or its path is not the bot's own.
pub(super) fn rewrite_get_file(
    body: &[u8],
    token: &str,
    server_root: Option<&str>,
) -> Option<Bytes> {
    #[derive(Deserialize)]
    struct Answer<'a> {
        ok: bool,
        #[serde(borrow)]
        result: Option<&'a RawValue>,
    }
    #[derive(Deserialize)]
    struct File<'a> {
        #[serde(borrow)]
        file_path: Option<&'a RawValue>,
    }
    let answer: Answer<'_> = serde_json::from_slice(body).ok()?;
    if !answer.ok {
        return None;
    }
    let file: File<'_> = serde_json::from_str(answer.result?.get()).ok()?;
    let raw = file.file_path?.get();
    let path: String = serde_json::from_str(raw).ok()?;
    let rel = own_relative(&path, token, server_root)?;
    // `raw` borrows from `body`: where it sits there.
    let start = raw.as_ptr().addr().checked_sub(body.as_ptr().addr())?;
    let end = start.checked_add(raw.len())?;
    if body.get(start..end) != Some(raw.as_bytes()) {
        return None;
    }
    let encoded = serde_json::to_vec(&rel).ok()?;
    let mut out = BytesMut::with_capacity(body.len() - raw.len() + encoded.len());
    out.extend_from_slice(&body[..start]);
    out.extend_from_slice(&encoded);
    out.extend_from_slice(&body[end..]);
    Some(out.freeze())
}

/// The non-empty remainder of `path` under the directory of the bot with
/// `token`, in either form, with `/` separators:
/// - with `server_root`, after `{server_root}/{token dir}/`;
/// - without (a file origin), after the first `/{token dir}/` of an
///   absolute path.
///
/// `\` separators (a server on Windows) count as `/`.
fn own_relative(path: &str, token: &str, server_root: Option<&str>) -> Option<String> {
    let dirs = Upstream::token_dirs(token);
    let mut found: Option<(usize, &str, char)> = None;
    for dir in &dirs {
        for sep in ['/', '\\'] {
            let rest = match server_root {
                Some(root) => path
                    .strip_prefix(root.trim_end_matches(['/', '\\']))
                    .and_then(|rest| rest.strip_prefix(sep))
                    .and_then(|rest| rest.strip_prefix(dir.as_str()))
                    .and_then(|rest| rest.strip_prefix(sep))
                    .map(|rest| (path.len() - rest.len(), rest)),
                None if absolute(path) => {
                    let needle = format!("{sep}{dir}{sep}");
                    path.find(&needle)
                        .map(|at| (at, &path[at + needle.len()..]))
                }
                None => None,
            };
            if let Some((at, rest)) = rest.filter(|(_, rest)| !rest.is_empty())
                && found.is_none_or(|(first, _, _)| at < first)
            {
                found = Some((at, rest, sep));
            }
        }
    }
    let (_, rest, sep) = found?;
    Some(if sep == '\\' {
        rest.replace('\\', "/")
    } else {
        rest.to_owned()
    })
}

/// `/…`, `\…` or `X:\…` / `X:/…`.
fn absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    match bytes {
        [b'/' | b'\\', ..] => true,
        [drive, b':', b'/' | b'\\', ..] => drive.is_ascii_alphabetic(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "123:AAbb";

    #[test]
    fn ranges() {
        let part = |start, end| Range::Part { start, end };
        assert_eq!(range("bytes=0-3", 10), part(0, 3));
        assert_eq!(range("bytes=2-", 10), part(2, 9));
        assert_eq!(range("bytes=-3", 10), part(7, 9));
        assert_eq!(range("bytes=-30", 10), part(0, 9));
        assert_eq!(range("bytes=5-100", 10), part(5, 9));
        assert_eq!(range("Bytes = 1-1", 10), part(1, 1));
        assert_eq!(range("bytes=10-", 10), Range::Unsatisfiable);
        assert_eq!(range("bytes=-0", 10), Range::Unsatisfiable);
        assert_eq!(range("bytes=-1", 0), Range::Unsatisfiable);
        assert_eq!(range("bytes=0-", 0), Range::Unsatisfiable);
        for whole in [
            "bytes=0-1,3-4",
            "items=0-1",
            "bytes=3-1",
            "bytes=+1-2",
            "bytes=a-",
            "bytes=-",
            "bytes 0-1",
        ] {
            assert_eq!(range(whole, 10), Range::Whole, "{whole}");
        }
    }

    #[test]
    fn relative_paths_only() {
        assert_eq!(
            relative("documents/file_1.pdf"),
            Some(PathBuf::from("documents/file_1.pdf"))
        );
        assert_eq!(relative("a%20b/c"), Some(PathBuf::from("a b/c")));
        for refused in [
            "",
            "/etc/passwd",
            "%2Fetc",
            "a//b",
            "a/",
            "..%2Fx",
            "a%2F..%2Fb",
            ".",
            "a/./b",
            "a%5Cb",
            "a%00",
            "%ff",
        ] {
            assert_eq!(relative(refused), None, "{refused}");
        }
        if cfg!(windows) {
            assert_eq!(relative("C%3Aabc"), None);
        }
    }

    #[test]
    fn origin_paths_checked_and_encoded_again() {
        let ok = |rel| origin_path(rel).unwrap_or_else(|| panic!("{rel} refused"));
        assert_eq!(ok("documents/file_1.pdf"), "documents/file_1.pdf");
        assert_eq!(ok("documents/file%5F1.pdf"), "documents/file_1.pdf");
        assert_eq!(ok("%252e%252e%252fx"), "%252e%252e%252fx");
        assert_eq!(ok("a%20b/c%3Ad"), "a%20b/c%3Ad");
        assert_eq!(ok("C%3Aabc"), "C%3Aabc");
        assert_eq!(ok("%C3%A9"), "%C3%A9");
        for refused in [
            "",
            "/x",
            "%2Fx",
            "a//b",
            "a/",
            ".",
            "..",
            "a/./b",
            "..%2F",
            "..%2F..%2Fx",
            "%2e%2e%2fx",
            "a%5C..%5Cx",
            "a%00b",
            "%ff",
        ] {
            assert_eq!(origin_path(refused), None, "{refused}");
        }
    }

    #[test]
    fn own_relative_paths() {
        let root = Some("/srv/tba");
        assert_eq!(
            own_relative("/srv/tba/123:AAbb/a/b", TOKEN, root).as_deref(),
            Some("a/b")
        );
        assert_eq!(
            own_relative("/srv/tba/123~AAbb/a", TOKEN, root).as_deref(),
            Some("a")
        );
        assert_eq!(
            own_relative("/srv/tba/123:AAbb/a", TOKEN, Some("/srv/tba/")).as_deref(),
            Some("a")
        );
        assert_eq!(
            own_relative("/123:AAbb/a", TOKEN, Some("/")).as_deref(),
            Some("a")
        );
        assert_eq!(
            own_relative("C:\\tba\\123~AAbb\\a\\b", TOKEN, Some("C:\\tba")).as_deref(),
            Some("a/b")
        );
        for verbatim in [
            "/srv/tba/123:AAbb/",
            "/srv/tba/123:AAbb",
            "/srv/tba/124:AAbb/a",
            "/srv/tba2/123:AAbb/a",
            "/srv/123:AAbb/a",
            "srv/tba/123:AAbb/a",
        ] {
            assert_eq!(own_relative(verbatim, TOKEN, root), None, "{verbatim}");
        }
        assert_eq!(
            own_relative("/x/y/123:AAbb/a/b", TOKEN, None).as_deref(),
            Some("a/b")
        );
        assert_eq!(
            own_relative("/x/123~AAbb/123:AAbb/a", TOKEN, None).as_deref(),
            Some("123:AAbb/a")
        );
        assert_eq!(
            own_relative("D:/x/123~AAbb/a", TOKEN, None).as_deref(),
            Some("a")
        );
        assert_eq!(own_relative("x/123:AAbb/a", TOKEN, None), None);
        assert_eq!(own_relative("/x/124:AAbb/a", TOKEN, None), None);
        assert_eq!(own_relative("/x/9123:AAbb/a", TOKEN, None), None);
    }

    #[test]
    fn get_file_rewritten_in_place() {
        let body = br#"{"ok":true,"result":{"file_id":"x","file_path":"\/srv\/tba\/123:AAbb\/a\/\u00e9.pdf","file_size":3}}"#;
        let out = rewrite_get_file(body, TOKEN, Some("/srv/tba")).expect("rewritten");
        assert_eq!(
            &out[..],
            "{\"ok\":true,\"result\":{\"file_id\":\"x\",\"file_path\":\"a/\u{e9}.pdf\",\"file_size\":3}}".as_bytes()
        );
        for verbatim in [
            &br#"{"ok":false,"result":{"file_path":"/srv/tba/123:AAbb/a"}}"#[..],
            br#"{"ok":true,"result":{"file_path":"/srv/tba/124:AAbb/a"}}"#,
            br#"{"ok":true,"result":{"file_path":"/srv/tba/123:AAbb/a","file_path":"/srv/tba/123:AAbb/b"}}"#,
            br#"{"ok":true,"result":{"file_path":7}}"#,
            br#"{"ok":true,"result":true}"#,
            br#"{"ok":true}"#,
            b"not json",
        ] {
            assert_eq!(rewrite_get_file(verbatim, TOKEN, Some("/srv/tba")), None);
        }
    }

    #[test]
    fn origin_base_encodes_the_token() {
        let url = Url::parse("http://files:8080/tba?x=1#f").expect("a URL");
        assert_eq!(
            origin_base(&url, "123:AA-b_c").expect("a base").as_str(),
            "http://files:8080/tba/123%3AAA-b_c/"
        );
        let url = Url::parse("http://files:8080/").expect("a URL");
        assert_eq!(
            origin_base(&url, "1:a").expect("a base").as_str(),
            "http://files:8080/1%3Aa/"
        );
    }

    #[test]
    fn conditionals() {
        let modified = UNIX_EPOCH + Duration::from_secs(1_000_000);
        // Full precision: 1_000_000 s + 500 ns (Windows keeps 100 ns).
        assert_eq!(
            etag(10, Some(modified + Duration::from_nanos(500))),
            "W/\"a-38d7ea4c681f4\""
        );
        let etag = etag(10, Some(modified));
        assert_eq!(etag, "W/\"a-38d7ea4c68000\"");
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut map = HeaderMap::new();
            for (name, value) in pairs {
                map.append(*name, HeaderValue::from_str(value).expect("a value"));
            }
            map
        };
        assert!(not_modified(
            &headers(&[("if-none-match", "\"a-38d7ea4c68000\"")]),
            &etag,
            Some(modified)
        ));
        assert!(not_modified(
            &headers(&[
                ("if-none-match", "\"x\""),
                ("if-none-match", "W/\"a-38d7ea4c68000\"")
            ]),
            &etag,
            Some(modified)
        ));
        assert!(!not_modified(
            &headers(&[("if-none-match", "\"x\"")]),
            &etag,
            Some(modified)
        ));
        let date = httpdate::fmt_http_date(modified);
        assert!(not_modified(
            &headers(&[("if-modified-since", &date)]),
            &etag,
            Some(modified)
        ));
        assert!(!not_modified(
            &headers(&[("if-modified-since", "garbage")]),
            &etag,
            Some(modified)
        ));
        assert!(!not_modified(
            &headers(&[("if-modified-since", &date)]),
            &etag,
            None
        ));
        assert!(!not_modified(&headers(&[]), &etag, Some(modified)));
    }
}
