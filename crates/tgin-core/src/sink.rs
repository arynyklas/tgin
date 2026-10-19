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

/// One update of a batch that a source admits whole or not at all
/// ([`Engine::admit_batch`](crate::engine::Engine::admit_batch)). Its
/// payloads are built on demand, once the update is routed, and only the
/// forms a routed consumer takes.
pub trait BatchUpdate {
    /// What the rules match on.
    fn facts(&self) -> Facts<'_>;
    /// The id consumers ack it by; rising within the source.
    fn update_id(&self) -> i64;
    /// For `X-Tgin-Update-Type`.
    fn update_type(&self) -> &'static str;
    /// The payload consumers receive. Called at most once per admission
    /// attempt, possibly again on a retry: return the same bytes.
    fn payload(&mut self) -> Bytes;
    /// The payload of consumers with `render: tl_json`; called only when
    /// one is routed to, otherwise as [`BatchUpdate::payload`].
    fn rendered(&mut self) -> Bytes;
}
