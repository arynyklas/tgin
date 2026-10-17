//! Checks on the raw config that need no environment.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use indexmap::IndexMap;
use serde_saphyr::Location;
use tgin_runtime::config::{ConfigError, Located};
use url::Url;

use super::model::{
    At, Consumer, Gateway, HumanDuration, Ingress, Name, Push, RateMode, RawConfig, Split,
    SplitStrategy, Target,
};

/// Paths that tgin itself serves or will serve: a configured path must not
/// be one of them or lie below one. The Bot API gateway's paths are reserved
/// by [`gateway_path`] instead.
const RESERVED: [&str; 6] = ["/c", "/mtproto", "/v1", "/livez", "/readyz", "/metrics"];

/// The shortest `reload.debounce`.
const MIN_DEBOUNCE: Duration = Duration::from_millis(10);
/// The shortest pull `lease_timeout`: leases are expired by a check that
/// runs once a second.
const MIN_LEASE_TIMEOUT: Duration = Duration::from_secs(1);
/// The shortest gateway `upstream_idle_timeout`.
const MIN_UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(1);
/// The longest gateway `max_delay` and `retry_429.max_wait`: a client
/// waits that long for its request.
const MAX_GATEWAY_WAIT: Duration = Duration::from_secs(60);

/// Errors and warnings found so far.
#[derive(Debug, Default)]
pub(super) struct Diagnostics {
    pub(super) errors: Vec<ConfigError>,
    pub(super) warnings: Vec<ConfigError>,
}

impl Diagnostics {
    pub(super) fn error(
        &mut self,
        path: impl Into<String>,
        location: Location,
        message: impl Into<String>,
    ) {
        self.errors
            .push(ConfigError::new(message).at(located(path, location)));
    }

    pub(super) fn warning(
        &mut self,
        path: impl Into<String>,
        location: Location,
        message: impl Into<String>,
    ) {
        self.warnings
            .push(ConfigError::warning(message).at(located(path, location)));
    }
}

pub(super) fn located(path: impl Into<String>, location: Location) -> Located {
    let saturate = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
    Located {
        path: path.into(),
        line: saturate(location.line()),
        column: saturate(location.column()),
    }
}

/// `primary` if it was written in the document, else `fallback`.
fn known(primary: Location, fallback: Location) -> Location {
    if primary.line() == 0 {
        fallback
    } else {
        primary
    }
}

/// Validates `raw`. HTTP paths and interpolated fields need the
/// environment, so `resolve` checks them.
pub(super) fn validate(raw: &RawConfig, diag: &mut Diagnostics) {
    if raw.version.value != 1 {
        diag.error(
            "version",
            raw.version.location,
            format!(
                "unsupported config version {}; this tgin reads version 1",
                raw.version.value
            ),
        );
    }

    duration(diag, "runtime.lame_duck", &raw.runtime.lame_duck);
    duration(diag, "runtime.shutdown_grace", &raw.runtime.shutdown_grace);
    if let Some(server) = &raw.server {
        at_least_one(
            diag,
            "server.max_connections".to_owned(),
            &server.max_connections,
        );
        duration(
            diag,
            "server.header_read_timeout",
            &server.header_read_timeout,
        );
        positive(
            diag,
            "server.header_read_timeout",
            &server.header_read_timeout,
            "0 would close every connection before its headers arrive; 10s is the default",
        );
    }
    if raw.admin.token.is_none() {
        for (i, listen) in raw.admin.listen.iter().enumerate() {
            if !listen.value.ip().is_loopback() {
                diag.error(
                    format!("admin.listen[{i}]"),
                    listen.location,
                    format!(
                        "the admin API listens on {}, which is not a loopback address, so `admin.token` is required",
                        listen.value
                    ),
                );
            }
        }
    }
    duration(diag, "reload.debounce", &raw.reload.debounce);
    if raw.reload.debounce.value.get() < MIN_DEBOUNCE {
        diag.error(
            "reload.debounce",
            raw.reload.debounce.location,
            format!(
                "`{}` is shorter than the minimum of 10ms",
                raw.reload.debounce.value.as_str()
            ),
        );
    }
    duration(diag, "reload.drain_timeout", &raw.reload.drain_timeout);

    consumers(raw, diag);
    bots(raw, diag);
}

fn consumers(raw: &RawConfig, diag: &mut Diagnostics) {
    for (name, consumer) in &raw.consumers {
        let path = format!("consumers.{}", name.as_str());
        check_name(diag, &path, name.as_str(), consumer.location);
        match &consumer.value {
            Consumer::Pull(pull) => {
                let path = format!("{path}.pull");
                if raw.server.is_none() {
                    diag.error(
                        &path,
                        consumer.location,
                        "a pull consumer needs a `server` block to serve getUpdates",
                    );
                }
                at_least_one(diag, format!("{path}.capacity"), &pull.capacity);
                capacity_bytes(diag, format!("{path}.capacity_bytes"), &pull.capacity_bytes);
                duration(diag, format!("{path}.lease_timeout"), &pull.lease_timeout);
                if pull.lease_timeout.value.get() < MIN_LEASE_TIMEOUT {
                    diag.errors.push(
                        ConfigError::new(format!(
                            "`{}` is shorter than the minimum of 1s",
                            pull.lease_timeout.value.as_str()
                        ))
                        .at(located(
                            format!("{path}.lease_timeout"),
                            known(pull.lease_timeout.location, consumer.location),
                        ))
                        .hint("leases are expired by a check that runs once a second, so a shorter lease ends at the next check; 2m is the default"),
                    );
                }
            }
            Consumer::Push(push) => {
                self::push(diag, &format!("{path}.push"), consumer.location, push)
            }
        }
    }
}

fn push(diag: &mut Diagnostics, path: &str, location: Location, push: &Push) {
    if push.endpoints.value.is_empty() && !push.accept_set_webhook {
        diag.errors.push(
            ConfigError::new("a push consumer needs at least one endpoint")
                .at(located(
                    format!("{path}.endpoints"),
                    known(push.endpoints.location, location),
                ))
                .hint("or set `accept_set_webhook: true` to take its endpoint from the bot's `setWebhook`"),
        );
    }
    for (k, endpoint) in push.endpoints.value.iter().enumerate() {
        let path = format!("{path}.endpoints[{k}].timeout");
        duration(diag, path.clone(), &endpoint.timeout);
        positive(
            diag,
            path,
            &endpoint.timeout,
            "0 times out every request at once; give the longest a delivery may take (10s is the default)",
        );
    }
    at_least_one(diag, format!("{path}.max_in_flight"), &push.max_in_flight);
    at_least_one(diag, format!("{path}.capacity"), &push.capacity);
    capacity_bytes(diag, format!("{path}.capacity_bytes"), &push.capacity_bytes);
    let retry_wait = "0 would retry a failed delivery without waiting; the defaults are 1s (`initial`) and 1m (`max`)";
    duration(diag, format!("{path}.retry.initial"), &push.retry.initial);
    positive(
        diag,
        format!("{path}.retry.initial"),
        &push.retry.initial,
        retry_wait,
    );
    duration(diag, format!("{path}.retry.max"), &push.retry.max);
    positive(
        diag,
        format!("{path}.retry.max"),
        &push.retry.max,
        retry_wait,
    );
    at_least_one(
        diag,
        format!("{path}.retry.max_attempts"),
        &push.retry.max_attempts,
    );
    duration(diag, format!("{path}.retry.max_age"), &push.retry.max_age);
    positive(
        diag,
        format!("{path}.retry.max_age"),
        &push.retry.max_age,
        "0 dead-letters every update before its first attempt; 1h is the default",
    );
    at_least_one(
        diag,
        format!("{path}.breaker.failure_threshold"),
        &push.breaker.failure_threshold,
    );
    duration(
        diag,
        format!("{path}.breaker.open_for"),
        &push.breaker.open_for,
    );
    positive(
        diag,
        format!("{path}.breaker.open_for"),
        &push.breaker.open_for,
        "0 would let requests through as soon as the breaker opens; 30s is the default",
    );
}

fn bots(raw: &RawConfig, diag: &mut Diagnostics) {
    let mut names: HashMap<&str, usize> = HashMap::new();
    // The bot that first targets each consumer that serves one bot.
    let mut owners: HashMap<usize, &str> = HashMap::new();
    for (i, bot) in raw.bots.iter().enumerate() {
        let path = format!("bots[{i}]");
        let name = bot.name.value.as_str();
        check_name(diag, &format!("{path}.name"), name, bot.name.location);
        if let Some(first) = names.get(name) {
            diag.error(
                format!("{path}.name"),
                bot.name.location,
                format!("bot name `{name}` is already used by bots[{first}]"),
            );
        } else {
            names.insert(name, i);
        }

        if let Some(ingress) = &bot.ingress {
            self::ingress(raw, diag, &path, ingress);
        }
        self::gateway(
            diag,
            &format!("{path}.gateway"),
            &bot.gateway,
            bot.name.location,
        );

        let mut reached = Vec::new();
        for (j, route) in bot.routes.iter().enumerate() {
            let path = format!("{path}.routes[{j}]");
            if let Some(m) = &route.value.r#match {
                commands(diag, &path, &m.commands);
            }
            targets(
                diag,
                &raw.consumers,
                &route.value.to,
                &format!("{path}.to"),
                &mut reached,
            );
        }
        // One report per consumer and bot, at its first reference.
        let mut seen = HashSet::new();
        reached.retain(|(consumer, _, _)| seen.insert(*consumer));
        if let Some(last) = bot.routes.last()
            && last.value.r#match.as_ref().is_some_and(|m| !m.is_empty())
        {
            diag.warning(
                format!("{path}.routes"),
                last.location,
                "last rule has a match; unmatched updates are dropped",
            );
        }

        for (consumer, location, target_path) in reached {
            let Some((consumer_name, consumer_value)) = raw.consumers.get_index(consumer) else {
                continue;
            };
            let (kind, rule) = match &consumer_value.value {
                Consumer::Pull(_) => ("pull", "a pull consumer serves one bot"),
                // A bot's token holder must never redirect another bot's
                // updates with `setWebhook`.
                Consumer::Push(push) if push.accept_set_webhook => (
                    "push",
                    "with `accept_set_webhook: true`, a push consumer serves one bot",
                ),
                Consumer::Push(_) => continue,
            };
            match owners.get(&consumer) {
                None => {
                    owners.insert(consumer, name);
                }
                Some(owner) if *owner == name => {}
                Some(owner) => diag.error(
                    target_path,
                    location,
                    format!(
                        "{kind} consumer `{}` is targeted by bots `{owner}` and `{name}`; {rule}",
                        consumer_name.as_str()
                    ),
                ),
            }
        }
    }
}

/// A bot's `gateway` block. Values left out are defaults, which are valid,
/// so every error points at a written value.
fn gateway(diag: &mut Diagnostics, path: &str, gateway: &Gateway, fallback: Location) {
    let idle = &gateway.upstream_idle_timeout;
    duration(diag, format!("{path}.upstream_idle_timeout"), idle);
    if idle.value.get() < MIN_UPSTREAM_IDLE_TIMEOUT {
        diag.errors.push(
            ConfigError::new(format!(
                "`{}` is shorter than the minimum of 1s",
                idle.value.as_str()
            ))
            .at(located(
                format!("{path}.upstream_idle_timeout"),
                known(idle.location, fallback),
            ))
            .hint("a slow but steady Bot API answer would be cut; 1m is the default"),
        );
    }

    let limit = &gateway.rate_limit;
    let path_limit = format!("{path}.rate_limit");
    let max_delay = &limit.max_delay;
    duration(diag, format!("{path_limit}.max_delay"), max_delay);
    at_most(
        diag,
        format!("{path_limit}.max_delay"),
        max_delay,
        MAX_GATEWAY_WAIT,
        "a client waits this long for its request to start; 5s is the default, and 0s answers 429 at once",
    );
    for (k, method) in limit.methods.value.iter().enumerate() {
        if !valid_method_pattern(&method.value) {
            diag.errors.push(
                ConfigError::new(format!("invalid method pattern `{}`", method.value))
                    .at(located(format!("{path_limit}.methods[{k}]"), method.location))
                    .hint("use a method name such as `sendMessage`, or a prefix followed by one `*`, such as `send*`"),
            );
        }
    }
    if limit.methods.value.is_empty() && limit.mode != RateMode::Off {
        diag.errors.push(
            ConfigError::new("the rate limit needs at least one method")
                .at(located(
                    format!("{path_limit}.methods"),
                    known(limit.methods.location, fallback),
                ))
                .hint("list the methods to limit, or set `mode: off`"),
        );
    }
    for (block, rate) in [
        ("global", limit.global.rate()),
        ("per_chat", limit.per_chat.rate()),
        ("per_group", limit.per_group.rate()),
    ] {
        let path = format!("{path_limit}.{block}");
        at_least_one(diag, format!("{path}.count"), rate.count);
        duration(diag, format!("{path}.per"), rate.per);
        positive(
            diag,
            format!("{path}.per"),
            rate.per,
            "0 would leave no time between requests; give the period `count` requests are spread over",
        );
        at_least_one(diag, format!("{path}.burst"), rate.burst);
    }

    let max_wait = &gateway.retry_429.max_wait;
    duration(diag, format!("{path}.retry_429.max_wait"), max_wait);
    at_most(
        diag,
        format!("{path}.retry_429.max_wait"),
        max_wait,
        MAX_GATEWAY_WAIT,
        "the client waits this long for the retry; 10s is the default, and 0s turns the retry off",
    );
}

/// Whether `method` is a gateway method pattern, [`METHOD_PATTERN`]: ASCII
/// letters, optionally followed by one `*`.
///
/// [`METHOD_PATTERN`]: super::schema::METHOD_PATTERN
fn valid_method_pattern(method: &str) -> bool {
    let name = method.strip_suffix('*').unwrap_or(method);
    !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphabetic())
}

fn ingress(raw: &RawConfig, diag: &mut Diagnostics, path: &str, ingress: &At<Ingress>) {
    match &ingress.value {
        Ingress::LongPoll(poll) => {
            let path = format!("{path}.ingress.long_poll");
            duration(diag, format!("{path}.timeout"), &poll.timeout);
            if poll.timeout.value.get() < Duration::from_secs(1) {
                diag.errors.push(
                    ConfigError::new(format!(
                        "`{}` is shorter than the minimum of 1s",
                        poll.timeout.value.as_str()
                    ))
                    .at(located(
                        format!("{path}.timeout"),
                        known(poll.timeout.location, ingress.location),
                    ))
                    .hint("Telegram takes whole seconds, and 0 would poll in a tight loop; use 1s or more (30s is usual)"),
                );
            }
            in_range(diag, format!("{path}.limit"), &poll.limit, ingress.location);
        }
        Ingress::Webhook(hook) => {
            let path = format!("{path}.ingress.webhook");
            if raw.server.is_none() {
                diag.error(
                    &path,
                    ingress.location,
                    "a webhook bot needs a `server` block to receive updates",
                );
            }
            if hook.register.value && hook.public_url.is_none() {
                diag.error(
                    format!("{path}.register"),
                    hook.register.location,
                    "`register: true` needs `public_url`, the https URL Telegram posts to",
                );
            }
            if let Some(certificate) = &hook.certificate_file
                && !hook.register.value
            {
                diag.error(
                    format!("{path}.certificate_file"),
                    certificate.0.location,
                    "`certificate_file` is uploaded by setWebhook, so it needs `register: true`",
                );
            }
            if let Some(max) = &hook.max_connections {
                in_range(
                    diag,
                    format!("{path}.max_connections"),
                    max,
                    ingress.location,
                );
            }
        }
    }
}

/// Checks every target, recording the consumers reached as (index,
/// location, path).
fn targets(
    diag: &mut Diagnostics,
    consumers: &IndexMap<Name, At<Consumer>>,
    targets: &[At<Target>],
    path: &str,
    reached: &mut Vec<(usize, Location, String)>,
) {
    for (k, target) in targets.iter().enumerate() {
        let path = format!("{path}[{k}]");
        match &target.value {
            Target::Consumer(name) => match consumers.get_index_of(name.as_str()) {
                Some(index) => reached.push((index, target.location, path)),
                None => {
                    let defined: Vec<&str> = consumers.keys().map(Name::as_str).collect();
                    let hint = if defined.is_empty() {
                        "no consumers are defined".to_owned()
                    } else {
                        format!("defined consumers: {}", defined.join(", "))
                    };
                    diag.errors.push(
                        ConfigError::new(format!("unknown consumer `{name}`"))
                            .at(located(path, target.location))
                            .hint(hint),
                    );
                }
            },
            Target::All(children) => {
                self::targets(diag, consumers, children, &format!("{path}.all"), reached);
            }
            Target::Split(split) => {
                let path = format!("{path}.split");
                weights(diag, &path, split, target.location);
                self::targets(
                    diag,
                    consumers,
                    &split.among,
                    &format!("{path}.among"),
                    reached,
                );
            }
        }
    }
}

fn weights(diag: &mut Diagnostics, path: &str, split: &Split, fallback: Location) {
    let weighted = split.strategy.value == SplitStrategy::Weighted;
    let Some(weights) = &split.weights else {
        if weighted {
            diag.error(
                format!("{path}.strategy"),
                known(split.strategy.location, fallback),
                "a weighted split needs `weights`, one per `among` entry",
            );
        }
        return;
    };
    let location = known(weights.location, fallback);
    if !weighted {
        diag.error(
            format!("{path}.weights"),
            location,
            "`weights` are only allowed with `strategy: weighted`",
        );
        return;
    }
    if weights.value.len() != split.among.len() {
        diag.error(
            format!("{path}.weights"),
            location,
            format!(
                "{} weights for {} `among` entries; give one weight per entry",
                weights.value.len(),
                split.among.len()
            ),
        );
    }
    for (k, weight) in weights.value.iter().enumerate() {
        if weight.value == 0 {
            diag.error(
                format!("{path}.weights[{k}]"),
                known(weight.location, location),
                "weights must be greater than 0",
            );
        }
    }
}

fn check_name(diag: &mut Diagnostics, path: &str, name: &str, location: Location) {
    let bytes = name.as_bytes();
    let allowed = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    let valid = bytes.len() <= 63
        && bytes.first().is_some_and(allowed)
        && bytes.iter().all(|c| allowed(c) || *c == b'-');
    if !valid {
        diag.errors.push(
            ConfigError::new(format!("invalid name `{name}`"))
                .at(located(path, location))
                .hint(
                    "use 1 to 63 lowercase letters, digits and `-`, starting with a letter or digit",
                ),
        );
    }
}

/// Whether `command`, without one optional leading `/`, is a name Telegram
/// can send: `[A-Za-z0-9_]{1,32}`.
pub(super) fn valid_command(command: &str) -> bool {
    let name = command.strip_prefix('/').unwrap_or(command);
    (1..=32).contains(&name.len()) && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn commands(diag: &mut Diagnostics, path: &str, commands: &[At<String>]) {
    for (k, command) in commands.iter().enumerate() {
        if !valid_command(&command.value) {
            diag.errors.push(
                ConfigError::new(format!("invalid command `{}`", command.value))
                    .at(located(format!("{path}.match.commands[{k}]"), command.location))
                    .hint(
                        "a command is 1 to 32 ASCII letters, digits or `_`, optionally after `/`, with no `@bot` suffix",
                    ),
            );
        }
    }
}

/// Rejects humantime's month and year units.
fn duration(diag: &mut Diagnostics, path: impl Into<String>, value: &At<HumanDuration>) {
    if let Some(unit) = value.value.calendar_unit() {
        diag.errors.push(
            ConfigError::new(format!(
                "`{}` uses `{unit}`, a month or year unit",
                value.value.as_str()
            ))
            .at(located(path, value.location))
            .hint("write days (`d`) or weeks (`w`) instead"),
        );
    }
}

/// Rejects a zero duration, which would disable what it configures or make
/// it misbehave; `hint` says what 0 would do.
fn positive(
    diag: &mut Diagnostics,
    path: impl Into<String>,
    value: &At<HumanDuration>,
    hint: &str,
) {
    if value.value.get().is_zero() {
        diag.errors.push(
            ConfigError::new(format!("`{}` must be longer than 0", value.value.as_str()))
                .at(located(path, value.location))
                .hint(hint),
        );
    }
}

/// Rejects a duration longer than `limit`; `hint` says what the value
/// does.
fn at_most(
    diag: &mut Diagnostics,
    path: impl Into<String>,
    value: &At<HumanDuration>,
    limit: Duration,
    hint: &str,
) {
    if value.value.get() > limit {
        diag.errors.push(
            ConfigError::new(format!(
                "`{}` is longer than the maximum of {}",
                value.value.as_str(),
                HumanDuration::from(limit).as_str()
            ))
            .at(located(path, value.location))
            .hint(hint),
        );
    }
}

fn at_least_one<T: PartialEq + Default>(diag: &mut Diagnostics, path: String, value: &At<T>) {
    if value.value == T::default() {
        diag.error(path, value.location, "must be at least 1");
    }
}

/// A queue's `capacity_bytes`: at least 1 MiB, so every update fits.
fn capacity_bytes(diag: &mut Diagnostics, path: String, value: &At<super::model::ByteSize>) {
    if value.value.0 < 1 << 20 {
        diag.errors.push(
            ConfigError::new("must be at least 1MiB")
                .at(located(path, value.location))
                .hint("updates can be up to 1 MiB, and a larger update is dropped"),
        );
    }
}

/// Telegram's 1 to 100.
fn in_range(diag: &mut Diagnostics, path: String, value: &At<u32>, fallback: Location) {
    if !(1..=100).contains(&value.value) {
        diag.error(
            path,
            known(value.location, fallback),
            format!("{} is out of range; use 1 to 100", value.value),
        );
    }
}

/// Every configured HTTP path, keyed by its expanded value, with the YAML
/// path of its first use. Expanded paths may hold secrets, so messages
/// never quote them.
#[derive(Default)]
pub(super) struct HttpPaths {
    seen: HashMap<String, String>,
}

impl HttpPaths {
    pub(super) fn add(
        &mut self,
        diag: &mut Diagnostics,
        yaml_path: String,
        http: &str,
        location: Location,
    ) {
        if let Some(problem) = path_problem(http) {
            diag.error(yaml_path, location, format!("the path {problem}"));
            return;
        }
        if let Some(first) = self.seen.get(http) {
            diag.error(
                yaml_path,
                location,
                format!("the path is already used by {first}"),
            );
        } else {
            self.seen.insert(http.to_owned(), yaml_path);
        }
    }
}

/// What is wrong with a configured HTTP path, if anything. Messages never
/// quote the path, which may hold a secret.
fn path_problem(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return Some("must start with `/`".to_owned());
    }
    if path.contains(['{', '}']) {
        return Some("contains `{` or `}`; paths are literal".to_owned());
    }
    if path
        .split('/')
        .any(|segment| segment.starts_with([':', '*']))
    {
        return Some("has a segment starting with `:` or `*`; paths are literal".to_owned());
    }
    if let Some(problem) = url_problem(path) {
        return Some(problem.to_owned());
    }
    if let Some(kind) = gateway_path(path) {
        return Some(format!(
            "starts with a bot-token {kind}, which tgin reserves for its Bot API gateway"
        ));
    }
    RESERVED
        .iter()
        .find(|reserved| {
            path.strip_prefix(**reserved)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
        .map(|reserved| format!("is `{reserved}` or below it, which tgin reserves"))
}

/// Why `path`, joined to a base URL (as `setWebhook` registers it, and as
/// clients request it), would not reach the router as written: the router
/// matches the request path byte for byte.
fn url_problem(path: &str) -> Option<&'static str> {
    if path.contains('?') {
        return Some("contains `?`; a URL would carry what follows as its query");
    }
    if path.contains('#') {
        return Some("contains `#`; a URL would drop what follows as its fragment");
    }
    if path.bytes().any(|b| b.is_ascii_control()) {
        return Some("contains a control character, which a URL encodes");
    }
    if path.contains(' ') {
        return Some("contains whitespace, which a URL encodes; write it percent-encoded");
    }
    if !path.is_ascii() {
        return Some(
            "contains a non-ASCII character, which a URL encodes; write it percent-encoded",
        );
    }
    let joined = Url::parse(&format!("http://tgin.invalid{path}"));
    if !joined.is_ok_and(|url| url.path() == path) {
        return Some(
            "changes when it is part of a URL (`.` or `..` segments, `\\`, or characters a URL encodes); write it as the path is sent",
        );
    }
    None
}

/// What the Bot API gateway would serve at `path`, if anything: a first
/// segment `bot<id>:<secret>` (a method path) or `/file/bot<id>:` (a file
/// path). The gateway serves only token-shaped segments, so other `/bot…`
/// paths, such as `/bot1/getUpdates`, stay free.
fn gateway_path(path: &str) -> Option<&'static str> {
    let rest = path.strip_prefix('/')?;
    let (rest, kind) = match rest.strip_prefix("file/") {
        Some(file) => (file, "file path"),
        None => (rest, "path segment"),
    };
    let ids = rest.strip_prefix("bot")?;
    let digits = ids.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0 && ids.as_bytes().get(digits) == Some(&b':')).then_some(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_rules() {
        for ok in [
            "/",
            "/hook",
            "/webhook/shop",
            "/a:b",
            "/vone/x",
            "/v1x",
            // Paths such as `/bot1/getUpdates`; the gateway serves only
            // token-shaped `bot<id>:<secret>` segments.
            "/bot1/getUpdates",
            "/bot",
            "/bot/x",
            "/botx",
            "/bot123",
            "/botx123:abc",
            "/a/bot123:abc",
            "/file/x",
            "/file/bot123",
            "/files/bot123:abc",
            // Reservations cover whole segments only.
            "/metricsbot/hook",
            "/livezone/getUpdates",
            "/readyzz",
            "/cx/y",
            "/mtprotocol",
            // What a URL carries unchanged.
            "/hook%20x",
            "/a|b",
            "/a;b=c,d",
            "/~user/x.y",
            "/a/.b",
        ] {
            assert_eq!(path_problem(ok), None, "{ok}");
        }
        for bad in [
            "hook",
            "/a/{id}",
            "/a}",
            "/a/:id",
            "/a/*rest",
            "/c/x",
            "/bot123:abc",
            "/bot123:abc/getUpdates",
            "/bot1:",
            "/file/bot123:abc/x",
            "/file/bot1:",
            "/mtproto/x",
            "/v1/x",
            "/v1",
            "/livez",
            "/readyz/x",
            "/metrics",
            "/c",
            "/livez/",
            "/metrics/x",
            "/mtproto",
            "/hook?bot=shop",
            "/hook#frag",
            "/hook x",
            "/hook\tx",
            "/hook\u{7f}",
            "/hook/\u{e9}",
            "/a/../b",
            "/a/./b",
            "/a/..",
            "/a\\b",
            "/a\"b",
            "/a<b>",
            "/a`b",
        ] {
            assert!(path_problem(bad).is_some(), "{bad} accepted");
        }
    }

    /// A token-shaped segment may hold a real token, so the message never
    /// quotes it.
    #[test]
    fn gateway_path_problem_never_quotes_the_token() {
        for path in ["/bot123:abc/getUpdates", "/file/bot123:abc/x"] {
            let problem = path_problem(path).expect("reserved");
            assert!(problem.contains("which tgin reserves"), "{problem}");
            assert!(!problem.contains("123:abc"), "{problem}");
            assert!(!problem.contains("123"), "{problem}");
        }
    }
}
