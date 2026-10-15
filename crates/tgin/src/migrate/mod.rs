//! `tgin config migrate`: converts a tgin 0.2 RON config into a version 1
//! YAML config.
//!
//! The conversion never reads the environment. Each 0.2 placeholder
//! `${VAR}` becomes the v1 reference `${env:VAR}`, and literal values are
//! copied as written. Errors and warnings never quote config text: they
//! name fields, positions and variables, and pass through token-shape
//! redaction. Config values appear only in the YAML output.

mod emit;
pub mod v0;

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use tgin_runtime::secret::Redactor;

use self::emit::Node;
use self::v0::{LogFormat, RegistrationWebhookConfig, RouteConfig, TginConfig, UpdateConfig};
use crate::exit;

/// A 0.2 placeholder, matched as 0.2 matched it.
static PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{(\w+)\}").expect("the placeholder regex is valid"));

/// The Bot API URL v1 uses when `api_url` is omitted.
const DEFAULT_API_URL: &str = "https://api.telegram.org";
/// The longest v1 consumer name.
const NAME_MAX: usize = 63;
/// 0.2's `WebhookRoute` timeout, which is also v1's endpoint default.
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 10_000;
/// The longest long-poll hold Telegram accepts; 0.2 capped at it too.
const MAX_LONG_POLL_TIMEOUT: u64 = 50;
/// v1's `long_poll` defaults.
const DEFAULT_LONG_POLL_TIMEOUT: u64 = 30;
const DEFAULT_LONG_POLL_LIMIT: u64 = 100;

const HEADER: &str = "# Converted from a tgin 0.2 config by `tgin config migrate`.";
const API_DROPPED: &str = "`api` was dropped: tgin 1.0 has no route-management API; edit the routes in this file instead, which tgin reloads on change";
const ADMIN_NOTE: &str = "tgin 0.2's auth_token. The status and metrics endpoints moved from the data port to admin.listen (default 127.0.0.1:9090).";
const PUSH_NOTE: &str = "tgin 0.2 made one delivery attempt and dropped what its downstream did not take, never pausing the source; delete on_full and retry for tgin 1.0's backpressure and retries.";

/// A converted config. `Debug` shows the warnings and the YAML's length,
/// never the YAML, which holds the config's values.
pub struct Migration {
    /// The YAML document, headed by a comment listing the warnings.
    pub yaml: String,
    /// What the conversion changed or could not carry over.
    pub warnings: Vec<String>,
}

impl std::fmt::Debug for Migration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Migration")
            .field("yaml_len", &self.yaml.len())
            .field("warnings", &self.warnings)
            .finish()
    }
}

/// Converts the 0.2 RON config `text`.
///
/// # Errors
///
/// Every reason the config cannot be converted. Like the warnings, the
/// messages never quote config text.
pub fn migrate(text: &str) -> Result<Migration, Vec<String>> {
    let redactor = Redactor::new(Vec::new());
    let redact = |messages: Vec<String>| -> Vec<String> {
        messages
            .into_iter()
            .map(|message| redactor.redact(&message).into_owned())
            .collect()
    };

    let stray = stray_placeholders(text);
    if !stray.is_empty() {
        return Err(redact(stray));
    }
    let cfg: TginConfig = ron::from_str(text).map_err(|err| redact(vec![parse_error(&err)]))?;

    let mut migrator = Migrator::new(&redactor);
    let body = migrator.config(&cfg);
    if !migrator.errors.is_empty() {
        return Err(redact(migrator.errors));
    }
    let warnings = redact(migrator.warnings);
    let mut yaml = String::from(HEADER);
    yaml.push('\n');
    if !warnings.is_empty() {
        yaml.push_str("# Warnings:\n");
        for warning in &warnings {
            yaml.push_str("# - ");
            yaml.push_str(warning);
            yaml.push('\n');
        }
    }
    yaml.push('\n');
    yaml.push_str(&emit::render(&body));
    Ok(Migration { yaml, warnings })
}

/// `tgin config migrate <path> [--out <out>]`: writes the YAML to `out` or
/// stdout and the warnings to stderr, and returns the exit code.
pub fn run(path: &Path, out: Option<&Path>) -> u8 {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("tgin: cannot read {}: {err}", path.display());
            return exit::FAILURE;
        }
    };
    let migration = match migrate(&text) {
        Ok(migration) => migration,
        Err(errors) => {
            for error in &errors {
                eprintln!("error: {error}");
            }
            eprintln!("tgin: {} was not migrated", path.display());
            return exit::FAILURE;
        }
    };
    for warning in &migration.warnings {
        eprintln!("warning: {warning}");
    }
    let written = match out {
        Some(out) => std::fs::write(out, &migration.yaml)
            .map_err(|err| format!("cannot write {}: {err}", out.display())),
        None => std::io::stdout()
            .lock()
            .write_all(migration.yaml.as_bytes())
            .map_err(|err| format!("cannot write to stdout: {err}")),
    };
    if let Err(err) = written {
        eprintln!("tgin: {err}");
        return exit::FAILURE;
    }
    if migration.warnings.is_empty() {
        exit::OK
    } else {
        exit::MIGRATED_WITH_WARNINGS
    }
}

/// Builds the v1 document, collecting errors and warnings.
struct Migrator<'r> {
    redactor: &'r Redactor,
    errors: Vec<String>,
    warnings: Vec<String>,
    /// Consumers in route-tree order.
    consumers: Vec<(String, Node)>,
    names: HashSet<String>,
    /// `LongPollRoute`s seen so far.
    pulls: usize,
    /// `WebhookRoute`s seen so far.
    pushes: usize,
}

impl<'r> Migrator<'r> {
    fn new(redactor: &'r Redactor) -> Self {
        Self {
            redactor,
            errors: Vec::new(),
            warnings: Vec::new(),
            consumers: Vec::new(),
            names: HashSet::new(),
            pulls: 0,
            pushes: 0,
        }
    }

    fn config(&mut self, cfg: &TginConfig) -> Node {
        let mut root = Node::map().field("version", Node::plain(1)).field(
            "runtime",
            Node::map().field("worker_threads", Node::plain(cfg.dark_threads)),
        );
        root = root.field("log", self.log(cfg));
        if let Some(server) = self.server(cfg) {
            root = root.field("server", server);
        }
        if let Some(token) = cfg.auth_token.as_deref().filter(|t| !t.trim().is_empty()) {
            // A blank 0.2 token disabled authentication; v1 has no token then.
            root = root.field(
                "admin",
                Node::map()
                    .comment(ADMIN_NOTE)
                    .field("token", self.string("auth_token", token)),
            );
        }
        if cfg.api.is_some() {
            self.warnings.push(API_DROPPED.to_owned());
        }

        let tree = self.target(&cfg.route, "route");
        if cfg.updates.len() > 1 && self.pulls > 0 {
            self.errors.push(format!(
                "the route tree sends updates from {} bots to LongPollRoute consumers, but in tgin 1.0 a pull consumer serves exactly one bot; migrate one bot at a time (remove the other updates), then merge the results with distinct consumer names",
                cfg.updates.len()
            ));
        }
        let bots = cfg
            .updates
            .iter()
            .enumerate()
            .map(|(i, update)| self.bot(i, update, &tree))
            .collect();
        root = root.field("bots", Node::Seq(bots));

        let consumers = std::mem::take(&mut self.consumers)
            .into_iter()
            .fold(Node::map(), |map, (name, consumer)| {
                map.field(name, consumer)
            });
        root.field("consumers", consumers)
    }

    fn log(&mut self, cfg: &TginConfig) -> Node {
        let level = cfg.log_level.to_ascii_lowercase();
        let level = match level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" => level,
            "off" => {
                self.warnings.push(
                    "`log_level` off has no tgin 1.0 equivalent; the converted config logs errors only"
                        .to_owned(),
                );
                "error".to_owned()
            }
            _ if PLACEHOLDER.is_match(&cfg.log_level) => {
                self.errors.push(
                    "`log_level` holds a placeholder; tgin 1.0 reads the log level literally, so write the level here"
                        .to_owned(),
                );
                String::new()
            }
            _ => {
                self.errors.push(
                    "`log_level` is not one of trace, debug, info, warn, error or off".to_owned(),
                );
                String::new()
            }
        };
        let format = match cfg.log_format {
            LogFormat::Compact => "text",
            LogFormat::Json => "json",
        };
        Node::map()
            .field("level", Node::plain(level))
            .field("format", Node::plain(format))
    }

    fn server(&mut self, cfg: &TginConfig) -> Option<Node> {
        let Some(port) = cfg.server_port else {
            if cfg.ssl.is_some() {
                self.warnings.push(
                    "`ssl` was dropped: without `server_port`, tgin 0.2 served no HTTP".to_owned(),
                );
            }
            return None;
        };
        let mut server = Node::map().field(
            "listen",
            Node::Seq(vec![Node::string(&format!("0.0.0.0:{port}"))]),
        );
        if let Some(ssl) = &cfg.ssl {
            let relative: Vec<&str> = [("`ssl.cert`", &ssl.cert), ("`ssl.key`", &ssl.key)]
                .into_iter()
                .filter(|(_, path)| is_relative(path))
                .map(|(field, _)| field)
                .collect();
            if !relative.is_empty() {
                let (is, them) = if relative.len() == 1 {
                    ("is a relative path", "it")
                } else {
                    ("are relative paths", "them")
                };
                self.warnings.push(format!(
                    "{} {is}: tgin 0.2 resolved {them} against its working directory, tgin 1.0 resolves {them} against the YAML file's directory; make {them} absolute or move the files",
                    relative.join(" and "),
                ));
            }
            server = server.field(
                "tls",
                Node::map()
                    .field("cert_file", self.string("ssl.cert", &ssl.cert))
                    .field("key_file", self.string("ssl.key", &ssl.key)),
            );
        }
        Some(server)
    }

    fn bot(&mut self, i: usize, update: &UpdateConfig, tree: &Node) -> Node {
        let n = i + 1;
        let mut bot = Node::map().field("name", Node::plain(format!("bot-{n}")));
        let ingress = match update {
            UpdateConfig::LongPollUpdate {
                token,
                url,
                default_timeout_sleep,
                error_timeout_sleep,
                long_poll_timeout,
                long_poll_limit,
            } => {
                bot = bot.field("token", self.string(&format!("updates[{i}].token"), token));
                if let Some(url) = url {
                    match bot_api_base(url, token, "/getUpdates") {
                        Some(api) => bot = self.api_url(bot, &format!("updates[{i}].url"), api),
                        None => self.errors.push(format!(
                            "updates[{i}].url: tgin 1.0 builds the getUpdates URL from `api_url`, so `url` must end in /bot<token>/getUpdates with this bot's token"
                        )),
                    }
                }
                let dropped: Vec<&str> = [
                    ("`default_timeout_sleep`", *default_timeout_sleep),
                    ("`error_timeout_sleep`", *error_timeout_sleep),
                ]
                .into_iter()
                .filter(|(_, value)| *value != 0)
                .map(|(field, _)| field)
                .collect();
                if !dropped.is_empty() {
                    self.warnings.push(format!(
                        "updates[{i}]: {} dropped; tgin 1.0 paces polling and backs off on errors by itself",
                        if dropped.len() == 1 {
                            format!("{} was", dropped[0])
                        } else {
                            format!("{} were", dropped.join(" and "))
                        }
                    ));
                }
                Node::map().field(
                    "long_poll",
                    self.long_poll(i, *long_poll_timeout, *long_poll_limit),
                )
            }
            UpdateConfig::WebhookUpdate {
                path,
                registration,
                secret_token,
            } => {
                let (token, api) = match registration {
                    Some(registration) => self.registration(i, registration),
                    None => (None, None),
                };
                let token = match token {
                    Some(token) => self.string(&format!("updates[{i}].token"), token),
                    None => {
                        self.warnings.push(format!(
                            "updates[{i}]: bot-{n} is a webhook without `registration`, so the 0.2 config has no bot token; set TGIN_BOT_{n}_TOKEN to it"
                        ));
                        Node::string(&format!("${{env:TGIN_BOT_{n}_TOKEN}}"))
                    }
                };
                bot = bot.field("token", token);
                if let Some(api) = api {
                    bot = self.api_url(
                        bot,
                        &format!("updates[{i}].registration.set_webhook_url"),
                        api,
                    );
                }
                let mut hook =
                    Node::map().field("path", self.string(&format!("updates[{i}].path"), path));
                if let Some(registration) = registration {
                    hook = hook
                        .field(
                            "public_url",
                            self.string(
                                &format!("updates[{i}].registration.public_ip"),
                                &registration.public_ip,
                            ),
                        )
                        .field("register", Node::plain("true"));
                }
                let secret = match secret_token.as_deref() {
                    // 0.2 checked no secret; v1's default `auto` would.
                    None => {
                        self.warnings.push(format!(
                            "updates[{i}]: bot-{n}: the webhook has no secret token, so anyone who can reach its path can post updates; set secret_token and register it with Telegram"
                        ));
                        Node::plain("none")
                    }
                    Some("auto" | "none") => {
                        self.errors.push(format!(
                            "updates[{i}].secret_token: `auto` and `none` are keywords in tgin 1.0, not secrets; choose another secret_token"
                        ));
                        Node::plain("none")
                    }
                    Some(secret) => self.string(&format!("updates[{i}].secret_token"), secret),
                };
                Node::map().field("webhook", hook.field("secret_token", secret))
            }
        };
        bot.field("ingress", ingress).field(
            "routes",
            Node::Seq(vec![Node::map().field("to", Node::Seq(vec![tree.clone()]))]),
        )
    }

    /// The bot token and Bot API base URL a webhook registration names.
    fn registration<'c>(
        &mut self,
        i: usize,
        registration: &'c RegistrationWebhookConfig,
    ) -> (Option<&'c str>, Option<&'c str>) {
        let Some(url) = &registration.set_webhook_url else {
            return (Some(&registration.token), None);
        };
        match bot_api_base_and_token(url, "/setWebhook") {
            Some((api, token)) => (Some(token), Some(api)),
            None => {
                self.errors.push(format!(
                    "updates[{i}].registration.set_webhook_url: tgin 1.0 builds the setWebhook URL from `api_url`, so it must end in /bot<token>/setWebhook"
                ));
                (Some(&registration.token), None)
            }
        }
    }

    fn api_url(&mut self, bot: Node, field: &str, api: &str) -> Node {
        if api == DEFAULT_API_URL {
            bot
        } else {
            let url = self.string(field, api);
            bot.field("api_url", url)
        }
    }

    fn long_poll(&mut self, i: usize, timeout: u64, limit: u64) -> Node {
        let mut poll = Node::map();
        // 0.2 capped both at Telegram's limits; v1 rejects values beyond them.
        let timeout = if timeout == 0 {
            self.warnings.push(format!(
                "updates[{i}]: `long_poll_timeout: 0` is now 1s, the shortest long poll tgin 1.0 allows"
            ));
            1
        } else {
            timeout.min(MAX_LONG_POLL_TIMEOUT)
        };
        if timeout != DEFAULT_LONG_POLL_TIMEOUT {
            poll = poll.field("timeout", Node::plain(format!("{timeout}s")));
        }
        let limit = limit.clamp(1, DEFAULT_LONG_POLL_LIMIT);
        if limit != DEFAULT_LONG_POLL_LIMIT {
            poll = poll.field("limit", Node::plain(limit));
        }
        poll
    }

    /// The v1 target for a 0.2 route; leaves become consumers.
    fn target(&mut self, route: &RouteConfig, at: &str) -> Node {
        match route {
            RouteConfig::LongPollRoute {
                path,
                max_buffered_updates,
            } => {
                self.pulls += 1;
                let name = match self.slug_of(path) {
                    Some(slug) => self.unique(&format!("pull-{slug}")),
                    None => self.unique(&format!("pull-{}", self.pulls)),
                };
                let alias = self.string(&format!("{at}.path"), path);
                let pull = Node::map()
                    .field("capacity", Node::plain(max_buffered_updates))
                    .field("on_full", Node::plain("drop_oldest"))
                    .field("aliases", Node::Seq(vec![alias]));
                self.consumers
                    .push((name.clone(), Node::map().field("pull", pull)));
                Node::plain(name)
            }
            RouteConfig::WebhookRoute {
                url,
                request_timeout_ms,
            } => {
                self.pushes += 1;
                // Only the origin names the consumer: 0.2 users often
                // authenticated a downstream with a secret path segment, and
                // names show up in logs, metrics and /v1/status.
                let slug = if PLACEHOLDER.is_match(url) {
                    None
                } else {
                    match url::Url::parse(url) {
                        Ok(parsed) => parsed.host_str().and_then(|host| match parsed.port() {
                            Some(port) => self.slug_of(&format!("{host}:{port}")),
                            None => self.slug_of(host),
                        }),
                        Err(_) => {
                            self.errors.push(format!("{at}.url is not a valid URL"));
                            None
                        }
                    }
                };
                let name = match slug {
                    Some(slug) => self.unique(&format!("push-{slug}")),
                    None => self.unique(&format!("push-{}", self.pushes)),
                };
                let mut endpoint = Node::map().field("url", self.string(&format!("{at}.url"), url));
                if *request_timeout_ms != DEFAULT_REQUEST_TIMEOUT_MS {
                    endpoint =
                        endpoint.field("timeout", Node::plain(format!("{request_timeout_ms}ms")));
                }
                let push = Node::map()
                    .field("endpoints", Node::Seq(vec![endpoint]))
                    .comment(PUSH_NOTE)
                    // With `backpressure`, one dead endpoint would stall
                    // every other consumer of the bot.
                    .field("on_full", Node::plain("drop_oldest"))
                    .field("retry", Node::map().field("max_attempts", Node::plain(1)));
                self.consumers
                    .push((name.clone(), Node::map().field("push", push)));
                Node::plain(name)
            }
            RouteConfig::RoundRobinLB { routes } => {
                let among = self.children(routes, at, "RoundRobinLB");
                Node::map().field(
                    "split",
                    Node::map()
                        .field("strategy", Node::plain("round_robin"))
                        .field("among", among),
                )
            }
            RouteConfig::AllLB { routes } => {
                let all = self.children(routes, at, "AllLB");
                Node::map().field("all", all)
            }
        }
    }

    fn children(&mut self, routes: &[RouteConfig], at: &str, kind: &str) -> Node {
        if routes.is_empty() {
            self.errors.push(format!("{at}: the {kind} has no routes"));
        }
        Node::Seq(
            routes
                .iter()
                .enumerate()
                .map(|(k, route)| self.target(route, &format!("{at}.routes[{k}]")))
                .collect(),
        )
    }

    /// `text` as a name part, or `None` when it holds a placeholder or a
    /// token, which must not end up in names, or nothing to name by.
    fn slug_of(&self, text: &str) -> Option<String> {
        if PLACEHOLDER.is_match(text) || self.redactor.redact(text) != text {
            return None;
        }
        let slug = slug(text);
        (!slug.is_empty()).then_some(slug)
    }

    /// `base`, cut to a valid length, or with the first free `-2`, `-3`, …
    fn unique(&mut self, base: &str) -> String {
        let mut name = truncated(base, NAME_MAX).to_owned();
        let mut k = 2;
        while !self.names.insert(name.clone()) {
            let suffix = format!("-{k}");
            name = format!("{}{suffix}", truncated(base, NAME_MAX - suffix.len()));
            k += 1;
        }
        name
    }

    /// A string field v1 expands references in, quoted.
    fn string(&mut self, field: &str, text: &str) -> Node {
        match template(text) {
            Some(value) => Node::string(&value),
            None => {
                self.errors.push(format!(
                    "{field}: a `$` right before a placeholder cannot be written in tgin 1.0, where `$${{` is an escape"
                ));
                Node::string("")
            }
        }
    }
}

/// `text` with its 0.2 placeholders as `${env:…}` references and every
/// other `${` escaped as `$${`; `None` if a `$` precedes a placeholder,
/// which v1 would read as an escape.
fn template(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len() + 8);
    let mut copied = 0;
    for caps in PLACEHOLDER.captures_iter(text) {
        let (Some(whole), Some(name)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        out.push_str(&text[copied..whole.start()].replace("${", "$${"));
        if out.ends_with('$') {
            return None;
        }
        out.push_str("${env:");
        out.push_str(name.as_str());
        out.push('}');
        copied = whole.end();
    }
    out.push_str(&text[copied..].replace("${", "$${"));
    Some(out)
}

/// The Bot API base URL of `url`, which must end in `/bot<token><method>`.
fn bot_api_base<'u>(url: &'u str, token: &str, method: &str) -> Option<&'u str> {
    url.strip_suffix(method)?
        .strip_suffix(token)?
        .strip_suffix("/bot")
        .filter(|base| !base.is_empty())
}

/// The Bot API base URL and token of `url`, which must end in
/// `/bot<token><method>`.
fn bot_api_base_and_token<'u>(url: &'u str, method: &str) -> Option<(&'u str, &'u str)> {
    let (base, token) = url.strip_suffix(method)?.rsplit_once("/bot")?;
    (!base.is_empty() && !token.is_empty() && !token.contains('/')).then_some((base, token))
}

/// Whether the written file path `text` is relative. One that starts with a
/// placeholder may expand to either, so it is not reported.
fn is_relative(text: &str) -> bool {
    let path = Path::new(text);
    !text.starts_with("${") && !path.is_absolute() && !path.has_root()
}

/// Lowercase ASCII letters and digits, with every other run of characters
/// as one `-`, trimmed.
fn slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

/// The first `max` bytes of the ASCII name `name`, without a trailing `-`.
fn truncated(name: &str, max: usize) -> &str {
    name.get(..max).unwrap_or(name).trim_end_matches('-')
}

/// One error per placeholder outside a string literal: 0.2 substituted it
/// into the RON text, but v1 expands references only inside strings.
fn stray_placeholders(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut errors = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        i = match bytes[i] {
            b'"' => skip_quoted(bytes, i + 1, b'"'),
            b'\'' => skip_quoted(bytes, i + 1, b'\''),
            b'r' if i == 0 || !is_ident(bytes[i - 1]) => skip_raw_string(bytes, i + 1),
            b'/' if bytes.get(i + 1) == Some(&b'/') => bytes[i..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |end| i + end),
            b'/' if bytes.get(i + 1) == Some(&b'*') => skip_block_comment(bytes, i + 2),
            b'$' => match text.get(i..).and_then(|rest| PLACEHOLDER.captures(rest)) {
                Some(caps) if caps.get(0).is_some_and(|whole| whole.start() == 0) => {
                    let (line, column) = position(text, i);
                    let name = caps.get(1).map_or("", |name| name.as_str());
                    errors.push(format!(
                        "line {line}, column {column}: the placeholder ${{{name}}} is outside a string; tgin 1.0 expands references only inside strings, so write a literal value here"
                    ));
                    i + caps.get(0).map_or(1, |whole| whole.end())
                }
                _ => i + 1,
            },
            _ => i + 1,
        };
    }
    errors
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The index after the string or char literal whose body starts at `i`.
fn skip_quoted(bytes: &[u8], mut i: usize, quote: u8) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// The index after the raw string `r#…"…"#…` whose `#`s start at `i`, or
/// `i` if this `r` starts no raw string.
fn skip_raw_string(bytes: &[u8], i: usize) -> usize {
    let hashes = bytes[i..].iter().take_while(|&&b| b == b'#').count();
    if bytes.get(i + hashes) != Some(&b'"') {
        return i;
    }
    let mut j = i + hashes + 1;
    while j < bytes.len() {
        if bytes[j] == b'"'
            && bytes.len() >= j + 1 + hashes
            && bytes[j + 1..j + 1 + hashes].iter().all(|&b| b == b'#')
        {
            return j + 1 + hashes;
        }
        j += 1;
    }
    bytes.len()
}

/// The index after the (nestable) block comment whose body starts at `i`.
fn skip_block_comment(bytes: &[u8], mut i: usize) -> usize {
    let mut depth = 1;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else if bytes[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// The 1-based line and column (in characters) of byte `at`.
fn position(text: &str, at: usize) -> (usize, usize) {
    let before = text.get(..at).unwrap_or(text);
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |nl| nl + 1);
    let column = before[line_start..].chars().count() + 1;
    (line, column)
}

/// A RON parse error as position and kind; never the text around it.
fn parse_error(err: &ron::error::SpannedError) -> String {
    let at = err.span.start;
    format!(
        "line {}, column {}: {}",
        at.line,
        at.col,
        parse_error_kind(&err.code)
    )
}

/// What went wrong, without the input text some kinds carry.
fn parse_error_kind(code: &ron::Error) -> String {
    use ron::Error as E;
    match code {
        E::NoSuchStructField { expected, .. } => {
            format!("unknown field; expected one of: {}", expected.join(", "))
        }
        E::NoSuchEnumVariant { expected, .. } => {
            format!("unknown variant; expected one of: {}", expected.join(", "))
        }
        E::MissingStructField { field, .. } => format!("missing field `{field}`"),
        E::DuplicateStructField { field, .. } => format!("duplicate field `{field}`"),
        E::ExpectedDifferentStructName { expected, .. } => {
            format!("expected the struct name `{expected}`")
        }
        E::ExpectedNamedStructLike(name) => format!("expected `{name}(…)`"),
        E::InvalidValueForType { expected, .. } => format!("invalid value; expected {expected}"),
        E::ExpectedDifferentLength { expected, found } => {
            format!("expected {expected}, found {found} elements")
        }
        E::UnexpectedChar(_) => "unexpected character".to_owned(),
        E::InvalidIntegerDigit { base, .. } => format!("invalid base-{base} digit"),
        // These carry no input text.
        E::Fmt
        | E::Eof
        | E::ExpectedArray
        | E::ExpectedArrayEnd
        | E::ExpectedAttribute
        | E::ExpectedAttributeEnd
        | E::ExpectedBoolean
        | E::ExpectedComma
        | E::ExpectedChar
        | E::ExpectedByteLiteral
        | E::ExpectedFloat
        | E::FloatUnderscore
        | E::ExpectedInteger
        | E::ExpectedOption
        | E::ExpectedOptionEnd
        | E::ExpectedMap
        | E::ExpectedMapColon
        | E::ExpectedMapEnd
        | E::ExpectedStructLike
        | E::ExpectedStructLikeEnd
        | E::ExpectedUnit
        | E::ExpectedString
        | E::ExpectedByteString
        | E::ExpectedStringEnd
        | E::ExpectedIdentifier
        | E::InvalidEscape(_)
        | E::IntegerOutOfBounds
        | E::UnclosedBlockComment
        | E::UnclosedLineComment
        | E::UnderscoreAtBeginning
        | E::Utf8Error(_)
        | E::TrailingCharacters
        | E::ExpectedRawValue
        | E::ExceededRecursionLimit => code.to_string(),
        // Messages, identifiers and anything newer may quote the input.
        _ => "invalid RON".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_converts_placeholders_and_escapes_literals() {
        assert_eq!(template("${A}").as_deref(), Some("${env:A}"));
        assert_eq!(
            template("x${A}y${B_2}z").as_deref(),
            Some("x${env:A}y${env:B_2}z")
        );
        // Not 0.2 placeholders: copied, with `${` escaped for v1.
        assert_eq!(
            template("${a-b} ${} $x").as_deref(),
            Some("$${a-b} $${} $x")
        );
        assert_eq!(template("$${a-b}").as_deref(), Some("$$${a-b}"));
        assert_eq!(
            template("${a-b}${A}${}").as_deref(),
            Some("$${a-b}${env:A}$${}")
        );
        // 0.2 substituted `$${A}` to `$` + value; v1 cannot say that.
        assert_eq!(template("$${A}"), None);
    }

    #[test]
    fn slug_and_truncation() {
        assert_eq!(slug("/Bot1//getUpdates/"), "bot1-getupdates");
        assert_eq!(slug("10.5.0.11/webhook"), "10-5-0-11-webhook");
        assert_eq!(slug("/ä/"), "");
        assert_eq!(truncated("pull-a-b", 7), "pull-a");
    }

    #[test]
    fn parse_error_kinds_never_carry_input() {
        let secret = "zz_unknown_secret_field";
        let err = ron::from_str::<TginConfig>(&format!("({secret}: 1)")).unwrap_err();
        assert!(err.to_string().contains(secret), "ron names the field");
        let message = parse_error(&err);
        assert!(!message.contains(secret), "{message}");
        assert!(message.starts_with("line 1, column "), "{message}");
        assert!(
            message.contains("expected one of: dark_threads"),
            "{message}"
        );
    }
}
