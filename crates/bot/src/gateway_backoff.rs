//! Bounded supervised reconnect schedule and Twilight reconnect spread.
//!
//! A dead gateway process exits and the container supervisor restarts it, but
//! a restart hot loop is unbounded work (the October crash-loop week). The
//! supervised runner paces retries on this schedule and gives up the episode
//! after [`ReconnectPolicy`] attempts. Twilight owns its separate receive-loop
//! reconnect backoff; [`wait_reconnect_spread`] adds only a non-growing spread.
//!
//! Growth is `base_delay * 2^attempt`, saturated and capped at `max_delay`,
//! with equal jitter: each wait lies in `[capped/2, capped]`, drawn
//! deterministically from `(seed, attempt)`. Equal jitter (not full jitter)
//! keeps a floor under each wait so a herd of restarting shards cannot
//! collapse onto near-zero synchronized retries, while still decorrelating
//! them. Attempt indices at or past `max_attempts` yield no delay: the type
//! itself carries "no unbounded retry".
//!
//! The binary's supervised runner (which publishes gateway state around this
//! schedule) keeps its own contract tests in
//! `gateway_tests::supervised_reconnect`; the seeded ranges are pinned here
//! and in `tests/jitter_backoff_acceptance.rs`.

use std::time::Duration;

/// First retry waits this long; each later attempt doubles, capped below.
pub const DEFAULT_BASE_DELAY: Duration = Duration::from_secs(1);
/// No single retry ever waits longer than this.
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(30);
/// The runner stops retrying after this many attempts: no unbounded hot loop.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 10;
/// Default jitter seed. Tests pass explicit seeds; production reseeds per run.
pub const DEFAULT_SEED: u64 = 0x9E3779B97F4A7C15;

/// Bounded retry budget for one supervised reconnect episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Delay before the first retry; doubled per attempt, capped at
    /// [`Self::max_delay`].
    pub base_delay: Duration,
    /// Ceiling for any single retry wait.
    pub max_delay: Duration,
    /// Attempts before the runner reports exhaustion. [`reconnect_delay`]
    /// returns `None` at and past this bound.
    pub max_attempts: u32,
    /// Deterministic jitter seed: the same seed replays the same delays, so
    /// tests pin exact timestamps without a clock or a global RNG.
    pub seed: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            base_delay: DEFAULT_BASE_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            seed: DEFAULT_SEED,
        }
    }
}

/// Deterministic 64-bit mixer (splitmix64) for jitter without a global RNG.
///
/// Pure integer arithmetic: the same `(seed, attempt)` pair always yields the
/// same stream, on every platform, with no clock and no `rand` dependency.
/// Shared by the reconnect schedule and the voice-actor load backoff.
pub(crate) fn jitter_stream(seed: u64, attempt: u32) -> u64 {
    let mixed = seed.wrapping_add(u64::from(attempt).wrapping_mul(0x9E3779B97F4A7C15));
    let mut z = mixed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Calculate the non-growing 500 ms–1 s spread before polling Twilight again.
///
/// Twilight already backs off its connection attempts. The attempt index only
/// varies the deterministic draw: it never grows the delay or exhausts a budget.
/// This decision needs no timer; [`wait_reconnect_spread`] owns the wait.
#[must_use]
fn reconnect_spread(seed: u64, attempt: u32) -> Duration {
    Duration::from_millis(500) + Duration::from_nanos(jitter_stream(seed, attempt) % 500_000_001)
}

/// Await the reconnect spread and return the requested wait for bounded logging.
pub async fn wait_reconnect_spread(seed: u64, attempt: u32) -> Duration {
    let delay = reconnect_spread(seed, attempt);
    tokio::time::sleep(delay).await;
    delay
}

/// Wait before retrying `attempt` (0-based), or `None` past the budget.
///
/// Growth is `base_delay * 2^attempt`, saturated and capped at `max_delay`,
/// with equal jitter: the returned delay lies in `[capped/2, capped]`, where
/// the top half is drawn deterministically from `(seed, attempt)`. Equal
/// jitter (not full jitter) keeps a floor under each wait so a herd of
/// restarting shards cannot collapse onto near-zero synchronized retries,
/// while still decorrelating them. Attempt indices at or past `max_attempts`
/// return `None`: the type itself carries "no unbounded retry".
#[must_use]
pub fn reconnect_delay(policy: &ReconnectPolicy, attempt: u32) -> Option<Duration> {
    if attempt >= policy.max_attempts {
        return None;
    }
    // The shift is at most 31, so the multiply factor never overflows u32.
    let grown = policy.base_delay.saturating_mul(1_u32 << attempt.min(31));
    let capped = grown.min(policy.max_delay);
    let half = capped / 2;
    // `span` is at least 1, so the remainder below never divides by zero; it
    // is capped at `u64::MAX` so the cast is exact even for absurd policies.
    let span = half.as_nanos().saturating_add(1).min(u128::from(u64::MAX)) as u64;
    Some(half + Duration::from_nanos(jitter_stream(policy.seed, attempt) % span))
}

#[cfg(test)]
mod schedule_tests {
    use std::time::Duration;

    use super::{
        reconnect_delay, ReconnectPolicy, DEFAULT_BASE_DELAY, DEFAULT_MAX_ATTEMPTS,
        DEFAULT_MAX_DELAY, DEFAULT_SEED,
    };

    #[test]
    fn defaults_carry_the_storm_contract() {
        assert_eq!(DEFAULT_BASE_DELAY, Duration::from_secs(1));
        assert_eq!(DEFAULT_MAX_DELAY, Duration::from_secs(30));
        assert_eq!(DEFAULT_MAX_ATTEMPTS, 10);
        assert_eq!(
            ReconnectPolicy::default(),
            ReconnectPolicy {
                base_delay: Duration::from_secs(1),
                max_delay: Duration::from_secs(30),
                max_attempts: 10,
                seed: DEFAULT_SEED,
            }
        );
    }

    #[test]
    fn schedule_is_jittered_within_cap_and_ends_at_budget() {
        let policy = ReconnectPolicy::default();
        for attempt in 0..DEFAULT_MAX_ATTEMPTS {
            let grown = DEFAULT_BASE_DELAY.saturating_mul(1u32 << attempt.min(31));
            let capped = grown.min(DEFAULT_MAX_DELAY);
            let sample = reconnect_delay(&policy, attempt).expect("within budget");
            assert!(
                sample >= capped / 2 && sample <= capped,
                "attempt {attempt}: {sample:?} outside [{:?}, {capped:?}]",
                capped / 2,
            );
        }
        assert!(reconnect_delay(&policy, DEFAULT_MAX_ATTEMPTS).is_none());
        let replay: Vec<_> = (0..3)
            .map(|attempt| reconnect_delay(&policy, attempt))
            .collect();
        assert_eq!(
            replay,
            (0..3)
                .map(|attempt| reconnect_delay(&policy, attempt))
                .collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod spread_tests {
    use super::{reconnect_spread, Duration, DEFAULT_SEED};

    #[test]
    fn spread_preserves_seeded_draws() {
        for (seed, attempt, nanos) in [
            (0, 0, 570_190_767),
            (1, 0, 798_389_749),
            (0, 1, 773_782_688),
            (DEFAULT_SEED, 2, 961_319_763),
            (u64::MAX, u32::MAX, 621_681_999),
        ] {
            assert_eq!(
                reconnect_spread(seed, attempt),
                Duration::from_nanos(nanos),
                "seed {seed}, attempt {attempt}"
            );
        }
    }

    #[test]
    fn spread_stays_bounded_through_and_past_the_supervised_budget() {
        for attempt in [0, 1, 2, 5, 9, 10, 11, 100, u32::MAX] {
            for seed in [0, 1, 42, u64::MAX] {
                let delay = reconnect_spread(seed, attempt);
                assert!(
                    (Duration::from_millis(500)..=Duration::from_secs(1)).contains(&delay),
                    "seed {seed}, attempt {attempt}: {delay:?} outside [500 ms, 1 s]"
                );
            }
        }
    }
}
