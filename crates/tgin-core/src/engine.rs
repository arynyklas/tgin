//! The engine: the routing state of one config generation, the consumers it
//! routes to, and the control-plane operations on them.
//!
//! [`Engine`] keeps its [`EngineState`] behind an `ArcSwap`. Admission (the
//! engine's [`Sink`](crate::sink::Sink) implementation) loads it once per
//! update; a reload ([`Engine::apply`]) and a bot username
//! ([`Engine::set_username`]) swap in a new one.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use serde::Serialize;
use tgin_runtime::backoff::Backoff;
use tgin_runtime::breaker::BreakerState;
use tgin_runtime::queue::{LeaseQueue, OnFull};
use tgin_runtime::supervisor::{Exit, Supervisor};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use crate::config::Config;
use crate::consumer::{PullConsumer, PushConsumer};
use crate::envelope::Envelope;
use crate::ids::SourceId;
use crate::metrics::{ConsumerDropReason, ConsumerMetrics, CoreMetrics, SourceMetrics};
use crate::reload::{self, Plan};
use crate::rules::{MatchScratch, RouteTable, RuleError};

/// What the engine's background tasks run under.
#[derive(Clone, Debug)]
pub struct EngineRuntime {
    /// Supervises every push dispatcher, as `push:<consumer>`.
    pub supervisor: Arc<Supervisor>,
    /// Graceful stop: push dispatchers lease nothing new and finish the
    /// requests they sent; pull lease expiry stops.
    pub stop: CancellationToken,
    /// Aborts in-flight push deliveries (second signal or deadline).
    pub force: CancellationToken,
}

/// Why a config could not be turned into an engine state. Nothing changed.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("the routing rules do not compile: {0}")]
    Rules(#[from] RuleError),
}

/// One consumer instance. Cloning shares it.
#[derive(Clone)]
pub enum Consumer {
    Pull(Arc<PullConsumer>),
    Push(Arc<PushConsumer>),
}

impl Consumer {
    pub fn name(&self) -> &str {
        match self {
            Self::Pull(c) => c.name(),
            Self::Push(c) => c.name(),
        }
    }

    pub fn mode(&self) -> Mode {
        match self {
            Self::Pull(_) => Mode::Pull,
            Self::Push(_) => Mode::Push,
        }
    }

    pub fn queue(&self) -> &LeaseQueue<Envelope> {
        match self {
            Self::Pull(c) => c.queue(),
            Self::Push(c) => c.queue(),
        }
    }

    /// Updates not yet acked, delivered or dead-lettered: queued and leased.
    /// Push deliveries in flight hold leases, so they are counted once.
    pub fn pending(&self) -> usize {
        match self {
            Self::Pull(c) => c.pending(),
            Self::Push(c) => c.pending(),
        }
    }

    /// Push deliveries in flight; always 0 for a pull consumer.
    pub fn in_flight(&self) -> usize {
        match self {
            Self::Pull(_) => 0,
            Self::Push(c) => c.in_flight(),
        }
    }

    pub fn is_paused(&self) -> bool {
        match self {
            Self::Pull(c) => c.is_paused(),
            Self::Push(c) => c.is_paused(),
        }
    }

    pub fn pause(&self) {
        match self {
            Self::Pull(c) => c.pause(),
            Self::Push(c) => c.pause(),
        }
    }

    pub fn resume(&self) {
        match self {
            Self::Pull(c) => c.resume(),
            Self::Push(c) => c.resume(),
        }
    }

    pub fn begin_drain(&self) {
        match self {
            Self::Pull(c) => c.begin_drain(),
            Self::Push(c) => c.begin_drain(),
        }
    }

    pub(crate) fn update_depth(&self) {
        match self {
            Self::Pull(c) => c.update_depth(),
            Self::Push(c) => c.update_depth(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Pull,
    Push,
}

/// One bot of the current generation.
#[derive(Clone, Debug)]
pub struct SourceInfo {
    pub id: SourceId,
    pub name: Arc<str>,
    /// The bot's username, once known: `/cmd@bot` rules match only then.
    pub username: Option<String>,
    pub metrics: SourceMetrics,
}

/// The routing state of one config generation.
pub struct EngineState {
    pub generation: u64,
    pub table: RouteTable,
    /// Indexed by [`ConsumerId`](crate::ids::ConsumerId).
    pub consumers: Vec<Consumer>,
    /// Indexed by [`SourceId`].
    pub sources: Vec<SourceInfo>,
    /// Admission's view of `consumers`, by the same index.
    pub(crate) targets: Vec<Target>,
    /// The config this state was built from, for the next reload's diff.
    pub(crate) config: Arc<Config>,
}

impl EngineState {
    pub(crate) fn target(&self, name: &str) -> Option<&Target> {
        self.targets.iter().find(|t| &*t.slot.name == name)
    }
}

/// A consumer as admission sees it in one generation.
#[derive(Clone)]
pub(crate) struct Target {
    pub(crate) slot: Arc<Slot>,
    pub(crate) on_full: OnFull,
}

/// One consumer instance with what the engine keeps for it. It lives as long
/// as the instance, across the reloads that keep it.
pub(crate) struct Slot {
    pub(crate) name: Arc<str>,
    pub(crate) consumer: Consumer,
    pub(crate) metrics: ConsumerMetrics,
    /// When the queue began refusing with backpressure, as nanoseconds after
    /// `epoch` plus one; 0 while it accepts.
    pressured_since: AtomicU64,
    epoch: Instant,
    /// Stops the instance's tasks gracefully; a child of the engine's stop.
    pub(crate) stop: CancellationToken,
    /// Ends the supervision of its push dispatcher, aborting it; a child of
    /// the engine's force.
    pub(crate) kill: CancellationToken,
}

impl Slot {
    pub(crate) fn new(
        name: Arc<str>,
        consumer: Consumer,
        metrics: ConsumerMetrics,
        rt: &EngineRuntime,
    ) -> Self {
        Self {
            name,
            consumer,
            metrics,
            pressured_since: AtomicU64::new(0),
            epoch: Instant::now(),
            stop: rt.stop.child_token(),
            kill: rt.force.child_token(),
        }
    }

    /// The queue refused with backpressure; the first refusal starts the
    /// clock.
    pub(crate) fn pressured(&self, now: Instant) {
        let nanos = now.saturating_duration_since(self.epoch).as_nanos();
        let mark = u64::try_from(nanos)
            .unwrap_or(u64::MAX - 1)
            .saturating_add(1);
        let _ =
            self.pressured_since
                .compare_exchange(0, mark, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// The queue reserved room; counts the time since the first refusal.
    pub(crate) fn relieved(&self, now: Instant) {
        if self.pressured_since.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mark = self.pressured_since.swap(0, Ordering::Relaxed);
        if let Some(nanos) = mark.checked_sub(1) {
            let since = self.epoch + Duration::from_nanos(nanos);
            let seconds = now.saturating_duration_since(since).as_secs_f64();
            self.metrics.backpressure(seconds);
        }
    }

    /// Starts the instance's background tasks.
    pub(crate) fn start(&self, rt: &EngineRuntime) {
        match &self.consumer {
            Consumer::Pull(c) => c.spawn_expiry(self.stop.clone()),
            Consumer::Push(c) => {
                let c = Arc::clone(c);
                let stop = self.stop.clone();
                let force = rt.force.clone();
                supervise_dispatcher(&rt.supervisor, &self.name, self.kill.clone(), move || {
                    c.spawn(stop.clone(), force.clone())
                });
            }
        }
    }
}

/// Supervises a push dispatcher as `push:<consumer>`: each attempt runs the
/// task `spawn` starts until it returns (`Exit::Done`) or panics
/// (`Exit::Retry`, restarted after a backoff). Cancelling `cancel` ends the
/// supervision and aborts the task.
pub(crate) fn supervise_dispatcher<F>(
    supervisor: &Supervisor,
    consumer: &str,
    cancel: CancellationToken,
    mut spawn: F,
) where
    F: FnMut() -> JoinHandle<()> + Send + 'static,
{
    supervisor.spawn(
        Arc::from(format!("push:{consumer}")),
        Backoff::new(
            Duration::from_secs(1),
            Duration::from_secs(60),
            Duration::ZERO,
        ),
        cancel,
        move || {
            let task = AbortOnDropHandle::new(spawn());
            async move {
                match task.await {
                    Ok(()) => Exit::Done,
                    Err(err) if err.is_panic() => Exit::Retry("dispatcher panicked".to_owned()),
                    Err(_) => Exit::Retry("dispatcher aborted".to_owned()),
                }
            }
        },
    );
}

/// The engine: routes admitted updates to consumer queues and applies
/// reloads and control-plane operations. Must be created and reloaded within
/// a Tokio runtime.
pub struct Engine {
    pub(crate) shared: Arc<Shared>,
    /// Reusable regex scratch; the lock is held only to take or return one.
    pub(crate) scratch: Mutex<Vec<MatchScratch>>,
    pub(crate) client: reqwest::Client,
    pub(crate) rt: EngineRuntime,
}

/// What the engine shares with its drain tasks.
pub(crate) struct Shared {
    pub(crate) state: ArcSwap<EngineState>,
    pub(crate) metrics: CoreMetrics,
    /// Serializes state writers: reloads, usernames and the end of drains.
    pub(crate) write: Mutex<()>,
    /// Removed or replaced instances whose drain has not ended.
    pub(crate) retired: Mutex<Vec<Arc<Slot>>>,
    /// Set by [`Engine::begin_drain`]; instances started later drain too.
    pub(crate) draining: AtomicBool,
}

impl Shared {
    /// Ends the drain of a retired instance: drops what it still holds,
    /// stops its tasks, and removes its metric series unless an instance
    /// of the same name still uses them.
    pub(crate) fn retire(&self, slot: &Arc<Slot>) {
        let dropped = slot.consumer.queue().drain_all().len();
        if dropped > 0 {
            slot.metrics.dropped(
                ConsumerDropReason::Reload,
                u64::try_from(dropped).unwrap_or(u64::MAX),
            );
            tracing::warn!(
                consumer = %slot.name,
                dropped,
                "dropped the updates a reloaded consumer still held"
            );
        }
        slot.stop.cancel();
        slot.kill.cancel();
        let _write = self.write.lock();
        let mut retired = self.retired.lock();
        retired.retain(|s| !Arc::ptr_eq(s, slot));
        let in_use = self.state.load().target(&slot.name).is_some()
            || retired.iter().any(|s| s.name == slot.name);
        drop(retired);
        if !in_use {
            self.metrics.remove_consumer(&slot.name);
        }
    }
}

impl Engine {
    /// Builds the engine for `cfg` as generation 1 and starts every
    /// consumer's tasks.
    ///
    /// # Errors
    ///
    /// The config's rules do not compile.
    pub fn new(
        cfg: &Config,
        metrics: CoreMetrics,
        client: reqwest::Client,
        rt: EngineRuntime,
    ) -> Result<Self, BuildError> {
        let plan = Plan::build(cfg, None, &metrics, &client, &rt)?;
        let (state, rest) = plan.into_parts();
        let engine = Self {
            shared: Arc::new(Shared {
                state: ArcSwap::from_pointee(state),
                metrics,
                write: Mutex::new(()),
                retired: Mutex::new(Vec::new()),
                draining: AtomicBool::new(false),
            }),
            scratch: Mutex::new(Vec::new()),
            client,
            rt,
        };
        reload::commit(&engine, None, rest);
        Ok(engine)
    }

    /// The current state.
    pub fn state(&self) -> Arc<EngineState> {
        self.shared.state.load_full()
    }

    /// The current instance of the consumer `name`.
    pub fn consumer(&self, name: &str) -> Option<Consumer> {
        let state = self.shared.state.load();
        state.target(name).map(|t| t.slot.consumer.clone())
    }

    /// Sets the username of a bot of the current generation, so that its
    /// `/cmd@bot` rules match, by recompiling the route table. The
    /// generation does not change.
    pub fn set_username(&self, source: SourceId, username: Option<String>) {
        let _write = self.shared.write.lock();
        let old = self.shared.state.load_full();
        let index = usize::from(source.0);
        if old
            .sources
            .get(index)
            .is_none_or(|s| s.username == username)
        {
            return;
        }
        let mut sources = old.sources.clone();
        sources[index].username = username;
        let table = match reload::compile(&old.config, &sources) {
            Ok(table) => table,
            Err(err) => {
                tracing::error!(error = %err, "recompiling the routes for a bot username failed");
                return;
            }
        };
        self.shared.state.store(Arc::new(EngineState {
            generation: old.generation,
            table,
            consumers: old.consumers.clone(),
            sources,
            targets: old.targets.clone(),
            config: Arc::clone(&old.config),
        }));
    }

    /// A snapshot for `/v1/status`; it holds no secrets.
    pub fn status(&self) -> EngineStatus {
        let state = self.shared.state.load();
        EngineStatus {
            generation: state.generation,
            consumers: state.consumers.iter().map(consumer_status).collect(),
        }
    }

    /// Starts the shutdown drain of every consumer, current and retired.
    pub fn begin_drain(&self) {
        self.shared.draining.store(true, Ordering::SeqCst);
        for c in &self.shared.state.load().consumers {
            c.begin_drain();
        }
        for slot in self.shared.retired.lock().iter() {
            slot.consumer.begin_drain();
        }
    }

    /// Updates not yet acked, delivered or dead-lettered over every
    /// consumer, including retired ones still draining.
    pub fn pending(&self) -> usize {
        let current: usize = self
            .shared
            .state
            .load()
            .consumers
            .iter()
            .map(Consumer::pending)
            .sum();
        let retired: usize = self
            .shared
            .retired
            .lock()
            .iter()
            .map(|s| s.consumer.pending())
            .sum();
        current + retired
    }

    /// Stops leasing to the consumer `name`; it still admits. `false` if
    /// there is no such consumer.
    pub fn pause(&self, name: &str) -> bool {
        let Some(consumer) = self.consumer(name) else {
            return false;
        };
        consumer.pause();
        true
    }

    /// Leases to the consumer `name` again. `false` if there is no such
    /// consumer.
    pub fn resume(&self, name: &str) -> bool {
        let Some(consumer) = self.consumer(name) else {
            return false;
        };
        consumer.resume();
        true
    }

    /// Drops the unleased updates of the consumer `name`, counted as
    /// `dropped{reason="purge"}`; `None` if there is no such consumer.
    pub fn purge(&self, name: &str) -> Option<usize> {
        let state = self.shared.state.load();
        let slot = &state.target(name)?.slot;
        let purged = slot.consumer.queue().purge();
        slot.metrics.dropped(
            ConsumerDropReason::Purge,
            u64::try_from(purged).unwrap_or(u64::MAX),
        );
        slot.consumer.update_depth();
        Some(purged)
    }
}

/// The engine's state for `/v1/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EngineStatus {
    pub generation: u64,
    pub consumers: Vec<ConsumerStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConsumerStatus {
    pub name: String,
    pub mode: Mode,
    /// Updates waiting to be leased.
    pub depth: usize,
    pub leased: usize,
    pub in_flight: usize,
    pub paused: bool,
    /// Push consumers only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<Vec<EndpointStatus>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EndpointStatus {
    pub index: usize,
    /// `scheme://host[:port]`, as config renderings show it.
    pub origin: String,
    pub breaker: BreakerStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakerStatus {
    Closed,
    Open,
    HalfOpen,
}

impl From<BreakerState> for BreakerStatus {
    fn from(state: BreakerState) -> Self {
        match state {
            BreakerState::Closed => Self::Closed,
            BreakerState::Open => Self::Open,
            BreakerState::HalfOpen => Self::HalfOpen,
        }
    }
}

fn consumer_status(c: &Consumer) -> ConsumerStatus {
    let stats = c.queue().stats();
    let endpoints = match c {
        Consumer::Pull(_) => None,
        Consumer::Push(push) => Some(
            push.endpoints()
                .into_iter()
                .enumerate()
                .map(|(index, (origin, breaker))| EndpointStatus {
                    index,
                    origin: origin.to_string(),
                    breaker: breaker.into(),
                })
                .collect(),
        ),
    };
    ConsumerStatus {
        name: c.name().to_owned(),
        mode: c.mode(),
        depth: stats.available,
        leased: stats.leased,
        in_flight: c.in_flight(),
        paused: c.is_paused(),
        endpoints,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use tgin_runtime::supervisor::TaskState;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn push_dispatcher_restarts_after_panic() {
        let supervisor = Supervisor::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        supervise_dispatcher(&supervisor, "h", CancellationToken::new(), move || {
            let n = counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                assert!(n > 0, "the first dispatcher panics");
                std::future::pending::<()>().await;
            })
        });
        let mut rx = supervisor.subscribe("push:h").expect("supervised");
        let state = rx
            .wait_for(|s| matches!(s, TaskState::Backoff { .. }))
            .await
            .expect("backoff")
            .clone();
        let TaskState::Backoff { last_error, .. } = state else {
            unreachable!()
        };
        assert_eq!(last_error, "dispatcher panicked");
        rx.wait_for(|s| *s == TaskState::Running)
            .await
            .expect("running again");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn returning_dispatcher_stops_and_cancel_aborts() {
        let supervisor = Supervisor::new();
        supervise_dispatcher(&supervisor, "done", CancellationToken::new(), || {
            tokio::spawn(async {})
        });
        let mut done = supervisor.subscribe("push:done").expect("supervised");
        done.wait_for(|s| *s == TaskState::Stopped)
            .await
            .expect("stopped");

        let cancel = CancellationToken::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let mut tx = Some(tx);
        supervise_dispatcher(&supervisor, "kill", cancel.clone(), move || {
            let tx = tx.take();
            tokio::spawn(async move {
                // Dropped, not completed, when the task is aborted.
                let _tx = tx;
                std::future::pending::<()>().await;
            })
        });
        let mut kill = supervisor.subscribe("push:kill").expect("supervised");
        kill.wait_for(|s| *s == TaskState::Running)
            .await
            .expect("running");
        cancel.cancel();
        kill.wait_for(|s| *s == TaskState::Stopped)
            .await
            .expect("stopped");
        assert!(rx.await.is_err(), "the dispatcher task was aborted");
    }
}
