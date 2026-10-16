//! Restarting supervisor for long-running tasks.
//!
//! Each supervised task is a factory of attempts. An attempt that panics
//! or returns [`Exit::Retry`] is restarted after a backoff delay;
//! [`Exit::Fatal`] ends the task in [`TaskState::Failed`]; [`Exit::Done`]
//! or cancellation ends it in [`TaskState::Stopped`].
//!
//! Backoff reset: an attempt that ran longer than the backoff's max delay
//! was healthy for a while, so the failure that ended it starts the delays
//! over from the first one. Without the reset a task that fails, say, once
//! an hour would wait the max delay after every failure forever.

use std::any::Any;
use std::collections::BTreeMap;
use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;

use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::backoff::Backoff;

/// How one attempt of a supervised task ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// Finished; do not restart.
    Done,
    /// Failed transiently; restart after a backoff delay.
    Retry(String),
    /// Failed permanently; do not restart.
    Fatal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    /// Spawned; the first attempt has not started yet.
    Starting,
    Running,
    /// The last attempt failed; the next one starts at `retry_at`.
    Backoff {
        retry_at: Instant,
        last_error: String,
    },
    /// Ended by [`Exit::Fatal`]; never restarted.
    Failed {
        reason: String,
    },
    /// Ended by [`Exit::Done`], cancellation or [`Supervisor::shutdown`].
    Stopped,
}

/// Runs named tasks, restarts them when they fail and publishes each one's
/// [`TaskState`].
#[derive(Debug, Default)]
pub struct Supervisor {
    tasks: Mutex<Tasks>,
}

#[derive(Debug, Default)]
struct Tasks {
    entries: BTreeMap<Arc<str>, Entry>,
    running: JoinSet<()>,
}

/// One supervised task: its published state, and the token that stops
/// this instance only (a child of the caller's token).
#[derive(Debug)]
struct Entry {
    state: watch::Sender<TaskState>,
    cancel: CancellationToken,
}

impl Supervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Supervises `make`: each call builds one attempt. Cancelling
    /// `cancel` drops the running attempt, or ends the backoff wait, and
    /// stops the task; an attempt that must finish in-flight work on
    /// shutdown watches a token of its own instead.
    ///
    /// Spawning a `name` that is already supervised stops the previous
    /// instance as if its token were cancelled, and the name then lists
    /// the new instance only. The caller's `cancel` is never cancelled by
    /// the supervisor. Must be called from within a Tokio runtime.
    pub fn spawn<F, Fut>(
        &self,
        name: Arc<str>,
        backoff: Backoff,
        cancel: CancellationToken,
        make: F,
    ) where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = Exit> + Send + 'static,
    {
        let state = watch::Sender::new(TaskState::Starting);
        let cancel = cancel.child_token();
        let mut tasks = self.tasks.lock();
        let entry = Entry {
            state: state.clone(),
            cancel: cancel.clone(),
        };
        if let Some(previous) = tasks.entries.insert(name, entry) {
            previous.cancel.cancel();
        }
        // Reap finished tasks so a long-lived supervisor does not
        // accumulate them.
        while tasks.running.try_join_next().is_some() {}
        tasks
            .running
            .spawn(supervise(StopOnDrop(state), backoff, cancel, make));
    }

    /// Every task's name and current state, ordered by name.
    pub fn states(&self) -> Vec<(Arc<str>, TaskState)> {
        self.tasks
            .lock()
            .entries
            .iter()
            .map(|(name, entry)| (name.clone(), entry.state.borrow().clone()))
            .collect()
    }

    pub fn subscribe(&self, name: &str) -> Option<watch::Receiver<TaskState>> {
        self.tasks
            .lock()
            .entries
            .get(name)
            .map(|entry| entry.state.subscribe())
    }

    /// Forgets the task `name` if it has ended, `Stopped` or `Failed`, so
    /// that [`Supervisor::states`] no longer lists it; a task still starting,
    /// running or backing off stays.
    pub fn remove(&self, name: &str) {
        let mut tasks = self.tasks.lock();
        let ended = tasks.entries.get(name).is_some_and(|entry| {
            matches!(
                *entry.state.borrow(),
                TaskState::Stopped | TaskState::Failed { .. }
            )
        });
        if ended {
            tasks.entries.remove(name);
        }
    }

    /// Waits for every task spawned so far to end, until `deadline`; then
    /// aborts the rest and returns how many it aborted. Aborted tasks end
    /// in [`TaskState::Stopped`]. Tasks spawned while this runs are not
    /// waited for.
    pub async fn shutdown(&self, deadline: Instant) -> usize {
        let mut tasks = std::mem::take(&mut self.tasks.lock().running);
        loop {
            match tokio::time::timeout_at(deadline, tasks.join_next()).await {
                Ok(Some(_)) => {}
                Ok(None) => return 0,
                Err(_elapsed) => break,
            }
        }
        tasks.abort_all();
        let mut aborted = 0;
        while let Some(joined) = tasks.join_next().await {
            if joined.is_err_and(|err| err.is_cancelled()) {
                aborted += 1;
            }
        }
        aborted
    }
}

/// Publishes [`TaskState::Stopped`] when dropped unless the task already
/// ended, so an aborted task does not stay `Running` or `Backoff`.
struct StopOnDrop(watch::Sender<TaskState>);

impl StopOnDrop {
    fn set(&self, state: TaskState) {
        self.0.send_replace(state);
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.send_if_modified(|state| {
            let ended = matches!(state, TaskState::Failed { .. } | TaskState::Stopped);
            if !ended {
                *state = TaskState::Stopped;
            }
            !ended
        });
    }
}

async fn supervise<F, Fut>(
    state: StopOnDrop,
    mut backoff: Backoff,
    cancel: CancellationToken,
    mut make: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Exit>,
{
    loop {
        state.set(TaskState::Running);
        let started = Instant::now();
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            outcome = attempt(&mut make) => outcome,
        };
        let last_error = match outcome {
            Ok(Exit::Done) => return,
            Ok(Exit::Fatal(reason)) => {
                state.set(TaskState::Failed { reason });
                return;
            }
            Ok(Exit::Retry(error)) => error,
            Err(payload) => format!("panicked: {}", panic_message(&*payload)),
        };
        if started.elapsed() > backoff.max() {
            backoff.reset();
        }
        let retry_at = Instant::now() + backoff.next_delay(&mut rand::rng());
        state.set(TaskState::Backoff {
            retry_at,
            last_error,
        });
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = tokio::time::sleep_until(retry_at) => {}
        }
    }
}

/// Runs one attempt, turning a panic in `make` or in the attempt itself
/// into `Err(payload)`.
async fn attempt<F, Fut>(make: &mut F) -> Result<Exit, Box<dyn Any + Send>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Exit>,
{
    let mut run = pin!(catch_unwind(AssertUnwindSafe(make))?);
    poll_fn(
        |cx| match catch_unwind(AssertUnwindSafe(|| run.as_mut().poll(cx))) {
            Ok(poll) => poll.map(Ok),
            Err(payload) => Poll::Ready(Err(payload)),
        },
    )
    .await
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "non-string panic payload"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::watch;

    /// Every delay is exactly `initial`: the first ceiling is `initial`
    /// and the floor lifts any draw below it.
    fn fixed_backoff(delay: Duration) -> Backoff {
        Backoff::new(delay, delay, delay)
    }

    async fn wait_until(
        rx: &mut watch::Receiver<TaskState>,
        pred: impl FnMut(&TaskState) -> bool,
    ) -> TaskState {
        rx.wait_for(pred).await.unwrap().clone()
    }

    fn is_running(state: &TaskState) -> bool {
        matches!(state, TaskState::Running)
    }

    fn is_backoff(state: &TaskState) -> bool {
        matches!(state, TaskState::Backoff { .. })
    }

    /// Sets its flag when dropped, to observe an attempt being dropped.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Attempts that never return and raise `dropped` when dropped.
    fn pending_with_flag(
        dropped: Arc<AtomicBool>,
    ) -> impl FnMut() -> std::pin::Pin<Box<dyn Future<Output = Exit> + Send>> + Send + 'static {
        move || {
            let flag = DropFlag(dropped.clone());
            Box::pin(async move {
                let _flag = flag;
                std::future::pending().await
            })
        }
    }

    /// Calls `first` on the first attempt; every later attempt never
    /// returns.
    fn first_then_pending(
        attempts: Arc<AtomicUsize>,
        first: fn() -> Exit,
    ) -> impl FnMut() -> std::pin::Pin<Box<dyn Future<Output = Exit> + Send>> + Send + 'static {
        move || {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    first()
                } else {
                    std::future::pending().await
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_restarts_after_panic_with_backoff() {
        let supervisor = Supervisor::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        supervisor.spawn(
            "source".into(),
            fixed_backoff(Duration::from_secs(2)),
            CancellationToken::new(),
            first_then_pending(attempts.clone(), || panic!("boom")),
        );
        let mut rx = supervisor.subscribe("source").unwrap();

        let TaskState::Backoff {
            retry_at,
            last_error,
        } = wait_until(&mut rx, is_backoff).await
        else {
            unreachable!()
        };
        assert_eq!(retry_at - Instant::now(), Duration::from_secs(2));
        assert!(last_error.contains("boom"), "{last_error}");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        wait_until(&mut rx, is_running).await;
        assert!(Instant::now() >= retry_at);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_retry_restarts() {
        let supervisor = Supervisor::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        supervisor.spawn(
            "source".into(),
            fixed_backoff(Duration::from_secs(1)),
            CancellationToken::new(),
            first_then_pending(attempts.clone(), || Exit::Retry("401 Unauthorized".into())),
        );
        let mut rx = supervisor.subscribe("source").unwrap();

        let state = wait_until(&mut rx, is_backoff).await;
        assert!(
            matches!(&state, TaskState::Backoff { last_error, .. } if last_error == "401 Unauthorized"),
            "{state:?}"
        );
        wait_until(&mut rx, is_running).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            supervisor.states(),
            vec![(Arc::<str>::from("source"), TaskState::Running)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_fatal_does_not_restart() {
        let supervisor = Supervisor::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        supervisor.spawn(
            "source".into(),
            fixed_backoff(Duration::from_millis(10)),
            CancellationToken::new(),
            first_then_pending(attempts.clone(), || Exit::Fatal("bad token".into())),
        );
        let mut rx = supervisor.subscribe("source").unwrap();

        let state = wait_until(&mut rx, |s| matches!(s, TaskState::Failed { .. })).await;
        assert_eq!(
            state,
            TaskState::Failed {
                reason: "bad token".into()
            }
        );
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            *rx.borrow(),
            TaskState::Failed {
                reason: "bad token".into()
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn remove_forgets_finished_tasks_only() {
        let supervisor = Supervisor::new();
        supervisor.spawn(
            "done".into(),
            fixed_backoff(Duration::from_secs(1)),
            CancellationToken::new(),
            || async { Exit::Done },
        );
        supervisor.spawn(
            "failed".into(),
            fixed_backoff(Duration::from_secs(1)),
            CancellationToken::new(),
            || async { Exit::Fatal("bad token".into()) },
        );
        let running = CancellationToken::new();
        supervisor.spawn(
            "running".into(),
            fixed_backoff(Duration::from_secs(1)),
            running.clone(),
            std::future::pending::<Exit>,
        );
        supervisor.spawn(
            "backoff".into(),
            fixed_backoff(Duration::from_secs(60)),
            CancellationToken::new(),
            || async { Exit::Retry("down".into()) },
        );
        let mut done = supervisor.subscribe("done").unwrap();
        wait_until(&mut done, |s| *s == TaskState::Stopped).await;
        let mut failed = supervisor.subscribe("failed").unwrap();
        wait_until(&mut failed, |s| matches!(s, TaskState::Failed { .. })).await;
        let mut rx = supervisor.subscribe("running").unwrap();
        wait_until(&mut rx, is_running).await;
        let mut backoff = supervisor.subscribe("backoff").unwrap();
        wait_until(&mut backoff, is_backoff).await;

        for name in ["done", "failed", "running", "backoff", "unknown"] {
            supervisor.remove(name);
        }
        let names: Vec<Arc<str>> = supervisor.states().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, [Arc::<str>::from("backoff"), Arc::from("running")]);
        assert!(supervisor.subscribe("done").is_none());

        running.cancel();
        wait_until(&mut rx, |s| *s == TaskState::Stopped).await;
        supervisor.remove("running");
        assert!(supervisor.subscribe("running").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_cancel_stops() {
        let supervisor = Supervisor::new();

        // Cancelled while running: the attempt is dropped.
        let running = CancellationToken::new();
        let dropped = Arc::new(AtomicBool::new(false));
        supervisor.spawn(
            "running".into(),
            fixed_backoff(Duration::from_secs(1)),
            running.clone(),
            pending_with_flag(dropped.clone()),
        );
        let mut rx = supervisor.subscribe("running").unwrap();
        wait_until(&mut rx, is_running).await;
        running.cancel();
        wait_until(&mut rx, |s| *s == TaskState::Stopped).await;
        assert!(dropped.load(Ordering::SeqCst));

        // Cancelled while backing off: no restart.
        let backing_off = CancellationToken::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        supervisor.spawn(
            "backing-off".into(),
            fixed_backoff(Duration::from_secs(60)),
            backing_off.clone(),
            first_then_pending(attempts.clone(), || Exit::Retry("down".into())),
        );
        let mut rx = supervisor.subscribe("backing-off").unwrap();
        wait_until(&mut rx, is_backoff).await;
        backing_off.cancel();
        wait_until(&mut rx, |s| *s == TaskState::Stopped).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        // Done also ends in Stopped.
        supervisor.spawn(
            "done".into(),
            fixed_backoff(Duration::from_secs(1)),
            CancellationToken::new(),
            || async { Exit::Done },
        );
        let mut rx = supervisor.subscribe("done").unwrap();
        wait_until(&mut rx, |s| *s == TaskState::Stopped).await;
        assert!(supervisor.subscribe("missing").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_restarts_after_factory_panic() {
        let supervisor = Supervisor::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        supervisor.spawn(
            "source".into(),
            fixed_backoff(Duration::from_secs(3)),
            CancellationToken::new(),
            move || {
                if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("factory boom");
                }
                std::future::pending::<Exit>()
            },
        );
        let mut rx = supervisor.subscribe("source").unwrap();

        let TaskState::Backoff {
            retry_at,
            last_error,
        } = wait_until(&mut rx, is_backoff).await
        else {
            unreachable!()
        };
        assert_eq!(retry_at - Instant::now(), Duration::from_secs(3));
        assert!(last_error.contains("factory boom"), "{last_error}");

        wait_until(&mut rx, is_running).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_respawn_same_name_stops_previous() {
        let supervisor = Supervisor::new();
        let cancel = CancellationToken::new();
        let first_dropped = Arc::new(AtomicBool::new(false));
        supervisor.spawn(
            "a".into(),
            fixed_backoff(Duration::from_secs(1)),
            cancel.clone(),
            pending_with_flag(first_dropped.clone()),
        );
        let mut first = supervisor.subscribe("a").unwrap();
        wait_until(&mut first, is_running).await;

        let second_dropped = Arc::new(AtomicBool::new(false));
        supervisor.spawn(
            "a".into(),
            fixed_backoff(Duration::from_secs(1)),
            cancel.clone(),
            pending_with_flag(second_dropped.clone()),
        );
        wait_until(&mut first, |s| *s == TaskState::Stopped).await;
        assert!(first_dropped.load(Ordering::SeqCst));

        let mut second = supervisor.subscribe("a").unwrap();
        wait_until(&mut second, is_running).await;
        assert!(!second_dropped.load(Ordering::SeqCst));
        assert!(!cancel.is_cancelled());
        assert_eq!(
            supervisor.states(),
            vec![(Arc::<str>::from("a"), TaskState::Running)]
        );

        // The caller's token still stops the replacement.
        cancel.cancel();
        wait_until(&mut second, |s| *s == TaskState::Stopped).await;
        assert!(second_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_shutdown_aborts_after_deadline() {
        let supervisor = Supervisor::new();
        supervisor.spawn(
            "finishes".into(),
            fixed_backoff(Duration::from_secs(1)),
            CancellationToken::new(),
            || async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Exit::Done
            },
        );
        supervisor.spawn(
            "stuck".into(),
            fixed_backoff(Duration::from_secs(1)),
            CancellationToken::new(),
            std::future::pending::<Exit>,
        );
        let start = Instant::now();
        let deadline = start + Duration::from_secs(5);

        assert_eq!(supervisor.shutdown(deadline).await, 1);
        assert_eq!(Instant::now(), deadline);
        assert_eq!(
            supervisor.states(),
            vec![
                (Arc::<str>::from("finishes"), TaskState::Stopped),
                (Arc::<str>::from("stuck"), TaskState::Stopped),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_shutdown_returns_once_all_tasks_finish() {
        let supervisor = Supervisor::new();
        supervisor.spawn(
            "finishes".into(),
            fixed_backoff(Duration::from_secs(1)),
            CancellationToken::new(),
            || async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Exit::Done
            },
        );
        let start = Instant::now();
        assert_eq!(
            supervisor.shutdown(start + Duration::from_secs(60)).await,
            0
        );
        assert_eq!(Instant::now(), start + Duration::from_secs(2));
    }

    /// Every run outlasts the backoff's max delay, so each restart starts
    /// over at the first, deterministic delay. Without the reset the
    /// attempts would climb and the jittered delays would range up to
    /// `max`.
    #[tokio::test(start_paused = true)]
    async fn supervisor_resets_backoff_after_long_run() {
        let supervisor = Supervisor::new();
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(8);
        supervisor.spawn(
            "flapping".into(),
            Backoff::new(initial, max, initial),
            CancellationToken::new(),
            move || async move {
                tokio::time::sleep(max + Duration::from_secs(1)).await;
                Exit::Retry("dropped".into())
            },
        );
        let mut rx = supervisor.subscribe("flapping").unwrap();
        for _ in 0..16 {
            let TaskState::Backoff { retry_at, .. } = wait_until(&mut rx, is_backoff).await else {
                unreachable!()
            };
            assert_eq!(retry_at - Instant::now(), initial);
            wait_until(&mut rx, is_running).await;
        }
    }
}
