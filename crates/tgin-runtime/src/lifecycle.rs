//! Process lifecycle: phases, OS signals and shutdown timing.
//!
//! The shutdown sequence a binary drives with these pieces: the first
//! terminating signal moves the [`Lifecycle`] to [`Phase::Draining`]
//! (readiness turns 503), the lame duck passes, the ingest token is
//! cancelled, the backlog drains for up to the grace period, and the phase
//! becomes [`Phase::Stopped`]. A second terminating signal at any point of
//! the drain cancels the force token: exit now, without finishing it.

use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Where the process is in its life. Phases only move forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    Starting,
    Running,
    Draining,
    Stopped,
}

/// A signal or console control event delivered to the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignalKind {
    /// SIGTERM.
    Terminate,
    /// SIGINT on Unix, `CTRL_C_EVENT` on Windows.
    Interrupt,
    /// SIGHUP: reload the configuration; does not shut down.
    Hangup,
    /// `CTRL_BREAK_EVENT`, also what `GenerateConsoleCtrlEvent` delivers to
    /// a process group started with `CREATE_NEW_PROCESS_GROUP`.
    CtrlBreak,
    /// `CTRL_CLOSE_EVENT`: the console window is closing.
    CtrlClose,
    /// `CTRL_SHUTDOWN_EVENT`: the system is shutting down.
    CtrlShutdown,
}

impl SignalKind {
    /// Whether the signal asks the process to shut down. The first one
    /// starts the drain; a second one forces exit.
    pub fn is_terminating(self) -> bool {
        !matches!(self, SignalKind::Hangup)
    }
}

/// Room for signals the consumer has not taken yet. A full channel delays
/// delivery; it never unregisters a listener.
const SIGNAL_CHANNEL_CAPACITY: usize = 16;

/// The process's signals as a stream.
pub struct Signals {
    rx: mpsc::Receiver<SignalKind>,
}

impl Signals {
    /// Registers the OS listeners: SIGTERM, SIGINT and SIGHUP on Unix;
    /// Ctrl-C, Ctrl-Break, console close and system shutdown on Windows.
    /// Each listener runs in its own task for the rest of the process,
    /// also after this `Signals` is dropped: on Windows an unregistered
    /// listener hands the next Ctrl-C to the default handler, which kills
    /// the process outright. Fails if any listener cannot be registered;
    /// the ones registered before the failure stay registered.
    ///
    /// Must be called from within a Tokio runtime with the IO and signal
    /// drivers enabled.
    pub fn install() -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel(SIGNAL_CHANNEL_CAPACITY);
        register(&tx)?;
        Ok(Self { rx })
    }

    /// Signals from `rx` instead of the OS, for tests and in-process
    /// injection.
    pub fn from_channel(rx: mpsc::Receiver<SignalKind>) -> Self {
        Self { rx }
    }

    /// The next signal, or `None` once every sender is gone, which never
    /// happens for [`Signals::install`].
    pub async fn recv(&mut self) -> Option<SignalKind> {
        self.rx.recv().await
    }
}

/// Spawns a task that forwards every event of `$listener` as `$kind`. The
/// loop keeps the listener alive even when the receiver is gone.
macro_rules! forward {
    ($listener:expr, $kind:expr, $tx:expr) => {{
        let mut listener = $listener;
        let tx = $tx.clone();
        tokio::spawn(async move {
            while listener.recv().await.is_some() {
                // A closed channel fails at once; ignoring it keeps the
                // listener registered.
                let _ = tx.send($kind).await;
            }
        });
    }};
}

#[cfg(unix)]
fn register(tx: &mpsc::Sender<SignalKind>) -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind as Unix, signal};

    forward!(signal(Unix::terminate())?, SignalKind::Terminate, tx);
    forward!(signal(Unix::interrupt())?, SignalKind::Interrupt, tx);
    forward!(signal(Unix::hangup())?, SignalKind::Hangup, tx);
    Ok(())
}

/// For close and shutdown, tokio holds the OS handler open, so the process
/// keeps running only until Windows' own kill deadline (see
/// [`ShutdownTiming::for_signal`]).
#[cfg(windows)]
fn register(tx: &mpsc::Sender<SignalKind>) -> std::io::Result<()> {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};

    forward!(ctrl_c()?, SignalKind::Interrupt, tx);
    forward!(ctrl_break()?, SignalKind::CtrlBreak, tx);
    forward!(ctrl_close()?, SignalKind::CtrlClose, tx);
    forward!(ctrl_shutdown()?, SignalKind::CtrlShutdown, tx);
    Ok(())
}

/// How long shutdown keeps serving before it stops ingest (`lame_duck`),
/// then how long it lets the backlog drain (`grace`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownTiming {
    pub lame_duck: Duration,
    pub grace: Duration,
}

impl Default for ShutdownTiming {
    /// 5 s + 15 s (+ the final 1 s HTTP close) fits the common 30 s
    /// orchestrator (k8s/systemd) kill window.
    fn default() -> Self {
        Self {
            lame_duck: Duration::from_secs(5),
            grace: Duration::from_secs(15),
        }
    }
}

impl ShutdownTiming {
    /// Windows kills the process about 5 s after `CTRL_CLOSE_EVENT`, so
    /// closing the console skips the lame duck and caps the grace at 4 s to
    /// finish inside that window. Every other signal keeps `self`.
    pub fn for_signal(self, s: SignalKind) -> Self {
        match s {
            SignalKind::CtrlClose => Self {
                lame_duck: Duration::ZERO,
                grace: self.grace.min(Duration::from_secs(4)),
            },
            _ => self,
        }
    }
}

/// The process phase plus the two shutdown tokens. The tokens returned by
/// [`Lifecycle::ingest_token`] and [`Lifecycle::force_token`] are clones
/// that share state: cancelling any clone cancels them all.
#[derive(Debug)]
pub struct Lifecycle {
    phase: watch::Sender<Phase>,
    ingest: CancellationToken,
    force: CancellationToken,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifecycle {
    pub fn new() -> Self {
        Self {
            phase: watch::Sender::new(Phase::Starting),
            ingest: CancellationToken::new(),
            force: CancellationToken::new(),
        }
    }

    pub fn phase(&self) -> Phase {
        *self.phase.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<Phase> {
        self.phase.subscribe()
    }

    pub fn set_running(&self) {
        self.advance(Phase::Running);
    }

    /// Enters [`Phase::Draining`]. Ingest keeps running until the ingest
    /// token is cancelled, after the lame duck.
    pub fn begin_drain(&self) {
        self.advance(Phase::Draining);
    }

    pub fn set_stopped(&self) {
        self.advance(Phase::Stopped);
    }

    /// Cancelled when ingress must stop accepting updates.
    pub fn ingest_token(&self) -> CancellationToken {
        self.ingest.clone()
    }

    /// Cancelled on the second terminating signal: abandon the drain.
    pub fn force_token(&self) -> CancellationToken {
        self.force.clone()
    }

    /// Moves to `to` if it is later than the current phase; never back.
    fn advance(&self, to: Phase) {
        self.phase.send_if_modified(|phase| {
            let later = to > *phase;
            if later {
                *phase = to;
            }
            later
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_phase_transitions() {
        let lifecycle = Lifecycle::new();
        let mut rx = lifecycle.subscribe();
        assert_eq!(lifecycle.phase(), Phase::Starting);
        assert!(!lifecycle.ingest_token().is_cancelled());
        assert!(!lifecycle.force_token().is_cancelled());

        lifecycle.set_running();
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), Phase::Running);

        lifecycle.begin_drain();
        assert_eq!(*rx.borrow_and_update(), Phase::Draining);
        // Draining does not stop ingest by itself: the lame duck comes first.
        assert!(!lifecycle.ingest_token().is_cancelled());

        // Phases never move backwards.
        lifecycle.set_running();
        assert_eq!(lifecycle.phase(), Phase::Draining);
        assert!(!rx.has_changed().unwrap());

        lifecycle.ingest_token().cancel();
        assert!(lifecycle.ingest_token().is_cancelled());
        assert!(!lifecycle.force_token().is_cancelled());
        lifecycle.force_token().cancel();
        assert!(lifecycle.force_token().is_cancelled());

        lifecycle.set_stopped();
        assert_eq!(*rx.borrow_and_update(), Phase::Stopped);
        lifecycle.begin_drain();
        assert_eq!(lifecycle.phase(), Phase::Stopped);
    }

    #[test]
    fn timing_ctrl_close_caps_grace() {
        let timing = ShutdownTiming::default();
        assert_eq!(
            timing,
            ShutdownTiming {
                lame_duck: Duration::from_secs(5),
                grace: Duration::from_secs(15),
            }
        );
        assert_eq!(
            timing.for_signal(SignalKind::CtrlClose),
            ShutdownTiming {
                lame_duck: Duration::ZERO,
                grace: Duration::from_secs(4),
            }
        );
        let short = ShutdownTiming {
            lame_duck: Duration::from_secs(1),
            grace: Duration::from_secs(2),
        };
        assert_eq!(
            short.for_signal(SignalKind::CtrlClose),
            ShutdownTiming {
                lame_duck: Duration::ZERO,
                grace: Duration::from_secs(2),
            }
        );
        for kind in [
            SignalKind::Terminate,
            SignalKind::Interrupt,
            SignalKind::Hangup,
            SignalKind::CtrlBreak,
            SignalKind::CtrlShutdown,
        ] {
            assert_eq!(timing.for_signal(kind), timing, "{kind:?}");
        }
    }

    #[test]
    fn only_hangup_is_not_terminating() {
        assert!(!SignalKind::Hangup.is_terminating());
        for kind in [
            SignalKind::Terminate,
            SignalKind::Interrupt,
            SignalKind::CtrlBreak,
            SignalKind::CtrlClose,
            SignalKind::CtrlShutdown,
        ] {
            assert!(kind.is_terminating(), "{kind:?}");
        }
    }

    #[tokio::test]
    async fn signals_from_channel_delivers_in_order() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let mut signals = Signals::from_channel(rx);
        for kind in [
            SignalKind::Interrupt,
            SignalKind::Hangup,
            SignalKind::CtrlClose,
        ] {
            tx.send(kind).await.unwrap();
        }
        drop(tx);
        assert_eq!(signals.recv().await, Some(SignalKind::Interrupt));
        assert_eq!(signals.recv().await, Some(SignalKind::Hangup));
        assert_eq!(signals.recv().await, Some(SignalKind::CtrlClose));
        assert_eq!(signals.recv().await, None);
    }

    /// Registration only: delivering a real signal needs either unsafe code
    /// or another process, so delivery is covered by the end-to-end tests.
    #[tokio::test]
    async fn install_succeeds() {
        let signals = Signals::install();
        assert!(signals.is_ok(), "{:?}", signals.err());
    }
}
