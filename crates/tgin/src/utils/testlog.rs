use std::future::Future;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct BufWriter(Arc<Mutex<Vec<u8>>>);

impl Write for BufWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for BufWriter {
    type Writer = BufWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Runs `f` with a scoped `tracing` subscriber that records every event at
/// TRACE and above, and returns the future's output with the captured text.
/// Events from tasks spawned inside `f` are not captured.
pub(crate) async fn capture_logs<F: Future>(f: F) -> (F::Output, String) {
    let buf = BufWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let output = f.with_subscriber(subscriber).await;
    let text = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
    (output, text)
}
