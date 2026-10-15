//! The long-running bench process: the fake Bot API is up from container
//! start, so tgin and the bots are polling it before a run begins, and
//! `POST /bench/run` performs one measured run on demand.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use clap::ValueEnum;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{oneshot, Mutex},
    time::interval,
};

use crate::{
    load::{rate, run_open_loop, wait_until, Schedule, SendPhase, SystemClock},
    mock::{self, MockApi},
    report::Report,
    send::Sender,
};

/// Spacing of warm-up sends.
const WARMUP_SPACING: Duration = Duration::from_millis(10);

#[derive(ValueEnum, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// POST updates to `--target`.
    Webhook,
    /// Serve updates through `getUpdates` on the fake Bot API.
    Longpoll,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::Longpoll => "longpoll",
        }
    }
}

/// One measured run.
#[derive(clap::Args, Serialize, Deserialize, Clone, Debug)]
pub struct RunOptions {
    #[arg(short, long, value_enum, default_value_t = Mode::Webhook)]
    pub mode: Mode,
    /// Webhook URL updates are posted to (webhook mode).
    #[arg(short, long, default_value = "http://127.0.0.1:3000/webhook")]
    pub target: String,
    /// Target send rate, updates per second.
    #[arg(short, long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..))]
    pub rps: u64,
    /// Length of the send phase, seconds.
    #[arg(short, long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    pub duration: u64,
    /// Scenario name for the report.
    #[arg(long, default_value = "manual")]
    pub scenario: String,
    /// Engine under test for the report, such as `v0` or `v1`.
    #[arg(long, default_value = "unknown")]
    pub engine: String,
    /// Most webhook sends in flight. A send that finds them all taken is not
    /// issued and counts as `dropped_by_generator`.
    #[arg(long, default_value_t = 10_000)]
    pub max_in_flight: usize,
    /// After the send phase, how long to wait for outstanding replies, seconds.
    #[arg(long, default_value_t = 30)]
    pub completion_timeout: u64,
    /// Unmeasured updates sent first to prove every consumer is live; 0 skips
    /// the warm-up.
    #[arg(long, default_value_t = 30)]
    pub warmup_updates: u64,
    /// How long to wait for every warm-up reply, seconds.
    #[arg(long, default_value_t = 30)]
    pub warmup_timeout: u64,
    /// Bot token of the long-poll update stream; the consumer must poll the
    /// fake Bot API under the same token.
    #[arg(long, default_value = "123:test")]
    pub longpoll_token: String,
}

pub struct Server {
    api: Arc<MockApi>,
    client: Client,
    /// The next `update_id`. Ids keep increasing across runs, as Telegram's
    /// do, because long-poll offsets depend on it. Held for a whole run, so
    /// runs never overlap.
    next_update_id: Mutex<u64>,
}

impl Server {
    pub fn new() -> Self {
        Self {
            api: Arc::new(MockApi::new()),
            client: Client::new(),
            next_update_id: Mutex::new(1),
        }
    }
}

/// The fake Bot API plus `GET /bench/health` and `POST /bench/run`.
pub fn router(server: Arc<Server>) -> Router {
    let control = Router::new()
        .route("/bench/health", get(|| async { "ok" }))
        .route("/bench/run", post(run_handler))
        .with_state(server.clone());
    mock::router(server.api.clone()).merge(control)
}

async fn run_handler(
    State(server): State<Arc<Server>>,
    Json(options): Json<RunOptions>,
) -> Response {
    let Ok(mut next_update_id) = server.next_update_id.try_lock() else {
        return (StatusCode::CONFLICT, "another run is in progress").into_response();
    };
    match run(&server, &options, &mut next_update_id).await {
        Ok(report) => Json(report).into_response(),
        Err(reason) => (StatusCode::UNPROCESSABLE_ENTITY, reason).into_response(),
    }
}

/// Warm up, run the open-loop send phase, wait for completion and report.
async fn run(
    server: &Server,
    options: &RunOptions,
    next_update_id: &mut u64,
) -> Result<Report, String> {
    if options.rps == 0 || options.duration == 0 {
        return Err("rps and duration must be at least 1".to_owned());
    }
    let tracker = server.api.start_run();
    let sender = Arc::new(match options.mode {
        Mode::Webhook => Sender::webhook(
            server.client.clone(),
            &options.target,
            options.max_in_flight,
            tracker.clone(),
        ),
        Mode::Longpoll => Sender::LongPoll {
            stream: server.api.stream(&options.longpoll_token),
            tracker: tracker.clone(),
        },
    });

    if options.warmup_updates > 0 {
        let mut spacing = interval(WARMUP_SPACING);
        for _ in 0..options.warmup_updates {
            spacing.tick().await;
            sender.send_warmup(*next_update_id);
            *next_update_id += 1;
        }
        wait_until(Duration::from_secs(options.warmup_timeout), || {
            tracker.warmup_done()
        })
        .await;
        if !tracker.warmup_done() {
            let counts = tracker.counts();
            return Err(format!(
                "warm-up failed: {} of {} updates came back within {} s; the system under test is not ready",
                counts.warmup_received, counts.warmup_sent, options.warmup_timeout
            ));
        }
    }

    let first_update_id = *next_update_id;
    let (schedule, phase) =
        send_phase(sender, first_update_id, options.rps, options.duration).await?;
    *next_update_id = first_update_id + schedule.total();
    let waited = wait_until(Duration::from_secs(options.completion_timeout), || {
        tracker.settled()
    })
    .await;

    let counts = tracker.counts();
    let delivery_window = tracker.last_reply().map_or(Duration::ZERO, |last| {
        last.into_std().saturating_duration_since(schedule.start())
    });
    Ok(Report {
        scenario: options.scenario.clone(),
        engine: options.engine.clone(),
        mode: options.mode.as_str().to_owned(),
        target_rps: options.rps,
        achieved_rps: rate(counts.sent, phase.wall),
        delivered_rps: rate(counts.received, delivery_window),
        duration_secs: options.duration,
        send_phase_secs: phase.wall.as_secs_f64(),
        lag: phase.lag,
        completion_wait_secs: waited.as_secs_f64(),
        sent: counts.sent,
        received: counts.received,
        unreceived: counts.unreceived(),
        duplicate_replies: counts.duplicate_replies,
        dropped_by_generator: counts.dropped_by_generator,
        send_errors: counts.send_errors,
        http_errors: counts.http_errors,
        unmatched_replies: counts.unmatched_replies,
        latency: tracker.latency(),
        polls: tracker.poll_stats(),
    })
}

/// Run the send loop on a dedicated OS thread, so neither the async
/// runtime's timer granularity nor its other tasks delay a send. The thread
/// hands each update to the runtime (webhook) or the update stream (long
/// poll) and moves on.
async fn send_phase(
    sender: Arc<Sender>,
    first_update_id: u64,
    rps: u64,
    duration_secs: u64,
) -> Result<(Schedule, SendPhase), String> {
    let (done_tx, done_rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("bench-send-loop".to_owned())
        .spawn(move || {
            let schedule = Schedule::new(Instant::now(), rps, duration_secs);
            let phase = run_open_loop(&schedule, &SystemClock, |i, due| {
                sender.send(first_update_id + i, due)
            });
            let _ = done_tx.send((schedule, phase));
        })
        .map_err(|error| format!("cannot start the send loop thread: {error}"))?;
    done_rx
        .await
        .map_err(|_| "the send loop thread panicked".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// A bench server on a free port; returns its base URL.
    async fn start_bench() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(Arc::new(Server::new()));
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    /// Reply through `sendMessage` like an aiogram bot: a form body.
    async fn reply(client: &Client, bench: &str, text: &str) {
        client
            .post(format!("{bench}/bot123:test/sendMessage"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(serde_urlencoded::to_string([("chat_id", "1"), ("text", text)]).unwrap())
            .send()
            .await
            .unwrap();
    }

    /// A webhook bot that echoes the updates `answer` accepts.
    async fn start_webhook_bot(bench: String, answer: fn(&str) -> bool) -> String {
        let client = Client::new();
        let app = Router::new().route(
            "/webhook",
            post(move |Json(update): Json<Value>| {
                let (client, bench) = (client.clone(), bench.clone());
                async move {
                    let text = update["message"]["text"].as_str().unwrap().to_owned();
                    if answer(&text) {
                        tokio::spawn(async move { reply(&client, &bench, &text).await });
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/webhook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }

    fn options(mode: Mode, target: &str) -> RunOptions {
        RunOptions {
            mode,
            target: target.to_owned(),
            rps: 200,
            duration: 1,
            scenario: "test".to_owned(),
            engine: "test".to_owned(),
            max_in_flight: 10_000,
            completion_timeout: 5,
            warmup_updates: 5,
            warmup_timeout: 5,
            longpoll_token: "123:test".to_owned(),
        }
    }

    async fn run_remote(bench: &str, options: &RunOptions) -> Result<Report, String> {
        let response = Client::new()
            .post(format!("{bench}/bench/run"))
            .json(options)
            .send()
            .await
            .unwrap();
        if response.status().is_success() {
            Ok(response.json().await.unwrap())
        } else {
            Err(response.text().await.unwrap())
        }
    }

    fn assert_on_target(report: &Report) {
        assert!(
            (190.0..=201.0).contains(&report.achieved_rps),
            "achieved {} RPS",
            report.achieved_rps
        );
        // 200 replies, the last about 1 s after the first scheduled send.
        assert!(
            (180.0..=205.0).contains(&report.delivered_rps),
            "delivered {} RPS",
            report.delivered_rps
        );
    }

    #[tokio::test]
    async fn webhook_run_receives_every_update() {
        let bench = start_bench().await;
        let bot = start_webhook_bot(bench.clone(), |_| true).await;

        let report = run_remote(&bench, &options(Mode::Webhook, &bot))
            .await
            .unwrap();

        assert_eq!(
            (report.sent, report.received, report.unreceived),
            (200, 200, 0)
        );
        assert_eq!(report.dropped_by_generator, 0);
        assert_eq!((report.unmatched_replies, report.duplicate_replies), (0, 0));
        assert!(report.latency.p99_ms.is_some());
        assert_on_target(&report);
        assert!(report.completion_wait_secs < 1.0, "{report:?}");
    }

    /// An aiogram-style long-poll bot: the offset goes in a form body, and
    /// every update gets `replies` replies.
    fn start_long_poll_bot(bench: String, replies: usize) {
        tokio::spawn(async move {
            let client = Client::new();
            let mut offset = 0;
            loop {
                let response: Value = client
                    .post(format!("{bench}/bot123:test/getUpdates"))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(format!("offset={offset}&timeout=1"))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                for update in response["result"].as_array().unwrap() {
                    offset = update["update_id"].as_i64().unwrap() + 1;
                    let text = update["message"]["text"].as_str().unwrap().to_owned();
                    for _ in 0..replies {
                        reply(&client, &bench, &text).await;
                    }
                }
            }
        });
    }

    #[tokio::test]
    async fn long_poll_run_with_an_offset_honouring_poller_has_no_duplicates() {
        let bench = start_bench().await;
        start_long_poll_bot(bench.clone(), 1);

        let report = run_remote(&bench, &options(Mode::Longpoll, ""))
            .await
            .unwrap();

        assert_eq!(
            (report.sent, report.received, report.unreceived),
            (200, 200, 0)
        );
        assert_eq!((report.unmatched_replies, report.duplicate_replies), (0, 0));
        assert!(report.polls.calls > 0);
        assert_on_target(&report);
    }

    #[tokio::test]
    async fn repeated_replies_count_as_duplicates_not_as_received() {
        let bench = start_bench().await;
        start_long_poll_bot(bench.clone(), 2);

        let report = run_remote(&bench, &options(Mode::Longpoll, ""))
            .await
            .unwrap();

        assert_eq!((report.sent, report.received), (200, 200));
        // Settling does not wait for the second reply to the last updates.
        assert!(
            (195..=200).contains(&report.duplicate_replies),
            "{report:?}"
        );
        // The 5 repeated warm-up replies are unmatched, not duplicates.
        assert_eq!(report.unmatched_replies, 5);
    }

    #[tokio::test]
    async fn missing_replies_are_unreceived_after_the_completion_wait() {
        let bench = start_bench().await;
        let bot = start_webhook_bot(bench.clone(), |text| {
            text.starts_with("warmup-") || text.parse::<u64>().unwrap() % 2 == 0
        })
        .await;
        let options = RunOptions {
            completion_timeout: 1,
            ..options(Mode::Webhook, &bot)
        };

        let report = run_remote(&bench, &options).await.unwrap();

        assert_eq!(report.sent, 200);
        assert_eq!(report.received + report.unreceived, 200);
        assert_eq!(report.unreceived, 100);
        assert!(report.completion_wait_secs >= 1.0, "{report:?}");
        assert!(report.latency.p50_ms.is_some());
    }

    #[tokio::test]
    async fn a_dead_path_fails_the_warm_up() {
        let bench = start_bench().await;
        let bot = start_webhook_bot(bench.clone(), |_| false).await;
        let options = RunOptions {
            warmup_timeout: 1,
            ..options(Mode::Webhook, &bot)
        };

        let error = run_remote(&bench, &options).await.unwrap_err();

        assert!(error.contains("warm-up failed: 0 of 5"), "{error}");
    }

    #[tokio::test]
    async fn runs_count_from_zero_and_keep_update_ids_increasing() {
        let bench = start_bench().await;
        start_long_poll_bot(bench.clone(), 1);
        let options = options(Mode::Longpoll, "");

        let first = run_remote(&bench, &options).await.unwrap();
        // Restarted ids would sit below the poller's offset and never arrive.
        let second = run_remote(&bench, &options).await.unwrap();

        assert_eq!((first.sent, first.received), (200, 200));
        assert_eq!((second.sent, second.received), (200, 200));
        assert_eq!(second.unmatched_replies, 0);
    }
}
