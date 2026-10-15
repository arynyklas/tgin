//! Hot reload: diffing a new config against the engine's state and swapping
//! the new state in.
//!
//! [`Engine::apply`] builds the complete new state first, so a config that
//! fails to build changes nothing. Then:
//! - a consumer with the same name and mode keeps its instance, and so its
//!   queue, pause flag and breakers unless its endpoints change; new
//!   parameters apply live;
//! - a removed consumer stops receiving updates at once and drains for
//!   `reload.drain_timeout`; what it still holds then is dropped
//!   (`dropped{reason="reload"}`), its tasks stop and its metric series go;
//! - a consumer whose mode changed is a new, empty instance; the old one
//!   drains as if removed, but its series stay for the new one.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tgin_runtime::breaker::Breaker;
use tgin_runtime::queue::OnFull;
use tokio::time::{Instant, sleep_until};

use crate::config::{self, Config, ConsumerKind};
use crate::consumer::push::{Endpoint, PushOptions};
use crate::consumer::{PullConsumer, PushConsumer};
use crate::engine::{
    BuildError, Consumer, Engine, EngineRuntime, EngineState, Shared, Slot, SourceInfo, Target,
};
use crate::metrics::{CoreMetrics, ReloadOutcome};
use crate::rules::{RouteTable, RuleError};

/// How often a drain looks whether its consumer is empty, and how long it
/// waits at least: an admission that loaded the previous state may still
/// commit to the instance, and must find it draining, not retired.
const DRAIN_CHECK: Duration = Duration::from_millis(100);

/// What a reload changed. Consumer and bot names are sorted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ReloadReport {
    /// The generation now in effect.
    pub generation: u64,
    /// Consumers that are new.
    pub added: Vec<String>,
    /// Consumers that are gone; they drain, then drop.
    pub removed: Vec<String>,
    /// Consumers whose settings or mode changed.
    pub changed: Vec<String>,
    /// Bots whose token, `api_url` or ingress changed: the caller restarts
    /// them.
    pub sources_restarted: Vec<String>,
    /// Changed settings that take effect only after a restart:
    /// `server.listen`, `admin.listen`, `runtime.worker_threads`.
    pub restart_required: Vec<String>,
}

impl Engine {
    /// Applies `new`: builds its state, applies changed consumer settings
    /// live, swaps the state in and starts the drains of removed and
    /// replaced consumers. The generation goes up by one.
    ///
    /// # Errors
    ///
    /// The config's rules do not compile; nothing changed. The caller
    /// counts such a rejection, as it counts configs that fail validation.
    pub fn apply(&self, new: &Config) -> Result<ReloadReport, BuildError> {
        let _write = self.shared.write.lock();
        let old = self.shared.state.load_full();
        let plan = Plan::build(
            new,
            Some(&old),
            &self.shared.metrics,
            &self.client,
            &self.rt,
        )?;
        let (state, changes) = plan.into_parts();
        let report = commit(self, Some(state), changes);
        self.shared.metrics.reload(ReloadOutcome::Applied);
        tracing::info!(
            generation = report.generation,
            added = report.added.len(),
            removed = report.removed.len(),
            changed = report.changed.len(),
            "config applied"
        );
        Ok(report)
    }
}

/// A new state with the changes it needs, nothing applied yet.
pub(crate) struct Plan {
    state: EngineState,
    changes: Changes,
}

pub(crate) struct Changes {
    /// Kept instances with new parameters.
    live: Vec<Live>,
    /// New instances, to start.
    started: Vec<Arc<Slot>>,
    /// Removed or replaced instances, to drain.
    retired: Vec<Arc<Slot>>,
    removed_sources: Vec<String>,
    drain_timeout: Duration,
    report: ReloadReport,
}

/// New parameters for a kept instance.
struct Live {
    slot: Arc<Slot>,
    queue: Option<tgin_runtime::queue::QueueConfig>,
    endpoints: Option<(Vec<Endpoint>, PushOptions)>,
}

impl Plan {
    /// Builds the state for `cfg`, keeping what it can of `old`. Fails
    /// before creating anything.
    pub(crate) fn build(
        cfg: &Config,
        old: Option<&EngineState>,
        metrics: &CoreMetrics,
        client: &reqwest::Client,
        rt: &EngineRuntime,
    ) -> Result<Self, BuildError> {
        let usernames: Vec<Option<String>> = cfg
            .bots
            .iter()
            .map(|bot| old.and_then(|old| known_username(old, bot)))
            .collect();
        let rules: Vec<_> = cfg.bots.iter().map(|b| (b.id, b.rules.clone())).collect();
        let named: Vec<_> = cfg
            .bots
            .iter()
            .zip(&usernames)
            .map(|(b, u)| (b.id, u.clone()))
            .collect();
        let table = RouteTable::compile(&rules, &named)?;

        let sources: Vec<SourceInfo> = cfg
            .bots
            .iter()
            .zip(usernames)
            .map(|(bot, username)| SourceInfo {
                id: bot.id,
                name: Arc::from(bot.name.as_str()),
                username,
                metrics: metrics.source(&bot.name),
            })
            .collect();
        let source_names: Arc<[Arc<str>]> = sources.iter().map(|s| Arc::clone(&s.name)).collect();
        let names_changed = old.is_some_and(|old| {
            old.sources
                .iter()
                .map(|s| &s.name)
                .ne(sources.iter().map(|s| &s.name))
        });

        let mut report = ReloadReport {
            generation: old.map_or(1, |old| old.generation + 1),
            ..ReloadReport::default()
        };
        let mut live = Vec::new();
        let mut started = Vec::new();
        let mut retired = Vec::new();
        let mut targets = Vec::with_capacity(cfg.consumers.len());
        for consumer in &cfg.consumers {
            let previous = old.and_then(|old| {
                let target = old.target(&consumer.name)?;
                let written = old
                    .config
                    .consumers
                    .iter()
                    .find(|c| c.name == consumer.name)?;
                Some((target, written))
            });
            let slot = match previous {
                Some((target, written)) if same_mode(&written.kind, &consumer.kind) => {
                    if written.kind != consumer.kind {
                        report.changed.push(consumer.name.clone());
                    }
                    let change = live_change(
                        &target.slot,
                        &written.kind,
                        &consumer.kind,
                        names_changed,
                        &source_names,
                    );
                    live.extend(change);
                    Arc::clone(&target.slot)
                }
                previous => {
                    if let Some((target, _)) = previous {
                        report.changed.push(consumer.name.clone());
                        retired.push(Arc::clone(&target.slot));
                    } else {
                        report.added.push(consumer.name.clone());
                    }
                    let slot = new_slot(consumer, metrics, client, &source_names, rt);
                    started.push(Arc::clone(&slot));
                    slot
                }
            };
            targets.push(Target {
                slot,
                on_full: on_full(&consumer.kind),
            });
        }

        let mut removed_sources = Vec::new();
        if let Some(old) = old {
            for target in &old.targets {
                if !cfg.consumers.iter().any(|c| *c.name == *target.slot.name) {
                    report.removed.push(target.slot.name.to_string());
                    retired.push(Arc::clone(&target.slot));
                }
            }
            for bot in &old.config.bots {
                match cfg.bots.iter().find(|b| b.name == bot.name) {
                    None => removed_sources.push(bot.name.clone()),
                    Some(new) => {
                        if new.token.expose() != bot.token.expose()
                            || new.api_url != bot.api_url
                            || new.ingress != bot.ingress
                        {
                            report.sources_restarted.push(new.name.clone());
                        }
                    }
                }
            }
            report.restart_required = restart_required(&old.config, cfg);
        }
        report.added.sort();
        report.removed.sort();
        report.changed.sort();
        report.sources_restarted.sort();

        let state = EngineState {
            generation: report.generation,
            table,
            consumers: targets.iter().map(|t| t.slot.consumer.clone()).collect(),
            sources,
            targets,
            config: Arc::new(cfg.clone()),
        };
        Ok(Self {
            state,
            changes: Changes {
                live,
                started,
                retired,
                removed_sources,
                drain_timeout: cfg.reload.drain_timeout,
                report,
            },
        })
    }

    pub(crate) fn into_parts(self) -> (EngineState, Changes) {
        (self.state, self.changes)
    }
}

/// Applies a plan's changes; `state`, if given, is swapped in after the
/// kept and new instances are ready and before the retired ones drain.
pub(crate) fn commit(
    engine: &Engine,
    state: Option<EngineState>,
    changes: Changes,
) -> ReloadReport {
    let shared = &engine.shared;
    for change in changes.live {
        let consumer = &change.slot.consumer;
        if let Some(queue) = change.queue {
            consumer.queue().set_config(queue);
        }
        if let (Consumer::Push(push), Some((endpoints, opts))) = (consumer, change.endpoints) {
            push.set_endpoints(endpoints, opts);
        }
    }
    let draining = shared.draining.load(std::sync::atomic::Ordering::SeqCst);
    for slot in &changes.started {
        slot.start(&engine.rt);
        if draining {
            slot.consumer.begin_drain();
        }
    }
    if let Some(state) = state {
        shared.state.store(Arc::new(state));
    }
    for slot in changes.retired {
        slot.consumer.begin_drain();
        shared.retired.lock().push(Arc::clone(&slot));
        tokio::spawn(drain(Arc::clone(shared), slot, changes.drain_timeout));
    }
    for name in &changes.removed_sources {
        shared.metrics.remove_source(name);
    }
    shared.metrics.set_generation(changes.report.generation);
    changes.report
}

/// Recompiles the route table of `cfg` with the usernames of `sources`.
pub(crate) fn compile(cfg: &Config, sources: &[SourceInfo]) -> Result<RouteTable, RuleError> {
    let rules: Vec<_> = cfg.bots.iter().map(|b| (b.id, b.rules.clone())).collect();
    let named: Vec<_> = sources.iter().map(|s| (s.id, s.username.clone())).collect();
    RouteTable::compile(&rules, &named)
}

/// Waits until the retired instance is empty (after at least one
/// [`DRAIN_CHECK`]) or `timeout` passes, then retires it for good.
async fn drain(shared: Arc<Shared>, slot: Arc<Slot>, timeout: Duration) {
    let start = Instant::now();
    let deadline = start.checked_add(timeout);
    let grace = start + DRAIN_CHECK;
    loop {
        let now = Instant::now();
        let empty = now >= grace && slot.consumer.pending() == 0;
        if empty || deadline.is_some_and(|deadline| now >= deadline) {
            break;
        }
        let check = now + DRAIN_CHECK;
        sleep_until(deadline.map_or(check, |deadline| deadline.min(check))).await;
    }
    shared.retire(&slot);
}

/// The username of `bot` in `old`, if its token did not change: another
/// token may be another bot.
fn known_username(old: &EngineState, bot: &config::Bot) -> Option<String> {
    let previous = old.config.bots.iter().find(|b| b.name == bot.name)?;
    if previous.token.expose() != bot.token.expose() {
        return None;
    }
    old.sources
        .get(usize::from(previous.id.0))?
        .username
        .clone()
}

fn same_mode(a: &ConsumerKind, b: &ConsumerKind) -> bool {
    matches!(
        (a, b),
        (ConsumerKind::Pull(_), ConsumerKind::Pull(_))
            | (ConsumerKind::Push(_), ConsumerKind::Push(_))
    )
}

fn on_full(kind: &ConsumerKind) -> OnFull {
    match kind {
        ConsumerKind::Pull(pull) => pull.queue.on_full,
        ConsumerKind::Push(push) => push.on_full,
    }
}

/// The live changes from `old` to `new` of the same mode, if any. A push
/// consumer gets new endpoints (and breakers) when its delivery settings or
/// the bot names it reports change.
fn live_change(
    slot: &Arc<Slot>,
    old: &ConsumerKind,
    new: &ConsumerKind,
    names_changed: bool,
    source_names: &Arc<[Arc<str>]>,
) -> Option<Live> {
    let (queue, endpoints) = match (old, new) {
        (ConsumerKind::Pull(old), ConsumerKind::Pull(new)) => {
            ((old.queue != new.queue).then_some(new.queue), None)
        }
        (ConsumerKind::Push(old), ConsumerKind::Push(new)) => {
            let delivery = old.endpoints != new.endpoints
                || old.balance != new.balance
                || old.max_in_flight != new.max_in_flight
                || old.retry != new.retry
                || old.breaker != new.breaker;
            (
                (old.queue() != new.queue()).then(|| new.queue()),
                (delivery || names_changed).then(|| (endpoints(new), options(new, source_names))),
            )
        }
        _ => (None, None),
    };
    (queue.is_some() || endpoints.is_some()).then(|| Live {
        slot: Arc::clone(slot),
        queue,
        endpoints,
    })
}

fn new_slot(
    consumer: &config::Consumer,
    metrics: &CoreMetrics,
    client: &reqwest::Client,
    source_names: &Arc<[Arc<str>]>,
    rt: &EngineRuntime,
) -> Arc<Slot> {
    let name: Arc<str> = Arc::from(consumer.name.as_str());
    let handles = metrics.consumer(&name);
    let instance = match &consumer.kind {
        ConsumerKind::Pull(pull) => Consumer::Pull(Arc::new(PullConsumer::new(
            Arc::clone(&name),
            pull.queue,
            handles.clone(),
        ))),
        ConsumerKind::Push(push) => Consumer::Push(Arc::new(PushConsumer::new(
            Arc::clone(&name),
            push.queue(),
            endpoints(push),
            options(push, source_names),
            client.clone(),
            handles.clone(),
        ))),
    };
    Arc::new(Slot::new(name, instance, handles, rt))
}

fn endpoints(push: &config::Push) -> Vec<Endpoint> {
    push.endpoints
        .iter()
        .map(|endpoint| Endpoint {
            url: endpoint.url.expose().clone(),
            origin: Arc::from(endpoint.url.origin()),
            secret_token: endpoint.secret_token.clone(),
            timeout: endpoint.timeout,
            breaker: Breaker::new(push.breaker),
        })
        .collect()
}

fn options(push: &config::Push, source_names: &Arc<[Arc<str>]>) -> PushOptions {
    PushOptions {
        max_in_flight: push.max_in_flight,
        balance: push.balance,
        retry: push.retry,
        source_names: Arc::clone(source_names),
    }
}

/// The settings that changed and need a restart.
fn restart_required(old: &Config, new: &Config) -> Vec<String> {
    let listen = |cfg: &Config| cfg.server.as_ref().map(|s| s.listen.clone());
    let mut out = Vec::new();
    if listen(old) != listen(new) {
        out.push("server.listen".to_owned());
    }
    if old.admin.listen != new.admin.listen {
        out.push("admin.listen".to_owned());
    }
    if old.runtime.worker_threads != new.runtime.worker_threads {
        out.push("runtime.worker_threads".to_owned());
    }
    out
}
