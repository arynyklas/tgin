//! The result of one run and its output formats.

use serde::{Deserialize, Serialize};

use crate::{
    load::Lag,
    track::{Latency, PollStats},
};

/// The columns `benchmark.sh` collects, in order.
pub const CSV_COLUMNS: [&str; 13] = [
    "scenario",
    "engine",
    "target_rps",
    "achieved_rps",
    "delivered_rps",
    "p50_ms",
    "p95_ms",
    "p99_ms",
    "max_ms",
    "unreceived",
    "duplicate_replies",
    "dropped_by_generator",
    "lag_p99_ms",
];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Report {
    pub scenario: String,
    pub engine: String,
    pub mode: String,
    pub target_rps: u64,
    /// Updates sent per second of send-phase wall time.
    pub achieved_rps: f64,
    /// Updates received per second, from the first scheduled send to the
    /// last first reply.
    pub delivered_rps: f64,
    pub duration_secs: u64,
    pub send_phase_secs: f64,
    /// How late sends left the generator.
    #[serde(flatten)]
    pub lag: Lag,
    /// How long the run waited for outstanding replies after the send phase.
    pub completion_wait_secs: f64,
    pub sent: u64,
    pub received: u64,
    /// Sent updates whose reply never came back, failed sends included.
    pub unreceived: u64,
    /// Further replies to measured updates already received.
    pub duplicate_replies: u64,
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
            format_f64(self.delivered_rps),
            format_optional_f64(self.latency.p50_ms),
            format_optional_f64(self.latency.p95_ms),
            format_optional_f64(self.latency.p99_ms),
            format_optional_f64(self.latency.max_ms),
            self.unreceived.to_string(),
            self.duplicate_replies.to_string(),
            self.dropped_by_generator.to_string(),
            // Lag is sub-millisecond when the generator keeps up.
            format!("{:.3}", self.lag.p99_ms),
        ]
        .join(",")
    }

    /// One line with the counters the CSV leaves out, for the console.
    pub fn summary_line(&self) -> String {
        format!(
            "{} ({}) at {} RPS: achieved {:.2} RPS, delivered {:.2} RPS, sent {}, received {}, \
             unreceived {}, duplicate replies {}, dropped by generator {}, send errors {}, \
             HTTP errors {}, unmatched replies {}, send lag p50 {:.3} ms p99 {:.3} ms max {:.3} ms, \
             completion wait {:.2} s, getUpdates calls {}",
            self.scenario,
            self.engine,
            self.target_rps,
            self.achieved_rps,
            self.delivered_rps,
            self.sent,
            self.received,
            self.unreceived,
            self.duplicate_replies,
            self.dropped_by_generator,
            self.send_errors,
            self.http_errors,
            self.unmatched_replies,
            self.lag.p50_ms,
            self.lag.p99_ms,
            self.lag.max_ms,
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
             Delivered RPS:       {:.2}\n\
             Send lag:            p50 {:.3} ms, p99 {:.3} ms, max {:.3} ms\n\
             Sent:                {}\n\
             Received:            {}\n\
             Unreceived:          {} (send errors {}, HTTP errors {})\n\
             Duplicate replies:   {}\n\
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
            self.delivered_rps,
            self.lag.p50_ms,
            self.lag.p99_ms,
            self.lag.max_ms,
            self.sent,
            self.received,
            self.unreceived,
            self.send_errors,
            self.http_errors,
            self.duplicate_replies,
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
            delivered_rps: 198.25,
            duration_secs: 5,
            send_phase_secs: 5.0,
            lag: Lag {
                p50_ms: 0.004,
                p99_ms: 0.0216,
                max_ms: 0.3,
            },
            completion_wait_secs: 0.01,
            sent: 1000,
            received: 998,
            unreceived: 2,
            duplicate_replies: 4,
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
            "scenario,engine,target_rps,achieved_rps,delivered_rps,p50_ms,p95_ms,p99_ms,max_ms,\
             unreceived,duplicate_replies,dropped_by_generator,lag_p99_ms"
        );
    }

    #[test]
    fn csv_row_matches_the_header() {
        assert_eq!(
            sample().csv_row(),
            "webhook-tgin-1,v1,200,199.50,198.25,0.90,2.00,3.00,7.50,2,4,3,0.022"
        );
    }

    #[test]
    fn run_without_replies_leaves_latency_empty() {
        let report = Report {
            received: 0,
            unreceived: 1000,
            delivered_rps: 0.0,
            latency: Latency::default(),
            ..sample()
        };

        assert_eq!(
            report.csv_row(),
            "webhook-tgin-1,v1,200,199.50,0.00,,,,,1000,4,3,0.022"
        );
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
            "delivered_rps",
            "unreceived",
            "duplicate_replies",
            "dropped_by_generator",
            "send_errors",
            "http_errors",
            "lag_p50_ms",
            "lag_p99_ms",
            "lag_max_ms",
            "p99_ms",
            "get_updates_calls",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
    }
}
