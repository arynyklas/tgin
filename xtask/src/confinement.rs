//! `cargo xtask grammers-confinement`: grammers crates stay inside
//! `tgin-mtproto`. No other workspace crate may reach a `grammers-*` package,
//! directly or through any dependency other than `tgin-mtproto` itself.

use std::collections::{HashMap, HashSet};
use std::process::{Command, ExitCode};

use serde_json::Value;

/// The one workspace crate allowed to depend on grammers.
const HOME: &str = "tgin-mtproto";

pub fn run() -> ExitCode {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--all-features",
        ])
        .output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            eprintln!(
                "cargo metadata failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("cannot run cargo metadata: {err}");
            return ExitCode::FAILURE;
        }
    };
    let metadata: Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("cargo metadata is not JSON: {err}");
            return ExitCode::FAILURE;
        }
    };
    match violations(&metadata) {
        Ok(found) if found.is_empty() => ExitCode::SUCCESS,
        Ok(found) => {
            for path in found {
                eprintln!("grammers outside {HOME}: {}", path.join(" -> "));
            }
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("cannot read cargo metadata: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Every dependency path (package names) from a workspace member other than
/// [`HOME`] to a grammers package, not passing through [`HOME`]; one path per
/// member and grammers package. Every dependency kind counts.
pub fn violations(metadata: &Value) -> Result<Vec<Vec<String>>, String> {
    let names: HashMap<&str, &str> = metadata["packages"]
        .as_array()
        .ok_or("no packages")?
        .iter()
        .filter_map(|p| Some((p["id"].as_str()?, p["name"].as_str()?)))
        .collect();
    let deps: HashMap<&str, Vec<&str>> = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or("no resolve graph")?
        .iter()
        .filter_map(|n| {
            let deps = n["deps"]
                .as_array()?
                .iter()
                .filter_map(|d| d["pkg"].as_str())
                .collect();
            Some((n["id"].as_str()?, deps))
        })
        .collect();
    let name = |id: &str| -> String { names.get(id).copied().unwrap_or(id).to_owned() };
    let mut found = Vec::new();
    for member in metadata["workspace_members"]
        .as_array()
        .ok_or("no workspace members")?
        .iter()
        .filter_map(Value::as_str)
    {
        if name(member) == HOME {
            continue;
        }
        // Depth-first, remembering each package's parent for the path.
        let mut parent: HashMap<&str, &str> = HashMap::new();
        let mut seen: HashSet<&str> = HashSet::from([member]);
        let mut stack = vec![member];
        while let Some(id) = stack.pop() {
            if name(id).starts_with("grammers-") {
                let mut path = vec![name(id)];
                let mut at = id;
                while let Some(&p) = parent.get(at) {
                    path.push(name(p));
                    at = p;
                }
                path.reverse();
                found.push(path);
                continue;
            }
            for &dep in deps.get(id).map(Vec::as_slice).unwrap_or_default() {
                if name(dep) != HOME && seen.insert(dep) {
                    parent.insert(dep, id);
                    stack.push(dep);
                }
            }
        }
    }
    found.sort();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(edges: &[(&str, &str)], members: &[&str]) -> Value {
        let mut ids: Vec<&str> = edges.iter().flat_map(|(a, b)| [*a, *b]).collect();
        ids.extend(members);
        ids.sort_unstable();
        ids.dedup();
        serde_json::json!({
            "packages": ids.iter().map(|id| serde_json::json!({"id": id, "name": id})).collect::<Vec<_>>(),
            "workspace_members": members,
            "resolve": {"nodes": ids.iter().map(|id| serde_json::json!({
                "id": id,
                "deps": edges.iter().filter(|(a, _)| a == id).map(|(_, b)| serde_json::json!({"pkg": b})).collect::<Vec<_>>(),
            })).collect::<Vec<_>>()},
        })
    }

    #[test]
    fn grammers_only_inside_tgin_mtproto() {
        let ok = metadata(
            &[
                ("tgin-mtproto", "grammers-mtproto"),
                ("tgin", "tgin-mtproto"),
                ("tgin", "serde"),
            ],
            &["tgin", "tgin-mtproto"],
        );
        assert_eq!(violations(&ok), Ok(vec![]));

        let direct = metadata(
            &[
                ("tgin-core", "grammers-crypto"),
                ("tgin-mtproto", "grammers-crypto"),
            ],
            &["tgin-core", "tgin-mtproto"],
        );
        assert_eq!(
            violations(&direct),
            Ok(vec![vec!["tgin-core".into(), "grammers-crypto".into()]])
        );

        let through = metadata(
            &[("tgin", "some-lib"), ("some-lib", "grammers-tl-types")],
            &["tgin"],
        );
        assert_eq!(
            violations(&through),
            Ok(vec![vec![
                "tgin".into(),
                "some-lib".into(),
                "grammers-tl-types".into()
            ]])
        );
    }
}
