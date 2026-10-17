//! Loading, validating and resolving `tgin.yaml` version 1.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tgin_core::config::{
    self, Balance, Config, ConsumerKind, Gateway, Ingress, IpAllowlist, LogFormat, LogLevel, Pull,
    Push, Rate, RateLimit, RateMode, WebhookReply, WebhookSecret,
};
use tgin_core::consumer::push::LEASE_TIMEOUT;
use tgin_core::ids::ConsumerId;
use tgin_core::rules::{SplitStrategy, TargetSpec, TextMatch};
use tgin_runtime::config::{ConfigError, Located, MapEnv, Severity, parse_yaml};
use tgin_runtime::queue::{OnFull, Ordering, QueueConfig};

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn load(yaml: &str) -> Result<Config, Vec<ConfigError>> {
    config::load(yaml, Path::new("tgin.yaml"), &MapEnv::default())
}

fn loads(yaml: &str) -> Config {
    load(yaml).unwrap_or_else(|errors| panic!("{}", render(&errors)))
}

fn errors(yaml: &str) -> Vec<ConfigError> {
    match load(yaml) {
        Ok(_) => panic!("the config loaded:\n{yaml}"),
        Err(errors) => errors,
    }
}

fn single_error(yaml: &str) -> ConfigError {
    let errors = errors(yaml);
    assert_eq!(errors.len(), 1, "{}", render(&errors));
    errors.into_iter().next().unwrap()
}

fn render(errors: &[ConfigError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The 1-based line and column (in characters) of the first `needle` in
/// `text`.
fn pos(text: &str, needle: &str) -> (u32, u32) {
    let offset = text
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` is not in the text"));
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let column = before[line_start..].chars().count() + 1;
    (line as u32, column as u32)
}

#[track_caller]
fn assert_at(err: &ConfigError, path: &str, (line, column): (u32, u32)) {
    assert_eq!(
        err.at,
        Some(Located {
            path: path.to_owned(),
            line,
            column,
        }),
        "{err}"
    );
}

#[track_caller]
fn assert_message(err: &ConfigError, needle: &str) {
    assert!(err.message.contains(needle), "`{needle}` not in: {err}");
}

fn pull(cfg: &Config, name: &str) -> Pull {
    match &cfg.consumers.iter().find(|c| c.name == name).unwrap().kind {
        ConsumerKind::Pull(pull) => pull.clone(),
        other => panic!("{name} is {other:?}"),
    }
}

fn push(cfg: &Config, name: &str) -> Push {
    match &cfg.consumers.iter().find(|c| c.name == name).unwrap().kind {
        ConsumerKind::Push(push) => push.clone(),
        other => panic!("{name} is {other:?}"),
    }
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}

#[test]
fn example_full_config_loads() {
    let mut env = MapEnv::default();
    env.vars
        .insert("SHOP_BOT_TOKEN".into(), "123456:shop-token-value".into());
    env.vars
        .insert("ADMIN_HOOK_SECRET".into(), "admin-hook-secret".into());
    // The fixture's absolute `${file:…}` path, as interpolation resolves it
    // against the origin's (relative) directory.
    env.files.insert(
        PathBuf::from("/run/secrets/tgin-admin-token"),
        "admin-api-token\n".into(),
    );
    let cfg = config::load(&fixture("full.yaml"), Path::new("fixtures/full.yaml"), &env)
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));

    assert!(cfg.warnings.is_empty(), "{}", render(&cfg.warnings));
    assert_eq!(cfg.runtime.worker_threads, None);
    assert_eq!(cfg.runtime.shutdown_grace, secs(20));
    assert_eq!(cfg.log.format, LogFormat::Text);
    let server = cfg.server.as_ref().unwrap();
    assert_eq!(
        server.listen,
        ["0.0.0.0:8080".parse::<SocketAddr>().unwrap()]
    );
    let tls = server.tls.as_ref().unwrap();
    assert_eq!(tls.cert_file.expose(), Path::new("/tls/tls.crt"));
    assert_eq!(tls.key_file.expose(), Path::new("/tls/tls.key"));
    assert_eq!(server.trusted_proxies, ["10.0.0.0/8".parse().unwrap()]);
    assert_eq!(
        cfg.admin.token.as_ref().map(|t| t.expose().as_str()),
        Some("admin-api-token")
    );

    let names: Vec<_> = cfg.consumers.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["workers", "analytics", "admin", "pay-a", "pay-b"]);
    for (i, consumer) in cfg.consumers.iter().enumerate() {
        assert_eq!(consumer.id, ConsumerId(i as u16));
    }
    let workers = pull(&cfg, "workers");
    assert_eq!(workers.queue.ordering, Ordering::PerKey);
    assert_eq!(workers.queue.lease_timeout, secs(120));
    assert_eq!(push(&cfg, "analytics").on_full, OnFull::DropOldest);
    let admin = push(&cfg, "admin");
    assert_eq!(
        admin.endpoints[0].url.expose().as_str(),
        "http://admin:8080/hook"
    );
    assert_eq!(
        admin.endpoints[0]
            .secret_token
            .as_ref()
            .map(|t| t.expose().as_str()),
        Some("admin-hook-secret")
    );
    assert_eq!(admin.ordering, Ordering::PerKey);

    let [shop] = cfg.bots.as_slice() else {
        panic!("{:?}", cfg.bots);
    };
    assert_eq!(shop.name, "shop");
    assert_eq!(shop.token.expose(), "123456:shop-token-value");
    assert_eq!(shop.api_url.expose().as_str(), "https://api.telegram.org/");
    let Some(Ingress::LongPoll(poll)) = &shop.ingress else {
        panic!("{:?}", shop.ingress);
    };
    assert_eq!(poll.timeout, secs(30));
    assert_eq!(poll.limit, 100);
    assert_eq!(
        poll.allowed_updates.as_deref(),
        Some(&["message".to_owned(), "callback_query".to_owned()][..])
    );
    assert_eq!(shop.rules.len(), 3);
    assert_eq!(shop.rules[0].predicates.commands, ["admin"]);
    assert_eq!(shop.rules[0].targets, [TargetSpec::Consumer(ConsumerId(2))]);
    assert_eq!(
        shop.rules[1].predicates.callback_data,
        Some(TextMatch::Prefix(vec!["pay:".into()]))
    );
    assert_eq!(
        shop.rules[1].targets,
        [TargetSpec::Split {
            strategy: SplitStrategy::HashChat,
            among: vec![
                TargetSpec::Consumer(ConsumerId(3)),
                TargetSpec::Consumer(ConsumerId(4))
            ],
            weights: vec![],
        }]
    );
    assert_eq!(
        shop.rules[2].targets,
        [
            TargetSpec::Consumer(ConsumerId(0)),
            TargetSpec::Consumer(ConsumerId(1))
        ]
    );
}

#[test]
fn defaults_applied() {
    let cfg = loads(&fixture("minimal.yaml"));

    assert_eq!(cfg.runtime.worker_threads, None);
    assert_eq!(cfg.runtime.lame_duck, secs(5));
    assert_eq!(cfg.runtime.shutdown_grace, secs(15));
    assert_eq!(cfg.log.level, LogLevel::Info);
    assert_eq!(cfg.log.format, LogFormat::Text);

    let server = cfg.server.as_ref().unwrap();
    assert_eq!(
        server.listen,
        ["0.0.0.0:8080".parse::<SocketAddr>().unwrap()]
    );
    assert_eq!(server.tls, None);
    assert!(server.trusted_proxies.is_empty());
    assert_eq!(server.max_connections, 10_000);
    assert_eq!(server.header_read_timeout, secs(10));

    assert_eq!(
        cfg.admin.listen,
        ["127.0.0.1:9090".parse::<SocketAddr>().unwrap()]
    );
    assert!(cfg.admin.token.is_none());
    assert!(!cfg.admin.public_metrics);

    assert!(cfg.reload.watch);
    assert_eq!(cfg.reload.debounce, Duration::from_millis(500));
    assert_eq!(cfg.reload.drain_timeout, secs(30));

    let poller = &cfg.bots[0];
    assert_eq!(
        poller.api_url.expose().as_str(),
        "https://api.telegram.org/"
    );
    let Some(Ingress::LongPoll(poll)) = &poller.ingress else {
        panic!("{:?}", poller.ingress);
    };
    assert_eq!(poll.timeout, secs(30));
    assert_eq!(poll.limit, 100);
    assert_eq!(poll.allowed_updates, None);
    assert!(poll.delete_webhook_on_start);

    let Some(Ingress::Webhook(hook)) = &cfg.bots[1].ingress else {
        panic!("{:?}", cfg.bots[1].ingress);
    };
    assert_eq!(hook.path.expose(), "/webhook/hooked");
    assert_eq!(hook.public_url, None);
    assert!(!hook.register);
    assert_eq!(hook.secret_token, WebhookSecret::Auto);
    assert_eq!(hook.allowed_updates, None);
    assert_eq!(hook.max_connections, None);
    assert!(!hook.drop_pending_updates);
    assert_eq!(hook.ip_address, None);
    assert_eq!(hook.certificate_file, None);
    assert_eq!(hook.ip_allowlist, IpAllowlist::None);

    let queue = pull(&cfg, "queue");
    assert_eq!(queue.queue.capacity, 10_000);
    assert_eq!(queue.queue.capacity_bytes, 64 << 20);
    assert_eq!(queue.queue.on_full, OnFull::Backpressure);
    assert_eq!(queue.queue.lease_timeout, secs(120));
    assert_eq!(queue.queue.ordering, Ordering::None);
    assert!(queue.auth_token.is_none());
    assert!(queue.aliases.is_empty());
    assert!(!queue.honor_drop_pending);

    let pusher = push(&cfg, "pusher");
    assert_eq!(pusher.endpoints[0].timeout, secs(10));
    assert!(pusher.endpoints[0].secret_token.is_none());
    assert_eq!(pusher.balance, Balance::RoundRobin);
    assert_eq!(pusher.max_in_flight, 32);
    assert_eq!(pusher.ordering, Ordering::None);
    assert_eq!(pusher.capacity, 10_000);
    assert_eq!(pusher.capacity_bytes, 64 << 20);
    assert_eq!(pusher.on_full, OnFull::Backpressure);
    assert_eq!(pusher.retry.initial, secs(1));
    assert_eq!(pusher.retry.max, secs(60));
    assert_eq!(pusher.retry.max_attempts, 10);
    assert_eq!(pusher.retry.max_age, secs(3600));
    assert_eq!(pusher.breaker.failure_threshold, 5);
    assert_eq!(pusher.breaker.open_for, secs(30));
    assert!(!pusher.accept_set_webhook);
    assert_eq!(pusher.webhook_reply, WebhookReply::Execute);
    assert!(!pusher.honor_drop_pending);
    assert_eq!(
        pusher.queue(),
        QueueConfig {
            capacity: 10_000,
            capacity_bytes: 64 << 20,
            lease_timeout: LEASE_TIMEOUT,
            ordering: Ordering::None,
            on_full: OnFull::Backpressure,
        }
    );

    // Without a `server` block there is no data plane.
    let cfg = loads("version: 1\n");
    assert!(cfg.server.is_none());
    assert!(cfg.bots.is_empty() && cfg.consumers.is_empty());
}

/// The gateway defaults of the plan's table, with method patterns
/// lowercased.
fn default_gateway() -> Gateway {
    Gateway {
        upstream_idle_timeout: secs(60),
        rate_limit: RateLimit {
            mode: RateMode::Shape,
            max_delay: secs(5),
            methods: vec![
                "send*".to_owned(),
                "copymessage*".to_owned(),
                "forwardmessage*".to_owned(),
            ],
            global: Rate {
                count: nz(30),
                per: secs(1),
                burst: nz(30),
            },
            per_chat: Rate {
                count: nz(1),
                per: secs(1),
                burst: nz(3),
            },
            per_group: Rate {
                count: nz(20),
                per: secs(60),
                burst: nz(20),
            },
        },
        retry_429_max_wait: secs(10),
    }
}

#[test]
fn gateway_defaults_exact() {
    let cfg = loads(&fixture("minimal.yaml"));
    for bot in &cfg.bots {
        assert_eq!(bot.gateway, default_gateway(), "{}", bot.name);
    }
    // Empty blocks are the same as omitted ones.
    let cfg = loads(
        "version: 1\nbots:\n  - name: a\n    token: \"1:a\"\n    gateway: { rate_limit: { global: {}, per_chat: {}, per_group: {} }, retry_429: {} }\n",
    );
    assert_eq!(cfg.bots[0].gateway, default_gateway());
}

/// A rate block's omitted fields take that block's defaults, not another
/// block's.
#[test]
fn rate_block_fields_default_individually() {
    let cfg = loads(
        r#"version: 1
bots:
  - name: a
    token: "1:a"
    gateway:
      upstream_idle_timeout: 2m
      rate_limit:
        mode: reject
        global: { per: 2s }
        per_chat: { burst: 5 }
        per_group: { count: 10 }
  - name: b
    token: "2:b"
    gateway:
      rate_limit: { max_delay: 0s, per_chat: { count: 2, per: 3s } }
      retry_429: { max_wait: 0s }
"#,
    );
    let mut a = default_gateway();
    a.upstream_idle_timeout = secs(120);
    a.rate_limit.mode = RateMode::Reject;
    a.rate_limit.global.per = secs(2);
    a.rate_limit.per_chat.burst = nz(5);
    a.rate_limit.per_group.count = nz(10);
    assert_eq!(cfg.bots[0].gateway, a);

    // Zero waits are meaningful: shaping behaves like `reject`, and the
    // transparent 429 retry is off.
    let mut b = default_gateway();
    b.rate_limit.max_delay = Duration::ZERO;
    b.rate_limit.per_chat.count = nz(2);
    b.rate_limit.per_chat.per = secs(3);
    b.retry_429_max_wait = Duration::ZERO;
    assert_eq!(cfg.bots[1].gateway, b);
}

#[test]
fn rate_limit_validation_errors() {
    let yaml = r#"version: 1
bots:
  - name: a
    token: "1:a"
    gateway:
      upstream_idle_timeout: 999ms
      rate_limit:
        max_delay: 61s
        methods: [send*, "get-me", "*", "send**", ""]
        global: { count: 0 }
        per_chat: { burst: 0 }
        per_group: { per: 0s }
      retry_429: { max_wait: 2m }
  - name: b
    token: "2:b"
    gateway: { rate_limit: { methods: [] } }
  - name: c
    token: "3:c"
    gateway: { rate_limit: { per_chat: { per: 1M } } }
"#;
    let errors = errors(yaml);
    let gw = "bots[0].gateway";
    let rl = "bots[0].gateway.rate_limit";
    // (path, text before the value, the value, message)
    let expected = [
        (
            format!("{gw}.upstream_idle_timeout"),
            "upstream_idle_timeout: ",
            "999ms",
            "shorter than the minimum of 1s",
        ),
        (
            format!("{rl}.max_delay"),
            "max_delay: ",
            "61s",
            "longer than the maximum of 1m",
        ),
        (
            format!("{rl}.methods[1]"),
            "send*, ",
            "\"get-me\"",
            "invalid method pattern `get-me`",
        ),
        (
            format!("{rl}.methods[2]"),
            "\"get-me\", ",
            "\"*\"",
            "invalid method pattern `*`",
        ),
        (
            format!("{rl}.methods[3]"),
            "\"*\", ",
            "\"send**\"",
            "invalid method pattern `send**`",
        ),
        (
            format!("{rl}.methods[4]"),
            "\"send**\", ",
            "\"\"",
            "invalid method pattern ``",
        ),
        (
            format!("{rl}.global.count"),
            "count: ",
            "0",
            "must be at least 1",
        ),
        (
            format!("{rl}.per_chat.burst"),
            "burst: ",
            "0",
            "must be at least 1",
        ),
        (
            format!("{rl}.per_group.per"),
            "per: ",
            "0s",
            "must be longer than 0",
        ),
        (
            format!("{gw}.retry_429.max_wait"),
            "max_wait: ",
            "2m",
            "longer than the maximum of 1m",
        ),
        (
            "bots[1].gateway.rate_limit.methods".to_owned(),
            "methods: ",
            "[]",
            "at least one method",
        ),
        (
            "bots[2].gateway.rate_limit.per_chat.per".to_owned(),
            "per: ",
            "1M",
            "a month or year unit",
        ),
    ];
    assert_eq!(errors.len(), expected.len(), "{}", render(&errors));
    for (err, (path, before, value, message)) in errors.iter().zip(&expected) {
        let (line, column) = pos(yaml, &format!("{before}{value}"));
        let column = column + u32::try_from(before.chars().count()).unwrap();
        assert_at(err, path, (line, column));
        assert_message(err, message);
    }

    // The limits themselves are allowed, and `mode: off` needs no methods.
    let cfg = loads(
        "version: 1\nbots:\n  - name: a\n    token: \"1:a\"\n    gateway: { upstream_idle_timeout: 1s, rate_limit: { mode: off, max_delay: 60s, methods: [] }, retry_429: { max_wait: 1m } }\n",
    );
    let gateway = &cfg.bots[0].gateway;
    assert_eq!(gateway.upstream_idle_timeout, secs(1));
    assert_eq!(gateway.rate_limit.mode, RateMode::Off);
    assert_eq!(gateway.rate_limit.max_delay, secs(60));
    assert!(gateway.rate_limit.methods.is_empty());
    assert_eq!(gateway.retry_429_max_wait, secs(60));
}

/// `1M` (a likely typo for `1m`) is reported once, as a month unit, not
/// also as over the cap.
#[test]
fn capped_duration_with_calendar_unit_is_one_error() {
    for (field, value) in [
        ("rate_limit: { max_delay", "1M"),
        ("retry_429: { max_wait", "1y"),
    ] {
        let yaml = format!(
            "version: 1\nbots:\n  - name: a\n    token: \"1:a\"\n    gateway: {{ {field}: {value} }} }}\n"
        );
        let err = single_error(&yaml);
        assert_message(&err, "a month or year unit");
    }
}

#[test]
fn methods_are_lowercased() {
    let cfg = loads(
        "version: 1\nbots:\n  - name: a\n    token: \"1:a\"\n    gateway: { rate_limit: { methods: [SendMessage, \"copyMessage*\", SENDPHOTO] } }\n",
    );
    assert_eq!(
        cfg.bots[0].gateway.rate_limit.methods,
        ["sendmessage", "copymessage*", "sendphoto"]
    );
}

#[test]
fn push_without_endpoints_needs_accept_set_webhook() {
    let yaml = "version: 1\nconsumers:\n  p: { push: {} }\n";
    let err = single_error(yaml);
    assert_at(&err, "consumers.p.push.endpoints", pos(yaml, "{ push"));
    assert_message(&err, "at least one endpoint");
    assert!(
        err.hint
            .as_deref()
            .is_some_and(|h| h.contains("accept_set_webhook: true")),
        "{err}"
    );

    let yaml = "version: 1\nconsumers:\n  p: { push: { endpoints: [] } }\n";
    let err = single_error(yaml);
    assert_at(&err, "consumers.p.push.endpoints", pos(yaml, "[]"));
    assert_message(&err, "at least one endpoint");

    let cfg = loads(
        "version: 1\nbots:\n  - name: a\n    token: \"1:a\"\n    routes: [{ to: [p] }]\nconsumers:\n  p: { push: { accept_set_webhook: true } }\n",
    );
    let p = push(&cfg, "p");
    assert!(p.endpoints.is_empty());
    assert!(p.accept_set_webhook);
    // Configured endpoints and setWebhook can be combined.
    let cfg = loads(
        "version: 1\nconsumers:\n  p: { push: { endpoints: [{ url: \"http://p/\" }], accept_set_webhook: true } }\n",
    );
    assert_eq!(push(&cfg, "p").endpoints.len(), 1);
}

/// A token holder must never redirect another bot's updates with
/// `setWebhook`.
#[test]
fn accept_set_webhook_needs_one_bot() {
    let yaml = r#"version: 1
bots:
  - name: first
    token: "1:a"
    routes: [{ to: [h, h] }]
  - name: second
    token: "2:b"
    routes: [{ to: [{ all: [h] }] }]
consumers:
  h: { push: { accept_set_webhook: true, endpoints: [{ url: "http://h/" }] } }
"#;
    let err = single_error(yaml);
    let (line, column) = pos(yaml, "all: [h");
    assert_at(&err, "bots[1].routes[0].to[0].all[0]", (line, column + 6));
    assert_message(&err, "push consumer `h`");
    assert_message(&err, "bots `first` and `second`");
    assert_message(&err, "accept_set_webhook");

    // Without setWebhook capture, a push consumer serves any number of bots.
    let cfg = loads(&yaml.replace("accept_set_webhook: true", "accept_set_webhook: false"));
    assert!(!push(&cfg, "h").accept_set_webhook);
}

/// The gateway serves only tokens shaped `<digits>:<secret>`; other tokens
/// still load, with a warning that never quotes them.
#[test]
fn token_not_gateway_shaped_warns() {
    let long = format!("\"1:{}\"", "s".repeat(129));
    for token in [
        "plainhush",
        "\"123:\"",
        "\":hush\"",
        "\"12a:hush\"",
        "\"123:hush:more\"",
        long.as_str(),
    ] {
        let yaml = format!("version: 1\nbots:\n  - name: a\n    token: {token}\n");
        let cfg = loads(&yaml);
        let [warning] = cfg.warnings.as_slice() else {
            panic!("{token}: {}", render(&cfg.warnings));
        };
        assert_eq!(warning.severity, Severity::Warning);
        assert_at(warning, "bots[0].token", pos(&yaml, token));
        assert_message(warning, "Bot API gateway");
        for part in ["hush", "plain", "123", "sss"] {
            assert!(!warning.to_string().contains(part), "{warning}");
        }
    }
    let longest = format!("\"1:{}\"", "s".repeat(128));
    for token in ["\"123456:AA-bb_CC\"", longest.as_str()] {
        let cfg = loads(&format!(
            "version: 1\nbots:\n  - name: a\n    token: {token}\n"
        ));
        assert!(cfg.warnings.is_empty(), "{}", render(&cfg.warnings));
    }
    // The one rule the gateway path parser applies too.
    assert!(config::gateway_token("123456:AA-bb_CC"));
    for bad in [
        "123456:a%3Ab",
        "123456:",
        ":abc",
        "1x:abc",
        "123:a:b",
        "123",
    ] {
        assert!(!config::gateway_token(bad), "{bad}");
    }
}

/// The design's example config, with every gateway, setWebhook-capture and
/// webhook-reply field written out, loads without a warning.
#[test]
fn design_example_loads() {
    let mut env = MapEnv::default();
    env.vars
        .insert("SHOP_BOT_TOKEN".into(), "123456:shop-token-value".into());
    env.vars
        .insert("ADMIN_HOOK_SECRET".into(), "admin-hook-secret".into());
    env.files.insert(
        PathBuf::from("/run/secrets/tgin-admin-token"),
        "admin-api-token\n".into(),
    );
    let cfg = config::load(
        &fixture("design.yaml"),
        Path::new("fixtures/design.yaml"),
        &env,
    )
    .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    assert!(cfg.warnings.is_empty(), "{}", render(&cfg.warnings));

    let [shop] = cfg.bots.as_slice() else {
        panic!("{:?}", cfg.bots);
    };
    let mut gateway = default_gateway();
    gateway.upstream_idle_timeout = secs(90);
    gateway
        .rate_limit
        .methods
        .push("editmessagetext".to_owned());
    gateway.rate_limit.per_chat.burst = nz(5);
    gateway.retry_429_max_wait = secs(20);
    assert_eq!(shop.gateway, gateway);

    let hooks = push(&cfg, "hooks");
    assert!(hooks.endpoints.is_empty());
    assert!(hooks.accept_set_webhook);
    assert_eq!(hooks.webhook_reply, WebhookReply::Ignore);
    assert!(hooks.honor_drop_pending);
    assert!(pull(&cfg, "workers").honor_drop_pending);
    assert!(!push(&cfg, "analytics").accept_set_webhook);
}

#[test]
fn sizes_and_durations_parse() {
    let cfg = loads(
        r#"
version: 1
server: {}
runtime: { worker_threads: 4, lame_duck: 1h 30m }
consumers:
  q: { pull: { capacity_bytes: 1536KiB, lease_timeout: 2d } }
  p:
    push:
      capacity_bytes: 1048576
      endpoints: [{ url: "https://p/", timeout: 250ms }]
      retry: { max_age: 1w }
"#,
    );
    assert_eq!(cfg.runtime.worker_threads.map(|n| n.get()), Some(4));
    assert_eq!(cfg.runtime.lame_duck, secs(90 * 60));
    let q = pull(&cfg, "q");
    assert_eq!(q.queue.capacity_bytes, 1536 << 10);
    assert_eq!(q.queue.lease_timeout, secs(2 * 86_400));
    let p = push(&cfg, "p");
    assert_eq!(p.capacity_bytes, 1 << 20);
    assert_eq!(p.endpoints[0].timeout, Duration::from_millis(250));
    assert_eq!(p.retry.max_age, secs(7 * 86_400));

    let yaml = "version: 1\nserver: {}\nconsumers:\n  q: { pull: { capacity_bytes: 64MB } }\n";
    let err = single_error(yaml);
    assert_at(&err, "consumers.q.pull.capacity_bytes", pos(yaml, "64MB"));
    assert_message(&err, "invalid size `64MB`");

    let yaml = "version: 1\nreload: { drain_timeout: 5 parsecs }\n";
    let err = single_error(yaml);
    assert_at(&err, "reload.drain_timeout", pos(yaml, "5 parsecs"));
    assert_message(&err, "invalid duration `5 parsecs`");

    let yaml = "version: 1\nruntime: { worker_threads: 0 }\n";
    let err = single_error(yaml);
    // serde-saphyr places a rejected integer at its key.
    assert_at(&err, "runtime.worker_threads", pos(yaml, "worker_threads"));
    assert_message(&err, "`auto` or a thread count of at least 1");
}

#[test]
fn month_unit_rejected() {
    let yaml = "version: 1\nserver: {}\nconsumers:\n  q: { pull: { lease_timeout: 5M } }\nreload: { drain_timeout: 1y }\n";
    let errors = errors(yaml);
    assert_eq!(errors.len(), 2, "{}", render(&errors));
    assert_at(
        &errors[0],
        "consumers.q.pull.lease_timeout",
        pos(yaml, "5M"),
    );
    assert_message(&errors[0], "`5M` uses `M`, a month or year unit");
    assert_at(&errors[1], "reload.drain_timeout", pos(yaml, "1y"));
    // Minutes are fine.
    loads("version: 1\nreload: { drain_timeout: 5m }\n");
}

#[test]
fn rule_version_is_1() {
    let yaml = "version: 2\n";
    let err = single_error(yaml);
    assert_at(&err, "version", pos(yaml, "2"));
    assert_message(&err, "unsupported config version 2");
}

#[test]
fn rule_names_valid() {
    let yaml = r#"
version: 1
server: {}
bots:
  - { name: Shop, token: t }
  - { name: -dash, token: t }
  - { name: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, token: t }
  - { name: ok-1, token: t }
consumers:
  Bad_Name: { pull: {} }
"#;
    let errors = errors(yaml);
    assert_eq!(errors.len(), 4, "{}", render(&errors));
    assert_at(&errors[0], "bots[0].name", pos(yaml, "Shop"));
    assert_at(&errors[1], "bots[1].name", pos(yaml, "-dash"));
    assert_at(&errors[2], "bots[2].name", pos(yaml, "aaaa"));
    // A consumer's errors point at its value.
    assert_at(&errors[3], "consumers.Bad_Name", pos(yaml, "{ pull"));
    assert_message(&errors[0], "invalid name `Shop`");
    // 63 characters is the limit.
    loads(&format!(
        "version: 1\nbots: [{{ name: {}, token: t }}]\n",
        "a".repeat(63)
    ));
}

#[test]
fn rule_names_unique() {
    let yaml = "version: 1\nbots:\n  - { name: a, token: t }\n  - { name: a, token: u }\n";
    let err = single_error(yaml);
    assert_at(&err, "bots[1].name", pos(yaml, "a, token: u"));
    assert_message(&err, "bot name `a` is already used by bots[0]");

    // Duplicate consumer keys are rejected by the parser.
    let yaml = "version: 1\nconsumers:\n  p: { push: { endpoints: [{ url: \"http://p/\" }] } }\n  p: { push: { endpoints: [{ url: \"http://q/\" }] } }\n";
    let err = single_error(yaml);
    assert_message(&err, "duplicate mapping key: p");
    assert_eq!(err.at.as_ref().map(|at| at.line), Some(4), "{err}");
}

#[test]
fn rule_at_most_one_ingress() {
    let yaml = r#"version: 1
server: {}
bots:
  - name: a
    token: t
    ingress:
      long_poll: {}
      webhook: {}
"#;
    let err = single_error(yaml);
    let (line, column) = pos(yaml, "webhook: {}");
    let at = err.at.as_ref().unwrap();
    assert_eq!((at.line, at.column), (line, column), "{err}");
    assert!(at.path.starts_with("bots[0].ingress"), "{err}");
}

#[test]
fn rule_targets_exist() {
    let yaml = r#"version: 1
bots:
  - name: a
    token: t
    routes:
      - to: [{ split: { strategy: round_robin, among: [p, nope] } }]
consumers:
  p: { push: { endpoints: [{ url: "http://p/" }] } }
"#;
    let err = single_error(yaml);
    assert_at(
        &err,
        "bots[0].routes[0].to[0].split.among[1]",
        pos(yaml, "nope"),
    );
    assert_message(&err, "unknown consumer `nope`");
    assert_eq!(err.hint.as_deref(), Some("defined consumers: p"));
}

#[test]
fn rule_pull_consumer_single_bot() {
    let yaml = r#"version: 1
server: {}
bots:
  - name: first
    token: t
    routes: [{ to: [q, q] }]
  - name: second
    token: t
    routes: [{ to: [{ all: [q] }] }]
consumers:
  q: { pull: {} }
"#;
    let err = single_error(yaml);
    let (line, column) = pos(yaml, "all: [q");
    assert_at(&err, "bots[1].routes[0].to[0].all[0]", (line, column + 6));
    assert_message(&err, "bots `first` and `second`");
}

#[test]
fn rule_split_weights_match_among() {
    let yaml = r#"version: 1
bots:
  - name: a
    token: t
    routes:
      - to: [{ split: { strategy: weighted, among: [p, p], weights: [1, 2, 3] } }]
      - to: [{ split: { strategy: weighted, among: [p] } }]
consumers:
  p: { push: { endpoints: [{ url: "http://p/" }] } }
"#;
    let errors = errors(yaml);
    assert_eq!(errors.len(), 2, "{}", render(&errors));
    assert_at(
        &errors[0],
        "bots[0].routes[0].to[0].split.weights",
        pos(yaml, "[1, 2, 3]"),
    );
    assert_message(&errors[0], "3 weights for 2 `among` entries");
    assert_at(
        &errors[1],
        "bots[0].routes[1].to[0].split.strategy",
        pos(yaml, "weighted, among: [p] }"),
    );
}

#[test]
fn rule_split_weights_only_weighted() {
    let yaml = r#"version: 1
bots:
  - name: a
    token: t
    routes:
      - to: [{ split: { strategy: hash_chat, among: [p], weights: [1] } }]
consumers:
  p: { push: { endpoints: [{ url: "http://p/" }] } }
"#;
    let err = single_error(yaml);
    assert_at(
        &err,
        "bots[0].routes[0].to[0].split.weights",
        pos(yaml, "[1]"),
    );
    assert_message(&err, "only allowed with `strategy: weighted`");
}

#[test]
fn rule_weights_positive() {
    let yaml = r#"version: 1
bots:
  - name: a
    token: t
    routes:
      - to: [{ split: { strategy: weighted, among: [p, p], weights: [3, 0] } }]
consumers:
  p: { push: { endpoints: [{ url: "http://p/" }] } }
"#;
    let err = single_error(yaml);
    assert_at(
        &err,
        "bots[0].routes[0].to[0].split.weights[1]",
        pos(yaml, "0]"),
    );
    assert_message(&err, "greater than 0");
}

#[test]
fn rule_paths_start_with_slash() {
    let yaml = "version: 1\nserver: {}\nconsumers:\n  q: { pull: { aliases: [relative] } }\n";
    let err = single_error(yaml);
    assert_at(&err, "consumers.q.pull.aliases[0]", pos(yaml, "relative"));
    assert_message(&err, "must start with `/`");
}

#[test]
fn rule_paths_literal() {
    let yaml = r#"version: 1
server: {}
bots:
  - name: a
    token: t
    ingress: { webhook: { path: "/hook/{id}" } }
consumers:
  q: { pull: { aliases: ["/x/:id", "/y/*rest"] } }
"#;
    let errors = errors(yaml);
    assert_eq!(errors.len(), 3, "{}", render(&errors));
    assert_at(
        &errors[0],
        "bots[0].ingress.webhook.path",
        pos(yaml, "\"/hook/{id}\""),
    );
    assert_at(
        &errors[1],
        "consumers.q.pull.aliases[0]",
        pos(yaml, "\"/x/"),
    );
    assert_at(
        &errors[2],
        "consumers.q.pull.aliases[1]",
        pos(yaml, "\"/y/"),
    );
}

#[test]
fn rule_paths_not_reserved() {
    for reserved in [
        "/c/x",
        "/bot123:abc/getUpdates",
        "/file/bot123:abc/x",
        "/mtproto/x",
        "/v1/x",
        "/livez",
        "/readyz",
        "/metrics",
    ] {
        let yaml = format!(
            "version: 1\nserver: {{}}\nconsumers:\n  q: {{ pull: {{ aliases: [\"{reserved}\"] }} }}\n"
        );
        let err = single_error(&yaml);
        assert_message(&err, "which tgin reserves");
        assert!(!err.to_string().contains("123:abc"), "{err}");
    }
}

/// Only token-shaped `bot<id>:<secret>` segments belong to the Bot API
/// gateway, so paths such as `/bot1/getUpdates` remain usable aliases.
#[test]
fn bot_paths_without_a_token_are_not_reserved() {
    let yaml = "version: 1\nserver: {}\nconsumers:\n  q: { pull: { aliases: [/bot1/getUpdates, /bot/x, /file/x] } }\n";
    let cfg = config::load(yaml, Path::new("tgin.yaml"), &MapEnv::default())
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    let ConsumerKind::Pull(pull) = &cfg.consumers[0].kind else {
        panic!("{:?}", cfg.consumers[0].kind);
    };
    let aliases: Vec<&str> = pull.aliases.iter().map(|a| a.expose().as_str()).collect();
    assert_eq!(aliases, ["/bot1/getUpdates", "/bot/x", "/file/x"]);
}

/// Reservations cover whole segments: the reserved path itself and what lies
/// below it, never a longer name that merely starts the same way.
#[test]
fn reserved_paths_match_whole_segments() {
    let yaml = "version: 1\nserver: {}\nbots:\n  - name: a\n    token: t\n    ingress: { webhook: { path: /metricsbot/hook } }\nconsumers:\n  q: { pull: { aliases: [/livezone/getUpdates, /readyzz, /v1x/y, /cx, /mtprotocol] } }\n";
    let cfg = loads(yaml);
    let q = pull(&cfg, "q");
    let aliases: Vec<&str> = q.aliases.iter().map(|a| a.expose().as_str()).collect();
    assert_eq!(
        aliases,
        [
            "/livezone/getUpdates",
            "/readyzz",
            "/v1x/y",
            "/cx",
            "/mtprotocol"
        ]
    );
    for reserved in ["/livez/x", "/metrics/", "/v1", "/c", "/mtproto"] {
        let yaml = format!(
            "version: 1\nserver: {{}}\nconsumers:\n  q: {{ pull: {{ aliases: [\"{reserved}\"] }} }}\n"
        );
        assert_message(&single_error(&yaml), "which tgin reserves");
    }
}

#[test]
fn rule_paths_unique() {
    let yaml = r#"version: 1
server: {}
bots:
  - name: a
    token: t
    ingress: { webhook: {} }
  - name: b
    token: t
    ingress: { webhook: { path: /hook } }
consumers:
  q: { pull: { aliases: [/webhook/a, /hook] } }
"#;
    let errors = errors(yaml);
    assert_eq!(errors.len(), 2, "{}", render(&errors));
    assert_at(
        &errors[0],
        "bots[0].ingress.webhook.path",
        pos(yaml, "{ webhook: {} }"),
    );
    assert_message(
        &errors[0],
        "the path is already used by consumers.q.pull.aliases[0]",
    );
    assert_at(
        &errors[1],
        "bots[1].ingress.webhook.path",
        pos(yaml, "/hook }"),
    );
}

#[test]
fn rule_webhook_needs_server() {
    let yaml = "version: 1\nbots:\n  - name: a\n    token: t\n    ingress: { webhook: {} }\n";
    let err = single_error(yaml);
    assert_at(&err, "bots[0].ingress.webhook", pos(yaml, "{ webhook"));
    assert_message(&err, "needs a `server` block");
}

#[test]
fn rule_register_needs_https_public_url() {
    let yaml = "version: 1\nserver: {}\nbots:\n  - name: a\n    token: t\n    ingress: { webhook: { register: true } }\n";
    let err = single_error(yaml);
    assert_at(&err, "bots[0].ingress.webhook.register", pos(yaml, "true"));
    assert_message(&err, "needs `public_url`");

    let yaml = "version: 1\nserver: {}\nbots:\n  - name: a\n    token: t\n    ingress: { webhook: { register: true, public_url: \"http://bot.example.com/webhook/a\" } }\n";
    let err = single_error(yaml);
    assert_at(
        &err,
        "bots[0].ingress.webhook.public_url",
        pos(yaml, "\"http:"),
    );
    assert_message(&err, "must be https");
}

#[test]
fn rule_pull_consumer_needs_server() {
    let yaml = "version: 1\nconsumers:\n  q: { pull: {} }\n";
    let err = single_error(yaml);
    assert_at(&err, "consumers.q.pull", pos(yaml, "{ pull"));
    assert_message(&err, "needs a `server` block");
}

#[test]
fn rule_admin_non_loopback_needs_token() {
    let yaml = "version: 1\nadmin: { listen: [\"127.0.0.1:9090\", \"0.0.0.0:9091\"] }\n";
    let err = single_error(yaml);
    assert_at(&err, "admin.listen[1]", pos(yaml, "\"0.0.0.0"));
    assert_message(&err, "`admin.token` is required");

    loads("version: 1\nadmin: { listen: [\"[::1]:9090\"] }\n");
    loads("version: 1\nadmin: { listen: [\"0.0.0.0:9090\"], token: admin-secret }\n");
}

#[test]
fn rule_certificate_file_requires_register() {
    let yaml = "version: 1\nserver: {}\nbots:\n  - name: a\n    token: t\n    ingress: { webhook: { certificate_file: /tls/self.pem } }\n";
    let err = single_error(yaml);
    assert_at(
        &err,
        "bots[0].ingress.webhook.certificate_file",
        pos(yaml, "/tls/self.pem"),
    );
    assert_message(&err, "needs `register: true`");
}

#[test]
fn rule_last_rule_match_warns() {
    let yaml = r#"version: 1
bots:
  - name: a
    token: "1:t"
    routes:
      - to: [p]
      - match: { update_types: [message] }
        to: [p]
consumers:
  p: { push: { endpoints: [{ url: "http://p/" }] } }
"#;
    let cfg = loads(yaml);
    let [warning] = cfg.warnings.as_slice() else {
        panic!("{}", render(&cfg.warnings));
    };
    assert_eq!(warning.severity, Severity::Warning);
    assert_at(warning, "bots[0].routes", pos(yaml, "match:"));
    assert!(
        warning
            .to_string()
            .ends_with("bots[0].routes (line 7, column 9): last rule has a match; unmatched updates are dropped"),
        "{warning}"
    );

    // An empty match matches everything.
    let cfg = loads(&yaml.replace("{ update_types: [message] }", "{}"));
    assert!(cfg.warnings.is_empty(), "{}", render(&cfg.warnings));
}

#[test]
fn rule_reload_debounce_minimum() {
    let yaml = "version: 1\nreload: { debounce: 9ms }\n";
    let err = single_error(yaml);
    assert_at(&err, "reload.debounce", pos(yaml, "9ms"));
    assert_message(&err, "minimum of 10ms");
    loads("version: 1\nreload: { debounce: 10ms }\n");
}

#[test]
fn rule_limits_in_range() {
    let yaml = r#"version: 1
server: { max_connections: 0 }
bots:
  - name: a
    token: t
    ingress: { long_poll: { limit: 0 } }
  - name: b
    token: t
    ingress: { webhook: { max_connections: 101 } }
consumers:
  q: { pull: { capacity: 0, capacity_bytes: 0 } }
  p:
    push:
      endpoints: []
      max_in_flight: 0
      retry: { max_attempts: 0 }
      breaker: { failure_threshold: 0 }
"#;
    let errors = errors(yaml);
    let paths: Vec<_> = errors
        .iter()
        .map(|e| e.at.as_ref().unwrap().path.as_str())
        .collect();
    assert_eq!(
        paths,
        [
            "server.max_connections",
            "bots[0].ingress.long_poll.limit",
            "bots[1].ingress.webhook.max_connections",
            "consumers.q.pull.capacity",
            "consumers.q.pull.capacity_bytes",
            "consumers.p.push.endpoints",
            "consumers.p.push.max_in_flight",
            "consumers.p.push.retry.max_attempts",
            "consumers.p.push.breaker.failure_threshold",
        ],
        "{}",
        render(&errors)
    );
}

#[test]
fn rule_capacity_bytes_at_least_one_mib() {
    let yaml = r#"version: 1
server: {}
consumers:
  q: { pull: { capacity_bytes: 1023KiB } }
  p:
    push:
      capacity_bytes: 1048575
      endpoints: [{ url: "http://p/" }]
"#;
    let errors = errors(yaml);
    assert_eq!(errors.len(), 2, "{}", render(&errors));
    for (err, (path, needle)) in errors.iter().zip([
        ("consumers.q.pull.capacity_bytes", "1023KiB"),
        ("consumers.p.push.capacity_bytes", "1048575"),
    ]) {
        assert_at(err, path, pos(yaml, needle));
        assert_message(err, "must be at least 1MiB");
        assert!(
            err.hint
                .as_deref()
                .is_some_and(|h| h.contains("updates can be up to 1 MiB")),
            "{err}"
        );
    }
    let cfg = loads(
        &yaml
            .replace("1023KiB", "1MiB")
            .replace("1048575", "1048576"),
    );
    assert_eq!(pull(&cfg, "q").queue.capacity_bytes, 1 << 20);
    assert_eq!(push(&cfg, "p").capacity_bytes, 1 << 20);
}

/// The schema's size branches accept what the loader accepts.
#[test]
fn byte_size_schema_matches_loader() {
    let schema: serde_json::Value = serde_json::from_str(&config::schema()).unwrap();
    let branches = schema["$defs"]["ByteSize"]["oneOf"].as_array().unwrap();
    assert_eq!(
        branches[0],
        serde_json::json!({ "type": "integer", "minimum": 1_048_576 })
    );
    let re = regex_automata::meta::Regex::new(branches[1]["pattern"].as_str().unwrap()).unwrap();
    let yaml = |size: &str| {
        format!(
            "version: 1\nserver: {{}}\nconsumers:\n  q: {{ pull: {{ capacity_bytes: \"{size}\" }} }}\n"
        )
    };
    for ok in [
        "1048576",
        "001048576",
        "1048579",
        "1048580",
        "1048599",
        "1048600",
        "1048999",
        "1049000",
        "1050000",
        "1100000",
        "2000000",
        "10000000",
        "1048576B",
        "1024KiB",
        "1025 KiB",
        "1030KiB",
        "1100KiB",
        "2000KiB",
        "10000KiB",
        "1MiB",
        " 64MiB ",
        "1GiB",
    ] {
        assert!(re.is_match(ok), "schema rejects {ok:?}");
        assert!(load(&yaml(ok)).is_ok(), "loader rejects {ok:?}");
    }
    for bad in [
        "1048575", "1048575B", "1048570", "1048500", "1047999", "1040000", "999999", "1023KiB",
        "1019KiB", "1000KiB", "999KiB", "0MiB", "0GiB", "", "64MB",
    ] {
        assert!(!re.is_match(bad), "schema accepts {bad:?}");
        assert!(load(&yaml(bad)).is_err(), "loader accepts {bad:?}");
    }
}

/// The fields that take a string or an object.
#[test]
fn string_or_object_fields_parse() {
    let yaml = r#"version: 1
server: { trusted_proxies: [10.0.0.0/8, 192.0.2.1, "2001:db8::/32"] }
runtime: { worker_threads: auto }
bots:
  - name: a
    token: t
    ingress: { webhook: { secret_token: none, ip_allowlist: telegram } }
    routes:
      - to: [p, { all: [p, { split: { strategy: round_robin, among: [p] } }] }]
  - name: b
    token: t
    ingress: { webhook: { secret_token: auto, ip_allowlist: [10.1.0.0/16, 192.0.2.7] } }
  - name: c
    token: t
    ingress: { webhook: { secret_token: my-secret, ip_allowlist: none } }
consumers:
  p: { push: { endpoints: [{ url: "http://p/" }] } }
"#;
    let cfg = loads(yaml);
    assert_eq!(cfg.runtime.worker_threads, None);
    assert_eq!(
        cfg.server.as_ref().unwrap().trusted_proxies,
        [
            "10.0.0.0/8".parse().unwrap(),
            "192.0.2.1/32".parse().unwrap(),
            "2001:db8::/32".parse().unwrap()
        ]
    );
    let hook = |i: usize| match &cfg.bots[i].ingress {
        Some(Ingress::Webhook(hook)) => hook.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(hook(0).secret_token, WebhookSecret::None);
    assert_eq!(hook(0).ip_allowlist, IpAllowlist::Telegram);
    assert_eq!(hook(1).secret_token, WebhookSecret::Auto);
    assert_eq!(
        hook(1).ip_allowlist,
        IpAllowlist::Networks(vec![
            "10.1.0.0/16".parse().unwrap(),
            "192.0.2.7/32".parse().unwrap()
        ])
    );
    let WebhookSecret::Token(secret) = hook(2).secret_token else {
        panic!("{:?}", hook(2).secret_token);
    };
    assert_eq!(secret.expose(), "my-secret");
    assert_eq!(hook(2).ip_allowlist, IpAllowlist::None);
    let p = TargetSpec::Consumer(ConsumerId(0));
    assert_eq!(
        cfg.bots[0].rules[0].targets,
        [
            p.clone(),
            TargetSpec::All(vec![
                p.clone(),
                TargetSpec::Split {
                    strategy: SplitStrategy::RoundRobin,
                    among: vec![p],
                    weights: vec![],
                }
            ])
        ]
    );

    let yaml = "version: 1\nserver: {}\nbots:\n  - name: a\n    token: t\n    ingress: { webhook: { ip_allowlist: [10.0.0.0/33] } }\n";
    let err = single_error(yaml);
    assert_at(
        &err,
        "bots[0].ingress.webhook.ip_allowlist[0]",
        pos(yaml, "10.0.0.0/33"),
    );
    assert_message(&err, "invalid network `10.0.0.0/33`");
}

/// Every error the rules engine finds is reported at its rule.
#[test]
fn rule_routes_compile() {
    let yaml = r#"version: 1
bots:
  - name: a
    token: t
    routes:
      - match: { text: { prefix: [] } }
        to: [p]
      - to: [{ all: [] }]
      - match: { text: { regex: ["("] } }
        to: [p]
      - to: [{ split: { strategy: weighted, among: [p, p], weights: [65536, 65537] } }]
      - to: []
      - to: [p]
consumers:
  p: { push: { endpoints: [{ url: "http://p/" }] } }
"#;
    let errors = errors(yaml);
    assert_eq!(errors.len(), 5, "{}", render(&errors));
    let expected = [
        ("match: { text: { prefix", "a text match has an empty list"),
        ("to: [{ all", "an `all` or `split` target has no children"),
        ("match: { text: { regex", "invalid regex \"(\""),
        ("to: [{ split", "period 131073 exceeds 65536"),
        ("to: []", "the rule has no targets"),
    ];
    for (j, (err, (needle, message))) in errors.iter().zip(expected).enumerate() {
        assert_at(err, &format!("bots[0].routes[{j}]"), pos(yaml, needle));
        assert_message(err, message);
    }
}

/// A rule with a bad target is still compiled, and each problem is
/// reported once.
#[test]
fn rule_with_bad_target_still_compiled() {
    let yaml = r#"version: 1
bots:
  - name: a
    token: t
    routes:
      - match: { text: { regex: ["("] }, commands: ["a@b"] }
        to: [nope, { split: { strategy: hash_chat, among: [p], weights: [1] } }, { all: [] }]
consumers:
  p: { push: { endpoints: [{ url: "http://p/" }] } }
"#;
    let errors = errors(yaml);
    let paths: Vec<_> = errors
        .iter()
        .map(|e| e.at.as_ref().unwrap().path.as_str())
        .collect();
    assert_eq!(
        paths,
        [
            "bots[0].routes[0]",
            "bots[0].routes[0].match.commands[0]",
            "bots[0].routes[0].to[0]",
            "bots[0].routes[0].to[1].split.weights",
        ],
        "{}",
        render(&errors)
    );
    // Compiling stops at the rule's first problem: here the regex.
    assert_message(&errors[0], "invalid regex \"(\"");
    assert_message(&errors[2], "unknown consumer `nope`");
}

#[test]
fn rule_commands_are_telegram_names() {
    let yaml = r#"version: 1
bots:
  - name: a
    token: t
    routes:
      - match: { commands: [start, /help_me, "/старт", "a@bot", "/", aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, A_1] }
        to: [p]
consumers:
  p: { push: { endpoints: [{ url: "http://p/" }] } }
"#;
    let errors = errors(yaml);
    assert_eq!(errors.len(), 4, "{}", render(&errors));
    for (err, (k, needle)) in errors.iter().zip([
        (2, "\"/старт\""),
        (3, "\"a@bot\""),
        (4, "\"/\""),
        (5, "aaaa"),
    ]) {
        assert_at(
            err,
            &format!("bots[0].routes[0].match.commands[{k}]"),
            pos(yaml, needle),
        );
        assert_message(err, "invalid command");
        assert!(
            err.hint
                .as_deref()
                .is_some_and(|h| h.contains("1 to 32 ASCII letters, digits or `_`")),
            "{err}"
        );
    }
    let cfg = loads(&yaml.replace(
        r#", "/старт", "a@bot", "/", aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"#,
        "",
    ));
    assert_eq!(
        cfg.bots[0].rules[0].predicates.commands,
        ["start", "/help_me", "A_1"]
    );
}

#[test]
fn rule_push_secret_token_is_header_safe() {
    let long = "a".repeat(257);
    let yaml = format!(
        r#"version: 1
consumers:
  p:
    push:
      endpoints:
        - {{ url: "http://p/", secret_token: "s3cr3t value" }}
        - {{ url: "http://p/", secret_token: "{long}" }}
        - {{ url: "http://p/", secret_token: "${{env:BAD_SECRET}}" }}
        - {{ url: "http://p/", secret_token: "ok-Secret_9" }}
"#
    );
    let errors = config::load(
        &yaml,
        Path::new("tgin.yaml"),
        &env(&[("BAD_SECRET", "x\ny")]),
    )
    .unwrap_err();
    assert_eq!(errors.len(), 3, "{}", render(&errors));
    for (err, (k, needle)) in
        errors
            .iter()
            .zip([(0, "\"s3cr3t"), (1, "\"aaaa"), (2, "\"${env:BAD")])
    {
        assert_at(
            err,
            &format!("consumers.p.push.endpoints[{k}].secret_token"),
            pos(&yaml, needle),
        );
        assert_message(err, "secret_token");
        assert!(
            err.hint
                .as_deref()
                .is_some_and(|h| h.contains("1 to 256 ASCII letters, digits, `_` or `-`")),
            "{err}"
        );
        for part in ["s3cr3t", "aaaa", "x\ny"] {
            assert!(!err.to_string().contains(part), "{err}");
        }
    }
    let max = "Z".repeat(256);
    let cfg = loads(&format!(
        "version: 1\nconsumers:\n  p: {{ push: {{ endpoints: [{{ url: \"http://p/\", secret_token: \"{max}\" }}, {{ url: \"http://p/\", secret_token: ok-Secret_9 }}] }} }}\n"
    ));
    let endpoints = push(&cfg, "p").endpoints;
    assert_eq!(endpoints[0].secret_token.as_ref().unwrap().expose(), &max);
    assert_eq!(
        endpoints[1].secret_token.as_ref().unwrap().expose(),
        "ok-Secret_9"
    );
}

#[test]
fn all_errors_collected() {
    let yaml = r#"version: 3
bots:
  - name: Shop
    token: t
    routes: [{ to: [missing] }]
"#;
    let errors = errors(yaml);
    assert_eq!(errors.len(), 3, "{}", render(&errors));
    assert_at(&errors[0], "version", (1, 10));
    assert_at(&errors[1], "bots[0].name", (3, 11));
    assert_at(&errors[2], "bots[0].routes[0].to[0]", pos(yaml, "missing"));
}

#[test]
fn secrets_resolve_from_env_and_file() {
    let yaml = r#"version: 1
server:
  tls: { cert_file: "${env:TLS_DIR}/tls.crt", key_file: "${file:key-path}" }
admin: { token: "${file:secrets/admin}" }
bots:
  - name: a
    token: "${env:BOT_TOKEN}"
    api_url: "${env:API}/"
    ingress: { webhook: { secret_token: "${env:HOOK_SECRET}" } }
consumers:
  q: { pull: { auth_token: "literal-$-token" } }
  p: { push: { endpoints: [{ url: "${env:PUSH_URL}", secret_token: "pre-${env:SUFFIX}" }] } }
"#;
    let mut env = MapEnv::default();
    for (k, v) in [
        ("TLS_DIR", "/tls"),
        ("BOT_TOKEN", "1:bot-token"),
        ("API", "http://bot-api:8081"),
        ("HOOK_SECRET", "hook-secret"),
        ("PUSH_URL", "https://p.example/tg"),
        ("SUFFIX", "fix"),
    ] {
        env.vars.insert(k.into(), v.into());
    }
    env.files.insert(
        Path::new("conf").join("secrets/admin"),
        "admin-token\n".into(),
    );
    env.files
        .insert(Path::new("conf").join("key-path"), "/tls/tls.key".into());
    let cfg = config::load(yaml, Path::new("conf/tgin.yaml"), &env)
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));

    assert_eq!(cfg.admin.token.as_ref().unwrap().expose(), "admin-token");
    let tls = cfg.server.as_ref().unwrap().tls.as_ref().unwrap();
    assert_eq!(tls.cert_file.expose(), Path::new("/tls/tls.crt"));
    assert_eq!(tls.key_file.expose(), Path::new("/tls/tls.key"));
    let bot = &cfg.bots[0];
    assert_eq!(bot.token.expose(), "1:bot-token");
    assert_eq!(bot.api_url.expose().as_str(), "http://bot-api:8081/");
    let Some(Ingress::Webhook(hook)) = &bot.ingress else {
        panic!("{:?}", bot.ingress);
    };
    let WebhookSecret::Token(secret) = &hook.secret_token else {
        panic!("{:?}", hook.secret_token);
    };
    assert_eq!(secret.expose(), "hook-secret");
    assert_eq!(
        pull(&cfg, "q").auth_token.unwrap().expose(),
        "literal-$-token"
    );
    let p = push(&cfg, "p");
    assert_eq!(p.endpoints[0].url.expose().as_str(), "https://p.example/tg");
    assert_eq!(
        p.endpoints[0].secret_token.as_ref().unwrap().expose(),
        "pre-fix"
    );
}

#[test]
fn placeholders_elsewhere_are_literal() {
    let yaml = r#"version: 1
# ${env:IN_A_COMMENT} is never expanded.
bots:
  - name: a
    token: t
    ingress: { long_poll: { allowed_updates: ["${env:NOT_EXPANDED}"] } }
    routes:
      - match: { text: { equals: ["${env:NOT_EXPANDED}"] } }
        to: [p]
      - to: [p]
consumers:
  p: { push: { endpoints: [{ url: "http://p/" }] } }
"#;
    let cfg = loads(yaml);
    let Some(Ingress::LongPoll(poll)) = &cfg.bots[0].ingress else {
        panic!("{:?}", cfg.bots[0].ingress);
    };
    assert_eq!(
        poll.allowed_updates.as_deref(),
        Some(&["${env:NOT_EXPANDED}".to_owned()][..])
    );
    assert_eq!(
        cfg.bots[0].rules[0].predicates.text,
        Some(TextMatch::Equals(vec!["${env:NOT_EXPANDED}".into()]))
    );
}

#[test]
fn missing_env_located() {
    let yaml = r#"version: 1
bots:
  - name: a
    token: "${env:NOPE}"
    api_url: "${file:missing.txt}"
"#;
    let errors = errors(yaml);
    assert_eq!(errors.len(), 2, "{}", render(&errors));
    assert_at(&errors[0], "bots[0].token", pos(yaml, "\"${env:NOPE}"));
    // The variable name is the operator's reference, not secret content.
    assert_message(&errors[0], "environment variable NOPE is not set");
    assert_at(&errors[1], "bots[0].api_url", pos(yaml, "\"${file:"));
    assert_message(&errors[1], "missing.txt");
}

#[test]
fn malformed_secret_reference_is_not_echoed() {
    let yaml = "version: 1\nbots:\n  - name: a\n    token: \"s3cr3t${oops\"\n";
    let err = single_error(yaml);
    assert_at(&err, "bots[0].token", pos(yaml, "\"s3cr3t"));
    assert!(!err.to_string().contains("s3cr3t"), "{err}");
    assert!(!err.to_string().contains("oops"), "{err}");
}

#[test]
fn redacted_json_has_no_secret() {
    let yaml = r#"version: 1
server: {}
admin: { token: literal-admin-token }
bots:
  - name: a
    token: "${env:BOT_TOKEN}"
    ingress: { webhook: { secret_token: literal-hook-secret } }
  - name: b
    token: "9:literal-bot-token"
consumers:
  q: { pull: { auth_token: literal-pull-token } }
  p: { push: { endpoints: [{ url: "http://p/", secret_token: literal-push-secret }] } }
"#;
    let mut env = MapEnv::default();
    env.vars
        .insert("BOT_TOKEN".into(), "1:env-bot-token".into());
    let cfg = config::load(yaml, Path::new("tgin.yaml"), &env)
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    let json = config::redacted_json(&cfg);
    let text = json.to_string();
    let debug = format!("{cfg:?}");
    for secret in [
        "literal-admin-token",
        "env-bot-token",
        "literal-hook-secret",
        "literal-bot-token",
        "literal-pull-token",
        "literal-push-secret",
    ] {
        assert!(!text.contains(secret), "{secret} in {text}");
        assert!(!debug.contains(secret), "{secret} in {debug}");
    }
    assert_eq!(json["admin"]["token"], "[redacted]");
    assert_eq!(json["bots"][0]["token"], "[redacted]");
    assert_eq!(
        json["bots"][0]["ingress"]["webhook"]["secret_token"],
        "[redacted]"
    );
    assert_eq!(
        json["consumers"]["p"]["push"]["endpoints"][0]["secret_token"],
        "[redacted]"
    );
    // The rest is the config as written, defaults included.
    assert_eq!(json["consumers"]["q"]["pull"]["capacity_bytes"], "64MiB");
    assert_eq!(json["reload"]["debounce"], "500ms");
}

#[test]
fn schema_is_up_to_date() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/tgin.v1.schema.json");
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == format!("{}\n", config::schema()),
        "{} is stale; run `cargo xtask schema`",
        path.display()
    );
}

#[test]
fn nested_split_typo_has_line_column() {
    let yaml = r#"version: 1
bots:
  - name: a
    token: t
    routes:
      - to: [{split: {among: [a, {all: [b]}], strategy: round_robbin}}]
"#;
    let err = single_error(yaml);
    assert_at(
        &err,
        "bots[0].routes[0].to[0].split.strategy",
        pos(yaml, "round_robbin"),
    );
    assert_message(&err, "unknown variant `round_robbin`");
}

#[test]
fn unknown_key_in_nested_target_has_line_column() {
    let yaml = r#"version: 1
bots:
  - name: a
    token: t
    routes:
      - to:
          - all:
              - a
              - split: { among: [b], stratgy: round_robin }
"#;
    let err = single_error(yaml);
    assert_at(
        &err,
        "bots[0].routes[0].to[0].all[1].split.stratgy",
        pos(yaml, "stratgy"),
    );
    assert_message(&err, "unknown field `stratgy`");

    let yaml = r#"version: 1
bots:
  - name: a
    token: t
    routes:
      - to: [{ all: [a, { alll: [b] }] }]
"#;
    let err = single_error(yaml);
    assert_at(&err, "bots[0].routes[0].to[0].all[1]", pos(yaml, "alll"));
    assert_message(&err, "unknown variant `alll`, expected one of split, all");

    let yaml = r#"version: 1
bots:
  - name: a
    token: t
    routes:
      - to: [{ all: [a], split: { among: [b], strategy: round_robin } }]
"#;
    let err = single_error(yaml);
    assert_message(&err, "exactly one key");
    assert_eq!(err.at.as_ref().map(|at| at.line), Some(6), "{err}");
}

fn env(vars: &[(&str, &str)]) -> MapEnv {
    let mut env = MapEnv::default();
    for (k, v) in vars {
        env.vars.insert((*k).into(), (*v).into());
    }
    env
}

#[test]
fn http_paths_interpolate() {
    let yaml = r#"version: 1
server: {}
bots:
  - name: a
    token: t
    ingress: { webhook: { path: "/hook/${env:HOOK_SEGMENT}" } }
consumers:
  q: { pull: { aliases: ["${env:ALIAS}"] } }
"#;
    let env = env(&[("HOOK_SEGMENT", "s3cret-segment"), ("ALIAS", "/queue/q")]);
    let cfg = config::load(yaml, Path::new("tgin.yaml"), &env)
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    let Some(Ingress::Webhook(hook)) = &cfg.bots[0].ingress else {
        panic!("{:?}", cfg.bots[0].ingress);
    };
    assert_eq!(hook.path.expose(), "/hook/s3cret-segment");
    assert_eq!(pull(&cfg, "q").aliases[0].expose(), "/queue/q");
    // Debug shows the path as written.
    let debug = format!("{cfg:?}");
    assert!(!debug.contains("s3cret-segment"), "{debug}");
    assert!(debug.contains("/hook/${env:HOOK_SEGMENT}"), "{debug}");
}

#[test]
fn interpolated_path_checked_after_expansion() {
    let yaml = r#"version: 1
server: {}
bots:
  - name: a
    token: t
    ingress: { webhook: { path: "${env:HOOK_PATH}" } }
consumers:
  q: { pull: { aliases: ["${env:ALIAS}"] } }
"#;
    let env = env(&[
        ("HOOK_PATH", "/v1/s3cret-hook"),
        ("ALIAS", "/webhook/s3cret-dup"),
    ]);
    let errors = config::load(yaml, Path::new("tgin.yaml"), &env).unwrap_err();
    assert_eq!(errors.len(), 1, "{}", render(&errors));
    assert_at(
        &errors[0],
        "bots[0].ingress.webhook.path",
        pos(yaml, "\"${env:HOOK_PATH}"),
    );
    assert_message(&errors[0], "which tgin reserves");
    assert!(!render(&errors).contains("s3cret"), "{}", render(&errors));

    // Uniqueness compares expanded paths.
    let env = self::env(&[
        ("HOOK_PATH", "/webhook/s3cret-dup"),
        ("ALIAS", "/webhook/s3cret-dup"),
    ]);
    let errors = config::load(yaml, Path::new("tgin.yaml"), &env).unwrap_err();
    assert_eq!(errors.len(), 1, "{}", render(&errors));
    assert_at(
        &errors[0],
        "bots[0].ingress.webhook.path",
        pos(yaml, "\"${env:HOOK_PATH}"),
    );
    assert_message(&errors[0], "already used by consumers.q.pull.aliases[0]");
    assert!(!render(&errors).contains("s3cret"), "{}", render(&errors));
}

/// A malformed reference or unknown scheme in a URL field may sit inside its
/// credentials, so the error quotes none of the written text.
#[test]
fn url_interpolation_errors_quote_no_text() {
    for (url, message) in [
        (
            "https://user:pa55w0rd@host/${env:BROKEN",
            "a `${…}` reference is malformed",
        ),
        (
            "https://user:pa55w0rd@host/${vault:k3y}",
            "a `${…}` reference is malformed",
        ),
        ("${env:MISSING_URL}", "MISSING_URL"),
    ] {
        let yaml =
            format!("version: 1\nbots:\n  - name: a\n    token: t\n    api_url: \"{url}\"\n");
        let errors = config::load(&yaml, Path::new("tgin.yaml"), &env(&[])).unwrap_err();
        assert_eq!(errors.len(), 1, "{}", render(&errors));
        assert_at(
            &errors[0],
            "bots[0].api_url",
            pos(&yaml, &format!("\"{url}")),
        );
        assert_message(&errors[0], message);
        let text = format!("{} {:?}", render(&errors), errors);
        for leaked in ["pa55w0rd", "vault", "k3y", "BROKEN"] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
    }
}

#[test]
fn url_scheme_error_quotes_no_scheme() {
    let yaml =
        "version: 1\nbots:\n  - name: a\n    token: t\n    api_url: \"s3cretscheme://host/\"\n";
    let errors = config::load(yaml, Path::new("tgin.yaml"), &env(&[])).unwrap_err();
    assert_eq!(errors.len(), 1, "{}", render(&errors));
    assert_message(&errors[0], "must be http or https");
    let text = format!("{} {:?}", render(&errors), errors);
    assert!(!text.contains("s3cretscheme"), "{text}");
}

/// The three renderings of a config: `Config`'s `Debug`, `redacted_json`
/// and `RawConfig`'s `Debug`.
fn renderings(yaml: &str, env: &MapEnv) -> [String; 3] {
    let cfg = config::load(yaml, Path::new("tgin.yaml"), env)
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    let raw: config::RawConfig = parse_yaml(yaml).unwrap();
    [
        format!("{cfg:?}"),
        config::redacted_json(&cfg).to_string(),
        format!("{raw:?}"),
    ]
}

#[test]
fn urls_and_paths_masked_in_debug_and_json() {
    let yaml = r#"version: 1
server: {}
admin: { token: configured-admin-token }
bots:
  - name: a
    token: "1:t"
    api_url: "${env:API_URL}"
  - name: b
    token: "2:t"
    ingress: { webhook: { public_url: "https://bot.example/hook?key=qu3ry-secret", register: true, path: /hook/configured-webhook-secret, secret_token: configured-webhook-secret } }
consumers:
  p:
    push:
      endpoints:
        - url: "${env:PUSH_URL}"
        - url: "https://api.telegram.org/bot123456789:AAbbccddeeffgghhiijjkkllmmnnoopp_qq/sendMessage"
        - url: "http://hooks.example/configured-admin-token/x"
"#;
    let env = env(&[
        ("API_URL", "https://user:pa55word@bot-api.internal/"),
        ("PUSH_URL", "https://push.example/tg?token=env-qu3ry"),
    ]);
    let cfg = config::load(yaml, Path::new("tgin.yaml"), &env)
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    let p = push(&cfg, "p");
    assert_eq!(p.endpoints[0].url.expose().query(), Some("token=env-qu3ry"));
    assert_eq!(cfg.bots[0].api_url.expose().password(), Some("pa55word"));

    for rendering in renderings(yaml, &env) {
        for leaked in [
            "pa55word",
            "env-qu3ry",
            "qu3ry-secret",
            "AAbbccddeeffgghhiijjkkllmmnnoopp_qq",
            "configured-admin-token",
            "configured-webhook-secret",
        ] {
            assert!(!rendering.contains(leaked), "{leaked} in {rendering}");
        }
    }
    let json = config::redacted_json(&cfg);
    let endpoints = &json["consumers"]["p"]["push"]["endpoints"];
    assert_eq!(endpoints[0]["url"], "https://push.example/…");
    assert_eq!(endpoints[1]["url"], "https://api.telegram.org/…");
    assert_eq!(endpoints[2]["url"], "http://hooks.example/…");
    assert_eq!(json["bots"][0]["api_url"], "https://bot-api.internal");
    let hook = &json["bots"][1]["ingress"]["webhook"];
    assert_eq!(hook["public_url"], "https://bot.example/…");
    assert_eq!(hook["path"], "/hook/#####");
}

/// URL fields with no resolved counterpart render as `[redacted]`, never as
/// written.
#[test]
fn redacted_json_url_fallback_fails_closed() {
    let yaml = r#"version: 1
server: {}
bots:
  - name: b
    token: "2:t"
    api_url: "https://user:pa55word@bot-api.internal/x"
    ingress: { webhook: { public_url: "https://bot.example/hook?key=qu3ry-secret", register: true } }
consumers:
  p:
    push:
      endpoints:
        - url: "https://push.example/tg?token=written-qu3ry"
"#;
    let mut cfg = config::load(yaml, Path::new("tgin.yaml"), &env(&[]))
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    cfg.bots.clear();
    for consumer in &mut cfg.consumers {
        if let config::ConsumerKind::Push(push) = &mut consumer.kind {
            push.endpoints.clear();
        }
    }
    let json = config::redacted_json(&cfg);
    assert_eq!(json["bots"][0]["api_url"], "[redacted]");
    assert_eq!(
        json["bots"][0]["ingress"]["webhook"]["public_url"],
        "[redacted]"
    );
    assert_eq!(
        json["consumers"]["p"]["push"]["endpoints"][0]["url"],
        "[redacted]"
    );
    let text = json.to_string();
    for leaked in ["bot-api.internal", "bot.example", "push.example"] {
        assert!(!text.contains(leaked), "{leaked} in {text}");
    }
}

/// Every URL renders as its parsed origin, whatever the written text looks
/// like, so no parser quirk can expose the rest.
#[test]
fn urls_render_origin_only() {
    let probes = [
        (
            "https://${env:HOOK_HOST}/hook?token=literal-query-secret",
            "https://bot.example/…",
        ),
        (
            "http://admin:pa55w0rd-literal@${env:HOST}:8080/hook",
            "http://push.internal:8080/…",
        ),
        (
            "${env:BASE}/hook?token=literal-query-secret",
            "https://base.example/…",
        ),
        ("${env:BASE}/hook#frag-secret", "https://base.example/…"),
        (
            "${env:BASE}/cb?token=lit-secret&next=http://x/",
            "https://base.example/…",
        ),
        (
            "https:///user:pa55w0rd-literal@push.example/x",
            "https://push.example/…",
        ),
        (
            "https:////h0st-secret.example/x",
            "https://h0st-secret.example/…",
        ),
        ("http://[::1]:9000/path-secret", "http://[::1]:9000/…"),
        ("https://h.example/путь-secret", "https://h.example/…"),
        (
            "https://api.telegram.org/bot${env:TOKEN}/sendMessage",
            "https://api.telegram.org/…",
        ),
    ];
    let env = env(&[
        ("HOOK_HOST", "bot.example"),
        ("HOST", "push.internal"),
        ("BASE", "https://base.example"),
        ("TOKEN", "123456789:AAbbccddeeffgghhiijjkkllmmnnoopp_qq"),
    ]);
    let endpoints: String = probes
        .iter()
        .map(|(url, _)| format!("        - url: \"{url}\"\n"))
        .collect();
    let yaml = format!(
        "version: 1\nserver: {{}}\nbots:\n  - name: a\n    token: \"1:t\"\n    api_url: \"{}\"\n    ingress: {{ webhook: {{ public_url: \"{}\", register: true }} }}\nconsumers:\n  p:\n    push:\n      endpoints:\n{endpoints}",
        probes[3].0, probes[2].0
    );
    let cfg = config::load(&yaml, Path::new("tgin.yaml"), &env)
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    let [debug, json_text, raw] = renderings(&yaml, &env);
    for rendering in [&debug, &json_text, &raw] {
        for leaked in [
            "literal-query-secret",
            "pa55w0rd-literal",
            "frag-secret",
            "lit-secret",
            "path-secret",
            "путь",
            "user:pa55",
            "admin:pa55",
            "AAbbccddeeffgghhiijjkkllmmnnoopp_qq",
            "${env:",
        ] {
            assert!(!rendering.contains(leaked), "{leaked} in {rendering}");
        }
    }
    let json = config::redacted_json(&cfg);
    let urls = &json["consumers"]["p"]["push"]["endpoints"];
    for (k, (written, shown)) in probes.iter().enumerate() {
        assert_eq!(urls[k]["url"], *shown, "{written}");
        let resolved = push(&cfg, "p").endpoints[k].url.clone();
        assert_eq!(format!("{resolved:?}"), format!("{shown:?}"), "{written}");
    }
    assert_eq!(json["bots"][0]["api_url"], "https://base.example/…");
    assert_eq!(
        json["bots"][0]["ingress"]["webhook"]["public_url"],
        "https://base.example/…"
    );
    // `RawConfig` cannot know the config's secrets, so it shows no URL or
    // path text at all.
    assert!(raw.contains("Template(..)"), "{raw}");
}

#[test]
fn secret_errors_never_quote_the_secret() {
    // Malformed references and unknown schemes may be secret content.
    for token in ["p4ss${w0rd:x}", "p4ss${w0rd"] {
        let yaml = format!("version: 1\nbots:\n  - name: a\n    token: \"{token}\"\n");
        let err = single_error(&yaml);
        assert_at(&err, "bots[0].token", pos(&yaml, "\"p4ss"));
        assert_message(&err, "cannot expand the secret");
        for part in ["p4ss", "w0rd"] {
            assert!(!err.to_string().contains(part), "{err}");
        }
    }

    // An unreadable secret file is named, with the error but no contents.
    let yaml = "version: 1\nbots:\n  - name: a\n    token: \"${file:secrets/token}\"\n";
    let err = single_error(yaml);
    assert_at(&err, "bots[0].token", pos(yaml, "\"${file:"));
    assert_message(&err, "cannot read");
    assert_message(&err, "token");
}

#[test]
fn empty_file_path_rejected() {
    let yaml = "version: 1\nserver:\n  tls: { cert_file: \"${env:EMPTY}\", key_file: /k }\n";
    let errors = config::load(yaml, Path::new("tgin.yaml"), &env(&[("EMPTY", "")])).unwrap_err();
    assert_eq!(errors.len(), 1, "{}", render(&errors));
    assert_at(
        &errors[0],
        "server.tls.cert_file",
        pos(yaml, "\"${env:EMPTY}"),
    );
    assert_message(&errors[0], "the file path is empty");
}

/// Relative file paths, written or expanded from a reference, resolve
/// against the config file's directory, never the process CWD (a Windows
/// service runs in `System32`); absolute paths stay as written.
#[test]
fn relative_file_paths_resolve_against_the_config_directory() {
    let dir = std::env::temp_dir().join("tgin-relative-file-paths");
    let absolute = std::env::temp_dir().join("elsewhere").join("tls.key");
    let yaml = format!(
        "version: 1\nserver:\n  tls: {{ cert_file: tls/tls.crt, key_file: \"{}\" }}\nbots:\n  - name: a\n    token: t\n    ingress: {{ webhook: {{ public_url: \"https://bot.example\", register: true, certificate_file: \"${{env:CERT}}\" }} }}\n",
        absolute.display().to_string().replace('\\', "\\\\")
    );
    let cfg = config::load(
        &yaml,
        &dir.join("tgin.yaml"),
        &env(&[("CERT", "certs/public.pem")]),
    )
    .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    let tls = cfg.server.as_ref().unwrap().tls.as_ref().unwrap();
    assert_eq!(tls.cert_file.expose(), &dir.join("tls/tls.crt"));
    assert_eq!(tls.key_file.expose(), &absolute);
    let Some(Ingress::Webhook(hook)) = &cfg.bots[0].ingress else {
        panic!("{:?}", cfg.bots[0].ingress);
    };
    assert_eq!(
        hook.certificate_file.as_ref().map(|file| file.expose()),
        Some(&dir.join("certs/public.pem"))
    );

    // A rooted path stays as written, on every platform.
    let yaml = "version: 1\nserver:\n  tls: { cert_file: /tls/tls.crt, key_file: /tls/tls.key }\n";
    let cfg = config::load(yaml, &dir.join("tgin.yaml"), &MapEnv::default())
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    let tls = cfg.server.as_ref().unwrap().tls.as_ref().unwrap();
    assert_eq!(tls.cert_file.expose(), Path::new("/tls/tls.crt"));
}

/// A combined-regex failure has no single rule; it is reported at the
/// first rule with a regex.
#[test]
fn regex_set_error_located() {
    // Each pattern compiles alone; together they exceed the NFA size limit.
    let pattern = r"\w{150}";
    let rules: String = (0..3)
        .map(|_| {
            format!("      - match: {{ text: {{ regex: ['{pattern}'] }} }}\n        to: [p]\n")
        })
        .collect();
    let yaml = format!(
        "version: 1\nbots:\n  - name: a\n    token: t\n    routes:\n      - to: [p]\n{rules}consumers:\n  p: {{ push: {{ endpoints: [{{ url: \"http://p/\" }}] }} }}\n"
    );
    let err = single_error(&yaml);
    assert_at(&err, "bots[0].routes[1]", pos(&yaml, "match: { text"));
    assert!(
        err.hint.as_deref().is_some_and(|h| h.contains("together")),
        "{err}"
    );
}

/// The schema's duration pattern accepts what the loader accepts.
#[test]
fn duration_schema_matches_loader() {
    let schema: serde_json::Value = serde_json::from_str(&config::schema()).unwrap();
    let branches = schema["$defs"]["Duration"]["anyOf"].as_array().unwrap();
    let pattern = branches[0]["pattern"].as_str().unwrap();
    assert_eq!(
        branches[1],
        serde_json::json!({ "type": "integer", "const": 0 })
    );
    let re = regex_automata::meta::Regex::new(pattern).unwrap();
    for ok in ["0", "1.5s", "0.5h", "500ms", "1h 30m", "2d"] {
        assert!(re.is_match(ok), "schema rejects {ok}");
        let cfg = loads(&format!(
            "version: 1\nreload: {{ drain_timeout: \"{ok}\" }}\n"
        ));
        assert_eq!(
            cfg.reload.drain_timeout,
            humantime::parse_duration(ok).unwrap()
        );
    }
    for bad in ["5M", "1y", "2 months", "1.s", "s", ""] {
        assert!(!re.is_match(bad), "schema accepts {bad}");
    }
    // An unquoted YAML `0` is the integer the schema's `const` branch takes.
    assert_eq!(
        loads("version: 1\nreload: { drain_timeout: 0 }\n")
            .reload
            .drain_timeout,
        Duration::ZERO
    );
    let yaml = "version: 1\nreload: { drain_timeout: 30 }\n";
    let err = single_error(yaml);
    assert_message(&err, "add a unit, such as `30s`");
}

#[test]
fn rule_long_poll_timeout_at_least_one_second() {
    for timeout in ["0", "500ms", "999ms"] {
        let yaml = format!(
            "version: 1\nbots:\n  - name: a\n    token: t\n    ingress: {{ long_poll: {{ timeout: \"{timeout}\" }} }}\n"
        );
        let err = single_error(&yaml);
        assert_at(
            &err,
            "bots[0].ingress.long_poll.timeout",
            pos(&yaml, &format!("\"{timeout}\"")),
        );
        assert_message(&err, "shorter than the minimum of 1s");
        assert!(
            err.hint
                .as_deref()
                .is_some_and(|h| h.contains("whole seconds")),
            "{err}"
        );
    }
    loads(
        "version: 1\nbots:\n  - name: a\n    token: t\n    ingress: { long_poll: { timeout: 1s } }\n",
    );
}

/// Durations that would disable what they configure, or make it misbehave,
/// are rejected where they are written.
#[test]
fn rule_degenerate_durations_rejected() {
    let push_yaml = |field: &str| {
        format!(
            "version: 1\nconsumers:\n  p:\n    push:\n      endpoints: [{{ url: \"http://p/\"{field} }}]\n"
        )
    };
    let pull_yaml = |lease: &str| {
        format!(
            "version: 1\nserver: {{}}\nconsumers:\n  q: {{ pull: {{ lease_timeout: {lease} }} }}\n"
        )
    };
    let cases = [
        (
            "version: 1\nserver: { header_read_timeout: 0 }\n".to_owned(),
            "server.header_read_timeout",
            "header_read_timeout: 0",
            "must be longer than 0",
        ),
        (
            push_yaml(", timeout: 0"),
            "consumers.p.push.endpoints[0].timeout",
            "timeout: 0",
            "must be longer than 0",
        ),
        (
            format!("{}      retry: {{ initial: 0 }}\n", push_yaml("")),
            "consumers.p.push.retry.initial",
            "initial: 0",
            "must be longer than 0",
        ),
        (
            format!("{}      retry: {{ max: 0s }}\n", push_yaml("")),
            "consumers.p.push.retry.max",
            "max: 0s",
            "must be longer than 0",
        ),
        (
            format!("{}      retry: {{ max_age: 0 }}\n", push_yaml("")),
            "consumers.p.push.retry.max_age",
            "max_age: 0",
            "must be longer than 0",
        ),
        (
            format!("{}      breaker: {{ open_for: 0 }}\n", push_yaml("")),
            "consumers.p.push.breaker.open_for",
            "open_for: 0",
            "must be longer than 0",
        ),
        (
            pull_yaml("0"),
            "consumers.q.pull.lease_timeout",
            "lease_timeout: 0",
            "shorter than the minimum of 1s",
        ),
        (
            pull_yaml("999ms"),
            "consumers.q.pull.lease_timeout",
            "lease_timeout: 999ms",
            "shorter than the minimum of 1s",
        ),
    ];
    for (yaml, path, needle, message) in cases {
        let err = single_error(&yaml);
        let (line, column) = pos(&yaml, needle);
        let value_column = column + u32::try_from(needle.find(": ").unwrap() + 2).unwrap();
        assert_at(&err, path, (line, value_column));
        assert_message(&err, message);
        assert!(err.hint.is_some(), "{err}");
    }

    // Zero is meaningful here: no lame duck, no drain, no drain on reload.
    let cfg = loads(
        "version: 1\nruntime: { lame_duck: 0, shutdown_grace: 0 }\nreload: { drain_timeout: 0 }\nserver: {}\nconsumers:\n  q: { pull: { lease_timeout: 1s } }\n",
    );
    assert_eq!(cfg.runtime.lame_duck, Duration::ZERO);
    assert_eq!(cfg.runtime.shutdown_grace, Duration::ZERO);
    assert_eq!(cfg.reload.drain_timeout, Duration::ZERO);
    assert_eq!(pull(&cfg, "q").queue.lease_timeout, secs(1));
}

/// A bot token is checked at load with the Bot API client's rule, so
/// `config validate`, start and reload all refuse what the client would.
#[test]
fn rule_bot_token_charset() {
    let env = env(&[("SPACED", "123456:abc def"), ("NEWLINE", "123456:abc\n")]);
    for (k, token) in [
        (0, "\"123456:abc def\""),
        (1, "\"123456:a+b/c=\""),
        (2, "\"${env:SPACED}\""),
        (3, "\"${env:NEWLINE}\""),
    ] {
        let yaml = format!("version: 1\nbots:\n  - name: b{k}\n    token: {token}\n");
        let errors = config::load(&yaml, Path::new("tgin.yaml"), &env).unwrap_err();
        assert_eq!(errors.len(), 1, "{}", render(&errors));
        let err = &errors[0];
        assert_at(err, "bots[0].token", pos(&yaml, token));
        assert_message(err, "bot token");
        assert!(
            err.hint
                .as_deref()
                .is_some_and(|h| h.contains("[A-Za-z0-9:_-]")),
            "{err}"
        );
        for part in ["abc", "a+b", "def"] {
            assert!(!err.to_string().contains(part), "{err}");
        }
    }
    assert!(config::valid_bot_token("123456:AA-bb_CC"));
    assert!(!config::valid_bot_token(""));
    assert!(!config::valid_bot_token("123456:abc "));
    loads("version: 1\nbots:\n  - name: b\n    token: \"123456:AA-bb_CC\"\n");
}

/// Telegram accepts a webhook secret of 1 to 256 of `[A-Za-z0-9_-]` only.
#[test]
fn rule_webhook_secret_token_charset() {
    let long = "a".repeat(257);
    for secret in [
        "\"has space+plus\"",
        "\"c2VjcmV0Lw==\"",
        &format!("\"{long}\""),
    ] {
        let yaml = format!(
            "version: 1\nserver: {{}}\nbots:\n  - name: a\n    token: t\n    ingress: {{ webhook: {{ secret_token: {secret} }} }}\n"
        );
        let err = single_error(&yaml);
        assert_at(
            &err,
            "bots[0].ingress.webhook.secret_token",
            pos(&yaml, secret),
        );
        assert_message(&err, "secret_token");
        assert!(
            err.hint
                .as_deref()
                .is_some_and(|h| h.contains("1 to 256 ASCII letters, digits, `_` or `-`")),
            "{err}"
        );
        for part in ["space", "c2Vj", "aaaa"] {
            assert!(!err.to_string().contains(part), "{err}");
        }
    }
    let max = "Z_-9".repeat(64);
    let cfg = loads(&format!(
        "version: 1\nserver: {{}}\nbots:\n  - name: a\n    token: t\n    ingress: {{ webhook: {{ secret_token: \"{max}\" }} }}\n"
    ));
    let Some(Ingress::Webhook(hook)) = &cfg.bots[0].ingress else {
        panic!("{:?}", cfg.bots[0].ingress);
    };
    assert!(matches!(&hook.secret_token, WebhookSecret::Token(s) if *s.expose() == max));
}

/// A webhook path or alias reaches the router byte for byte only if a URL
/// carries it unchanged; anything a URL moves, drops or encodes is refused.
#[test]
fn rule_paths_survive_url_encoding() {
    for (path, message) in [
        ("/hook?bot=shop", "`?`"),
        ("/hook#frag", "`#`"),
        ("/hook x", "whitespace"),
        ("/hook\\tx", "control"),
        ("/hook/\u{e9}", "non-ASCII"),
        ("/a/../b", "changes"),
        ("/a/./b", "changes"),
        ("/a\\\\b", "changes"),
    ] {
        for yaml in [
            format!(
                "version: 1\nserver: {{}}\nbots:\n  - name: a\n    token: t\n    ingress: {{ webhook: {{ path: \"{path}\" }} }}\n"
            ),
            format!(
                "version: 1\nserver: {{}}\nconsumers:\n  q: {{ pull: {{ aliases: [\"{path}\"] }} }}\n"
            ),
        ] {
            let err = single_error(&yaml);
            assert_message(&err, message);
            for part in ["bot=shop", "#frag", "hook x", "/b"] {
                assert!(!err.to_string().contains(part), "{err}");
            }
        }
    }
    let cfg = loads(
        "version: 1\nserver: {}\nconsumers:\n  q: { pull: { aliases: [\"/hook%20x\", \"/a|b\", \"/a;b=c\", \"/~u/x.y\"] } }\n",
    );
    let aliases: Vec<String> = pull(&cfg, "q")
        .aliases
        .iter()
        .map(|a| a.expose().clone())
        .collect();
    assert_eq!(aliases, ["/hook%20x", "/a|b", "/a;b=c", "/~u/x.y"]);
}
