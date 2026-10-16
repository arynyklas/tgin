//! Running the `tgin` binary as a child process.
//!
//! The child's config binds every listener to port 0, and the address the
//! child got is read back from its log. A test that picked a port by
//! binding and releasing it could lose it to any other socket (another
//! test's listener, or an outgoing connection's ephemeral port) before the
//! child binds it.

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Starts `command` with stdout and stderr written to `log`, and waits,
/// within `patience`, until the child has logged where its admin plane
/// listens; returns the child and that address. Kills and reaps the child
/// and panics with its log if it exits or takes longer.
///
/// The child does not inherit `RUST_LOG`, which overrides `log.level`: the
/// address comes from an info line.
pub async fn spawn_listening(
    command: &mut Command,
    log: &Path,
    patience: Duration,
) -> (Child, SocketAddr) {
    let sink = std::fs::File::create(log).expect("the log file");
    let mut child = command
        .env_remove("RUST_LOG")
        .stderr(sink.try_clone().expect("the log file"))
        .stdout(sink)
        .spawn()
        .expect("tgin starts");
    let started = Instant::now();
    loop {
        let logged = std::fs::read_to_string(log).unwrap_or_default();
        if let Some(admin) = listening(&logged, "admin plane listening") {
            return (child, admin);
        }
        let exited = child.try_wait().ok().flatten().is_some();
        if exited || started.elapsed() > patience {
            let _ = child.kill();
            let _ = child.wait();
            let logged = std::fs::read_to_string(log).unwrap_or_default();
            panic!("tgin did not report its admin listener (exited: {exited}); its log:\n{logged}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The first address of the log line `… message listen=[addr, …]`.
fn listening(logged: &str, message: &str) -> Option<SocketAddr> {
    let line = logged.lines().find(|line| line.contains(message))?;
    let (_, list) = line.split_once("listen=[")?;
    let (list, _) = list.split_once(']')?;
    list.split(", ").next()?.parse().ok()
}
