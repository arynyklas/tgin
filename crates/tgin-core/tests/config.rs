//! Loading, validating and resolving `tgin.yaml` version 1.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tgin_core::config::{
    self, Balance, Config, ConsumerKind, Files, Gateway, Ingress, IpAllowlist, LogFormat, LogLevel,
    Pull, Push, Rate, RateLimit, RateMode, Upstream, UpstreamKind, WebhookReply, WebhookRules,
    WebhookSecret,
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
    assert_eq!(
        shop.upstream.url.expose().as_str(),
        "https://api.telegram.org/"
    );
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
        poller.upstream.url.expose().as_str(),
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
  - { name: -dash, token: t2 }
  - { name: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, token: t3 }
  - { name: ok-1, token: t4 }
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
    // The one rule the gateway path parser applies to `/c/<name>` too.
    for good in ["a", "0", "ok-1", "a-", &"a".repeat(63)] {
        assert!(config::valid_name(good), "{good}");
    }
    for bad in ["", "Shop", "-dash", "a_b", "a.b", "é", &"a".repeat(64)] {
        assert!(!config::valid_name(bad), "{bad}");
    }
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
    token: t2
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
    token: t2
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
    token: t2
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
    token: t2
    ingress: { webhook: { secret_token: auto, ip_allowlist: [10.1.0.0/16, 192.0.2.7] } }
  - name: c
    token: t3
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
    assert_eq!(bot.upstream.url.expose().as_str(), "http://bot-api:8081/");
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
    assert_eq!(
        cfg.bots[0].upstream.url.expose().password(),
        Some("pa55word")
    );

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

/// The repository's configs, each with every `${env:…}` it uses set (a
/// `*_URL` to the cloud, anything else to a distinct token-shaped value),
/// and the upstream URL all its `api_url` shorthand bots have.
fn repo_configs() -> Vec<(PathBuf, String, MapEnv, &'static str)> {
    let cloud = "https://api.telegram.org/";
    let files = [
        ("crates/tgin-core/tests/fixtures/full.yaml", cloud),
        ("crates/tgin-core/tests/fixtures/design.yaml", cloud),
        ("crates/tgin-core/tests/fixtures/minimal.yaml", cloud),
        ("tgin.yaml", cloud),
        ("examples/simple/config/tgin.yaml", cloud),
        ("tests/e2e/tgin.yaml", "http://fake:8081/"),
        (
            "tests/performance/cfgs/v1/longpoll-1.yaml",
            "http://10.5.0.3:8090/",
        ),
        (
            "tests/performance/cfgs/v1/longpoll-3.yaml",
            "http://10.5.0.3:8090/",
        ),
        (
            "tests/performance/cfgs/v1/webhook-1.yaml",
            "http://10.5.0.3:8090/",
        ),
        (
            "tests/performance/cfgs/v1/webhook-3.yaml",
            "http://10.5.0.3:8090/",
        ),
    ];
    files
        .into_iter()
        .map(|(file, url)| {
            let (path, text, env) = repo_config(file);
            (path, text, env, url)
        })
        .collect()
}

/// The repository config `file`, with every `${env:…}` it uses set as
/// [`repo_configs`] sets them: its path (relative, so that an absolute
/// `${file:…}` path resolves as written on every platform), text and
/// environment.
fn repo_config(file: &str) -> (PathBuf, String, MapEnv) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut env = MapEnv::default();
    env.files.insert(
        PathBuf::from("/run/secrets/tgin-admin-token"),
        "admin-api-token\n".into(),
    );
    for (i, reference) in text.split("${env:").skip(1).enumerate() {
        let name = reference.split('}').next().unwrap_or_default();
        let value = if name.ends_with("_URL") {
            "https://api.telegram.org".to_owned()
        } else {
            format!("value-{i}")
        };
        env.vars.insert(name.to_owned(), value);
    }
    (PathBuf::from(file), text, env)
}

/// The self-hosted example loads, its one bot on the `local` upstream in
/// local mode with files from the server's `--files-dir`, its `file:`
/// references refused; its only warning is the plain http to the server,
/// named by the upstream.
#[test]
fn self_hosted_example_loads() {
    let (path, text, mut env) = repo_config("examples/self-hosted/config/tgin.yaml");
    // A bot token as Telegram shapes it, which the gateway needs to serve
    // the bot (`value-…` would warn).
    env.vars
        .insert("BOT_TOKEN".to_owned(), "123456:example-token".to_owned());
    let cfg = config::load(&text, &path, &env)
        .unwrap_or_else(|errors| panic!("{}: {}", path.display(), render(&errors)));
    let [bot] = cfg.bots.as_slice() else {
        panic!("{} bots", cfg.bots.len());
    };
    let upstream = &bot.upstream;
    assert_eq!(&*upstream.name, "tba");
    assert_eq!(upstream.kind, UpstreamKind::Local);
    assert_eq!(
        upstream.url.expose().as_str(),
        "http://telegram-bot-api:8081/"
    );
    assert!(upstream.local_mode);
    let Some(Files::Dir { dir, server_dir }) = &upstream.files else {
        panic!("{:?}", upstream.files);
    };
    let files = std::path::absolute("/var/lib/telegram-bot-api/files").unwrap();
    assert_eq!((dir.expose(), server_dir.expose()), (&files, &files));
    assert_eq!(upstream.response_timeout, secs(600));
    assert!(!bot.allow_local_files);
    assert!(upstream.refuses_local_files(bot.allow_local_files));
    let [warning] = cfg.warnings.as_slice() else {
        panic!("{}", render(&cfg.warnings));
    };
    assert_at(
        warning,
        "upstreams.tba.url",
        pos(&text, "http://telegram-bot-api"),
    );
}

/// `api_url`, written or left out, is an unnamed cloud upstream named
/// `api_url`, with the URL every existing config already resolved to. Bots
/// on a named upstream are checked by the upstream tests; every listed
/// config keeps at least one shorthand bot, so the check is never vacuous.
#[test]
fn api_url_shorthand_unchanged() {
    for (path, text, env, url) in repo_configs() {
        let cfg = config::load(&text, &path, &env)
            .unwrap_or_else(|errors| panic!("{}: {}", path.display(), render(&errors)));
        let mut shorthand = 0;
        for bot in &cfg.bots {
            let upstream = &bot.upstream;
            if &*upstream.name != "api_url" {
                continue;
            }
            shorthand += 1;
            assert_eq!(upstream.kind, UpstreamKind::Cloud, "{}", path.display());
            assert_eq!(upstream.url.expose().as_str(), url, "{}", path.display());
            assert!(!upstream.local_mode, "{}", path.display());
            assert_eq!(upstream.files, None, "{}", path.display());
            assert_eq!(upstream.response_timeout, secs(60), "{}", path.display());
            // The benchmark configs' fake stands in for the cloud, which
            // tgin does not scan; every other config keeps the default.
            let benchmark = path.starts_with("tests/performance");
            assert_eq!(bot.allow_local_files, benchmark, "{}", path.display());
        }
        assert!(
            shorthand > 0,
            "{}: no api_url shorthand bot",
            path.display()
        );
    }

    // A bot with neither `api_url` nor `upstream` gets the cloud default;
    // `/v1/config` shows it as `api_url`, defaults filled in.
    let cfg = loads(
        "version: 1\nbots:\n  - name: a\n    token: \"1:t\"\n    api_url: \"https://bot-api.example/x\"\n  - name: b\n    token: \"2:t\"\n",
    );
    assert_eq!(
        cfg.bots[0].upstream.url.expose().as_str(),
        "https://bot-api.example/x"
    );
    assert_eq!(
        cfg.bots[1].upstream.url.expose().as_str(),
        "https://api.telegram.org/"
    );
    let json = config::redacted_json(&cfg);
    assert_eq!(json["bots"][0]["api_url"], "https://bot-api.example/…");
    assert_eq!(json["bots"][1]["api_url"], "https://api.telegram.org");
    assert_eq!(json["bots"][1]["allow_local_files"], false);
    assert!(json["bots"][1].get("upstream").is_none(), "{json}");
    assert!(json.get("upstreams").is_none(), "{json}");
}

/// `file:` references are refused for every server but Telegram's cloud
/// (`https` to `api.telegram.org`, its host in any case), and for every
/// `local` upstream, unless the bot allows them. Plain `http` to that host
/// authenticates nothing, so it is refused too.
#[test]
fn local_files_refused_off_the_telegram_cloud() {
    let yaml = r#"version: 1
upstreams:
  custom: { kind: cloud, url: "https://bot-api.example" }
  tba: { kind: local, url: "https://api.telegram.org" }
bots:
  - { name: default, token: "1:t" }
  - { name: cloud, token: "2:t", api_url: "https://api.telegram.org" }
  - { name: cased, token: "3:t", api_url: "https://API.Telegram.ORG:443/x" }
  - { name: port, token: "9:t", api_url: "https://API.telegram.org:443" }
  - { name: userinfo, token: "10:t", api_url: "https://u:p@api.telegram.org" }
  - { name: plain, token: "11:t", api_url: "http://api.telegram.org" }
  - { name: at, token: "12:t", api_url: "https://api.telegram.org@evil" }
  - { name: dot, token: "13:t", api_url: "https://api.telegram.org." }
  - { name: loopback, token: "4:t", api_url: "http://127.0.0.1:8081" }
  - { name: lookalike, token: "5:t", api_url: "https://api.telegram.org.example" }
  - { name: sub, token: "6:t", api_url: "https://x.api.telegram.org" }
  - { name: named, token: "7:t", upstream: custom }
  - { name: local, token: "8:t", upstream: tba }
"#;
    let cfg = loads(yaml);
    let refused: Vec<(&str, bool, bool, bool)> = cfg
        .bots
        .iter()
        .map(|bot| {
            (
                bot.name.as_str(),
                bot.upstream.is_telegram_cloud(),
                bot.upstream.refuses_local_files(false),
                bot.upstream.refuses_local_files(true),
            )
        })
        .collect();
    assert_eq!(
        refused,
        [
            ("default", true, false, false),
            ("cloud", true, false, false),
            ("cased", true, false, false),
            ("port", true, false, false),
            ("userinfo", true, false, false),
            ("plain", false, true, false),
            ("at", false, true, false),
            ("dot", false, true, false),
            ("loopback", false, true, false),
            ("lookalike", false, true, false),
            ("sub", false, true, false),
            ("named", false, true, false),
            ("local", true, true, false),
        ]
    );
}

#[test]
fn upstream_and_api_url_exclusive() {
    let yaml = r#"version: 1
upstreams:
  local: { kind: local, url: "http://127.0.0.1:8081" }
bots:
  - name: a
    token: "1:t"
    api_url: https://api.telegram.org
    upstream: local
"#;
    let err = single_error(yaml);
    assert_at(&err, "bots[0].upstream", pos(yaml, "local\n"));
    assert_message(&err, "`upstream` and `api_url`");

    let cfg = loads(&yaml.replace("    api_url: https://api.telegram.org\n", ""));
    let upstream = &cfg.bots[0].upstream;
    assert_eq!(&*upstream.name, "local");
    assert_eq!(upstream.kind, UpstreamKind::Local);
    assert_eq!(upstream.url.expose().as_str(), "http://127.0.0.1:8081/");
    assert!(std::sync::Arc::ptr_eq(upstream, &cfg.upstreams[0]));
}

#[test]
fn unknown_upstream_reference_located() {
    let yaml = r#"version: 1
upstreams:
  local: { kind: local, url: "http://127.0.0.1:8081" }
bots:
  - name: a
    token: "1:t"
    upstream: lcoal
  - name: b
    token: "2:t"
    upstream: api_url
"#;
    let errors = errors(yaml);
    assert_eq!(errors.len(), 2, "{}", render(&errors));
    assert_at(&errors[0], "bots[0].upstream", pos(yaml, "lcoal"));
    assert_message(&errors[0], "unknown upstream `lcoal`");
    assert_at(&errors[1], "bots[1].upstream", pos(yaml, "api_url\n"));
    assert_message(&errors[1], "unknown upstream `api_url`");
    assert!(
        errors[1]
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("`local`")),
        "{}",
        errors[1]
    );

    // `api_url` names the shorthand; no upstream may take it.
    let yaml =
        "version: 1\nupstreams:\n  api_url: { kind: cloud, url: \"https://api.telegram.org\" }\n";
    let err = single_error(yaml);
    assert_at(&err, "upstreams.api_url", pos(yaml, "{ kind"));
    assert_message(&err, "reserved");
    let yaml =
        "version: 1\nupstreams:\n  Bad_Name: { kind: cloud, url: \"https://api.telegram.org\" }\n";
    let err = single_error(yaml);
    assert_message(&err, "invalid name `Bad_Name`");
}

#[test]
fn files_rules() {
    let upstream = |body: &str| {
        format!(
            "version: 1\nupstreams:\n  u:\n{body}bots:\n  - name: a\n    token: \"1:t\"\n    upstream: u\n"
        )
    };

    let yaml = upstream(
        "    kind: cloud\n    url: \"https://api.telegram.org\"\n    files: { dir: /srv/tba }\n",
    );
    let err = single_error(&yaml);
    assert_at(&err, "upstreams.u.files", pos(&yaml, "{ dir"));
    assert_message(&err, "`files`");

    let yaml =
        upstream("    kind: cloud\n    url: \"https://api.telegram.org\"\n    local_mode: true\n");
    let err = single_error(&yaml);
    assert_at(&err, "upstreams.u.local_mode", pos(&yaml, "true"));
    assert_message(&err, "`local_mode`");

    let yaml = upstream(
        "    kind: local\n    url: \"http://127.0.0.1:8081\"\n    files: { server_dir: /var/lib/tba }\n",
    );
    let err = single_error(&yaml);
    assert_at(&err, "upstreams.u.files.server_dir", pos(&yaml, "/var"));
    assert_message(&err, "`server_dir` needs `dir`");

    let yaml = upstream(
        "    kind: local\n    url: \"http://127.0.0.1:8081\"\n    files: { dir: /srv/tba, url: \"http://files:8080\" }\n",
    );
    let err = single_error(&yaml);
    assert_at(&err, "upstreams.u.files", pos(&yaml, "{ dir"));
    assert_message(&err, "either `dir` or `url`");
    let yaml = upstream("    kind: local\n    url: \"http://127.0.0.1:8081\"\n    files: {}\n");
    let err = single_error(&yaml);
    assert_message(&err, "either `dir` or `url`");

    let yaml = upstream(
        "    kind: local\n    url: \"http://127.0.0.1:8081\"\n    files: { dir: /srv/tba, server_dir: var/lib/tba }\n",
    );
    let err = single_error(&yaml);
    assert_at(&err, "upstreams.u.files.server_dir", pos(&yaml, "var/lib"));
    assert_message(&err, "absolute");
    // The server's `--files-dir` only: without it, its bot directories
    // hold TDLib's databases too, which are never to be served.
    let hint = err.hint.as_deref().unwrap_or_default();
    assert!(
        hint.contains("as the Bot API server sees it") && hint.contains("--files-dir"),
        "{err}"
    );
    assert!(!hint.contains("`--dir`"), "{err}");

    // `server_dir` defaults to `dir`, made absolute.
    let cfg = loads(&upstream(
        "    kind: local\n    url: \"http://127.0.0.1:8081\"\n    local_mode: true\n    files: { dir: /srv/tba }\n",
    ));
    let upstream_of = |cfg: &Config| cfg.bots[0].upstream.clone();
    assert!(upstream_of(&cfg).local_mode);
    let srv = std::path::absolute("/srv/tba").unwrap();
    assert_eq!(dirs(&cfg), (srv.clone(), srv.clone()));
    let cfg = loads(&upstream(
        "    kind: local\n    url: \"http://127.0.0.1:8081\"\n    files: { dir: /srv/tba, server_dir: /var/lib/telegram-bot-api }\n",
    ));
    assert_eq!(
        dirs(&cfg),
        (srv, PathBuf::from("/var/lib/telegram-bot-api"))
    );
    let cfg = loads(&upstream(
        "    kind: local\n    url: \"http://127.0.0.1:8081\"\n    files: { url: \"http://127.0.0.1:8080/files\" }\n",
    ));
    let Some(Files::Url(url)) = &upstream_of(&cfg).files else {
        panic!("{:?}", upstream_of(&cfg).files);
    };
    assert_eq!(url.expose().as_str(), "http://127.0.0.1:8080/files");

    // The server names a bot's directory after its token, keeping the
    // `:`, or with `~` for `:` where its filesystem refuses `:`.
    assert_eq!(
        Upstream::token_dirs("123456:AA-bb_CC"),
        ["123456:AA-bb_CC", "123456~AA-bb_CC"]
    );
}

/// The resolved `files.dir` and `files.server_dir` of bot 0's upstream.
fn dirs(cfg: &Config) -> (PathBuf, PathBuf) {
    match &cfg.bots[0].upstream.files {
        Some(Files::Dir { dir, server_dir }) => (dir.expose().clone(), server_dir.expose().clone()),
        other => panic!("{other:?}"),
    }
}

/// A relative `files.dir` is relative to the config file's directory, and
/// to the working directory below that: it and the `server_dir` it
/// defaults to are absolute. `Debug` shows both as written.
#[test]
fn files_dir_absolute_and_masked() {
    let yaml = "version: 1\nupstreams:\n  u: { kind: local, url: \"http://127.0.0.1:8081\", files: { dir: tba } }\nbots:\n  - name: a\n    token: \"1:t\"\n    upstream: u\n";
    for (origin, relative) in [("tgin.yaml", "tba"), ("conf/tgin.yaml", "conf/tba")] {
        let cfg = config::load(yaml, Path::new(origin), &env(&[]))
            .unwrap_or_else(|errors| panic!("{}", render(&errors)));
        let (dir, server_dir) = dirs(&cfg);
        assert!(dir.is_absolute(), "{origin}: {}", dir.display());
        assert_eq!(dir, std::path::absolute(relative).unwrap(), "{origin}");
        assert_eq!(server_dir, dir, "{origin}");
    }

    let yaml = "version: 1\nupstreams:\n  u: { kind: local, url: \"http://127.0.0.1:8081\", files: { dir: \"${env:FILES_DIR}\", server_dir: \"/var/${env:SEG}\" } }\nbots:\n  - name: a\n    token: \"1:t\"\n    upstream: u\n";
    let env = env(&[("FILES_DIR", "/srv/s3cret-env"), ("SEG", "s3cret-seg")]);
    let cfg = config::load(yaml, Path::new("tgin.yaml"), &env)
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    assert_eq!(
        dirs(&cfg),
        (
            std::path::absolute("/srv/s3cret-env").unwrap(),
            PathBuf::from("/var/s3cret-seg")
        )
    );
    for rendering in renderings(yaml, &env) {
        for leaked in ["s3cret-env", "s3cret-seg"] {
            assert!(!rendering.contains(leaked), "{leaked} in {rendering}");
        }
    }
    let debug = format!("{cfg:?}");
    assert!(debug.contains("${env:FILES_DIR}"), "{debug}");
    assert!(debug.contains("/var/${env:SEG}"), "{debug}");
}

/// A token repeated across bots splits its updates between them: refused,
/// at the second bot, without quoting the token.
#[test]
fn duplicate_tokens_rejected() {
    let yaml = "version: 1\nbots:\n  - name: a\n    token: \"1:dup-s3cret\"\n  - name: b\n    token: '1:dup-s3cret'\n";
    let err = single_error(yaml);
    assert_at(&err, "bots[1].token", pos(yaml, "'1:dup"));
    assert_message(&err, "bots[0]");
    assert!(!err.to_string().contains("dup-s3cret"), "{err}");

    let yaml = "version: 1\nbots:\n  - name: a\n    token: \"${env:A}\"\n  - name: b\n    token: \"${env:B}\"\n  - name: c\n    token: \"${env:C}\"\n";
    let env = env(&[
        ("A", "1:dup-s3cret"),
        ("B", "1:dup-s3cret"),
        ("C", "2:other"),
    ]);
    let errors = config::load(yaml, Path::new("tgin.yaml"), &env).unwrap_err();
    assert_eq!(errors.len(), 1, "{}", render(&errors));
    assert_at(&errors[0], "bots[1].token", pos(yaml, "\"${env:B}"));
    assert!(
        !errors[0].to_string().contains("dup-s3cret"),
        "{}",
        errors[0]
    );
}

/// Webhook `public_url`, port and `max_connections` limits follow the
/// bot's upstream: only a local server in local mode posts over plain http,
/// to any port, with up to 100 000 connections.
#[test]
fn webhook_rules_follow_the_upstream_kind() {
    let yaml = |upstreams: &str, bot: &str, hook: &str| {
        format!(
            "version: 1\nserver: {{ max_connections: 100000 }}\n{upstreams}bots:\n  - name: a\n    token: \"1:t\"\n{bot}    ingress: {{ webhook: {hook} }}\n"
        )
    };
    let cloud_like = [
        ("", ""),
        (
            "upstreams:\n  c: { kind: cloud, url: \"https://api.telegram.org\" }\n",
            "    upstream: c\n",
        ),
        (
            "upstreams:\n  l: { kind: local, url: \"http://127.0.0.1:8081\" }\n",
            "    upstream: l\n",
        ),
    ];
    for (upstreams, bot) in cloud_like {
        let text = yaml(upstreams, bot, "{ public_url: \"http://tgin.example\" }");
        let err = single_error(&text);
        assert_at(
            &err,
            "bots[0].ingress.webhook.public_url",
            pos(&text, "\"http://tgin"),
        );
        assert_message(&err, "must be https");

        let text = yaml(
            upstreams,
            bot,
            "{ public_url: \"https://tgin.example:8080\" }",
        );
        let err = single_error(&text);
        assert_at(
            &err,
            "bots[0].ingress.webhook.public_url",
            pos(&text, "\"https://tgin"),
        );
        assert_message(&err, "port 8080");

        let text = yaml(upstreams, bot, "{ max_connections: 101 }");
        let err = single_error(&text);
        assert_at(
            &err,
            "bots[0].ingress.webhook.max_connections",
            pos(&text, "101"),
        );
        assert_message(&err, "use 1 to 100");

        for ok in [
            "{ public_url: \"https://tgin.example\", max_connections: 100 }",
            "{ public_url: \"https://tgin.example:8443/x\" }",
            "{ public_url: \"https://tgin.example:88\" }",
            "{ public_url: \"https://tgin.example:80\" }",
        ] {
            loads(&yaml(upstreams, bot, ok));
        }
        let cfg = loads(&yaml(upstreams, bot, "{}"));
        assert_eq!(
            cfg.bots[0].upstream.webhook_rules(),
            WebhookRules {
                http_allowed: false,
                any_port: false,
                max_connections: 100,
            }
        );
    }

    let local =
        "upstreams:\n  l: { kind: local, url: \"http://127.0.0.1:8081\", local_mode: true }\n";
    let cfg = loads(&yaml(
        local,
        "    upstream: l\n",
        "{ public_url: \"http://tgin:8080\", register: true, max_connections: 100000 }",
    ));
    assert_eq!(
        cfg.bots[0].upstream.webhook_rules(),
        WebhookRules {
            http_allowed: true,
            any_port: true,
            max_connections: 100_000,
        }
    );
    let Some(Ingress::Webhook(hook)) = &cfg.bots[0].ingress else {
        panic!("{:?}", cfg.bots[0].ingress);
    };
    assert_eq!(
        hook.public_url.as_ref().unwrap().expose().as_str(),
        "http://tgin:8080/"
    );
    assert_eq!(hook.max_connections, Some(100_000));
    let text = yaml(local, "    upstream: l\n", "{ max_connections: 100001 }");
    let err = single_error(&text);
    assert_at(
        &err,
        "bots[0].ingress.webhook.max_connections",
        pos(&text, "100001"),
    );
    assert_message(&err, "use 1 to 100000");

    // Telegram's networks never post for a local server.
    for local in [
        local,
        "upstreams:\n  l: { kind: local, url: \"http://127.0.0.1:8081\" }\n",
    ] {
        let text = yaml(local, "    upstream: l\n", "{ ip_allowlist: telegram }");
        let err = single_error(&text);
        assert_at(
            &err,
            "bots[0].ingress.webhook.ip_allowlist",
            pos(&text, "telegram }"),
        );
        assert!(
            err.hint.as_deref().is_some_and(|h| h.contains("CIDR")),
            "{err}"
        );
    }
    loads(&yaml("", "", "{ ip_allowlist: telegram }"));
}

/// A local-mode webhook's `max_connections` above 100 must also fit the
/// data plane's own `server.max_connections`, which closes connections
/// beyond it; values in Telegram's 1 to 100 are never checked against it,
/// so no v1 config changes.
#[test]
fn local_webhook_max_connections_fit_the_data_plane() {
    const TOKEN: &str = "123456789:AAAAexampleSecretTokenValue_-xyz";
    let yaml = |upstream: &str, server: &str, max: u32| {
        format!(
            "version: 1\nserver: {server}\nupstreams:\n  l: {upstream}\nbots:\n  - name: a\n    token: \"{TOKEN}\"\n    upstream: l\n    ingress: {{ webhook: {{ max_connections: {max} }} }}\n"
        )
    };
    let local = "{ kind: local, url: \"http://127.0.0.1:8081\", local_mode: true }";
    let text = yaml(local, "{}", 10_001);
    let err = single_error(&text);
    assert_at(
        &err,
        "bots[0].ingress.webhook.max_connections",
        pos(&text, "10001"),
    );
    assert_message(&err, "above `server.max_connections` (10000)");
    let hint = err.hint.as_deref().unwrap_or_default();
    assert!(hint.contains("server.max_connections"), "{err}");
    assert!(!format!("{err}").contains(TOKEN) && !hint.contains(TOKEN));

    let text = yaml(local, "{ max_connections: 500 }", 501);
    assert_at(
        &single_error(&text),
        "bots[0].ingress.webhook.max_connections",
        pos(&text, "501"),
    );
    for (upstream, server, max) in [
        (local, "{ max_connections: 20000 }", 20_000),
        (local, "{}", 10_000),
        // Within Telegram's range: never compared.
        (local, "{ max_connections: 50 }", 100),
        (
            "{ kind: cloud, url: \"https://api.telegram.org\" }",
            "{ max_connections: 50 }",
            100,
        ),
    ] {
        loads(&yaml(upstream, server, max));
    }
}

/// A plain-http upstream off loopback warns once, naming the upstream and
/// never the URL.
#[test]
fn http_upstream_warning() {
    let yaml = |url: &str| {
        format!(
            "version: 1\nupstreams:\n  local: {{ kind: local, url: \"{url}\" }}\nbots:\n  - name: a\n    token: \"1:t\"\n    upstream: local\n  - name: b\n    token: \"2:t\"\n    upstream: local\n"
        )
    };
    let text = yaml("http://telegram-bot-api:8081");
    let cfg = loads(&text);
    let [warning] = cfg.warnings.as_slice() else {
        panic!("{}", render(&cfg.warnings));
    };
    assert_eq!(warning.severity, Severity::Warning);
    assert_at(warning, "upstreams.local.url", pos(&text, "\"http://"));
    assert_message(warning, "`local`");
    assert!(
        !warning.to_string().contains("telegram-bot-api"),
        "{warning}"
    );
    let hint = warning.hint.as_deref().unwrap_or_default();
    assert!(hint.contains("Bot API server"), "{warning}");
    assert!(!hint.contains("kind: local"), "{warning}");

    for quiet in [
        "http://127.0.0.1:8081",
        "http://[::1]:8081",
        "http://localhost:8081",
        "https://telegram-bot-api:8443",
    ] {
        let cfg = loads(&yaml(quiet));
        assert!(
            cfg.warnings.is_empty(),
            "{quiet}: {}",
            render(&cfg.warnings)
        );
    }

    // The shorthand is an upstream too, a cloud one: a self-hosted server
    // behind it has its `file:` references refused, but no file routes.
    let text = "version: 1\nbots:\n  - name: a\n    token: \"1:t\"\n    api_url: \"http://bot-api:8081\"\n";
    let cfg = loads(text);
    let [warning] = cfg.warnings.as_slice() else {
        panic!("{}", render(&cfg.warnings));
    };
    assert_at(warning, "bots[0].api_url", pos(text, "\"http://"));
    assert_message(warning, "`api_url`");
    assert!(!warning.to_string().contains("bot-api"), "{warning}");
    let hint = warning.hint.as_deref().unwrap_or_default();
    assert!(
        hint.contains("upstreams: { <name>: { kind: local") && hint.contains("`getFile`"),
        "{warning}"
    );
    assert!(!hint.contains("only there"), "{warning}");
}

/// A `files.url` origin is asked for each bot's directory, named after its
/// token: plain http to a host that is not loopback warns as an upstream
/// URL does, naming the upstream and never the URL.
#[test]
fn http_files_url_warning() {
    let yaml = |files_url: &str| {
        format!(
            "version: 1\nupstreams:\n  local: {{ kind: local, url: \"http://127.0.0.1:8081\", files: {{ url: \"{files_url}\" }} }}\nbots:\n  - name: a\n    token: \"1:t\"\n    upstream: local\n"
        )
    };
    let text = yaml("http://files.internal/tba");
    let cfg = loads(&text);
    let [warning] = cfg.warnings.as_slice() else {
        panic!("{}", render(&cfg.warnings));
    };
    assert_eq!(warning.severity, Severity::Warning);
    assert_at(
        warning,
        "upstreams.local.files.url",
        pos(&text, "\"http://files"),
    );
    assert_message(warning, "`local`");
    assert!(!warning.to_string().contains("files.internal"), "{warning}");
    let hint = warning.hint.as_deref().unwrap_or_default();
    assert!(
        hint.contains("file server") && !hint.contains("Bot API server"),
        "{warning}"
    );
    for quiet in ["http://127.0.0.1:8090/tba", "https://files.internal/tba"] {
        let cfg = loads(&yaml(quiet));
        assert!(
            cfg.warnings.is_empty(),
            "{quiet}: {}",
            render(&cfg.warnings)
        );
    }
}

#[test]
fn response_timeout_defaults_and_bounds() {
    let yaml = |kind: &str, timeout: &str| {
        format!(
            "version: 1\nupstreams:\n  u: {{ kind: {kind}, url: \"https://127.0.0.1:8081\"{timeout} }}\nbots:\n  - name: a\n    token: \"1:t\"\n    upstream: u\n"
        )
    };
    let timeout = |text: &str| loads(text).bots[0].upstream.response_timeout;
    assert_eq!(timeout(&yaml("cloud", "")), secs(60));
    assert_eq!(timeout(&yaml("local", "")), secs(600));
    assert_eq!(timeout(&yaml("local", ", response_timeout: 2m")), secs(120));
    assert_eq!(
        timeout(&yaml("cloud", ", response_timeout: 1h")),
        secs(3600)
    );

    let text = yaml("local", ", response_timeout: 0s");
    let err = single_error(&text);
    assert_at(&err, "upstreams.u.response_timeout", pos(&text, "0s"));
    assert_message(&err, "longer than 0");
    let text = yaml("local", ", response_timeout: 61m");
    let err = single_error(&text);
    assert_at(&err, "upstreams.u.response_timeout", pos(&text, "61m"));
    assert_message(&err, "maximum of 1h");
}

/// Upstream URLs show as their origin only, like every other URL field;
/// one without a resolved counterpart renders as `[redacted]`.
#[test]
fn redacted_json_masks_upstream_urls() {
    let yaml = r#"version: 1
upstreams:
  local:
    kind: local
    url: "http://user:pa55word@bot-api.internal:8081/x?k=qu3ry"
    files: { url: "https://files.internal/s3cret-path?k=f1le-qu3ry" }
bots:
  - name: a
    token: "1:t"
    upstream: local
"#;
    let env = env(&[]);
    let cfg = config::load(yaml, Path::new("tgin.yaml"), &env)
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    for rendering in renderings(yaml, &env) {
        for leaked in ["pa55word", "qu3ry", "s3cret-path", "f1le-qu3ry"] {
            assert!(!rendering.contains(leaked), "{leaked} in {rendering}");
        }
    }
    let json = config::redacted_json(&cfg);
    let local = &json["upstreams"]["local"];
    assert_eq!(local["url"], "http://bot-api.internal:8081/…");
    assert_eq!(local["files"]["url"], "https://files.internal/…");
    assert_eq!(json["bots"][0]["upstream"], "local");
    assert!(json["bots"][0].get("api_url").is_none(), "{json}");

    let mut cfg = cfg;
    cfg.upstreams.clear();
    cfg.bots.clear();
    let json = config::redacted_json(&cfg);
    let local = &json["upstreams"]["local"];
    assert_eq!(local["url"], "[redacted]");
    assert_eq!(local["files"]["url"], "[redacted]");
    let text = json.to_string();
    for leaked in ["bot-api.internal", "files.internal"] {
        assert!(!text.contains(leaked), "{leaked} in {text}");
    }
}

/// Every config in the repository, as (path from the workspace root).
/// `tests/smoke/real_telegram.yaml` is a template with placeholders, not a
/// config.
const REPO_CONFIGS: [&str; 12] = [
    "tgin.yaml",
    "examples/simple/config/tgin.yaml",
    "examples/self-hosted/config/tgin.yaml",
    "tests/e2e/tgin.yaml",
    "tests/performance/cfgs/v1/longpoll-1.yaml",
    "tests/performance/cfgs/v1/longpoll-3.yaml",
    "tests/performance/cfgs/v1/webhook-1.yaml",
    "tests/performance/cfgs/v1/webhook-3.yaml",
    "crates/tgin-botapi/tests/fixtures/hot_path.yaml",
    "crates/tgin-core/tests/fixtures/design.yaml",
    "crates/tgin-core/tests/fixtures/full.yaml",
    "crates/tgin-core/tests/fixtures/minimal.yaml",
];

/// An environment that gives every `${env:…}` and `${file:…}` reference in
/// `text` a value of the shape its name suggests: a distinct bot token, a
/// URL or a header-safe secret.
fn stand_in_env(text: &str) -> MapEnv {
    let mut env = MapEnv::default();
    let mut offset = 0;
    let mut n = 0;
    while let Some(start) = text[offset..].find("${") {
        let start = offset + start + 2;
        let Some(end) = text[start..].find('}') else {
            break;
        };
        let reference = &text[start..start + end];
        offset = start + end;
        n += 1;
        if let Some(name) = reference.strip_prefix("env:") {
            let value = if name.ends_with("URL") {
                "https://api.telegram.org".to_owned()
            } else if name.ends_with("TOKEN") && !name.contains("ADMIN") {
                format!("{}:stand_in_{name}", 100_000 + n)
            } else {
                format!("stand_in_{n}")
            };
            env.vars.insert(name.to_owned(), value);
        } else if let Some(file) = reference.strip_prefix("file:") {
            env.files
                .insert(PathBuf::from(file), format!("stand_in_{n}\n"));
        }
    }
    env
}

/// A config without `accounts` loads as before: every config in the
/// repository still loads, with exactly the warnings it had, and its
/// `/v1/config` rendering gains no `accounts` or `render` key.
#[test]
fn v1_without_accounts_unchanged() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    const PLAIN_HTTP: &str = "sends bot tokens over plain http to a host that is not loopback";
    // The warnings each config had before accounts existed, with counts.
    let baseline: [(&str, &[(&str, usize)]); 6] = [
        (
            "examples/self-hosted/config/tgin.yaml",
            &[("upstream `tba`", 1)],
        ),
        (
            "tests/e2e/tgin.yaml",
            &[("upstream `fake-local`", 1), ("upstream `api_url`", 8)],
        ),
        (
            "tests/performance/cfgs/v1/longpoll-1.yaml",
            &[("upstream `api_url`", 1)],
        ),
        (
            "tests/performance/cfgs/v1/longpoll-3.yaml",
            &[("upstream `api_url`", 1)],
        ),
        (
            "tests/performance/cfgs/v1/webhook-1.yaml",
            &[("upstream `api_url`", 1)],
        ),
        (
            "tests/performance/cfgs/v1/webhook-3.yaml",
            &[("upstream `api_url`", 1)],
        ),
    ];
    for file in REPO_CONFIGS {
        let text =
            std::fs::read_to_string(root.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        // A relative origin, so that `${file:/…}` stays as written on
        // Windows too.
        let cfg = config::load(&text, Path::new(file), &stand_in_env(&text))
            .unwrap_or_else(|errors| panic!("{file}:\n{}", render(&errors)));
        let mut warnings: Vec<(String, usize)> = Vec::new();
        for warning in &cfg.warnings {
            match warnings.iter_mut().find(|(m, _)| *m == warning.message) {
                Some((_, n)) => *n += 1,
                None => warnings.push((warning.message.clone(), 1)),
            }
        }
        let expected: Vec<(String, usize)> = baseline
            .iter()
            .find(|(name, _)| *name == file)
            .map_or(&[][..], |(_, w)| *w)
            .iter()
            .map(|(upstream, n)| (format!("{upstream} {PLAIN_HTTP}"), *n))
            .collect();
        assert_eq!(warnings, expected, "{file}");
        let json = config::redacted_json(&cfg);
        assert!(json.get("accounts").is_none(), "{file}: {json}");
        for (name, consumer) in json["consumers"].as_object().into_iter().flatten() {
            let kind = consumer.as_object().unwrap().values().next().unwrap();
            assert!(kind.get("render").is_none(), "{file}: {name}: {json}");
        }
    }
}

/// One account, `main`, with one RPC client, `bot`, and `extra` spliced
/// into the account.
fn account_yaml(extra: &str) -> String {
    format!(
        r#"version: 1
server: {{}}
admin: {{ token: admin-t0ken-value }}
accounts:
  main:
    api_id: 12345
    api_hash: "${{env:API_HASH}}"
    session_dir: sessions/main
    phone: "+15550001234"
    rpc:
      clients:
        bot:
          token: c1ient-t0ken-value
          allow: ["messages.*"]
{extra}consumers:
  feed: {{ pull: {{ auth_token: pull-t0ken-value }} }}
"#
    )
}

fn api_env() -> MapEnv {
    env(&[("API_HASH", "0123456789abcdef0123456789abcdef")])
}

fn load_account(yaml: &str) -> Result<Config, Vec<ConfigError>> {
    config::load(yaml, Path::new("conf/tgin.yaml"), &api_env())
}

fn account_errors(yaml: &str) -> Vec<ConfigError> {
    match load_account(yaml) {
        Ok(_) => panic!("the config loaded:\n{yaml}"),
        Err(errors) => errors,
    }
}

#[test]
fn accounts_parse_and_redact() {
    let yaml = account_yaml(
        r#"    routes:
      - match: { constructors: [updateNewMessage], peer_ids: [-1001], user_ids: [7], commands: [start], text: { prefix: ["hi"] } }
        to: [feed]
      - to: [feed]
"#,
    );
    let cfg = load_account(&yaml).unwrap_or_else(|errors| panic!("{}", render(&errors)));
    assert!(cfg.warnings.is_empty(), "{}", render(&cfg.warnings));
    let [account] = &cfg.accounts[..] else {
        panic!("{:?}", cfg.accounts);
    };
    assert_eq!(account.name, "main");
    assert_eq!(account.api_id, 12345);
    assert_eq!(
        account.api_hash.expose(),
        "0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        account.phone.as_ref().map(|p| p.expose().as_str()),
        Some("+15550001234")
    );
    // Relative to the config file's directory, made absolute.
    let dir = account.session_dir.expose();
    assert!(dir.is_absolute(), "{}", dir.display());
    assert!(dir.ends_with("conf/sessions/main"), "{}", dir.display());
    assert_eq!(account.rpc.max_rps, nz(20));
    assert_eq!(account.rpc.flood_sleep_threshold, secs(10));
    let [client] = &account.rpc.clients[..] else {
        panic!("{:?}", account.rpc.clients);
    };
    assert_eq!(client.name, "bot");
    assert_eq!(client.token.expose(), "c1ient-t0ken-value");
    assert!(client.acl.allows("messages.sendMessage"));
    assert!(!client.acl.allows("channels.joinChannel"));
    // Account facts map onto the rule predicates.
    let [rule, _] = &account.rules[..] else {
        panic!("{:?}", account.rules);
    };
    assert_eq!(rule.predicates.update_types, ["updateNewMessage"]);
    assert_eq!(rule.predicates.chat_ids, [-1001]);
    assert_eq!(rule.predicates.user_ids, [7]);
    assert_eq!(rule.predicates.commands, ["start"]);
    assert_eq!(
        rule.predicates.text,
        Some(TextMatch::Prefix(vec!["hi".into()]))
    );
    assert_eq!(rule.targets, [TargetSpec::Consumer(ConsumerId(0))]);

    let raw: config::RawConfig = parse_yaml(&yaml).unwrap();
    for rendering in [
        format!("{cfg:?}"),
        config::redacted_json(&cfg).to_string(),
        format!("{raw:?}"),
    ] {
        for leaked in [
            "0123456789abcdef0123456789abcdef",
            "15550001234",
            "c1ient-t0ken-value",
            "pull-t0ken-value",
        ] {
            assert!(!rendering.contains(leaked), "{leaked} in {rendering}");
        }
    }
    let json = config::redacted_json(&cfg);
    let main = &json["accounts"]["main"];
    assert_eq!(main["api_hash"], "[redacted]");
    assert_eq!(main["phone"], "[redacted]");
    assert_eq!(main["rpc"]["clients"]["bot"]["token"], "[redacted]");
    assert_eq!(main["api_id"], 12345);
    assert_eq!(main["session_dir"], "sessions/main");
}

#[test]
fn account_fields_required_and_closed() {
    let yaml = "version: 1\naccounts:\n  main: { api_id: 1, api_hash: h }\n";
    let err = single_error(yaml);
    assert_message(&err, "session_dir");
    let yaml = "version: 1\naccounts:\n  main: { api_id: 1, api_hash: h, session_dir: s, layers: [230] }\n";
    let err = single_error(yaml);
    assert_message(&err, "layers");
    let yaml = "version: 1\naccounts:\n  main: { api_id: 0, api_hash: h, session_dir: s }\n";
    let err = single_error(yaml);
    assert_at(&err, "accounts.main.api_id", pos(yaml, "0,"));
    let yaml = "version: 1\naccounts:\n  Main: { api_id: 1, api_hash: h, session_dir: s, rpc: { max_rps: 0, clients: { Bad: { token: t } } } }\n";
    let errors = errors(yaml);
    let text = render(&errors);
    assert_eq!(errors.len(), 3, "{text}");
    assert!(text.contains("invalid name `Main`"), "{text}");
    assert!(text.contains("invalid name `Bad`"), "{text}");
    assert!(text.contains("accounts.Main.rpc.max_rps"), "{text}");
}

#[test]
fn session_dirs_unique_after_resolution() {
    let yaml = r#"version: 1
accounts:
  a: { api_id: 1, api_hash: shared-hash, session_dir: sessions/a }
  b: { api_id: 1, api_hash: shared-hash, session_dir: ./sessions/a/ }
"#;
    let err = single_error(yaml);
    assert_at(&err, "accounts.b.session_dir", pos(yaml, "./sessions/a/"));
    assert_message(&err, "account `a`");
    assert!(err.hint.is_some(), "{err}");

    // The developer's app pair is shared; only the directory must differ.
    let cfg = loads(
        r#"version: 1
admin: { token: admin-t0ken-value }
accounts:
  a: { api_id: 1, api_hash: shared-hash, session_dir: sessions/a }
  b: { api_id: 1, api_hash: shared-hash, session_dir: sessions/b }
"#,
    );
    assert_eq!(cfg.accounts.len(), 2);
    assert!(cfg.warnings.is_empty(), "{}", render(&cfg.warnings));
}

#[test]
fn account_name_not_a_bot_name() {
    let yaml = r#"version: 1
bots:
  - { name: main, token: "1:t" }
accounts:
  main: { api_id: 1, api_hash: h, session_dir: s }
"#;
    let err = single_error(yaml);
    assert_message(&err, "bot");
    assert!(
        err.at.as_ref().unwrap().path.starts_with("accounts.main"),
        "{err}"
    );
}

#[test]
fn rpc_client_tokens_unique_per_account() {
    let yaml = r#"version: 1
accounts:
  main:
    api_id: 1
    api_hash: h
    session_dir: s
    rpc: { clients: { a: { token: same-t0ken }, b: { token: same-t0ken } } }
"#;
    let err = single_error(yaml);
    assert_at(
        &err,
        "accounts.main.rpc.clients.b.token",
        pos(yaml, "same-t0ken } }"),
    );
    assert!(!err.to_string().contains("same-t0ken"), "{err}");
}

#[test]
fn rpc_glob_syntax() {
    for good in [
        "*",
        "messages.*",
        "messages.sendMessage",
        "ping",
        "invokeWithLayer",
    ] {
        let yaml = format!(
            "version: 1\naccounts:\n  main: {{ api_id: 1, api_hash: h, session_dir: s, rpc: {{ clients: {{ c: {{ token: t, allow: [\"{good}\"], deny: [\"{good}\"] }} }} }} }}\n"
        );
        loads(&yaml);
    }
    for bad in [
        "messages.send*",
        "*.sendMessage",
        "messages.",
        ".x",
        "a.b.c",
        "messages.**",
        "",
        "mess ages.x",
    ] {
        let yaml = format!(
            "version: 1\naccounts:\n  main: {{ api_id: 1, api_hash: h, session_dir: s, rpc: {{ clients: {{ c: {{ token: t, deny: [\"{bad}\"] }} }} }} }}\n"
        );
        let err = single_error(&yaml);
        assert_eq!(
            err.at.as_ref().unwrap().path,
            "accounts.main.rpc.clients.c.deny[0]",
            "{bad}: {err}"
        );
        assert!(err.hint.is_some(), "{err}");
    }
}

#[test]
fn auth_methods_need_exact_listing() {
    let yaml = r#"version: 1
admin: { token: admin-t0ken-value }
accounts:
  main:
    api_id: 1
    api_hash: h
    session_dir: s
    rpc:
      clients:
        wide: { token: t1, allow: ["*", "auth.*", "account.*"] }
        exact: { token: t2, allow: ["messages.*", "account.updateProfile", "auth.logOut"], deny: ["messages.deleteHistory"] }
"#;
    let cfg = loads(yaml);
    let clients = &cfg.accounts[0].rpc.clients;
    let wide = &clients[0].acl;
    assert!(wide.allows("messages.sendMessage"));
    assert!(wide.allows("channels.joinChannel"));
    for method in config::ACCOUNT_CHANGING {
        let method = method
            .strip_suffix('*')
            .map_or(method.to_owned(), |ns| format!("{ns}anything"));
        assert!(!wide.allows(&method), "{method}");
    }
    assert!(!wide.allows("auth.logOut"));
    assert!(!wide.allows("account.updateProfile"));
    assert!(!wide.allows("contacts.resetSaved"));
    let exact = &clients[1].acl;
    assert!(exact.allows("account.updateProfile"));
    assert!(exact.allows("auth.logOut"));
    assert!(!exact.allows("auth.resetAuthorizations"));
    assert!(!exact.allows("account.deleteAccount"));
    assert!(exact.allows("messages.sendMessage"));
    // Deny wins.
    assert!(!exact.allows("messages.deleteHistory"));
    // Matching is exact and case-sensitive.
    assert!(!exact.allows("Messages.sendMessage"));
    assert!(!exact.allows("messages"));
    assert!(!exact.allows("messages."));

    let text = render(&cfg.warnings);
    assert_eq!(cfg.warnings.len(), 2, "{text}");
    // A glob over protected methods allows none of them.
    let wide_warning = &cfg.warnings[0];
    assert_eq!(wide_warning.severity, Severity::Warning);
    assert_eq!(
        wide_warning.at.as_ref().unwrap().path,
        "accounts.main.rpc.clients.wide.allow[1]"
    );
    assert_message(wide_warning, "`auth.*`");
    assert!(wide_warning.hint.is_some(), "{wide_warning}");
    // Exact listings work, and are named.
    let exact_warning = cfg
        .warnings
        .iter()
        .find(|w| w.at.as_ref().unwrap().path.contains(".exact."))
        .unwrap_or_else(|| panic!("{text}"));
    assert_message(exact_warning, "account.updateProfile");
    assert_message(exact_warning, "auth.logOut");
    // `account.*` grants the read-only getters, so it is not reported as
    // useless, and the hint never says to remove a glob.
    assert!(
        !wide_warning.message.contains("account.*"),
        "{wide_warning}"
    );
    assert!(
        !wide_warning
            .hint
            .as_deref()
            .unwrap_or("")
            .contains("remove"),
        "{wide_warning}"
    );
}

#[test]
fn deny_by_default() {
    let cfg = loads(
        "version: 1\naccounts:\n  main: { api_id: 1, api_hash: h, session_dir: s, rpc: { clients: { c: { token: t } } } }\n",
    );
    let acl = &cfg.accounts[0].rpc.clients[0].acl;
    assert!(!acl.allows("messages.sendMessage"));
    assert!(!acl.allows("help.getConfig"));
}

#[test]
fn account_rules_validated() {
    let yaml = account_yaml(
        r#"    routes:
      - match: { commands: ["bad command"] }
        to: [nowhere]
      - match: { constructors: [updateNewMessage] }
        to: [feed]
"#,
    );
    let errors = account_errors(&yaml);
    let text = render(&errors);
    assert_eq!(errors.len(), 2, "{text}");
    assert_at(
        &errors[0],
        "accounts.main.routes[0].match.commands[0]",
        pos(&yaml, "\"bad command\""),
    );
    assert_at(
        &errors[1],
        "accounts.main.routes[0].to[0]",
        pos(&yaml, "nowhere"),
    );
    assert_message(&errors[1], "unknown consumer `nowhere`");

    let cfg = load_account(&yaml.replace(
        "      - match: { commands: [\"bad command\"] }\n        to: [nowhere]\n",
        "",
    ))
    .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    // The last rule has a match, as for a bot.
    assert_eq!(cfg.warnings.len(), 1, "{}", render(&cfg.warnings));
    assert_message(&cfg.warnings[0], "last rule has a match");
    assert_eq!(
        cfg.warnings[0].at.as_ref().unwrap().path,
        "accounts.main.routes"
    );

    // Bot facts are not account facts.
    let yaml =
        account_yaml("    routes:\n      - match: { chat_types: [private] }\n        to: [feed]\n");
    let err = &account_errors(&yaml)[0];
    assert_message(err, "chat_types");
}

#[test]
fn render_requires_account_sources() {
    let accounts = r#"accounts:
  main:
    api_id: 1
    api_hash: h
    session_dir: s
    routes: [{ to: [acct] }]
"#;
    let yaml = |render: &str, bot_to: &str| {
        format!(
            "version: 1\nserver: {{}}\nbots:\n  - {{ name: b, token: \"1:t\", routes: [{{ to: [{bot_to}] }}] }}\n{accounts}consumers:\n  acct: {{ pull: {{ auth_token: a{render} }} }}\n  botc: {{ push: {{ endpoints: [{{ url: \"http://x\" }}]{render} }} }}\n"
        )
    };
    // Account sources only: `tl_json` is allowed, and resolved.
    let text = yaml("", "botc").replace("auth_token: a", "auth_token: a, render: tl_json");
    let cfg = loads(&text);
    let acct = cfg.consumers.iter().find(|c| c.name == "acct").unwrap();
    assert_eq!(acct.render, config::Render::TlJson);
    let botc = cfg.consumers.iter().find(|c| c.name == "botc").unwrap();
    assert_eq!(botc.render, config::Render::Raw);
    assert_eq!(
        config::redacted_json(&cfg)["consumers"]["acct"]["pull"]["render"],
        "tl_json"
    );

    // A bot source with `tl_json`.
    let text = yaml(", render: tl_json", "botc")
        .replace("auth_token: a, render: tl_json", "auth_token: a");
    let err = single_error(&text);
    assert_at(&err, "consumers.botc.push.render", pos(&text, "tl_json"));
    assert_message(&err, "bot `b`");

    // Bot and account sources in one consumer, whatever the render.
    let text = yaml("", "acct");
    let err = single_error(&text);
    assert_message(&err, "consumer `acct`");
    assert_message(&err, "bot `b`");
    assert_message(&err, "account `main`");
}

/// Contacts that change the address book are protected; the read-only
/// `account.get*` methods of `ACCOUNT_READ_ONLY` are not, and the
/// credential and Passport getters stay protected.
#[test]
fn account_changing_list() {
    for method in [
        "contacts.deleteContacts",
        "contacts.importContacts",
        "contacts.addContact",
        "contacts.deleteByPhones",
        "contacts.acceptContact",
        "contacts.updateContactNote",
        "photos.uploadContactProfilePhoto",
        "account.getTmpPassword",
        "account.getPassword",
        "account.getPasswordSettings",
        "account.getAllSecureValues",
        "account.getSecureValue",
        "account.getAuthorizationForm",
        "account.getPasskeys",
        "account.getSomethingNew",
        "account.updateProfile",
    ] {
        assert!(config::account_changing(method), "{method}");
    }
    for method in config::ACCOUNT_READ_ONLY {
        assert!(method.starts_with("account.get"), "{method}");
        assert!(!config::account_changing(method), "{method}");
    }
    for method in [
        "account.getPrivacy",
        "account.getNotifySettings",
        "contacts.getContacts",
    ] {
        assert!(!config::account_changing(method), "{method}");
    }
    let cfg = loads(
        "version: 1\naccounts:\n  main: { api_id: 1, api_hash: h, session_dir: s, rpc: { clients: { c: { token: t, allow: [\"account.*\", \"contacts.*\", \"photos.*\"] } } } }\n",
    );
    let acl = &cfg.accounts[0].rpc.clients[0].acl;
    assert!(acl.allows("account.getPrivacy"));
    assert!(acl.allows("contacts.getContacts"));
    assert!(!acl.allows("contacts.addContact"));
    for method in [
        "contacts.deleteByPhones",
        "contacts.acceptContact",
        "contacts.updateContactNote",
    ] {
        assert!(!acl.allows(method), "{method}");
    }
    assert!(!acl.allows("account.getTmpPassword"));
    assert!(!acl.allows("photos.uploadContactProfilePhoto"));
    assert!(acl.allows("photos.getUserPhotos"));
}

/// An exactly listed account-changing method that `deny` blocks is not
/// named in the warning: the client cannot call it.
#[test]
fn account_changing_warning_skips_denied() {
    let cfg = loads(
        "version: 1\nadmin: { token: admin-t0ken-value }\naccounts:\n  main: { api_id: 1, api_hash: h, session_dir: s, rpc: { clients: { c: { token: t, allow: [auth.logOut, account.updateProfile], deny: [\"auth.*\"] }, d: { token: u, allow: [auth.logOut], deny: [\"*\"] } } } }\n",
    );
    let text = render(&cfg.warnings);
    assert_eq!(cfg.warnings.len(), 1, "{text}");
    assert_message(&cfg.warnings[0], "account.updateProfile");
    assert!(!text.contains("auth.logOut"), "{text}");
    assert_eq!(
        cfg.warnings[0].at.as_ref().unwrap().path,
        "accounts.main.rpc.clients.c.allow[1]"
    );
}

/// A pull consumer serves one account, as it serves one bot: two
/// accounts' update ids would share one offset.
#[test]
fn account_pull_consumer_single_account() {
    let yaml = r#"version: 1
server: {}
accounts:
  a: { api_id: 1, api_hash: h, session_dir: sa, routes: [{ to: [feed] }, { to: [feed] }] }
  b: { api_id: 1, api_hash: h, session_dir: sb, routes: [{ to: [other, feed] }] }
consumers:
  feed: { pull: { auth_token: t1 } }
  other: { pull: { auth_token: t2 } }
"#;
    let err = single_error(yaml);
    let (line, column) = pos(yaml, "other, feed");
    assert_at(&err, "accounts.b.routes[0].to[1]", (line, column + 7));
    assert_message(
        &err,
        "pull consumer `feed` is targeted by accounts `a` and `b`",
    );
    assert!(err.hint.is_some(), "{err}");
    // A push consumer may take several accounts.
    loads(&yaml.replace(
        "feed: { pull: { auth_token: t1 } }",
        "feed: { push: { endpoints: [{ url: \"http://x\" }] } }",
    ));
}

/// `accept_set_webhook` takes its endpoint from a bot's `setWebhook`, so a
/// push consumer that only accounts feed may not have it.
#[test]
fn accept_set_webhook_needs_a_bot_source() {
    let yaml = r#"version: 1
accounts:
  a: { api_id: 1, api_hash: h, session_dir: sa, routes: [{ to: [hooks] }] }
consumers:
  hooks: { push: { accept_set_webhook: true } }
"#;
    let err = single_error(yaml);
    assert_at(
        &err,
        "consumers.hooks.push.accept_set_webhook",
        pos(yaml, "{ push: { accept"),
    );
    assert_message(&err, "account `a`");
    assert!(err.hint.is_some(), "{err}");
}

/// `api_id` takes `${env:…}` and `${file:…}`, as the documented
/// account example writes it; it is checked once expanded, and is not a secret.
#[test]
fn api_id_interpolates() {
    let yaml = "version: 1\naccounts:\n  main: { api_id: \"${env:TG_API_ID}\", api_hash: h, session_dir: s }\n";
    let cfg = config::load(
        yaml,
        Path::new("tgin.yaml"),
        &env(&[("TG_API_ID", "424242")]),
    )
    .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    assert_eq!(cfg.accounts[0].api_id, 424242);
    assert_eq!(
        config::redacted_json(&cfg)["accounts"]["main"]["api_id"],
        "${env:TG_API_ID}"
    );
    for bad in ["0", "-3", "12ab", "99999999999"] {
        let errors = config::load(yaml, Path::new("tgin.yaml"), &env(&[("TG_API_ID", bad)]))
            .err()
            .unwrap_or_else(|| panic!("{bad} loaded"));
        assert_eq!(errors.len(), 1, "{}", render(&errors));
        assert_at(&errors[0], "accounts.main.api_id", pos(yaml, "\"${env"));
        assert_message(&errors[0], "must be between 1 and 2147483647");
        // The expanded value is never quoted.
        assert!(!errors[0].to_string().contains(bad), "{}", errors[0]);
    }
    let errors = config::load(yaml, Path::new("tgin.yaml"), &env(&[])).unwrap_err();
    assert_message(&errors[0], "TG_API_ID");
    let mut files = MapEnv::default();
    files
        .files
        .insert(PathBuf::from("/run/secrets/api-id"), "777\n".into());
    let cfg = config::load(
        &yaml.replace("${env:TG_API_ID}", "${file:/run/secrets/api-id}"),
        Path::new("tgin.yaml"),
        &files,
    )
    .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    assert_eq!(cfg.accounts[0].api_id, 777);
}

/// D16: a pull consumer an account feeds needs `auth_token`.
#[test]
fn account_pull_consumer_needs_auth_token() {
    let yaml = r#"version: 1
server: {}
accounts:
  a: { api_id: 1, api_hash: h, session_dir: sa, routes: [{ to: [feed] }] }
consumers:
  feed: { pull: {} }
"#;
    let err = single_error(yaml);
    assert_at(&err, "consumers.feed.pull.auth_token", pos(yaml, "{ pull"));
    assert_message(&err, "account `a`");
    assert!(err.hint.is_some(), "{err}");
}

fn two_sessions(a: &str, b: &str) -> String {
    format!(
        "version: 1\naccounts:\n  a: {{ api_id: 1, api_hash: h, session_dir: \"{a}\" }}\n  b: {{ api_id: 1, api_hash: h, session_dir: \"{b}\" }}\n"
    )
}

/// `..` names the directory it leads to, whether or not it exists yet.
#[test]
fn session_dir_dot_dot_collides() {
    let err = single_error(&two_sessions("sessions/a", "sessions/x/../a"));
    assert_message(&err, "account `a` already uses this session directory");
    let err = single_error(&two_sessions("sessions/a", "other/../sessions/./a"));
    assert_eq!(err.at.as_ref().unwrap().path, "accounts.b.session_dir");

    // An existing directory, reached through `..` from inside it.
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("session-dir-dot-dot");
    std::fs::create_dir_all(root.join("a/inner")).unwrap();
    let a = root.join("a");
    let b = root.join("a/inner/..");
    let yaml = two_sessions(
        &a.display().to_string().replace('\\', "/"),
        &b.display().to_string().replace('\\', "/"),
    );
    let err = single_error(&yaml);
    assert_message(&err, "already uses this session directory");
}

/// Windows and macOS volumes ignore case, so `Sessions/A` is `sessions/a`
/// there; Linux keeps them apart.
#[test]
fn session_dir_case_follows_the_platform() {
    let yaml = two_sessions("sessions/a", "Sessions/A");
    if cfg!(any(windows, target_os = "macos")) {
        let err = single_error(&yaml);
        assert_message(&err, "already uses this session directory");
    } else {
        loads(&yaml);
    }
}

/// A literal `api_id` out of range names the range.
#[test]
fn flood_sleep_threshold_at_most_a_minute() {
    let yaml = "version: 1\naccounts:\n  main: { api_id: 1, api_hash: h, session_dir: s, rpc: { flood_sleep_threshold: 61s } }\n";
    let err = single_error(yaml);
    assert_at(
        &err,
        "accounts.main.rpc.flood_sleep_threshold",
        pos(yaml, "61s"),
    );
    assert_message(&err, "`61s` is longer than the maximum of 1m");
    let ok = "version: 1\naccounts:\n  main: { api_id: 1, api_hash: h, session_dir: s, rpc: { flood_sleep_threshold: 60s } }\n";
    let account = &loads(ok).accounts[0];
    assert_eq!(account.rpc.flood_sleep_threshold, Duration::from_secs(60));
}

#[test]
fn api_id_out_of_range() {
    for bad in ["0", "-3", "99999999999"] {
        let yaml = format!(
            "version: 1\naccounts:\n  main: {{ api_id: {bad}, api_hash: h, session_dir: s }}\n"
        );
        let err = single_error(&yaml);
        assert_at(&err, "accounts.main.api_id", pos(&yaml, bad));
        assert_message(&err, "must be between 1 and 2147483647");
    }
}

/// The account routes need `admin.token`: accounts without one load, with
/// a warning at the first account that says so. With a token, no warning.
#[test]
fn accounts_without_admin_token_warn() {
    let without = account_yaml("").replace("admin: { token: admin-t0ken-value }\n", "");
    let cfg = load_account(&without).expect("loads");
    let [warning] = cfg.warnings.as_slice() else {
        panic!("{}", render(&cfg.warnings));
    };
    assert_eq!(warning.at.as_ref().unwrap().path, "accounts.main");
    assert_message(warning, "admin.token");
    let cfg = load_account(&account_yaml("")).expect("loads");
    assert!(cfg.warnings.is_empty(), "{}", render(&cfg.warnings));
}

/// A glob grants only methods of the reviewed namespaces that change
/// nothing account-wide; every other method needs an exact entry.
#[test]
fn glob_grants_only_reviewed_namespaces() {
    let cfg = loads(
        "version: 1\naccounts:\n  main: { api_id: 1, api_hash: h, session_dir: s, rpc: { clients: { c: { token: t, allow: [\"*\", \"messages.*\", \"payments.*\", \"wallet.*\", \"futurens.*\"] }, e: { token: u, allow: [payments.getStarsStatus, messages.sendPaidReaction, futurens.doThing] } } } }\n",
    );
    let glob = &cfg.accounts[0].rpc.clients[0].acl;
    let exact = &cfg.accounts[0].rpc.clients[1].acl;
    for method in [
        "messages.sendMessage",
        "channels.joinChannel",
        "upload.getFile",
        "users.getUsers",
    ] {
        assert!(!config::exact_only(method), "{method}");
        assert!(glob.allows(method), "{method}");
    }
    for method in [
        "messages.sendPaidReaction",
        "messages.setDefaultHistoryTTL",
        "messages.setDefaultReaction",
        "messages.requestWebView",
        "messages.requestSimpleWebView",
        "messages.requestAppWebView",
        "messages.requestMainWebView",
        "messages.requestChatJoinWebView",
        "messages.prolongWebView",
        "messages.checkUrlAuthMatchCode",
        "messages.acceptUrlAuth",
        "messages.requestUrlAuth",
        "messages.toggleBotInAttachMenu",
        "messages.updateDialogFilter",
        "messages.toggleSuggestedPostApproval",
        "channels.updatePaidMessagesPrice",
        "contacts.block",
        "contacts.setBlocked",
        "contacts.editCloseFriends",
        "stories.toggleAllStoriesHidden",
        "payments.getStarsStatus",
        "payments.sendStarsForm",
        "wallet.getWallets",
        "smsjobs.join",
        "premium.applyBoost",
        "bots.setBotInfo",
        "fragment.getCollectibleInfo",
        "auth.logOut",
        "futurens.doThing",
        "ping",
    ] {
        assert!(config::exact_only(method), "{method}");
        assert!(!glob.allows(method), "{method}");
    }
    assert!(exact.allows("payments.getStarsStatus"));
    assert!(exact.allows("messages.sendPaidReaction"));
    assert!(exact.allows("futurens.doThing"));
    assert!(!exact.allows("payments.sendStarsForm"));
    for namespace in [
        "payments", "wallet", "smsjobs", "premium", "bots", "fragment", "auth",
    ] {
        assert!(!config::glob_namespace(namespace), "{namespace}");
    }
}

/// `allow_paid_stars` lets a client's requests spend Stars on paid
/// messages; it is off unless set.
#[test]
fn allow_paid_stars_per_client() {
    let cfg = loads(
        "version: 1\naccounts:\n  main: { api_id: 1, api_hash: h, session_dir: s, rpc: { clients: { a: { token: t1 }, b: { token: t2, allow_paid_stars: true } } } }\n",
    );
    let clients = &cfg.accounts[0].rpc.clients;
    assert!(!clients[0].allow_paid_stars);
    assert!(clients[1].allow_paid_stars);
    let json = config::redacted_json(&cfg);
    assert_eq!(
        json["accounts"]["main"]["rpc"]["clients"]["b"]["allow_paid_stars"],
        true
    );
    assert_eq!(
        json["accounts"]["main"]["rpc"]["clients"]["a"]["allow_paid_stars"],
        false
    );
    let err = single_error(
        "version: 1\naccounts:\n  main: { api_id: 1, api_hash: h, session_dir: s, rpc: { clients: { a: { token: t1, allow_paid_stars: yes please } } } }\n",
    );
    assert!(
        err.at.as_ref().unwrap().path.contains("allow_paid_stars"),
        "{err}"
    );
}

/// The warnings follow the classes: a glob over a namespace no glob may
/// grant says so and names the namespaces one may; an exact listing of
/// any exact-only method is named, payments as much as auth.
#[test]
fn allowlist_warnings_follow_the_classes() {
    let cfg = loads(
        "version: 1\naccounts:\n  main: { api_id: 1, api_hash: h, session_dir: s, rpc: { clients: { c: { token: t, allow: [\"messages.*\", \"payments.*\", \"wallet.*\", payments.getStarsStatus, messages.sendPaidReaction] } } } }\n",
    );
    let warnings: Vec<&ConfigError> = cfg
        .warnings
        .iter()
        .filter(|w| w.at.as_ref().is_some_and(|at| at.path.contains(".rpc.")))
        .collect();
    assert_eq!(warnings.len(), 2, "{}", render(&cfg.warnings));
    let useless = warnings[0];
    assert_eq!(
        useless.at.as_ref().unwrap().path,
        "accounts.main.rpc.clients.c.allow[1]"
    );
    assert_message(useless, "`payments.*`, `wallet.*`");
    assert_message(useless, "grant nothing");
    assert!(!useless.message.contains("account-changing"), "{useless}");
    let hint = useless.hint.as_deref().unwrap_or_default();
    assert!(hint.contains("payments.<method>"), "{hint}");
    assert!(
        hint.contains("messages") && hint.contains("upload"),
        "{hint}"
    );
    let exact = warnings[1];
    assert_eq!(
        exact.at.as_ref().unwrap().path,
        "accounts.main.rpc.clients.c.allow[3]"
    );
    assert_message(exact, "payments.getStarsStatus, messages.sendPaidReaction");
    assert_message(exact, "only an exact entry grants");
    let hint = exact.hint.as_deref().unwrap_or_default();
    assert!(hint.contains("move assets"), "{hint}");
}

/// Declining a website login changes nothing, so a glob may grant it.
#[test]
fn decline_url_auth_stays_glob_grantable() {
    assert!(!config::exact_only("messages.declineUrlAuth"));
    assert!(config::exact_only("messages.requestUrlAuth"));
}

/// `contacts.getLocated` with `self_expires` publishes the account's
/// location, so only an exact entry grants it.
#[test]
fn get_located_needs_an_exact_entry() {
    assert!(config::exact_only("contacts.getLocated"));
    assert!(!config::exact_only("contacts.getContacts"));
}

/// Methods that give other users control of the owner's chats, and the
/// account-wide quick replies, need an exact entry.
#[test]
fn admin_rights_and_quick_replies_need_an_exact_entry() {
    for method in [
        "channels.editAdmin",
        "messages.editChatAdmin",
        "messages.editChatCreator",
        "messages.checkQuickReplyShortcut",
        "messages.deleteQuickReplyMessages",
        "messages.deleteQuickReplyShortcut",
        "messages.editQuickReplyShortcut",
        "messages.getQuickReplies",
        "messages.getQuickReplyMessages",
        "messages.reorderQuickReplies",
        "messages.sendQuickReplyMessages",
        "account.updateBusinessAwayMessage",
        "account.updateBusinessGreetingMessage",
    ] {
        assert!(config::exact_only(method), "{method}");
    }
    // Ordinary chat administration stays glob-grantable.
    assert!(!config::exact_only("channels.editBanned"));
    assert!(!config::exact_only("messages.editChatTitle"));
}
