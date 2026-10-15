//! Core metric families and pre-resolved handles.
//!
//! [`CoreMetrics::register`] adds every family to a registry that carries the
//! `tgin` prefix. [`SourceMetrics`], [`ConsumerMetrics`] and
//! [`EndpointMetrics`] hold handles for every label combination of one
//! source, consumer or push endpoint, resolved once, so the hot path never
//! hashes labels. A handle obtained before [`CoreMetrics::remove_source`] /
//! [`CoreMetrics::remove_consumer`] keeps counting but is no longer exported;
//! re-resolve after removal.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use parking_lot::Mutex;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

macro_rules! label_enum {
    ($(#[$doc:meta])* $name:ident { $($variant:ident => $label:literal),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: [Self; [$(Self::$variant),+].len()] = [$(Self::$variant),+];

            /// The exported label value.
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $label),+ }
            }
        }
    };
}

label_enum!(
    /// `reason` of `tgin_source_errors_total`.
    SourceErrorReason {
        Network => "network",
        Http => "http",
        Decode => "decode",
        Unauthorized => "unauthorized",
        RateLimited => "rate_limited",
        Conflict => "conflict",
    }
);

label_enum!(
    /// `reason` of `tgin_source_dropped_total`.
    SourceDropReason { BufferFull => "buffer_full" }
);

label_enum!(
    /// `state` of `tgin_source_state`.
    SourceStateLabel {
        Starting => "starting",
        Running => "running",
        Backoff => "backoff",
        Failed => "failed",
        Stopped => "stopped",
    }
);

label_enum!(
    /// `reason` of `tgin_consumer_dropped_total`.
    ConsumerDropReason {
        Overflow => "overflow",
        DeadLetter => "dead_letter",
        Reload => "reload",
        Purge => "purge",
    }
);

label_enum!(
    /// `outcome` of `tgin_delivery_attempts_total`.
    DeliveryOutcome {
        Delivered => "delivered",
        Retry => "retry",
        DeadLetter => "dead_letter",
    }
);

label_enum!(
    /// `outcome` of `tgin_config_reloads_total`.
    ReloadOutcome {
        Applied => "applied",
        Rejected => "rejected",
    }
);

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SourceLabels {
    source: Arc<str>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SourceReasonLabels {
    source: Arc<str>,
    reason: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SourceStateLabels {
    source: Arc<str>,
    state: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ConsumerLabels {
    consumer: Arc<str>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ConsumerReasonLabels {
    consumer: Arc<str>,
    reason: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct AttemptLabels {
    consumer: Arc<str>,
    /// The endpoint's index in its consumer's list, in decimal.
    endpoint: Arc<str>,
    outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OutcomeLabels {
    outcome: &'static str,
}

/// How many endpoint indices each consumer resolved handles for, so that
/// [`CoreMetrics::remove_consumer`] finds every label set it created.
type EndpointCounts = Arc<Mutex<HashMap<Arc<str>, usize>>>;

type SecondsCounter = Counter<f64, AtomicU64>;
type HistogramCtor = fn() -> Histogram;

fn latency_histogram() -> Histogram {
    Histogram::new(exponential_buckets(0.001, 2.0, 16))
}

/// The core metric families. Cloning shares the families.
#[derive(Clone, Debug)]
pub struct CoreMetrics {
    source_updates: Family<SourceLabels, Counter>,
    source_errors: Family<SourceReasonLabels, Counter>,
    source_dropped: Family<SourceReasonLabels, Counter>,
    source_state: Family<SourceStateLabels, Gauge>,
    routes_unmatched: Family<SourceLabels, Counter>,
    consumer_queue_depth: Family<ConsumerLabels, Gauge>,
    consumer_leased: Family<ConsumerLabels, Gauge>,
    consumer_in_flight: Family<ConsumerLabels, Gauge>,
    consumer_dropped: Family<ConsumerReasonLabels, Counter>,
    consumer_backpressure: Family<ConsumerLabels, SecondsCounter>,
    delivery_attempts: Family<AttemptLabels, Counter>,
    delivery_latency: Family<ConsumerLabels, Histogram, HistogramCtor>,
    /// Handles of `tgin_config_reloads_total`, by [`ReloadOutcome`].
    config_reloads: [Counter; ReloadOutcome::ALL.len()],
    config_generation: Gauge,
    shutdown_abandoned: Counter,
    endpoints: EndpointCounts,
}

impl CoreMetrics {
    /// Creates the families and registers them (names without the `tgin` prefix).
    pub fn register(reg: &mut Registry) -> CoreMetrics {
        let config_reloads: Family<OutcomeLabels, Counter> = Family::default();
        let m = CoreMetrics {
            source_updates: Family::default(),
            source_errors: Family::default(),
            source_dropped: Family::default(),
            source_state: Family::default(),
            routes_unmatched: Family::default(),
            consumer_queue_depth: Family::default(),
            consumer_leased: Family::default(),
            consumer_in_flight: Family::default(),
            consumer_dropped: Family::default(),
            consumer_backpressure: Family::default(),
            delivery_attempts: Family::default(),
            delivery_latency: Family::new_with_constructor(latency_histogram),
            config_reloads: ReloadOutcome::ALL.map(|o| {
                config_reloads.get_or_create_owned(&OutcomeLabels {
                    outcome: o.as_str(),
                })
            }),
            config_generation: Gauge::default(),
            shutdown_abandoned: Counter::default(),
            endpoints: EndpointCounts::default(),
        };
        reg.register(
            "source_updates",
            "Updates received per source",
            m.source_updates.clone(),
        );
        reg.register(
            "source_errors",
            "Source ingest errors by reason",
            m.source_errors.clone(),
        );
        reg.register(
            "source_dropped",
            "Updates a source dropped by reason",
            m.source_dropped.clone(),
        );
        reg.register(
            "source_state",
            "Source state (1 = current)",
            m.source_state.clone(),
        );
        reg.register(
            "routes_unmatched",
            "Updates no rule matched",
            m.routes_unmatched.clone(),
        );
        reg.register(
            "consumer_queue_depth",
            "Updates waiting in a consumer queue",
            m.consumer_queue_depth.clone(),
        );
        reg.register(
            "consumer_leased",
            "Updates leased to a consumer",
            m.consumer_leased.clone(),
        );
        reg.register(
            "consumer_in_flight",
            "Deliveries in flight to a consumer",
            m.consumer_in_flight.clone(),
        );
        reg.register(
            "consumer_dropped",
            "Updates a consumer dropped by reason",
            m.consumer_dropped.clone(),
        );
        reg.register(
            "consumer_backpressure_seconds",
            "Seconds a consumer queue applied backpressure",
            m.consumer_backpressure.clone(),
        );
        reg.register(
            "delivery_attempts",
            "Delivery attempts by endpoint and outcome",
            m.delivery_attempts.clone(),
        );
        reg.register(
            "delivery_latency_seconds",
            "Delivery latency",
            m.delivery_latency.clone(),
        );
        reg.register(
            "config_reloads",
            "Config reloads by outcome",
            config_reloads,
        );
        reg.register(
            "config_generation",
            "Generation of the config in effect",
            m.config_generation.clone(),
        );
        reg.register(
            "shutdown_abandoned",
            "Updates still pending when shutdown ended",
            m.shutdown_abandoned.clone(),
        );
        m
    }

    /// One config reload attempt with its outcome.
    pub fn reload(&self, outcome: ReloadOutcome) {
        self.config_reloads[outcome as usize].inc();
    }

    pub fn set_generation(&self, generation: u64) {
        self.config_generation
            .set(i64::try_from(generation).unwrap_or(i64::MAX));
    }

    /// Updates left pending when shutdown gave up on them.
    pub fn shutdown_abandoned(&self, n: u64) {
        self.shutdown_abandoned.inc_by(n);
    }

    /// Resolves the handles of every label combination of one source.
    pub fn source(&self, name: &str) -> SourceMetrics {
        let source: Arc<str> = Arc::from(name);
        let labels = SourceLabels {
            source: source.clone(),
        };
        SourceMetrics {
            updates: self.source_updates.get_or_create_owned(&labels),
            errors: SourceErrorReason::ALL.map(|r| {
                self.source_errors.get_or_create_owned(&SourceReasonLabels {
                    source: source.clone(),
                    reason: r.as_str(),
                })
            }),
            dropped: SourceDropReason::ALL.map(|r| {
                self.source_dropped
                    .get_or_create_owned(&SourceReasonLabels {
                        source: source.clone(),
                        reason: r.as_str(),
                    })
            }),
            state: SourceStateLabel::ALL.map(|s| {
                self.source_state.get_or_create_owned(&SourceStateLabels {
                    source: source.clone(),
                    state: s.as_str(),
                })
            }),
            unmatched: self.routes_unmatched.get_or_create_owned(&labels),
        }
    }

    /// Resolves the handles of every label combination of one consumer;
    /// [`ConsumerMetrics::endpoint`] resolves those of its push endpoints.
    pub fn consumer(&self, name: &str) -> ConsumerMetrics {
        let consumer: Arc<str> = Arc::from(name);
        let labels = ConsumerLabels {
            consumer: consumer.clone(),
        };
        ConsumerMetrics {
            depth: self.consumer_queue_depth.get_or_create_owned(&labels),
            leased: self.consumer_leased.get_or_create_owned(&labels),
            in_flight: self.consumer_in_flight.get_or_create_owned(&labels),
            dropped: ConsumerDropReason::ALL.map(|r| {
                self.consumer_dropped
                    .get_or_create_owned(&ConsumerReasonLabels {
                        consumer: consumer.clone(),
                        reason: r.as_str(),
                    })
            }),
            backpressure: self.consumer_backpressure.get_or_create_owned(&labels),
            latency: self.delivery_latency.get_or_create_owned(&labels),
            attempts: self.delivery_attempts.clone(),
            endpoints: Arc::clone(&self.endpoints),
            consumer,
        }
    }

    /// Removes every series of one source from the exposition.
    pub fn remove_source(&self, name: &str) {
        let source: Arc<str> = Arc::from(name);
        let labels = SourceLabels {
            source: source.clone(),
        };
        self.source_updates.remove(&labels);
        self.routes_unmatched.remove(&labels);
        for r in SourceErrorReason::ALL {
            self.source_errors.remove(&SourceReasonLabels {
                source: source.clone(),
                reason: r.as_str(),
            });
        }
        for r in SourceDropReason::ALL {
            self.source_dropped.remove(&SourceReasonLabels {
                source: source.clone(),
                reason: r.as_str(),
            });
        }
        for s in SourceStateLabel::ALL {
            self.source_state.remove(&SourceStateLabels {
                source: source.clone(),
                state: s.as_str(),
            });
        }
    }

    /// Removes every series of one consumer from the exposition, including
    /// those of every endpoint index ever resolved for it.
    pub fn remove_consumer(&self, name: &str) {
        let consumer: Arc<str> = Arc::from(name);
        let labels = ConsumerLabels {
            consumer: consumer.clone(),
        };
        self.consumer_queue_depth.remove(&labels);
        self.consumer_leased.remove(&labels);
        self.consumer_in_flight.remove(&labels);
        self.consumer_backpressure.remove(&labels);
        self.delivery_latency.remove(&labels);
        for r in ConsumerDropReason::ALL {
            self.consumer_dropped.remove(&ConsumerReasonLabels {
                consumer: consumer.clone(),
                reason: r.as_str(),
            });
        }
        let mut endpoints = self.endpoints.lock();
        let count = endpoints.remove(&consumer).unwrap_or(0);
        for index in 0..count {
            let endpoint: Arc<str> = Arc::from(index.to_string());
            for o in DeliveryOutcome::ALL {
                self.delivery_attempts.remove(&AttemptLabels {
                    consumer: consumer.clone(),
                    endpoint: endpoint.clone(),
                    outcome: o.as_str(),
                });
            }
        }
    }
}

/// Pre-resolved metric handles for one source.
#[derive(Clone, Debug)]
pub struct SourceMetrics {
    updates: Counter,
    errors: [Counter; SourceErrorReason::ALL.len()],
    dropped: [Counter; SourceDropReason::ALL.len()],
    state: [Gauge; SourceStateLabel::ALL.len()],
    unmatched: Counter,
}

impl SourceMetrics {
    /// One update received.
    pub fn update(&self) {
        self.updates.inc();
    }

    pub fn error(&self, reason: SourceErrorReason) {
        self.errors[reason as usize].inc();
    }

    pub fn dropped(&self, reason: SourceDropReason, n: u64) {
        self.dropped[reason as usize].inc_by(n);
    }

    /// Sets the gauge of `state` to 1 and the others to 0.
    pub fn set_state(&self, state: SourceStateLabel) {
        for (i, g) in self.state.iter().enumerate() {
            g.set(i64::from(i == state as usize));
        }
    }

    /// One update no rule matched.
    pub fn unmatched(&self) {
        self.unmatched.inc();
    }
}

/// Pre-resolved metric handles for one consumer.
#[derive(Clone, Debug)]
pub struct ConsumerMetrics {
    depth: Gauge,
    leased: Gauge,
    in_flight: Gauge,
    dropped: [Counter; ConsumerDropReason::ALL.len()],
    backpressure: SecondsCounter,
    latency: Histogram,
    consumer: Arc<str>,
    attempts: Family<AttemptLabels, Counter>,
    endpoints: EndpointCounts,
}

impl ConsumerMetrics {
    pub fn set_depth(&self, available: i64, leased: i64, in_flight: i64) {
        self.depth.set(available);
        self.leased.set(leased);
        self.in_flight.set(in_flight);
    }

    pub fn dropped(&self, reason: ConsumerDropReason, n: u64) {
        self.dropped[reason as usize].inc_by(n);
    }

    pub fn backpressure(&self, seconds: f64) {
        self.backpressure.inc_by(seconds);
    }

    /// Resolves the `tgin_delivery_attempts_total` handles of the endpoint at
    /// `index`, labelled with the index in decimal. Call it when endpoints
    /// are set or replaced, not per attempt.
    pub fn endpoint(&self, index: usize) -> EndpointMetrics {
        let endpoint: Arc<str> = Arc::from(index.to_string());
        // Recorded under the lock that `remove_consumer` takes, so a removal
        // sees either none of these series or all of them.
        let mut endpoints = self.endpoints.lock();
        let count = endpoints.entry(self.consumer.clone()).or_default();
        *count = (*count).max(index.saturating_add(1));
        EndpointMetrics {
            attempts: DeliveryOutcome::ALL.map(|o| {
                self.attempts.get_or_create_owned(&AttemptLabels {
                    consumer: self.consumer.clone(),
                    endpoint: endpoint.clone(),
                    outcome: o.as_str(),
                })
            }),
        }
    }

    pub fn latency(&self, seconds: f64) {
        self.latency.observe(seconds);
    }
}

/// Pre-resolved `tgin_delivery_attempts_total` handles of one push endpoint.
#[derive(Clone, Debug)]
pub struct EndpointMetrics {
    attempts: [Counter; DeliveryOutcome::ALL.len()],
}

impl EndpointMetrics {
    pub fn attempt(&self, outcome: DeliveryOutcome) {
        self.attempts[outcome as usize].inc();
    }
}

#[cfg(test)]
mod tests {
    use prometheus_client::encoding::text::encode;
    use prometheus_client::registry::Registry;

    use super::*;

    fn setup() -> (Registry, CoreMetrics) {
        let mut reg = Registry::with_prefix("tgin");
        let m = CoreMetrics::register(&mut reg);
        (reg, m)
    }

    fn text(reg: &Registry) -> String {
        let mut s = String::new();
        encode(&mut s, reg).expect("encode");
        s
    }

    fn lines(s: &str) -> Vec<&str> {
        s.lines().filter(|l| !l.starts_with('#')).collect()
    }

    #[test]
    fn metrics_names_and_labels_exact() {
        let (reg, m) = setup();
        let s = m.source("s1");
        s.update();
        s.error(SourceErrorReason::RateLimited);
        s.dropped(SourceDropReason::BufferFull, 2);
        s.set_state(SourceStateLabel::Running);
        s.unmatched();
        let c = m.consumer("c1");
        c.set_depth(3, 2, 1);
        c.dropped(ConsumerDropReason::DeadLetter, 1);
        c.backpressure(1.5);
        c.endpoint(0).attempt(DeliveryOutcome::Delivered);
        c.endpoint(1).attempt(DeliveryOutcome::Retry);
        c.latency(0.002);
        m.reload(ReloadOutcome::Applied);
        m.set_generation(7);
        m.shutdown_abandoned(4);
        let out = text(&reg);
        let got = lines(&out);
        for want in [
            r#"tgin_source_updates_total{source="s1"} 1"#,
            r#"tgin_source_errors_total{source="s1",reason="rate_limited"} 1"#,
            r#"tgin_source_errors_total{source="s1",reason="network"} 0"#,
            r#"tgin_source_dropped_total{source="s1",reason="buffer_full"} 2"#,
            r#"tgin_source_state{source="s1",state="running"} 1"#,
            r#"tgin_source_state{source="s1",state="starting"} 0"#,
            r#"tgin_routes_unmatched_total{source="s1"} 1"#,
            r#"tgin_consumer_queue_depth{consumer="c1"} 3"#,
            r#"tgin_consumer_leased{consumer="c1"} 2"#,
            r#"tgin_consumer_in_flight{consumer="c1"} 1"#,
            r#"tgin_consumer_dropped_total{consumer="c1",reason="dead_letter"} 1"#,
            r#"tgin_consumer_dropped_total{consumer="c1",reason="overflow"} 0"#,
            r#"tgin_consumer_backpressure_seconds_total{consumer="c1"} 1.5"#,
            r#"tgin_delivery_attempts_total{consumer="c1",endpoint="0",outcome="delivered"} 1"#,
            r#"tgin_delivery_attempts_total{consumer="c1",endpoint="0",outcome="dead_letter"} 0"#,
            r#"tgin_delivery_attempts_total{consumer="c1",endpoint="1",outcome="retry"} 1"#,
            r#"tgin_delivery_attempts_total{consumer="c1",endpoint="1",outcome="delivered"} 0"#,
            r#"tgin_delivery_latency_seconds_count{consumer="c1"} 1"#,
            r#"tgin_delivery_latency_seconds_bucket{le="0.002",consumer="c1"} 1"#,
            r#"tgin_delivery_latency_seconds_bucket{le="0.001",consumer="c1"} 0"#,
            r#"tgin_config_reloads_total{outcome="applied"} 1"#,
            r#"tgin_config_reloads_total{outcome="rejected"} 0"#,
            "tgin_config_generation 7",
            "tgin_shutdown_abandoned_total 4",
        ] {
            assert!(got.contains(&want), "missing {want:?} in\n{out}");
        }
        for name in [
            "tgin_source_updates",
            "tgin_source_errors",
            "tgin_source_dropped",
            "tgin_source_state",
            "tgin_routes_unmatched",
            "tgin_consumer_queue_depth",
            "tgin_consumer_leased",
            "tgin_consumer_in_flight",
            "tgin_consumer_dropped",
            "tgin_consumer_backpressure_seconds",
            "tgin_delivery_attempts",
            "tgin_delivery_latency_seconds",
            "tgin_config_reloads",
            "tgin_config_generation",
            "tgin_shutdown_abandoned",
        ] {
            assert!(out.contains(&format!("# TYPE {name} ")), "no family {name}");
        }
    }

    #[test]
    fn handles_share_state_with_family() {
        let (reg, m) = setup();
        let a = m.source("s1");
        let b = m.source("s1");
        a.update();
        b.update();
        assert!(lines(&text(&reg)).contains(&r#"tgin_source_updates_total{source="s1"} 2"#));
    }

    #[test]
    fn remove_consumer_drops_series() {
        let (reg, m) = setup();
        let c1 = m.consumer("c1");
        // Endpoint sets resolved over time, through different handles.
        for i in 0..3 {
            c1.endpoint(i).attempt(DeliveryOutcome::Retry);
        }
        m.consumer("c1")
            .endpoint(0)
            .attempt(DeliveryOutcome::Delivered);
        m.consumer("c2").endpoint(0).attempt(DeliveryOutcome::Retry);
        m.source("s1").update();
        m.remove_consumer("c1");
        let out = text(&reg);
        assert!(!out.contains(r#"consumer="c1""#), "{out}");
        assert!(out.contains(r#"consumer="c2""#));
        m.remove_source("s1");
        assert!(!text(&reg).contains(r#"source="s1""#));
    }

    #[test]
    fn removed_consumer_resolves_again() {
        let (reg, m) = setup();
        m.consumer("c1").endpoint(2).attempt(DeliveryOutcome::Retry);
        m.remove_consumer("c1");
        m.consumer("c1")
            .endpoint(0)
            .attempt(DeliveryOutcome::Delivered);
        m.remove_consumer("c1");
        assert!(!text(&reg).contains(r#"consumer="c1""#));
    }

    #[test]
    fn source_state_is_one_hot() {
        let (reg, m) = setup();
        let s = m.source("s1");
        for state in SourceStateLabel::ALL {
            s.set_state(state);
            let out = text(&reg);
            let got = lines(&out);
            for other in SourceStateLabel::ALL {
                let v = u8::from(other == state);
                let want = format!(
                    r#"tgin_source_state{{source="s1",state="{}"}} {v}"#,
                    other.as_str()
                );
                assert!(got.contains(&want.as_str()), "missing {want} in\n{out}");
            }
        }
    }
}
