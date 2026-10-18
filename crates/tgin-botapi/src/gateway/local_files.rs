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
//! `application/x-www-form-urlencoded` as more parameters.
//!
//! Every scan is one pass over the bytes through small state machines. It
//! does not recurse on JSON structure, so nesting costs nothing; it follows
//! JSON carried in strings at most [`MAX_DEPTH`] values deep, and refuses a
//! JSON document found deeper. The only allocation is a multipart part's
//! header block. Nothing here logs: a value is a path on the server.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use http_body::{Body as HttpBody, Frame, SizeHint};
use tgin_core::config::{Upstream, UpstreamKind};
use tokio::sync::oneshot;

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
/// The longest multipart boundary the server takes (`HttpReader`'s
/// `MAX_BOUNDARY_LENGTH`).
const MAX_BOUNDARY_LEN: usize = 70;
/// `\r\n--` and the longest boundary.
const MAX_DELIMITER_LEN: usize = MAX_BOUNDARY_LEN + 4;
/// The part header blocks of a multipart body, together (`HttpReader`'s
/// `MAX_TOTAL_HEADERS_LENGTH`).
const MAX_PART_HEADERS: usize = 1 << 18;
/// The server's bound on its text parameters (`HttpReader`'s
/// `MAX_TOTAL_PARAMETERS_LENGTH`): the longest JSON or urlencoded body tgin
/// reads to scan, and the most multipart text it scans (ruling R2).
pub(super) const MAX_PARAMETERS: usize = 1 << 20;

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

/// The boundary of a multipart `Content-Type` as the server takes it: after
/// the first `boundary` in any case, then the `=` after it, up to a `;`,
/// one pair of quotes around it dropped. `None` where the server answers
/// 400.
pub(super) fn boundary(content_type: &[u8]) -> Option<&[u8]> {
    const NAME: &[u8] = b"boundary";
    let after = content_type
        .windows(NAME.len())
        .position(|window| window.eq_ignore_ascii_case(NAME))?
        + NAME.len();
    let start = after + content_type[after..].iter().position(|&b| b == b'=')? + 1;
    let end = content_type[start..]
        .iter()
        .position(|&b| b == b';')
        .map_or(content_type.len(), |semicolon| start + semicolon);
    let mut boundary = &content_type[start..end];
    if let [b'"', inner @ .., b'"'] = boundary {
        boundary = inner;
    }
    (!boundary.is_empty() && boundary.len() <= MAX_BOUNDARY_LEN).then_some(boundary)
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

/// Where an urlencoded text's scan is: pairs split on `&`, the name from
/// the value on the first `=`; values, `+` as a space and percent-decoded
/// (a `%` without two hex digits stays), are values at depth 0.
#[derive(Clone, Copy, Default)]
enum Form {
    #[default]
    Name,
    Value,
    /// After a `%` in a value.
    Percent,
    /// After a `%` and one hex digit.
    PercentDigit(u8),
}

impl Form {
    fn byte(&mut self, values: &mut Values, b: u8) {
        match *self {
            Form::Name => {
                if b == b'=' {
                    *self = Form::Value;
                    values.start(0);
                }
            }
            Form::Value => self.value_byte(values, b),
            Form::Percent => {
                if hex(b).is_some() {
                    *self = Form::PercentDigit(b);
                } else {
                    *self = Form::Value;
                    values.byte(0, b'%');
                    self.value_byte(values, b);
                }
            }
            Form::PercentDigit(first) => {
                *self = Form::Value;
                match (hex(first), hex(b)) {
                    (Some(high), Some(low)) => values.byte(0, high << 4 | low),
                    _ => {
                        values.byte(0, b'%');
                        values.byte(0, first);
                        self.value_byte(values, b);
                    }
                }
            }
        }
    }

    fn value_byte(&mut self, values: &mut Values, b: u8) {
        match b {
            b'&' => *self = Form::Name,
            b'%' => *self = Form::Percent,
            b'+' => values.byte(0, b' '),
            _ => values.byte(0, b),
        }
    }

    /// The text ended: a `%` it ended on is part of the last value.
    fn end(&mut self, values: &mut Values) {
        match std::mem::take(self) {
            Form::Percent => values.byte(0, b'%'),
            Form::PercentDigit(first) => {
                values.byte(0, b'%');
                values.byte(0, first);
            }
            Form::Name | Form::Value => {}
        }
    }
}

/// A multipart delimiter searched for byte by byte (Knuth–Morris–Pratt),
/// so one split across frames is found, and the bytes before it are
/// released as content as soon as they can no longer begin it.
struct Delimiter {
    pattern: [u8; MAX_DELIMITER_LEN],
    /// For each prefix of `pattern`, the length of its longest proper
    /// prefix that is also its suffix.
    fail: [u8; MAX_DELIMITER_LEN],
    len: usize,
    /// Bytes of `pattern` the input ends with.
    matched: usize,
}

impl Delimiter {
    /// `prefix` then `boundary`, at most [`MAX_DELIMITER_LEN`] bytes.
    fn new(prefix: &[u8], boundary: &[u8]) -> Delimiter {
        let mut pattern = [0u8; MAX_DELIMITER_LEN];
        let len = prefix.len() + boundary.len();
        pattern[..prefix.len()].copy_from_slice(prefix);
        pattern[prefix.len()..len].copy_from_slice(boundary);
        let mut fail = [0u8; MAX_DELIMITER_LEN];
        let mut k = 0;
        for i in 1..len {
            while k > 0 && pattern[i] != pattern[k] {
                k = usize::from(fail[k - 1]);
            }
            if pattern[i] == pattern[k] {
                k += 1;
            }
            // At most MAX_DELIMITER_LEN.
            fail[i] = u8::try_from(k).unwrap_or(u8::MAX);
        }
        Delimiter {
            pattern,
            fail,
            len,
            matched: 0,
        }
    }

    /// Feeds `b`, handing the bytes now known to be content to `content`;
    /// `true` when `b` completed the delimiter.
    fn step(&mut self, b: u8, mut content: impl FnMut(u8)) -> bool {
        loop {
            if self.pattern[self.matched] == b {
                self.matched += 1;
                if self.matched == self.len {
                    self.matched = 0;
                    return true;
                }
                return false;
            }
            if self.matched == 0 {
                content(b);
                return false;
            }
            let kept = usize::from(self.fail[self.matched - 1]);
            for &c in &self.pattern[..self.matched - kept] {
                content(c);
            }
            self.matched = kept;
        }
    }

    /// How many leading bytes of `data` cannot begin the delimiter while
    /// nothing of it is matched.
    fn skippable(&self, data: &[u8]) -> usize {
        if self.matched > 0 {
            return 0;
        }
        data.iter()
            .position(|&b| b == self.pattern[0])
            .unwrap_or(data.len())
    }
}

/// How the server reads a part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A parameter.
    Text,
    /// A parameter typed `application/x-www-form-urlencoded`: parameters.
    Form,
    /// A file: never read as a parameter.
    File,
}

/// Where a multipart body's scan is (`HttpReader::parse_multipart_form_data`).
#[derive(Clone, Copy)]
enum State {
    /// Before the first `--<boundary>`, anywhere.
    Prologue,
    /// A part's header block, up to `\r\n\r\n`.
    Headers,
    /// A part's content, up to `\r\n--<boundary>`.
    Part(Kind),
    /// After a delimiter: `--` ends the body, anything else begins the
    /// next part's header block. The first byte when it arrived.
    Last(Option<u8>),
}

/// The multipart grammar of the server's `HttpReader`, fed the body in
/// pieces, scanning every parameter.
pub(super) struct MultipartScan {
    state: State,
    /// `--<boundary>`, which ends the prologue.
    first: Delimiter,
    /// `\r\n--<boundary>`, which ends a part.
    next: Delimiter,
    /// The bytes after the last delimiter, up to the `\r\n\r\n` that ends
    /// the header block.
    headers: Vec<u8>,
    headers_total: usize,
    /// Text parameter bytes so far.
    text_total: usize,
    /// A text part as one value.
    values: Values,
    /// A form part as parameters.
    form: Form,
    form_values: Values,
    verdict: Option<Verdict>,
}

impl MultipartScan {
    pub(super) fn new(boundary: &[u8]) -> MultipartScan {
        let boundary = &boundary[..boundary.len().min(MAX_BOUNDARY_LEN)];
        MultipartScan {
            state: State::Prologue,
            first: Delimiter::new(b"--", boundary),
            next: Delimiter::new(b"\r\n--", boundary),
            headers: Vec::new(),
            headers_total: 0,
            text_total: 0,
            values: Values::default(),
            form: Form::default(),
            form_values: Values::default(),
            verdict: None,
        }
    }

    /// Feeds the next bytes of the body; the verdict once there is one,
    /// which nothing after it changes: [`Verdict::Clean`] once the closing
    /// delimiter is read, the rest being the epilogue the server skips.
    pub(super) fn feed(&mut self, mut data: &[u8]) -> Option<Verdict> {
        while self.verdict.is_none() {
            let skipped = match self.state {
                State::Prologue => self.first.skippable(data),
                State::Part(Kind::File) => self.next.skippable(data),
                _ => 0,
            };
            data = &data[skipped..];
            let Some((&b, rest)) = data.split_first() else {
                break;
            };
            data = rest;
            self.byte(b);
        }
        self.verdict
    }

    /// The body ended: without its closing delimiter, the server refuses it.
    pub(super) fn finish(&mut self) -> Verdict {
        *self
            .verdict
            .get_or_insert(Verdict::Refused(Refusal::Unreadable))
    }

    fn refuse(&mut self, refusal: Refusal) {
        self.verdict = Some(Verdict::Refused(refusal));
    }

    fn byte(&mut self, b: u8) {
        match self.state {
            State::Prologue => {
                if self.first.step(b, |_| {}) {
                    self.headers.clear();
                    self.state = State::Headers;
                }
            }
            State::Headers => self.header_byte(b),
            State::Part(kind) => self.part_byte(kind, b),
            State::Last(None) => self.state = State::Last(Some(b)),
            State::Last(Some(first)) => {
                if first == b'-' && b == b'-' {
                    self.verdict = Some(Verdict::Clean);
                } else {
                    self.headers.clear();
                    self.state = State::Headers;
                    self.header_byte(first);
                    if self.verdict.is_none() {
                        self.header_byte(b);
                    }
                }
            }
        }
    }

    /// A byte of a header block: the bytes after a delimiter up to
    /// `\r\n\r\n`, of which the server parses all but the first two.
    fn header_byte(&mut self, b: u8) {
        self.headers.push(b);
        if self.headers_total + self.headers.len() > MAX_PART_HEADERS + 4 {
            self.refuse(Refusal::Unreadable);
            return;
        }
        if !self.headers.ends_with(b"\r\n\r\n") {
            return;
        }
        let len = self.headers.len() - 4;
        self.headers_total += len;
        let kind = match len {
            0 => None,
            _ => part_kind(&self.headers[2..len + 2]),
        };
        let Some(kind) = kind else {
            self.refuse(Refusal::Unreadable);
            return;
        };
        self.values.start(0);
        self.form = Form::default();
        self.state = State::Part(kind);
    }

    fn part_byte(&mut self, kind: Kind, b: u8) {
        let values = &mut self.values;
        let form = &mut self.form;
        let form_values = &mut self.form_values;
        let text_total = &mut self.text_total;
        let ended = self.next.step(b, |c| {
            if kind == Kind::File {
                return;
            }
            *text_total += 1;
            values.byte(0, c);
            if kind == Kind::Form {
                form.byte(form_values, c);
            }
        });
        if ended && kind == Kind::Form {
            self.form.end(&mut self.form_values);
        }
        if self.values.found || self.form_values.found {
            self.refuse(Refusal::LocalFile);
        } else if self.text_total > MAX_PARAMETERS {
            self.refuse(Refusal::TooLarge);
        } else if ended {
            self.state = State::Last(None);
        }
    }
}

/// How the server reads a part with the header block `block`: a file when
/// a `Content-Disposition` has a `filename` parameter, else a parameter,
/// read as more parameters when its `Content-Type` begins with
/// `application/x-www-form-urlencoded`. `None` where the server answers
/// 400. A port of `HttpReader`'s header parsing: a header's value runs to a
/// CR LF that no space or tab follows, trimmed; names ignore case.
fn part_kind(block: &[u8]) -> Option<Kind> {
    let mut named = false;
    let mut file = false;
    let mut content_type: &[u8] = b"application/octet-stream";
    let mut rest = block;
    while !rest.is_empty() {
        let colon = rest.iter().position(|&b| b == b':')?;
        let name = trim(&rest[..colon]);
        let line = &rest[colon + 1..];
        let mut end;
        let mut next = 0;
        loop {
            end = next + line[next..].iter().position(|&b| b == b'\r')?;
            if line.get(end + 1) != Some(&b'\n') {
                return None;
            }
            next = end + 2;
            if !matches!(line.get(next), Some(b' ' | b'\t')) {
                break;
            }
        }
        let value = trim(&line[..end]);
        rest = &line[next..];
        if name.eq_ignore_ascii_case(b"content-disposition") {
            disposition(value, &mut named, &mut file)?;
        } else if name.eq_ignore_ascii_case(b"content-type") {
            content_type = value;
        }
    }
    if !named {
        return None;
    }
    Some(if file {
        Kind::File
    } else if content_type.starts_with(b"application/x-www-form-urlencoded") {
        Kind::Form
    } else {
        Kind::Text
    })
}

/// Reads a `Content-Disposition` value as the server does: `form-data;`
/// then `key=value` pairs, a value quoted (`\` escaping the next byte) or
/// up to a `;`. A `name` sets whether the part is named (the last one
/// counts), any `filename` makes it a file. `None` where the server answers
/// 400.
fn disposition(value: &[u8], named: &mut bool, file: &mut bool) -> Option<()> {
    let mut rest = value.strip_prefix(b"form-data;")?;
    loop {
        rest = trim(rest);
        let Some(equals) = rest.iter().position(|&b| b == b'=') else {
            return Some(());
        };
        let key = trim(&rest[..equals]);
        rest = trim_start(&rest[equals + 1..]);
        let empty;
        if rest.first() == Some(&b'"') {
            let mut at = 1;
            let mut chars = 0;
            loop {
                let c = *rest.get(at)?;
                at += 1;
                if c == b'"' {
                    break;
                }
                if c == b'\\' {
                    rest.get(at)?;
                    at += 1;
                }
                chars += 1;
            }
            empty = chars == 0;
            rest = trim_start(&rest[at..]);
            if let Some((&first, after)) = rest.split_first() {
                if first != b';' {
                    return None;
                }
                rest = after;
            }
        } else {
            match rest.iter().position(|&b| b == b';') {
                Some(semicolon) => {
                    empty = trim(&rest[..semicolon]).is_empty();
                    rest = &rest[semicolon + 1..];
                }
                None => {
                    empty = trim(rest).is_empty();
                    rest = &[];
                }
            }
        }
        if key == b"name" {
            *named = !empty;
        } else if key == b"filename" {
            *file = true;
        }
    }
}

/// td's `is_space`.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\0' | 0x0b)
}

fn trim_start(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|&b| !is_space(b))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

/// td's `trim`.
fn trim(bytes: &[u8]) -> &[u8] {
    let bytes = trim_start(bytes);
    let end = bytes
        .iter()
        .rposition(|&b| !is_space(b))
        .map_or(0, |last| last + 1);
    &bytes[..end]
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
        let cases: [(&str, Option<&str>); 9] = [
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
