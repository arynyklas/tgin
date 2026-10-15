//! Reports changes to a configuration file.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use notify_debouncer_full::notify::{self, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use parking_lot::Mutex;
use tokio::sync::mpsc;

/// Keeps the watch on a configuration file alive. Dropping it stops
/// watching; the channel closes shortly after.
pub struct ConfigWatcher {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    /// Dropping it wakes the poll thread, which then exits.
    _stop_poll: std_mpsc::Sender<()>,
}

/// Watches `file` and sends `()` whenever its bytes differ from the last
/// bytes seen, starting from its content when this is called.
///
/// The parent directory is watched, not the file: editors and deployment
/// tools replace the file by renaming over it, and a Kubernetes ConfigMap
/// update swaps a `..data` symlink without any event naming the file. Every
/// event in the directory, after `debounce` of quiet, re-reads the file.
/// File-system events are not delivered on every platform (network mounts,
/// some container setups), so the file is also re-read every `poll_every`.
///
/// Bursts coalesce: at most one `()` is pending, and a receiver should
/// re-read the file rather than count notifications. A file that cannot be
/// read, for example in the middle of a swap, is not reported.
///
/// # Errors
///
/// Fails with [`io::ErrorKind::InvalidInput`] if `debounce` or `poll_every`
/// is zero, and otherwise if the parent directory cannot be watched or the
/// poll thread cannot be started.
pub fn watch_config(
    file: &Path,
    debounce: Duration,
    poll_every: Duration,
) -> io::Result<(ConfigWatcher, mpsc::Receiver<()>)> {
    // Zero would make the debouncer tick, or the poll thread loop, without
    // ever sleeping.
    for (name, interval) in [("debounce", debounce), ("poll_every", poll_every)] {
        if interval.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the config watcher's {name} interval must be greater than zero"),
            ));
        }
    }
    let (tx, rx) = mpsc::channel(1);
    let change = Arc::new(Change {
        last: Mutex::new(std::fs::read(file).ok()),
        file: file.to_owned(),
        tx,
    });

    let on_event = Arc::clone(&change);
    let mut debouncer = new_debouncer(debounce, None, move |_: DebounceEventResult| {
        // Errors, such as a dropped event buffer, may hide a change too.
        on_event.check();
    })
    .map_err(into_io)?;
    let dir = file
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    debouncer
        .watch(dir, RecursiveMode::NonRecursive)
        .map_err(into_io)?;

    let (stop_poll, stopped) = std_mpsc::channel::<()>();
    thread::Builder::new()
        .name("tgin-config-poll".to_owned())
        .spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(poll_every) {
                change.check();
            }
        })?;

    Ok((
        ConfigWatcher {
            _debouncer: debouncer,
            _stop_poll: stop_poll,
        },
        rx,
    ))
}

/// Compares the file with the bytes last seen and reports a difference.
struct Change {
    file: PathBuf,
    /// `None` until the file has been read successfully.
    last: Mutex<Option<Vec<u8>>>,
    tx: mpsc::Sender<()>,
}

impl Change {
    fn check(&self) {
        // Reading under the lock keeps the event and poll threads from
        // storing an older read over a newer one.
        let mut last = self.last.lock();
        let Ok(bytes) = std::fs::read(&self.file) else {
            return;
        };
        if last.as_deref() == Some(bytes.as_slice()) {
            return;
        }
        *last = Some(bytes);
        // `Full` means a notification is already pending, and `Closed` that
        // nobody listens; either way there is nothing to add.
        let _ = self.tx.try_send(());
    }
}

fn into_io(err: notify::Error) -> io::Error {
    match err.kind {
        notify::ErrorKind::Io(err) => err,
        _ => io::Error::other(err),
    }
}
