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
    sync::{Mutex, Notify},
    time::interval,
};
use uuid::Uuid;

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

    let gen_state = state.clone();
    let target_url = args.target.clone();
    let rps = args.rps;
    let duration_sec = args.duration;
    let mode = args.mode.clone();
    let client = Client::builder()
        .pool_max_idle_per_host(1000)
        .build()
        .unwrap();

    let generator_handle = tokio::spawn(async move {
        let start_test = Instant::now();
        let interval_micros = if rps > 0 { 1_000_000 / rps } else { 100_000 };
        let mut ticker = interval(Duration::from_micros(interval_micros));
        let mut update_id_counter = 100000;

        while start_test.elapsed().as_secs() < duration_sec {
            ticker.tick().await;
            let uuid = Uuid::new_v4().to_string();
            let s = gen_state.clone();
            s.pending.insert(uuid.clone(), Instant::now());
            s.sent_count.fetch_add(1, Ordering::Relaxed);

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
                    "text": uuid
                }
            });

            match mode {
                BenchMode::Webhook => {
                    let c = client.clone();
                    let u = target_url.clone();
                    tokio::spawn(async move {
                        match c.post(&u).json(&update).send().await {
                            Ok(response) if !response.status().is_success() => {
                                s.http_errors_count.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) => {
                                s.send_errors_count.fetch_add(1, Ordering::Relaxed);
                            }
                            Ok(_) => {}
                        }
                    });
                }
                BenchMode::Longpoll => {
                    let mut q = s.lp_queue.lock().await;
                    q.push(update);
                    // notify_one (not notify_waiters): closes the same lost-wakeup
                    // window that handle_get_updates has between dropping its empty-
                    // check guard and registering the Notified future. With
                    // notify_waiters here, a notification fired while the consumer
                    // is mid-registration is silently dropped and the consumer holds
                    // its request open until the 1 s internal timeout.
                    s.notify.notify_one();
                }
            }
        }
    });

    let _stats_handle = tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
        }
    });

    generator_handle.await.unwrap();
    if args.format == OutputFormat::Text {
        println!(
            "🏁 Sending finished. Waiting {}s for trailing responses...",
            args.drain_timeout
        );
    }
    tokio::time::sleep(Duration::from_secs(args.drain_timeout)).await;

    let report = build_report(&args, &state).await;
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

async fn build_report(args: &Args, state: &BenchState) -> BenchReport {
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
    let rps_actual = if args.duration > 0 {
        sent as f64 / args.duration as f64
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
    println!("Errors (Net):      {}", report.send_errors);
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
}
