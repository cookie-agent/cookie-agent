//! Delays between retries: one policy type for every retry loop.
//!
//! A [`Backoff`] doubles from `base` up to `cap`, optionally jittered. It
//! only computes delays; callers sleep (on whichever executor they run) and
//! decide how many attempts to make.

use std::time::Duration;

/// How a [`Backoff`] spreads its delays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Jitter {
    /// Every attempt waits exactly its delay.
    None,
    /// Adds up to this much, in whole milliseconds, to each delay, which can
    /// then exceed the cap by that much.
    Add(Duration),
    /// Scales each delay by a per-mille factor between `low` and `high`
    /// inclusive, then keeps it between one millisecond and the cap.
    Scale {
        /// The smallest factor, in thousandths.
        low: u32,
        /// The largest factor, in thousandths.
        high: u32,
    },
}

/// Exponential backoff: attempt `n` (from 0) waits `base * 2^n`, at most
/// `cap`, then [`Jitter`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Backoff {
    /// The first delay.
    pub base: Duration,
    /// The longest delay before jitter.
    pub cap: Duration,
    /// How the delays are spread.
    pub jitter: Jitter,
}

impl Backoff {
    /// Doubles from `base` up to `cap`, without jitter.
    #[must_use]
    pub const fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            jitter: Jitter::None,
        }
    }

    /// Waits `delay` before every attempt.
    #[must_use]
    pub const fn fixed(delay: Duration) -> Self {
        Self::new(delay, delay)
    }

    /// The same backoff with `jitter`.
    #[must_use]
    pub const fn with_jitter(self, jitter: Jitter) -> Self {
        Self { jitter, ..self }
    }

    /// The delay before retry `attempt` (from 0), before jitter: `base`
    /// doubled `attempt` times, at most `cap`.
    #[must_use]
    pub fn delay(&self, attempt: u32) -> Duration {
        let multiplier = 1_u128.checked_shl(attempt).unwrap_or(u128::MAX);
        let nanos = self
            .base
            .as_nanos()
            .saturating_mul(multiplier)
            .min(self.cap.as_nanos());
        from_nanos(nanos)
    }

    /// The delay before retry `attempt` with jitter drawn from `entropy`,
    /// any number the caller has (a clock reading, a random draw, a hash).
    #[must_use]
    pub fn jittered(&self, attempt: u32, entropy: u64) -> Duration {
        let delay = self.delay(attempt);
        match self.jitter {
            Jitter::None => delay,
            Jitter::Add(most) => {
                let span = most.as_millis().saturating_add(1);
                delay.saturating_add(from_millis(u128::from(entropy) % span))
            }
            Jitter::Scale { low, high } => {
                let span = u64::from(high.saturating_sub(low)) + 1;
                let per_mille = u64::from(low) + entropy % span;
                let millis = delay.as_millis().saturating_mul(u128::from(per_mille)) / 1000;
                from_millis(millis.clamp(1, self.cap.as_millis().max(1)))
            }
        }
    }

    /// The un-jittered delays of successive retries, from attempt 0.
    #[must_use]
    pub const fn delays(self) -> Delays {
        Delays {
            backoff: self,
            attempt: 0,
        }
    }
}

/// The delays of a [`Backoff`], one per retry; see [`Backoff::delays`].
#[derive(Clone, Debug)]
pub struct Delays {
    backoff: Backoff,
    attempt: u32,
}

impl Delays {
    /// The next delay; the delays never run out.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.backoff.delay(self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// Starts over from the first delay.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

impl Iterator for Delays {
    type Item = Duration;

    fn next(&mut self) -> Option<Duration> {
        Some(self.next_delay())
    }
}

fn from_nanos(nanos: u128) -> Duration {
    const PER_SECOND: u128 = 1_000_000_000;
    let seconds = u64::try_from(nanos / PER_SECOND).unwrap_or(u64::MAX);
    let subsecond = u32::try_from(nanos % PER_SECOND).expect("below one billion");
    Duration::new(seconds, subsecond)
}

fn from_millis(millis: u128) -> Duration {
    from_nanos(millis.saturating_mul(1_000_000))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Backoff, Jitter};

    const fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn delays_double_from_the_base_up_to_the_cap() {
        let backoff = Backoff::new(ms(100), ms(1_000));
        let delays: Vec<_> = backoff.delays().take(6).collect();
        assert_eq!(
            delays,
            [ms(100), ms(200), ms(400), ms(800), ms(1_000), ms(1_000)]
        );
    }

    #[test]
    fn a_huge_attempt_saturates_at_the_cap() {
        let backoff = Backoff::new(ms(1_000), Duration::from_secs(60));
        assert_eq!(backoff.delay(63), Duration::from_secs(60));
        assert_eq!(backoff.delay(200), Duration::from_secs(60));
        let unbounded = Backoff::new(Duration::from_secs(1), Duration::MAX);
        assert_eq!(unbounded.delay(u32::MAX), Duration::MAX);
    }

    #[test]
    fn a_fixed_backoff_never_grows() {
        let delays: Vec<_> = Backoff::fixed(ms(25)).delays().take(3).collect();
        assert_eq!(delays, [ms(25); 3]);
    }

    #[test]
    fn reset_starts_from_the_base_again() {
        let mut delays = Backoff::new(ms(10), ms(80)).delays();
        assert_eq!(delays.nth(2), Some(ms(40)));
        delays.reset();
        assert_eq!(delays.next(), Some(ms(10)));
    }

    #[test]
    fn additive_jitter_adds_at_most_its_bound() {
        let backoff = Backoff::new(ms(2), ms(40)).with_jitter(Jitter::Add(ms(10)));
        assert_eq!(backoff.jittered(0, 0), ms(2));
        assert_eq!(backoff.jittered(0, 10), ms(12));
        // The jitter wraps at its bound plus one.
        assert_eq!(backoff.jittered(0, 11), ms(2));
        assert_eq!(backoff.jittered(9, 10), ms(50));
    }

    #[test]
    fn scaled_jitter_stays_between_one_millisecond_and_the_cap() {
        let backoff = Backoff::new(ms(1_000), ms(60_000)).with_jitter(Jitter::Scale {
            low: 750,
            high: 1_250,
        });
        assert_eq!(backoff.jittered(0, 0), ms(750));
        assert_eq!(backoff.jittered(0, 500), ms(1_250));
        assert_eq!(backoff.jittered(0, 501), ms(750));
        // Scaled above the cap, it is the cap.
        assert_eq!(backoff.jittered(10, 500), ms(60_000));
        let tiny = Backoff::new(Duration::from_micros(1), ms(5)).with_jitter(Jitter::Scale {
            low: 750,
            high: 1_250,
        });
        assert_eq!(tiny.jittered(0, 0), ms(1));
    }

    #[test]
    fn no_jitter_is_the_plain_delay() {
        let backoff = Backoff::new(ms(50), ms(5_000));
        assert_eq!(backoff.jittered(3, u64::MAX), ms(400));
    }
}
