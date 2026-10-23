//! Lane-jitter acceptance: seeded contention backoffs within the 100 ms base.
//!
//! RED now — compile: the Builder adds a pure jittered backoff to the library
//! (the two sleep sites live in the binary-only `self_role_runtime`, which
//! integration tests cannot import):
//!
//! ```rust,ignore
//! // two_bot::lane_jitter (new library module)
//! pub const LANE_BACKOFF_BASE: Duration = Duration::from_millis(100);
//! pub fn lane_backoff_jittered(retry: u32, seed: u64) -> Duration;
//! ```
//!
//! Full or equal jitter both satisfy these pins: every sample stays within
//! `(0, base]`, so a correct implementation never sleeps past today's 100 ms
//! step and never hot-loops on a zero sleep. `retry` is the zero-based
//! contention count; implementations may ignore it, but call sites MUST vary
//! `(retry, seed)` across sleeps so contenders spread (reviewer: confirm the
//! call sites do not pass a fixed seed — these pins cannot observe that).
//! Seeded like the `gateway_backoff` schedule: deterministic per input.
//!
//! Adoption at both Busy arms is pinned behaviorally in
//! `lane_jitter_adoption_guard.rs`, which runs on the unmodified tree.
//! Offline: no database, no Discord, no clock.
//!
//! Builder contract: satisfy the ranges. If a signature or bound needs to
//! change, renegotiate on the card — never edit this file to loosen it.

use std::collections::HashSet;
use std::time::Duration;

use two_bot::lane_jitter::{lane_backoff_jittered, LANE_BACKOFF_BASE};

/// Today's deterministic base, derived independently of the product code
/// (mirrors the private `LANE_BACKOFF` const): every contention sleep is
/// exactly 100 ms, so contenders retry in lockstep.
const BASE: Duration = Duration::from_millis(100);

#[test]
fn jitter_base_pins_todays_hundred_ms_step() {
    assert_eq!(LANE_BACKOFF_BASE, BASE);
}

#[test]
fn jitter_stays_within_the_base_for_every_retry() {
    for retry in 0..=8u32 {
        for seed in 0..64u64 {
            let sample = lane_backoff_jittered(retry, seed);
            assert!(
                sample > Duration::ZERO && sample <= BASE,
                "retry {retry} seed {seed}: {sample:?} outside (0, {BASE:?}]"
            );
        }
    }
}

#[test]
fn jitter_spreads_contenders_across_seeds() {
    // First contention has the full 100 ms base: after a busy panel every
    // claimant retries in lockstep today.
    let distinct: HashSet<Duration> = (0..64u64)
        .map(|seed| lane_backoff_jittered(0, seed))
        .collect();
    assert!(
        distinct.len() >= 8,
        "only {} distinct delays across 64 seeds",
        distinct.len()
    );
}

#[test]
fn jitter_is_deterministic_per_retry_and_seed() {
    assert_eq!(lane_backoff_jittered(0, 42), lane_backoff_jittered(0, 42));
    assert_eq!(lane_backoff_jittered(3, 7), lane_backoff_jittered(3, 7));
}
