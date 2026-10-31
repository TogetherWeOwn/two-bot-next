//! Seeded jitter contract for `backoff_duration_jittered` (RED-first).
//!
//! RED now: `two_bot_cutover::rest::backoff_duration_jittered` does not exist
//! yet, so this file fails to compile until the Builder adds it. That compile
//! failure IS the red evidence for the missing jitter — the private
//! `backoff_duration` helper it bounds is deterministic `500 ms * 2^attempt`
//! today, so cutover 5xx retries re-hammer in lockstep.
//!
//! Contract (seeded, pure, no `rand` in tests — the same shape as the bot
//! crate's `gateway_backoff` schedule):
//!
//! ```rust,ignore
//! pub fn backoff_duration_jittered(attempt: u32, seed: u64) -> Duration;
//! ```
//!
//! - `attempt` is 0-based like `backoff_duration`; `seed` selects the jitter
//!   draw deterministically (same seed, same value; distinct seeds spread).
//! - Full or equal jitter, Builder's choice: samples must lie in
//!   `[1 ms, 500 ms * 2^min(attempt, 10)]`. Never zero and never above the
//!   deterministic base, so the existing 512 s saturation cap holds by
//!   construction.
//! - The existing private helper stays as the deterministic base; the retry
//!   budgets at its call sites are unchanged.
//!
//! Builder contract: implement the function to satisfy these ranges. If the
//! signature or bounds need to change, renegotiate on the card — never edit
//! this file to loosen it.

use std::collections::HashSet;
use std::time::Duration;

use two_bot_cutover::rest::backoff_duration_jittered;

/// Today's deterministic base, derived independently of the product code:
/// `500 ms * 2^attempt`, saturating the shift at 10 (512 s cap).
fn deterministic_base(attempt: u32) -> Duration {
    Duration::from_millis(500u64.saturating_mul(1u64 << attempt.min(10)))
}

#[test]
fn jittered_samples_stay_within_the_deterministic_base() {
    for attempt in 0..8u32 {
        let base = deterministic_base(attempt);
        for seed in 0..32u64 {
            let sample = backoff_duration_jittered(attempt, seed);
            assert!(
                sample >= Duration::from_millis(1) && sample <= base,
                "attempt {attempt} seed {seed}: {sample:?} outside [1 ms, {base:?}]"
            );
        }
    }
}

#[test]
fn jittered_samples_spread_across_seeds() {
    // Attempt 3 has a 4 s base: cutover 5xx retries re-hammer in phase today.
    let distinct: HashSet<Duration> = (0..64u64)
        .map(|seed| backoff_duration_jittered(3, seed))
        .collect();
    assert!(
        distinct.len() >= 8,
        "only {} distinct delays across 64 seeds",
        distinct.len()
    );
}

#[test]
fn jittered_samples_are_deterministic_per_seed() {
    assert_eq!(
        backoff_duration_jittered(3, 42),
        backoff_duration_jittered(3, 42)
    );
    assert_eq!(
        backoff_duration_jittered(0, 7),
        backoff_duration_jittered(0, 7)
    );
}

#[test]
fn jittered_samples_never_exceed_the_saturated_cap() {
    let cap = deterministic_base(u32::MAX);
    assert_eq!(cap, Duration::from_secs(512));
    for seed in [0u64, 1, 42, u64::MAX] {
        let sample = backoff_duration_jittered(u32::MAX, seed);
        assert!(
            sample >= Duration::from_millis(1) && sample <= cap,
            "seed {seed}: {sample:?} outside [1 ms, {cap:?}]"
        );
    }
}
