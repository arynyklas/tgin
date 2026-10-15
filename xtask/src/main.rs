//! Repository automation for tgin, run with `cargo xtask <command>`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask <command>

commands:
  schema           write schema/tgin.v1.schema.json from the config model
  schema --check   fail if schema/tgin.v1.schema.json is out of date";

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
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// The committed JSON Schema of the config file.
fn schema_path() -> PathBuf {
    let xtask = Path::new(env!("CARGO_MANIFEST_DIR"));
    xtask
        .parent()
        .unwrap_or(xtask)
        .join("schema")
        .join("tgin.v1.schema.json")
}

fn schema(check: bool) -> ExitCode {
    let path = schema_path();
    let expected = format!("{}\n", tgin_core::config::schema());
    if check {
        return match std::fs::read_to_string(&path) {
            Ok(committed) if committed == expected => ExitCode::SUCCESS,
            Ok(_) => {
                eprintln!(
                    "{} is out of date; run `cargo xtask schema`",
                    path.display()
                );
                ExitCode::FAILURE
            }
            Err(err) => {
                eprintln!(
                    "cannot read {}: {err}; run `cargo xtask schema`",
                    path.display()
                );
                ExitCode::FAILURE
            }
        };
    }
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, expected));
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
