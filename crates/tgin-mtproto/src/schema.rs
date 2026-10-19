//! The schemas the decoder and the TL-JSON codec walk: static tables that
//! `build.rs` generates from the vendored `.tl` files, plus the strict byte
//! reader both use.
//!
//! - [`API`]: `schema/api-230.tl`, layer 230, the layer of grammers 0.11
//!   and so the one layer every account serves.
//! - [`MTPROTO`]: `schema/mtproto.tl`, the transport schema.
//!
//! Both files are byte copies of grammers-tl-types 0.11.0's, under a header.
//!
//! The reader is strict, so a decode that succeeds re-encodes to the same
//! bytes: flag bits no field uses, non-canonical `string`/`bytes` lengths,
//! non-zero padding, a `Bool` other than `boolTrue`/`boolFalse` and a
//! vector longer than its bytes are refused. Unknown flag bits are refused,
//! not dropped: the server encodes for the layer the account serves, so a
//! bit the schema does not name means the bytes are not that layer's.

/// Nesting deeper than this many objects and vectors is refused, so a
/// hostile input cannot exhaust the stack. Real Telegram objects nest a few
/// tens deep at most; serde_json's own limit is 128.
pub(crate) const MAX_DEPTH: u32 = 100;

pub(crate) const VECTOR_ID: u32 = 0x1cb5_c415;
pub(crate) const BOOL_TRUE: u32 = 0x9972_75b5;
pub(crate) const BOOL_FALSE: u32 = 0xbc79_9737;

/// Why TL bytes or TL-JSON were refused. The messages name schema items and
/// byte offsets only, never content, so they are safe to log.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TlError {
    #[error("unexpected end of TL at byte {at}")]
    Eof { at: usize },
    #[error("trailing bytes after the object at byte {at}")]
    Trailing { at: usize },
    #[error("unknown constructor 0x{id:08x} at byte {at}")]
    UnknownConstructor { id: u32, at: usize },
    #[error("{got} where a {want} is expected")]
    WrongType {
        want: &'static str,
        got: &'static str,
    },
    /// A flags word has bits no field of the constructor uses.
    #[error("{ctor}.{word} has flag bits 0x{bits:08x} no field uses")]
    UnusedFlagBits {
        ctor: &'static str,
        word: &'static str,
        bits: u32,
    },
    /// A length in its long form for a short value, a length prefix TL does
    /// not define, non-zero padding, or a vector count larger than its bytes.
    #[error("non-canonical {what} at byte {at}")]
    NonCanonical { what: &'static str, at: usize },
    #[error("objects nest deeper than {MAX_DEPTH}")]
    TooDeep,
    /// TL-JSON that is not valid JSON.
    #[error("TL-JSON syntax error at line {line}, column {column}")]
    JsonSyntax { line: usize, column: usize },
    /// Valid JSON that is not TL-JSON for the schema. `ctor` and `field`
    /// name where (empty when not inside an object), `what` the rule.
    #[error("TL-JSON {ctor}.{field}: {what}")]
    Json {
        ctor: &'static str,
        field: &'static str,
        what: &'static str,
    },
}

/// One schema: every constructor and function, looked up by id or by name.
pub struct Schema {
    /// The `// LAYER n` of the file (0 for mtproto.tl, which has none).
    pub layer: i32,
    pub(crate) ctors: &'static [Ctor],
    /// `(id, index into ctors)`, sorted by id.
    by_id: &'static [(u32, u16)],
    /// `(full name, index into ctors)`, sorted by name.
    by_name: &'static [(&'static str, u16)],
    /// Boxed types, sorted by name; `Ty::Boxed` and `Ctor::ty` index it.
    pub(crate) types: &'static [TypeDef],
}

pub(crate) struct TypeDef {
    pub name: &'static str,
    /// Its constructors, as indices into `Schema::ctors` (the test
    /// generator picks from them).
    #[cfg_attr(not(test), allow(dead_code))]
    pub ctors: &'static [u16],
}

pub(crate) struct Ctor {
    pub id: u32,
    /// Full name, e.g. `messages.sendMessage`.
    pub name: &'static str,
    /// The boxed type (index into `Schema::types`); `None` for a function.
    pub ty: Option<u16>,
    /// A function's result type; `None` for a constructor.
    pub result: Option<Ty>,
    /// The update box an `Update` or `Updates` constructor belongs to.
    pub seq: Seq,
    pub params: &'static [Param],
}

/// Which update box a constructor's `pts`/`qts` counts in, by its fields:
/// a `qts` field is the secondary box; a `pts` with a `channel_id` field
/// or `Channel` in the name is a channel's; any other `pts` is common.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seq {
    None,
    Common,
    Qts,
    Channel,
}

pub(crate) struct Param {
    pub name: &'static str,
    pub kind: Kind,
    pub role: Role,
}

pub(crate) enum Kind {
    /// A flags word. `slot` is its position among the constructor's flags
    /// words; `known` the bits its fields use.
    Flags {
        slot: u8,
        known: u32,
    },
    Field {
        ty: Ty,
        flag: Option<Flag>,
    },
}

/// A field present only when bit `bit` of flags word `slot` is set.
#[derive(Clone, Copy)]
pub(crate) struct Flag {
    pub slot: u8,
    pub bit: u8,
}

/// The most flags words one constructor has (`flags` and `flags2`).
pub(crate) const MAX_FLAG_WORDS: usize = 2;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ty {
    Int,
    Long,
    Double,
    Str,
    Bytes,
    Int128,
    Int256,
    Bool,
    True,
    /// `Vector<T>`: id, count, items.
    Vector(&'static Ty),
    /// `vector<T>`: count, items.
    BareVector(&'static Ty),
    /// A boxed type (index into `Schema::types`): id, then fields.
    Boxed(u16),
    /// A bare constructor (index into `Schema::ctors`): fields only.
    Bare(u16),
    /// `!X` or `Object`: any boxed object of the schema.
    Any,
}

/// The part a field plays for [`crate::decode::facts`], assigned by
/// `build.rs` from the field's name and type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    None,
    /// `pts:int`
    Pts,
    /// `pts_count:int`
    PtsCount,
    /// `qts:int`
    Qts,
    /// `channel_id:long`
    ChannelId,
    /// `chat_id:long`
    ChatId,
    /// `user_id:long`
    UserId,
    /// `peer:Peer` or `peer_id:Peer`
    Peer,
    /// `from_id:Peer`
    From,
    /// `from_id:long` (`updateShortChatMessage`)
    FromUser,
    /// `message:Message`
    Message,
    /// `message:string`
    Text,
    /// `update:Update` (`updateShort`)
    Inner,
    /// `out:flags.N?true`
    Out,
}

/// The api schema, layer 230.
pub static API: Schema = include!(concat!(env!("OUT_DIR"), "/schema_api.rs"));

/// The transport schema (mtproto.tl).
pub static MTPROTO: Schema = include!(concat!(env!("OUT_DIR"), "/schema_mtproto.rs"));

impl Schema {
    pub(crate) fn ctor_by_id(&self, id: u32) -> Option<&'static Ctor> {
        let i = self.by_id.binary_search_by_key(&id, |&(k, _)| k).ok()?;
        self.ctors.get(usize::from(self.by_id[i].1))
    }

    pub(crate) fn ctor_by_name(&self, name: &str) -> Option<&'static Ctor> {
        let i = self.by_name.binary_search_by_key(&name, |&(k, _)| k).ok()?;
        self.ctors.get(usize::from(self.by_name[i].1))
    }

    pub(crate) fn ctor(&self, idx: u16) -> &'static Ctor {
        &self.ctors[usize::from(idx)]
    }

    pub(crate) fn type_def(&self, idx: u16) -> &'static TypeDef {
        &self.types[usize::from(idx)]
    }

    pub(crate) fn type_index(&self, name: &str) -> Option<u16> {
        self.types
            .binary_search_by_key(&name, |t| t.name)
            .ok()
            .and_then(|i| u16::try_from(i).ok())
    }

    /// Reads a boxed object's id and its constructor. With `want`, the
    /// constructor must be of that boxed type (not a function).
    pub(crate) fn read_ctor(
        &self,
        r: &mut Reader<'_>,
        want: Option<u16>,
    ) -> Result<&'static Ctor, TlError> {
        let at = r.pos;
        let id = r.u32()?;
        let ctor = self
            .ctor_by_id(id)
            .ok_or(TlError::UnknownConstructor { id, at })?;
        if let Some(want) = want
            && ctor.ty != Some(want)
        {
            return Err(TlError::WrongType {
                want: self.type_def(want).name,
                got: ctor.name,
            });
        }
        Ok(ctor)
    }

    /// Skips one boxed object, checking it as strictly as a decode does.
    pub(crate) fn skip_boxed(
        &self,
        r: &mut Reader<'_>,
        want: Option<u16>,
        depth: u32,
    ) -> Result<&'static Ctor, TlError> {
        let ctor = self.read_ctor(r, want)?;
        self.skip_fields(r, ctor, depth)?;
        Ok(ctor)
    }

    /// Skips a constructor's fields (its id already read).
    pub(crate) fn skip_fields(
        &self,
        r: &mut Reader<'_>,
        ctor: &'static Ctor,
        depth: u32,
    ) -> Result<(), TlError> {
        let depth = deeper(depth)?;
        let mut words = [0u32; MAX_FLAG_WORDS];
        for p in ctor.params {
            match &p.kind {
                Kind::Flags { slot, known } => {
                    words[usize::from(*slot)] = r.flags(ctor, p, *known)?;
                }
                Kind::Field { ty, flag } => {
                    if present(&words, *flag) {
                        self.skip_value(r, ty, depth)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn skip_value(
        &self,
        r: &mut Reader<'_>,
        ty: &'static Ty,
        depth: u32,
    ) -> Result<(), TlError> {
        match ty {
            Ty::Int => r.take(4).map(drop),
            Ty::Long | Ty::Double => r.take(8).map(drop),
            Ty::Int128 => r.take(16).map(drop),
            Ty::Int256 => r.take(32).map(drop),
            Ty::Str | Ty::Bytes => r.tl_bytes().map(drop),
            Ty::Bool => r.bool().map(drop),
            Ty::True => Ok(()),
            Ty::Vector(t) => {
                r.vector_id()?;
                self.skip_items(r, t, depth)
            }
            Ty::BareVector(t) => self.skip_items(r, t, depth),
            Ty::Boxed(t) => self.skip_boxed(r, Some(*t), depth).map(drop),
            Ty::Bare(c) => self.skip_fields(r, self.ctor(*c), depth),
            Ty::Any => self.skip_boxed(r, None, depth).map(drop),
        }
    }

    fn skip_items(&self, r: &mut Reader<'_>, t: &'static Ty, depth: u32) -> Result<(), TlError> {
        let depth = deeper(depth)?;
        let n = r.count(self.min_size(t))?;
        for _ in 0..n {
            self.skip_value(r, t, depth)?;
        }
        Ok(())
    }

    /// The fewest bytes one value of `ty` takes, at least 1: it bounds a
    /// vector's count by the bytes left.
    pub(crate) fn min_size(&self, ty: &Ty) -> usize {
        match ty {
            Ty::Int | Ty::Str | Ty::Bytes | Ty::Bool | Ty::Boxed(_) | Ty::Any => 4,
            Ty::Long | Ty::Double | Ty::Vector(_) => 8,
            Ty::Int128 => 16,
            Ty::Int256 => 32,
            Ty::BareVector(_) => 4,
            // A bare constructor or `true` may take no bytes; 1 still bounds
            // the count by the input's length.
            Ty::True | Ty::Bare(_) => 1,
        }
    }
}

/// Whether a field with `flag` is present under the flags words read so far.
pub(crate) fn present(words: &[u32; MAX_FLAG_WORDS], flag: Option<Flag>) -> bool {
    flag.is_none_or(|f| words[usize::from(f.slot)] & (1 << f.bit) != 0)
}

pub(crate) fn deeper(depth: u32) -> Result<u32, TlError> {
    if depth >= MAX_DEPTH {
        Err(TlError::TooDeep)
    } else {
        Ok(depth + 1)
    }
}

/// A strict little-endian TL reader over borrowed bytes; never allocates.
pub(crate) struct Reader<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// `Trailing` unless every byte was read.
    pub(crate) fn finish(&self) -> Result<(), TlError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(TlError::Trailing { at: self.pos })
        }
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], TlError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or(TlError::Eof { at: self.pos })?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn arr<const N: usize>(&mut self) -> Result<[u8; N], TlError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, TlError> {
        self.arr().map(u32::from_le_bytes)
    }

    pub(crate) fn i32(&mut self) -> Result<i32, TlError> {
        self.arr().map(i32::from_le_bytes)
    }

    pub(crate) fn i64(&mut self) -> Result<i64, TlError> {
        self.arr().map(i64::from_le_bytes)
    }

    pub(crate) fn u64(&mut self) -> Result<u64, TlError> {
        self.arr().map(u64::from_le_bytes)
    }

    pub(crate) fn bool(&mut self) -> Result<bool, TlError> {
        let at = self.pos;
        match self.u32()? {
            BOOL_TRUE => Ok(true),
            BOOL_FALSE => Ok(false),
            id => Err(TlError::UnknownConstructor { id, at }),
        }
    }

    pub(crate) fn vector_id(&mut self) -> Result<(), TlError> {
        let at = self.pos;
        match self.u32()? {
            VECTOR_ID => Ok(()),
            id => Err(TlError::UnknownConstructor { id, at }),
        }
    }

    /// A vector's count, refused when negative or when `min_item` bytes per
    /// item would not fit in what is left.
    pub(crate) fn count(&mut self, min_item: usize) -> Result<usize, TlError> {
        let at = self.pos;
        let n = self.i32()?;
        usize::try_from(n)
            .ok()
            .filter(|&n| n.saturating_mul(min_item) <= self.remaining())
            .ok_or(TlError::NonCanonical {
                what: "vector length",
                at,
            })
    }

    /// A flags word, refused when it has bits outside `known`.
    pub(crate) fn flags(
        &mut self,
        ctor: &'static Ctor,
        p: &'static Param,
        known: u32,
    ) -> Result<u32, TlError> {
        let w = self.u32()?;
        let unused = w & !known;
        if unused != 0 {
            return Err(TlError::UnusedFlagBits {
                ctor: ctor.name,
                word: p.name,
                bits: unused,
            });
        }
        Ok(w)
    }

    /// TL `string`/`bytes`, strict: the short form for lengths below 254,
    /// never the 255 prefix, zero padding to 4.
    pub(crate) fn tl_bytes(&mut self) -> Result<&'a [u8], TlError> {
        let at = self.pos;
        let first = self.take(1)?[0];
        let (len, head) = match first {
            254 => {
                let l = self.take(3)?;
                let len = usize::from(l[0]) | usize::from(l[1]) << 8 | usize::from(l[2]) << 16;
                if len < 254 {
                    return Err(TlError::NonCanonical {
                        what: "string length",
                        at,
                    });
                }
                (len, 4)
            }
            255 => {
                return Err(TlError::NonCanonical {
                    what: "string length",
                    at,
                });
            }
            n => (usize::from(n), 1),
        };
        let data = self.take(len)?;
        let pad = (4 - (head + len) % 4) % 4;
        let pad_at = self.pos;
        if self.take(pad)?.iter().any(|&b| b != 0) {
            return Err(TlError::NonCanonical {
                what: "padding",
                at: pad_at,
            });
        }
        Ok(data)
    }
}

/// TL `string`/`bytes` encoding: short (< 254) or long length prefix, zero
/// padding to 4. `None` for 16 MiB and more, which TL cannot carry.
#[must_use]
pub(crate) fn write_tl_bytes(out: &mut Vec<u8>, b: &[u8]) -> Option<()> {
    let len = b.len();
    let head = if len < 254 {
        out.push(len as u8);
        1
    } else if len < 1 << 24 {
        out.push(254);
        out.extend_from_slice(&(len as u32).to_le_bytes()[..3]);
        4
    } else {
        return None;
    };
    out.extend_from_slice(b);
    let pad = (4 - (head + len) % 4) % 4;
    out.extend(std::iter::repeat_n(0u8, pad));
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_consistent() {
        for s in [&API, &MTPROTO] {
            assert!(s.by_id.windows(2).all(|w| w[0].0 < w[1].0));
            assert!(s.by_name.windows(2).all(|w| w[0].0 < w[1].0));
            assert!(s.types.windows(2).all(|w| w[0].name < w[1].name));
            assert_eq!(s.by_id.len(), s.ctors.len());
            for c in s.ctors {
                assert_eq!(s.ctor_by_id(c.id).map(|k| k.name), Some(c.name));
                assert_eq!(s.ctor_by_name(c.name).map(|k| k.id), Some(c.id));
            }
        }
        assert_eq!(API.layer, 230);
    }

    /// The vendored api schema is grammers' compiled layer: same layer, same
    /// name for every id, every id of grammers' dispatch covered.
    #[test]
    fn schema_is_grammers_compiled_layer() {
        assert_eq!(API.layer, grammers_tl_types::LAYER);
        for s in [&API, &MTPROTO] {
            let mismatched: Vec<String> = s
                .ctors
                .iter()
                .filter(|c| grammers_tl_types::name_for_id(c.id) != c.name)
                .map(|c| format!("0x{:08x} {}", c.id, c.name))
                .collect();
            assert!(mismatched.is_empty(), "{mismatched:?}");
        }
        let ids = |s: &Schema| -> Vec<u32> {
            let mut v: Vec<u32> = s.ctors.iter().map(|c| c.id).collect();
            v.sort_unstable();
            v
        };
        let sorted = |c: &[u32]| {
            let mut v = c.to_vec();
            v.sort_unstable();
            v
        };
        assert_eq!(ids(&API), sorted(crate::testgen::typed::COVERED_API));
        assert_eq!(
            ids(&MTPROTO),
            sorted(crate::testgen::typed::COVERED_MTPROTO)
        );
        assert_eq!(
            API.ctors.len(),
            2553,
            "api.tl layer 230: 1703 types, 850 functions"
        );
    }

    /// The body of each vendored file, below its header, is grammers-tl-types
    /// 0.11.0's file byte for byte.
    #[test]
    fn vendored_files_have_their_header() {
        for (text, file) in [
            (include_str!("../schema/api-230.tl"), "tl/api.tl"),
            (include_str!("../schema/mtproto.tl"), "tl/mtproto.tl"),
        ] {
            let first = text.lines().next().unwrap_or_default();
            assert_eq!(
                first,
                format!("// Vendored from grammers-tl-types 0.11.0 (crates.io), file {file}.")
            );
        }
    }

    #[test]
    fn strict_reader_rules() {
        // A short string in the long form.
        let mut r = Reader::new(&[254, 1, 0, 0, b'x', 0, 0, 0]);
        assert!(matches!(
            r.tl_bytes(),
            Err(TlError::NonCanonical {
                what: "string length",
                ..
            })
        ));
        // The 255 prefix.
        let mut r = Reader::new(&[255, 0, 0, 0]);
        assert!(matches!(r.tl_bytes(), Err(TlError::NonCanonical { .. })));
        // Non-zero padding.
        let mut r = Reader::new(&[1, b'x', 0, 9]);
        assert!(matches!(
            r.tl_bytes(),
            Err(TlError::NonCanonical {
                what: "padding",
                ..
            })
        ));
        // The long form at its lower bound is canonical.
        let mut b = Vec::new();
        assert!(write_tl_bytes(&mut b, &[7u8; 254]).is_some());
        assert_eq!(&b[..4], &[254, 254, 0, 0]);
        assert_eq!(Reader::new(&b).tl_bytes().map(<[u8]>::len), Ok(254));
        // A count no input can hold.
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0x7f]);
        assert!(matches!(
            r.count(4),
            Err(TlError::NonCanonical {
                what: "vector length",
                ..
            })
        ));
        let minus_one = (-1i32).to_le_bytes();
        let mut r = Reader::new(&minus_one);
        assert!(r.count(4).is_err());
    }
}
