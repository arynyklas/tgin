//! The result of one run and its output formats.

use serde::{Deserialize, Serialize};

use crate::track::{Latency, PollStats};

/// The columns `benchmark.sh` collects, in order.
pub const CSV_COLUMNS: [&str; 10] = [
    "scenario",
    "engine",
    "target_rps",
    "achieved_rps",
    "p50_ms",
    "p95_ms",
    "p99_ms",
    "max_ms",
    "unreceived",
    "dropped_by_generator",
];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Report {
    pub scenario: String,
    pub engine: String,
    pub mode: String,
    pub target_rps: u64,
    /// Updates sent per second of send-phase wall time.
    pub achieved_rps: f64,
    pub duration_secs: u64,
    pub send_phase_secs: f64,
    /// The largest delay between a send's due time and when it went out.
    pub max_send_lag_ms: f64,
    /// How long the run waited for outstanding replies after the send phase.
    pub completion_wait_secs: f64,
    pub sent: u64,
    pub received: u64,
    /// Sent updates whose reply never came back, failed sends included.
    pub unreceived: u64,
    /// Sends never issued because the in-flight budget was spent.
    pub dropped_by_generator: u64,
    pub send_errors: u64,
    pub http_errors: u64,
    pub unmatched_replies: u64,
    #[serde(flatten)]
    pub latency: Latency,
    #[serde(flatten)]
    pub polls: PollStats,
}

pub fn csv_header() -> String {
    CSV_COLUMNS.join(",")
}

impl Report {
    pub fn csv_row(&self) -> String {
        [
            csv_escape(&self.scenario),
            csv_escape(&self.engine),
            self.target_rps.to_string(),
            format_f64(self.achieved_rps),
            format_optional_f64(self.latency.p50_ms),
            format_optional_f64(self.latency.p95_ms),
            format_optional_f64(self.latency.p99_ms),
            format_optional_f64(self.latency.max_ms),
            self.unreceived.to_string(),
            self.dropped_by_generator.to_string(),
        ]
        .join(",")
    }

    /// One line with the counters the CSV leaves out, for the console.
    pub fn summary_line(&self) -> String {
        format!(
            "{} ({}) at {} RPS: achieved {:.2} RPS, sent {}, received {}, unreceived {}, \
             dropped by generator {}, send errors {}, HTTP errors {}, unmatched replies {}, \
             max send lag {:.2} ms, completion wait {:.2} s, getUpdates calls {}",
            self.scenario,
            self.engine,
            self.target_rps,
            self.achieved_rps,
            self.sent,
            self.received,
            self.unreceived,
            self.dropped_by_generator,
            self.send_errors,
            self.http_errors,
            self.unmatched_replies,
            self.max_send_lag_ms,
            self.completion_wait_secs,
            self.polls.calls,
        )
    }

    pub fn text(&self) -> String {
        let ms =
            |value: Option<f64>| value.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.2} ms"));
        let mut out = format!(
            "==========================================\n\
             BENCHMARK RESULTS: {} ({}, {})\n\
             ==========================================\n\
             Target RPS:          {}\n\
             Achieved RPS:        {:.2} over {:.2} s\n\
             Max send lag:        {:.2} ms\n\
             Sent:                {}\n\
             Received:            {}\n\
             Unreceived:          {} (send errors {}, HTTP errors {})\n\
             Dropped by generator: {}\n\
             Unmatched replies:   {}\n\
             Completion wait:     {:.2} s\n\
             ------------------------------------------\n\
             LATENCY from scheduled send to reply:\n\
             \x20 min  {}\n\
             \x20 mean {}\n\
             \x20 p50  {}\n\
             \x20 p95  {}\n\
             \x20 p99  {}\n\
             \x20 max  {}\n",
            self.scenario,
            self.engine,
            self.mode,
            self.target_rps,
            self.achieved_rps,
            self.send_phase_secs,
            self.max_send_lag_ms,
            self.sent,
            self.received,
            self.unreceived,
            self.send_errors,
            self.http_errors,
            self.dropped_by_generator,
            self.unmatched_replies,
            self.completion_wait_secs,
            ms(self.latency.min_ms),
            ms(self.latency.mean_ms),
            ms(self.latency.p50_ms),
            ms(self.latency.p95_ms),
            ms(self.latency.p99_ms),
            ms(self.latency.max_ms),
        );
        if self.polls.calls > 0 {
            let mean_batch = self
                .polls
                .mean_batch()
                .map_or_else(|| "n/a".to_owned(), |m| format!("{m:.2}"));
            out.push_str(&format!(
                "------------------------------------------\n\
                 getUpdates: {} calls, {} empty, max batch {}, mean batch {}\n",
                self.polls.calls, self.polls.empty_calls, self.polls.max_batch, mean_batch
            ));
        }
        out.push_str("==========================================");
        out
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
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Report {
        Report {
            scenario: "webhook-tgin-1".to_owned(),
            engine: "v1".to_owned(),
            mode: "webhook".to_owned(),
            target_rps: 200,
            achieved_rps: 199.5,
            duration_secs: 5,
            send_phase_secs: 5.0,
            max_send_lag_ms: 1.25,
            completion_wait_secs: 0.01,
            sent: 1000,
            received: 998,
            unreceived: 2,
            dropped_by_generator: 3,
            send_errors: 1,
            http_errors: 1,
            unmatched_replies: 0,
            latency: Latency {
                min_ms: Some(0.5),
                mean_ms: Some(1.0),
                p50_ms: Some(0.9),
                p95_ms: Some(2.0),
                p99_ms: Some(3.0),
                max_ms: Some(7.5),
            },
            polls: PollStats::default(),
        }
    }

    #[test]
    fn csv_header_is_exactly_the_collected_columns() {
        assert_eq!(
            csv_header(),
            "scenario,engine,target_rps,achieved_rps,p50_ms,p95_ms,p99_ms,max_ms,unreceived,dropped_by_generator"
        );
    }

    #[test]
    fn csv_row_matches_the_header() {
        assert_eq!(
            sample().csv_row(),
            "webhook-tgin-1,v1,200,199.50,0.90,2.00,3.00,7.50,2,3"
        );
    }

    #[test]
    fn run_without_replies_leaves_latency_empty() {
        let report = Report {
            received: 0,
            unreceived: 1000,
            latency: Latency::default(),
            ..sample()
        };

        assert_eq!(report.csv_row(), "webhook-tgin-1,v1,200,199.50,,,,,1000,3");
    }

    #[test]
    fn csv_fields_are_escaped() {
        let report = Report {
            scenario: "a,b".to_owned(),
            ..sample()
        };

        assert!(report.csv_row().starts_with("\"a,b\",v1,"));
    }

    #[test]
    fn json_report_carries_every_counter() {
        let json = serde_json::to_value(sample()).unwrap();

        for key in [
            "achieved_rps",
            "unreceived",
            "dropped_by_generator",
            "send_errors",
            "http_errors",
            "max_send_lag_ms",
            "p99_ms",
            "get_updates_calls",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
    }
}
