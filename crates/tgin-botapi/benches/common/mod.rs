//! The hot path the benches and `tests/alloc_budget.rs` measure: 1000
//! realistic updates (`tests/fixtures/updates.jsonl`) routed by an engine
//! built from `tests/fixtures/hot_path.yaml`.
//!
//! The engine runs on a current-thread runtime that is never driven, so its
//! background tasks (pull lease expiry, the push dispatcher) never run: the
//! push consumer's dispatcher is never spawned and nothing but the caller
//! touches the queues. [`HotPath::drain`] empties them between rounds.

// Each bench and test target uses its own part of this module.
#![allow(dead_code)]

use std::path::Path;

use bytes::Bytes;
use prometheus_client::registry::Registry;
use tgin_botapi::{extract, split_updates};
use tgin_core::config;
use tgin_core::engine::{Consumer, Engine, EngineRuntime};
use tgin_core::envelope::Envelope;
use tgin_core::facts::Facts;
use tgin_core::ids::SourceId;
use tgin_core::metrics::CoreMetrics;
use tgin_core::sink::{Admit, Sink};
use tgin_runtime::config::MapEnv;
use tgin_runtime::queue::{HolderId, LeaseQueue, OnFull, Ordering, QueueConfig};
use tgin_runtime::supervisor::Supervisor;
use tokio::runtime::Runtime;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub const CONFIG: &str = include_str!("../../tests/fixtures/hot_path.yaml");
pub const UPDATES: &str = include_str!("../../tests/fixtures/updates.jsonl");

/// The bot of `CONFIG` and the username its `getMe` would report.
pub const BOT: &str = "shop";
pub const USERNAME: &str = "tgin_bot";
pub const PULL: [&str; 2] = ["workers", "analytics"];
pub const PUSH: &str = "hooks";

/// Updates per `getUpdates` body, Telegram's maximum `limit`.
const BATCH: usize = 100;
/// Updates a `getUpdates` poll leases at once.
const POLL: usize = 100;

/// Every update of `UPDATES`, one JSON object each.
pub fn payloads() -> Vec<Bytes> {
    UPDATES
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| Bytes::copy_from_slice(l.as_bytes()))
        .collect()
}

/// `UPDATES` as `getUpdates` response bodies of up to 100 updates each.
pub fn bodies() -> Vec<Vec<u8>> {
    let lines: Vec<&str> = UPDATES.lines().filter(|l| !l.is_empty()).collect();
    lines
        .chunks(BATCH)
        .map(|chunk| format!(r#"{{"ok":true,"result":[{}]}}"#, chunk.join(",")).into_bytes())
        .collect()
}

/// The facts of every payload, as the long-poll source extracts them.
pub fn facts(payloads: &[Bytes]) -> Vec<(i64, Facts<'_>)> {
    payloads
        .iter()
        .map(|p| extract(p).expect("fixture updates are valid"))
        .collect()
}

/// The envelope admission would queue for `payload`.
pub fn envelope(source: SourceId, payload: &Bytes) -> Envelope {
    let (update_id, facts) = extract(payload).expect("fixture updates are valid");
    Envelope {
        source,
        update_id,
        update_type: Bytes::copy_from_slice(facts.update_type.as_bytes()),
        key: facts.ordering_key(),
        payload: payload.clone(),
        received_at: Instant::now(),
    }
}

/// A queue configured as a default `per_chat` pull consumer's.
pub fn queue() -> LeaseQueue<Envelope> {
    LeaseQueue::new(QueueConfig {
        capacity: 10_000,
        capacity_bytes: 64 << 20,
        lease_timeout: std::time::Duration::from_secs(120),
        ordering: Ordering::PerKey,
        on_full: OnFull::Backpressure,
    })
}

/// Commits every envelope, then leases them in polls of up to 100 and acks
/// each poll: one pull consumer's queue work for the batch.
pub fn commit_lease_ack(queue: &LeaseQueue<Envelope>, holder: &HolderId, batch: &[Envelope]) {
    for env in batch {
        let permit = queue.try_reserve(env.payload.len()).expect("room");
        queue
            .commit(permit, env.clone(), env.key, env.update_id)
            .expect("own permit");
    }
    let now = Instant::now();
    while !queue.lease(holder, POLL, now).is_empty() {
        queue.ack_all(holder);
    }
}

/// The engine of `CONFIG` with its runtime.
pub struct HotPath {
    pub engine: Engine,
    pub source: SourceId,
    holder: HolderId,
    // Dropped after the engine, which spawned onto it.
    _rt: Runtime,
}

impl HotPath {
    pub fn new() -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let cfg = config::load(CONFIG, Path::new("hot_path.yaml"), &MapEnv::default())
            .unwrap_or_else(|errors| {
                let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
                panic!("hot_path.yaml: {}", errors.join("; "))
            });
        let mut reg = Registry::default();
        let rt_parts = EngineRuntime {
            supervisor: std::sync::Arc::new(Supervisor::new()),
            stop: CancellationToken::new(),
            force: CancellationToken::new(),
        };
        let engine = {
            let _enter = rt.enter();
            Engine::new(
                &cfg,
                CoreMetrics::register(&mut reg),
                reqwest::Client::new(),
                rt_parts,
            )
            .expect("engine")
        };
        let source = engine.source_id(BOT).expect("the bot");
        engine.set_username(source, Some(USERNAME.to_owned()));
        Self {
            engine,
            source,
            holder: HolderId("bench".into()),
            _rt: rt,
        }
    }

    /// What the long-poll source does with one `getUpdates` body: splits
    /// it, extracts each update's facts and admits it. Returns how many
    /// updates were admitted.
    pub fn ingest(&self, body: &Bytes) -> usize {
        let updates = split_updates(body).expect("fixture bodies are valid");
        let mut admitted = 0;
        for (update_id, payload) in &updates {
            let (_, facts) = extract(payload).expect("fixture updates are valid");
            if self
                .engine
                .admit(self.source, *update_id, payload.clone(), &facts)
                == Admit::Admitted
            {
                admitted += 1;
            }
        }
        admitted
    }

    /// Admits updates whose facts are already extracted. Returns how many
    /// were admitted.
    pub fn admit(&self, payloads: &[Bytes], facts: &[(i64, Facts<'_>)]) -> usize {
        payloads
            .iter()
            .zip(facts)
            .filter(|(payload, (update_id, facts))| {
                self.engine
                    .admit(self.source, *update_id, (*payload).clone(), facts)
                    == Admit::Admitted
            })
            .count()
    }

    /// Empties every queue: pull consumers are polled and acked as their
    /// clients would, the push consumer, whose dispatcher never runs, is
    /// purged.
    pub fn drain(&self) {
        let now = Instant::now();
        for name in PULL {
            let Some(Consumer::Pull(c)) = self.engine.consumer(name) else {
                panic!("{name} is a pull consumer");
            };
            while !c.queue().lease(&self.holder, POLL, now).is_empty() {
                c.queue().ack_all(&self.holder);
            }
        }
        self.engine.purge(PUSH).expect("the push consumer");
        assert_eq!(self.engine.pending(), 0, "every queue is empty");
    }
}
