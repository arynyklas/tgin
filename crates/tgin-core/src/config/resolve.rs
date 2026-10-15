//! The resolved config: secrets expanded, URLs parsed, names replaced by
//! dense ids and rules compile-checked.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use ipnet::IpNet;
use serde_saphyr::Location;
use tgin_runtime::breaker::BreakerConfig;
use tgin_runtime::config::{ConfigError, Env, InterpError, interpolate};
use tgin_runtime::queue::{self, QueueConfig};
use tgin_runtime::secret::Secret;
use url::Url;

use super::model::{self as raw, At, Name, RawConfig, SecretText, Template};
use super::sensitive::{Masker, Sensitive, SensitivePath, SensitiveRoute, SensitiveUrl};
use super::validate::{Diagnostics, HttpPaths, located, valid_command};
use crate::facts::ChatType;
use crate::ids::{ConsumerId, SourceId};
use crate::rules::{self, Predicates, RouteTable, RuleError, RuleSpec, TargetSpec};

pub use super::model::{Balance, LogFormat, LogLevel};

/// A loaded, valid configuration. Its rules compile into a [`RouteTable`].
///
/// `Debug` never shows secrets: they are [`Secret`]s, and URLs and paths
/// are [`Sensitive`].
#[derive(Clone)]
pub struct Config {
    pub runtime: Runtime,
    pub log: Log,
    /// The data plane; `None` serves no webhooks and no pull consumers.
    pub server: Option<Server>,
    pub admin: Admin,
    pub reload: Reload,
    /// Indexed by [`SourceId`].
    pub bots: Vec<Bot>,
    /// Indexed by [`ConsumerId`].
    pub consumers: Vec<Consumer>,
    /// Problems that do not reject the config.
    pub warnings: Vec<ConfigError>,
    /// The config as written, for `/v1/config` (see
    /// [`redacted_json`](super::redacted_json)).
    pub raw: RawConfig,
    /// Masks the config's secrets in [`redacted_json`](super::redacted_json).
    pub(super) masker: Masker,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `raw` holds URLs and paths as written; it is left out.
        f.debug_struct("Config")
            .field("runtime", &self.runtime)
            .field("log", &self.log)
            .field("server", &self.server)
            .field("admin", &self.admin)
            .field("reload", &self.reload)
            .field("bots", &self.bots)
            .field("consumers", &self.consumers)
            .field("warnings", &self.warnings)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runtime {
    /// `None` is one per CPU.
    pub worker_threads: Option<NonZeroUsize>,
    pub lame_duck: Duration,
    pub shutdown_grace: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Log {
    pub level: LogLevel,
    pub format: LogFormat,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub listen: Vec<SocketAddr>,
    pub tls: Option<Tls>,
    pub trusted_proxies: Vec<IpNet>,
    pub max_connections: usize,
    pub header_read_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tls {
    pub cert_file: SensitivePath,
    pub key_file: SensitivePath,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admin {
    pub listen: Vec<SocketAddr>,
    pub token: Option<Secret<String>>,
    pub public_metrics: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reload {
    pub watch: bool,
    pub debounce: Duration,
    pub drain_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Bot {
    pub id: SourceId,
    pub name: String,
    pub token: Secret<String>,
    pub api_url: SensitiveUrl,
    pub ingress: Option<Ingress>,
    /// In order; the first match wins.
    pub rules: Vec<RuleSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "built once per config load; boxing would only complicate callers"
)]
pub enum Ingress {
    LongPoll(LongPoll),
    Webhook(Webhook),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LongPoll {
    pub timeout: Duration,
    pub limit: u32,
    pub allowed_updates: Option<Vec<String>>,
    pub delete_webhook_on_start: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Webhook {
    pub path: SensitiveRoute,
    /// Always `https`.
    pub public_url: Option<SensitiveUrl>,
    pub register: bool,
    pub secret_token: WebhookSecret,
    pub allowed_updates: Option<Vec<String>>,
    pub max_connections: Option<u32>,
    pub drop_pending_updates: bool,
    pub ip_address: Option<IpAddr>,
    pub certificate_file: Option<SensitivePath>,
    pub ip_allowlist: IpAllowlist,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebhookSecret {
    /// Generate one at start.
    Auto,
    None,
    Token(Secret<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IpAllowlist {
    None,
    Telegram,
    Networks(Vec<IpNet>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Consumer {
    pub id: ConsumerId,
    pub name: String,
    pub kind: ConsumerKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConsumerKind {
    Pull(Pull),
    Push(Push),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pull {
    pub queue: QueueConfig,
    pub auth_token: Option<Secret<String>>,
    pub aliases: Vec<SensitiveRoute>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Push {
    pub endpoints: Vec<Endpoint>,
    pub balance: Balance,
    pub max_in_flight: usize,
    pub ordering: queue::Ordering,
    pub capacity: usize,
    pub capacity_bytes: usize,
    pub on_full: queue::OnFull,
    pub retry: Retry,
    pub breaker: BreakerConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// `http` or `https`.
    pub url: SensitiveUrl,
    pub secret_token: Option<Secret<String>>,
    pub timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    pub initial: Duration,
    pub max: Duration,
    pub max_attempts: u32,
    pub max_age: Duration,
}

/// Expands secrets, URLs and paths, checks HTTP paths, resolves names to
/// ids and compiles every rule. Errors go to `diag`; the returned config is
/// only meaningful if there are none.
pub(super) fn resolve(
    raw: RawConfig,
    env: &dyn Env,
    base_dir: &Path,
    diag: &mut Diagnostics,
) -> Config {
    let mut r = Resolver {
        env,
        base_dir,
        diag,
        secrets: Vec::new(),
        paths: HttpPaths::default(),
    };
    let consumers = r.consumers(&raw.consumers);
    let bots = r.bots(&raw);
    let server = raw.server.as_ref().map(|server| r.server(server));
    let admin = Admin {
        listen: raw.admin.listen.iter().map(|a| a.value).collect(),
        token: raw
            .admin
            .token
            .as_ref()
            .and_then(|t| r.secret("admin.token", t)),
        public_metrics: raw.admin.public_metrics,
    };
    if r.diag.errors.is_empty() {
        r.compile_all(&raw, &bots);
    }
    let masker = Masker::new(r.secrets);
    let mut config = Config {
        runtime: Runtime {
            worker_threads: match raw.runtime.worker_threads {
                raw::WorkerThreads::Auto => None,
                raw::WorkerThreads::Count(n) => Some(n),
            },
            lame_duck: raw.runtime.lame_duck.value.get(),
            shutdown_grace: raw.runtime.shutdown_grace.value.get(),
        },
        log: Log {
            level: raw.log.level,
            format: raw.log.format,
        },
        server,
        admin,
        reload: Reload {
            watch: raw.reload.watch,
            debounce: raw.reload.debounce.value.get(),
            drain_timeout: raw.reload.drain_timeout.value.get(),
        },
        bots,
        consumers,
        warnings: Vec::new(),
        raw,
        masker,
    };
    mask(&mut config);
    config
}

/// Renders every [`Sensitive`] field with the config's secrets masked.
fn mask(config: &mut Config) {
    let redactor = config.masker.clone();
    let redactor = redactor.redactor();
    if let Some(tls) = config.server.as_mut().and_then(|s| s.tls.as_mut()) {
        tls.cert_file.mask(redactor);
        tls.key_file.mask(redactor);
    }
    for bot in &mut config.bots {
        bot.api_url.mask(redactor);
        if let Some(Ingress::Webhook(hook)) = &mut bot.ingress {
            hook.path.mask(redactor);
            if let Some(url) = &mut hook.public_url {
                url.mask(redactor);
            }
            if let Some(file) = &mut hook.certificate_file {
                file.mask(redactor);
            }
        }
    }
    for consumer in &mut config.consumers {
        match &mut consumer.kind {
            ConsumerKind::Pull(pull) => {
                for alias in &mut pull.aliases {
                    alias.mask(redactor);
                }
            }
            ConsumerKind::Push(push) => {
                for endpoint in &mut push.endpoints {
                    endpoint.url.mask(redactor);
                }
            }
        }
    }
}

struct Resolver<'a> {
    env: &'a dyn Env,
    base_dir: &'a Path,
    diag: &'a mut Diagnostics,
    /// Every expanded secret, for masking.
    secrets: Vec<String>,
    paths: HttpPaths,
}

impl Resolver<'_> {
    /// Expands references in a secret. Errors never quote text from the
    /// value: a malformed reference or unknown scheme may be part of the
    /// secret. A missing variable or unreadable file names the reference,
    /// which the operator wrote, and never a file's contents.
    fn secret(&mut self, path: impl Into<String>, text: &SecretText) -> Option<Secret<String>> {
        let path = path.into();
        let value = match interpolate(&text.0.value, self.env, self.base_dir) {
            Ok(value) => value,
            Err(err @ (InterpError::MissingVar(_) | InterpError::UnreadableFile { .. })) => {
                self.diag.error(path, text.0.location, err.to_string());
                return None;
            }
            Err(InterpError::BadSyntax(_) | InterpError::UnknownScheme(_)) => {
                self.diag.errors.push(
                    ConfigError::new("cannot expand the secret: a `${…}` reference is malformed")
                        .at(located(path, text.0.location))
                        .hint("references are `${env:NAME}` or `${file:PATH}`; write `$${` for a literal `${`"),
                );
                return None;
            }
        };
        if value.is_empty() {
            self.diag
                .error(path, text.0.location, "the secret is empty");
            return None;
        }
        self.secrets.push(value.clone());
        Some(Secret::new(value))
    }

    fn template(&mut self, path: &str, text: &Template) -> Option<String> {
        match interpolate(&text.0.value, self.env, self.base_dir) {
            Ok(value) => Some(value),
            Err(err) => {
                self.diag.error(path, text.0.location, err.to_string());
                None
            }
        }
    }

    fn file(&mut self, path: &str, text: &Template) -> Option<SensitivePath> {
        let value = self.template(path, text)?;
        if value.is_empty() {
            self.diag
                .error(path, text.0.location, "the file path is empty");
            return None;
        }
        Some(Sensitive::new(PathBuf::from(value), &text.0.value))
    }

    /// An HTTP route path, checked against every other configured path.
    fn route(&mut self, path: String, text: &Template) -> Option<SensitiveRoute> {
        let value = self.template(&path, text)?;
        self.paths.add(self.diag, path, &value, text.0.location);
        Some(Sensitive::new(value, &text.0.value))
    }

    /// An `http` or `https` URL; only `https` if `https_only`.
    fn url(&mut self, path: &str, text: &Template, https_only: bool) -> Option<SensitiveUrl> {
        let value = self.template(path, text)?;
        // The URL may carry credentials, so the error does not quote it.
        let url = match Url::parse(&value) {
            Ok(url) => url,
            Err(err) => {
                self.diag
                    .error(path, text.0.location, format!("invalid URL: {err}"));
                return None;
            }
        };
        let allowed = if https_only {
            url.scheme() == "https"
        } else {
            matches!(url.scheme(), "http" | "https")
        };
        if !allowed {
            let expected = if https_only { "https" } else { "http or https" };
            self.diag.error(
                path,
                text.0.location,
                format!("the URL scheme is `{}`; expected {expected}", url.scheme()),
            );
            return None;
        }
        Some(Sensitive::new(url, &text.0.value))
    }

    fn server(&mut self, server: &raw::Server) -> Server {
        let tls = server.tls.as_ref().and_then(|tls| {
            let cert_file = self.file("server.tls.cert_file", &tls.cert_file);
            let key_file = self.file("server.tls.key_file", &tls.key_file);
            Some(Tls {
                cert_file: cert_file?,
                key_file: key_file?,
            })
        });
        Server {
            listen: server.listen.iter().map(|a| a.value).collect(),
            tls,
            trusted_proxies: server.trusted_proxies.iter().map(|c| c.0).collect(),
            max_connections: server.max_connections.value,
            header_read_timeout: server.header_read_timeout.value.get(),
        }
    }

    fn consumers(&mut self, consumers: &IndexMap<Name, At<raw::Consumer>>) -> Vec<Consumer> {
        let mut out = Vec::with_capacity(consumers.len());
        for (index, (name, consumer)) in consumers.iter().enumerate() {
            let path = format!("consumers.{}", name.as_str());
            let Some(id) = self.id(index, &path, consumer.location, "consumers") else {
                break;
            };
            let kind = match &consumer.value {
                raw::Consumer::Pull(pull) => {
                    let auth_token = pull
                        .auth_token
                        .as_ref()
                        .and_then(|t| self.secret(format!("{path}.pull.auth_token"), t));
                    let aliases = pull
                        .aliases
                        .iter()
                        .enumerate()
                        .filter_map(|(k, alias)| {
                            self.route(format!("{path}.pull.aliases[{k}]"), alias)
                        })
                        .collect();
                    ConsumerKind::Pull(Pull {
                        queue: QueueConfig {
                            capacity: pull.capacity.value,
                            capacity_bytes: bytes(pull.capacity_bytes.value),
                            lease_timeout: pull.lease_timeout.value.get(),
                            ordering: ordering(pull.ordering),
                            on_full: on_full(pull.on_full),
                        },
                        auth_token,
                        aliases,
                    })
                }
                raw::Consumer::Push(push) => ConsumerKind::Push(self.push(&path, push)),
            };
            out.push(Consumer {
                id: ConsumerId(id),
                name: name.as_str().to_owned(),
                kind,
            });
        }
        out
    }

    fn push(&mut self, path: &str, push: &raw::Push) -> Push {
        let mut endpoints = Vec::with_capacity(push.endpoints.value.len());
        for (k, endpoint) in push.endpoints.value.iter().enumerate() {
            let path = format!("{path}.push.endpoints[{k}]");
            let url = self.url(&format!("{path}.url"), &endpoint.url, false);
            let secret_token = endpoint
                .secret_token
                .as_ref()
                .and_then(|t| self.secret(format!("{path}.secret_token"), t));
            if let Some(url) = url {
                endpoints.push(Endpoint {
                    url,
                    secret_token,
                    timeout: endpoint.timeout.value.get(),
                });
            }
        }
        Push {
            endpoints,
            balance: push.balance,
            max_in_flight: push.max_in_flight.value,
            ordering: ordering(push.ordering),
            capacity: push.capacity.value,
            capacity_bytes: bytes(push.capacity_bytes.value),
            on_full: on_full(push.on_full),
            retry: Retry {
                initial: push.retry.initial.value.get(),
                max: push.retry.max.value.get(),
                max_attempts: push.retry.max_attempts.value,
                max_age: push.retry.max_age.value.get(),
            },
            breaker: BreakerConfig {
                failure_threshold: push.breaker.failure_threshold.value,
                open_for: push.breaker.open_for.value.get(),
            },
        }
    }

    fn bots(&mut self, raw: &RawConfig) -> Vec<Bot> {
        let mut out = Vec::with_capacity(raw.bots.len());
        for (index, bot) in raw.bots.iter().enumerate() {
            let path = format!("bots[{index}]");
            let Some(id) = self.id(index, &path, bot.name.location, "bots") else {
                break;
            };
            let token = self.secret(format!("{path}.token"), &bot.token);
            let api_url = self.url(&format!("{path}.api_url"), &bot.api_url, false);
            let ingress = bot
                .ingress
                .as_ref()
                .map(|ingress| self.ingress(&path, bot.name.value.as_str(), ingress));
            let mut rules = Vec::with_capacity(bot.routes.len());
            for (j, route) in bot.routes.iter().enumerate() {
                let spec = rule_spec(&route.value, &raw.consumers);
                self.compile_rule(&format!("{path}.routes[{j}]"), route.location, &spec);
                rules.push(spec);
            }
            if let (Some(token), Some(api_url)) = (token, api_url) {
                out.push(Bot {
                    id: SourceId(id),
                    name: bot.name.value.as_str().to_owned(),
                    token,
                    api_url,
                    ingress,
                    rules,
                });
            }
        }
        out
    }

    fn ingress(&mut self, path: &str, name: &str, ingress: &At<raw::Ingress>) -> Ingress {
        match &ingress.value {
            raw::Ingress::LongPoll(poll) => Ingress::LongPoll(LongPoll {
                timeout: poll.timeout.value.get(),
                limit: poll.limit.value,
                allowed_updates: poll.allowed_updates.clone(),
                delete_webhook_on_start: poll.delete_webhook_on_start,
            }),
            raw::Ingress::Webhook(hook) => {
                let path = format!("{path}.ingress.webhook");
                let route = match &hook.path {
                    Some(text) => self.route(format!("{path}.path"), text),
                    None => {
                        let default = Template(At {
                            value: format!("/webhook/{name}"),
                            location: ingress.location,
                        });
                        self.route(format!("{path}.path"), &default)
                    }
                };
                Ingress::Webhook(Webhook {
                    path: route.unwrap_or_else(|| Sensitive::new(String::new(), "")),
                    public_url: hook
                        .public_url
                        .as_ref()
                        .and_then(|url| self.url(&format!("{path}.public_url"), url, true)),
                    register: hook.register.value,
                    secret_token: match &hook.secret_token {
                        raw::WebhookSecret::Auto => WebhookSecret::Auto,
                        raw::WebhookSecret::None => WebhookSecret::None,
                        raw::WebhookSecret::Secret(secret) => self
                            .secret(format!("{path}.secret_token"), secret)
                            .map_or(WebhookSecret::None, WebhookSecret::Token),
                    },
                    allowed_updates: hook.allowed_updates.clone(),
                    max_connections: hook.max_connections.as_ref().map(|m| m.value),
                    drop_pending_updates: hook.drop_pending_updates,
                    ip_address: hook.ip_address,
                    certificate_file: hook
                        .certificate_file
                        .as_ref()
                        .and_then(|file| self.file(&format!("{path}.certificate_file"), file)),
                    ip_allowlist: match &hook.ip_allowlist {
                        raw::IpAllowlist::None => IpAllowlist::None,
                        raw::IpAllowlist::Telegram => IpAllowlist::Telegram,
                        raw::IpAllowlist::Networks(networks) => {
                            IpAllowlist::Networks(networks.iter().map(|c| c.0).collect())
                        }
                    },
                })
            }
        }
    }

    /// A dense id for the `index`th bot or consumer.
    fn id(&mut self, index: usize, path: &str, location: Location, what: &str) -> Option<u16> {
        let id = u16::try_from(index).ok();
        if id.is_none() {
            self.diag.error(
                path,
                location,
                format!("too many {what}; at most {}", u32::from(u16::MAX) + 1),
            );
        }
        id
    }

    /// Compiles one rule alone, so that every bad rule is reported.
    fn compile_rule(&mut self, path: &str, location: Location, spec: &RuleSpec) {
        let rules = [(SourceId(0), vec![spec.clone()])];
        if let Err(err) = RouteTable::compile(&rules, &[]) {
            self.diag.error(path, location, rule_message(&err));
        }
    }

    /// Compiles every bot's rules together, as the engine will.
    fn compile_all(&mut self, raw: &RawConfig, bots: &[Bot]) {
        let rules: Vec<(SourceId, Vec<RuleSpec>)> =
            bots.iter().map(|b| (b.id, b.rules.clone())).collect();
        let Err(err) = RouteTable::compile(&rules, &[]) else {
            return;
        };
        let message = rule_message(&err);
        match &err {
            RuleError::Invalid {
                source_id, rule, ..
            } => {
                let index = usize::from(source_id.0);
                let location = raw
                    .bots
                    .get(index)
                    .and_then(|bot| bot.routes.get(*rule))
                    .map_or(Location::UNKNOWN, |route| route.location);
                self.diag
                    .error(format!("bots[{index}].routes[{rule}]"), location, message);
            }
            RuleError::RegexSet(_) => {
                let (path, location) = first_regex_route(raw).unwrap_or_else(|| bots_at(raw));
                self.diag.errors.push(
                    ConfigError::new(message)
                        .at(located(path, location))
                        .hint("the regex size limits apply to the `regex` matches of every bot and rule together"),
                );
            }
            RuleError::DuplicateSource(_) => {
                let (path, location) = bots_at(raw);
                self.diag.error(path, location, message);
            }
        }
    }
}

/// The first rule with a `regex` match, as (path, location).
fn first_regex_route(raw: &RawConfig) -> Option<(String, Location)> {
    let is_regex = |m: &Option<raw::TextMatch>| matches!(m, Some(raw::TextMatch::Regex(_)));
    raw.bots.iter().enumerate().find_map(|(i, bot)| {
        bot.routes.iter().enumerate().find_map(|(j, route)| {
            let m = route.value.r#match.as_ref()?;
            (is_regex(&m.text) || is_regex(&m.callback_data))
                .then(|| (format!("bots[{i}].routes[{j}]"), route.location))
        })
    })
}

/// The `bots` list, located at its first bot.
fn bots_at(raw: &RawConfig) -> (String, Location) {
    let location = raw
        .bots
        .first()
        .map_or(Location::UNKNOWN, |bot| bot.name.location);
    ("bots".to_owned(), location)
}

/// The reason of a rule error, without the source and rule numbers that the
/// error's location already gives.
fn rule_message(err: &RuleError) -> String {
    match err {
        RuleError::Invalid { reason, .. } => reason.to_string(),
        other => other.to_string(),
    }
}

/// Stands in for a consumer that does not exist, so that the rule's other
/// problems are still found. Validation reports the unknown name.
const PLACEHOLDER: ConsumerId = ConsumerId(u16::MAX);

/// The rule as the engine runs it. Parts that validation already rejected
/// (unknown consumers, invalid commands, bad weights) are replaced with
/// stand-ins so that compiling the rule reports only its other problems;
/// for a valid config the result is exact.
fn rule_spec(route: &raw::Route, consumers: &IndexMap<Name, At<raw::Consumer>>) -> RuleSpec {
    let predicates = route
        .r#match
        .as_ref()
        .map_or_else(Predicates::default, |m| Predicates {
            update_types: m.update_types.clone(),
            chat_ids: m.chat_ids.clone(),
            chat_types: m.chat_types.iter().map(|t| chat_type(*t)).collect(),
            user_ids: m.user_ids.clone(),
            commands: m
                .commands
                .iter()
                .filter(|c| valid_command(&c.value))
                .map(|c| c.value.clone())
                .collect(),
            text: m.text.as_ref().map(text_match),
            callback_data: m.callback_data.as_ref().map(text_match),
        });
    RuleSpec {
        predicates,
        targets: route.to.iter().map(|t| target_spec(t, consumers)).collect(),
    }
}

fn target_spec(
    target: &At<raw::Target>,
    consumers: &IndexMap<Name, At<raw::Consumer>>,
) -> TargetSpec {
    let children = |list: &[At<raw::Target>]| -> Vec<TargetSpec> {
        list.iter().map(|t| target_spec(t, consumers)).collect()
    };
    match &target.value {
        raw::Target::Consumer(name) => TargetSpec::Consumer(
            consumers
                .get_index_of(name.as_str())
                .and_then(|index| u16::try_from(index).ok())
                .map_or(PLACEHOLDER, ConsumerId),
        ),
        raw::Target::All(list) => TargetSpec::All(children(list)),
        raw::Target::Split(split) => {
            let among = children(&split.among);
            let weights: Vec<u32> = split
                .weights
                .as_ref()
                .map(|w| w.value.iter().map(|w| w.value).collect())
                .unwrap_or_default();
            let (strategy, weights) = match split.strategy.value {
                raw::SplitStrategy::RoundRobin => (rules::SplitStrategy::RoundRobin, Vec::new()),
                raw::SplitStrategy::HashChat => (rules::SplitStrategy::HashChat, Vec::new()),
                raw::SplitStrategy::Weighted => {
                    let valid = weights.len() == among.len() && !weights.contains(&0);
                    let weights = if valid { weights } else { vec![1; among.len()] };
                    (rules::SplitStrategy::Weighted, weights)
                }
            };
            TargetSpec::Split {
                strategy,
                among,
                weights,
            }
        }
    }
}

fn chat_type(t: raw::ChatType) -> ChatType {
    match t {
        raw::ChatType::Private => ChatType::Private,
        raw::ChatType::Group => ChatType::Group,
        raw::ChatType::Supergroup => ChatType::Supergroup,
        raw::ChatType::Channel => ChatType::Channel,
    }
}

fn text_match(m: &raw::TextMatch) -> rules::TextMatch {
    match m {
        raw::TextMatch::Prefix(list) => rules::TextMatch::Prefix(list.clone()),
        raw::TextMatch::Regex(list) => rules::TextMatch::Regex(list.clone()),
        raw::TextMatch::Equals(list) => rules::TextMatch::Equals(list.clone()),
    }
}

fn ordering(o: raw::Ordering) -> queue::Ordering {
    match o {
        raw::Ordering::None => queue::Ordering::None,
        raw::Ordering::PerChat => queue::Ordering::PerKey,
    }
}

fn on_full(o: raw::OnFull) -> queue::OnFull {
    match o {
        raw::OnFull::Backpressure => queue::OnFull::Backpressure,
        raw::OnFull::DropOldest => queue::OnFull::DropOldest,
    }
}

/// Sizes beyond the address space mean "unlimited".
fn bytes(size: raw::ByteSize) -> usize {
    usize::try_from(size.0).unwrap_or(usize::MAX)
}
