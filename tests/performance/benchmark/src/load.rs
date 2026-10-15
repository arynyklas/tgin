//! Open-loop load: sends go out on a fixed schedule derived from the target
//! rate, whatever the system under test does with them.

use std::time::Duration;

use tokio::time::{sleep_until, Instant};

/// When each send is due: send `i` at `start + i / rps`.
#[derive(Clone, Copy, Debug)]
pub struct Schedule {
    start: Instant,
    rps: u64,
    total: u64,
}

/// How often [`wait_until`] re-checks its condition.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

impl Schedule {
    /// `rps * duration_secs` sends from `start`. `rps` must be at least 1.
    pub fn new(start: Instant, rps: u64, duration_secs: u64) -> Self {
        assert!(rps > 0, "rps must be at least 1");
        Self {
            start,
            rps,
            total: rps.saturating_mul(duration_secs),
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// How long after `start` send `i` is due. Computed from `i` directly, so
    /// rounding never accumulates into drift.
    pub fn offset(&self, i: u64) -> Duration {
        let nanos = u128::from(i) * 1_000_000_000 / u128::from(self.rps);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    pub fn due(&self, i: u64) -> Instant {
        self.start + self.offset(i)
    }
}

/// What the send phase took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendPhase {
    /// From the start to the end of the phase: the later of the last actual
    /// send and the nominal end `total / rps`.
    pub wall: Duration,
    /// The largest delay between a send's due time and when it went out.
    pub max_lag: Duration,
}

/// Run the send phase: call `send(i, due)` for every scheduled send in order,
/// at its due time or, when the loop is behind, at once. A late send still
/// gets its due time, so whoever records latency counts the delay.
///
/// `send` must not wait for the system under test; it hands the update off
/// and returns.
pub async fn run_open_loop(schedule: &Schedule, mut send: impl FnMut(u64, Instant)) -> SendPhase {
    let mut max_lag = Duration::ZERO;
    for i in 0..schedule.total {
        let due = schedule.due(i);
        if due > Instant::now() {
            sleep_until(due).await;
        }
        max_lag = max_lag.max(Instant::now().saturating_duration_since(due));
        send(i, due);
    }
    SendPhase {
        wall: schedule
            .start
            .elapsed()
            .max(schedule.offset(schedule.total)),
        max_lag,
    }
}

/// Updates sent per second of send-phase wall time.
pub fn achieved_rps(sent: u64, wall: Duration) -> f64 {
    let secs = wall.as_secs_f64();
    if secs > 0.0 {
        sent as f64 / secs
    } else {
        0.0
    }
}

/// Wait until `done()` holds or `timeout` passes, whichever is first.
/// Returns how long it waited.
pub async fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> Duration {
    let started = Instant::now();
    let deadline = started + timeout;
    while !done() {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        sleep_until(deadline.min(now + POLL_INTERVAL)).await;
    }
    started.elapsed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[tokio::test(start_paused = true)]
    async fn sends_are_scheduled_at_i_over_rps() {
        let start = Instant::now();
        let schedule = Schedule::new(start, 200, 5);

        assert_eq!(schedule.total(), 1000);
        assert_eq!(schedule.offset(0), Duration::ZERO);
        assert_eq!(schedule.offset(1), Duration::from_millis(5));
        assert_eq!(schedule.offset(200), Duration::from_secs(1));
        assert_eq!(schedule.offset(999), Duration::from_millis(4995));
        assert_eq!(schedule.due(7), start + Duration::from_millis(35));
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_does_not_accumulate_rounding_drift() {
        let schedule = Schedule::new(Instant::now(), 3, 1000);

        assert_eq!(schedule.offset(1), Duration::from_nanos(333_333_333));
        assert_eq!(schedule.offset(2), Duration::from_nanos(666_666_666));
        assert_eq!(schedule.offset(3), Duration::from_secs(1));
        assert_eq!(schedule.offset(2999), Duration::from_nanos(999_666_666_666));
    }

    #[tokio::test(start_paused = true)]
    async fn open_loop_sends_each_update_at_its_due_time() {
        let start = Instant::now();
        let schedule = Schedule::new(start, 100, 1);
        let mut sends = Vec::new();

        let phase = run_open_loop(&schedule, |i, due| sends.push((i, due, Instant::now()))).await;

        assert_eq!(sends.len(), 100);
        for (i, due, at) in &sends {
            assert_eq!(*due, start + Duration::from_millis(10 * i));
            assert_eq!(at, due, "send {i} left early or late");
        }
        assert_eq!(phase.max_lag, Duration::ZERO);
        // The last send is due at 990 ms; the phase still lasts the nominal 1 s.
        assert_eq!(phase.wall, Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn late_sends_go_out_at_once_and_keep_their_scheduled_time() {
        // The loop starts 55 ms behind: sends 0..=5 are already due.
        let start = Instant::now() - Duration::from_millis(55);
        let schedule = Schedule::new(start, 100, 1);
        let resumed = Instant::now();
        let mut sends = Vec::new();

        let phase = run_open_loop(&schedule, |i, due| sends.push((i, due, Instant::now()))).await;

        assert_eq!(sends.len(), 100, "late sends must not be skipped");
        for (i, due, at) in &sends {
            assert_eq!(*due, start + Duration::from_millis(10 * i));
            if *i <= 5 {
                assert_eq!(*at, resumed, "late send {i} must go out at once");
            } else {
                assert_eq!(at, due);
            }
        }
        assert_eq!(phase.max_lag, Duration::from_millis(55));
    }

    #[tokio::test(start_paused = true)]
    async fn open_loop_does_not_wait_for_slow_sends_to_complete() {
        // A send that hands off work (here: a task that takes 10 s) must not
        // delay the next one.
        let start = Instant::now();
        let schedule = Schedule::new(start, 10, 1);
        let mut issued = Vec::new();

        run_open_loop(&schedule, |i, _| {
            tokio::spawn(tokio::time::sleep(Duration::from_secs(10)));
            issued.push((i, Instant::now()));
        })
        .await;

        assert_eq!(issued.len(), 10);
        assert_eq!(issued[9].1, start + Duration::from_millis(900));
    }

    #[tokio::test(start_paused = true)]
    async fn send_phase_wall_time_extends_past_nominal_when_behind() {
        let start = Instant::now() - Duration::from_secs(3);
        let schedule = Schedule::new(start, 10, 1);

        let phase = run_open_loop(&schedule, |_, _| {}).await;

        // All ten sends were overdue; the phase ended when the last went out.
        assert_eq!(phase.wall, Duration::from_secs(3));
    }

    #[test]
    fn achieved_rps_is_sent_over_wall_time() {
        assert_eq!(achieved_rps(1000, Duration::from_secs(5)), 200.0);
        assert_eq!(achieved_rps(1000, Duration::from_secs(10)), 100.0);
        assert_eq!(achieved_rps(750, Duration::from_millis(2500)), 300.0);
        assert_eq!(achieved_rps(0, Duration::from_secs(5)), 0.0);
        assert_eq!(achieved_rps(10, Duration::ZERO), 0.0);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_until_returns_once_done() {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            flag.store(true, Ordering::Relaxed);
        });

        let waited = wait_until(Duration::from_secs(30), || done.load(Ordering::Relaxed)).await;

        assert!(
            waited >= Duration::from_millis(1500) && waited < Duration::from_millis(1600),
            "waited {waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_until_gives_up_at_the_timeout() {
        let waited = wait_until(Duration::from_secs(30), || false).await;

        assert_eq!(waited, Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_until_does_not_wait_when_already_done() {
        let waited = wait_until(Duration::from_secs(30), || true).await;

        assert_eq!(waited, Duration::ZERO);
    }
}
