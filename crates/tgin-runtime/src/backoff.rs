//! Full-jitter exponential backoff.

use std::time::Duration;

use rand::RngExt;

/// Exponential backoff with full jitter and a lower bound.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    floor: Duration,
    attempt: u32,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration, floor: Duration) -> Self {
        Self {
            initial,
            max,
            floor,
            attempt: 0,
        }
    }

    /// `max(floor, uniform(0, min(max, initial * 2^attempt)))`, then counts
    /// the attempt.
    pub fn next_delay(&mut self, rng: &mut impl rand::Rng) -> Duration {
        let factor = 2u32.saturating_pow(self.attempt);
        let ceiling = self.initial.saturating_mul(factor).min(self.max);
        let ceiling_nanos = u64::try_from(ceiling.as_nanos()).unwrap_or(u64::MAX);
        let drawn = Duration::from_nanos(rng.random_range(0..=ceiling_nanos));
        self.attempt = self.attempt.saturating_add(1);
        drawn.max(self.floor)
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

/// The longer of `computed` and the server's `retry_after`, the latter capped
/// at `cap`.
pub fn honor_retry_after(
    computed: Duration,
    retry_after: Option<Duration>,
    cap: Duration,
) -> Duration {
    match retry_after {
        Some(ra) => computed.max(ra.min(cap)),
        None => computed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    const INITIAL: Duration = Duration::from_millis(100);
    const MAX: Duration = Duration::from_secs(10);
    const FLOOR: Duration = Duration::from_millis(20);

    #[test]
    fn backoff_full_jitter_bounds() {
        let mut rng = StdRng::seed_from_u64(42);
        let mut b = Backoff::new(INITIAL, MAX, FLOOR);
        let mut capped = Vec::new();
        for n in 0..1000u32 {
            assert_eq!(b.attempt(), n);
            let ceiling = INITIAL
                .saturating_mul(2u32.saturating_pow(n.min(31)))
                .min(MAX);
            let d = b.next_delay(&mut rng);
            assert!(
                d >= FLOOR && d <= ceiling.max(FLOOR),
                "draw {n}: {d:?} outside [{FLOOR:?}, {ceiling:?}]"
            );
            if ceiling == MAX {
                capped.push(d);
            }
        }
        // Jitter spreads over the whole capped range rather than clustering.
        assert!(capped.len() > 900);
        assert!(capped.iter().any(|&d| d > MAX / 2));
        assert!(capped.iter().any(|&d| d < MAX / 4));
        capped.sort_unstable();
        capped.dedup();
        assert!(capped.len() >= 100, "{} distinct draws", capped.len());
    }

    #[test]
    fn backoff_reset() {
        let mut rng = StdRng::seed_from_u64(7);
        let mut b = Backoff::new(INITIAL, MAX, FLOOR);
        for _ in 0..5 {
            b.next_delay(&mut rng);
        }
        assert_eq!(b.attempt(), 5);
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert!(b.next_delay(&mut rng) <= INITIAL);
    }

    #[test]
    fn retry_after_wins_when_longer_and_is_capped() {
        let s = Duration::from_secs;
        assert_eq!(honor_retry_after(s(2), Some(s(5)), s(60)), s(5));
        assert_eq!(honor_retry_after(s(2), Some(s(500)), s(60)), s(60));
        assert_eq!(honor_retry_after(s(9), Some(s(5)), s(60)), s(9));
        assert_eq!(honor_retry_after(s(2), None, s(60)), s(2));
    }
}
