//! Loading, validating and resolving `tgin.yaml` version 1.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tgin_core::config::{
    self, Balance, Config, ConsumerKind, Ingress, IpAllowlist, LogFormat, LogLevel, Pull, Push,
    WebhookSecret,
};
use tgin_core::ids::ConsumerId;
use tgin_core::rules::{SplitStrategy, TargetSpec, TextMatch};
use tgin_runtime::config::{ConfigError, Located, MapEnv, Severity, parse_yaml};
use tgin_runtime::queue::{OnFull, Ordering};

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

    // Without a `server` block there is no data plane.
    let cfg = loads("version: 1\n");
    assert!(cfg.server.is_none());
    assert!(cfg.bots.is_empty() && cfg.consumers.is_empty());
}

#[test]
fn sizes_and_durations_parse() {
    let cfg = loads(
        r#"
version: 1
server: {}
runtime: { worker_threads: 4, lame_duck: 1h 30m }
consumers:
  q: { pull: { capacity_bytes: 512KiB, lease_timeout: 2d } }
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
    assert_eq!(q.queue.capacity_bytes, 512 << 10);
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
        "/bot1",
        "/file/x",
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
    assert_message(&err, "expected https");
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
    token: t
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

/// The three renderings of a config: `Config`'s `Debug`, `redacted_json`
/// and `RawConfig`'s `Debug`.
fn renderings(yaml: &str, env: &MapEnv) -> [String; 3] {
    let cfg = config::load(yaml, Path::new("tgin.yaml"), env)
        .unwrap_or_else(|errors| panic!("{}", render(&errors)));
    let raw: config::RawConfig = parse_yaml(yaml, Path::new("tgin.yaml")).unwrap();
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
    assert_eq!(endpoints[0]["url"], "${env:PUSH_URL}");
    assert_eq!(
        endpoints[1]["url"],
        "https://api.telegram.org/bot#####/sendMessage"
    );
    assert_eq!(endpoints[2]["url"], "http://hooks.example/#####/x");
    let hook = &json["bots"][1]["ingress"]["webhook"];
    assert_eq!(hook["public_url"], "https://bot.example/hook?[redacted]");
    assert_eq!(hook["path"], "/hook/#####");
}

/// A `${…}` in the host must not stop the credentials around it from being
/// masked; the reference itself is shown as written.
#[test]
fn templated_urls_masked_as_written() {
    let yaml = r#"version: 1
server: {}
bots:
  - name: a
    token: "1:t"
    api_url: "https://api.telegram.org/bot${env:TOKEN}/"
    ingress: { webhook: { public_url: "https://${env:HOOK_HOST}/hook?token=literal-query-secret", register: true } }
consumers:
  p: { push: { endpoints: [{ url: "http://admin:pa55w0rd-literal@${env:HOST}:8080/hook" }] } }
"#;
    let env = env(&[
        ("TOKEN", "123456789:AAbbccddeeffgghhiijjkkllmmnnoopp_qq"),
        ("HOOK_HOST", "bot.example"),
        ("HOST", "push.internal"),
    ]);
    let [debug, json, raw] = renderings(yaml, &env);
    for rendering in [&debug, &json, &raw] {
        for leaked in [
            "literal-query-secret",
            "pa55w0rd-literal",
            "AAbbccddeeffgghhiijjkkllmmnnoopp_qq",
            "push.internal",
        ] {
            assert!(!rendering.contains(leaked), "{leaked} in {rendering}");
        }
    }
    for written in [
        "https://api.telegram.org/bot${env:TOKEN}/",
        "https://${env:HOOK_HOST}/hook?[redacted]",
        "http://[redacted]@${env:HOST}:8080/hook",
    ] {
        assert!(debug.contains(written), "{written} not in {debug}");
        assert!(json.contains(written), "{written} not in {json}");
    }
    // `RawConfig` cannot know the config's secrets, so it shows no
    // URL or path text at all.
    assert!(raw.contains("Template(..)"), "{raw}");
    assert!(!raw.contains("${env:"), "{raw}");
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
    let pattern = schema["$defs"]["Duration"]["pattern"].as_str().unwrap();
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
    assert_eq!(
        loads("version: 1\nreload: { drain_timeout: \"0\" }\n")
            .reload
            .drain_timeout,
        Duration::ZERO
    );
}
