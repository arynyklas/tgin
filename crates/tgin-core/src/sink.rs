//! Where sources hand updates to the engine.

use bytes::Bytes;

use crate::facts::Facts;
use crate::ids::SourceId;

/// The result of offering one update to a [`Sink`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit {
    /// Queued for at least one consumer.
    Admitted,
    /// A target queue is full; the source should slow down and retry.
    Backpressure,
    /// No rule matched the update.
    Unroutable,
}

/// Accepts updates from sources. Implementations never block.
pub trait Sink: Send + Sync + 'static {
    fn admit(&self, source: SourceId, update_id: i64, payload: Bytes, facts: &Facts<'_>) -> Admit;
}
