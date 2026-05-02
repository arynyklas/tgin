use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc::Sender;

#[async_trait]
pub trait Updater: Send + Sync {
    async fn start(&self, tx: Sender<Bytes>);
}
