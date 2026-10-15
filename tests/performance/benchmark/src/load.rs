//! Open-loop load: sends go out on a fixed schedule derived from the target
//! rate, whatever the system under test does with them.
//!
//! The send loop runs on its own OS thread. It sleeps until shortly before a
//! send is due and spins the rest of the way, because a timer alone wakes up
//! to a couple of milliseconds late, and that lateness would be charged to
//! every latency sample.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};

/// How long before a due time the send loop stops sleeping and spins.
pub const SPIN_WINDOW: Duration = Duration::from_millis(2);
/// Longest single sleep of the send loop, so a stop is seen within it even
/// at low rates.
pub const MAX_SLEEP_SLICE: Duration = Duration::from_millis(10);
/// Longest send lag the lag histogram tells apart, 1 h in nanoseconds.
const LAG_MAX_NS: u64 = 3_600_000_000_000;
/// How often [`wait_until`] re-checks its condition.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The time source of the send loop; tests substitute a fake one.
pub trait Clock {
    fn now(&self) -> Instant;
    /// Block the thread for about `duration`; it may oversleep.
    fn sleep(&self, duration: Duration);
}

/// The real clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// When each send is due: send `i` at `start + i / rps`.
#[derive(Clone, Copy, Debug)]
pub struct Schedule {
    start: Instant,
    rps: u64,
    total: u64,
}

impl Schedule {
    /// `rps * duration_secs` sends from `start`. `rps` must be at least 1.
    pub fn new(start: Instant, rps: u64, duration_secs: u64) -> Self {
        assert!(rps > 0, "rps must be at least 1");
        Self {
            start,
            rps,
            total: Self::count(rps, duration_secs),
        }
    }

    /// How many sends a schedule of `rps` for `duration_secs` holds.
    pub fn count(rps: u64, duration_secs: u64) -> u64 {
        rps.saturating_mul(duration_secs)
    }

    pub fn start(&self) -> Instant {
        self.start
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

/// How late sends left, from their due time to the end of their hand-off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Lag {
    #[serde(rename = "lag_p50_ms")]
    pub p50_ms: f64,
    #[serde(rename = "lag_p99_ms")]
    pub p99_ms: f64,
    #[serde(rename = "lag_max_ms")]
    pub max_ms: f64,
}

/// What the send phase took.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SendPhase {
    /// From the start to the end of the phase: the later of the last actual
    /// send and the nominal end `total / rps`.
    pub wall: Duration,
    pub lag: Lag,
}

/// Block until `due` or until `stop` is raised: sleep while more than
/// [`SPIN_WINDOW`] remains, in slices of at most [`MAX_SLEEP_SLICE`], then
/// spin. Returns `false` when stopped.
fn wait_for(clock: &impl Clock, due: Instant, stop: &AtomicBool) -> bool {
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let now = clock.now();
        if now >= due {
            return true;
        }
        let remaining = due - now;
        if remaining > SPIN_WINDOW {
            clock.sleep((remaining - SPIN_WINDOW).min(MAX_SLEEP_SLICE));
        } else {
            std::hint::spin_loop();
        }
    }
}

/// Run the send phase on the calling thread: call `send(i, due)` for every
/// scheduled send in order, at its due time or, when the loop is behind, at
/// once. A late send still gets its due time, so whoever records latency
/// counts the delay. Raising `stop` ends the loop within
/// [`MAX_SLEEP_SLICE`], before the next send.
///
/// `send` must not wait for the system under test; it hands the update off
/// and returns.
pub fn run_open_loop(
    schedule: &Schedule,
    clock: &impl Clock,
    stop: &AtomicBool,
    mut send: impl FnMut(u64, Instant),
) -> SendPhase {
    let mut lag_ns =
        Histogram::<u64>::new_with_bounds(1, LAG_MAX_NS, 3).expect("valid histogram bounds");
    for i in 0..schedule.total {
        let due = schedule.due(i);
        if !wait_for(clock, due, stop) {
            break;
        }
        send(i, due);
        let lag = clock.now().saturating_duration_since(due);
        lag_ns.saturating_record(u64::try_from(lag.as_nanos()).unwrap_or(u64::MAX));
    }
    let wall = clock
        .now()
        .saturating_duration_since(schedule.start)
        .max(schedule.offset(schedule.total));
    let ms = |nanos: u64| nanos as f64 / 1_000_000.0;
    let lag = if lag_ns.is_empty() {
        Lag::default()
    } else {
        Lag {
            p50_ms: ms(lag_ns.value_at_quantile(0.50)),
            p99_ms: ms(lag_ns.value_at_quantile(0.99)),
            max_ms: ms(lag_ns.max()),
        }
    };
    SendPhase { wall, lag }
}

/// Updates per second over `window`; 0 for an empty window.
pub fn rate(count: u64, window: Duration) -> f64 {
    let secs = window.as_secs_f64();
    if secs > 0.0 {
        count as f64 / secs
    } else {
        0.0
    }
}

/// Wait until `done()` holds or `timeout` passes, whichever is first.
/// Returns how long it waited.
pub async fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> Duration {
    let started = tokio::time::Instant::now();
    let deadline = started + timeout;
    while !done() {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        tokio::time::sleep_until(deadline.min(now + POLL_INTERVAL)).await;
    }
    started.elapsed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };

    /// A clock that moves only when read or slept on: each read advances it
    /// by `tick`, each sleep by the duration plus `oversleep`.
    struct FakeClock {
        now: Cell<Instant>,
        tick: Duration,
        oversleep: Duration,
        /// Where each sleep would have ended without oversleeping.
        sleep_ends: RefCell<Vec<Instant>>,
        /// The length of each sleep asked for.
        sleeps: RefCell<Vec<Duration>>,
        /// Raised after this many sleeps, as if another thread stopped the run.
        stop_after_sleeps: Option<(usize, Arc<AtomicBool>)>,
    }

    impl FakeClock {
        fn new(oversleep: Duration) -> Self {
            Self {
                now: Cell::new(Instant::now()),
                tick: Duration::from_nanos(100),
                oversleep,
                sleep_ends: RefCell::new(Vec::new()),
                sleeps: RefCell::new(Vec::new()),
                stop_after_sleeps: None,
            }
        }

        fn peek(&self) -> Instant {
            self.now.get()
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            let now = self.now.get();
            self.now.set(now + self.tick);
            now
        }

        fn sleep(&self, duration: Duration) {
            let end = self.now.get() + duration;
            self.sleep_ends.borrow_mut().push(end);
            self.sleeps.borrow_mut().push(duration);
            self.now.set(end + self.oversleep);
            if let Some((after, stop)) = &self.stop_after_sleeps {
                if self.sleeps.borrow().len() >= *after {
                    stop.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    /// Run `schedule` on `clock`; return `(i, due, sent_at)` per send.
    fn record(schedule: &Schedule, clock: &FakeClock) -> (Vec<(u64, Instant, Instant)>, SendPhase) {
        let mut sends = Vec::new();
        let never = AtomicBool::new(false);
        let phase = run_open_loop(schedule, clock, &never, |i, due| {
            sends.push((i, due, clock.peek()))
        });
        (sends, phase)
    }

    #[test]
    fn a_stop_between_sends_ends_the_loop_before_the_next_send() {
        let clock = FakeClock::new(Duration::ZERO);
        let schedule = Schedule::new(clock.peek(), 100, 10);
        let stop = AtomicBool::new(false);
        let mut sent = 0;

        run_open_loop(&schedule, &clock, &stop, |i, _| {
            sent += 1;
            if i == 10 {
                stop.store(true, Ordering::Relaxed);
            }
        });

        assert_eq!(sent, 11);
    }

    #[test]
    fn a_stop_while_waiting_ends_the_loop_without_sending() {
        // 1 RPS: the wait for send 1 is about a second of sleeps.
        let stop = Arc::new(AtomicBool::new(false));
        let clock = FakeClock {
            stop_after_sleeps: Some((3, stop.clone())),
            ..FakeClock::new(Duration::ZERO)
        };
        let schedule = Schedule::new(clock.peek(), 1, 10);
        let mut sent = 0;

        run_open_loop(&schedule, &clock, &stop, |_, _| sent += 1);

        assert_eq!(sent, 1, "only send 0, due at once, went out");
        assert_eq!(
            clock.sleeps.borrow().len(),
            3,
            "the loop kept sleeping after the stop"
        );
    }

    #[test]
    fn long_waits_sleep_in_short_slices_so_a_stop_is_seen_quickly() {
        let clock = FakeClock::new(Duration::ZERO);
        let schedule = Schedule::new(clock.peek(), 1, 2);

        let (sends, _) = record(&schedule, &clock);

        assert_eq!(sends.len(), 2);
        let sleeps = clock.sleeps.borrow();
        assert!(sleeps.len() >= 99, "{} sleeps", sleeps.len());
        assert!(sleeps.iter().all(|sleep| *sleep <= MAX_SLEEP_SLICE));
    }

    #[test]
    fn sends_are_scheduled_at_i_over_rps() {
        let start = Instant::now();
        let schedule = Schedule::new(start, 200, 5);

        assert_eq!(schedule.total, 1000);
        assert_eq!(Schedule::count(200, 5), 1000);
        assert_eq!(schedule.offset(0), Duration::ZERO);
        assert_eq!(schedule.offset(1), Duration::from_millis(5));
        assert_eq!(schedule.offset(200), Duration::from_secs(1));
        assert_eq!(schedule.offset(999), Duration::from_millis(4995));
        assert_eq!(schedule.due(7), start + Duration::from_millis(35));
    }

    #[test]
    fn schedule_does_not_accumulate_rounding_drift() {
        let schedule = Schedule::new(Instant::now(), 3, 1000);

        assert_eq!(schedule.offset(1), Duration::from_nanos(333_333_333));
        assert_eq!(schedule.offset(2), Duration::from_nanos(666_666_666));
        assert_eq!(schedule.offset(3), Duration::from_secs(1));
        assert_eq!(schedule.offset(2999), Duration::from_nanos(999_666_666_666));
    }

    #[test]
    fn sends_leave_at_their_due_time_never_before() {
        let clock = FakeClock::new(Duration::ZERO);
        let schedule = Schedule::new(clock.peek(), 100, 1);

        let (sends, phase) = record(&schedule, &clock);

        assert_eq!(sends.len(), 100);
        for (i, due, at) in &sends {
            assert_eq!(*due, schedule.due(*i));
            assert!(at >= due, "send {i} left early");
            assert!(*at - *due <= Duration::from_micros(1), "send {i} left late");
        }
        assert!(phase.lag.max_ms < 0.001, "{:?}", phase.lag);
        // The last send is due at 990 ms; the phase still lasts the nominal 1 s.
        assert_eq!(phase.wall, Duration::from_secs(1));
    }

    #[test]
    fn the_loop_sleeps_only_until_the_spin_window_and_spins_the_rest() {
        let clock = FakeClock::new(Duration::ZERO);
        let schedule = Schedule::new(clock.peek(), 100, 1);
        let dues: Vec<Instant> = (0..100).map(|i| schedule.due(i)).collect();

        record(&schedule, &clock);

        let sleep_ends = clock.sleep_ends.borrow();
        assert_eq!(
            sleep_ends.len(),
            99,
            "one sleep before every send but the first"
        );
        // Each sleep ends where the spin window of a due time begins, give or
        // take the fake clock's read ticks.
        for end in sleep_ends.iter() {
            let spin_starts_near = |due: &Instant| {
                let window_start = *due - SPIN_WINDOW;
                *end >= window_start && *end - window_start <= Duration::from_micros(1)
            };
            assert!(
                dues.iter().any(spin_starts_near),
                "a sleep ended at {:?} after start, not {SPIN_WINDOW:?} before a due time",
                *end - schedule.start()
            );
        }
    }

    #[test]
    fn oversleeping_within_the_spin_window_costs_no_lag() {
        let clock = FakeClock::new(Duration::from_micros(1500));
        let schedule = Schedule::new(clock.peek(), 100, 1);

        let (sends, phase) = record(&schedule, &clock);

        for (i, due, at) in &sends {
            assert!(
                at >= due && *at - *due <= Duration::from_micros(1),
                "send {i}"
            );
        }
        assert!(phase.lag.max_ms < 0.001, "{:?}", phase.lag);
    }

    #[test]
    fn late_sends_keep_their_scheduled_time_and_count_as_lag() {
        // Every sleep overshoots the spin window by 1 ms.
        let clock = FakeClock::new(SPIN_WINDOW + Duration::from_millis(1));
        let schedule = Schedule::new(clock.peek(), 100, 1);

        let (sends, phase) = record(&schedule, &clock);

        assert_eq!(sends.len(), 100, "late sends must not be skipped");
        for (i, due, at) in sends.iter().skip(1) {
            assert_eq!(*due, schedule.due(*i));
            let late = *at - *due;
            assert!(
                late >= Duration::from_millis(1) && late <= Duration::from_micros(1001),
                "send {i} left {late:?} late"
            );
        }
        assert!((phase.lag.p99_ms - 1.0).abs() < 0.01, "{:?}", phase.lag);
        assert!((phase.lag.max_ms - 1.0).abs() < 0.01, "{:?}", phase.lag);
    }

    #[test]
    fn overdue_sends_go_out_at_once() {
        let clock = FakeClock::new(Duration::ZERO);
        // The loop starts 55 ms behind: sends 0..=5 are already due.
        let schedule = Schedule::new(clock.peek() - Duration::from_millis(55), 100, 1);
        let resumed = clock.peek();

        let (sends, phase) = record(&schedule, &clock);

        assert_eq!(sends.len(), 100);
        for (i, due, at) in &sends {
            if *i <= 5 {
                assert!(
                    *at - resumed < Duration::from_micros(10),
                    "late send {i} waited"
                );
            } else {
                assert!(
                    at >= due && *at - *due <= Duration::from_micros(1),
                    "send {i}"
                );
            }
        }
        assert!((phase.lag.max_ms - 55.0).abs() < 0.1, "{:?}", phase.lag);
        assert!(phase.lag.p50_ms < 0.001, "{:?}", phase.lag);
    }

    #[test]
    fn send_phase_wall_time_extends_past_nominal_when_behind() {
        let clock = FakeClock::new(Duration::ZERO);
        let schedule = Schedule::new(clock.peek() - Duration::from_secs(3), 10, 1);

        let (_, phase) = record(&schedule, &clock);

        // All ten sends were overdue; the phase ended when the last went out.
        let wall = phase.wall.as_secs_f64();
        assert!((3.0..3.001).contains(&wall), "{wall}");
    }

    #[test]
    fn the_real_clock_sends_on_time() {
        let schedule = Schedule::new(Instant::now(), 1000, 1);

        let phase = run_open_loop(&schedule, &SystemClock, &AtomicBool::new(false), |_, _| {});

        assert!(phase.lag.p50_ms < 1.0, "{:?}", phase.lag);
        assert!(phase.wall >= Duration::from_secs(1));
    }

    #[test]
    fn rate_is_count_over_window() {
        assert_eq!(rate(1000, Duration::from_secs(5)), 200.0);
        assert_eq!(rate(1000, Duration::from_secs(10)), 100.0);
        assert_eq!(rate(750, Duration::from_millis(2500)), 300.0);
        assert_eq!(rate(0, Duration::from_secs(5)), 0.0);
        assert_eq!(rate(10, Duration::ZERO), 0.0);
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
