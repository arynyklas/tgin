//! The parameter grammar of telegram-bot-api's HTTP server, ported from
//! TDLib's `HttpReader` so that the local file scan reads a request as the
//! server does: header values trimmed (`process_header`), the multipart
//! boundary (`do_read_next`), the multipart body and its part headers
//! (`parse_multipart_form_data`, `find_boundary`), urlencoded parameters
//! (`parse_parameters`, `url_decode`), and `is_space` and `trim`.
//!
//! Ported from TDLib at `bc9c263e2bfee06aaab41e82db51a103376030bc`, the
//! `td` submodule of telegram-bot-api `e3e9dd8`:
//! `tdnet/td/net/HttpReader.cpp`, `tdnet/td/net/HttpReader.h`,
//! `tdutils/td/utils/find_boundary.cpp`, `tdutils/td/utils/misc.cpp`,
//! `tdutils/td/utils/misc.h` and `tdutils/td/utils/Parser.h`. On a server
//! image bump, diff those functions at the new pinned commit against this
//! file (AGENTS.md, "Maintaining … on a Bot API release").
//!
//! Copyright Aliaksei Levin (levlam@telegram.org), Arseny Smirnov (arseny30@gmail.com) 2014-2026
//!
//! Distributed under the Boost Software License, Version 1.0. (See LICENSES/BSL-1.0.txt)

use super::{Refusal, Values, Verdict, hex};

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
pub(in crate::gateway) const MAX_PARAMETERS: usize = 1 << 20;

/// The boundary of a multipart `Content-Type` as the server takes it: the
/// whole value trimmed (`process_header`), then after the first `boundary`
/// in any case, the `=` after it, up to a `;`, one pair of quotes around it
/// dropped. `None` where the server answers 400.
pub(in crate::gateway) fn boundary(content_type: &[u8]) -> Option<&[u8]> {
    const NAME: &[u8] = b"boundary";
    let content_type = trim(content_type);
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

/// Where an urlencoded text's scan is: pairs split on `&`, the name from
/// the value on the first `=`; values, `+` as a space and percent-decoded
/// (a `%` without two hex digits stays), are values at depth 0.
#[derive(Clone, Copy, Default)]
pub(super) enum Form {
    #[default]
    Name,
    Value,
    /// After a `%` in a value.
    Percent,
    /// After a `%` and one hex digit.
    PercentDigit(u8),
}

impl Form {
    pub(super) fn byte(&mut self, values: &mut Values, b: u8) {
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
    pub(super) fn end(&mut self, values: &mut Values) {
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
pub(super) fn is_space(b: u8) -> bool {
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
