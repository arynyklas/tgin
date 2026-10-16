//! One update as it travels from a source to the consumer queues.

use bytes::Bytes;
use tokio::time::Instant;

use crate::ids::SourceId;

/// An update with its routing metadata. Cloning only bumps reference counts.
#[derive(Clone, Debug)]
pub struct Envelope {
    /// The source the update arrived on.
    pub source: SourceId,
    /// Telegram's `update_id`.
    pub update_id: i64,
    /// The update type (`message`, `callback_query`, ...): a slice of the
    /// payload's key or a static string.
    pub update_type: Bytes,
    /// Ordering key (see [`crate::facts::Facts::ordering_key`]).
    pub key: Option<u64>,
    /// The raw update JSON, forwarded verbatim.
    pub payload: Bytes,
    /// When admission queued the update (not when the source received it:
    /// time spent held back by backpressure, or in Telegram's retries, is
    /// not counted). `retry.max_age` and `tgin_delivery_latency_seconds`
    /// count from here.
    pub received_at: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_shares_payload() {
        let e = Envelope {
            source: SourceId(0),
            update_id: 1,
            update_type: Bytes::from_static(b"message"),
            key: Some(7),
            payload: Bytes::from_static(b"{}"),
            received_at: Instant::now(),
        };
        let c = e.clone();
        assert_eq!(c.payload.as_ptr(), e.payload.as_ptr());
        assert_eq!(c.key, Some(7));
    }
}
