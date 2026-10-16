//! Process exit codes.

/// Ran and shut down cleanly; also `version`, `config schema` and a valid
/// `config validate`.
pub const OK: u8 = 0;
/// The config file cannot be read or is invalid, or tgin could not start.
pub const FAILURE: u8 = 1;
/// Every long-poll source failed permanently (Telegram rejected each
/// token) and no webhook could deliver updates.
pub const ALL_SOURCES_FAILED: u8 = 2;
/// The command line is malformed (`EX_USAGE`).
pub const USAGE: u8 = 64;
/// A second shutdown signal cut the drain short (128 + SIGINT).
pub const FORCED: u8 = 130;

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Shut down on a signal after draining.
    Clean,
    /// Shut down because every source failed permanently.
    AllSourcesFailed,
    /// A second signal forced the exit.
    Forced,
}

impl Outcome {
    pub fn code(self) -> u8 {
        match self {
            Self::Clean => OK,
            Self::AllSourcesFailed => ALL_SOURCES_FAILED,
            Self::Forced => FORCED,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_codes() {
        assert_eq!(Outcome::Clean.code(), 0);
        assert_eq!(Outcome::AllSourcesFailed.code(), 2);
        assert_eq!(Outcome::Forced.code(), 130);
    }
}
