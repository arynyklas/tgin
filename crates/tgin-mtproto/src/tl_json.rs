//! The TL-JSON codec of design §10.5, driven by the layer's schema
//! ([`crate::schema::API`]).
//!
//! Mapping: `"_"` = constructor name, then the fields in schema order;
//! `int` → number; `double` → number, and NaN or ±Inf, which JSON numbers
//! cannot carry, → `{"@double": "<16 lowercase hex digits of the IEEE-754
//! bits>"}`; `long` → decimal string (a request may also give a safe
//! integer, |n| ≤ 2^53 − 1); `string` → string, invalid UTF-8 →
//! `{"@base64": "…"}`; `bytes` → base64; `int128`/`int256` → lowercase hex;
//! `Bool` → boolean; `true` flags → `true`, or omitted (a request may give
//! `false`); absent flagged fields omitted (a request may give `null`);
//! flags words never emitted; `Vector` → array.
//!
//! Decoding TL is strict (see [`crate::schema`]), so every [`to_json`] that
//! succeeds re-encodes through [`from_json`] to the same bytes. Doubles are
//! written in Rust's shortest round-trip form and read with Rust's
//! correctly rounded parser, so a double survives exactly (S11 F2: without
//! exact parsing 327 constructors failed the round trip).
//!
//! Neither direction builds a `serde_json::Value`: [`to_json`] writes text
//! straight from the bytes, and [`from_json`] reads borrowed
//! `serde_json::value::RawValue`s, field by field in schema order.

use std::borrow::Cow;
use std::fmt::{self, Write as _};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;

use crate::schema::{
    API, BOOL_FALSE, BOOL_TRUE, Ctor, Kind, MAX_FLAG_WORDS, Reader, Schema, TlError, Ty, VECTOR_ID,
    deeper, present, write_tl_bytes,
};

/// Largest integer a JS double holds exactly (2^53 − 1).
const MAX_SAFE: i64 = (1 << 53) - 1;

/// One boxed object of the api schema (a type or a function) → TL-JSON,
/// appended to `out`. On error `out` is left as it was.
pub fn to_json(bytes: &[u8], out: &mut String) -> Result<(), TlError> {
    API.to_json(bytes, out)
}

/// TL-JSON of one boxed object of the api schema → its TL bytes, appended
/// to `out`. On error `out` is left as it was.
pub fn from_json(json: &str, out: &mut Vec<u8>) -> Result<(), TlError> {
    API.from_json(json, out)
}

impl Schema {
    /// [`to_json`] for this schema.
    pub fn to_json(&self, bytes: &[u8], out: &mut String) -> Result<(), TlError> {
        let start = out.len();
        let mut r = Reader::new(bytes);
        let res = self
            .json_boxed(&mut r, out, None, 0)
            .and_then(|()| r.finish());
        if res.is_err() {
            out.truncate(start);
        }
        res
    }

    /// [`from_json`] for this schema.
    pub fn from_json(&self, json: &str, out: &mut Vec<u8>) -> Result<(), TlError> {
        let start = out.len();
        let res = parse::<&RawValue>(json).and_then(|v| self.tl_boxed(out, v, None, 0, ("", "")));
        if res.is_err() {
            out.truncate(start);
        }
        res
    }

    // ---- TL → JSON ---------------------------------------------------

    fn json_boxed(
        &self,
        r: &mut Reader<'_>,
        out: &mut String,
        want: Option<u16>,
        depth: u32,
    ) -> Result<(), TlError> {
        let ctor = self.read_ctor(r, want)?;
        self.json_fields(r, out, ctor, depth)
    }

    fn json_fields(
        &self,
        r: &mut Reader<'_>,
        out: &mut String,
        ctor: &'static Ctor,
        depth: u32,
    ) -> Result<(), TlError> {
        let depth = deeper(depth)?;
        out.push_str("{\"_\":");
        json_str(out, ctor.name);
        let mut words = [0u32; MAX_FLAG_WORDS];
        for p in ctor.params {
            match &p.kind {
                Kind::Flags { slot, known } => {
                    words[usize::from(*slot)] = r.flags(ctor, p, *known)?;
                }
                Kind::Field { ty, flag } => {
                    if !present(&words, *flag) {
                        continue;
                    }
                    out.push(',');
                    json_str(out, p.name);
                    out.push(':');
                    self.json_value(r, out, ty, depth)?;
                }
            }
        }
        out.push('}');
        Ok(())
    }

    fn json_value(
        &self,
        r: &mut Reader<'_>,
        out: &mut String,
        ty: &'static Ty,
        depth: u32,
    ) -> Result<(), TlError> {
        match ty {
            Ty::Int => write_display(out, r.i32()?),
            Ty::Long => {
                out.push('"');
                write_display(out, r.i64()?);
                out.push('"');
            }
            Ty::Double => {
                let x = f64::from_bits(r.u64()?);
                if x.is_finite() {
                    // Debug: the shortest digits that read back to the same
                    // bits, with an exponent for very large or small values;
                    // always a valid JSON number when finite.
                    let _ = write!(out, "{x:?}");
                } else {
                    out.push_str("{\"@double\":\"");
                    let _ = write!(out, "{:016x}", x.to_bits());
                    out.push_str("\"}");
                }
            }
            Ty::Str => {
                let b = r.tl_bytes()?;
                match std::str::from_utf8(b) {
                    Ok(s) => json_str(out, s),
                    Err(_) => {
                        out.push_str("{\"@base64\":\"");
                        B64.encode_string(b, out);
                        out.push_str("\"}");
                    }
                }
            }
            Ty::Bytes => {
                out.push('"');
                B64.encode_string(r.tl_bytes()?, out);
                out.push('"');
            }
            Ty::Int128 => hex(out, r.take(16)?),
            Ty::Int256 => hex(out, r.take(32)?),
            Ty::Bool => out.push_str(if r.bool()? { "true" } else { "false" }),
            Ty::True => out.push_str("true"),
            Ty::Vector(t) => {
                r.vector_id()?;
                self.json_items(r, out, t, depth)?;
            }
            Ty::BareVector(t) => self.json_items(r, out, t, depth)?,
            Ty::Boxed(t) => self.json_boxed(r, out, Some(*t), depth)?,
            Ty::Bare(c) => self.json_fields(r, out, self.ctor(*c), depth)?,
            Ty::Any => self.json_boxed(r, out, None, depth)?,
        }
        Ok(())
    }

    fn json_items(
        &self,
        r: &mut Reader<'_>,
        out: &mut String,
        t: &'static Ty,
        depth: u32,
    ) -> Result<(), TlError> {
        let depth = deeper(depth)?;
        let n = r.count(self.min_size(t))?;
        out.push('[');
        for i in 0..n {
            if i > 0 {
                out.push(',');
            }
            self.json_value(r, out, t, depth)?;
        }
        out.push(']');
        Ok(())
    }

    // ---- JSON → TL ---------------------------------------------------

    fn tl_boxed(
        &self,
        out: &mut Vec<u8>,
        v: &RawValue,
        want: Option<u16>,
        depth: u32,
        at: At,
    ) -> Result<(), TlError> {
        let fields = object(v, at)?;
        let name = fields
            .get("_")
            .ok_or(json_err(at, "an object needs \"_\""))?;
        let name = string(name, at)?;
        let ctor = self
            .ctor_by_name(&name)
            .ok_or(json_err(at, "unknown constructor name"))?;
        if let Some(want) = want
            && ctor.ty != Some(want)
        {
            return Err(TlError::WrongType {
                want: self.type_def(want).name,
                got: ctor.name,
            });
        }
        out.extend_from_slice(&ctor.id.to_le_bytes());
        self.tl_fields(out, ctor, &fields, depth)
    }

    fn tl_fields(
        &self,
        out: &mut Vec<u8>,
        ctor: &'static Ctor,
        fields: &Fields<'_>,
        depth: u32,
    ) -> Result<(), TlError> {
        let depth = deeper(depth)?;
        for (k, _) in &fields.0 {
            let known = k == "_"
                || ctor
                    .params
                    .iter()
                    .any(|p| p.name == k && matches!(p.kind, Kind::Field { .. }));
            if !known {
                return Err(json_err(
                    (ctor.name, ""),
                    "a key the constructor has no field for",
                ));
            }
        }
        // A flagged field is written when its bit is set, and its bit is set
        // when any field on that bit is given: fields that share a bit
        // (`views` and `forwards` of `message`) come together or not at all.
        let mut words = [0u32; MAX_FLAG_WORDS];
        for p in ctor.params {
            let at = (ctor.name, p.name);
            match &p.kind {
                Kind::Flags { slot, .. } => {
                    let mut w = 0u32;
                    for q in ctor.params {
                        if let Kind::Field { ty, flag: Some(f) } = &q.kind
                            && f.slot == *slot
                            && is_set(fields.get(q.name), ty, (ctor.name, q.name))?
                        {
                            w |= 1 << f.bit;
                        }
                    }
                    words[usize::from(*slot)] = w;
                    out.extend_from_slice(&w.to_le_bytes());
                }
                Kind::Field { ty, flag } => {
                    if !present(&words, *flag) || *ty == Ty::True {
                        continue;
                    }
                    let v = fields
                        .get(p.name)
                        .filter(|v| v.get() != "null")
                        .ok_or(json_err(at, "a required field is missing"))?;
                    self.tl_value(out, ty, v, depth, at)?;
                }
            }
        }
        Ok(())
    }

    fn tl_value(
        &self,
        out: &mut Vec<u8>,
        ty: &'static Ty,
        v: &RawValue,
        depth: u32,
        at: At,
    ) -> Result<(), TlError> {
        match ty {
            Ty::Int => {
                let n = number(v)
                    .and_then(|t| t.parse::<i32>().ok())
                    .ok_or(json_err(
                        at,
                        "an int must be an integer number in the int range",
                    ))?;
                out.extend_from_slice(&n.to_le_bytes());
            }
            Ty::Long => {
                let n = match number(v) {
                    Some(t) => t.parse::<i64>().ok().filter(|n| n.abs() <= MAX_SAFE),
                    None => string(v, at)
                        .ok()
                        .filter(|s| !s.starts_with('+'))
                        .and_then(|s| s.parse::<i64>().ok()),
                }
                .ok_or(json_err(
                    at,
                    "a long must be a decimal string or a safe integer",
                ))?;
                out.extend_from_slice(&n.to_le_bytes());
            }
            Ty::Double => {
                let x = match number(v) {
                    // Rust's parser is correctly rounded; overflow to ±Inf
                    // is refused, since that needs `@double`.
                    Some(t) => t.parse::<f64>().ok().filter(|x| x.is_finite()),
                    None => escaped(v, "@double", at)?
                        .filter(|h| h.len() == 16 && h.bytes().all(is_lower_hex))
                        .and_then(|h| u64::from_str_radix(&h, 16).ok())
                        .map(f64::from_bits),
                }
                .ok_or(json_err(
                    at,
                    "a double must be a finite number or {\"@double\": 16 hex digits}",
                ))?;
                out.extend_from_slice(&x.to_bits().to_le_bytes());
            }
            Ty::Str => {
                let bytes: Cow<'_, [u8]> = match string(v, at) {
                    Ok(Cow::Borrowed(s)) => Cow::Borrowed(s.as_bytes()),
                    Ok(Cow::Owned(s)) => Cow::Owned(s.into_bytes()),
                    Err(_) => {
                        let b = escaped(v, "@base64", at)?
                            .and_then(|b| B64.decode(b.as_bytes()).ok())
                            .ok_or(json_err(
                                at,
                                "a string must be a string or {\"@base64\": base64}",
                            ))?;
                        Cow::Owned(b)
                    }
                };
                write_tl_bytes(out, &bytes).ok_or(json_err(at, "16 MiB or longer"))?;
            }
            Ty::Bytes => {
                let b = string(v, at)
                    .ok()
                    .and_then(|s| B64.decode(s.as_bytes()).ok())
                    .ok_or(json_err(at, "bytes must be a base64 string"))?;
                write_tl_bytes(out, &b).ok_or(json_err(at, "16 MiB or longer"))?;
            }
            Ty::Int128 => unhex(out, v, 16, at)?,
            Ty::Int256 => unhex(out, v, 32, at)?,
            Ty::Bool => {
                let b = boolean(v).ok_or(json_err(at, "a Bool must be a boolean"))?;
                out.extend_from_slice(&if b { BOOL_TRUE } else { BOOL_FALSE }.to_le_bytes());
            }
            Ty::True => {}
            Ty::Vector(t) => {
                out.extend_from_slice(&VECTOR_ID.to_le_bytes());
                self.tl_items(out, t, v, depth, at)?;
            }
            Ty::BareVector(t) => self.tl_items(out, t, v, depth, at)?,
            Ty::Boxed(t) => self.tl_boxed(out, v, Some(*t), depth, at)?,
            Ty::Bare(c) => {
                let ctor = self.ctor(*c);
                let fields = object(v, at)?;
                // A bare object may name itself; nothing else.
                if let Some(name) = fields.get("_")
                    && string(name, at)? != ctor.name
                {
                    return Err(json_err(at, "a bare object names another constructor"));
                }
                self.tl_fields(out, ctor, &fields, depth)?;
            }
            Ty::Any => self.tl_boxed(out, v, None, depth, at)?,
        }
        Ok(())
    }

    fn tl_items(
        &self,
        out: &mut Vec<u8>,
        t: &'static Ty,
        v: &RawValue,
        depth: u32,
        at: At,
    ) -> Result<(), TlError> {
        let depth = deeper(depth)?;
        let items: Vec<&RawValue> =
            parse(v.get()).map_err(|_| json_err(at, "a vector must be an array"))?;
        let n = i32::try_from(items.len()).map_err(|_| json_err(at, "vector too long"))?;
        out.extend_from_slice(&n.to_le_bytes());
        for item in items {
            self.tl_value(out, t, item, depth, at)?;
        }
        Ok(())
    }
}

/// `(constructor, field)` an error points at.
type At = (&'static str, &'static str);

fn json_err(at: At, what: &'static str) -> TlError {
    TlError::Json {
        ctor: at.0,
        field: at.1,
        what,
    }
}

fn parse<'a, T: Deserialize<'a>>(json: &'a str) -> Result<T, TlError> {
    serde_json::from_str(json).map_err(|e| TlError::JsonSyntax {
        line: e.line(),
        column: e.column(),
    })
}

/// A flagged field's presence: absent, `null` and a `false` `true` flag
/// are unset.
fn is_set(v: Option<&RawValue>, ty: &Ty, at: At) -> Result<bool, TlError> {
    let Some(v) = v else { return Ok(false) };
    if v.get() == "null" {
        return Ok(false);
    }
    if *ty == Ty::True {
        return boolean(v).ok_or(json_err(at, "a true flag must be a boolean"));
    }
    Ok(true)
}

/// An object's members in order, keys borrowed when unescaped; duplicate
/// keys are refused.
struct Fields<'a>(Vec<(Cow<'a, str>, &'a RawValue)>);

impl<'a> Fields<'a> {
    fn get(&self, key: &str) -> Option<&'a RawValue> {
        self.0.iter().find(|(k, _)| k == key).map(|&(_, v)| v)
    }
}

impl<'de> Deserialize<'de> for Fields<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Fields<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Fields<'de>, A::Error> {
                let mut v: Vec<(Cow<'de, str>, &'de RawValue)> = Vec::new();
                while let Some(Str(k)) = m.next_key::<Str<'de>>()? {
                    if v.iter().any(|(e, _)| *e == k) {
                        return Err(de::Error::custom("duplicate key"));
                    }
                    v.push((k, m.next_value()?));
                }
                Ok(Fields(v))
            }
        }
        d.deserialize_map(V)
    }
}

/// A JSON string, borrowed when it has no escapes.
struct Str<'a>(Cow<'a, str>);

impl<'de> Deserialize<'de> for Str<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Str<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_borrowed_str<E: de::Error>(self, s: &'de str) -> Result<Str<'de>, E> {
                Ok(Str(Cow::Borrowed(s)))
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Str<'de>, E> {
                Ok(Str(Cow::Owned(s.to_owned())))
            }
            fn visit_string<E: de::Error>(self, s: String) -> Result<Str<'de>, E> {
                Ok(Str(Cow::Owned(s)))
            }
        }
        d.deserialize_str(V)
    }
}

fn object<'a>(v: &'a RawValue, at: At) -> Result<Fields<'a>, TlError> {
    parse(v.get()).map_err(|_| json_err(at, "an object is expected"))
}

fn string<'a>(v: &'a RawValue, at: At) -> Result<Cow<'a, str>, TlError> {
    if !v.get().starts_with('"') {
        return Err(json_err(at, "a string is expected"));
    }
    parse::<Str<'a>>(v.get()).map(|s| s.0)
}

fn boolean(v: &RawValue) -> Option<bool> {
    match v.get() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// A JSON number's text: RawValue holds valid JSON, so a value that starts
/// with `-` or a digit is a number.
fn number(v: &RawValue) -> Option<&str> {
    let t = v.get();
    t.starts_with(|c: char| c == '-' || c.is_ascii_digit())
        .then_some(t)
}

/// `{"<key>": "<s>"}` and nothing else → `Some(s)`; another object → `None`.
fn escaped<'a>(v: &'a RawValue, key: &str, at: At) -> Result<Option<Cow<'a, str>>, TlError> {
    if !v.get().starts_with('{') {
        return Ok(None);
    }
    let f = object(v, at)?;
    match f.0.as_slice() {
        [(k, s)] if k == key => string(s, at).map(Some),
        _ => Ok(None),
    }
}

fn is_lower_hex(c: u8) -> bool {
    c.is_ascii_digit() || (b'a'..=b'f').contains(&c)
}

fn unhex(out: &mut Vec<u8>, v: &RawValue, len: usize, at: At) -> Result<(), TlError> {
    let bad = || {
        json_err(
            at,
            "int128/int256 must be lowercase hex of their exact length",
        )
    };
    let s = string(v, at).map_err(|_| bad())?;
    if s.len() != len * 2 || !s.bytes().all(is_lower_hex) {
        return Err(bad());
    }
    let digit = |c: u8| if c <= b'9' { c - b'0' } else { c - b'a' + 10 };
    let (pairs, _) = s.as_bytes().as_chunks::<2>();
    out.extend(pairs.iter().map(|&[hi, lo]| digit(hi) << 4 | digit(lo)));
    Ok(())
}

fn write_display(out: &mut String, v: impl fmt::Display) {
    let _ = write!(out, "{v}");
}

fn hex(out: &mut String, b: &[u8]) {
    const D: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    for &x in b {
        out.push(char::from(D[usize::from(x >> 4)]));
        out.push(char::from(D[usize::from(x & 15)]));
    }
    out.push('"');
}

/// A JSON string literal: `"` and `\` escaped, control characters as
/// `\n`, `\r`, `\t`, `\b`, `\f` or `\u00XX`.
fn json_str(out: &mut String, s: &str) {
    out.push('"');
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let esc = match c {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            '\u{8}' => "\\b",
            '\u{c}' => "\\f",
            c if c < ' ' => "",
            _ => continue,
        };
        out.push_str(&s[start..i]);
        if esc.is_empty() {
            let _ = write!(out, "\\u{:04x}", u32::from(c));
        } else {
            out.push_str(esc);
        }
        start = i + c.len_utf8();
    }
    out.push_str(&s[start..]);
    out.push('"');
}

#[cfg(test)]
mod tests;
