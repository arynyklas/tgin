//! Integration tests for the configuration watcher, against real files in a
//! temporary directory.

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use tgin_runtime::config::{ConfigWatcher, watch_config};
use tokio::sync::mpsc::Receiver;
use tokio::time::timeout;

const DEBOUNCE: Duration = Duration::from_millis(200);
/// Long enough that only the file-system watcher can report a change.
const POLL_EVERY: Duration = Duration::from_secs(3600);
/// Generous bound for an event to arrive.
const ARRIVES: Duration = Duration::from_secs(10);
/// How long to wait before concluding that no event is coming.
const QUIET: Duration = Duration::from_millis(1500 + 2 * 200);

async fn assert_change(rx: &mut Receiver<()>, what: &str) {
    timeout(ARRIVES, rx.recv())
        .await
        .unwrap_or_else(|_| panic!("no change reported after {what}"))
        .expect("the watcher is alive");
}

async fn assert_quiet(rx: &mut Receiver<()>, what: &str) {
    if let Ok(received) = timeout(QUIET, rx.recv()).await {
        panic!("unexpected {received:?} after {what}");
    }
}

/// Writes `tgin.yaml.tmp` and renames it over `file`, as editors and
/// deployment tools do.
fn replace_atomically(file: &Path, content: &str) {
    let tmp = file.with_extension("yaml.tmp");
    fs::write(&tmp, content).unwrap();
    fs::rename(&tmp, file).unwrap();
}

/// A watch whose file-system events were set up.
fn watch(file: &Path, debounce: Duration, poll_every: Duration) -> (ConfigWatcher, Receiver<()>) {
    let (watcher, rx, events) = watch_config(file, debounce, poll_every).unwrap();
    if let Some(err) = events {
        panic!("file events unavailable: {err}");
    }
    (watcher, rx)
}

#[tokio::test]
async fn watch_detects_atomic_rename() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tgin.yaml");
    fs::write(&file, "version: 1\n").unwrap();
    let (_watcher, mut rx) = watch(&file, DEBOUNCE, POLL_EVERY);

    replace_atomically(&file, "version: 2\n");
    assert_change(&mut rx, "the first rename").await;
    // The watch survives the replaced file.
    replace_atomically(&file, "version: 3\n");
    assert_change(&mut rx, "the second rename").await;
    assert_quiet(&mut rx, "the second rename was reported").await;
}

#[tokio::test]
async fn watch_ignores_unchanged_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tgin.yaml");
    fs::write(&file, "version: 1\n").unwrap();
    let (_watcher, mut rx) = watch(&file, DEBOUNCE, POLL_EVERY);

    fs::write(&file, "version: 1\n").unwrap();
    replace_atomically(&file, "version: 1\n");
    fs::write(dir.path().join("other.yaml"), "unrelated\n").unwrap();
    assert_quiet(&mut rx, "rewriting identical bytes").await;

    // The watcher is still alive: a real change is reported.
    fs::write(&file, "version: 2\n").unwrap();
    assert_change(&mut rx, "a real change").await;
}

#[tokio::test]
async fn watch_closes_channel_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tgin.yaml");
    fs::write(&file, "version: 1\n").unwrap();
    let (watcher, mut rx) = watch(&file, DEBOUNCE, POLL_EVERY);

    drop(watcher);
    let closed = timeout(ARRIVES, rx.recv())
        .await
        .expect("the channel closes");
    assert_eq!(closed, None);
}

#[tokio::test]
async fn watch_falls_back_to_polling() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tgin.yaml");
    fs::write(&file, "version: 1\n").unwrap();
    // An hour of debounce: only the poll thread can report within the test.
    let (_watcher, mut rx) = watch(&file, Duration::from_secs(3600), Duration::from_millis(100));

    fs::write(&file, "version: 2\n").unwrap();
    assert_change(&mut rx, "an in-place write, by polling").await;
}

/// A directory that does not exist yet cannot be watched for events, so the
/// setup reports that, and the poll still finds the file once it appears.
#[tokio::test]
async fn watch_polls_when_events_cannot_be_set_up() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("not-yet");
    let file = missing.join("tgin.yaml");
    let (_watcher, mut rx, events) =
        watch_config(&file, DEBOUNCE, Duration::from_millis(100)).unwrap();
    assert!(events.is_some(), "watching a missing directory succeeded");

    fs::create_dir(&missing).unwrap();
    fs::write(&file, "version: 1\n").unwrap();
    assert_change(&mut rx, "the file appeared, by polling").await;
}

#[test]
fn watch_rejects_zero_intervals() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tgin.yaml");
    fs::write(&file, "version: 1\n").unwrap();

    for (debounce, poll_every, name) in [
        (Duration::ZERO, POLL_EVERY, "debounce"),
        // The debouncer ticks every debounce / 4, which is zero below 4ns.
        (Duration::from_nanos(3), POLL_EVERY, "debounce"),
        (DEBOUNCE, Duration::ZERO, "poll_every"),
    ] {
        let Err(err) = watch_config(&file, debounce, poll_every) else {
            panic!("{name} {debounce:?}/{poll_every:?} was accepted");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert!(err.to_string().contains(name), "{err}");
    }
}

/// Publishes `content` the way the kubelet updates a mounted ConfigMap:
/// a new timestamped directory, a `..data_tmp` symlink to it, renamed over
/// `..data`. `tgin.yaml` is a symlink to `..data/tgin.yaml`.
#[cfg(unix)]
fn k8s_publish(dir: &Path, timestamp: &str, content: &str) {
    use std::os::unix::fs::symlink;

    fs::create_dir(dir.join(timestamp)).unwrap();
    fs::write(dir.join(timestamp).join("tgin.yaml"), content).unwrap();
    symlink(timestamp, dir.join("..data_tmp")).unwrap();
    fs::rename(dir.join("..data_tmp"), dir.join("..data")).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn watch_detects_k8s_symlink_swap() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tgin.yaml");
    k8s_publish(dir.path(), "..2026_10_15_1", "version: 1\n");
    std::os::unix::fs::symlink("..data/tgin.yaml", &file).unwrap();
    let (_watcher, mut rx) = watch(&file, DEBOUNCE, POLL_EVERY);

    k8s_publish(dir.path(), "..2026_10_15_2", "version: 2\n");
    fs::remove_dir_all(dir.path().join("..2026_10_15_1")).unwrap();
    assert_change(&mut rx, "the first swap").await;
    // Watching the symlink itself would have died with the first swap.
    k8s_publish(dir.path(), "..2026_10_15_3", "version: 3\n");
    fs::remove_dir_all(dir.path().join("..2026_10_15_2")).unwrap();
    assert_change(&mut rx, "the second swap").await;
}
