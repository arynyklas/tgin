//! Every function of the vendored layer, classified the way the RPC
//! endpoint treats it, against the reviewed classification in
//! `tests/fixtures/acl-layer-230.txt`.
//!
//! - `refused`: never forwarded, whatever the allowlist says
//!   ([`always_refused`]: the authorization and update methods and the
//!   `invoke*` wrappers); [`unwrap_request`] refuses each.
//! - `stripped`: `initConnection` and `invokeWithLayer`, which
//!   [`unwrap_request`] unwraps.
//! - `exact`: granted only by an exact `allow` entry.
//! - `glob`: a glob may grant it.
//!
//! A layer bump fails this test until the new classification is reviewed
//! and the fixture renamed and rewritten. To print the classification the
//! code gives now, into the fixture: `TGIN_WRITE_ACL_FIXTURE=1 cargo test
//! -p tgin --test acl_layer`, then review the diff.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tgin_core::config::{self, ACCOUNT_CHANGING, ACCOUNT_READ_ONLY, GLOB_NAMESPACES};
use tgin_mtproto::rpc::{Refused, always_refused, function_id, unwrap_request};

/// The layer the classification was reviewed for; the vendored schema
/// must be this one.
const LAYER: i32 = 230;

/// The wrappers [`unwrap_request`] unwraps rather than refuses.
const STRIPPED: [&str; 2] = ["initConnection", "invokeWithLayer"];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The functions of the vendored schema, by name.
fn functions() -> Vec<String> {
    let text = read(&workspace().join(format!("crates/tgin-mtproto/schema/api-{LAYER}.tl")));
    assert!(
        text.lines()
            .any(|l| l.trim() == format!("// LAYER {LAYER}")),
        "the vendored schema is not layer {LAYER}"
    );
    let mut in_functions = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        match line {
            "---functions---" => in_functions = true,
            "---types---" => in_functions = false,
            _ if in_functions && line.starts_with(|c: char| c.is_ascii_alphabetic()) => {
                let name = line.split(['#', ' ']).next().unwrap_or_default();
                out.push(name.to_owned());
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

fn classify(functions: &[String]) -> BTreeMap<String, &'static str> {
    functions
        .iter()
        .map(|method| {
            let class = if always_refused(method) {
                "refused"
            } else if STRIPPED.contains(&method.as_str()) {
                "stripped"
            } else if config::exact_only(method) {
                "exact"
            } else {
                "glob"
            };
            (method.clone(), class)
        })
        .collect()
}

fn render(classes: &BTreeMap<String, &str>) -> String {
    let mut out = format!(
        "# Layer {LAYER}: how the RPC endpoint treats each function; see tests/acl_layer.rs.\n"
    );
    for (method, class) in classes {
        out.push_str(&format!("{class}\t{method}\n"));
    }
    out
}

#[test]
fn every_function_of_the_layer_is_classified_as_reviewed() {
    let functions = functions();
    let classes = classify(&functions);
    let actual = render(&classes);
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/acl-layer-{LAYER}.txt"));
    if std::env::var_os("TGIN_WRITE_ACL_FIXTURE").is_some() {
        std::fs::write(&fixture, &actual).expect("write the fixture");
    }
    let expected = read(&fixture).replace("\r\n", "\n");
    if expected != actual {
        let expected: BTreeMap<&str, &str> = expected
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_once('\t').map(|(c, m)| (m, c)))
            .collect();
        let mut diff = Vec::new();
        for (method, class) in &classes {
            match expected.get(method.as_str()) {
                Some(reviewed) if reviewed == class => {}
                Some(reviewed) => diff.push(format!("{method}: reviewed {reviewed}, now {class}")),
                None => diff.push(format!("{method}: new, {class}")),
            }
        }
        for method in expected.keys().filter(|m| !classes.contains_key(**m)) {
            diff.push(format!("{method}: no longer in the layer"));
        }
        panic!(
            "the classification differs from {}:\n{}",
            fixture.display(),
            diff.join("\n")
        );
    }
}

/// The functions read from the schema file are the ones the RPC endpoint
/// decodes, and the only ones without a namespace are the wrappers, each
/// refused or stripped.
#[test]
fn the_schema_file_is_the_endpoint_schema() {
    for method in functions() {
        assert!(function_id(&method).is_some(), "{method}");
        if !method.contains('.') {
            assert!(
                always_refused(&method) || STRIPPED.contains(&method.as_str()),
                "{method} has no namespace and is neither refused nor stripped"
            );
        }
    }
}

/// `unwrap_request` refuses every function of the `refused` class from its
/// id alone, whatever follows, and refuses none of the others for being
/// one.
#[test]
fn unwrap_request_refuses_the_refused_class() {
    let functions = functions();
    let refused: Vec<&String> = functions.iter().filter(|m| always_refused(m)).collect();
    assert!(refused.len() >= 20, "{refused:?}");
    for method in refused {
        let id = function_id(method).unwrap_or_else(|| panic!("{method} has no id"));
        let body = id.to_le_bytes();
        let answer = unwrap_request(&body, LAYER);
        assert!(
            matches!(
                answer,
                Err(Refused::AuthorizationMethod(name)
                    | Refused::UpdatesMethod(name)
                    | Refused::Wrapper(name)) if name == method
            ),
            "{method}: {answer:?}"
        );
    }
    for method in functions.iter().filter(|m| !always_refused(m)) {
        let id = function_id(method).unwrap_or_else(|| panic!("{method} has no id"));
        let body = id.to_le_bytes();
        let answer = unwrap_request(&body, LAYER);
        assert!(
            !matches!(
                answer,
                Err(Refused::AuthorizationMethod(_)
                    | Refused::UpdatesMethod(_)
                    | Refused::Wrapper(_))
            ),
            "{method}: {answer:?}"
        );
    }
}

/// Every name the lists give exists in the layer, so none is a typo, and
/// every glob namespace is one of the layer's.
#[test]
fn the_lists_name_functions_of_the_layer() {
    let functions = functions();
    for entry in ACCOUNT_CHANGING.iter().chain(&ACCOUNT_READ_ONLY) {
        let found = match entry.strip_suffix(".*") {
            Some(namespace) => functions
                .iter()
                .any(|f| f.split_once('.').is_some_and(|(ns, _)| ns == namespace)),
            None => functions.iter().any(|f| f == entry),
        };
        assert!(found, "{entry} is not in layer {LAYER}");
    }
    for namespace in GLOB_NAMESPACES {
        assert!(
            functions
                .iter()
                .any(|f| f.split_once('.').is_some_and(|(ns, _)| ns == namespace)),
            "{namespace} is not a namespace of layer {LAYER}"
        );
    }
    // Every `account.get*` is either read-only or kept exact-only.
    for getter in functions.iter().filter(|f| f.starts_with("account.get")) {
        assert_eq!(
            config::exact_only(getter),
            !ACCOUNT_READ_ONLY.contains(&getter.as_str()),
            "{getter}"
        );
    }
}

/// A namespace no reviewed list names is exact-only, as one a later layer
/// adds would be.
#[test]
fn an_unreviewed_namespace_is_exact_only() {
    let reviewed: Vec<&str> = GLOB_NAMESPACES.to_vec();
    for method in functions() {
        if let Some((namespace, _)) = method.split_once('.')
            && !reviewed.contains(&namespace)
        {
            assert!(config::exact_only(&method), "{method}");
        }
    }
    assert!(config::exact_only("layer231namespace.newMethod"));
}
