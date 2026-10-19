//! Admission: routing one update and queueing it in every target consumer.
//!
//! The engine reserves room in every `backpressure` target first; if one is
//! full it cancels the reservations made so far and refuses the update, so
//! nothing is queued anywhere. Then it reserves room in the `drop_oldest`
//! targets, which may evict their oldest unleased updates; one still full
//! (everything leased) skips the update. A target the update can never fit
//! (larger than its `capacity_bytes`) skips it too, in either mode. Then it
//! commits every reservation. One queue lock is held at a time, and nothing
//! awaits.
//!
//! A batch ([`Engine::admit_batch`]) goes the same way, all its updates at
//! once: room for every update in every `backpressure` target first, so one
//! full queue refuses the whole batch and queues nothing; then the
//! `drop_oldest` targets; then the commits, in batch order.
//!
//! A reload may retire a target between loading the state and committing.
//! Admission checks each target's `closed` flag before reserving, and routes
//! once more on the current state if one is closed; after committing it
//! checks again and drops what it queued to a target closed meanwhile.

use bytes::Bytes;
use smallvec::SmallVec;
use tgin_runtime::queue::{Full, OnFull, Permit};
use tokio::time::Instant;

use crate::engine::{Engine, EngineState, Target};
use crate::envelope::Envelope;
use crate::facts::Facts;
use crate::ids::{ConsumerId, SourceId};
use crate::metrics::ConsumerDropReason;
use crate::sink::{Admit, BatchUpdate, Sink};

impl Sink for Engine {
    fn admit(&self, source: SourceId, update_id: i64, payload: Bytes, facts: &Facts<'_>) -> Admit {
        let state = self.shared.state.load();
        self.admit_with(&state, source, update_id, payload, facts)
    }
}

/// How reserving went.
#[expect(
    clippy::large_enum_variant,
    reason = "lives on the stack for one admission; boxing would allocate per update"
)]
pub(crate) enum Attempt<'s> {
    Done(Admit),
    Reserved(Reserved<'s>),
    /// A target was retired; nothing is reserved.
    Closed,
}

/// Room reserved in every target that takes the update.
pub(crate) struct Reserved<'s> {
    permits: SmallVec<[(&'s Target, Permit); 8]>,
    now: Instant,
}

/// How reserving in one target went.
enum Step {
    Reserved(Permit),
    /// The update can never fit, or a `drop_oldest` queue is all leased:
    /// this target goes without it.
    Skipped,
    /// A `backpressure` queue is full.
    Full,
    /// The target was retired.
    Closed,
}

/// Room for an update of `size` in `target`, with the metrics and logs of
/// what happened.
fn reserve_in(
    state: &EngineState,
    target: &Target,
    pass: OnFull,
    size: usize,
    now: Instant,
    source: SourceId,
    update_id: i64,
) -> Step {
    let slot = &target.slot;
    if slot.is_closed() {
        return Step::Closed;
    }
    match slot.consumer.queue().try_reserve(size) {
        Ok(permit) => {
            slot.relieved(now);
            // A reload may have switched the queue to `drop_oldest` since
            // `state` was loaded.
            if permit.evicted > 0 {
                slot.overflowed(u64::try_from(permit.evicted).unwrap_or(u64::MAX));
            } else {
                slot.reserved_cleanly();
            }
            Step::Reserved(permit)
        }
        Err(Full::TooLarge) => {
            too_large(state, target, source, update_id, size);
            Step::Skipped
        }
        Err(_) if pass == OnFull::Backpressure => {
            slot.pressured(now);
            Step::Full
        }
        Err(_) => {
            slot.overflowed(1);
            Step::Skipped
        }
    }
}

/// Counts and logs an update `target` can never take.
fn too_large(state: &EngineState, target: &Target, source: SourceId, update_id: i64, size: usize) {
    let slot = &target.slot;
    slot.metrics.dropped(ConsumerDropReason::Oversized, 1);
    let source_name = state
        .source_names
        .get(usize::from(source.0))
        .map_or("?", |name| &**name);
    tracing::warn!(
        consumer = %slot.name,
        source = %source_name,
        update_id,
        size,
        capacity = target.capacity_bytes,
        "update is larger than the consumer's capacity_bytes; dropped for it"
    );
}

/// One routed update of a batch, with its payloads once built.
struct Routed {
    index: usize,
    key: Option<u64>,
    targets: SmallVec<[ConsumerId; 8]>,
    /// For targets with `render: raw`, when one is routed to.
    payload: Option<Bytes>,
    /// For targets with `render: tl_json`, when one is routed to.
    rendered: Option<Bytes>,
}

impl Engine {
    /// Admits on `state`, then once more on the current state if a target of
    /// `state` was retired; a retired target then too means
    /// [`Admit::Backpressure`].
    pub(crate) fn admit_with(
        &self,
        state: &EngineState,
        source: SourceId,
        update_id: i64,
        payload: Bytes,
        facts: &Facts<'_>,
    ) -> Admit {
        match self.reserve(state, source, update_id, payload.len(), facts) {
            Attempt::Done(admit) => admit,
            Attempt::Reserved(reserved) => {
                self.commit_reserved(reserved, source, update_id, payload, facts)
            }
            Attempt::Closed => {
                let current = self.shared.state.load();
                match self.reserve(&current, source, update_id, payload.len(), facts) {
                    Attempt::Done(admit) => admit,
                    Attempt::Reserved(reserved) => {
                        self.commit_reserved(reserved, source, update_id, payload, facts)
                    }
                    Attempt::Closed => Admit::Backpressure,
                }
            }
        }
    }

    /// Routes the update and reserves room for it in its targets.
    pub(crate) fn reserve<'s>(
        &self,
        state: &'s EngineState,
        source: SourceId,
        update_id: i64,
        size: usize,
        facts: &Facts<'_>,
    ) -> Attempt<'s> {
        let mut ids: SmallVec<[ConsumerId; 8]> = SmallVec::new();
        let mut scratch = self.scratch.lock().pop().unwrap_or_default();
        let matched = state.table.route(source, facts, &mut ids, &mut scratch);
        self.scratch.lock().push(scratch);
        let info = state.source(source);
        if matched.is_none() {
            if let Some(info) = info {
                info.unmatched(update_id, facts.update_type);
            }
            return Attempt::Done(Admit::Unroutable);
        }
        if let Some(info) = info {
            info.matched();
        }

        let now = Instant::now();
        let mut permits: SmallVec<[(&Target, Permit); 8]> = SmallVec::new();
        for pass in [OnFull::Backpressure, OnFull::DropOldest] {
            let targets = ids
                .iter()
                .filter_map(|id| state.targets.get(usize::from(id.0)))
                .filter(|t| t.on_full == pass);
            for target in targets {
                match reserve_in(state, target, pass, size, now, source, update_id) {
                    Step::Reserved(permit) => permits.push((target, permit)),
                    Step::Skipped => {}
                    Step::Full => {
                        cancel(permits);
                        return Attempt::Done(Admit::Backpressure);
                    }
                    Step::Closed => {
                        cancel(permits);
                        return Attempt::Closed;
                    }
                }
            }
        }
        Attempt::Reserved(Reserved { permits, now })
    }

    /// Queues the update in the room reserved for it.
    pub(crate) fn commit_reserved(
        &self,
        reserved: Reserved<'_>,
        source: SourceId,
        update_id: i64,
        payload: Bytes,
        facts: &Facts<'_>,
    ) -> Admit {
        let key = facts.ordering_key();
        let envelope = Envelope {
            source,
            update_id,
            update_type: update_type(&payload, facts.update_type),
            key,
            payload,
            received_at: reserved.now,
        };
        for (target, permit) in reserved.permits {
            commit_in(target, permit, envelope.clone());
        }
        Admit::Admitted
    }

    /// Admits every update of `updates` (from the source `source`) or none
    /// of them: [`Admit::Backpressure`] when a `backpressure` target lacks
    /// room for its share of the batch, and then nothing is queued
    /// anywhere. A share larger than such a target's `capacity` or
    /// `capacity_bytes` is taken when the target is empty, so it never
    /// waits for room that cannot come; the target then holds at most one
    /// batch over its limits. Otherwise each routed update is queued in
    /// batch order for each of its targets, as [`Sink::admit`] queues one;
    /// a `drop_oldest` target short of room keeps the batch's newest
    /// updates, as it would one by one. [`Admit::Unroutable`] when no rule
    /// matches any update.
    ///
    /// Each payload is built once, and only the forms a routed target
    /// takes: [`BatchUpdate::payload`] for targets with `render: raw`,
    /// [`BatchUpdate::rendered`] for those with `render: tl_json`.
    pub fn admit_batch<U: BatchUpdate>(&self, source: SourceId, updates: &mut [U]) -> Admit {
        let state = self.shared.state.load();
        if let Some(admit) = self.admit_batch_on(&state, source, updates) {
            return admit;
        }
        let current = self.shared.state.load();
        self.admit_batch_on(&current, source, updates)
            .unwrap_or(Admit::Backpressure)
    }

    /// [`Engine::admit_batch`] on `state`; `None` when a target was retired
    /// meanwhile and nothing was reserved.
    fn admit_batch_on<U: BatchUpdate>(
        &self,
        state: &EngineState,
        source: SourceId,
        updates: &mut [U],
    ) -> Option<Admit> {
        let mut routed: Vec<Routed> = Vec::with_capacity(updates.len());
        let mut scratch = self.scratch.lock().pop().unwrap_or_default();
        for (index, update) in updates.iter().enumerate() {
            let facts = update.facts();
            let mut targets = SmallVec::new();
            if state
                .table
                .route(source, &facts, &mut targets, &mut scratch)
                .is_none()
            {
                tracing::debug!(
                    update_id = update.update_id(),
                    update_type = facts.update_type,
                    "no rule matched the update"
                );
                continue;
            }
            routed.push(Routed {
                index,
                key: facts.ordering_key(),
                targets,
                payload: None,
                rendered: None,
            });
        }
        self.scratch.lock().push(scratch);
        if routed.is_empty() {
            return Some(Admit::Unroutable);
        }
        for r in &mut routed {
            let update = &mut updates[r.index];
            let (mut raw, mut tl_json) = (false, false);
            for t in r
                .targets
                .iter()
                .filter_map(|id| state.targets.get(usize::from(id.0)))
            {
                if t.tl_json {
                    tl_json = true;
                } else {
                    raw = true;
                }
            }
            if raw {
                r.payload = Some(update.payload());
            }
            if tl_json {
                r.rendered = Some(update.rendered());
            }
        }

        let now = Instant::now();
        let mut permits: Vec<(usize, &Target, Permit)> = Vec::with_capacity(routed.len());
        // Each `backpressure` target's share of the batch at once.
        let mut done: SmallVec<[ConsumerId; 8]> = SmallVec::new();
        for r in &routed {
            for &id in &r.targets {
                let Some(target) = state.targets.get(usize::from(id.0)) else {
                    continue;
                };
                if target.on_full != OnFull::Backpressure || done.contains(&id) {
                    continue;
                }
                done.push(id);
                match reserve_share(state, target, id, &routed, updates, now, source) {
                    Ok(share) => permits.extend(share.into_iter().map(|(i, p)| (i, target, p))),
                    Err(step) => {
                        cancel_all(permits);
                        return match step {
                            Step::Closed => None,
                            _ => Some(Admit::Backpressure),
                        };
                    }
                }
            }
        }
        // `drop_oldest` targets newest first, so one short of room keeps
        // the batch's newest updates.
        for (i, r) in routed.iter().enumerate().rev() {
            let update_id = updates[r.index].update_id();
            let targets = r
                .targets
                .iter()
                .filter_map(|id| state.targets.get(usize::from(id.0)))
                .filter(|t| t.on_full == OnFull::DropOldest);
            for target in targets {
                let size = r.payload_for(target).len();
                match reserve_in(
                    state,
                    target,
                    OnFull::DropOldest,
                    size,
                    now,
                    source,
                    update_id,
                ) {
                    Step::Reserved(permit) => permits.push((i, target, permit)),
                    Step::Skipped | Step::Full => {}
                    Step::Closed => {
                        cancel_all(permits);
                        return None;
                    }
                }
            }
        }

        // Batch order within each queue; the sort is stable, so an update's
        // targets keep their order too.
        permits.sort_by_key(|&(i, _, _)| i);
        for (i, target, permit) in permits {
            let r = &routed[i];
            let update = &updates[r.index];
            let envelope = Envelope {
                source,
                update_id: update.update_id(),
                update_type: Bytes::from_static(update.update_type().as_bytes()),
                key: r.key,
                payload: r.payload_for(target).clone(),
                received_at: now,
            };
            commit_in(target, permit, envelope);
        }
        Some(Admit::Admitted)
    }
}

/// Room in the `backpressure` target `target` (consumer `id`) for every
/// routed update of the batch it takes, all at once: `Err(Step::Full)` when
/// it has no room for all of them, `Err(Step::Closed)` when it was retired.
/// An update larger than its `capacity_bytes` is left out, as one by one.
fn reserve_share<U: BatchUpdate>(
    state: &EngineState,
    target: &Target,
    id: ConsumerId,
    routed: &[Routed],
    updates: &[U],
    now: Instant,
    source: SourceId,
) -> Result<Vec<(usize, Permit)>, Step> {
    let slot = &target.slot;
    if slot.is_closed() {
        return Err(Step::Closed);
    }
    let mut share: Vec<(usize, usize)> = Vec::new();
    for (i, r) in routed.iter().enumerate() {
        if !r.targets.contains(&id) {
            continue;
        }
        let size = r.payload_for(target).len();
        if size > target.capacity_bytes {
            too_large(state, target, source, updates[r.index].update_id(), size);
        } else {
            share.push((i, size));
        }
    }
    let sizes: Vec<usize> = share.iter().map(|&(_, size)| size).collect();
    match slot.consumer.queue().try_reserve_all(&sizes) {
        Ok(permits) => {
            slot.relieved(now);
            slot.reserved_cleanly();
            Ok(share.into_iter().map(|(i, _)| i).zip(permits).collect())
        }
        Err(_) => {
            slot.pressured(now);
            Err(Step::Full)
        }
    }
}

impl Routed {
    /// The form `target` takes; the other one only if that one was not
    /// built, which admission never lets happen.
    fn payload_for(&self, target: &Target) -> &Bytes {
        let (first, second) = if target.tl_json {
            (&self.rendered, &self.payload)
        } else {
            (&self.payload, &self.rendered)
        };
        first.as_ref().or(second.as_ref()).unwrap_or(&EMPTY)
    }
}

static EMPTY: Bytes = Bytes::from_static(b"");

/// Queues `envelope` in the room `permit` reserved in `target`.
fn commit_in(target: &Target, permit: Permit, envelope: Envelope) {
    let slot = &target.slot;
    let (key, update_id) = (envelope.key, envelope.update_id);
    if let Err(foreign) = slot
        .consumer
        .queue()
        .commit(permit, envelope, key, update_id)
    {
        // Every permit comes from the queue it is committed to.
        debug_assert!(false, "a permit went to another queue");
        drop(foreign);
        slot.metrics.dropped(ConsumerDropReason::Overflow, 1);
    }
    if slot.is_closed() {
        // Retired after the check before reserving: its final drain may
        // have run before this commit.
        slot.drain_late();
    } else {
        slot.consumer.update_depth();
    }
}

fn cancel_all(permits: Vec<(usize, &Target, Permit)>) {
    for (_, target, permit) in permits {
        target.slot.consumer.queue().cancel(permit);
    }
}

fn cancel(permits: SmallVec<[(&Target, Permit); 8]>) {
    for (target, permit) in permits {
        target.slot.consumer.queue().cancel(permit);
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
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use prometheus_client::encoding::text::encode;
    use prometheus_client::registry::Registry;
    use tgin_runtime::config::MapEnv;
    use tgin_runtime::supervisor::Supervisor;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::config::{self, Config};
    use crate::engine::EngineRuntime;
    use crate::metrics::CoreMetrics;

    /// `callback_query` goes to `p1`, `message` to `p1` and `p2`.
    const BASE: &str = r#"version: 1
server: {}
bots:
  - name: a
    token: "1:aaa"
    routes:
      - match: { update_types: [message] }
        to: [{ all: [p1, p2] }]
      - match: { update_types: [callback_query] }
        to: [p1]
consumers:
  p1: { pull: { capacity: 1 } }
  p2: { pull: { capacity: 1, on_full: drop_oldest } }
"#;

    fn load(yaml: &str) -> Config {
        config::load(yaml, Path::new("tgin.yaml"), &MapEnv::default()).expect("valid config")
    }

    struct Harness {
        reg: Registry,
        engine: Engine,
        force: CancellationToken,
    }

    impl Harness {
        fn new(yaml: &str) -> Self {
            let mut reg = Registry::with_prefix("tgin");
            let metrics = CoreMetrics::register(&mut reg);
            let force = CancellationToken::new();
            let rt = EngineRuntime {
                supervisor: Arc::new(Supervisor::new()),
                stop: CancellationToken::new(),
                force: force.clone(),
                via: tgin_runtime::http::Via::generate(),
                reply: Default::default(),
            };
            let engine =
                Engine::new(&load(yaml), metrics, reqwest::Client::new(), rt).expect("engine");
            Self { reg, engine, force }
        }

        fn metric(&self, series: &str) -> Option<String> {
            let mut out = String::new();
            encode(&mut out, &self.reg).expect("encode");
            let prefix = format!("{series} ");
            out.lines()
                .find_map(|l| l.strip_prefix(&prefix))
                .map(str::to_owned)
        }

        fn slot(&self, state: &EngineState, name: &str) -> Arc<crate::engine::Slot> {
            Arc::clone(&state.target(name).expect("target").slot)
        }

        /// Ends the drain of the retired `slot` now.
        fn retire(&self, slot: &Arc<crate::engine::Slot>) {
            let drain = slot.drain.lock().clone().expect("draining");
            assert!(self.engine.shared.retire(slot, &drain).is_some());
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.force.cancel();
        }
    }

    fn facts(update_type: &str) -> Facts<'_> {
        Facts {
            update_type,
            chat_id: Some(7),
            ..Facts::default()
        }
    }

    fn payload(id: i64) -> Bytes {
        Bytes::from(format!(r#"{{"update_id":{id}}}"#))
    }

    #[tokio::test(start_paused = true)]
    async fn closed_target_routes_again_on_the_current_state() {
        let h = Harness::new(BASE);
        let old = h.engine.state();
        let p1 = h.slot(&old, "p1");
        // `p1` goes; `callback_query` now goes to `p2`.
        let without_p1 = BASE
            .replace("        to: [{ all: [p1, p2] }]\n", "        to: [p2]\n")
            .replace("        to: [p1]\n", "        to: [p2]\n")
            .replace("  p1: { pull: { capacity: 1 } }\n", "");
        h.engine.apply(&load(&without_p1)).expect("apply");
        h.retire(&p1);

        let admit = h
            .engine
            .admit_with(&old, SourceId(0), 1, payload(1), &facts("callback_query"));
        assert_eq!(admit, Admit::Admitted);
        assert_eq!(p1.consumer.queue().stats().available, 0);
        let p2 = h.engine.consumer("p2").expect("p2");
        assert_eq!(p2.queue().stats().available, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn closed_target_again_is_backpressure_and_reserves_nothing() {
        let h = Harness::new(BASE);
        let state = h.engine.state();
        // Not possible through reloads; it stands in for a retirement that
        // the current state would still route to.
        h.slot(&state, "p2").closed.store(true, Ordering::SeqCst);
        let admit = h
            .engine
            .admit_with(&state, SourceId(0), 1, payload(1), &facts("message"));
        assert_eq!(admit, Admit::Backpressure);
        for name in ["p1", "p2"] {
            let stats = h.slot(&state, name).consumer.queue().stats();
            assert_eq!((stats.available, stats.reserved), (0, 0), "{name}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn commit_after_retirement_is_dropped_and_counted() {
        let h = Harness::new(BASE);
        let old = h.engine.state();
        let p1 = h.slot(&old, "p1");
        let Attempt::Reserved(reserved) = h.engine.reserve(
            &old,
            SourceId(0),
            1,
            payload(1).len(),
            &facts("callback_query"),
        ) else {
            panic!("p1 has room");
        };
        // A mode change retires the old `p1`; its series stay for the new one.
        let pushed = BASE.replace(
            "p1: { pull: { capacity: 1 } }",
            r#"p1: { push: { endpoints: [{ url: "http://127.0.0.1:9/" }] } }"#,
        );
        h.engine.apply(&load(&pushed)).expect("apply");
        h.retire(&p1);
        assert_eq!(
            h.metric(r#"tgin_consumer_dropped_total{consumer="p1",reason="reload"}"#)
                .as_deref(),
            Some("0")
        );

        let admit = h.engine.commit_reserved(
            reserved,
            SourceId(0),
            1,
            payload(1),
            &facts("callback_query"),
        );
        assert_eq!(admit, Admit::Admitted);
        assert_eq!(p1.consumer.pending(), 0);
        assert_eq!(
            h.metric(r#"tgin_consumer_dropped_total{consumer="p1",reason="reload"}"#)
                .as_deref(),
            Some("1")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn evictions_count_in_the_backpressure_pass() {
        let h = Harness::new(BASE);
        let old = h.engine.state();
        let admit = h
            .engine
            .admit_with(&old, SourceId(0), 1, payload(1), &facts("callback_query"));
        assert_eq!(admit, Admit::Admitted);
        // A reload switches `p1` to `drop_oldest` while `old` still says
        // `backpressure`.
        let evicting = BASE.replace(
            "p1: { pull: { capacity: 1 } }",
            "p1: { pull: { capacity: 1, on_full: drop_oldest } }",
        );
        h.engine.apply(&load(&evicting)).expect("apply");
        let admit = h
            .engine
            .admit_with(&old, SourceId(0), 2, payload(2), &facts("callback_query"));
        assert_eq!(admit, Admit::Admitted);
        assert_eq!(
            h.metric(r#"tgin_consumer_dropped_total{consumer="p1",reason="overflow"}"#)
                .as_deref(),
            Some("1")
        );
    }

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
