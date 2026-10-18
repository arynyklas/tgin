//! The gateway's metric families and pre-resolved handles.
//!
//! [`GatewayMetrics::register`] adds every gateway family once, to a
//! registry that carries the `tgin` prefix. [`BotMetrics`] holds one bot's
//! handles: those of its rate limiter and retries resolved up front, those
//! of each method resolved on the method's first request and kept by method
//! index, so a request never hashes labels after that and an unused method
//! exports no series. [`UpstreamMetrics`] holds a bot's `tgin_bot_info`
//! series and its `tgin_upstream_up` handle, for as long as the bot keeps
//! its upstream. Labels name the bot, the method and the upstream, never a
//! token, a URL or a path.

use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;
use tgin_core::label_enum;

use super::methods::{METHOD_COUNT, MethodId, OTHER_LABEL, lookup};

label_enum!(
    /// `outcome` of `tgin_gateway_requests_total`.
    Outcome {
        Ok => "ok",
        Rejected => "rejected",
        RateLimited => "rate_limited",
        UpstreamError => "upstream_error",
        Unreachable => "unreachable",
        Local => "local",
        ClientClosed => "client_closed",
    }
);

label_enum!(
    /// `scope` of `tgin_gateway_ratelimited_total`.
    RateScope {
        Global => "global",
        Chat => "chat",
        Group => "group",
    }
);

label_enum!(
    /// `action` of `tgin_gateway_ratelimited_total`.
    RateAction {
        Delayed => "delayed",
        Rejected => "rejected",
    }
);

label_enum!(
    /// `outcome` of `tgin_webhook_replies_total`.
    ReplyOutcome {
        Executed => "executed",
        Failed => "failed",
        Refused => "refused",
        TooLarge => "too_large",
        RateLimited => "rate_limited",
    }
);

/// The `method` label of file downloads.
pub const FILE_LABEL: &str = "file";

/// Every `method` label: the table's methods, then `other`, then `file`.
const METHOD_LABELS: usize = METHOD_COUNT + 2;

/// The `method` label of a gateway request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MethodLabel {
    /// A method of the table, labelled by its canonical name.
    Method(MethodId),
    /// A method missing from the table.
    Other,
    /// A file download.
    File,
}

impl MethodLabel {
    /// The label of the method named `method`, in any case.
    pub fn of(method: &str) -> MethodLabel {
        lookup(method).map_or(MethodLabel::Other, MethodLabel::Method)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MethodLabel::Method(id) => id.label(),
            MethodLabel::Other => OTHER_LABEL,
            MethodLabel::File => FILE_LABEL,
        }
    }

    fn index(self) -> usize {
        match self {
            MethodLabel::Method(id) => id.index(),
            MethodLabel::Other => METHOD_COUNT,
            MethodLabel::File => METHOD_COUNT + 1,
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RequestLabels {
    bot: Arc<str>,
    method: &'static str,
    outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BotMethodLabels {
    bot: Arc<str>,
    method: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RateLabels {
    bot: Arc<str>,
    scope: &'static str,
    action: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BotLabels {
    bot: Arc<str>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct InfoLabels {
    bot: Arc<str>,
    upstream: Arc<str>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ReplyLabels {
    consumer: Arc<str>,
    outcome: &'static str,
}

type HistogramCtor = fn() -> Histogram;

/// Time to response headers: 1 ms to about 33 s, as delivery latency.
fn upstream_histogram() -> Histogram {
    Histogram::new(exponential_buckets(0.001, 2.0, 16))
}

/// The gateway metric families. Cloning shares the families.
#[derive(Clone, Debug)]
pub struct GatewayMetrics {
    requests: Family<RequestLabels, Counter>,
    upstream: Family<BotMethodLabels, Histogram, HistogramCtor>,
    ratelimited: Family<RateLabels, Counter>,
    retries: Family<BotLabels, Counter>,
    unauthorized: Counter,
    webhook_replies: Family<ReplyLabels, Counter>,
    bot_info: Family<InfoLabels, Gauge>,
    upstream_up: Family<BotLabels, Gauge>,
}

impl GatewayMetrics {
    /// Creates every gateway family and registers it (names without the
    /// `tgin` prefix).
    pub fn register(reg: &mut Registry) -> GatewayMetrics {
        let m = GatewayMetrics {
            requests: Family::default(),
            upstream: Family::new_with_constructor(upstream_histogram),
            ratelimited: Family::default(),
            retries: Family::default(),
            unauthorized: Counter::default(),
            webhook_replies: Family::default(),
            bot_info: Family::default(),
            upstream_up: Family::default(),
        };
        reg.register(
            "gateway_requests",
            "Gateway requests by bot, method and outcome",
            m.requests.clone(),
        );
        reg.register(
            "gateway_upstream_seconds",
            "Time from sending a gateway request to Telegram's response headers",
            m.upstream.clone(),
        );
        reg.register(
            "gateway_ratelimited",
            "Gateway requests the rate limiter delayed or rejected, by scope",
            m.ratelimited.clone(),
        );
        reg.register(
            "gateway_retries",
            "Gateway requests sent again after an upstream 429",
            m.retries.clone(),
        );
        reg.register(
            "gateway_unauthorized",
            "Gateway requests whose bot token matches no configured bot",
            m.unauthorized.clone(),
        );
        reg.register(
            "webhook_replies",
            "Webhook replies of push consumers by outcome",
            m.webhook_replies.clone(),
        );
        reg.register(
            "bot_info",
            "1 for each bot, naming its upstream (`api_url` for the shorthand)",
            m.bot_info.clone(),
        );
        reg.register(
            "upstream_up",
            "1 after any HTTP answer from the bot's upstream, 0 after a connect failure, a timeout or a lost connection; absent before the first",
            m.upstream_up.clone(),
        );
        m
    }

    /// A request whose token matches no configured bot.
    pub fn unauthorized(&self) {
        self.unauthorized.inc();
    }

    /// The handles of the bot `name`.
    pub fn bot(&self, name: &str) -> BotMetrics {
        let bot: Arc<str> = Arc::from(name);
        let ratelimited = RateScope::ALL.map(|scope| {
            RateAction::ALL.map(|action| {
                self.ratelimited.get_or_create_owned(&RateLabels {
                    bot: Arc::clone(&bot),
                    scope: scope.as_str(),
                    action: action.as_str(),
                })
            })
        });
        let retries = self.retries.get_or_create_owned(&BotLabels {
            bot: Arc::clone(&bot),
        });
        BotMetrics(Arc::new(BotHandles {
            families: self.clone(),
            methods: (0..METHOD_LABELS).map(|_| OnceLock::new()).collect(),
            ratelimited,
            retries,
            bot,
        }))
    }

    /// Removes every series `bot`'s handles created. Its handles keep
    /// counting, unexported.
    pub fn remove_bot(&self, bot: &BotMetrics) {
        let name = &bot.0.bot;
        for scope in RateScope::ALL {
            for action in RateAction::ALL {
                self.ratelimited.remove(&RateLabels {
                    bot: Arc::clone(name),
                    scope: scope.as_str(),
                    action: action.as_str(),
                });
            }
        }
        self.retries.remove(&BotLabels {
            bot: Arc::clone(name),
        });
        for method in bot.0.methods.iter().filter_map(OnceLock::get) {
            for outcome in Outcome::ALL {
                self.requests.remove(&RequestLabels {
                    bot: Arc::clone(name),
                    method: method.label,
                    outcome: outcome.as_str(),
                });
            }
            self.upstream.remove(&BotMethodLabels {
                bot: Arc::clone(name),
                method: method.label,
            });
        }
    }

    /// The `tgin_webhook_replies_total` handles of the push consumer
    /// `consumer`.
    pub fn replies(&self, consumer: &str) -> ReplyMetrics {
        let consumer: Arc<str> = Arc::from(consumer);
        ReplyMetrics(ReplyOutcome::ALL.map(|outcome| {
            self.webhook_replies.get_or_create_owned(&ReplyLabels {
                consumer: Arc::clone(&consumer),
                outcome: outcome.as_str(),
            })
        }))
    }

    /// Removes the `tgin_webhook_replies_total` series of `consumer`.
    pub fn remove_replies(&self, consumer: &str) {
        let consumer: Arc<str> = Arc::from(consumer);
        for outcome in ReplyOutcome::ALL {
            self.webhook_replies.remove(&ReplyLabels {
                consumer: Arc::clone(&consumer),
                outcome: outcome.as_str(),
            });
        }
    }

    /// The bot `bot`'s series for its upstream `upstream` (the upstream's
    /// name): `tgin_bot_info` set to 1 now, `tgin_upstream_up` on its
    /// first outcome.
    pub fn upstream(&self, bot: &str, upstream: &str) -> UpstreamMetrics {
        let bot: Arc<str> = Arc::from(bot);
        let info = InfoLabels {
            bot: Arc::clone(&bot),
            upstream: Arc::from(upstream),
        };
        self.bot_info.get_or_create(&info).set(1);
        UpstreamMetrics {
            info,
            up: UpstreamUp(Arc::new(UpState {
                family: self.upstream_up.clone(),
                labels: BotLabels { bot },
                gauge: OnceLock::new(),
                retired: Mutex::new(false),
            })),
        }
    }

    /// Removes the series of `upstream`; its `tgin_upstream_up` handle no
    /// longer exports anything.
    pub fn remove_upstream(&self, upstream: &UpstreamMetrics) {
        self.bot_info.remove(&upstream.info);
        upstream.up.retire();
    }
}

/// One bot's handles. Cloning shares them.
#[derive(Clone, Debug)]
pub struct BotMetrics(Arc<BotHandles>);

#[derive(Debug)]
struct BotHandles {
    bot: Arc<str>,
    families: GatewayMetrics,
    /// By [`MethodLabel::index`], resolved on first use.
    methods: Box<[OnceLock<MethodMetrics>]>,
    /// By [`RateScope`], then [`RateAction`].
    ratelimited: [[Counter; RateAction::ALL.len()]; RateScope::ALL.len()],
    retries: Counter,
}

impl BotMetrics {
    /// The handles of `method`, resolved on its first use.
    pub fn method(&self, method: MethodLabel) -> &MethodMetrics {
        let handles = &self.0;
        let slot = &handles.methods[method.index()];
        slot.get_or_init(|| {
            let label = method.as_str();
            MethodMetrics {
                label,
                outcomes: Outcome::ALL.map(|outcome| {
                    handles
                        .families
                        .requests
                        .get_or_create_owned(&RequestLabels {
                            bot: Arc::clone(&handles.bot),
                            method: label,
                            outcome: outcome.as_str(),
                        })
                }),
                upstream: handles
                    .families
                    .upstream
                    .get_or_create_owned(&BotMethodLabels {
                        bot: Arc::clone(&handles.bot),
                        method: label,
                    }),
            }
        })
    }

    /// The rate limiter delayed or rejected a request in `scope`.
    pub fn ratelimited(&self, scope: RateScope, action: RateAction) {
        self.0.ratelimited[scope as usize][action as usize].inc();
    }

    /// A request went upstream a second time after a 429.
    pub fn retried(&self) {
        self.0.retries.inc();
    }
}

/// The handles of one bot's method.
#[derive(Debug)]
pub struct MethodMetrics {
    label: &'static str,
    outcomes: [Counter; Outcome::ALL.len()],
    upstream: Histogram,
}

impl MethodMetrics {
    /// One request answered with `outcome`.
    pub fn count(&self, outcome: Outcome) {
        self.outcomes[outcome as usize].inc();
    }

    /// Telegram's response headers arrived `seconds` after sending.
    pub fn upstream_seconds(&self, seconds: f64) {
        self.upstream.observe(seconds);
    }
}

/// One push consumer's `tgin_webhook_replies_total` handles.
#[derive(Clone, Debug)]
pub struct ReplyMetrics([Counter; ReplyOutcome::ALL.len()]);

impl ReplyMetrics {
    pub fn count(&self, outcome: ReplyOutcome) {
        self.0[outcome as usize].inc();
    }
}

/// A bot's series for its current upstream, from
/// [`GatewayMetrics::upstream`] to [`GatewayMetrics::remove_upstream`].
/// Cloning shares them.
#[derive(Clone, Debug)]
pub struct UpstreamMetrics {
    info: InfoLabels,
    up: UpstreamUp,
}

impl UpstreamMetrics {
    /// The bot's `tgin_upstream_up` handle.
    pub fn up(&self) -> &UpstreamUp {
        &self.up
    }
}

/// One bot's `tgin_upstream_up` on one upstream. Its series is created by
/// the first outcome, so it is absent until then, and never again once
/// retired: a retired handle that a task still holds exports nothing.
/// Cloning shares it.
#[derive(Clone, Debug)]
pub struct UpstreamUp(Arc<UpState>);

#[derive(Debug)]
struct UpState {
    family: Family<BotLabels, Gauge>,
    labels: BotLabels,
    /// Resolved by the first outcome, under `retired`.
    gauge: OnceLock<Gauge>,
    /// Whether the series was removed; creating it is checked against
    /// this, so a late outcome never brings a removed series back.
    retired: Mutex<bool>,
}

impl UpstreamUp {
    /// The upstream sent an HTTP answer, whatever its status.
    pub fn answered(&self) {
        self.set(1);
    }

    /// The upstream could not be reached, did not answer in time, or lost
    /// the connection.
    pub fn unreachable(&self) {
        self.set(0);
    }

    fn set(&self, value: i64) {
        let state = &self.0;
        if let Some(gauge) = state.gauge.get() {
            gauge.set(value);
            return;
        }
        let retired = state.retired.lock();
        if *retired {
            return;
        }
        state
            .gauge
            .get_or_init(|| state.family.get_or_create_owned(&state.labels))
            .set(value);
    }

    fn retire(&self) {
        let state = &self.0;
        let mut retired = state.retired.lock();
        *retired = true;
        state.family.remove(&state.labels);
    }
}

#[cfg(test)]
mod tests {
    use prometheus_client::encoding::text::encode;

    use super::*;

    fn exported(reg: &Registry) -> String {
        let mut out = String::new();
        encode(&mut out, reg).expect("encode");
        out
    }

    /// A bot's `tgin_upstream_up` handle exports nothing once its series
    /// were removed, even when its first outcome comes only then (a task
    /// of the bot's old upstream still running); the bot's new handle
    /// starts absent.
    #[test]
    fn retired_upstream_up_never_exports() {
        let mut reg = Registry::with_prefix("tgin");
        let metrics = GatewayMetrics::register(&mut reg);
        let old = metrics.upstream("shop", "api_url");
        let late = old.up().clone();
        metrics.remove_upstream(&old);
        let new = metrics.upstream("shop", "tba");
        late.unreachable();
        let out = exported(&reg);
        assert!(!out.contains("tgin_upstream_up{"), "{out}");
        assert!(!out.contains(r#"upstream="api_url""#), "{out}");
        let info = r#"tgin_bot_info{bot="shop",upstream="tba"} 1"#;
        assert!(out.contains(info), "{out}");
        new.up().answered();
        late.unreachable();
        assert!(exported(&reg).contains("tgin_upstream_up{bot=\"shop\"} 1\n"));
    }

    /// Every family is registered at once; a bot exports its limiter and
    /// retry series up front and a method's series from its first request
    /// on; removing the bot removes them all.
    #[test]
    fn families_and_lazy_method_series() {
        let mut reg = Registry::with_prefix("tgin");
        let metrics = GatewayMetrics::register(&mut reg);
        assert!(
            exported(&reg).contains("tgin_gateway_unauthorized_total 0"),
            "the unlabelled counter is there from the start"
        );

        metrics.unauthorized();
        let bot = metrics.bot("shop");
        let send = bot.method(MethodLabel::of("SENDMESSAGE"));
        send.count(Outcome::Ok);
        send.upstream_seconds(0.01);
        bot.method(MethodLabel::File).count(Outcome::UpstreamError);
        bot.method(MethodLabel::of("noSuchMethod"))
            .count(Outcome::Rejected);
        bot.ratelimited(RateScope::Group, RateAction::Delayed);
        bot.retried();
        metrics.replies("hooks").count(ReplyOutcome::TooLarge);
        let out = exported(&reg);
        for family in [
            "tgin_gateway_requests counter",
            "tgin_gateway_upstream_seconds histogram",
            "tgin_gateway_ratelimited counter",
            "tgin_gateway_retries counter",
            "tgin_gateway_unauthorized counter",
            "tgin_webhook_replies counter",
        ] {
            assert!(
                out.contains(&format!("# TYPE {family}\n")),
                "{family} missing:\n{out}"
            );
        }
        for line in [
            "tgin_gateway_unauthorized_total 1",
            r#"tgin_gateway_requests_total{bot="shop",method="sendMessage",outcome="ok"} 1"#,
            r#"tgin_gateway_requests_total{bot="shop",method="sendMessage",outcome="unreachable"} 0"#,
            r#"tgin_gateway_requests_total{bot="shop",method="file",outcome="upstream_error"} 1"#,
            r#"tgin_gateway_requests_total{bot="shop",method="other",outcome="rejected"} 1"#,
            r#"tgin_gateway_upstream_seconds_count{bot="shop",method="sendMessage"} 1"#,
            r#"tgin_gateway_ratelimited_total{bot="shop",scope="group",action="delayed"} 1"#,
            r#"tgin_gateway_ratelimited_total{bot="shop",scope="global",action="rejected"} 0"#,
            r#"tgin_gateway_retries_total{bot="shop"} 1"#,
            r#"tgin_webhook_replies_total{consumer="hooks",outcome="too_large"} 1"#,
        ] {
            assert!(out.lines().any(|l| l == line), "missing {line:?} in\n{out}");
        }
        assert!(!out.contains("getMe"), "an unused method exports nothing");

        metrics.remove_bot(&bot);
        let out = exported(&reg);
        assert!(!out.contains(r#"bot="shop""#), "{out}");
        assert!(out.contains(r#"consumer="hooks""#), "{out}");

        metrics.remove_replies("hooks");
        let out = exported(&reg);
        assert!(!out.contains(r#"consumer="hooks""#), "{out}");
    }
}
