//! Local file references (plan decision 4, ruling R14): a Bot API server in
//! local mode reads a parameter as a file of its own disk when the value
//! begins with `file:/` (telegram-bot-api `Client::get_input_file`). For a
//! bot on a `local` upstream in local mode without `allow_local_files`, the
//! gateway and the webhook-reply executor refuse every request in which a
//! parameter the server reads begins with `file:`: one byte short of the
//! server's prefix, a margin against newer servers.
//!
//! The server reads parameters from the query string and from a JSON, an
//! urlencoded or a multipart body (td's `HttpReader`), and parses JSON
//! carried in some of them (`media`, `stickers`, `results`, …). Each is
//! decoded here as the server decodes it: percent-decoding (`+` as a space)
//! for the query string and urlencoded values, unescaping for JSON strings,
//! and `HttpReader`'s own multipart grammar, which tells text fields from
//! files by its own `Content-Disposition` rules (multer's differ: it takes
//! `filename` inside a quoted parameter for a file name, where the server
//! sees a text field) and reads a part typed
//! `application/x-www-form-urlencoded` as more parameters. What follows
//! the server's own parser is ported in [`http_reader`], TDLib's code.
//!
//! Every scan is one pass over the bytes through small state machines. It
//! does not recurse on JSON structure, so nesting costs nothing; it follows
//! JSON carried in strings at most [`MAX_DEPTH`] values deep, and refuses a
//! JSON document found deeper. The only allocation is a multipart part's
//! header block. Nothing here logs: a value is a path on the server.

mod http_reader;

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use http_body::{Body as HttpBody, Frame, SizeHint};
use tgin_core::config::{Upstream, UpstreamKind};
use tokio::sync::oneshot;

use self::http_reader::{Form, MultipartScan, is_space};
pub(super) use self::http_reader::{MAX_PARAMETERS, boundary};
use super::errors;
use super::intercept::TOO_LARGE;
use super::params::{Encoding, encoding};

/// The answer to a local file reference.
const LOCAL_FILE_REFUSED: &str = "Bad Request: local file uploads are disabled by tgin";
/// The answer to a body the server would refuse as malformed, which tgin
/// cannot clear either.
const UNREADABLE: &str = "Bad Request: can't parse the request parameters";
/// What tgin refuses: a decoded value beginning with it, in this case.
const LOCAL_PREFIX: &[u8] = b"file:";
/// Values deep the scan follows: a parameter, the strings of JSON it
/// carries, the strings of JSON carried in those, and one more level; a
/// JSON document in a value of the last level is refused unread.
const MAX_DEPTH: usize = 4;

/// Whether a bot on `upstream` has its local file references refused: a
/// `local` upstream in local mode, unless the bot allows them.
pub(super) fn refuses_local_files(upstream: &Upstream, allow_local_files: bool) -> bool {
    upstream.kind == UpstreamKind::Local && upstream.local_mode && !allow_local_files
}

/// The outcome of a scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// No parameter carries a local file reference.
    Clean,
    Refused(Refusal),
}

/// Why a request is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// A parameter carries a local file reference, or JSON nested deeper
    /// than [`MAX_DEPTH`].
    LocalFile,
    /// The parameters are over [`MAX_PARAMETERS`].
    TooLarge,
    /// The server would refuse the body as malformed; tgin cannot clear
    /// it.
    Unreadable,
}

impl Refusal {
    /// The Telegram-shaped answer.
    pub(super) fn answer(self) -> Response {
        match self {
            Refusal::LocalFile => errors::error(StatusCode::BAD_REQUEST, LOCAL_FILE_REFUSED),
            Refusal::TooLarge => errors::error(StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE),
            Refusal::Unreadable => errors::error(StatusCode::BAD_REQUEST, UNREADABLE),
        }
    }

    /// A log field.
    pub(super) fn reason(self) -> &'static str {
        match self {
            Refusal::LocalFile => "local_file",
            Refusal::TooLarge => "too_large",
            Refusal::Unreadable => "unreadable",
        }
    }
}

/// Whether a JSON body carries a local file reference: any string at any
/// depth, JSON carried in a string included. Malformed JSON is scanned as
/// far as it goes, since the server keeps the parameters it read before an
/// error.
pub(super) fn json_has_local_file(body: &[u8]) -> bool {
    let mut values = Values::default();
    body.iter().any(|&b| {
        values.json_byte(0, b);
        values.found
    })
}

/// Whether an urlencoded text (a body, or a query string) carries a local
/// file reference: any value, decoded as the server decodes it, JSON
/// carried in it included.
pub(super) fn form_has_local_file(text: &[u8]) -> bool {
    let mut values = Values::default();
    let mut form = Form::default();
    for &b in text {
        form.byte(&mut values, b);
        if values.found {
            return true;
        }
    }
    form.end(&mut values);
    values.found
}

/// The verdict on a whole body of type `content_type`, read as the server
/// reads it; a body of any other type has no parameters.
pub(super) fn body_verdict(content_type: Option<&HeaderValue>, body: &[u8]) -> Verdict {
    let found = |found: bool| {
        if found {
            Verdict::Refused(Refusal::LocalFile)
        } else {
            Verdict::Clean
        }
    };
    let Some(content_type) = content_type else {
        return Verdict::Clean;
    };
    match encoding(content_type) {
        None => Verdict::Clean,
        Some(Encoding::Json) => found(json_has_local_file(body)),
        Some(Encoding::Form) => found(form_has_local_file(body)),
        Some(Encoding::Multipart) => match boundary(content_type.as_bytes()) {
            None => Verdict::Refused(Refusal::Unreadable),
            Some(boundary) => {
                let mut scan = MultipartScan::new(boundary);
                scan.feed(body).unwrap_or_else(|| scan.finish())
            }
        },
    }
}

/// What a value has shown of itself so far.
#[derive(Clone, Copy, Default)]
struct Value {
    /// Bytes of [`LOCAL_PREFIX`] it began with; `u8::MAX` once it cannot.
    matched: u8,
    shape: Shape,
}

#[derive(Clone, Copy, Default)]
enum Shape {
    /// Nothing but whitespace yet.
    #[default]
    Leading,
    /// A JSON document (it began with `[` or `{`), scanned for strings.
    Json,
    /// Anything else.
    Plain,
}

/// Where a JSON document's scan is: its strings are values one level
/// deeper, whatever the structure around them.
#[derive(Clone, Copy, Default)]
enum Json {
    #[default]
    Outside,
    Text,
    /// After a `\` in a string.
    Escape,
    /// After `\u` and `digits` hex digits of `code`.
    Unicode {
        digits: u8,
        code: u32,
    },
}

/// The values being read at each depth and whether one is a local file
/// reference.
#[derive(Default)]
struct Values {
    value: [Value; MAX_DEPTH],
    json: [Json; MAX_DEPTH],
    found: bool,
}

impl Values {
    /// A new value at `depth`.
    fn start(&mut self, depth: usize) {
        self.value[depth] = Value::default();
        self.json[depth] = Json::Outside;
    }

    /// The next decoded byte of the value at `depth`.
    fn byte(&mut self, depth: usize, b: u8) {
        let value = &mut self.value[depth];
        if let Some(&expected) = LOCAL_PREFIX.get(usize::from(value.matched)) {
            if b == expected {
                value.matched += 1;
                self.found |= usize::from(value.matched) == LOCAL_PREFIX.len();
            } else {
                value.matched = u8::MAX;
            }
        }
        match value.shape {
            Shape::Plain => {}
            Shape::Json => self.json_byte(depth, b),
            Shape::Leading if is_space(b) => {}
            Shape::Leading if b == b'[' || b == b'{' => {
                value.shape = Shape::Json;
                if depth + 1 < MAX_DEPTH {
                    self.json_byte(depth, b);
                } else {
                    self.found = true;
                }
            }
            Shape::Leading => value.shape = Shape::Plain,
        }
    }

    /// The next byte of the JSON document in the value at `depth`, where
    /// `depth + 1` is below [`MAX_DEPTH`]; the decoded bytes of its strings
    /// are values at `depth + 1`.
    fn json_byte(&mut self, depth: usize, b: u8) {
        let inner = depth + 1;
        match self.json[depth] {
            Json::Outside => {
                if b == b'"' {
                    self.json[depth] = Json::Text;
                    self.start(inner);
                }
            }
            Json::Text => match b {
                b'"' => self.json[depth] = Json::Outside,
                b'\\' => self.json[depth] = Json::Escape,
                _ => self.byte(inner, b),
            },
            Json::Escape => {
                if b == b'u' {
                    self.json[depth] = Json::Unicode { digits: 0, code: 0 };
                } else {
                    self.json[depth] = Json::Text;
                    self.byte(inner, unescape(b));
                }
            }
            Json::Unicode { digits, code } => match hex(b) {
                Some(digit) => {
                    let code = code << 4 | u32::from(digit);
                    if digits < 3 {
                        self.json[depth] = Json::Unicode {
                            digits: digits + 1,
                            code,
                        };
                    } else {
                        self.json[depth] = Json::Text;
                        // Only ASCII can match or shape anything: any other
                        // character stands as one non-ASCII byte.
                        let decoded = u8::try_from(code).ok().filter(u8::is_ascii).unwrap_or(0x80);
                        self.byte(inner, decoded);
                    }
                }
                // No escape after all: the byte is text again.
                None => {
                    self.json[depth] = Json::Text;
                    self.json_byte(depth, b);
                }
            },
        }
    }
}

fn hex(b: u8) -> Option<u8> {
    char::from(b)
        .to_digit(16)
        .and_then(|digit| u8::try_from(digit).ok())
}

/// The byte a JSON escape `\b` stands for.
fn unescape(b: u8) -> u8 {
    match b {
        b'b' => 0x08,
        b'f' => 0x0c,
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        other => other,
    }
}

/// Why a [`MultipartGuard`] failed its body.
#[derive(Debug, thiserror::Error)]
#[error("the request body was refused by the local file scan")]
struct Refused;

/// A multipart request body on its way to a server in local mode. Each
/// frame is scanned before it is passed on, so the frame that completes a
/// local file reference never is, and neither is the closing delimiter
/// after it: the body fails instead, which drops the upstream connection
/// with the request incomplete. Once the scan has read the closing
/// delimiter, the rest passes unread. The verdict goes to the receiver
/// [`MultipartGuard::new`] returns as soon as there is one.
pub(super) struct MultipartGuard {
    body: Body,
    scan: Scan,
    verdict: Option<oneshot::Sender<Verdict>>,
}

enum Scan {
    Scanning(Box<MultipartScan>),
    Clean,
    Refused,
}

impl MultipartGuard {
    pub(super) fn new(body: Body, boundary: &[u8]) -> (MultipartGuard, oneshot::Receiver<Verdict>) {
        let (verdict, receiver) = oneshot::channel();
        let guard = MultipartGuard {
            body,
            scan: Scan::Scanning(Box::new(MultipartScan::new(boundary))),
            verdict: Some(verdict),
        };
        (guard, receiver)
    }

    fn decide(&mut self, verdict: Verdict) {
        self.scan = match verdict {
            Verdict::Clean => Scan::Clean,
            Verdict::Refused(_) => Scan::Refused,
        };
        if let Some(sender) = self.verdict.take() {
            // Nobody waiting: the request is gone already.
            let _ = sender.send(verdict);
        }
    }
}

impl HttpBody for MultipartGuard {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        let scan = match &mut this.scan {
            Scan::Clean => return Pin::new(&mut this.body).poll_frame(cx),
            Scan::Refused => return Poll::Ready(Some(Err(axum::Error::new(Refused)))),
            Scan::Scanning(scan) => scan,
        };
        let polled = Pin::new(&mut this.body).poll_frame(cx);
        let verdict = match &polled {
            Poll::Ready(Some(Ok(frame))) => frame.data_ref().and_then(|data| scan.feed(data)),
            Poll::Ready(None) => Some(scan.finish()),
            Poll::Ready(Some(Err(_))) | Poll::Pending => None,
        };
        match verdict {
            None => polled,
            Some(Verdict::Clean) => {
                this.decide(Verdict::Clean);
                polled
            }
            Some(refused) => {
                this.decide(refused);
                Poll::Ready(Some(Err(axum::Error::new(Refused))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self.scan, Scan::Clean) && self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDARY: &str = "B0und";

    fn multipart_type() -> HeaderValue {
        HeaderValue::from_str(&format!("multipart/form-data; boundary={BOUNDARY}")).unwrap()
    }

    /// A multipart body of `parts` (header lines, value).
    fn multipart(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (headers, value) in parts {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n{headers}\r\n\r\n").as_bytes());
            body.extend_from_slice(value);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    fn text(name: &str) -> String {
        format!("Content-Disposition: form-data; name=\"{name}\"")
    }

    /// The verdict on a multipart body, fed whole and then byte by byte:
    /// both must agree.
    fn multipart_verdict(body: &[u8]) -> Verdict {
        let whole = body_verdict(Some(&multipart_type()), body);
        let mut scan = MultipartScan::new(BOUNDARY.as_bytes());
        let bytewise = body
            .chunks(1)
            .find_map(|byte| scan.feed(byte))
            .unwrap_or_else(|| scan.finish());
        assert_eq!(whole, bytewise, "{}", String::from_utf8_lossy(body));
        whole
    }

    const LOCAL: Verdict = Verdict::Refused(Refusal::LocalFile);
    const UNREADABLE: Verdict = Verdict::Refused(Refusal::Unreadable);

    #[test]
    fn json_strings_scanned_decoded_at_any_depth() {
        for body in [
            r#"{"document":"file:/x"}"#,
            r#"{"a":{"b":[1,{"c":"file:x"}]}}"#,
            r#"{"document":"file:\/x"}"#,
            r#"{"document":"\u0066ile:/x"}"#,
            r#"{"document":"\u0066\u0069\u006C\u0065\u003a/x"}"#,
            r#"{"media":"[{\"media\":\"file:/x\"}]"}"#,
            r#"{"media":" \n[\"file:/x\"]"}"#,
            r#"{"media":"{\"a\":\"\\u0066ile:/x\"}"}"#,
            r#""file:/x""#,
            // The server keeps what it parsed before an error.
            r#"{"document":"file:/x", garbage"#,
            r#"{"document":"file:"#,
        ] {
            assert!(json_has_local_file(body.as_bytes()), "{body}");
        }
        for body in [
            r#"{"text":"see file:/x"}"#,
            r#"{"text":" file:/x"}"#,
            r#"{"text":"File:/x"}"#,
            r#"{"text":"fil"}"#,
            r#"{"text":"file"}"#,
            r#"{"media":"x[\"file:/x\"]"}"#,
            r#"{"text":"\"file:/x\""}"#,
            r#"{"n":1,"b":true,"x":null}"#,
            "",
            "not json at all: file:/x",
        ] {
            assert!(!json_has_local_file(body.as_bytes()), "{body}");
        }
    }

    #[test]
    fn form_values_scanned_decoded() {
        for body in [
            "document=file:/x",
            "document=file%3A%2Fx",
            "document=file%3a",
            "document=%66ile:",
            "a=1&&document=file:/x",
            "document=file:=x",
            "media=%5B%22file%3A%2Fx%22%5D",
            "media=+[\"file:/x\"]",
            r#"media=[{"media":"file:/x"}]"#,
        ] {
            assert!(form_has_local_file(body.as_bytes()), "{body}");
        }
        for body in [
            "document=+file:/x",
            "file:/x",
            "file%3A=x",
            "text=see+file:/x",
            "text=file%3",
            "text=file%zz:",
            "text=fi%6Ce",
            "a=b&c",
            "",
        ] {
            assert!(!form_has_local_file(body.as_bytes()), "{body}");
        }
    }

    /// `levels` JSON documents, each carried in a string of the one
    /// around it, the innermost holding `innermost`.
    fn nested(levels: usize, innermost: &str) -> String {
        let mut document = format!("[{}]", serde_json::to_string(innermost).unwrap());
        for _ in 1..levels {
            document = format!("[{}]", serde_json::to_string(&document).unwrap());
        }
        document
    }

    #[test]
    fn json_in_strings_followed_to_max_depth_then_refused() {
        // A form value is a value at depth 0: JSON documents at depths 0, 1
        // and 2 are read, one at depth 3 is refused unread.
        let form = |document: String| format!("media={document}").into_bytes();
        assert!(!form_has_local_file(&form(nested(3, "clean"))));
        assert!(form_has_local_file(&form(nested(3, "file:/x"))));
        assert!(form_has_local_file(&form(nested(4, "clean"))));
        // A JSON body is a document at depth 0.
        let body = |document: String| {
            format!(
                r#"{{"media":{}}}"#,
                serde_json::to_string(&document).unwrap()
            )
            .into_bytes()
        };
        assert!(!json_has_local_file(&body(nested(2, "clean"))));
        assert!(json_has_local_file(&body(nested(2, "file:/x"))));
        assert!(json_has_local_file(&body(nested(3, "clean"))));
    }

    /// Inputs a recursive parser would choke on take one pass.
    #[test]
    fn adversarial_input_takes_one_pass() {
        let deep = "[".repeat(1 << 20);
        assert!(!json_has_local_file(deep.as_bytes()));
        assert!(!form_has_local_file(format!("a={deep}").as_bytes()));
        let strings = r#"["x",""#.repeat((1 << 20) / 6);
        assert!(!json_has_local_file(strings.as_bytes()));
        let escapes = format!(r#"["{}"]"#, r"\\\u00".repeat((1 << 20) / 6));
        assert!(!json_has_local_file(escapes.as_bytes()));
    }

    #[test]
    fn boundary_read_as_the_server_reads_it() {
        let cases: [(&str, Option<&str>); 15] = [
            ("multipart/form-data; boundary=abc", Some("abc")),
            (
                "multipart/form-data; Boundary=abc; charset=utf-8",
                Some("abc"),
            ),
            ("multipart/form-data; boundary=\"a b\"", Some("a b")),
            ("multipart/form-data; boundary=\"", Some("\"")),
            ("multipart/form-data; xboundary=abc", Some("abc")),
            ("multipart/form-data; boundary = abc", Some(" abc")),
            ("multipart/form-data; boundary=", None),
            ("multipart/form-data", None),
            ("multipart/form-data; boundary", None),
            // The server trims the whole value first (`process_header`),
            // and only the value.
            ("multipart/form-data; boundary=\"abc\" ", Some("abc")),
            ("multipart/form-data; boundary=abc ", Some("abc")),
            ("\t multipart/form-data; boundary=\"abc\"\t ", Some("abc")),
            (
                "multipart/form-data; boundary=\"abc\" ; x=y",
                Some("\"abc\" "),
            ),
            ("multipart/form-data; boundary=abc ; x=y", Some("abc ")),
            ("multipart/form-data; boundary= ", None),
        ];
        for (content_type, expected) in cases {
            assert_eq!(
                boundary(content_type.as_bytes()),
                expected.map(str::as_bytes),
                "{content_type}"
            );
        }
        let longest = format!("multipart/form-data; boundary={}", "b".repeat(70));
        assert_eq!(boundary(longest.as_bytes()).map(<[u8]>::len), Some(70));
        let over = format!("multipart/form-data; boundary={}", "b".repeat(71));
        assert_eq!(boundary(over.as_bytes()), None);
    }

    /// A body whose parts under the server's boundary carry a reference,
    /// wrapped in parts under the boundary an untrimmed `Content-Type`
    /// would give: the scan reads it with the server's.
    #[test]
    fn untrimmed_content_type_scanned_with_the_servers_boundary() {
        let part = |boundary: &str, name: &str, value: &str| {
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
        };
        let body = [
            part("abc", "method", "sendDocument"),
            part("abc", "chat_id", "1"),
            part("abc", "document", "file:///etc/passwd"),
            "--abc--\r\n".to_owned(),
            part("\"abc\" ", "x", "y"),
            "--\"abc\" --\r\n".to_owned(),
        ]
        .concat();
        for content_type in [
            "multipart/form-data; boundary=\"abc\" ",
            "multipart/form-data; boundary=\"abc\"\t",
            " multipart/form-data; boundary=\"abc\"",
        ] {
            let content_type = HeaderValue::from_str(content_type).unwrap();
            assert_eq!(
                body_verdict(Some(&content_type), body.as_bytes()),
                LOCAL,
                "{content_type:?}"
            );
        }
    }

    #[test]
    fn multipart_parts_read_as_the_server_reads_them() {
        let file = "Content-Disposition: form-data; name=\"document\"; filename=\"a.bin\"";
        let cases: Vec<(Vec<u8>, Verdict)> = vec![
            (multipart(&[(&text("thumbnail"), b"file:/x")]), LOCAL),
            (
                multipart(&[(&text("caption"), b"see file:/x")]),
                Verdict::Clean,
            ),
            (multipart(&[(file, b"file:/x")]), Verdict::Clean),
            (
                multipart(&[(&text("media"), br#"[{"media":"file:/x"}]"#)]),
                LOCAL,
            ),
            // `filename` inside a quoted parameter: no file name, a text.
            (
                multipart(&[(
                    "Content-Disposition: form-data; name=thumbnail; x=\"; filename=a\"",
                    b"file:/x",
                )]),
                LOCAL,
            ),
            // An empty file name is a file name.
            (
                multipart(&[(
                    "Content-Disposition: form-data; name=\"thumbnail\"; filename=\"\"",
                    b"file:/x",
                )]),
                Verdict::Clean,
            ),
            // A folded header, and a name in a later header.
            (
                multipart(&[(
                    "Content-Disposition: form-data;\r\n name=\"thumbnail\"",
                    b"file:/x",
                )]),
                LOCAL,
            ),
            (
                multipart(&[(
                    "Content-Disposition: form-data; name=\"\"\r\ncontent-disposition: form-data; name=x",
                    b"file:/x",
                )]),
                LOCAL,
            ),
            // Urlencoded parameters in a part; the type is matched as the
            // server matches it.
            (
                multipart(&[(
                    &format!(
                        "{}\r\nContent-Type: application/x-www-form-urlencoded",
                        text("p")
                    ),
                    b"a=1&document=file%3A%2Fx",
                )]),
                LOCAL,
            ),
            (
                multipart(&[(
                    &format!(
                        "{}\r\nContent-Type: Application/X-WWW-Form-Urlencoded",
                        text("p")
                    ),
                    b"document=file%3A%2Fx",
                )]),
                Verdict::Clean,
            ),
            // Delimiter look-alikes inside values are content.
            (
                multipart(&[(
                    &text("media"),
                    format!("[\"\r\n--{}\",\"file:/x\"]", &BOUNDARY[..4]).as_bytes(),
                )]),
                LOCAL,
            ),
            (
                multipart(&[
                    (
                        file,
                        format!("\r\n--{}\r\n--{}", &BOUNDARY[..4], &BOUNDARY[..3]).as_bytes(),
                    ),
                    (&text("thumbnail"), b"file:/x"),
                ]),
                LOCAL,
            ),
            // The prologue and the epilogue are no parameters.
            (
                [
                    b"file:/x\r\n".as_slice(),
                    &multipart(&[(&text("a"), b"1")]),
                    b"file:/x",
                ]
                .concat(),
                Verdict::Clean,
            ),
            // Part header names and values trimmed, names in any case, as
            // the server reads them.
            (
                multipart(&[(
                    " CONTENT-disposition \t:\t form-data; name=thumbnail \t",
                    b"file:/x",
                )]),
                LOCAL,
            ),
            (
                multipart(&[(
                    &format!(
                        "{}\r\nContent-Type: \t application/x-www-form-urlencoded; charset=utf-8 ",
                        text("p")
                    ),
                    b"document=file%3A%2Fx",
                )]),
                LOCAL,
            ),
            // The disposition's `form-data;` is matched in its case.
            (
                multipart(&[("Content-Disposition: Form-Data; name=thumbnail", b"file:/x")]),
                UNREADABLE,
            ),
            // Bodies the server refuses.
            (
                multipart(&[("Content-Disposition: form-data; filename=a", b"1")]),
                UNREADABLE,
            ),
            (multipart(&[("Content-Type: text/plain", b"1")]), UNREADABLE),
            (
                multipart(&[("Content-Disposition: form-data", b"1")]),
                UNREADABLE,
            ),
            (
                multipart(&[("Content-Disposition: form-data; name=\"a", b"1")]),
                UNREADABLE,
            ),
            (multipart(&[("no colon here", b"1")]), UNREADABLE),
            (
                format!("--{BOUNDARY}\r\n\r\nvalue").into_bytes(),
                UNREADABLE,
            ),
            (
                format!("--{BOUNDARY}\r\n{}\r\n\r\nno end", text("a")).into_bytes(),
                UNREADABLE,
            ),
            (Vec::new(), UNREADABLE),
        ];
        for (body, expected) in cases {
            assert_eq!(
                multipart_verdict(&body),
                expected,
                "{}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    #[test]
    fn multipart_text_over_the_parameter_bound_refused() {
        let long = vec![b'a'; MAX_PARAMETERS];
        let body = multipart(&[(&text("a"), &long), (&text("b"), b"1")]);
        assert_eq!(
            body_verdict(Some(&multipart_type()), &body),
            Verdict::Refused(Refusal::TooLarge)
        );
        let file = "Content-Disposition: form-data; name=\"f\"; filename=\"f\"";
        let body = multipart(&[(file, &long), (file, &long), (&text("b"), b"1")]);
        assert_eq!(body_verdict(Some(&multipart_type()), &body), Verdict::Clean);
    }

    /// Polls every frame of `guard` until it ends or fails: the data passed
    /// on, and whether it failed.
    async fn drain(mut guard: MultipartGuard) -> (Vec<u8>, bool) {
        let mut passed = Vec::new();
        while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut guard).poll_frame(cx)).await
        {
            match frame {
                Ok(frame) => passed.extend_from_slice(frame.data_ref().expect("data")),
                Err(_) => return (passed, true),
            }
        }
        (passed, false)
    }

    fn frames(frames: &[&[u8]]) -> Body {
        let frames: Vec<Result<Bytes, std::io::Error>> = frames
            .iter()
            .map(|frame| Ok(Bytes::copy_from_slice(frame)))
            .collect();
        Body::from_stream(futures_util::stream::iter(frames))
    }

    #[tokio::test]
    async fn guard_withholds_the_frame_that_completes_a_reference() {
        let head = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"document\"; filename=\"a\"\r\n\r\n0123"
        );
        let late = format!("\r\n--{BOUNDARY}\r\n{}\r\n\r\nfi", text("thumbnail"));
        let closing = format!("\r\n--{BOUNDARY}--\r\n");
        let body = frames(&[
            head.as_bytes(),
            late.as_bytes(),
            b"le:/x",
            closing.as_bytes(),
        ]);
        let (guard, mut verdict) = MultipartGuard::new(body, BOUNDARY.as_bytes());
        let (passed, failed) = drain(guard).await;
        assert!(failed);
        assert_eq!(passed, [head.as_bytes(), late.as_bytes()].concat());
        assert_eq!(verdict.try_recv(), Ok(LOCAL));

        let ended_early = frames(&[head.as_bytes()]);
        let (guard, mut verdict) = MultipartGuard::new(ended_early, BOUNDARY.as_bytes());
        let (passed, failed) = drain(guard).await;
        assert!(failed);
        assert_eq!(passed, head.as_bytes());
        assert_eq!(verdict.try_recv(), Ok(UNREADABLE));
    }

    #[tokio::test]
    async fn guard_passes_a_clean_body_whole() {
        let body = multipart(&[
            (&text("caption"), b"see file:/x"),
            (
                "Content-Disposition: form-data; name=\"document\"; filename=\"a\"",
                b"file:/x",
            ),
        ]);
        let (start, end) = body.split_at(body.len() / 2);
        let epilogue = b"file:/x, unread";
        let (guard, mut verdict) =
            MultipartGuard::new(frames(&[start, end, epilogue]), BOUNDARY.as_bytes());
        let (passed, failed) = drain(guard).await;
        assert!(!failed);
        assert_eq!(passed, [&body[..], epilogue].concat());
        assert_eq!(verdict.try_recv(), Ok(Verdict::Clean));
    }
}
