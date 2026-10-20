//! Every function of the vendored layer, classified the way the RPC
//! endpoint treats it, against the reviewed classification in
//! `tests/fixtures/acl-layer-230.txt`.
//!
//! - `refused`: never forwarded, whatever the allowlist says: the wrappers
//!   (`initConnection`, `invoke*`), the update methods and the
//!   authorization methods of `tgin-mtproto`'s `rpc.rs`.
//! - `exact`: granted only by an exact `allow` entry.
//! - `glob`: a glob may grant it.
//!
//! A layer bump fails this test until the new classification is reviewed
//! and the fixture renamed and rewritten. To print the classification the
//! code gives now, into the fixture: `TGIN_WRITE_ACL_FIXTURE=1 cargo test
//! -p tgin-core --test acl_layer`, then review the diff.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tgin_core::config::{self, ACCOUNT_CHANGING, ACCOUNT_READ_ONLY, GLOB_NAMESPACES};

/// The layer the classification was reviewed for; the vendored schema
/// must be this one.
const LAYER: u32 = 230;

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

/// The quoted names of the `const <name>: [&str; N] = [ … ];` in `text`.
fn const_list(text: &str, name: &str) -> Vec<String> {
    let start = text
        .find(&format!("const {name}: [&str;"))
        .unwrap_or_else(|| panic!("no `{name}` in rpc.rs"));
    let body = &text[start..];
    let body = &body[body.find("= [").expect("a list")..body.find("];").expect("its end")];
    body.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// What `tgin-mtproto` refuses whatever the allowlist says.
fn always_refused() -> Vec<String> {
    let rpc = read(&workspace().join("crates/tgin-mtproto/src/rpc.rs"));
    let mut out = const_list(&rpc, "AUTHORIZATION_METHODS");
    out.extend(const_list(&rpc, "UPDATES_METHODS"));
    out
}

fn classify(functions: &[String]) -> BTreeMap<String, &'static str> {
    let refused = always_refused();
    functions
        .iter()
        .map(|method| {
            let class = if refused.contains(method) || !method.contains('.') {
                "refused"
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

/// The wrappers are the only functions without a namespace, and the RPC
/// endpoint unwraps or refuses each.
#[test]
fn functions_without_a_namespace_are_wrappers() {
    for method in functions().iter().filter(|m| !m.contains('.')) {
        assert!(
            method == "initConnection" || method.starts_with("invoke"),
            "{method} has no namespace and is not a wrapper"
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
    for entry in always_refused() {
        assert!(
            functions.contains(&entry),
            "{entry} is not in layer {LAYER}"
        );
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
