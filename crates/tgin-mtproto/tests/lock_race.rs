//! `second_open_is_held`, across processes: 50 rounds of two processes
//! opening the same session directory at once, exactly one winner each.
//!
//! `harness = false`: this `main` drives the race, and re-runs its own
//! executable with `TGIN_LOCK_CHILD_DIR` set as each contender. A test
//! target, so no release build contains it.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};

use tgin_mtproto::{SessionError, SessionStore};

const CHILD_DIR: &str = "TGIN_LOCK_CHILD_DIR";
const ROUNDS: usize = 50;

fn main() -> ExitCode {
    if let Some(dir) = std::env::var_os(CHILD_DIR) {
        return child(Path::new(&dir));
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        println!("{NAME}: test");
        return ExitCode::SUCCESS;
    }
    if !selected(&args) {
        println!("lock_race: skipped (filter)");
        return ExitCode::SUCCESS;
    }
    race()
}

const NAME: &str = "second_open_is_held_across_processes";

/// Libtest options whose value is the next argument.
const VALUED: &[&str] = &[
    "--skip",
    "--test-threads",
    "--format",
    "--color",
    "--logfile",
    "--shuffle-seed",
    "-Z",
];

/// Whether libtest's arguments (`cargo test -- <args>`) select this test:
/// no name filter or one that matches (`--exact`: equal), and no `--skip`
/// that matches.
fn selected(args: &[String]) -> bool {
    let exact = args.iter().any(|a| a == "--exact");
    let matches = |pattern: &str| {
        if exact {
            NAME == pattern
        } else {
            NAME.contains(pattern)
        }
    };
    let mut filters = Vec::new();
    let mut skips = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if let Some(skip) = arg.strip_prefix("--skip=") {
            skips.push(skip.to_owned());
        } else if VALUED.contains(&arg.as_str()) {
            let value = args.next();
            if arg == "--skip"
                && let Some(skip) = value
            {
                skips.push(skip.clone());
            }
        } else if !arg.starts_with('-') {
            filters.push(arg.clone());
        }
    }
    (filters.is_empty() || filters.iter().any(|f| matches(f))) && !skips.iter().any(|s| matches(s))
}

/// Opens the store, prints `won` or `held`, and (if it won) keeps the lock
/// until the driver closes stdin, so the other contender always meets it.
fn child(dir: &Path) -> ExitCode {
    let mut out = std::io::stdout();
    match SessionStore::open(dir) {
        Ok(store) => {
            let _ = writeln!(out, "won");
            let _ = out.flush();
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
            drop(store);
            ExitCode::SUCCESS
        }
        Err(e @ SessionError::Held { .. }) => {
            let _ = writeln!(out, "held: {e}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(out, "error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn spawn(exe: &Path, dir: &Path) -> Child {
    Command::new(exe)
        .env(CHILD_DIR, dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn contender")
}

fn first_line(child: &mut Child) -> String {
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().expect("stdout"))
        .read_line(&mut line)
        .expect("read contender");
    line.trim_end().to_owned()
}

fn race() -> ExitCode {
    let exe = std::env::current_exe().expect("own executable");
    let tmp = tempfile::tempdir().expect("tempdir");
    for round in 0..ROUNDS {
        let dir = tmp.path().join(format!("acct-{round}"));
        let mut a = spawn(&exe, &dir);
        let mut b = spawn(&exe, &dir);
        let lines = [first_line(&mut a), first_line(&mut b)];
        // Closing stdin releases the winner.
        drop(a.stdin.take());
        drop(b.stdin.take());
        let ok = [a.wait(), b.wait()]
            .into_iter()
            .all(|s| s.is_ok_and(|s| s.success()));
        let won = lines.iter().filter(|l| *l == "won").count();
        let held = lines
            .iter()
            .filter(|l| *l == &format!("held: account `acct-{round}` is held by another process"))
            .count();
        if !ok || won != 1 || held != 1 {
            eprintln!("round {round}: expected one winner, got {lines:?}");
            return ExitCode::FAILURE;
        }
    }
    println!("test second_open_is_held_across_processes ... ok ({ROUNDS} rounds)");
    ExitCode::SUCCESS
}
