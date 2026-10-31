//! Seeded jitter contract for `backoff_ms_jittered` (RED-first).
//!
//! RED now: `two_bot_core::action_outcomes::backoff_ms_jittered` does not
//! exist yet, so this file fails to compile until the Builder adds it. That
//! compile failure IS the red evidence for the missing jitter — the
//! deterministic [`backoff_ms`] it bounds is pinned GREEN in
//! `jitter_backoff_bounds_acceptance.rs`.
//!
//! Contract (seeded, pure, no `rand` in tests — the same shape as the bot
//! crate's `gateway_backoff` schedule, which jitters from an explicit seed):
//!
//! ```rust,ignore
//! pub fn backoff_ms_jittered(attempt: u32, seed: u64) -> u64;
//! ```
//!
//! - `attempt` is 0-based like [`backoff_ms`]; `seed` selects the jitter
//!   draw deterministically (same seed, same value; distinct seeds spread).
//! - Full or equal jitter, Builder's choice: samples must lie in
//!   `[1, backoff_ms(attempt)]`. Never zero (a zero wait is no backoff) and
//!   never above the deterministic base, so existing caps hold by
//!   construction.
//! - The [`MAX_HTTP_TRIES`] budget is unchanged.
//!
//! Builder contract: implement the function to satisfy these ranges. If the
//! signature or bounds need to change, renegotiate on the card — never edit
//! this file to loosen it.

use std::collections::HashSet;

use two_bot_core::action_outcomes::{backoff_ms, backoff_ms_jittered, MAX_HTTP_TRIES};

#[test]
fn jittered_samples_stay_within_the_deterministic_base() {
    for attempt in 0..8u32 {
        let base = backoff_ms(attempt);
        for seed in 0..32u64 {
            let sample = backoff_ms_jittered(attempt, seed);
            assert!(
                (1..=base).contains(&sample),
                "attempt {attempt} seed {seed}: {sample} outside [1, {base}]"
            );
        }
    }
}

#[test]
fn jittered_samples_spread_across_seeds() {
    // Attempt 3 has a 4 s base: 64 seeds must not collapse onto a few values
    // (fleet-wide 5xx episodes retry in phase today).
    let distinct: HashSet<u64> = (0..64u64)
        .map(|seed| backoff_ms_jittered(3, seed))
        .collect();
    assert!(
        distinct.len() >= 8,
        "only {} distinct delays across 64 seeds",
        distinct.len()
    );
}

#[test]
fn jittered_samples_are_deterministic_per_seed() {
    assert_eq!(backoff_ms_jittered(3, 42), backoff_ms_jittered(3, 42));
    assert_eq!(backoff_ms_jittered(0, 7), backoff_ms_jittered(0, 7));
}

#[test]
fn jittered_samples_never_exceed_the_saturated_cap() {
    let cap = backoff_ms(u32::MAX);
    for seed in [0u64, 1, 42, u64::MAX] {
        let sample = backoff_ms_jittered(u32::MAX, seed);
        assert!(
            (1..=cap).contains(&sample),
            "seed {seed}: {sample} outside [1, {cap}]"
        );
    }
}

#[test]
fn http_try_budget_is_unchanged_by_jitter() {
    assert_eq!(MAX_HTTP_TRIES, 5);
}
