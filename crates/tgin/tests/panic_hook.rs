//! The panic hook, in a test binary of its own: it swaps the process-wide
//! hook, which would hide the panic messages of tests running beside it.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tgin::cli::install_panic_hook;

/// Collects what a subscriber writes.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The hook logs the panic through `tracing` and replaces the previous hook
/// instead of chaining to it: the default hook would print the message to
/// stderr unmasked.
#[test]
fn panic_hook_logs_through_tracing_only() {
    let chained = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&chained);
    std::panic::set_hook(Box::new(move |_| flag.store(true, Ordering::SeqCst)));
    install_panic_hook();

    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let panicked = tracing::subscriber::with_default(subscriber, || {
        std::panic::catch_unwind(|| panic!("boom {}", 42))
    });
    drop(std::panic::take_hook());

    assert!(panicked.is_err());
    assert!(!chained.load(Ordering::SeqCst), "the previous hook ran");
    let logged = String::from_utf8(capture.0.lock().clone()).unwrap();
    assert!(logged.contains("ERROR"), "{logged}");
    assert!(logged.contains("panicked: boom 42"), "{logged}");
    assert!(logged.contains("panic_hook.rs"), "no location: {logged}");
    assert!(
        logged.contains("backtrace="),
        "no backtrace field: {logged}"
    );
}
