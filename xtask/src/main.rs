//! Repository automation for tgin, run with `cargo xtask <command>`.

mod config_docs;
mod confinement;
mod e2e;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask <command>

commands:
  schema                write schema/tgin.v1.schema.json from the config model
  schema --check        fail if schema/tgin.v1.schema.json is out of date
  config-docs           write docs/config-reference.md from the config schema
  config-docs --check   fail if docs/config-reference.md is out of date
  e2e                   run the end-to-end suite (Docker): real bot frameworks
                        against tgin and the fake Telegram; tears it down after
  e2e --keep            the same, leaving the stack up
  grammers-confinement  fail if a workspace crate other than tgin-mtproto
                        depends on a grammers crate, directly or transitively";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["schema"] => schema(false),
        ["schema", "--check"] => schema(true),
        ["config-docs"] => config_reference(false),
        ["config-docs", "--check"] => config_reference(true),
        ["e2e"] => e2e::run(false),
        ["e2e", "--keep"] => e2e::run(true),
        ["grammers-confinement"] => confinement::run(),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// `relative`, a path from the repository root.
fn repo_path(relative: &[&str]) -> PathBuf {
    let xtask = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = xtask.parent().unwrap_or(xtask);
    relative
        .iter()
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

/// The committed JSON Schema of the config file.
fn schema_path() -> PathBuf {
    repo_path(&["schema", "tgin.v1.schema.json"])
}

/// The committed config reference.
fn config_reference_path() -> PathBuf {
    repo_path(&["docs", "config-reference.md"])
}

fn schema(check: bool) -> ExitCode {
    let expected = format!("{}\n", tgin_core::config::schema());
    generated(&schema_path(), &expected, check, "schema")
}

/// The config reference, rendered from the config model's schema.
fn config_reference_text() -> Result<String, String> {
    let schema: serde_json::Value = serde_json::from_str(&tgin_core::config::schema())
        .map_err(|err| format!("the config schema is not JSON: {err}"))?;
    config_docs::render(&schema)
}

fn config_reference(check: bool) -> ExitCode {
    match config_reference_text() {
        Ok(expected) => generated(&config_reference_path(), &expected, check, "config-docs"),
        Err(err) => {
            eprintln!("cannot render the config reference: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Writes `expected` to `path`, or with `check` fails unless `path` holds
/// it already; `command` is the xtask command that writes it.
fn generated(path: &Path, expected: &str, check: bool, command: &str) -> ExitCode {
    if check {
        return match std::fs::read_to_string(path) {
            Ok(committed) if committed == expected => ExitCode::SUCCESS,
            Ok(_) => {
                eprintln!(
                    "{} is out of date; run `cargo xtask {command}`",
                    path.display()
                );
                ExitCode::FAILURE
            }
            Err(err) => {
                eprintln!(
                    "cannot read {}: {err}; run `cargo xtask {command}`",
                    path.display()
                );
                ExitCode::FAILURE
            }
        };
    }
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, expected));
    match written {
        Ok(()) => {
            println!("wrote {}", path.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("cannot write {}: {err}", path.display());
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed reference matches the config model, as CI checks with
    /// `cargo xtask config-docs --check`.
    #[test]
    fn config_reference_is_up_to_date() {
        let path = config_reference_path();
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        let expected = config_reference_text().expect("the reference renders");
        assert!(
            committed == expected,
            "{} is stale; run `cargo xtask config-docs`",
            path.display()
        );
    }
}
