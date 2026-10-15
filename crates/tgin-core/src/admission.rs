//! Admission: routing one update and queueing it in every target consumer.
//!
//! The engine reserves room in every `backpressure` target first; if one is
//! full it cancels the reservations made so far and refuses the update, so
//! nothing is queued or evicted anywhere. Then it reserves room in the
//! `drop_oldest` targets, which may evict their oldest unleased updates; one
//! still full (everything leased) skips the update. Then it commits every
//! reservation. One queue lock is held at a time, and nothing awaits.

use bytes::Bytes;
use smallvec::SmallVec;
use tgin_runtime::queue::{OnFull, Permit};
use tokio::time::Instant;

use crate::engine::{Engine, Target};
use crate::envelope::Envelope;
use crate::facts::Facts;
use crate::ids::{ConsumerId, SourceId};
use crate::metrics::ConsumerDropReason;
use crate::sink::{Admit, Sink};

impl Sink for Engine {
    fn admit(&self, source: SourceId, update_id: i64, payload: Bytes, facts: &Facts<'_>) -> Admit {
        let state = self.shared.state.load();
        let mut ids: SmallVec<[ConsumerId; 8]> = SmallVec::new();
        let mut scratch = self.scratch.lock().pop().unwrap_or_default();
        let matched = state.table.route(source, facts, &mut ids, &mut scratch);
        self.scratch.lock().push(scratch);
        if matched.is_none() {
            if let Some(info) = state.sources.get(usize::from(source.0)) {
                info.metrics.unmatched();
            }
            return Admit::Unroutable;
        }

        let now = Instant::now();
        let size = payload.len();
        let targets = || {
            ids.iter()
                .filter_map(|id| state.targets.get(usize::from(id.0)))
        };
        let mut permits: SmallVec<[(&Target, Permit); 8]> = SmallVec::new();
        for target in targets().filter(|t| t.on_full == OnFull::Backpressure) {
            match target.slot.consumer.queue().try_reserve(size) {
                Ok(permit) => {
                    target.slot.relieved(now);
                    permits.push((target, permit));
                }
                Err(_) => {
                    target.slot.pressured(now);
                    for (target, permit) in permits {
                        target.slot.consumer.queue().cancel(permit);
                    }
                    return Admit::Backpressure;
                }
            }
        }
        for target in targets().filter(|t| t.on_full == OnFull::DropOldest) {
            let metrics = &target.slot.metrics;
            match target.slot.consumer.queue().try_reserve(size) {
                Ok(permit) => {
                    if permit.evicted > 0 {
                        let evicted = u64::try_from(permit.evicted).unwrap_or(u64::MAX);
                        metrics.dropped(ConsumerDropReason::Overflow, evicted);
                    }
                    target.slot.relieved(now);
                    permits.push((target, permit));
                }
                Err(_) => metrics.dropped(ConsumerDropReason::Overflow, 1),
            }
        }

        let key = facts.ordering_key();
        let envelope = Envelope {
            source,
            update_id,
            update_type: update_type(&payload, facts.update_type),
            key,
            payload,
            received_at: now,
        };
        for (target, permit) in permits {
            let consumer = &target.slot.consumer;
            if let Err(foreign) = consumer
                .queue()
                .commit(permit, envelope.clone(), key, update_id)
            {
                // Every permit comes from the queue it is committed to.
                debug_assert!(false, "a permit went to another queue");
                drop(foreign);
                target.slot.metrics.dropped(ConsumerDropReason::Overflow, 1);
            }
            consumer.update_depth();
        }
        Admit::Admitted
    }
}

/// The update type as a slice of the payload when `update_type` borrows from
/// it, else a copy.
fn update_type(payload: &Bytes, update_type: &str) -> Bytes {
    let start = payload.as_ptr().addr();
    let at = update_type.as_ptr().addr();
    let within = at >= start
        && at
            .checked_add(update_type.len())
            .is_some_and(|end| end <= start + payload.len());
    if within {
        payload.slice_ref(update_type.as_bytes())
    } else {
        Bytes::copy_from_slice(update_type.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_type_slices_the_payload_when_borrowed() {
        let payload = Bytes::from_static(br#"{"update_id":1,"message":{}}"#);
        let text = std::str::from_utf8(&payload).unwrap();
        let at = text.find("message").unwrap();
        let borrowed = &text[at..at + "message".len()];
        let got = update_type(&payload, borrowed);
        assert_eq!(&got[..], b"message");
        assert_eq!(got.as_ptr(), payload[at..].as_ptr(), "no copy");

        let owned = String::from("callback_query");
        let got = update_type(&payload, &owned);
        assert_eq!(&got[..], b"callback_query");
        assert_ne!(got.as_ptr(), owned.as_ptr());

        assert!(update_type(&payload, "").is_empty());
    }
}
