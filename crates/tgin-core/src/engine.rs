//! The engine: the routing state of one config generation, the consumers it
//! routes to, and the control-plane operations on them.
//!
//! [`Engine`] keeps its [`EngineState`] behind an `ArcSwap`. Admission (the
//! engine's [`Sink`](crate::sink::Sink) implementation) loads it once per
//! update; a reload ([`Engine::apply`]) and a bot username
//! ([`Engine::set_username`]) swap in a new one.
//!
//! Source ids are the engine's, not the config's: a bot keeps its id, by
//! name, for as long as it stays configured, and an id is never given to
//! another bot. Sources must admit with [`Engine::source_id`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use serde::Serialize;
use tgin_runtime::backoff::Backoff;
use tgin_runtime::breaker::BreakerState;
use tgin_runtime::queue::{LeaseQueue, OnFull};
use tgin_runtime::supervisor::{Exit, Supervisor, TaskState};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use crate::config::{self, Config};
use crate::consumer::{PullConsumer, PushConsumer};
use crate::envelope::Envelope;
use crate::ids::{ConsumerId, SourceId};
use crate::metrics::{ConsumerDropReason, ConsumerMetrics, CoreMetrics, SourceMetrics};
use crate::reload::{self, Plan};
use crate::rules::{MatchScratch, RouteTable, RuleError, TargetSpec};

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
    #[error(
        "no source ids are left for new bots: every one of the 65536 ids was given out since \
         tgin started; restart tgin to apply this config"
    )]
    SourceIdsExhausted,
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

    pub(crate) fn end_drain(&self) {
        match self {
            Self::Pull(c) => c.end_drain(),
            Self::Push(c) => c.end_drain(),
        }
    }

    pub(crate) fn publish_gauges(&self, on: bool) {
        match self {
            Self::Pull(c) => c.publish_gauges(on),
            Self::Push(c) => c.publish_gauges(on),
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
    /// The engine's id of the bot; see [`Engine::source_id`].
    pub id: SourceId,
    pub name: Arc<str>,
    /// The bot's username, once known: `/cmd@bot` rules match only then.
    pub username: Option<String>,
    pub metrics: SourceMetrics,
    /// Set while the bot's updates match no rule, so that only the first of
    /// a run is logged as a warning. Shared across generations.
    pub(crate) unmatched: Arc<AtomicBool>,
}

impl SourceInfo {
    /// Counts and logs an update no rule matched.
    pub(crate) fn unmatched(&self, update_id: i64, update_type: &str) {
        self.metrics.unmatched();
        tracing::debug!(source = %self.name, update_id, update_type, "no rule matched the update");
        if !self.unmatched.load(Ordering::Relaxed)
            && self
                .unmatched
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(
                source = %self.name,
                "updates match no rule; they are dropped until one matches"
            );
        }
    }

    /// An update matched a rule: ends a run of unmatched ones.
    pub(crate) fn matched(&self) {
        if self.unmatched.load(Ordering::Relaxed) {
            self.unmatched.store(false, Ordering::Relaxed);
        }
    }
}

/// The routing state of one config generation.
pub struct EngineState {
    pub generation: u64,
    pub table: RouteTable,
    /// Indexed by [`ConsumerId`](crate::ids::ConsumerId).
    pub consumers: Vec<Consumer>,
    /// Indexed by the engine's [`SourceId`]; `None` for ids of bots no
    /// longer configured.
    pub sources: Vec<Option<SourceInfo>>,
    /// Admission's view of `consumers`, by the same index.
    pub(crate) targets: Vec<Target>,
    /// The name of every source id ever given out, by id; push consumers
    /// name the source of queued updates with it.
    pub(crate) source_names: Arc<[Arc<str>]>,
    /// The config this state was built from, for the next reload's diff.
    pub(crate) config: Arc<Config>,
}

impl EngineState {
    pub(crate) fn target(&self, name: &str) -> Option<&Target> {
        self.targets.iter().find(|t| &*t.slot.name == name)
    }

    pub(crate) fn source(&self, id: SourceId) -> Option<&SourceInfo> {
        self.sources.get(usize::from(id.0))?.as_ref()
    }

    pub(crate) fn source_named(&self, name: &str) -> Option<&SourceInfo> {
        self.sources.iter().flatten().find(|s| &*s.name == name)
    }

    /// Whether a rule of the bot `source` targets the consumer `consumer`.
    pub(crate) fn feeds(&self, source: SourceId, consumer: &str) -> bool {
        let Some(info) = self.source(source) else {
            return false;
        };
        let Some(id) = self
            .config
            .consumers
            .iter()
            .find(|c| c.name == consumer)
            .map(|c| c.id)
        else {
            return false;
        };
        self.config
            .bots
            .iter()
            .find(|bot| *bot.name == *info.name)
            .is_some_and(|bot| bot_targets(bot, id))
    }

    /// The first bot whose rules target the consumer `consumer`; a pull
    /// consumer has exactly one.
    pub(crate) fn feeder(&self, consumer: &str) -> Option<SourceId> {
        self.sources
            .iter()
            .flatten()
            .map(|info| info.id)
            .find(|&id| self.feeds(id, consumer))
    }
}

fn bot_targets(bot: &config::Bot, consumer: ConsumerId) -> bool {
    fn reaches(spec: &TargetSpec, consumer: ConsumerId) -> bool {
        match spec {
            TargetSpec::Consumer(id) => *id == consumer,
            TargetSpec::All(among) | TargetSpec::Split { among, .. } => {
                among.iter().any(|spec| reaches(spec, consumer))
            }
        }
    }
    bot.rules
        .iter()
        .any(|rule| rule.targets.iter().any(|spec| reaches(spec, consumer)))
}

/// A consumer as admission sees it in one generation.
#[derive(Clone)]
pub(crate) struct Target {
    pub(crate) slot: Arc<Slot>,
    pub(crate) on_full: OnFull,
    pub(crate) capacity_bytes: usize,
}

/// One consumer instance with what the engine keeps for it. It lives as long
/// as the instance, across the reloads that keep it.
pub(crate) struct Slot {
    pub(crate) name: Arc<str>,
    pub(crate) consumer: Consumer,
    pub(crate) metrics: ConsumerMetrics,
    /// The instant up to which backpressure time is counted, as nanoseconds
    /// after `epoch` plus one; 0 while the queue accepts.
    pressured_since: AtomicU64,
    epoch: Instant,
    /// Set while the queue drops updates for overflow, so that only the
    /// first drop of a run is logged as a warning.
    overflowing: AtomicBool,
    /// Set when the instance is retired for good: admission must not queue
    /// to it any more.
    pub(crate) closed: AtomicBool,
    /// Stops the instance's tasks gracefully; a child of the engine's stop.
    pub(crate) stop: CancellationToken,
    /// Ends the supervision of its push dispatcher, aborting it; a child of
    /// the engine's force.
    pub(crate) kill: CancellationToken,
    /// While the instance drains after a reload: cancelling it calls the
    /// drain off.
    pub(crate) drain: Mutex<Option<CancellationToken>>,
    /// The bot that fed a pull instance when a reload retired it, so that
    /// [`Engine::routes_to`] keeps answering for it while it drains.
    pub(crate) fed_by: Mutex<Option<SourceId>>,
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
            overflowing: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            stop: rt.stop.child_token(),
            kill: rt.force.child_token(),
            drain: Mutex::new(None),
            fed_by: Mutex::new(None),
        }
    }

    pub(crate) fn mode(&self) -> Mode {
        self.consumer.mode()
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn mark(&self, now: Instant) -> u64 {
        let nanos = now.saturating_duration_since(self.epoch).as_nanos();
        u64::try_from(nanos)
            .unwrap_or(u64::MAX - 1)
            .saturating_add(1)
    }

    /// Counts the backpressure time from `mark` (if any) up to `now`.
    fn count_pressure(&self, mark: u64, now: Instant) {
        if let Some(nanos) = mark.checked_sub(1) {
            let since = self.epoch + Duration::from_nanos(nanos);
            let seconds = now.saturating_duration_since(since).as_secs_f64();
            self.metrics.backpressure(seconds);
        }
    }

    /// The queue refused with backpressure: counts the time since the
    /// episode began or the previous refusal.
    pub(crate) fn pressured(&self, now: Instant) {
        let previous = self.pressured_since.swap(self.mark(now), Ordering::Relaxed);
        self.count_pressure(previous, now);
    }

    /// The queue reserved room: counts the rest of the episode, if any.
    pub(crate) fn relieved(&self, now: Instant) {
        if self.pressured_since.load(Ordering::Relaxed) == 0 {
            return;
        }
        let previous = self.pressured_since.swap(0, Ordering::Relaxed);
        self.count_pressure(previous, now);
    }

    /// `n` updates dropped for overflow; the first of a run is logged.
    pub(crate) fn overflowed(&self, n: u64) {
        self.metrics.dropped(ConsumerDropReason::Overflow, n);
        if !self.overflowing.load(Ordering::Relaxed)
            && self
                .overflowing
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(
                consumer = %self.name,
                reason = ConsumerDropReason::Overflow.as_str(),
                dropped = self.metrics.dropped_count(ConsumerDropReason::Overflow),
                "the consumer queue is full; dropping updates"
            );
        }
    }

    /// A reservation that dropped nothing: ends a run of overflow drops.
    pub(crate) fn reserved_cleanly(&self) {
        if self.overflowing.load(Ordering::Relaxed) {
            self.overflowing.store(false, Ordering::Relaxed);
        }
    }

    /// Drops what an admission committed after the instance was retired.
    pub(crate) fn drain_late(&self) {
        let dropped = self.consumer.queue().drain_all().len();
        if dropped > 0 {
            self.metrics.dropped(
                ConsumerDropReason::Reload,
                u64::try_from(dropped).unwrap_or(u64::MAX),
            );
            tracing::warn!(
                consumer = %self.name,
                dropped,
                "dropped updates queued to a consumer after a reload retired it"
            );
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

/// The supervisor's name of a push consumer's dispatcher.
pub(crate) fn dispatcher_name(consumer: &str) -> String {
    format!("push:{consumer}")
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
        Arc::from(dispatcher_name(consumer)),
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
}

/// What the engine shares with its drain tasks.
pub(crate) struct Shared {
    pub(crate) state: ArcSwap<EngineState>,
    pub(crate) metrics: CoreMetrics,
    pub(crate) rt: EngineRuntime,
    /// Serializes state writers: reloads, usernames, the shutdown drain and
    /// the end of reload drains. Taken before `retired`.
    pub(crate) write: Mutex<()>,
    /// Removed or replaced instances whose drain has not ended.
    pub(crate) retired: Mutex<Vec<Arc<Slot>>>,
    /// Set by [`Engine::begin_drain`]; instances started later drain too.
    pub(crate) draining: AtomicBool,
}

/// A retired instance's dispatcher, to forget once it has stopped.
pub(crate) struct Retired {
    pub(crate) dispatcher: Option<(String, watch::Receiver<TaskState>)>,
}

impl Shared {
    /// Ends the drain `drain` of a retired instance, unless a reload called
    /// it off: closes the instance to admission, drops what it still holds,
    /// stops its tasks, and removes its metric series unless an instance of
    /// the same name still uses them.
    pub(crate) fn retire(&self, slot: &Arc<Slot>, drain: &CancellationToken) -> Option<Retired> {
        let _write = self.write.lock();
        if drain.is_cancelled() {
            return None;
        }
        {
            let mut retired = self.retired.lock();
            let index = retired.iter().position(|s| Arc::ptr_eq(s, slot))?;
            retired.remove(index);
        }
        slot.drain.lock().take();
        // Before `drain_all`: an admission that commits after it sees the
        // flag and drops what it committed.
        slot.closed.store(true, Ordering::SeqCst);
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
        // Taken before the supervision ends, while no other dispatcher of
        // this name can exist (a new one of the same mode would have
        // revived this instance).
        let dispatcher = match slot.consumer {
            Consumer::Push(_) => {
                let name = dispatcher_name(&slot.name);
                self.rt.supervisor.subscribe(&name).map(|rx| (name, rx))
            }
            Consumer::Pull(_) => None,
        };
        slot.stop.cancel();
        slot.kill.cancel();
        let state = self.state.load();
        let current = state.target(&slot.name);
        let in_use = current.is_some() || self.retired.lock().iter().any(|s| s.name == slot.name);
        if !in_use {
            self.metrics.remove_consumer(&slot.name);
        }
        if let Some(current) = current {
            current.slot.consumer.update_depth();
        }
        Some(Retired { dispatcher })
    }
}

impl Engine {
    /// Builds the engine for `cfg` as generation 1 and starts every
    /// consumer's tasks. Bots get source ids in config order.
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
        let plan = Plan::build(cfg, None, &[], &metrics, &client, &rt)?;
        let (state, changes) = plan.into_parts();
        let engine = Self {
            shared: Arc::new(Shared {
                state: ArcSwap::from_pointee(state),
                metrics,
                rt,
                write: Mutex::new(()),
                retired: Mutex::new(Vec::new()),
                draining: AtomicBool::new(false),
            }),
            scratch: Mutex::new(Vec::new()),
            client,
        };
        {
            let _write = engine.shared.write.lock();
            reload::commit(&engine, None, changes);
        }
        Ok(engine)
    }

    /// The current state.
    pub fn state(&self) -> Arc<EngineState> {
        self.shared.state.load_full()
    }

    /// The engine's id of the bot `name`, which its source must admit with.
    /// It stays the same across reloads that keep the bot.
    pub fn source_id(&self, name: &str) -> Option<SourceId> {
        self.shared.state.load().source_named(name).map(|s| s.id)
    }

    /// The current instance of the consumer `name`.
    pub fn consumer(&self, name: &str) -> Option<Consumer> {
        let state = self.shared.state.load();
        state.target(name).map(|t| t.slot.consumer.clone())
    }

    /// The pull consumer `name` to poll: the current instance, else an
    /// instance a reload removed or replaced that still drains, so that its
    /// clients can fetch and ack what it holds.
    pub fn pull_consumer(&self, name: &str) -> Option<Arc<PullConsumer>> {
        if let Some(Consumer::Pull(c)) = self.consumer(name) {
            return Some(c);
        }
        self.shared
            .retired
            .lock()
            .iter()
            .filter(|s| &*s.name == name)
            .find_map(|s| match &s.consumer {
                Consumer::Pull(c) => Some(Arc::clone(c)),
                Consumer::Push(_) => None,
            })
    }

    /// Whether the bot `source` routes to the consumer `consumer`: a rule of
    /// the current config targets it, or it is a pull instance a reload
    /// removed or replaced that still drains and that `source` fed.
    pub fn routes_to(&self, source: SourceId, consumer: &str) -> bool {
        if self.shared.state.load().feeds(source, consumer) {
            return true;
        }
        self.shared.retired.lock().iter().any(|slot| {
            *slot.name == *consumer
                && slot.mode() == Mode::Pull
                && *slot.fed_by.lock() == Some(source)
        })
    }

    /// Sets the username of a bot of the current generation, so that its
    /// `/cmd@bot` rules match, by recompiling the route table. The
    /// generation does not change.
    pub fn set_username(&self, source: SourceId, username: Option<String>) {
        let _write = self.shared.write.lock();
        let old = self.shared.state.load_full();
        if old.source(source).is_none_or(|s| s.username == username) {
            return;
        }
        let mut sources = old.sources.clone();
        if let Some(Some(info)) = sources.get_mut(usize::from(source.0)) {
            info.username = username;
        }
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
            source_names: Arc::clone(&old.source_names),
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

    /// Starts the shutdown drain of every consumer, current and retired,
    /// and of every instance a later reload starts.
    pub fn begin_drain(&self) {
        let _write = self.shared.write.lock();
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

    /// Leases to the consumer `name` again, and ends a drain that
    /// [`Engine::drain`] began, unless the whole engine drains (shutdown).
    /// `false` if there is no such consumer.
    pub fn resume(&self, name: &str) -> bool {
        // Serialized with `begin_drain`, so a shutdown drain is never undone.
        let _write = self.shared.write.lock();
        let Some(consumer) = self.consumer(name) else {
            return false;
        };
        consumer.resume();
        if !self.shared.draining.load(Ordering::SeqCst) {
            consumer.end_drain();
        }
        true
    }

    /// Starts the drain of the consumer `name` alone: pull polls on its
    /// empty queue answer within the drain hold, so its clients wind down;
    /// [`Engine::resume`] ends it. `false` if there is no such consumer.
    pub fn drain(&self, name: &str) -> bool {
        let Some(consumer) = self.consumer(name) else {
            return false;
        };
        consumer.begin_drain();
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
