//! The waits Telegram asks for and the backoff tgin keeps on its own.
//!
//! - [`Wait::parse`] reads the `X` of an error such as `FLOOD_WAIT_X`,
//!   `FLOOD_PREMIUM_WAIT_X` or `SLOWMODE_WAIT_X`: the one parser of those
//!   messages.
//! - [`Backoff`] is the delay between attempts that fail: 1 s, doubling up
//!   to 60 s, back to 1 s once one succeeds. The sender's reconnects, the
//!   update manager's failed and refused fetches, media setups and the
//!   account supervisor's restarts all use it.

use std::time::Duration;

/// Which wait an error message asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitKind {
    /// `FLOOD_WAIT_X`.
    Flood,
    /// `FLOOD_PREMIUM_WAIT_X`.
    PremiumFlood,
    /// `SLOWMODE_WAIT_X`.
    SlowMode,
    /// Another message ending in `_X` (`FLOOD_TEST_PHONE_WAIT_X`…).
    Other,
}

/// A wait of `seconds` that an error message asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wait {
    pub kind: WaitKind,
    pub seconds: u64,
}

impl Wait {
    /// The wait `message` asks for: the number after its last `_` (the
    /// whole message without one), and the kind its head names. `None`
    /// when that is no number.
    #[must_use]
    pub fn parse(message: &str) -> Option<Self> {
        let (head, x) = message.rsplit_once('_').unwrap_or(("", message));
        let seconds = x.parse().ok()?;
        let kind = match head {
            "FLOOD_WAIT" => WaitKind::Flood,
            "FLOOD_PREMIUM_WAIT" => WaitKind::PremiumFlood,
            "SLOWMODE_WAIT" => WaitKind::SlowMode,
            _ => WaitKind::Other,
        };
        Some(Self { kind, seconds })
    }
}

/// The delay before the next attempt after a failure: [`Backoff::FIRST`]
/// after the first, doubling per failure up to [`Backoff::MAX`];
/// [`reset`](Backoff::reset) once an attempt succeeds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backoff {
    /// The last delay given; zero before any or after a reset.
    last: Duration,
}

impl Backoff {
    pub const FIRST: Duration = Duration::from_secs(1);
    pub const MAX: Duration = Duration::from_secs(60);

    /// A failure: the delay before the next attempt.
    #[allow(
        clippy::should_implement_trait,
        reason = "never ends, so a Duration, not an iterator's Option"
    )]
    pub fn next(&mut self) -> Duration {
        self.last = if self.last.is_zero() {
            Self::FIRST
        } else {
            (self.last * 2).min(Self::MAX)
        };
        self.last
    }

    /// A success: the next failure waits [`Backoff::FIRST`] again.
    pub fn reset(&mut self) {
        self.last = Duration::ZERO;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_the_cap_and_resets() {
        let mut b = Backoff::default();
        let delays: Vec<u64> = (0..9).map(|_| b.next().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        b.reset();
        assert_eq!(b.next(), Backoff::FIRST);
        assert_eq!(b.next(), Duration::from_secs(2));
        // A reset mid-way starts over too.
        b.reset();
        assert_eq!(b.next(), Backoff::FIRST);
    }

    #[test]
    fn waits_are_read_from_the_last_number() {
        let wait = |kind, seconds| Some(Wait { kind, seconds });
        assert_eq!(Wait::parse("FLOOD_WAIT_30"), wait(WaitKind::Flood, 30));
        assert_eq!(
            Wait::parse("FLOOD_PREMIUM_WAIT_5"),
            wait(WaitKind::PremiumFlood, 5)
        );
        assert_eq!(Wait::parse("SLOWMODE_WAIT_9"), wait(WaitKind::SlowMode, 9));
        assert_eq!(
            Wait::parse("FLOOD_TEST_PHONE_WAIT_7"),
            wait(WaitKind::Other, 7)
        );
        assert_eq!(
            Wait::parse("FLOOD_WAIT_18446744073709551615"),
            wait(WaitKind::Flood, u64::MAX)
        );
        assert_eq!(Wait::parse("FLOOD_WAIT_1_2"), wait(WaitKind::Other, 2));
        // No `_`: the whole message is the number, as the sender always read it.
        assert_eq!(Wait::parse("30"), wait(WaitKind::Other, 30));
        assert_eq!(Wait::parse("FLOOD_WAIT_"), None);
        assert_eq!(Wait::parse("FLOOD_WAIT_X"), None);
        assert_eq!(Wait::parse("FROZEN_METHOD_INVALID"), None);
    }
}
