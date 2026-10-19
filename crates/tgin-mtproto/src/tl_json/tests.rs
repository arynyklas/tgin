use std::time::Instant;

use super::*;
use crate::schema::MTPROTO;
use crate::testgen::{self, FlagMode, Gen, Plan, Stats, hex as hexs, par_each, seed, typed};

fn json(bytes: &[u8]) -> Result<String, TlError> {
    let mut s = String::new();
    to_json(bytes, &mut s).map(|()| s)
}

fn tl(text: &str) -> Result<Vec<u8>, TlError> {
    let mut b = Vec::new();
    from_json(text, &mut b).map(|()| b)
}

/// bytes → TL-JSON text → bytes, through `schema`.
fn roundtrip(schema: &Schema, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut text = String::new();
    schema
        .to_json(bytes, &mut text)
        .map_err(|e| format!("to_json: {e}"))?;
    let mut out = Vec::new();
    schema
        .from_json(&text, &mut out)
        .map_err(|e| format!("from_json: {e} (json {text})"))?;
    Ok(out)
}

fn le(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn rendering_follows_design_10_5() {
    // inputPeerUser#dde8a54c user_id:long access_hash:long
    let b = le(&[
        &0xdde8_a54cu32.to_le_bytes(),
        &(1i64 << 60).to_le_bytes(),
        &(-5i64).to_le_bytes(),
    ]);
    assert_eq!(
        json(&b).as_deref(),
        Ok(r#"{"_":"inputPeerUser","user_id":"1152921504606846976","access_hash":"-5"}"#)
    );
    // A safe integer is accepted for long in requests; an unsafe one is not.
    let enc = tl(r#"{"_": "inputPeerUser", "user_id": 7, "access_hash": "-5"}"#);
    assert_eq!(enc.as_ref().map(|e| &e[4..12]), Ok(&7i64.to_le_bytes()[..]));
    let unsafe_long = format!(
        r#"{{"_":"inputPeerUser","user_id":{},"access_hash":"1"}}"#,
        1i64 << 60
    );
    assert!(matches!(
        tl(&unsafe_long),
        Err(TlError::Json {
            field: "user_id",
            ..
        })
    ));

    // Flags words are never emitted; absent flagged fields are omitted;
    // `true` flags are `true`; Bool is a boolean; bytes are base64;
    // int256 is lowercase hex; Vector is an array.
    // phone.joinGroupCall#8fb53057 flags:# muted:flags.0?true
    //   video_stopped:flags.2?true call:InputGroupCall join_as:InputPeer
    //   invite_hash:flags.1?string public_key:flags.3?int256
    //   block:flags.3?bytes params:DataJSON
    let mut b = le(&[
        &0x8fb5_3057u32.to_le_bytes(),
        &(1u32 | 1 << 3).to_le_bytes(),
    ]);
    // inputGroupCallSlug#fe06823f slug:string
    b.extend_from_slice(&0xfe06_823fu32.to_le_bytes());
    assert!(write_tl_bytes(&mut b, b"ab").is_some());
    b.extend_from_slice(&0x7f3b_18eau32.to_le_bytes()); // inputPeerEmpty
    b.extend_from_slice(&[0xab; 32]);
    assert!(write_tl_bytes(&mut b, &[0, 1, 2]).is_some());
    b.extend_from_slice(&0x7d74_8d04u32.to_le_bytes()); // dataJSON data:string
    assert!(write_tl_bytes(&mut b, b"{}").is_some());
    let text = json(&b).unwrap_or_default();
    assert_eq!(
        text,
        format!(
            r#"{{"_":"phone.joinGroupCall","muted":true,"call":{{"_":"inputGroupCallSlug","slug":"ab"}},"join_as":{{"_":"inputPeerEmpty"}},"public_key":"{}","block":"AAEC","params":{{"_":"dataJSON","data":"{{}}"}}}}"#,
            "ab".repeat(32)
        )
    );
    assert_eq!(tl(&text), Ok(b));

    // Bool → boolean, Vector → array: account.reorderUsernames#ef500eab
    // order:Vector<string>; account.updateStatus#6628562c offline:Bool.
    let b = le(&[
        &0x6628_562cu32.to_le_bytes(),
        &crate::schema::BOOL_TRUE.to_le_bytes(),
    ]);
    assert_eq!(
        json(&b).as_deref(),
        Ok(r#"{"_":"account.updateStatus","offline":true}"#)
    );
    let mut b = le(&[
        &0xef50_0eabu32.to_le_bytes(),
        &VECTOR_ID.to_le_bytes(),
        &2i32.to_le_bytes(),
    ]);
    assert!(write_tl_bytes(&mut b, b"a\"\\\n\x01").is_some());
    assert!(write_tl_bytes(&mut b, "ж".as_bytes()).is_some());
    let text = json(&b).unwrap_or_default();
    assert_eq!(
        text,
        r#"{"_":"account.reorderUsernames","order":["a\"\\\n\u0001","ж"]}"#
    );
    assert_eq!(tl(&text), Ok(b));
}

#[test]
fn requests_may_use_null_false_and_whitespace() {
    // A request may write absent flagged fields as null and unset `true`
    // flags as false, in any key order, with any JSON whitespace.
    let a = tl(
        r#" { "muted" : false , "invite_hash": null, "params": {"data": "{}", "_": "dataJSON"},
        "join_as": {"_": "inputPeerEmpty"}, "call": {"_": "inputGroupCallSlug", "slug": "ab"},
        "_": "phone.joinGroupCall" } "#,
    );
    let b = tl(
        r#"{"_":"phone.joinGroupCall","call":{"_":"inputGroupCallSlug","slug":"ab"},"join_as":{"_":"inputPeerEmpty"},"params":{"_":"dataJSON","data":"{}"}}"#,
    );
    assert!(a.is_ok());
    assert_eq!(a, b);
    assert_eq!(a.map(|v| v[4..8].to_vec()), Ok(0u32.to_le_bytes().to_vec()));
}

#[test]
fn invalid_utf8_string_is_base64_escaped() {
    // error#c4b9f9bb code:int text:string
    let mut b = le(&[&0xc4b9_f9bbu32.to_le_bytes(), &1i32.to_le_bytes()]);
    assert!(write_tl_bytes(&mut b, &[0xff, 0xfe, b'x']).is_some());
    let text = json(&b).unwrap_or_default();
    assert_eq!(text, r#"{"_":"error","code":1,"text":{"@base64":"//54"}}"#);
    assert_eq!(tl(&text), Ok(b));
}

/// NaN and ±Inf, which JSON numbers cannot carry, are `@double` (S11 F1);
/// finite doubles, -0.0 included, are numbers.
#[test]
fn non_finite_doubles_are_escaped() {
    // inputGeoPoint#48222faf flags:# lat:double long:double
    //   accuracy_radius:flags.0?int
    let point = |lat: f64, long: f64| {
        le(&[
            &0x4822_2fafu32.to_le_bytes(),
            &0u32.to_le_bytes(),
            &lat.to_bits().to_le_bytes(),
            &long.to_bits().to_le_bytes(),
        ])
    };
    let nan = f64::from_bits(0x7ff8_0000_0000_0001);
    let b = point(nan, f64::NEG_INFINITY);
    let text = json(&b).unwrap_or_default();
    assert_eq!(
        text,
        r#"{"_":"inputGeoPoint","lat":{"@double":"7ff8000000000001"},"long":{"@double":"fff0000000000000"}}"#
    );
    assert_eq!(tl(&text), Ok(b));
    let b = point(-0.0, 1e300);
    let text = json(&b).unwrap_or_default();
    assert_eq!(text, r#"{"_":"inputGeoPoint","lat":-0.0,"long":1e300}"#);
    assert_eq!(tl(&text), Ok(b));
    // A number literal that overflows to infinity is refused: it needs
    // `@double`. Uppercase or short hex is refused too.
    for bad in [
        r#"{"_":"inputGeoPoint","lat":1e400,"long":0}"#,
        r#"{"_":"inputGeoPoint","lat":{"@double":"7FF0000000000000"},"long":0}"#,
        r#"{"_":"inputGeoPoint","lat":{"@double":"7ff00000"},"long":0}"#,
    ] {
        assert!(
            matches!(tl(bad), Err(TlError::Json { field: "lat", .. })),
            "{bad}"
        );
    }
}

/// Doubles survive exactly (S11 F2): written in the shortest form that
/// reads back to the same bits, read with a correctly rounded parser.
#[test]
fn float_roundtrip() {
    let mut rng = testgen::Rng(testgen::SEED);
    let hard = [
        2.225_073_858_507_201_4e-308,
        f64::MIN_POSITIVE,
        f64::MAX,
        f64::MIN,
        f64::EPSILON,
        5e-324,
        0.1 + 0.2,
        9_007_199_254_740_993.0,
        1.797_693_134_862_315_7e308,
    ];
    let cases = hard
        .into_iter()
        .map(f64::to_bits)
        .chain((0..100_000).map(|_| rng.next()))
        .map(f64::from_bits)
        .filter(|x| x.is_finite());
    for x in cases {
        let b = le(&[
            &0x4822_2fafu32.to_le_bytes(),
            &0u32.to_le_bytes(),
            &x.to_bits().to_le_bytes(),
            &0f64.to_bits().to_le_bytes(),
        ]);
        let text = json(&b).unwrap_or_default();
        // A JSON number, not an escape.
        assert!(!text.contains('@'), "{text}");
        assert_eq!(tl(&text), Ok(b), "{x:e} via {text}");
    }
}

/// One case per strict-decoding rule (S11 F4): each is refused, never
/// dropped, so a decode that succeeds re-encodes to the same bytes.
#[test]
fn strict_decoder_refuses() {
    let err = |b: &[u8]| json(b).err();
    // error#c4b9f9bb code:int text:string
    let error = |text: &[u8]| le(&[&0xc4b9_f9bbu32.to_le_bytes(), &1i32.to_le_bytes(), text]);

    // Unused flag bits: inputMediaPhoto#e3af4434 flags:# spoiler:flags.1?true
    //   live_photo:flags.2?true id:InputPhoto ttl_seconds:flags.0?int
    //   video:flags.2?InputDocument; bit 31 is no field's.
    let b = le(&[
        &0xe3af_4434u32.to_le_bytes(),
        &(1u32 << 31).to_le_bytes(),
        &0x1cd7_bf0du32.to_le_bytes(), // inputPhotoEmpty
    ]);
    assert_eq!(
        err(&b),
        Some(TlError::UnusedFlagBits {
            ctor: "inputMediaPhoto",
            word: "flags",
            bits: 1 << 31
        })
    );
    // The same bytes with bit 31 clear decode.
    let ok = le(&[
        &0xe3af_4434u32.to_le_bytes(),
        &0u32.to_le_bytes(),
        &0x1cd7_bf0du32.to_le_bytes(),
    ]);
    assert!(json(&ok).is_ok());

    // Non-canonical length: a 1-byte string in the 254 long form.
    assert_eq!(
        err(&error(&[254, 1, 0, 0, b'x', 0, 0, 0])),
        Some(TlError::NonCanonical {
            what: "string length",
            at: 8
        })
    );
    // Non-canonical length: the 255 prefix, which TL does not define.
    assert!(matches!(
        err(&error(&[255, 0, 0, 0])),
        Some(TlError::NonCanonical {
            what: "string length",
            ..
        })
    ));
    // Non-zero padding.
    assert_eq!(
        err(&error(&[1, b'x', 0, 9])),
        Some(TlError::NonCanonical {
            what: "padding",
            at: 10
        })
    );
    assert!(json(&error(&[1, b'x', 0, 0])).is_ok());

    // And the structural rules around them.
    assert!(matches!(
        err(&error(&[1, b'x', 0])),
        Some(TlError::Eof { .. })
    ));
    assert!(matches!(
        err(&le(&[&error(&[0, 0, 0, 0]), &[0]])),
        Some(TlError::Trailing { at: 12 })
    ));
    // A Bool that is neither boolTrue nor boolFalse.
    let b = le(&[&0x6628_562cu32.to_le_bytes(), &0x1234_5678u32.to_le_bytes()]);
    assert!(matches!(
        err(&b),
        Some(TlError::UnknownConstructor {
            id: 0x1234_5678,
            ..
        })
    ));
    // A constructor of the wrong type: inputPeerEmpty where InputGroupCall is expected.
    let b = le(&[
        &0x500377f9u32.to_le_bytes(),
        &0x7f3b_18eau32.to_le_bytes(),
        &0i32.to_le_bytes(),
    ]);
    assert!(matches!(
        err(&b),
        Some(TlError::WrongType {
            want: "InputGroupCall",
            got: "inputPeerEmpty"
        })
    ));
    // A vector count larger than its bytes.
    let b = le(&[
        &0xef50_0eabu32.to_le_bytes(),
        &VECTOR_ID.to_le_bytes(),
        &i32::MAX.to_le_bytes(),
    ]);
    assert!(matches!(
        err(&b),
        Some(TlError::NonCanonical {
            what: "vector length",
            ..
        })
    ));
    // Every failure leaves the output as it was.
    let mut out = String::from("kept");
    assert!(to_json(&b, &mut out).is_err());
    assert_eq!(out, "kept");
}

#[test]
fn from_json_refuses_what_is_not_tl_json() {
    let cases: &[(&str, &str)] = &[
        (
            r#"{"_":"inputPeerUser","user_id":"1","access_hash":"1","x":1}"#,
            "an unknown key",
        ),
        (
            r#"{"_":"inputPeerUser","user_id":"1","user_id":"2","access_hash":"1"}"#,
            "a duplicate key",
        ),
        (r#"{"_":"inputPeerUser","user_id":"1"}"#, "a missing field"),
        (
            r#"{"_":"inputPeerUser","user_id":"+1","access_hash":"1"}"#,
            "a signed long string",
        ),
        (
            r#"{"_":"inputPeerUser","user_id":1.0,"access_hash":"1"}"#,
            "a fractional long",
        ),
        (r#"{"_":"nope"}"#, "an unknown constructor"),
        (r#"{"user_id":"1"}"#, "no _"),
        (r#"[1]"#, "not an object"),
        (
            r#"{"_":"error","code":2147483648,"text":""}"#,
            "an int out of range",
        ),
        (
            r#"{"_":"error","code":1,"text":{"@base64":"%%"}}"#,
            "bad base64",
        ),
        (
            r#"{"_":"inputMediaPhoto","flags":0,"id":{"_":"inputPhotoEmpty"}}"#,
            "a flags word",
        ),
        (
            r#"{"_":"inputMediaPhoto","spoiler":1,"id":{"_":"inputPhotoEmpty"}}"#,
            "a non-boolean true flag",
        ),
        (
            r#"{"_":"account.updateStatus","offline":1}"#,
            "a non-boolean Bool",
        ),
        (
            r#"{"_":"message","id":1,"peer_id":{"_":"peerUser","user_id":"1"},"date":0,"message":"","views":4}"#,
            "a field without the field that shares its flag bit (forwards)",
        ),
        (r#"{"_":"inputPeerEmpty""#, "a syntax error"),
    ];
    for (text, what) in cases {
        let mut out = vec![9];
        assert!(from_json(text, &mut out).is_err(), "{what}: {text}");
        assert_eq!(out, [9], "{what}: the output is left as it was");
    }
    assert!(matches!(tl("{"), Err(TlError::JsonSyntax { line: 1, .. })));
}

/// A hostile request nests `invokeWithLayer` deeper than any real object:
/// both directions refuse it instead of exhausting the stack.
#[test]
fn nesting_is_bounded() {
    // invokeWithLayer#da9b0d0d {X:Type} layer:int query:!X = X
    let mut b = Vec::new();
    for _ in 0..10_000 {
        b.extend_from_slice(&0xda9b_0d0du32.to_le_bytes());
        b.extend_from_slice(&230i32.to_le_bytes());
    }
    b.extend_from_slice(&0xedd4_882au32.to_le_bytes()); // updates.getState
    assert_eq!(json(&b), Err(TlError::TooDeep));
    let mut text = String::new();
    for _ in 0..10_000 {
        text.push_str(r#"{"_":"invokeWithLayer","layer":230,"query":"#);
    }
    assert!(tl(&text).is_err());
    // 40 deep is fine both ways.
    let shallow = &b[b.len() - 4 - 40 * 8..];
    let text = json(shallow).unwrap_or_default();
    assert_eq!(tl(&text).as_deref(), Ok(shallow));
}

/// Every constructor of `schema` (types and functions): seeded random
/// values → grammers' typed bytes → TL-JSON text → bytes, byte-identical.
/// The generator's own bytes go through the codec too, which covers what
/// grammers cannot hold (invalid UTF-8 in a `string`).
fn run(
    label: &str,
    schema: &Schema,
    cases: u32,
    typed: fn(&[u8]) -> Option<testgen::DeResult<Vec<u8>>>,
) -> Stats {
    let started = Instant::now();
    let plan = Plan::new(schema);
    let unreachable: Vec<&str> = plan
        .cost
        .iter()
        .zip(schema.ctors)
        .filter(|(c, _)| **c == u32::MAX)
        .map(|(_, k)| k.name)
        .collect();
    assert!(unreachable.is_empty(), "no finite value: {unreachable:?}");
    let all: Vec<usize> = (0..schema.ctors.len()).collect();
    let total = std::sync::Mutex::new(Stats::default());
    let findings = par_each(&all, |idx, found| {
        let c = &schema.ctors[idx];
        let mut st = Stats::default();
        let mut failed = 0;
        for case in 0..cases {
            let mut g = Gen::new(schema, &plan, seed(c.id, case), FlagMode::of(case), &mut st);
            let mut raw = Vec::new();
            g.ctor(&mut raw, idx, true, 0);
            let lossy = g.lossy;
            st.cases += 1;
            st.bytes += raw.len() as u64;
            st.max_len = st.max_len.max(raw.len());
            let mut report = |what: String| {
                if failed < 3 {
                    found.push(format!(
                        "{} case {case}: {what}\n  input {}",
                        c.name,
                        hexs(&raw)
                    ));
                }
                failed += 1;
            };
            let typed_bytes = match typed(&raw) {
                None => {
                    report("no grammers type for this id".into());
                    continue;
                }
                Some(Err(e)) => {
                    report(format!("grammers cannot deserialize: {e}"));
                    continue;
                }
                Some(Ok(t)) => t,
            };
            if lossy {
                st.grammers_lossy_cases += 1;
            } else if typed_bytes != raw {
                report("grammers re-serializes differently".into());
            }
            for (label, input) in [("typed", &typed_bytes), ("generated", &raw)] {
                match roundtrip(schema, input) {
                    Ok(out) if out == *input => {}
                    Ok(out) => report(format!(
                        "{label} bytes differ after the round trip: {}",
                        hexs(&out)
                    )),
                    Err(e) => report(format!("{label} bytes: {e}")),
                }
            }
        }
        if let Ok(mut t) = total.lock() {
            t.add(&st);
        }
    });
    let st = total.into_inner().unwrap_or_else(|e| e.into_inner());
    println!(
        "TL-JSON round trip, {label}: layer {}, {} constructors, {cases} cases each, {} cases, {} bytes, \
         largest {}, {:.1} s",
        schema.layer,
        schema.ctors.len(),
        st.cases,
        st.bytes,
        st.max_len,
        started.elapsed().as_secs_f64()
    );
    println!(
        "coverage: flag words set {} / zero {}; optional present {} / absent {}; true set {} / unset {}; \
         vectors non-empty {} / empty {}; long-form strings {} / bytes {}; invalid UTF-8 {}; multi-byte {}; \
         non-finite doubles {}; -0.0 {}; int128 {}; int256 {}; Bool {}; nested {} (depth {}); !X {}; lossy {}",
        st.flag_words_set,
        st.flag_words_zero,
        st.optional_present,
        st.optional_absent,
        st.true_set,
        st.true_unset,
        st.vectors_nonempty,
        st.vectors_empty,
        st.long_form_strings,
        st.long_form_bytes,
        st.invalid_utf8,
        st.multibyte_utf8,
        st.nonfinite_doubles,
        st.negative_zero,
        st.int128,
        st.int256,
        st.bools,
        st.nested_objects,
        st.max_depth,
        st.generic_queries,
        st.grammers_lossy_cases
    );
    for f in findings.iter().take(30) {
        println!("FINDING {f}");
    }
    assert!(findings.is_empty(), "{} findings", findings.len());
    st
}

/// The plan's 1 000 cases per constructor, on every PR. The api run takes
/// about 48 s in a debug build on 4 CPUs (7.4 s in release), inside the
/// plan's 5 min bound; `TGIN_TL_CASES` overrides it.
const PR_CASES: u32 = 1000;

#[test]
fn tl_json_roundtrip_every_constructor() {
    let st = run(
        "api",
        &API,
        testgen::cases(PR_CASES),
        typed::typed_roundtrip_api,
    );
    // api.tl (layer 230) has no int128 field; mtproto.tl covers it.
    assert_eq!(st.int128, 0);
    for (name, n) in [
        ("flag words set", st.flag_words_set),
        ("flag words zero", st.flag_words_zero),
        ("optional present", st.optional_present),
        ("optional absent", st.optional_absent),
        ("true set", st.true_set),
        ("true unset", st.true_unset),
        ("vectors non-empty", st.vectors_nonempty),
        ("vectors empty", st.vectors_empty),
        ("long-form strings", st.long_form_strings),
        ("long-form bytes", st.long_form_bytes),
        ("invalid UTF-8", st.invalid_utf8),
        ("multi-byte UTF-8", st.multibyte_utf8),
        ("non-finite doubles", st.nonfinite_doubles),
        ("-0.0", st.negative_zero),
        ("int256", st.int256),
        ("Bool", st.bools),
        ("nested", st.nested_objects),
        ("generic !X", st.generic_queries),
    ] {
        assert!(n > 0, "the generator never covered {name}");
    }
}

/// Every constructor of mtproto.tl, the same way; it is where `int128` is.
#[test]
fn tl_json_roundtrip_mtproto() {
    let st = run(
        "mtproto",
        &MTPROTO,
        testgen::cases(PR_CASES),
        typed::typed_roundtrip_mtproto,
    );
    assert!(st.int128 > 0 && st.int256 > 0 && st.vectors_nonempty > 0);
}
