use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::observe::UpdaterMetrics;

#[async_trait]
pub trait Updater: Send + Sync {
    async fn start(&self, tx: Sender<Bytes>, shutdown: CancellationToken);

    /// Per-updater counters surfaced on `/status` and `/metrics`. Adapters
    /// that track ingress return `Some`; the default is `None` for adapters
    /// with nothing to report.
    fn metrics(&self) -> Option<Arc<UpdaterMetrics>> {
        None
    }
}
