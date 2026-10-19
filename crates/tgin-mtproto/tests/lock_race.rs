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
    // `cargo test -- <filter>` passes arguments; run unless filtered out.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        println!("second_open_is_held_across_processes: test");
        return ExitCode::SUCCESS;
    }
    if let Some(filter) = args.iter().find(|a| !a.starts_with('-'))
        && !"second_open_is_held_across_processes".contains(filter.as_str())
    {
        return ExitCode::SUCCESS;
    }
    race()
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
