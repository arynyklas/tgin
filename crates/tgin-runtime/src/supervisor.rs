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
    states: Mutex<BTreeMap<Arc<str>, watch::Sender<TaskState>>>,
    tasks: Mutex<JoinSet<()>>,
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
    /// A `name` that is already supervised gets the new task's state; the
    /// old task keeps running but is no longer listed. Must be called from
    /// within a Tokio runtime.
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
        self.states.lock().insert(name, state.clone());
        let mut tasks = self.tasks.lock();
        // Reap finished tasks so a long-lived supervisor does not
        // accumulate them.
        while tasks.try_join_next().is_some() {}
        tasks.spawn(supervise(StopOnDrop(state), backoff, cancel, make));
    }

    /// Every task's name and current state, ordered by name.
    pub fn states(&self) -> Vec<(Arc<str>, TaskState)> {
        self.states
            .lock()
            .iter()
            .map(|(name, state)| (name.clone(), state.borrow().clone()))
            .collect()
    }

    pub fn subscribe(&self, name: &str) -> Option<watch::Receiver<TaskState>> {
        self.states.lock().get(name).map(watch::Sender::subscribe)
    }

    /// Waits for every task spawned so far to end, until `deadline`; then
    /// aborts the rest and returns how many it aborted. Aborted tasks end
    /// in [`TaskState::Stopped`]. Tasks spawned while this runs are not
    /// waited for.
    pub async fn shutdown(&self, deadline: Instant) -> usize {
        let mut tasks = std::mem::take(&mut *self.tasks.lock());
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
    async fn supervisor_cancel_stops() {
        let supervisor = Supervisor::new();

        // Cancelled while running: the attempt is dropped.
        let running = CancellationToken::new();
        supervisor.spawn(
            "running".into(),
            fixed_backoff(Duration::from_secs(1)),
            running.clone(),
            std::future::pending::<Exit>,
        );
        let mut rx = supervisor.subscribe("running").unwrap();
        wait_until(&mut rx, is_running).await;
        running.cancel();
        wait_until(&mut rx, |s| *s == TaskState::Stopped).await;

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
