//! Jittered lane-contention backoff for self-role panel claims.
//!
//! Two contenders that lose a panel race used to sleep the bare 100 ms
//! [`LANE_BACKOFF_BASE`] step and retry in exact lockstep, so a busy panel
//! re-collided every round until the 20 s lane deadline. These helpers keep
//! the pure-const + deadline shape and only spread the sleeps: every sample
//! stays within `(0, base]`, deterministically derived from `(retry, seed)`
//! with no clock and no global RNG, mirroring the `gateway_backoff` schedule.
//!
//! Equal jitter (not full jitter): samples land in `[base/2, base]`, so the
//! floor keeps a herd of contenders from collapsing onto near-zero
//! synchronized retries while still decorrelating them. Call sites MUST vary
//! `(retry, seed)` across sleeps — a fixed seed replays one delay and the
//! lockstep returns. The adoption guard
//! (`crates/bot/tests/lane_jitter_adoption_guard.rs`) pins both Busy arms to
//! this function; the seeded distribution is pinned in
//! `crates/bot/tests/lane_jitter_acceptance.rs`.

use std::time::Duration;

/// Base contention step in milliseconds: the integer the jitter samples, and
/// the single source of [`LANE_BACKOFF_BASE`].
const LANE_BACKOFF_BASE_MS: u64 = 100;

/// Base contention step: every jittered sleep stays within this bound.
/// Mirrors the private `LANE_BACKOFF` const at the two Busy arms.
pub const LANE_BACKOFF_BASE: Duration = Duration::from_millis(LANE_BACKOFF_BASE_MS);

/// Deterministic 64-bit mixer (splitmix64) for jitter without a global RNG.
///
/// Pure integer arithmetic: the same `(seed, retry)` pair always yields the
/// same stream, on every platform, with no clock and no `rand` dependency.
/// Same construction as `gateway_backoff`'s mixer.
fn jitter_stream(seed: u64, retry: u32) -> u64 {
    let mixed = seed.wrapping_add(u64::from(retry).wrapping_mul(0x9E3779B97F4A7C15));
    let mut z = mixed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Jittered sleep for contention `retry` (0-based) under `seed`.
///
/// Returns a delay in `[base/2, base]` milliseconds — never zero (no hot
/// loop), never past today's 100 ms step (deadlines and Busy/Rejected
/// semantics unchanged). Deterministic per input so seeded tests pin range
/// bounds without a clock.
#[must_use]
pub fn lane_backoff_jittered(retry: u32, seed: u64) -> Duration {
    let base = LANE_BACKOFF_BASE_MS;
    let half = base / 2;
    let span = base - half + 1;
    let extra = jitter_stream(seed, retry) % span;
    Duration::from_millis(half + extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_stay_within_the_base() {
        for retry in 0..=8u32 {
            for seed in 0..64u64 {
                let sample = lane_backoff_jittered(retry, seed);
                assert!(sample > Duration::ZERO && sample <= LANE_BACKOFF_BASE);
            }
        }
    }
}
