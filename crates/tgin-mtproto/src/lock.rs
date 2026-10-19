//! The exclusive `{dir}/lock` that makes one process the owner of a session
//! directory.
//!
//! The lock is an OS file lock (`flock` on Unix, `LockFileEx` on Windows), so
//! it only excludes processes on the same OS instance: a session directory
//! must never be shared across hosts, bind mounts into other machines or
//! network filesystems.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::thread::JoinHandle;

use fd_lock::RwLock;

use crate::SessionError;

/// The held `{dir}/lock`, released on drop. fd-lock's guard borrows its
/// `RwLock`, so a small thread owns both and holds the guard until this
/// value drops.
#[derive(Debug)]
pub(crate) struct DirLock {
    release: Option<mpsc::Sender<()>>,
    owner: Option<JoinHandle<()>>,
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // Dropping the sender wakes the owner, which drops the guard.
        drop(self.release.take());
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

/// Takes the exclusive lock on `{dir}/lock`, or fails at once with
/// [`SessionError::Held`] naming the directory's last component.
pub(crate) fn lock_dir(dir: &Path) -> Result<DirLock, SessionError> {
    let path = dir.join("lock");
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    let (result_tx, result_rx) = mpsc::sync_channel::<io::Result<()>>(1);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let owner = std::thread::Builder::new()
        .name("session-lock".into())
        .spawn(move || {
            let mut lock = RwLock::new(file);
            match lock.try_write() {
                Ok(_guard) => {
                    if result_tx.send(Ok(())).is_ok() {
                        // Blocks until `DirLock` drops the sender.
                        let _ = release_rx.recv();
                    }
                }
                Err(e) => {
                    let _ = result_tx.send(Err(e));
                }
            }
        })?;
    let result = result_rx.recv();
    match result {
        Ok(Ok(())) => Ok(DirLock {
            release: Some(release_tx),
            owner: Some(owner),
        }),
        Ok(Err(e)) => {
            let _ = owner.join();
            if e.kind() == io::ErrorKind::WouldBlock {
                Err(SessionError::Held {
                    account: dir.file_name().map_or_else(
                        || dir.display().to_string(),
                        |n| n.to_string_lossy().into_owned(),
                    ),
                })
            } else {
                Err(SessionError::Io(e))
            }
        }
        Err(_) => {
            let _ = owner.join();
            Err(SessionError::Io(io::Error::other(
                "session lock thread exited",
            )))
        }
    }
}
