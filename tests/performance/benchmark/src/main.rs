use axum::{
    body::Bytes,
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
};
use clap::{Parser, ValueEnum};
use dashmap::DashMap;
use hdrhistogram::Histogram;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Mutex, Notify, Semaphore},
    task::JoinSet,
    time::{interval, MissedTickBehavior},
};

const CSV_COLUMNS: &[&str] = &[
    "run_id",
    "git_sha",
    "scenario_family",
    "transport",
    "route_path",
    "scenario",
    "mode",
    "bot_count",
    "rps_target",
    "rps_actual",
    "duration_seconds",
    "drain_timeout_seconds",
    "max_in_flight",
    "sent",
    "received",
    "send_errors",
    "http_errors",
    "pending_end",
    "loss_percent",
    "min_ms",
    "mean_ms",
    "p50_ms",
    "p95_ms",
    "p99_ms",
    "max_ms",
];

#[derive(ValueEnum, Clone, Debug)]
enum BenchMode {
    Webhook,
    Longpoll,
}

impl BenchMode {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::Longpoll => "longpoll",
        }
    }
}

#[derive(ValueEnum, Clone, Debug, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Json,
    CsvHeader,
    CsvRow,
}

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(short, long, default_value = "http://127.0.0.1:3000/webhook")]
    target: String,
    #[arg(short, long, default_value_t = 1000)]
    rps: u64,
    #[arg(short, long, default_value_t = 10)]
    duration: u64,
    #[arg(short, long, default_value_t = 8090)]
    port: u16,
    #[arg(value_enum, short, long, default_value_t = BenchMode::Webhook)]
    mode: BenchMode,
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,
    #[arg(long, default_value = "manual")]
    run_id: String,
    #[arg(long, default_value = "unknown")]
    git_sha: String,
    #[arg(long, default_value = "unknown")]
    scenario_family: String,
    #[arg(long, default_value = "unknown")]
    transport: String,
    #[arg(long, default_value = "unknown")]
    route_path: String,
    #[arg(long, default_value = "unknown")]
    scenario: String,
    #[arg(long, default_value_t = 1)]
    bot_count: u16,
    #[arg(long, default_value_t = 2)]
    drain_timeout: u64,
    /// Maximum concurrent in-flight webhook sends. The scheduler waits on a
    /// semaphore at this bound; achieved RPS exposes generator saturation
    /// instead of letting it manifest as runaway scheduler memory.
    #[arg(long, default_value_t = 10_000)]
    max_in_flight: usize,
}

struct BenchState {
    pending: DashMap<String, Instant>,
    histogram: Mutex<Histogram<u64>>,
    sent_count: AtomicUsize,
    received_count: AtomicUsize,
    send_errors_count: AtomicUsize,
    http_errors_count: AtomicUsize,
    lp_queue: Mutex<Vec<Value>>,
    notify: Notify,
}

#[derive(Serialize, Clone, Debug)]
struct BenchReport {
    run_id: String,
    git_sha: String,
    scenario_family: String,
    transport: String,
    route_path: String,
    scenario: String,
    mode: String,
    bot_count: u16,
    rps_target: u64,
    rps_actual: f64,
    duration_seconds: u64,
    drain_timeout_seconds: u64,
    max_in_flight: usize,
    sent: usize,
    received: usize,
    send_errors: usize,
    http_errors: usize,
    pending_end: usize,
    loss_percent: f64,
    min_ms: Option<f64>,
    mean_ms: Option<f64>,
    p50_ms: Option<f64>,
    p95_ms: Option<f64>,
    p99_ms: Option<f64>,
    max_ms: Option<f64>,
}

impl BenchReport {
    fn csv_header() -> String {
        CSV_COLUMNS.join(",")
    }

    fn csv_row(&self) -> String {
        [
            csv_escape(&self.run_id),
            csv_escape(&self.git_sha),
            csv_escape(&self.scenario_family),
            csv_escape(&self.transport),
            csv_escape(&self.route_path),
            csv_escape(&self.scenario),
            csv_escape(&self.mode),
            self.bot_count.to_string(),
            self.rps_target.to_string(),
            format_f64(self.rps_actual),
            self.duration_seconds.to_string(),
            self.drain_timeout_seconds.to_string(),
            self.max_in_flight.to_string(),
            self.sent.to_string(),
            self.received.to_string(),
            self.send_errors.to_string(),
            self.http_errors.to_string(),
            self.pending_end.to_string(),
            format_f64(self.loss_percent),
            format_optional_f64(self.min_ms),
            format_optional_f64(self.mean_ms),
            format_optional_f64(self.p50_ms),
            format_optional_f64(self.p95_ms),
            format_optional_f64(self.p99_ms),
            format_optional_f64(self.max_ms),
        ]
        .join(",")
    }

    /// Build a report from raw counters with placeholder metadata. Used by
    /// unit tests that assert the report math distinguishes failure modes.
    #[cfg(test)]
    fn from_counts_for_test(
        sent: usize,
        received: usize,
        send_errors: usize,
        http_errors: usize,
        pending_end: usize,
    ) -> BenchReport {
        let loss_percent = if sent > 0 {
            100.0 * (sent.saturating_sub(received)) as f64 / sent as f64
        } else {
            0.0
        };
        BenchReport {
            run_id: "test".to_string(),
            git_sha: "test".to_string(),
            scenario_family: "test".to_string(),
            transport: "test".to_string(),
            route_path: "test".to_string(),
            scenario: "test".to_string(),
            mode: "webhook".to_string(),
            bot_count: 1,
            rps_target: 0,
            rps_actual: 0.0,
            duration_seconds: 0,
            drain_timeout_seconds: 0,
            max_in_flight: 0,
            sent,
            received,
            send_errors,
            http_errors,
            pending_end,
            loss_percent,
            min_ms: None,
            mean_ms: None,
            p50_ms: None,
            p95_ms: None,
            p99_ms: None,
            max_ms: None,
        }
    }
}

#[derive(Deserialize)]
struct GetUpdatesParams {
    #[allow(dead_code)]
    offset: Option<i64>,
    #[allow(dead_code)]
    timeout: Option<u64>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.format == OutputFormat::CsvHeader {
        println!("{}", BenchReport::csv_header());
        return;
    }

    let state = Arc::new(BenchState {
        pending: DashMap::new(),
        histogram: Mutex::new(Histogram::<u64>::new(3).unwrap()),
        sent_count: AtomicUsize::new(0),
        received_count: AtomicUsize::new(0),
        send_errors_count: AtomicUsize::new(0),
        http_errors_count: AtomicUsize::new(0),
        lp_queue: Mutex::new(Vec::new()),
        notify: Notify::new(),
    });

    if args.format == OutputFormat::Text {
        println!("🚀 Starting Benchmark ({:?})", args.mode);
    }

    let server_state = state.clone();
    let app = Router::new()
        .route("/bot:token/sendMessage", post(handle_send_message))
        .route("/bot:token/getMe", get(handle_get_me).post(handle_get_me))
        .route(
            "/bot:token/getUpdates",
            get(handle_get_updates).post(handle_get_updates),
        )
        .with_state(server_state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], args.port));

    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client = Client::builder()
        .pool_max_idle_per_host(1000)
        .build()
        .unwrap();

    let send_semaphore = Arc::new(Semaphore::new(args.max_in_flight.max(1)));
    let mut send_tasks: JoinSet<()> = JoinSet::new();

    let start_test = Instant::now();
    let interval_micros = if args.rps > 0 {
        1_000_000 / args.rps
    } else {
        100_000
    };
    let mut ticker = interval(Duration::from_micros(interval_micros));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut update_id_counter: u64 = 100_000;
    let mut sequence: u64 = 0;
    let duration_sec = args.duration;

    while start_test.elapsed().as_secs() < duration_sec {
        ticker.tick().await;

        // Monotonic per-run correlation ID. Uniqueness within a run is all
        // the bookkeeping needs; UUIDv4 generation was hot-path noise.
        let correlation_id = format!("{}:{}", args.run_id, sequence);
        sequence += 1;

        state.pending.insert(correlation_id.clone(), Instant::now());
        state.sent_count.fetch_add(1, Ordering::Relaxed);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        update_id_counter += 1;

        let update = json!({
            "update_id": update_id_counter,
            "message": {
                "message_id": 123,
                "date": timestamp,
                "chat": { "id": 1, "type": "private" },
                "from": { "id": 1, "is_bot": false, "first_name": "Bench" },
                "text": correlation_id
            }
        });

        match args.mode {
            BenchMode::Webhook => {
                // Acquire before spawn: when the in-flight bound is reached,
                // the scheduler blocks here. That backpressure shows up in
                // `rps_actual` and tells the caller the generator (not the
                // SUT) is saturated.
                let permit = send_semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("semaphore closed");
                let c = client.clone();
                let u = args.target.clone();
                let s = state.clone();
                let cid = correlation_id.clone();
                send_tasks.spawn(async move {
                    let _permit = permit;
                    match c.post(&u).json(&update).send().await {
                        Ok(response) if response.status().is_success() => {}
                        Ok(_) => {
                            s.http_errors_count.fetch_add(1, Ordering::Relaxed);
                            // The send did not reach a successful path; the
                            // caller never gets a sendMessage callback for
                            // this id, so the pending entry would otherwise
                            // be miscounted as response-loss.
                            s.pending.remove(&cid);
                        }
                        Err(_) => {
                            s.send_errors_count.fetch_add(1, Ordering::Relaxed);
                            s.pending.remove(&cid);
                        }
                    }
                });
            }
            BenchMode::Longpoll => {
                let mut q = state.lp_queue.lock().await;
                q.push(update);
                // notify_one (not notify_waiters): closes the same lost-wakeup
                // window that handle_get_updates has between dropping its empty-
                // check guard and registering the Notified future. With
                // notify_waiters here, a notification fired while the consumer
                // is mid-registration is silently dropped and the consumer holds
                // its request open until the 1 s internal timeout.
                state.notify.notify_one();
            }
        }
    }

    let elapsed_generation = start_test.elapsed();

    if args.format == OutputFormat::Text {
        println!(
            "🏁 Sending finished. Draining outstanding sends and trailing responses (drain timeout: {}s)...",
            args.drain_timeout
        );
    }

    let drain_deadline = Instant::now() + Duration::from_secs(args.drain_timeout);

    // Drain in two stages, both bounded by the same deadline:
    //   1. Outstanding webhook send tasks. Until they finish we cannot tell
    //      send/HTTP failures from genuine pending-end loss.
    //   2. Trailing sendMessage callbacks for updates already in flight on
    //      the SUT side.
    while !send_tasks.is_empty() {
        let now = Instant::now();
        if now >= drain_deadline {
            break;
        }
        let remaining = drain_deadline - now;
        tokio::select! {
            _ = send_tasks.join_next() => {}
            _ = tokio::time::sleep(remaining) => break,
        }
    }

    while !state.pending.is_empty() && Instant::now() < drain_deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let report = build_report(&args, &state, elapsed_generation).await;
    emit_report(&args.format, &report);
}

async fn handle_get_me() -> Json<Value> {
    Json(json!({
        "ok": true,
        "result": {
            "id": 123456789,
            "is_bot": true,
            "first_name": "Tgin Bench Bot",
            "username": "bench_bot",
            "can_join_groups": true,
            "can_read_all_group_messages": false,
            "supports_inline_queries": false
        }
    }))
}

async fn handle_send_message(State(state): State<Arc<BenchState>>, body: Bytes) -> Json<Value> {
    let payload: Value = if let Ok(json) = serde_json::from_slice(&body) {
        json
    } else if let Ok(form) = serde_urlencoded::from_bytes(&body) {
        form
    } else {
        json!({})
    };

    if let Some(text) = payload.get("text").and_then(|t| t.as_str()) {
        if let Some((_, start_time)) = state.pending.remove(text) {
            let elapsed = start_time.elapsed().as_micros() as u64;
            let mut hist = state.histogram.lock().await;
            let _ = hist.record(elapsed);
            state.received_count.fetch_add(1, Ordering::Relaxed);
        }
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    Json(
        json!({ "ok": true, "result": { "message_id": 123, "date": timestamp, "chat": { "id": 1, "type": "private" }, "text": "ok" } }),
    )
}

async fn handle_get_updates(
    State(state): State<Arc<BenchState>>,
    _query: Option<Query<GetUpdatesParams>>,
) -> Json<Value> {
    let updates = {
        let mut q = state.lp_queue.lock().await;
        let batch: Vec<Value> = q.drain(..).collect();
        batch
    };
    if updates.is_empty() {
        let _ = tokio::time::timeout(Duration::from_secs(1), state.notify.notified()).await;
        let mut q = state.lp_queue.lock().await;
        let batch: Vec<Value> = q.drain(..).collect();
        return Json(json!({ "ok": true, "result": batch }));
    }
    Json(json!({ "ok": true, "result": updates }))
}

async fn build_report(
    args: &Args,
    state: &BenchState,
    elapsed_generation: Duration,
) -> BenchReport {
    let sent = state.sent_count.load(Ordering::Relaxed);
    let received = state.received_count.load(Ordering::Relaxed);
    let send_errors = state.send_errors_count.load(Ordering::Relaxed);
    let http_errors = state.http_errors_count.load(Ordering::Relaxed);
    let pending_end = state.pending.len();
    let hist = state.histogram.lock().await;
    let loss_percent = if sent > 0 {
        100.0 * (sent.saturating_sub(received)) as f64 / sent as f64
    } else {
        0.0
    };
    // Achieved generation rate based on how long the scheduler actually ran,
    // not the configured duration. When the semaphore or scheduler delays
    // ticks, this number diverges from `rps_target` and flags the generator
    // as the bottleneck.
    let elapsed_secs = elapsed_generation.as_secs_f64();
    let rps_actual = if elapsed_secs > 0.0 {
        sent as f64 / elapsed_secs
    } else {
        0.0
    };

    let (min_ms, mean_ms, p50_ms, p95_ms, p99_ms, max_ms) = if received > 0 {
        (
            Some(hist.min() as f64 / 1000.0),
            Some(hist.mean() / 1000.0),
            Some(hist.value_at_quantile(0.5) as f64 / 1000.0),
            Some(hist.value_at_quantile(0.95) as f64 / 1000.0),
            Some(hist.value_at_quantile(0.99) as f64 / 1000.0),
            Some(hist.max() as f64 / 1000.0),
        )
    } else {
        (None, None, None, None, None, None)
    };

    BenchReport {
        run_id: args.run_id.clone(),
        git_sha: args.git_sha.clone(),
        scenario_family: args.scenario_family.clone(),
        transport: args.transport.clone(),
        route_path: args.route_path.clone(),
        scenario: args.scenario.clone(),
        mode: args.mode.as_str().to_string(),
        bot_count: args.bot_count,
        rps_target: args.rps,
        rps_actual,
        duration_seconds: args.duration,
        drain_timeout_seconds: args.drain_timeout,
        max_in_flight: args.max_in_flight,
        sent,
        received,
        send_errors,
        http_errors,
        pending_end,
        loss_percent,
        min_ms,
        mean_ms,
        p50_ms,
        p95_ms,
        p99_ms,
        max_ms,
    }
}

fn emit_report(format: &OutputFormat, report: &BenchReport) {
    match format {
        OutputFormat::Text => print_text_report(report),
        OutputFormat::Json => println!("{}", serde_json::to_string(report).unwrap()),
        OutputFormat::CsvHeader => println!("{}", BenchReport::csv_header()),
        OutputFormat::CsvRow => println!("{}", report.csv_row()),
    }
}

fn print_text_report(report: &BenchReport) {
    println!("\n==========================================");
    println!("📊 BENCHMARK RESULTS");
    println!("==========================================");
    println!("Requests Sent:     {}", report.sent);
    println!("Responses Recv:    {}", report.received);
    println!("Send Errors:       {}", report.send_errors);
    println!("HTTP Errors:       {}", report.http_errors);
    println!("Pending at End:    {}", report.pending_end);
    println!("RPS Target:        {}", report.rps_target);
    println!("RPS Actual:        {:.2}", report.rps_actual);
    println!("Loss Rate:         {:.2}%", report.loss_percent);
    println!("------------------------------------------");
    println!("LATENCY (Round-Trip Time):");
    print_latency_line("Min", 4, report.min_ms);
    print_latency_line("Mean", 3, report.mean_ms);
    print_latency_line("p50", 4, report.p50_ms);
    print_latency_line("p95", 4, report.p95_ms);
    print_latency_line("p99", 4, report.p99_ms);
    print_latency_line("Max", 4, report.max_ms);
    println!("==========================================");
}

fn print_latency_line(label: &str, spaces_after_colon: usize, value_ms: Option<f64>) {
    let padding = " ".repeat(spaces_after_colon);
    match value_ms {
        Some(value_ms) => println!("  {label}:{padding}{value_ms:.2} ms"),
        None => println!("  {label}:{padding}n/a"),
    }
}

fn format_f64(value: f64) -> String {
    format!("{value:.2}")
}

fn format_optional_f64(value: Option<f64>) -> String {
    value.map(format_f64).unwrap_or_default()
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> BenchReport {
        BenchReport {
            run_id: "run-1".to_string(),
            git_sha: "abc123".to_string(),
            scenario_family: "family".to_string(),
            transport: "webhook".to_string(),
            route_path: "/webhook".to_string(),
            scenario: "steady".to_string(),
            mode: "webhook".to_string(),
            bot_count: 1,
            rps_target: 100,
            rps_actual: 99.5,
            duration_seconds: 10,
            drain_timeout_seconds: 2,
            max_in_flight: 10_000,
            sent: 1000,
            received: 995,
            send_errors: 3,
            http_errors: 2,
            pending_end: 5,
            loss_percent: 0.5,
            min_ms: Some(1.0),
            mean_ms: Some(2.0),
            p50_ms: Some(1.5),
            p95_ms: Some(3.5),
            p99_ms: Some(4.5),
            max_ms: Some(5.0),
        }
    }

    #[test]
    fn csv_header_and_row_have_same_column_count() {
        let report = sample_report();
        let header_count = BenchReport::csv_header().split(',').count();
        let row_count = report.csv_row().split(',').count();

        assert_eq!(header_count, row_count);
    }

    #[test]
    fn failed_run_uses_empty_latency_fields() {
        let report = BenchReport {
            received: 0,
            min_ms: None,
            mean_ms: None,
            p50_ms: None,
            p95_ms: None,
            p99_ms: None,
            max_ms: None,
            ..sample_report()
        };

        let row = report.csv_row();
        let fields: Vec<&str> = row.split(',').collect();
        let min_idx = BenchReport::csv_header()
            .split(',')
            .position(|column| column == "min_ms")
            .unwrap();
        let max_idx = BenchReport::csv_header()
            .split(',')
            .position(|column| column == "max_ms")
            .unwrap();

        for field in &fields[min_idx..=max_idx] {
            assert!(
                field.is_empty(),
                "expected empty latency field, got {field:?}"
            );
        }
    }

    #[test]
    fn report_distinguishes_send_errors_from_pending_loss() {
        let report = BenchReport::from_counts_for_test(100, 90, 3, 2, 5);
        assert_eq!(report.sent, 100);
        assert_eq!(report.received, 90);
        assert_eq!(report.send_errors, 3);
        assert_eq!(report.http_errors, 2);
        assert_eq!(report.pending_end, 5);
        assert_eq!(report.loss_percent, 10.0);
    }

    #[test]
    fn report_zero_sent_is_not_a_division_by_zero() {
        let report = BenchReport::from_counts_for_test(0, 0, 0, 0, 0);
        assert_eq!(report.loss_percent, 0.0);
    }
}
