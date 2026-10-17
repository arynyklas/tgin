//! The resolved config: secrets expanded, URLs parsed, consumer names
//! replaced by dense ids and rules compile-checked.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use ipnet::IpNet;
use serde_saphyr::Location;
use tgin_runtime::breaker::BreakerConfig;
use tgin_runtime::config::{ConfigError, Env, InterpError, interpolate};
use tgin_runtime::queue::{self, QueueConfig};
use tgin_runtime::secret::Secret;
use url::{Host, Url};

use super::defaults;
use super::model::{self as raw, At, Name, RawConfig, SecretText, Template};
use super::sensitive::{Masker, Sensitive, SensitivePath, SensitiveRoute, SensitiveUrl};
use super::validate::{Diagnostics, HttpPaths, located, valid_command, webhook_rules};
use crate::facts::ChatType;
use crate::ids::{ConsumerId, SourceId};
use crate::rules::{self, Predicates, RouteTable, RuleError, RuleSpec, TargetSpec};

pub use super::model::{Balance, LogFormat, LogLevel, RateMode, UpstreamKind, WebhookReply};

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
    /// The named `upstreams`, in file order; bots share them. A bot's
    /// `api_url` shorthand is its own and is not listed.
    pub upstreams: Vec<Arc<Upstream>>,
    /// In file order. The engine gives each bot a [`SourceId`] of its own,
    /// stable by name across reloads: look it up with
    /// [`Engine::source_id`](crate::engine::Engine::source_id), never by
    /// position here.
    pub bots: Vec<Bot>,
    /// Indexed by [`ConsumerId`].
    pub consumers: Vec<Consumer>,
    /// Problems that do not reject the config.
    pub warnings: Vec<ConfigError>,
    /// The config as written; [`redacted_json`](super::redacted_json)
    /// renders it, masked, for `/v1/config`.
    pub(super) raw: RawConfig,
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
            .field("upstreams", &self.upstreams)
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bot {
    pub name: String,
    pub token: Secret<String>,
    /// A named upstream, shared with the other bots that use it, or the
    /// bot's own from its `api_url` shorthand.
    pub upstream: Arc<Upstream>,
    /// `file:` references may reach an upstream in local mode.
    pub allow_local_files: bool,
    pub ingress: Option<Ingress>,
    pub gateway: Gateway,
    /// In order; the first match wins.
    pub rules: Vec<RuleSpec>,
}

/// A Bot API server bots talk to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    /// The `upstreams` key, or [`Upstream::SHORTHAND`] for a bot's
    /// `api_url`.
    pub name: Arc<str>,
    pub kind: UpstreamKind,
    /// `http` or `https`.
    pub url: SensitiveUrl,
    /// The server runs with `--local`; only for [`UpstreamKind::Local`].
    pub local_mode: bool,
    /// Only for [`UpstreamKind::Local`].
    pub files: Option<Files>,
    /// The longest wait for the response once a request was sent; above
    /// zero, at most an hour.
    pub response_timeout: Duration,
}

/// Where a local server's files are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Files {
    /// The server's directory mounted in tgin at `dir`, which the server
    /// itself sees at `server_dir` (absolute).
    Dir { dir: PathBuf, server_dir: PathBuf },
    /// An HTTP origin serving the server's directory.
    Url(SensitiveUrl),
}

/// What a webhook registered with an upstream may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebhookRules {
    /// `public_url` may be `http`.
    pub http_allowed: bool,
    /// `public_url` may use any port, not only 443, 80, 88 and 8443.
    pub any_port: bool,
    /// The largest `max_connections`.
    pub max_connections: u32,
}

/// The ports Telegram posts webhooks to.
pub(super) const WEBHOOK_PORTS: [u16; 4] = [443, 80, 88, 8443];

impl WebhookRules {
    /// Telegram's rules, unless the server is local and in local mode.
    pub(super) fn new(kind: UpstreamKind, local_mode: bool) -> WebhookRules {
        let local = kind == UpstreamKind::Local && local_mode;
        WebhookRules {
            http_allowed: local,
            any_port: local,
            max_connections: if local { 100_000 } else { 100 },
        }
    }
}

impl Upstream {
    /// The name of a bot's `api_url` shorthand, which no `upstreams` entry
    /// can take.
    pub const SHORTHAND: &str = "api_url";

    /// The name of a bot's directory on the server: its token with `:`
    /// replaced by `~`. It holds the token: never log it.
    pub fn token_dir(token: &str) -> String {
        token.replace(':', "~")
    }

    pub fn webhook_rules(&self) -> WebhookRules {
        WebhookRules::new(self.kind, self.local_mode)
    }

    fn mask(&mut self, redactor: &tgin_runtime::secret::Redactor) {
        self.url.mask(redactor);
        if let Some(Files::Url(url)) = &mut self.files {
            url.mask(redactor);
        }
    }
}

/// The Bot API gateway of one bot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gateway {
    /// The read timeout of upstream requests.
    pub upstream_idle_timeout: Duration,
    pub rate_limit: RateLimit,
    /// The longest upstream `retry_after` retried once; zero never retries.
    pub retry_429_max_wait: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub mode: RateMode,
    /// At most 60 s; zero behaves like [`RateMode::Reject`].
    pub max_delay: Duration,
    /// ASCII-lowercased method names and `prefix*` globs; empty only with
    /// [`RateMode::Off`].
    pub methods: Vec<String>,
    pub global: Rate,
    pub per_chat: Rate,
    pub per_group: Rate,
}

/// `count` requests per `per`, after a burst of `burst`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    pub count: NonZeroU32,
    /// Above zero.
    pub per: Duration,
    pub burst: NonZeroU32,
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
    /// Derived from the bot token.
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
    pub honor_drop_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Push {
    /// Empty only with `accept_set_webhook`.
    pub endpoints: Vec<Endpoint>,
    pub balance: Balance,
    pub max_in_flight: usize,
    pub ordering: queue::Ordering,
    pub capacity: usize,
    pub capacity_bytes: usize,
    pub on_full: queue::OnFull,
    pub retry: Retry,
    pub breaker: BreakerConfig,
    /// `setWebhook` on the consumer's gateway path sets a runtime endpoint;
    /// only for a consumer fed by one bot.
    pub accept_set_webhook: bool,
    pub webhook_reply: WebhookReply,
    pub honor_drop_pending: bool,
}

impl Push {
    /// The push queue's limits. The dispatcher acks or releases every lease
    /// it takes, so its leases are made never to expire.
    pub fn queue(&self) -> QueueConfig {
        QueueConfig {
            capacity: self.capacity,
            capacity_bytes: self.capacity_bytes,
            lease_timeout: crate::consumer::push::LEASE_TIMEOUT,
            ordering: self.ordering,
            on_full: self.on_full,
        }
    }
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
    let upstreams = r.upstreams(&raw);
    let bots = r.bots(&raw, &upstreams);
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
        upstreams,
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
    // Named upstreams are shared: each is masked once, then handed back to
    // its bots. A bot's shorthand is its own.
    for upstream in &mut config.upstreams {
        Arc::make_mut(upstream).mask(redactor);
    }
    for bot in &mut config.bots {
        match config
            .upstreams
            .iter()
            .find(|named| named.name == bot.upstream.name)
        {
            Some(named) => bot.upstream = Arc::clone(named),
            None => Arc::make_mut(&mut bot.upstream).mask(redactor),
        }
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
                self.malformed(path, text.0.location, "secret");
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

    /// Reports a malformed reference or unknown scheme in `what` without
    /// quoting the written text, which may hold credentials.
    fn malformed(&mut self, path: String, location: Location, what: &str) {
        self.diag.errors.push(
            ConfigError::new(format!(
                "cannot expand the {what}: a `${{…}}` reference is malformed"
            ))
            .at(located(path, location))
            .hint("references are `${env:NAME}` or `${file:PATH}`; write `$${` for a literal `${`"),
        );
    }

    /// A `secret_token` sent as `X-Telegram-Bot-Api-Secret-Token`, which
    /// Telegram allows only as 1 to 256 of `[A-Za-z0-9_-]`.
    fn header_secret(&mut self, path: String, text: &SecretText) -> Option<Secret<String>> {
        let secret = self.secret(path.clone(), text)?;
        if header_safe_secret(secret.expose()) {
            return Some(secret);
        }
        // Never quotes the value: it is the secret.
        self.diag.errors.push(
            ConfigError::new("the secret_token is not a valid secret header value")
                .at(located(path, text.0.location))
                .hint(
                    "it is sent as `X-Telegram-Bot-Api-Secret-Token`: use 1 to 256 ASCII letters, digits, `_` or `-`",
                ),
        );
        None
    }

    /// A bot token, checked with the rule the Bot API client applies. A
    /// token the gateway cannot serve only warns: the bot still works
    /// without the gateway.
    fn bot_token(&mut self, path: String, text: &SecretText) -> Option<Secret<String>> {
        let token = self.secret(path.clone(), text)?;
        if super::valid_bot_token(token.expose()) {
            if !super::gateway_token(token.expose()) {
                // Never quotes the value: it is the token.
                self.diag.warning(
                    path,
                    text.0.location,
                    "the Bot API gateway cannot serve this bot: its token is not shaped `<digits>:<secret>`, as Telegram issues them",
                );
            }
            return Some(token);
        }
        // Never quotes the value: it is the token.
        self.diag.errors.push(
            ConfigError::new("this is not a valid bot token")
                .at(located(path, text.0.location))
                .hint(
                    "a bot token uses only [A-Za-z0-9:_-]; look for a space, quotes or an extra newline in the value or its file",
                ),
        );
        None
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

    /// A file path; a relative one resolves against the config file's
    /// directory, never the process CWD.
    fn file(&mut self, path: &str, text: &Template) -> Option<SensitivePath> {
        let value = self.template(path, text)?;
        if value.is_empty() {
            self.diag
                .error(path, text.0.location, "the file path is empty");
            return None;
        }
        let file = PathBuf::from(value);
        // `has_root` keeps `/x` as written on Windows too.
        let file = if file.is_absolute() || file.has_root() {
            file
        } else {
            self.base_dir.join(file)
        };
        Some(Sensitive::new(file, &text.0.value))
    }

    /// An HTTP route path, checked against every other configured path.
    fn route(&mut self, path: String, text: &Template) -> Option<SensitiveRoute> {
        let value = self.template(&path, text)?;
        self.paths.add(self.diag, path, &value, text.0.location);
        Some(Sensitive::new(value, &text.0.value))
    }

    /// An `http` or `https` URL; only `https` if `https_only`.
    fn url(&mut self, path: &str, text: &Template, https_only: bool) -> Option<SensitiveUrl> {
        // The URL may carry credentials, so no error quotes it.
        let value = match interpolate(&text.0.value, self.env, self.base_dir) {
            Ok(value) => value,
            Err(err @ (InterpError::MissingVar(_) | InterpError::UnreadableFile { .. })) => {
                self.diag.error(path, text.0.location, err.to_string());
                return None;
            }
            Err(InterpError::BadSyntax(_) | InterpError::UnknownScheme(_)) => {
                self.malformed(path.to_owned(), text.0.location, "URL");
                return None;
            }
        };
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
                format!("the URL scheme must be {expected}"),
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
                        honor_drop_pending: pull.honor_drop_pending,
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
                .and_then(|t| self.header_secret(format!("{path}.secret_token"), t));
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
            accept_set_webhook: push.accept_set_webhook,
            webhook_reply: push.webhook_reply,
            honor_drop_pending: push.honor_drop_pending,
        }
    }

    /// The named upstreams. One that fails to resolve is left out, with its
    /// errors recorded, and so are the bots that use it.
    fn upstreams(&mut self, raw: &RawConfig) -> Vec<Arc<Upstream>> {
        let mut out = Vec::with_capacity(raw.upstreams.len());
        for (name, upstream) in &raw.upstreams {
            let path = format!("upstreams.{}", name.as_str());
            let upstream = &upstream.value;
            let url = self.upstream_url(&format!("{path}.url"), name.as_str(), &upstream.url);
            let files = match &upstream.files {
                None => Some(None),
                Some(files) => self.files(&format!("{path}.files"), &files.value).map(Some),
            };
            let kind = upstream.kind.value;
            let response_timeout = upstream.response_timeout.as_ref().map_or(
                match kind {
                    UpstreamKind::Cloud => defaults::CLOUD_RESPONSE_TIMEOUT,
                    UpstreamKind::Local => defaults::LOCAL_RESPONSE_TIMEOUT,
                },
                |timeout| timeout.value.get(),
            );
            if let (Some(url), Some(files)) = (url, files) {
                out.push(Arc::new(Upstream {
                    name: Arc::from(name.as_str()),
                    kind,
                    url,
                    local_mode: upstream.local_mode.value,
                    files,
                    response_timeout,
                }));
            }
        }
        out
    }

    /// An upstream's URL. Plain `http` to a host that is not loopback
    /// warns, naming the upstream and never the URL, which may hold
    /// credentials.
    fn upstream_url(&mut self, path: &str, name: &str, text: &Template) -> Option<SensitiveUrl> {
        let url = self.url(path, text, false)?;
        if url.expose().scheme() == "http" && !loopback(url.expose()) {
            self.diag.warnings.push(
                ConfigError::warning(format!(
                    "upstream `{name}` sends bot tokens over plain http to a host that is not loopback"
                ))
                .at(located(path, text.0.location))
                .hint("use https, or keep the Bot API server on a network only tgin reaches"),
            );
        }
        Some(url)
    }

    /// An upstream's `files`; validation reported a shape that is neither
    /// `dir` nor `url`.
    fn files(&mut self, path: &str, files: &raw::Files) -> Option<Files> {
        if let Some(url) = &files.url {
            return self.url(&format!("{path}.url"), url, false).map(Files::Url);
        }
        let dir = self.file(&format!("{path}.dir"), files.dir.as_ref()?)?;
        let dir = dir.expose().clone();
        let server_dir = match &files.server_dir {
            None => dir.clone(),
            Some(text) => {
                let path = format!("{path}.server_dir");
                let server_dir = PathBuf::from(self.template(&path, text)?);
                // `has_root` keeps `/x` absolute on Windows too.
                if !server_dir.has_root() {
                    self.diag.errors.push(
                        ConfigError::new("`server_dir` must be an absolute path")
                            .at(located(path, text.0.location))
                            .hint("it is the directory as the Bot API server sees it, which `getFile` answers absolute paths in"),
                    );
                    return None;
                }
                server_dir
            }
        };
        Some(Files::Dir { dir, server_dir })
    }

    fn bots(&mut self, raw: &RawConfig, upstreams: &[Arc<Upstream>]) -> Vec<Bot> {
        let mut out = Vec::with_capacity(raw.bots.len());
        // Each token, expanded, with the first bot that has it.
        let mut tokens: HashMap<String, usize> = HashMap::new();
        for (index, bot) in raw.bots.iter().enumerate() {
            let path = format!("bots[{index}]");
            if self.id(index, &path, bot.name.location, "bots").is_none() {
                break;
            }
            let token = self.bot_token(format!("{path}.token"), &bot.token);
            if let Some(token) = &token {
                match tokens.entry(token.expose().clone()) {
                    Entry::Vacant(entry) => {
                        entry.insert(index);
                    }
                    // Never quotes the value: it is the token.
                    Entry::Occupied(first) => self.diag.errors.push(
                        ConfigError::new(format!(
                            "bots[{}] (`{}`) already has this token",
                            first.get(),
                            raw.bots[*first.get()].name.value.as_str()
                        ))
                        .at(located(format!("{path}.token"), bot.token.0.location))
                        .hint("Telegram splits a token's updates between everyone who receives them; give each bot its own token"),
                    ),
                }
            }
            let upstream = self.bot_upstream(&path, bot, upstreams);
            let rules = webhook_rules(raw, bot);
            let ingress = bot
                .ingress
                .as_ref()
                .map(|ingress| self.ingress(&path, bot.name.value.as_str(), ingress, rules));
            let mut rules = Vec::with_capacity(bot.routes.len());
            for (j, route) in bot.routes.iter().enumerate() {
                let spec = rule_spec(&route.value, &raw.consumers);
                self.compile_rule(&format!("{path}.routes[{j}]"), route.location, &spec);
                rules.push(spec);
            }
            if let (Some(token), Some(upstream)) = (token, upstream) {
                out.push(Bot {
                    name: bot.name.value.as_str().to_owned(),
                    token,
                    upstream,
                    allow_local_files: bot.allow_local_files,
                    ingress,
                    gateway: gateway(&bot.gateway),
                    rules,
                });
            }
        }
        out
    }

    /// The bot's named upstream, or its own from `api_url` or the default.
    /// Validation reports an unknown name, and a named upstream that failed
    /// to resolve reported its own errors.
    fn bot_upstream(
        &mut self,
        path: &str,
        bot: &raw::Bot,
        upstreams: &[Arc<Upstream>],
    ) -> Option<Arc<Upstream>> {
        if let Some(name) = &bot.upstream {
            return upstreams
                .iter()
                .find(|upstream| *upstream.name == *name.value.as_str())
                .cloned();
        }
        let path = format!("{path}.api_url");
        let url = match &bot.api_url {
            Some(text) => self.upstream_url(&path, Upstream::SHORTHAND, text)?,
            None => self.url(&path, &defaults::api_url(), false)?,
        };
        Some(Arc::new(Upstream {
            name: Arc::from(Upstream::SHORTHAND),
            kind: UpstreamKind::Cloud,
            url,
            local_mode: false,
            files: None,
            response_timeout: defaults::CLOUD_RESPONSE_TIMEOUT,
        }))
    }

    /// A webhook's `public_url`, by the rules of the bot's upstream.
    fn public_url(
        &mut self,
        path: &str,
        text: &Template,
        rules: WebhookRules,
    ) -> Option<SensitiveUrl> {
        let url = self.url(path, text, !rules.http_allowed)?;
        let port = url.expose().port_or_known_default();
        if !rules.any_port && !port.is_some_and(|port| WEBHOOK_PORTS.contains(&port)) {
            let port = port.map_or_else(String::new, |port| format!(" {port}"));
            self.diag.errors.push(
                ConfigError::new(format!(
                    "webhooks are posted only to ports 443, 80, 88 and 8443, not to port{port}"
                ))
                .at(located(path, text.0.location))
                .hint("only a `local` upstream with `local_mode: true` posts webhooks to any port"),
            );
            return None;
        }
        Some(url)
    }

    fn ingress(
        &mut self,
        path: &str,
        name: &str,
        ingress: &At<raw::Ingress>,
        rules: WebhookRules,
    ) -> Ingress {
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
                        .and_then(|url| self.public_url(&format!("{path}.public_url"), url, rules)),
                    register: hook.register.value,
                    secret_token: match &hook.secret_token {
                        raw::WebhookSecret::Auto => WebhookSecret::Auto,
                        raw::WebhookSecret::None => WebhookSecret::None,
                        raw::WebhookSecret::Secret(secret) => self
                            .header_secret(format!("{path}.secret_token"), secret)
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
                    ip_allowlist: match &hook.ip_allowlist.value {
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

    /// A dense id for the `index`th bot or consumer, if one is left.
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

    /// Compiles every bot's rules together, as the engine will. The bots
    /// take their positions in the file as ids here, so that an error names
    /// the bot it comes from; the engine gives them its own ids.
    fn compile_all(&mut self, raw: &RawConfig, bots: &[Bot]) {
        let rules: Vec<(SourceId, Vec<RuleSpec>)> = bots
            .iter()
            .enumerate()
            .filter_map(|(index, bot)| {
                Some((SourceId(u16::try_from(index).ok()?), bot.rules.clone()))
            })
            .collect();
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

/// Whether `url`'s host is this machine: a loopback address or
/// `localhost`.
fn loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

fn gateway(gateway: &raw::Gateway) -> Gateway {
    let limit = &gateway.rate_limit;
    Gateway {
        upstream_idle_timeout: gateway.upstream_idle_timeout.value.get(),
        rate_limit: RateLimit {
            mode: limit.mode,
            max_delay: limit.max_delay.value.get(),
            methods: limit
                .methods
                .value
                .iter()
                .map(|method| method.value.to_ascii_lowercase())
                .collect(),
            global: rate(&limit.global.rate()),
            per_chat: rate(&limit.per_chat.rate()),
            per_group: rate(&limit.per_group.rate()),
        },
        retry_429_max_wait: gateway.retry_429.max_wait.value.get(),
    }
}

/// Validation rejects a zero count or burst; the stand-in only keeps an
/// invalid config's other errors coming.
fn rate(rate: &raw::Rate<'_>) -> Rate {
    let nonzero = |n: u32| NonZeroU32::new(n).unwrap_or(NonZeroU32::MIN);
    Rate {
        count: nonzero(rate.count.value),
        per: rate.per.value.get(),
        burst: nonzero(rate.burst.value),
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

/// Whether a push endpoint's `secret_token` is sendable as
/// `X-Telegram-Bot-Api-Secret-Token`: `^[A-Za-z0-9_-]{1,256}$`, as Telegram
/// requires of webhook secrets.
fn header_safe_secret(secret: &str) -> bool {
    (1..=256).contains(&secret.len())
        && secret
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Sizes beyond the address space mean "unlimited".
fn bytes(size: raw::ByteSize) -> usize {
    usize::try_from(size.0).unwrap_or(usize::MAX)
}
