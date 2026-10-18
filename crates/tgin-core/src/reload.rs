//! Hot reload: diffing a new config against the engine's state and swapping
//! the new state in.
//!
//! [`Engine::apply`] builds the complete new state first, so a config that
//! fails to build changes nothing. Then:
//! - a bot keeps its source id by name; a new bot gets an id never used
//!   before;
//! - a consumer with the same name and mode keeps its instance, and so its
//!   queue, pause flag and breakers unless its endpoints change; new
//!   parameters apply live;
//! - a kept push consumer keeps its runtime endpoint (from `setWebhook`)
//!   while it still accepts one and the bot that set it still feeds it;
//!   otherwise the endpoint is cleared;
//! - a removed consumer stops receiving updates at once and drains for
//!   `reload.drain_timeout`; what it still holds then is dropped
//!   (`dropped{reason="reload"}`), its tasks stop and its metric series go;
//! - a consumer whose mode changed is a new, empty instance; the old one
//!   drains as if removed, but its series stay for the new one;
//! - a consumer that comes back, with its mode, while its old instance
//!   still drains gets that instance back, with the new parameters.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tgin_runtime::breaker::Breaker;
use tgin_runtime::queue::{OnFull, QueueConfig};
use tgin_runtime::supervisor::TaskState;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::config::{self, Config, ConsumerKind, WebhookReply};
use crate::consumer::push::{Endpoint, PushOptions};
use crate::consumer::{PullConsumer, PushConsumer};
use crate::engine::{
    BuildError, Consumer, Engine, EngineRuntime, EngineState, Mode, Shared, Slot, SourceInfo,
    Target,
};
use crate::ids::SourceId;
use crate::metrics::{CoreMetrics, ReloadOutcome};
use crate::reply::ReplyHandle;
use crate::rules::{RouteTable, RuleError, RuleSpec};

/// How often a drain looks whether its consumer is empty, and the shortest
/// drain of a consumer that still holds updates.
const DRAIN_CHECK: Duration = Duration::from_millis(100);

/// What a reload changed. Names are sorted.
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
    /// Bots that are new: the caller starts them, with
    /// [`Engine::source_id`].
    pub sources_added: Vec<String>,
    /// Bots that are gone: the caller stops them.
    pub sources_removed: Vec<String>,
    /// Bots whose token, upstream (by name, kind, URL or local mode) or
    /// ingress changed: the caller restarts them.
    pub sources_restarted: Vec<String>,
    /// Bots now on another upstream (another name or URL); also in
    /// `sources_restarted`. Unless the old server logged the bot out
    /// first, Telegram may split its updates between the two servers.
    pub upstream_changed: Vec<String>,
    /// Changed settings that take effect only after a restart:
    /// `server.listen`, `server.max_connections`,
    /// `server.header_read_timeout`, `server.tls` (turned on or off),
    /// `admin.listen`, `runtime.worker_threads`, `log.format`.
    pub restart_required: Vec<String>,
}

impl Engine {
    /// Applies `new`: builds its state, applies changed consumer settings
    /// live, swaps the state in and starts the drains of removed and
    /// replaced consumers. The generation goes up by one.
    ///
    /// # Errors
    ///
    /// The config's rules do not compile, or no source id is left for a new
    /// bot; nothing changed. The caller counts such a rejection, as it
    /// counts configs that fail validation.
    pub fn apply(&self, new: &Config) -> Result<ReloadReport, BuildError> {
        let shared = &self.shared;
        let _write = shared.write.lock();
        let old = shared.state.load_full();
        let retired = shared.retired.lock().clone();
        let plan = Plan::build(
            new,
            Some(&old),
            &retired,
            &shared.metrics,
            &self.client,
            &shared.rt,
        )?;
        let (state, changes) = plan.into_parts();
        let report = commit(self, Some(state), changes);
        shared.metrics.reload(ReloadOutcome::Applied);
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
    /// Kept or revived instances with new parameters.
    live: Vec<Live>,
    /// New instances, to start.
    started: Vec<Arc<Slot>>,
    /// Draining instances that come back.
    revived: Vec<Arc<Slot>>,
    /// Removed or replaced instances, to drain.
    retiring: Vec<Arc<Slot>>,
    removed_sources: Vec<String>,
    drain_timeout: Duration,
    report: ReloadReport,
}

/// New parameters for a kept or revived instance.
struct Live {
    slot: Arc<Slot>,
    queue: Option<QueueConfig>,
    endpoints: Option<(Vec<Endpoint>, PushOptions)>,
    /// New options for a push consumer that keeps its endpoints (and their
    /// breakers).
    options: Option<PushOptions>,
}

/// A bot of the new generation before its metrics are resolved.
struct Bot<'a> {
    id: SourceId,
    config: &'a config::Bot,
    username: Option<String>,
    unmatched: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Plan {
    /// Builds the state for `cfg`, keeping what it can of `old` and
    /// reviving instances of `retired`. Fails before creating anything.
    pub(crate) fn build(
        cfg: &Config,
        old: Option<&EngineState>,
        retired: &[Arc<Slot>],
        metrics: &CoreMetrics,
        client: &reqwest::Client,
        rt: &EngineRuntime,
    ) -> Result<Self, BuildError> {
        let mut report = ReloadReport {
            generation: old.map_or(1, |old| old.generation + 1),
            ..ReloadReport::default()
        };

        // Source ids by name; ids are never reused.
        let mut names: Vec<Arc<str>> = old.map_or_else(Vec::new, |old| old.source_names.to_vec());
        let mut bots = Vec::with_capacity(cfg.bots.len());
        for bot in &cfg.bots {
            let kept = old.and_then(|old| old.source_named(&bot.name));
            let entry = match kept {
                Some(kept) => Bot {
                    id: kept.id,
                    config: bot,
                    username: old.and_then(|old| known_username(old, kept, bot)),
                    unmatched: Some(Arc::clone(&kept.unmatched)),
                },
                None => {
                    let id =
                        u16::try_from(names.len()).map_err(|_| BuildError::SourceIdsExhausted)?;
                    names.push(Arc::from(bot.name.as_str()));
                    report.sources_added.push(bot.name.clone());
                    Bot {
                        id: SourceId(id),
                        config: bot,
                        username: None,
                        unmatched: None,
                    }
                }
            };
            bots.push(entry);
        }
        let rules: Vec<(SourceId, Vec<RuleSpec>)> = bots
            .iter()
            .map(|b| (b.id, b.config.rules.clone()))
            .collect();
        let usernames: Vec<_> = bots.iter().map(|b| (b.id, b.username.clone())).collect();
        let table = RouteTable::compile(&rules, &usernames)?;

        let source_names: Arc<[Arc<str>]> = names.into();
        let names_grew = old.is_some_and(|old| old.source_names.len() != source_names.len());
        let mut sources: Vec<Option<SourceInfo>> = vec![None; source_names.len()];
        for bot in bots {
            sources[usize::from(bot.id.0)] = Some(SourceInfo {
                id: bot.id,
                name: Arc::from(bot.config.name.as_str()),
                username: bot.username,
                metrics: metrics.source(&bot.config.name),
                unmatched: bot.unmatched.unwrap_or_default(),
            });
        }

        let mut live = Vec::new();
        let mut started = Vec::new();
        let mut revived = Vec::new();
        let mut retiring = Vec::new();
        let mut targets = Vec::with_capacity(cfg.consumers.len());
        for consumer in &cfg.consumers {
            let mode = mode(&consumer.kind);
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
                Some((target, written)) if target.slot.mode() == mode => {
                    if written.kind != consumer.kind {
                        report.changed.push(consumer.name.clone());
                    }
                    live.extend(live_change(
                        &target.slot,
                        &written.kind,
                        &consumer.kind,
                        names_grew,
                        &source_names,
                        &rt.reply,
                    ));
                    Arc::clone(&target.slot)
                }
                previous => {
                    if let Some((target, _)) = previous {
                        report.changed.push(consumer.name.clone());
                        retiring.push(Arc::clone(&target.slot));
                    } else {
                        report.added.push(consumer.name.clone());
                    }
                    let draining = retired
                        .iter()
                        .find(|s| *s.name == *consumer.name && s.mode() == mode);
                    match draining {
                        Some(slot) => {
                            live.push(Live {
                                slot: Arc::clone(slot),
                                queue: Some(queue(&consumer.kind)),
                                endpoints: match &consumer.kind {
                                    ConsumerKind::Push(push) => Some((
                                        endpoints(push),
                                        options(push, &source_names, &rt.reply),
                                    )),
                                    ConsumerKind::Pull(_) => None,
                                },
                                options: None,
                            });
                            revived.push(Arc::clone(slot));
                            Arc::clone(slot)
                        }
                        None => {
                            let slot = new_slot(consumer, metrics, client, &source_names, rt);
                            started.push(Arc::clone(&slot));
                            slot
                        }
                    }
                }
            };
            targets.push(Target {
                slot,
                on_full: on_full(&consumer.kind),
                capacity_bytes: queue(&consumer.kind).capacity_bytes,
            });
        }

        let mut removed_sources = Vec::new();
        if let Some(old) = old {
            for target in &old.targets {
                if !cfg.consumers.iter().any(|c| *c.name == *target.slot.name) {
                    report.removed.push(target.slot.name.to_string());
                    retiring.push(Arc::clone(&target.slot));
                }
            }
            for bot in &old.config.bots {
                match cfg.bots.iter().find(|b| b.name == bot.name) {
                    None => {
                        report.sources_removed.push(bot.name.clone());
                        removed_sources.push(bot.name.clone());
                    }
                    Some(new) => {
                        let (old_upstream, new_upstream) = (&bot.upstream, &new.upstream);
                        let moved = !new_upstream.same_server(old_upstream);
                        if moved {
                            report.upstream_changed.push(new.name.clone());
                        }
                        if moved
                            || new.token.expose() != bot.token.expose()
                            || new_upstream.kind != old_upstream.kind
                            || new_upstream.local_mode != old_upstream.local_mode
                            || new.ingress != bot.ingress
                        {
                            report.sources_restarted.push(new.name.clone());
                        }
                    }
                }
            }
            report.restart_required = restart_required(&old.config, cfg);
        }
        for list in [
            &mut report.added,
            &mut report.removed,
            &mut report.changed,
            &mut report.sources_added,
            &mut report.sources_removed,
            &mut report.sources_restarted,
            &mut report.upstream_changed,
            &mut report.restart_required,
        ] {
            list.sort();
        }

        let state = EngineState {
            generation: report.generation,
            table,
            consumers: targets.iter().map(|t| t.slot.consumer.clone()).collect(),
            sources,
            targets,
            source_names,
            config: Arc::new(cfg.clone()),
        };
        Ok(Self {
            state,
            changes: Changes {
                live,
                started,
                revived,
                retiring,
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

/// Applies a plan's changes, with the engine's write lock held. `state`, if
/// given, is swapped in once the kept, revived and new instances are ready
/// and the retired ones drain.
pub(crate) fn commit(
    engine: &Engine,
    state: Option<EngineState>,
    changes: Changes,
) -> ReloadReport {
    let shared = &engine.shared;
    let draining = shared.draining.load(std::sync::atomic::Ordering::SeqCst);
    for change in changes.live {
        let consumer = &change.slot.consumer;
        if let Some(queue) = change.queue {
            consumer.queue().set_config(queue);
        }
        if let (Consumer::Push(push), Some((endpoints, opts))) = (consumer, change.endpoints) {
            push.set_endpoints(endpoints, opts);
        } else if let (Consumer::Push(push), Some(opts)) = (consumer, change.options) {
            push.set_options(opts);
        }
    }
    for slot in &changes.revived {
        if let Some(drain) = slot.drain.lock().take() {
            drain.cancel();
        }
        shared.retired.lock().retain(|s| !Arc::ptr_eq(s, slot));
        if !draining {
            slot.consumer.end_drain();
        }
    }
    for slot in &changes.started {
        slot.start(&shared.rt);
        if draining {
            slot.consumer.begin_drain();
        }
    }
    // Registered before the swap, so that `pending` and `begin_drain` never
    // miss them. The state still in effect is the one they were current in.
    let previous = shared.state.load();
    let mut drains = Vec::with_capacity(changes.retiring.len());
    for slot in changes.retiring {
        if slot.mode() == Mode::Pull {
            *slot.fed_by.lock() = previous.feeder(&slot.name);
        }
        let drain = CancellationToken::new();
        *slot.drain.lock() = Some(drain.clone());
        slot.consumer.begin_drain();
        shared.retired.lock().push(Arc::clone(&slot));
        drains.push((slot, drain));
    }
    drop(previous);
    {
        // A draining instance leaves the gauges of a name in use to the
        // current instance.
        let loaded;
        let current: &EngineState = match &state {
            Some(state) => state,
            None => {
                loaded = shared.state.load();
                &loaded
            }
        };
        for slot in current.targets.iter().map(|t| &t.slot) {
            slot.consumer.publish_gauges(true);
        }
        for slot in shared.retired.lock().iter() {
            slot.consumer
                .publish_gauges(current.target(&slot.name).is_none());
        }
    }
    if let Some(state) = state {
        // Before the swap: no update the new state admits may reach an
        // endpoint it no longer allows.
        keep_runtime_endpoints(shared, &state);
        evict_rotated_captures(shared, &state);
        shared.state.store(Arc::new(state));
    }
    for slot in changes.started.iter().chain(&changes.revived) {
        slot.consumer.update_depth();
    }
    for (slot, drain) in drains {
        tokio::spawn(drain_retired(
            Arc::clone(shared),
            slot,
            drain,
            changes.drain_timeout,
        ));
    }
    for name in &changes.removed_sources {
        shared.metrics.remove_source(name);
    }
    shared.metrics.set_generation(changes.report.generation);
    changes.report
}

/// Recompiles the route table of `cfg` with the ids and usernames of
/// `sources`.
pub(crate) fn compile(
    cfg: &Config,
    sources: &[Option<SourceInfo>],
) -> Result<RouteTable, RuleError> {
    let mut rules = Vec::with_capacity(cfg.bots.len());
    let mut usernames = Vec::with_capacity(cfg.bots.len());
    for info in sources.iter().flatten() {
        if let Some(bot) = cfg.bots.iter().find(|b| *b.name == *info.name) {
            rules.push((info.id, bot.rules.clone()));
            usernames.push((info.id, info.username.clone()));
        }
    }
    RouteTable::compile(&rules, &usernames)
}

/// Clears the runtime endpoint of every push consumer of `state` that no
/// longer accepts one, that the bot whose `setWebhook` set it no longer
/// feeds (R37), or whose bot's token changed since: a token holder never
/// redirects another bot's updates, not even across reloads, and rotating a
/// leaked token evicts what its holder set. Draining instances keep theirs
/// to deliver what they hold, unless their bot's token changed (see
/// [`evict_rotated_captures`]).
fn keep_runtime_endpoints(shared: &Shared, state: &EngineState) {
    for target in &state.targets {
        let Consumer::Push(push) = &target.slot.consumer else {
            continue;
        };
        let Some((source, fingerprint)) = push.runtime_endpoint_owner() else {
            continue;
        };
        let accepts = state.config.consumers.iter().any(|c| {
            *c.name == *target.slot.name
                && matches!(&c.kind, ConsumerKind::Push(p) if p.accept_set_webhook)
        });
        let reason = if !accepts {
            "accept_set_webhook is off"
        } else if !state.feeds(source, &target.slot.name) {
            "the bot whose setWebhook set it no longer feeds the consumer"
        } else if shared.token_fingerprint(state, source) != Some(fingerprint) {
            "the token of the bot whose setWebhook set it changed"
        } else {
            continue;
        };
        push.set_runtime_endpoint(None);
        tracing::info!(consumer = %target.slot.name, reason, "runtime endpoint cleared");
    }
}

/// Clears the runtime endpoint of every draining push instance whose bot's
/// token `state` changes: rotating a leaked token also stops the updates
/// a removed consumer still holds from reaching its holder's capture.
fn evict_rotated_captures(shared: &Shared, state: &EngineState) {
    for slot in shared.retired.lock().iter() {
        let Consumer::Push(push) = &slot.consumer else {
            continue;
        };
        let Some((source, fingerprint)) = push.runtime_endpoint_owner() else {
            continue;
        };
        if shared
            .token_fingerprint(state, source)
            .is_some_and(|current| current != fingerprint)
        {
            push.set_runtime_endpoint(None);
            tracing::info!(
                consumer = %slot.name,
                reason = "the token of the bot whose setWebhook set it changed",
                "runtime endpoint of a draining instance cleared"
            );
        }
    }
}

/// Waits until the retired instance is empty or the drain timeout (at least
/// [`DRAIN_CHECK`]) passes, then retires it for good, unless a reload
/// revives it first. Its dispatcher, if any, is forgotten once it stops.
async fn drain_retired(
    shared: Arc<Shared>,
    slot: Arc<Slot>,
    drain: CancellationToken,
    timeout: Duration,
) {
    let deadline = Instant::now().checked_add(timeout.max(DRAIN_CHECK));
    while slot.consumer.pending() > 0 {
        let now = Instant::now();
        if deadline.is_some_and(|deadline| now >= deadline) {
            break;
        }
        let check = now + DRAIN_CHECK;
        tokio::select! {
            () = drain.cancelled() => return,
            () = sleep_until(deadline.map_or(check, |deadline| deadline.min(check))) => {}
        }
    }
    let Some(retired) = shared.retire(&slot, &drain) else {
        return;
    };
    drop(slot);
    if let Some((name, mut rx)) = retired.dispatcher {
        // An error means the task is gone too.
        let _ = rx
            .wait_for(|s| matches!(s, TaskState::Stopped | TaskState::Failed { .. }))
            .await;
        shared.rt.supervisor.remove(&name);
    }
}

/// The username `kept` had in `old`, if its token did not change: another
/// token may be another bot.
fn known_username(old: &EngineState, kept: &SourceInfo, bot: &config::Bot) -> Option<String> {
    let previous = old.config.bots.iter().find(|b| b.name == bot.name)?;
    if previous.token.expose() != bot.token.expose() {
        return None;
    }
    kept.username.clone()
}

fn mode(kind: &ConsumerKind) -> Mode {
    match kind {
        ConsumerKind::Pull(_) => Mode::Pull,
        ConsumerKind::Push(_) => Mode::Push,
    }
}

fn queue(kind: &ConsumerKind) -> QueueConfig {
    match kind {
        ConsumerKind::Pull(pull) => pull.queue,
        ConsumerKind::Push(push) => push.queue(),
    }
}

fn on_full(kind: &ConsumerKind) -> OnFull {
    queue(kind).on_full
}

/// The live changes from `old` to `new` of the same mode, if any. A push
/// consumer gets new endpoints (and breakers) when its delivery settings
/// change; when only `webhook_reply` changes or new bots appear, it keeps
/// them and gets new options (the bots' names and the reply handle among
/// them).
fn live_change(
    slot: &Arc<Slot>,
    old: &ConsumerKind,
    new: &ConsumerKind,
    names_grew: bool,
    source_names: &Arc<[Arc<str>]>,
    reply: &ReplyHandle,
) -> Option<Live> {
    let (queue, endpoints, opts) = match (old, new) {
        (ConsumerKind::Pull(old), ConsumerKind::Pull(new)) => {
            ((old.queue != new.queue).then_some(new.queue), None, None)
        }
        (ConsumerKind::Push(old), ConsumerKind::Push(new)) => {
            let delivery = old.endpoints != new.endpoints
                || old.balance != new.balance
                || old.max_in_flight != new.max_in_flight
                || old.retry != new.retry
                || old.breaker != new.breaker;
            let refresh = names_grew || old.webhook_reply != new.webhook_reply;
            (
                (old.queue() != new.queue()).then(|| new.queue()),
                delivery.then(|| (endpoints(new), options(new, source_names, reply))),
                (!delivery && refresh).then(|| options(new, source_names, reply)),
            )
        }
        _ => (None, None, None),
    };
    (queue.is_some() || endpoints.is_some() || opts.is_some()).then(|| Live {
        slot: Arc::clone(slot),
        queue,
        endpoints,
        options: opts,
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
            options(push, source_names, &rt.reply),
            client.clone(),
            &rt.via,
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

/// A push consumer's options; `reply` goes with `webhook_reply: execute`.
fn options(
    push: &config::Push,
    source_names: &Arc<[Arc<str>]>,
    reply: &ReplyHandle,
) -> PushOptions {
    PushOptions {
        max_in_flight: push.max_in_flight,
        balance: push.balance,
        retry: push.retry,
        source_names: Arc::clone(source_names),
        breaker: push.breaker,
        runtime_timeout: config::DEFAULT_ENDPOINT_TIMEOUT,
        reply: (push.webhook_reply == WebhookReply::Execute).then(|| Arc::clone(reply)),
    }
}

/// The settings that changed and need a restart: what the process sets up
/// once at start. Every other setting applies live, in the engine or the
/// app.
fn restart_required(old: &Config, new: &Config) -> Vec<String> {
    let (was, is) = (old.server.as_ref(), new.server.as_ref());
    let tls = |server: Option<&config::Server>| server.is_some_and(|s| s.tls.is_some());
    let mut out = Vec::new();
    for (name, differs) in [
        (
            "server.listen",
            was.map(|s| &s.listen) != is.map(|s| &s.listen),
        ),
        (
            "server.max_connections",
            was.map(|s| s.max_connections) != is.map(|s| s.max_connections),
        ),
        (
            "server.header_read_timeout",
            was.map(|s| s.header_read_timeout) != is.map(|s| s.header_read_timeout),
        ),
        // A changed certificate or key file reloads live; TLS on or off
        // does not.
        ("server.tls", tls(was) != tls(is)),
        ("admin.listen", old.admin.listen != new.admin.listen),
        (
            "runtime.worker_threads",
            old.runtime.worker_threads != new.runtime.worker_threads,
        ),
        ("log.format", old.log.format != new.log.format),
    ] {
        if differs {
            out.push(name.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use prometheus_client::registry::Registry;
    use tgin_runtime::config::MapEnv;
    use tgin_runtime::supervisor::Supervisor;

    use super::*;

    fn load(yaml: &str) -> Config {
        config::load(yaml, Path::new("tgin.yaml"), &MapEnv::default()).expect("valid config")
    }

    #[test]
    fn new_bot_fails_once_every_source_id_was_given_out() {
        let metrics = CoreMetrics::register(&mut Registry::default());
        let client = reqwest::Client::new();
        let rt = EngineRuntime {
            supervisor: Arc::new(Supervisor::new()),
            stop: CancellationToken::new(),
            force: CancellationToken::new(),
            via: tgin_runtime::http::Via::generate(),
            reply: Default::default(),
        };
        let one = "version: 1\nbots:\n  - { name: a, token: \"1:a\" }\n";
        let build = |cfg: &Config, old: Option<&EngineState>| {
            Plan::build(cfg, old, &[], &metrics, &client, &rt).map(Plan::into_parts)
        };
        let (mut old, _) = build(&load(one), None).expect("build");
        // Every id below 65536 was given out since start; `a` holds id 0.
        old.source_names = (0..=u16::MAX)
            .map(|i| {
                Arc::from(if i == 0 {
                    "a".to_owned()
                } else {
                    format!("gone-{i}")
                })
            })
            .collect();

        let (state, _) = build(&load(one), Some(&old)).expect("a keeps its id");
        assert_eq!(state.source_named("a").map(|s| s.id), Some(SourceId(0)));
        let two = format!("{one}  - {{ name: b, token: \"2:b\" }}\n");
        assert!(matches!(
            build(&load(&two), Some(&old)),
            Err(BuildError::SourceIdsExhausted)
        ));
    }

    /// Gateway, setWebhook-capture and webhook-reply settings apply live:
    /// no restart, no source restart, the same consumer instance; a changed
    /// `webhook_reply` gives the push consumer new options.
    #[tokio::test]
    async fn gateway_and_webhook_reply_changes_apply_live() {
        let metrics = CoreMetrics::register(&mut Registry::default());
        let client = reqwest::Client::new();
        let rt = EngineRuntime {
            supervisor: Arc::new(Supervisor::new()),
            stop: CancellationToken::new(),
            force: CancellationToken::new(),
            via: tgin_runtime::http::Via::generate(),
            reply: Default::default(),
        };
        let build = |cfg: &Config, old: Option<&EngineState>| {
            Plan::build(cfg, old, &[], &metrics, &client, &rt).map(Plan::into_parts)
        };
        let base = "version: 1\nserver: {}\nbots:\n  - { name: a, token: \"1:a\", routes: [{ to: [h, q] }] }\nconsumers:\n  h: { push: { endpoints: [{ url: \"http://h/\" }] } }\n  q: { pull: {} }\n";
        let (old, _) = build(&load(base), None).expect("build");

        let gateway = base.replace(
            "token: \"1:a\",",
            "token: \"1:a\", gateway: { upstream_idle_timeout: 5s, rate_limit: { mode: reject } },",
        );
        let (_, changes) = build(&load(&gateway), Some(&old)).expect("build");
        let report = &changes.report;
        assert!(report.sources_restarted.is_empty(), "{report:?}");
        assert!(report.restart_required.is_empty(), "{report:?}");
        assert!(report.changed.is_empty(), "{report:?}");

        let ignore = base.replace("}] } }", "}], webhook_reply: ignore } }");
        let (_, changes) = build(&load(&ignore), Some(&old)).expect("build");
        assert_eq!(changes.report.changed, ["h"]);
        assert!(changes.retiring.is_empty() && changes.started.is_empty());
        let [live] = changes.live.as_slice() else {
            panic!("one live change");
        };
        assert_eq!(&*live.slot.name, "h");
        assert!(live.endpoints.is_none(), "breakers reset");
        let opts = live.options.as_ref().expect("push options refreshed");
        assert!(opts.reply.is_none(), "ignore drops the reply handle");

        // Back to `execute`: the options carry the runtime's one handle.
        let (ignored, _) = build(&load(&ignore), None).expect("build");
        let (_, changes) = build(&load(base), Some(&ignored)).expect("build");
        let [live] = changes.live.as_slice() else {
            panic!("one live change");
        };
        assert!(live.endpoints.is_none(), "breakers reset");
        let opts = live.options.as_ref().expect("push options refreshed");
        let handle = opts.reply.as_ref().expect("the reply handle");
        assert!(Arc::ptr_eq(handle, &rt.reply));

        let flags = base
            .replace(
                "}] } }",
                "}], accept_set_webhook: true, honor_drop_pending: true } }",
            )
            .replace(
                "q: { pull: {} }",
                "q: { pull: { honor_drop_pending: true } }",
            );
        let (_, changes) = build(&load(&flags), Some(&old)).expect("build");
        assert_eq!(changes.report.changed, ["h", "q"]);
        assert!(changes.retiring.is_empty() && changes.started.is_empty());
        assert!(changes.report.restart_required.is_empty());
    }
}
