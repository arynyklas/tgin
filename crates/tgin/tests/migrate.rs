//! `tgin config migrate`: tgin 0.2 RON configs to YAML v1.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tgin::migrate::{Migration, migrate};
use tgin_core::config::{self, Config, ConsumerKind, Ingress, LogFormat, LogLevel, WebhookSecret};
use tgin_runtime::config::MapEnv;
use tgin_runtime::queue::OnFull;

const TOKEN: &str = "123456789:AAAAexampleSecretTokenValue_-xyz";
/// The secret half of [`TOKEN`]; must never reach stderr.
const TOKEN_SECRET: &str = "AAAAexampleSecretTokenValue_-xyz";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/migrate_fixtures")
}

/// The shipped 0.2 configs, by fixture name.
const SHIPPED: [&str; 6] = [
    "tgin",
    "examples-simple",
    "longpull-one",
    "longpull",
    "webhook-one",
    "webhook",
];

fn read(name: &str) -> String {
    let path = fixtures().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn migrated(ron: &str) -> Migration {
    migrate(ron).unwrap_or_else(|errors| panic!("not migrated:\n{}", errors.join("\n")))
}

fn errors(ron: &str) -> Vec<String> {
    match migrate(ron) {
        Ok(m) => panic!("migrated:\n{}", m.yaml),
        Err(errors) => errors,
    }
}

fn golden(name: &str) {
    let out = migrated(&read(&format!("{name}.ron")));
    let expected = read(&format!("{name}.yaml"));
    assert_eq!(
        out.yaml, expected,
        "{name}: migrated YAML differs from the golden file"
    );
}

#[test]
fn migrate_tgin() {
    golden("tgin");
}

#[test]
fn migrate_examples_simple() {
    golden("examples-simple");
}

#[test]
fn migrate_longpull_one() {
    golden("longpull-one");
}

#[test]
fn migrate_longpull() {
    golden("longpull");
}

#[test]
fn migrate_webhook_one() {
    golden("webhook-one");
}

#[test]
fn migrate_webhook() {
    golden("webhook");
}

/// Every `${env:NAME}` in `yaml`.
fn env_references(yaml: &str) -> BTreeSet<String> {
    yaml.split("${env:")
        .skip(1)
        .filter_map(|rest| rest.split_once('}').map(|(name, _)| name.to_owned()))
        .collect()
}

/// Values for the variables the migrated configs reference.
fn env_for(yaml: &str) -> MapEnv {
    let known = [
        ("TOKEN", TOKEN),
        ("TGIN_AUTH_TOKEN", "test-admin-token"),
        ("WH_ENDPOINT", "http://127.0.0.1:3001/bot"),
        (
            "TGIN_BOT_1_TOKEN",
            "987654321:BBBBanotherSecretTokenValue_-xyz",
        ),
        (
            "TGIN_BOT_2_TOKEN",
            "987654322:CCCCanotherSecretTokenValue_-xyz",
        ),
        ("HOOK_SECRET", "hook-secret-value"),
        ("ADMIN", "admin-token-value"),
        ("API", "http://127.0.0.1:8081"),
    ];
    let mut env = MapEnv::default();
    for name in env_references(yaml) {
        let value = known
            .iter()
            .find(|(known, _)| *known == name)
            .unwrap_or_else(|| panic!("no test value for ${{env:{name}}}"))
            .1;
        env.vars.insert(name, value.to_owned());
    }
    env
}

fn load(yaml: &str) -> Config {
    config::load(yaml, Path::new("tgin.yaml"), &env_for(yaml)).unwrap_or_else(|errors| {
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        panic!("{}\n---\n{yaml}", errors.join("\n"))
    })
}

#[test]
fn migrated_output_validates() {
    for name in SHIPPED {
        let yaml = read(&format!("{name}.yaml"));
        let cfg = load(&yaml);
        // No v1 warning is expected for any shipped config.
        assert!(cfg.warnings.is_empty(), "{name}: {:?}", cfg.warnings);
        assert_eq!(cfg.bots.len(), 1, "{name}");
    }
}

/// 0.2 dropped what a failing downstream did not take and never paused the
/// source; with v1's `backpressure`, one dead push endpoint would stall
/// every other consumer of its bot.
#[test]
fn migrated_push_consumers_never_backpressure() {
    let mut pushes = 0;
    for name in SHIPPED {
        let cfg = load(&read(&format!("{name}.yaml")));
        for consumer in &cfg.consumers {
            if let ConsumerKind::Push(push) = &consumer.kind {
                pushes += 1;
                assert_eq!(
                    push.on_full,
                    OnFull::DropOldest,
                    "{name}: {}",
                    consumer.name
                );
                assert_eq!(push.retry.max_attempts, 1, "{name}: {}", consumer.name);
            }
        }
    }
    assert_eq!(pushes, 4, "push consumers in the shipped configs");
}

#[test]
fn multi_bot_with_pull_is_error() {
    let ron = r#"(
        server_port: Some(3000),
        updates: [
            LongPollUpdate(token: "1:a"),
            LongPollUpdate(token: "2:b"),
        ],
        route: AllLB(routes: [
            WebhookRoute(url: "http://a.example/hook"),
            LongPollRoute(path: "/bot/getUpdates"),
        ]),
    )"#;
    let errors = errors(ron);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("2 bots"), "{}", errors[0]);
    assert!(
        errors[0].contains("a pull consumer serves exactly one bot"),
        "{}",
        errors[0]
    );

    // Push consumers may serve several bots: both bots route to it.
    let ron = r#"(
        updates: [
            LongPollUpdate(token: "1:a"),
            LongPollUpdate(token: "2:b"),
        ],
        route: WebhookRoute(url: "http://a.example/hook"),
    )"#;
    let out = migrated(ron);
    let cfg = load(&out.yaml);
    assert_eq!(cfg.bots.len(), 2);
    assert_eq!(cfg.bots[0].name, "bot-1");
    assert_eq!(cfg.bots[1].name, "bot-2");
    assert_eq!(cfg.consumers.len(), 1);
    assert_eq!(cfg.consumers[0].name, "push-a-example");
    for bot in &cfg.bots {
        assert_eq!(bot.rules.len(), 1, "{}: one catch-all rule", bot.name);
    }
}

#[test]
fn custom_longpoll_url_is_error() {
    for url in [
        // Not a getUpdates URL at all.
        "http://proxy.internal:8080/custom/poll",
        // A getUpdates URL for another token than the bot's.
        "http://proxy.internal:8080/bot999999999:ZZZZotherSecretTokenValue_-xyz/getUpdates",
        // A query string: v1 cannot express it.
        "http://proxy.internal:8080/bot1:a/getUpdates?timeout=5",
    ] {
        let ron = format!(
            r#"(updates: [LongPollUpdate(token: "1:a", url: Some("{url}"))], route: WebhookRoute(url: "http://a/")) "#
        );
        let errors = errors(&ron);
        assert_eq!(errors.len(), 1, "{url}: {errors:?}");
        assert!(errors[0].contains("updates[0].url"), "{}", errors[0]);
        assert!(errors[0].contains("api_url"), "{}", errors[0]);
        assert!(!errors[0].contains("proxy.internal"), "{}", errors[0]);
    }

    // The bot's own token, with a custom host, becomes `api_url`.
    let ron = r#"(updates: [LongPollUpdate(token: "${TOKEN}", url: Some("http://bot-api.internal:8081/bot${TOKEN}/getUpdates"))], route: WebhookRoute(url: "http://a/"))"#;
    let cfg = load(&migrated(ron).yaml);
    assert_eq!(
        cfg.bots[0].api_url.expose().as_str(),
        "http://bot-api.internal:8081/"
    );
    assert_eq!(cfg.bots[0].token.expose(), TOKEN);
}

#[test]
fn webhook_without_token_gets_placeholder_and_warning() {
    let ron = r#"(
        server_port: Some(3000),
        updates: [
            LongPollUpdate(token: "1:a"),
            WebhookUpdate(path: "/hook"),
        ],
        route: WebhookRoute(url: "http://a.example/hook"),
    )"#;
    let out = migrated(ron);
    assert!(
        out.yaml.contains("token: \"${env:TGIN_BOT_2_TOKEN}\""),
        "{}",
        out.yaml
    );
    assert_eq!(out.warnings.len(), 2, "{:?}", out.warnings);
    assert!(
        out.warnings[0].contains("updates[1]") && out.warnings[0].contains("TGIN_BOT_2_TOKEN"),
        "{}",
        out.warnings[0]
    );
    // 0.2 accepted any post to a webhook without a secret; v1 keeps that,
    // with a warning that names neither the path nor a token.
    assert!(
        out.warnings[1].contains("bot-2") && out.warnings[1].contains("no secret token"),
        "{}",
        out.warnings[1]
    );
    assert!(!out.warnings[1].contains("/hook"), "{}", out.warnings[1]);
    // The warning heads the file too.
    assert!(
        out.yaml
            .lines()
            .any(|line| line.starts_with('#') && line.contains("TGIN_BOT_2_TOKEN")),
        "{}",
        out.yaml
    );

    let cfg = load(&out.yaml);
    let hook = &cfg.bots[1];
    assert_eq!(
        hook.token.expose(),
        "987654322:CCCCanotherSecretTokenValue_-xyz"
    );
    let Some(Ingress::Webhook(webhook)) = &hook.ingress else {
        panic!("{:?}", hook.ingress);
    };
    assert_eq!(webhook.path.expose(), "/hook");
    assert!(!webhook.register);
    // 0.2 checked no secret without `secret_token`; `auto` would reject
    // Telegram's posts to a webhook registered without one.
    assert_eq!(webhook.secret_token, WebhookSecret::None);
}

#[test]
fn webhook_registration_supplies_token_url_and_secret() {
    let ron = r#"(
        server_port: Some(8443),
        updates: [WebhookUpdate(
            path: "/hook",
            secret_token: Some("${HOOK_SECRET}"),
            registration: Some((
                public_ip: "https://bot.example.com",
                set_webhook_url: Some("http://bot-api.internal:8081/bot42:local-token/setWebhook"),
                token: "42:local-token",
            )),
        )],
        route: WebhookRoute(url: "http://a.example/hook"),
    )"#;
    let out = migrated(ron);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let cfg = load(&out.yaml);
    let bot = &cfg.bots[0];
    assert_eq!(bot.token.expose(), "42:local-token");
    assert_eq!(
        bot.api_url.expose().as_str(),
        "http://bot-api.internal:8081/"
    );
    let Some(Ingress::Webhook(hook)) = &bot.ingress else {
        panic!("{:?}", bot.ingress);
    };
    assert!(hook.register);
    assert_eq!(
        hook.public_url.as_ref().map(|url| url.expose().as_str()),
        Some("https://bot.example.com/")
    );
    let WebhookSecret::Token(secret) = &hook.secret_token else {
        panic!("{:?}", hook.secret_token);
    };
    assert_eq!(secret.expose(), "hook-secret-value");

    // Without `set_webhook_url`, the registration's token is the bot's.
    let ron = r#"(server_port: Some(8443), updates: [WebhookUpdate(path: "/hook", secret_token: Some("literal-secret"), registration: Some((public_ip: "https://bot.example.com", token: "${TOKEN}")))], route: WebhookRoute(url: "http://a/"))"#;
    let out = migrated(ron);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let cfg = load(&out.yaml);
    assert_eq!(cfg.bots[0].token.expose(), TOKEN);
    assert_eq!(
        cfg.bots[0].api_url.expose().as_str(),
        "https://api.telegram.org/"
    );
}

/// Placeholders become `${env:…}`; literal `${` text stays literal.
#[test]
fn placeholders_become_env_references() {
    let ron = r#"(
        auth_token: Some("${ADMIN}"),
        server_port: Some(3000),
        updates: [LongPollUpdate(token: "1:a${not-a-placeholder}")],
        route: LongPollRoute(path: "/poll/${TOKEN}"),
    )"#;
    let out = migrated(ron);
    let cfg = load(&out.yaml);
    assert_eq!(
        cfg.admin
            .token
            .as_ref()
            .map(|token| token.expose().as_str()),
        Some("admin-token-value")
    );
    assert_eq!(cfg.bots[0].token.expose(), "1:a${not-a-placeholder}");
    let ConsumerKind::Pull(pull) = &cfg.consumers[0].kind else {
        panic!("{:?}", cfg.consumers[0].kind);
    };
    let aliases: Vec<&str> = pull.aliases.iter().map(|a| a.expose().as_str()).collect();
    assert_eq!(aliases, [format!("/poll/{TOKEN}")]);
    // A path with a placeholder has nothing to name the consumer by.
    assert_eq!(cfg.consumers[0].name, "pull-1");
}

#[test]
fn placeholder_outside_a_string_is_error() {
    let ron = "(\n    server_port: Some(${PORT}),\n    updates: [],\n    // ${IN_A_COMMENT} is fine\n    route: WebhookRoute(url: \"http://a/${HOST}\"),\n)";
    let errors = errors(ron);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("PORT"), "{}", errors[0]);
    assert!(errors[0].contains("line 2, column 23"), "{}", errors[0]);
    assert!(errors[0].contains("literal"), "{}", errors[0]);
}

#[test]
fn consumer_names_are_slugged_and_unique() {
    let long = format!("{0}.{0}.example", "x".repeat(40));
    let ron = format!(
        r#"(
        server_port: Some(3000),
        updates: [LongPollUpdate(token: "1:a")],
        route: AllLB(routes: [
            LongPollRoute(path: "/Bot1/getUpdates"),
            LongPollRoute(path: "/bot1//getupdates"),
            LongPollRoute(path: "/poll/bot{TOKEN}"),
            WebhookRoute(url: "http://Hook.Example:1/x"),
            WebhookRoute(url: "http://hook.example:1/y"),
            WebhookRoute(url: "http://hook.example/x"),
            WebhookRoute(url: "https://hook.example:443/y"),
            RoundRobinLB(routes: [WebhookRoute(url: "http://{long}/")]),
        ]),
    )"#
    );
    let out = migrated(&ron);
    let cfg = load(&out.yaml);
    let names: Vec<&str> = cfg.consumers.iter().map(|c| c.name.as_str()).collect();
    let truncated = format!("push-{}", "x".repeat(40) + "-" + &"x".repeat(17));
    assert_eq!(
        names,
        [
            "pull-bot1-getupdates",
            "pull-bot1-getupdates-2",
            // A token-shaped path never becomes part of a name.
            "pull-3",
            // Push consumers are named by host and explicit port only.
            "push-hook-example-1",
            "push-hook-example-1-2",
            "push-hook-example",
            // 443 is https's default port, so the URL names no port.
            "push-hook-example-2",
            truncated.as_str(),
        ]
    );
    for name in &names {
        assert!(!name.contains("123456789"), "{name}");
    }
}

/// 0.2 sent no secret header, so a secret path segment often authenticated
/// a push downstream; only the endpoint `url` may carry it.
#[test]
fn push_url_path_appears_only_in_the_endpoint_url() {
    let ron = r#"(
        updates: [LongPollUpdate(token: "1:a")],
        route: AllLB(routes: [
            WebhookRoute(url: "http://hook.example:8080/hook/s3cretpath"),
            WebhookRoute(url: "http://hook.example:8080/other/s3cretpath2"),
        ]),
    )"#;
    let out = migrated(ron);
    let lines: Vec<&str> = out
        .yaml
        .lines()
        .filter(|line| line.contains("s3cretpath"))
        .collect();
    assert_eq!(
        lines,
        [
            r#"        - url: "http://hook.example:8080/hook/s3cretpath""#,
            r#"        - url: "http://hook.example:8080/other/s3cretpath2""#,
        ],
        "{}",
        out.yaml
    );
    assert!(
        !out.warnings.iter().any(|w| w.contains("s3cret")),
        "{:?}",
        out.warnings
    );
    let cfg = load(&out.yaml);
    let names: Vec<&str> = cfg.consumers.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["push-hook-example-8080", "push-hook-example-8080-2"]
    );
}

/// The branches no shipped config reaches: TLS, log settings, long-poll
/// parameters and dropped sleeps.
#[test]
fn settings_without_a_golden_map_and_warn() {
    let ron = r#"(
        server_port: Some(8443),
        ssl: Some((cert: "certs/tls.crt", key: "/etc/tls/tls.key")),
        log_level: "OFF",
        log_format: Json,
        updates: [
            LongPollUpdate(token: "1:a", long_poll_timeout: 0, long_poll_limit: 0, default_timeout_sleep: 100),
            LongPollUpdate(token: "2:b", long_poll_timeout: 90, long_poll_limit: 500, default_timeout_sleep: 7, error_timeout_sleep: 5),
            LongPollUpdate(token: "3:c", long_poll_timeout: 10, long_poll_limit: 20),
        ],
        route: WebhookRoute(url: "http://a.example/"),
    )"#;
    let out = migrated(ron);
    let w = &out.warnings;
    assert_eq!(w.len(), 5, "{w:?}");
    assert!(w[0].contains("`log_level` off"), "{}", w[0]);
    assert!(
        w[1].starts_with("`ssl.cert` is a relative path") && !w[1].contains("ssl.key"),
        "{}",
        w[1]
    );
    assert!(!w[1].contains("certs/tls.crt"), "{}", w[1]);
    assert!(
        w[2].starts_with("updates[0]: `default_timeout_sleep` was dropped"),
        "{}",
        w[2]
    );
    assert!(
        w[3].starts_with("updates[0]: `long_poll_timeout: 0` is now 1s"),
        "{}",
        w[3]
    );
    assert!(
        w[4].starts_with(
            "updates[1]: `default_timeout_sleep` and `error_timeout_sleep` were dropped"
        ),
        "{}",
        w[4]
    );

    let cfg = config::load(
        &out.yaml,
        &Path::new("/etc/tgin").join("tgin.yaml"),
        &MapEnv::default(),
    )
    .unwrap_or_else(|errors| panic!("{errors:?}\n{}", out.yaml));
    assert_eq!(cfg.log.level, LogLevel::Error);
    assert_eq!(cfg.log.format, LogFormat::Json);
    let tls = cfg
        .server
        .as_ref()
        .and_then(|s| s.tls.as_ref())
        .expect("tls");
    assert_eq!(
        tls.cert_file.expose(),
        &Path::new("/etc/tgin").join("certs/tls.crt")
    );
    assert_eq!(tls.key_file.expose(), Path::new("/etc/tls/tls.key"));
    let polls: Vec<(u64, u32)> = cfg
        .bots
        .iter()
        .map(|bot| match &bot.ingress {
            Some(Ingress::LongPoll(poll)) => (poll.timeout.as_secs(), poll.limit),
            other => panic!("{other:?}"),
        })
        .collect();
    // 0.2 capped at Telegram's limits (50 s, 1 to 100); v1 rejects beyond.
    assert_eq!(polls, [(1, 1), (50, 100), (10, 20)]);
}

#[test]
fn unconvertible_webhook_and_placeholder_values_are_errors() {
    // A setWebhook URL tgin 1.0 cannot rebuild from `api_url`.
    let ron = r#"(server_port: Some(1), updates: [WebhookUpdate(path: "/hook", registration: Some((public_ip: "https://bot.example", set_webhook_url: Some("http://bot-api.internal/custom/setWebhook"), token: "1:a")))], route: WebhookRoute(url: "http://a/"))"#;
    let found = errors(ron);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].starts_with("updates[0].registration.set_webhook_url:"),
        "{}",
        found[0]
    );
    assert!(!found[0].contains("bot-api.internal"), "{}", found[0]);

    // `auto` and `none` are v1 keywords, not secrets.
    for secret in ["auto", "none"] {
        let ron = format!(
            r#"(server_port: Some(1), updates: [WebhookUpdate(path: "/hook", secret_token: Some("{secret}"))], route: WebhookRoute(url: "http://a/"))"#
        );
        let errors = errors(&ron);
        assert_eq!(errors.len(), 1, "{secret}: {errors:?}");
        assert!(
            errors[0].starts_with("updates[0].secret_token: `auto` and `none` are keywords"),
            "{}",
            errors[0]
        );
    }

    // 0.2 expanded `$${TOKEN}` to `$` + the value; in v1 `$${` is an escape.
    let ron =
        r#"(updates: [LongPollUpdate(token: "$${TOKEN}")], route: WebhookRoute(url: "http://a/"))"#;
    let found = errors(ron);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].starts_with("updates[0].token: a `$` right before a placeholder"),
        "{}",
        found[0]
    );
}

#[test]
fn migration_debug_hides_the_yaml() {
    let out = migrated(&read("longpull.ron"));
    let debug = format!("{out:?}");
    assert!(!debug.contains("123:test"), "{debug}");
    assert!(!debug.contains("bench"), "{debug}");
    assert!(debug.contains("api"), "warnings are shown: {debug}");
}

#[test]
fn trees_map_to_split_and_all() {
    let ron = r#"(
        server_port: Some(3000),
        updates: [LongPollUpdate(token: "1:a")],
        route: AllLB(routes: [
            RoundRobinLB(routes: [
                WebhookRoute(url: "http://a.example/", request_timeout_ms: 2500),
                WebhookRoute(url: "http://b.example/"),
            ]),
            LongPollRoute(path: "/log/getUpdates", max_buffered_updates: 50),
        ]),
    )"#;
    let out = migrated(ron);
    let cfg = load(&out.yaml);
    let ConsumerKind::Push(a) = &cfg.consumers[0].kind else {
        panic!("{:?}", cfg.consumers[0].kind);
    };
    assert_eq!(
        a.endpoints[0].timeout,
        std::time::Duration::from_millis(2500)
    );
    assert_eq!(a.retry.max_attempts, 1);
    // 0.2 never paused the source for a failing downstream.
    assert_eq!(a.on_full, OnFull::DropOldest);
    let ConsumerKind::Push(b) = &cfg.consumers[1].kind else {
        panic!("{:?}", cfg.consumers[1].kind);
    };
    assert_eq!(b.endpoints[0].timeout, std::time::Duration::from_secs(10));
    assert_eq!((b.on_full, b.retry.max_attempts), (OnFull::DropOldest, 1));
    let ConsumerKind::Pull(log) = &cfg.consumers[2].kind else {
        panic!("{:?}", cfg.consumers[2].kind);
    };
    assert_eq!(log.queue.capacity, 50);
    assert_eq!(log.queue.on_full, OnFull::DropOldest);
    assert!(
        out.yaml.contains(
            "      - to:\n          - all:\n              - split:\n                  strategy: round_robin\n"
        ),
        "{}",
        out.yaml
    );
}

#[test]
fn empty_load_balancer_is_error() {
    let ron = r#"(updates: [], route: AllLB(routes: [WebhookRoute(url: "http://a/"), RoundRobinLB(routes: [])]))"#;
    let errors = errors(ron);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("route.routes[1]"), "{}", errors[0]);
}

// --- The command line ------------------------------------------------------

fn tgin(args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_tgin"));
    command.args(args).env_remove("TGIN_CONFIG");
    command
}

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn output(command: &mut std::process::Command) -> Output {
    let out = command.output().expect("tgin runs");
    Output {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A scratch directory of its own for `test`.
fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

#[test]
fn cli_exit_codes_and_out() {
    let dir = scratch("migrate_cli_exit_codes_and_out");
    let fixture = fixtures().join("webhook.ron");
    let fixture = fixture.to_str().unwrap();
    let golden = read("webhook.yaml");

    // Warnings: exit 3, YAML on stdout, warnings on stderr.
    let out = output(&mut tgin(&["config", "migrate", fixture]));
    assert_eq!(out.code, Some(3), "{}", out.stderr);
    assert_eq!(out.stdout, golden);
    assert!(
        out.stderr.contains("warning: `api` was dropped"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("TGIN_BOT_1_TOKEN"), "{}", out.stderr);

    // `-o` and `--out` write the file instead.
    for flag in ["-o", "--out"] {
        let file = dir.join(format!("out{flag}.yaml"));
        let _ = std::fs::remove_file(&file);
        let out = output(&mut tgin(&[
            "config",
            "migrate",
            fixture,
            flag,
            file.to_str().unwrap(),
        ]));
        assert_eq!(out.code, Some(3), "{flag}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{flag}");
        assert!(out.stderr.contains("warning: "), "{flag}: {}", out.stderr);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), golden, "{flag}");
    }

    // No warnings: exit 0.
    let clean = dir.join("clean.ron");
    std::fs::write(
        &clean,
        r#"(updates: [LongPollUpdate(token: "${TOKEN}")], route: WebhookRoute(url: "http://a.example/"))"#,
    )
    .unwrap();
    let out = output(&mut tgin(&["config", "migrate", clean.to_str().unwrap()]));
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(out.stderr, "");
    load(&out.stdout);

    // Errors: exit 1, nothing on stdout.
    let multi = dir.join("multi.ron");
    std::fs::write(
        &multi,
        r#"(server_port: Some(1), updates: [LongPollUpdate(token: "1:a"), LongPollUpdate(token: "2:b")], route: LongPollRoute(path: "/p"))"#,
    )
    .unwrap();
    let out = output(&mut tgin(&["config", "migrate", multi.to_str().unwrap()]));
    assert_eq!(out.code, Some(1), "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert!(out.stderr.contains("error: "), "{}", out.stderr);
    let missing = dir.join("missing.ron");
    let out = output(&mut tgin(&["config", "migrate", missing.to_str().unwrap()]));
    assert_eq!(out.code, Some(1), "{}", out.stderr);

    // Usage errors: exit 64.
    for args in [
        ["config", "migrate"].as_slice(),
        &["config", "migrate", fixture, "--out"],
        &["config", "migrate", fixture, "--bogus"],
    ] {
        let out = output(&mut tgin(args));
        assert_eq!(out.code, Some(64), "{args:?}: {}", out.stderr);
    }
}

/// The legacy `-f` hint names a command that exists.
#[test]
fn legacy_hint_command_works() {
    let dir = scratch("migrate_legacy_hint_command_works");
    let fixture = fixtures().join("tgin.ron");
    let out = output(tgin(&["-f", fixture.to_str().unwrap()]).current_dir(&dir));
    let hint = out
        .stderr
        .split("convert with: ")
        .nth(1)
        .expect("a migrate hint")
        .trim()
        .to_owned();
    let args: Vec<&str> = hint.split(' ').skip(1).collect();
    assert_eq!(args[..2], ["config", "migrate"], "{hint}");
    let out = output(tgin(&args).current_dir(&dir));
    assert_eq!(out.code, Some(3), "{}", out.stderr);
    let written = std::fs::read_to_string(dir.join("tgin.yaml")).unwrap();
    assert_eq!(written, read("tgin.yaml"));
}

/// A RON parse error reports line, column and kind; never the text around
/// it, which may hold a token.
#[test]
fn parse_error_never_quotes_config_text() {
    let dir = scratch("migrate_parse_error_never_quotes_config_text");
    let cases = [
        // An unknown field on the token's line.
        format!(
            "(\n    updates: [LongPollUpdate(token: \"{TOKEN}\", url: Some(\"http://s3cret-marker/\"), bogus: 1)],\n    route: WebhookRoute(url: \"http://a/\"),\n)"
        ),
        // A token where a number belongs.
        format!(
            "(\n    server_port: Some({TOKEN}),\n    updates: [],\n    route: WebhookRoute(url: \"http://a/\"),\n)"
        ),
        // An unterminated string holding a token.
        format!("(\n    updates: [LongPollUpdate(token: \"{TOKEN})],\n"),
    ];
    for (i, ron) in cases.iter().enumerate() {
        let file = dir.join(format!("case{i}.ron"));
        std::fs::write(&file, ron).unwrap();
        let out = output(&mut tgin(&["config", "migrate", file.to_str().unwrap()]));
        assert_eq!(out.code, Some(1), "case {i}: {}", out.stderr);
        assert!(out.stderr.contains(", column "), "case {i}: {}", out.stderr);
        assert!(
            !out.stderr.contains(TOKEN_SECRET),
            "case {i} leaked the token: {}",
            out.stderr
        );
        assert!(
            !out.stderr.contains("123456789"),
            "case {i} quoted config text: {}",
            out.stderr
        );
        // Text that is not token-shaped is not masked, so it must never
        // be quoted at all.
        assert!(
            !out.stderr.contains("s3cret-marker") && !out.stderr.contains("LongPollUpdate(token"),
            "case {i} quoted config text: {}",
            out.stderr
        );
        assert_eq!(out.stdout, "", "case {i}");
    }
}
