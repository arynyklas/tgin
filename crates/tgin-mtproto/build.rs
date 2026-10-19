//! Turns the vendored schemas (`schema/api-230.tl`, `schema/mtproto.tl`)
//! into static tables that `src/schema.rs` includes: one `Schema` value per
//! file, so the decoder and the TL-JSON codec need no parsing, no lock and
//! no allocation at run time.
//!
//! It also writes the test-only code that sends bytes through grammers'
//! own typed (de)serializers (`typed_*.rs`), included only by `cfg(test)`
//! modules. Rust names follow grammers-tl-gen 0.11.0's `rustifier`, whose
//! naming rule is private there and repeated below.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::{env, fs};

use grammers_tl_parser::parse_tl_file;
use grammers_tl_parser::tl::{Category, Definition, ParameterType, Type};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=schema/api-230.tl");
    println!("cargo:rerun-if-changed=schema/mtproto.tl");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    let api = fs::read_to_string("schema/api-230.tl").expect("read schema/api-230.tl");
    let api_layer = find_layer(&api).expect("schema/api-230.tl has no `// LAYER n` line");
    let api = Built::new(&api);
    fs::write(out.join("schema_api.rs"), api.emit(api_layer)).expect("write schema_api.rs");

    let mtproto = fs::read_to_string("schema/mtproto.tl").expect("read schema/mtproto.tl");
    let mtproto = Built::new(&mtproto);
    fs::write(out.join("schema_mtproto.rs"), mtproto.emit(0)).expect("write schema_mtproto.rs");

    let mut typed = String::new();
    api.typed_dispatch(&mut typed, "api");
    mtproto.typed_dispatch(&mut typed, "mtproto");
    api.update_oracle(&mut typed);
    fs::write(out.join("typed.rs"), typed).expect("write typed.rs");
}

/// The `// LAYER n` comment, read as grammers-tl-types' build script does.
fn find_layer(tl: &str) -> Option<i32> {
    tl.lines().find_map(|l| {
        l.trim()
            .strip_prefix("//")
            .and_then(|s| s.split_once("LAYER"))
            .and_then(|(_, n)| n.trim().parse().ok())
    })
}

fn full_name(namespace: &[String], name: &str) -> String {
    let mut s = String::new();
    for ns in namespace {
        s.push_str(ns);
        s.push('.');
    }
    s.push_str(name);
    s
}

/// One parsed schema plus the indices the tables refer to.
struct Built {
    /// Same filter as grammers-tl-types' build script: unparsable lines (the
    /// generic `vector` and mtproto.tl's primitive declarations) are dropped.
    defs: Vec<Definition>,
    /// Boxed type name → its constructors (types only), sorted by name.
    types: BTreeMap<String, Vec<usize>>,
    by_name: HashMap<String, usize>,
}

impl Built {
    fn new(tl: &str) -> Self {
        let defs: Vec<Definition> = parse_tl_file(tl).filter_map(Result::ok).collect();
        let mut types: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut by_name = HashMap::new();
        let mut ids = HashMap::new();
        for (i, d) in defs.iter().enumerate() {
            assert!(
                by_name.insert(d.full_name(), i).is_none(),
                "duplicate name {}",
                d.full_name()
            );
            assert!(ids.insert(d.id, i).is_none(), "duplicate id {:08x}", d.id);
            assert!(i < usize::from(u16::MAX), "too many constructors");
            if d.category == Category::Types {
                types
                    .entry(full_name(&d.ty.namespace, &d.ty.name))
                    .or_default()
                    .push(i);
            }
        }
        Built {
            defs,
            types,
            by_name,
        }
    }

    fn type_index(&self, name: &str) -> usize {
        self.types
            .keys()
            .position(|k| k == name)
            .unwrap_or_else(|| panic!("no constructor for type {name}"))
    }

    /// A field type as a `Ty` expression.
    fn ty(&self, ty: &Type) -> String {
        if ty.generic_ref {
            return "Ty::Any".into();
        }
        let arg = || {
            let a = ty
                .generic_arg
                .as_deref()
                .unwrap_or_else(|| panic!("{ty} without an argument"));
            self.ty(a)
        };
        match (ty.namespace.is_empty(), ty.name.as_str()) {
            (true, "int") => "Ty::Int".into(),
            (true, "long") => "Ty::Long".into(),
            (true, "double") => "Ty::Double".into(),
            (true, "string") => "Ty::Str".into(),
            (true, "bytes") => "Ty::Bytes".into(),
            (true, "int128") => "Ty::Int128".into(),
            (true, "int256") => "Ty::Int256".into(),
            (true, "Bool") => "Ty::Bool".into(),
            (true, "true") => "Ty::True".into(),
            (true, "Object") => "Ty::Any".into(),
            (true, "Vector") => format!("Ty::Vector(&{})", arg()),
            (true, "vector") => format!("Ty::BareVector(&{})", arg()),
            _ => {
                let name = full_name(&ty.namespace, &ty.name);
                if ty.bare {
                    // A bare constructor by name, else the one constructor of
                    // a boxed type written bare (`%Message`).
                    let idx = self.by_name.get(&name).copied().or_else(|| {
                        let cap = {
                            let mut c = name.clone();
                            if let Some(f) = c.get_mut(..1) {
                                f.make_ascii_uppercase();
                            }
                            c
                        };
                        match self.types.get(&cap).map(Vec::as_slice) {
                            Some([one]) => Some(*one),
                            _ => None,
                        }
                    });
                    let idx = idx.unwrap_or_else(|| panic!("no bare constructor {name}"));
                    format!("Ty::Bare({idx})")
                } else {
                    format!("Ty::Boxed({})", self.type_index(&name))
                }
            }
        }
    }

    /// A function's result type: `Ty::Any` for a generic `X` (the
    /// `invoke*` wrappers) or a type with no constructor in this schema.
    fn result_ty(&self, ty: &Type) -> String {
        if self.resolvable(ty) {
            self.ty(ty)
        } else {
            "Ty::Any".into()
        }
    }

    fn resolvable(&self, ty: &Type) -> bool {
        if ty.generic_ref {
            return false;
        }
        match ty.generic_arg.as_deref() {
            Some(arg) => ty.namespace.is_empty() && ty.name == "Vector" && self.resolvable(arg),
            None => {
                (ty.namespace.is_empty()
                    && matches!(
                        ty.name.as_str(),
                        "int"
                            | "long"
                            | "double"
                            | "string"
                            | "bytes"
                            | "int128"
                            | "int256"
                            | "Bool"
                    ))
                    || (!ty.bare && self.types.contains_key(&full_name(&ty.namespace, &ty.name)))
            }
        }
    }

    fn emit(&self, layer: i32) -> String {
        let mut s = String::new();
        let type_names: Vec<&String> = self.types.keys().collect();
        writeln!(s, "Schema {{").ok();
        writeln!(s, "    layer: {layer},").ok();
        writeln!(s, "    ctors: &[").ok();
        for d in &self.defs {
            let name = d.full_name();
            let ty_name = full_name(&d.ty.namespace, &d.ty.name);
            let ty = if d.category == Category::Types {
                format!("Some({})", self.type_index(&ty_name))
            } else {
                "None".into()
            };
            let result = if d.category == Category::Types {
                "None".to_owned()
            } else {
                format!("Some({})", self.result_ty(&d.ty))
            };
            let mut slots: Vec<&str> = Vec::new();
            let mut params = String::new();
            let has = |n: &str| d.params.iter().any(|p| p.name == n);
            for p in &d.params {
                let (kind, role) = match &p.ty {
                    ParameterType::Flags => {
                        let slot = slots.len();
                        slots.push(&p.name);
                        let known = d
                            .params
                            .iter()
                            .filter_map(|q| match &q.ty {
                                ParameterType::Normal { flag: Some(f), .. } if f.name == p.name => {
                                    Some(1u32 << f.index)
                                }
                                _ => None,
                            })
                            .fold(0, |a, b| a | b);
                        (
                            format!("Kind::Flags {{ slot: {slot}, known: 0x{known:08x} }}"),
                            "None",
                        )
                    }
                    ParameterType::Normal { ty, flag } => {
                        let flag = match flag {
                            None => "None".to_owned(),
                            Some(f) => {
                                let slot = slots
                                    .iter()
                                    .position(|w| *w == f.name)
                                    .unwrap_or_else(|| panic!("{name}.{} flag read late", p.name));
                                assert!(f.index < 32, "{name}.{} bit {}", p.name, f.index);
                                format!("Some(Flag {{ slot: {slot}, bit: {} }})", f.index)
                            }
                        };
                        (
                            format!("Kind::Field {{ ty: {}, flag: {flag} }}", self.ty(ty)),
                            role(&p.name, ty),
                        )
                    }
                };
                writeln!(
                    params,
                    "            Param {{ name: {:?}, kind: {kind}, role: Role::{role} }},",
                    p.name
                )
                .ok();
            }
            assert!(slots.len() <= 2, "{name} has more than 2 flag words");
            let seq = if d.category != Category::Types
                || !(ty_name == "Update" || ty_name == "Updates")
            {
                "None"
            } else if has("qts") {
                "Qts"
            } else if has("pts") && (has("channel_id") || name.contains("Channel")) {
                "Channel"
            } else if has("pts") {
                "Common"
            } else {
                "None"
            };
            writeln!(
                s,
                "        Ctor {{ id: 0x{:08x}, name: {name:?}, ty: {ty}, result: {result}, seq: Seq::{seq}, params: &[\n{params}        ] }},",
                d.id
            )
            .ok();
        }
        writeln!(s, "    ],").ok();

        let mut by_id: Vec<(u32, usize)> = self
            .defs
            .iter()
            .enumerate()
            .map(|(i, d)| (d.id, i))
            .collect();
        by_id.sort_unstable();
        writeln!(s, "    by_id: &[").ok();
        for (id, i) in by_id {
            writeln!(s, "        (0x{id:08x}, {i}),").ok();
        }
        writeln!(s, "    ],").ok();

        let mut by_name: Vec<(String, usize)> = self
            .defs
            .iter()
            .enumerate()
            .map(|(i, d)| (d.full_name(), i))
            .collect();
        by_name.sort_unstable();
        writeln!(s, "    by_name: &[").ok();
        for (n, i) in by_name {
            writeln!(s, "        ({n:?}, {i}),").ok();
        }
        writeln!(s, "    ],").ok();

        writeln!(s, "    types: &[").ok();
        for n in type_names {
            let ctors: Vec<String> = self.types[n].iter().map(usize::to_string).collect();
            writeln!(
                s,
                "        TypeDef {{ name: {n:?}, ctors: &[{}] }},",
                ctors.join(", ")
            )
            .ok();
        }
        writeln!(s, "    ],").ok();
        writeln!(s, "}}").ok();
        s
    }

    /// Test-only: `typed_roundtrip_{label}` sends a boxed object (a type or a
    /// function, id first) through grammers' typed value and back to bytes.
    fn typed_dispatch(&self, s: &mut String, label: &str) {
        let mut type_arms = String::new();
        let mut function_arms = String::new();
        let mut covered = String::new();
        for d in &self.defs {
            writeln!(covered, "    0x{:08x},", d.id).ok();
            match d.category {
                Category::Types => {
                    // grammers special-cases `Bool` as Rust `bool` (no enum).
                    if d.ty.name == "Bool" && d.ty.namespace.is_empty() {
                        writeln!(
                            type_arms,
                            "        0x{:08x} => bool::from_bytes(raw).map(|v| v.to_bytes()),",
                            d.id
                        )
                        .ok();
                        continue;
                    }
                    let ty = item_path("enums", &d.ty.namespace, &d.ty.name);
                    writeln!(
                        type_arms,
                        "        0x{:08x} => <{ty}>::from_bytes(raw).map(|v| v.to_bytes()),",
                        d.id
                    )
                    .ok();
                }
                Category::Functions => {
                    let generic = d.params.iter().any(
                        |p| matches!(&p.ty, ParameterType::Normal { ty, .. } if ty.generic_ref),
                    );
                    let f = item_path("functions", &d.namespace, &d.name);
                    let f = if generic {
                        format!("{f}<AnyObject>")
                    } else {
                        f
                    };
                    writeln!(
                        function_arms,
                        "        0x{:08x} => <{f}>::deserialize(cur).map(|v| v.to_bytes()),",
                        d.id
                    )
                    .ok();
                }
            }
        }
        writeln!(
            s,
            "/// Every constructor id of the {label} schema.
pub(crate) const COVERED_{up}: &[u32] = &[
{covered}];

/// `raw` (a boxed type or a function, id first) → grammers' typed value →
/// grammers' bytes. `None`: the id is not in the {label} schema.
pub(crate) fn typed_roundtrip_{label}(raw: &[u8]) -> Option<DeResult<Vec<u8>>> {{
    let id = u32::from_le_bytes(raw.get(..4)?.try_into().ok()?);
    Some(match id {{
{type_arms}        _ => return function_body_{label}(id, &mut Cursor::from_slice(&raw[4..])),
    }})
}}

/// A function's body (its id already read) → typed → bytes (id included).
pub(crate) fn function_body_{label}(id: u32, cur: &mut Cursor<'_>) -> Option<DeResult<Vec<u8>>> {{
    Some(match id {{
{function_arms}        _ => return None,
    }})
}}
",
            up = label.to_ascii_uppercase()
        )
        .ok();
    }

    /// Test-only: for every `Update` constructor and the `Updates` short
    /// forms, read the update's body through grammers' typed struct and pick
    /// the fields `decode::facts` reports, by the same names.
    fn update_oracle(&self, s: &mut String) {
        let mut arms = String::new();
        for d in &self.defs {
            let ty = full_name(&d.ty.namespace, &d.ty.name);
            if d.category != Category::Types || !(ty == "Update" || ty == "Updates") {
                continue;
            }
            let path = item_path("types", &d.namespace, &d.name);
            let mut body = String::new();
            for p in &d.params {
                let ParameterType::Normal { ty, .. } = &p.ty else {
                    continue;
                };
                let field = match role(&p.name, ty) {
                    "Pts" => "pts",
                    "PtsCount" => "pts_count",
                    "Qts" => "qts",
                    "ChannelId" => "channel_id",
                    "ChatId" => "chat_id",
                    "UserId" => "user_id",
                    "Peer" => "peer",
                    "From" => "from",
                    "FromUser" => "from_user",
                    "Message" => "message",
                    "Text" => "text",
                    "Out" => "out",
                    "Inner" => "inner",
                    _ => continue,
                };
                writeln!(
                    body,
                    "            o.{field} = G::g(&u.{});",
                    attr_name(&p.name)
                )
                .ok();
            }
            if d.params.is_empty() {
                writeln!(arms, "        0x{:08x} => Some(Oracle::default()),", d.id).ok();
            } else {
                writeln!(
                    arms,
                    "        0x{:08x} => {{\n            let u = <{path}>::from_bytes(body).ok()?;\n            let mut o = Oracle::default();\n{body}            Some(o)\n        }}",
                    d.id
                )
                .ok();
            }
        }
        writeln!(
            s,
            "/// An `Update` or `Updates` constructor's body, read by grammers' typed
/// struct; `None` when grammers cannot read it or the id is another type's.
pub(crate) fn update_oracle(id: u32, body: &[u8]) -> Option<Oracle> {{
    #[allow(unused_variables, unused_mut)]
    match id {{
{arms}        _ => None,
    }}
}}"
        )
        .ok();
    }
}

/// The part a field plays for `decode::facts`, by its name and type. Roles
/// mean something only inside `Update` constructors, the `Updates` short
/// forms and `Message`.
fn role(name: &str, ty: &Type) -> &'static str {
    let t = full_name(&ty.namespace, &ty.name);
    match (name, t.as_str()) {
        ("pts", "int") => "Pts",
        ("pts_count", "int") => "PtsCount",
        ("qts", "int") => "Qts",
        ("channel_id", "long") => "ChannelId",
        ("chat_id", "long") => "ChatId",
        ("user_id", "long") => "UserId",
        ("peer" | "peer_id", "Peer") => "Peer",
        ("from_id", "Peer") => "From",
        ("from_id", "long") => "FromUser",
        ("message", "Message") => "Message",
        ("message", "string") => "Text",
        ("update", "Update") => "Inner",
        ("out", "true") => "Out",
        _ => "None",
    }
}

/// grammers-tl-gen 0.11.0 `rustifier::rusty_type_name`:
/// `"ns.some_OK_name"` → `"SomeOkName"`.
fn rusty(name: &str) -> String {
    #[derive(Clone, Copy)]
    enum Casing {
        Upper,
        Lower,
        Preserve,
    }
    let name = name.rsplit_once('.').map_or(name, |(_, n)| n);
    let mut out = String::with_capacity(name.len());
    let mut casing = Casing::Upper;
    for c in name.chars() {
        if c == '_' {
            casing = Casing::Upper;
            continue;
        }
        casing = match casing {
            Casing::Upper => {
                out.push(c.to_ascii_uppercase());
                Casing::Lower
            }
            Casing::Lower | Casing::Preserve => {
                if matches!(casing, Casing::Lower) {
                    out.push(c.to_ascii_lowercase());
                } else {
                    out.push(c);
                }
                if c.is_ascii_uppercase() {
                    Casing::Lower
                } else {
                    Casing::Preserve
                }
            }
        };
    }
    out
}

/// grammers-tl-gen 0.11.0 `rustifier::parameters::attr_name`.
fn attr_name(name: &str) -> String {
    match name {
        "final" => "r#final".into(),
        "loop" => "r#loop".into(),
        "self" => "is_self".into(),
        "static" => "r#static".into(),
        "type" => "r#type".into(),
        n => n.to_ascii_lowercase(),
    }
}

fn item_path(module: &str, namespace: &[String], name: &str) -> String {
    let mut p = format!("grammers_tl_types::{module}::");
    for ns in namespace {
        p.push_str(ns);
        p.push_str("::");
    }
    p.push_str(&rusty(name));
    p
}
