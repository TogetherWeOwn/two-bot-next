//! Retry-jitter acceptance: voice-actor backoff, reconnect schedule, storm bound.
//!
//! Two parts, all offline (no database, no Discord, no clock). Both are RED
//! now at compile time; the behavioral loop-adoption guard lives separately
//! in `reconnect_adoption_guard.rs` so it runs on the unmodified tree.
//!
//! 1. Voice-actor jitter (RED now — compile): the Builder adds
//!    `two_bot::voice_rooms::voice_actor_load_backoff_jittered`, seeded like
//!    the `gateway_backoff` schedule, bounded by today's deterministic
//!    1 s → 60 s progression. The existing private helper and its unit pins
//!    stay untouched as the deterministic base.
//!
//!    ```rust,ignore
//!    pub fn voice_actor_load_backoff_jittered(failed_attempts: u32, seed: u64) -> Duration;
//!    ```
//!
//! 2. Reconnect schedule (RED now — compile): the Builder exposes the pure
//!    schedule from the binary-only `gateway_backoff` module to the library
//!    as `two_bot::gateway_backoff::{ReconnectPolicy, reconnect_delay,
//!    DEFAULT_BASE_DELAY, DEFAULT_MAX_DELAY, DEFAULT_MAX_ATTEMPTS,
//!    DEFAULT_SEED}` (same semantics the binary's `supervised_reconnect`
//!    suite pins internally; `run_supervised` itself may stay in the binary).
//!    These pins assert seeded RANGES, and add the storm bound the internal
//!    suite does not pin.
//!
//! Builder contract: satisfy the ranges. If a signature or bound needs to
//! change, renegotiate on the card — never edit this file to loosen it.

use std::collections::HashSet;
use std::time::Duration;

use two_bot::gateway_backoff::{
    reconnect_delay, ReconnectPolicy, DEFAULT_BASE_DELAY, DEFAULT_MAX_ATTEMPTS, DEFAULT_MAX_DELAY,
    DEFAULT_SEED,
};
use two_bot::voice_rooms::voice_actor_load_backoff_jittered;

// ---------------------------------------------------------------------------
// (1) voice-actor load backoff: seeded jitter within the 1 s → 60 s base
// ---------------------------------------------------------------------------

/// Today's deterministic progression, derived independently of the product
/// code (mirrors the private helper's unit pins): 1 s doubling per
/// consecutive failure, capped at 60 s. `failed_attempts` is 1-based.
fn voice_actor_base(failed_attempts: u32) -> Duration {
    Duration::from_secs(1)
        .saturating_mul(1u32 << failed_attempts.saturating_sub(1).min(6))
        .min(Duration::from_secs(60))
}

#[test]
fn voice_actor_jitter_stays_within_the_deterministic_progression() {
    for failed_attempts in 1..=6u32 {
        let base = voice_actor_base(failed_attempts);
        for seed in 0..32u64 {
            let sample = voice_actor_load_backoff_jittered(failed_attempts, seed);
            assert!(
                sample >= Duration::from_millis(1) && sample <= base,
                "failures {failed_attempts} seed {seed}: {sample:?} outside [1 ms, {base:?}]"
            );
        }
    }
}

#[test]
fn voice_actor_jitter_never_exceeds_the_sixty_second_cap() {
    let cap = Duration::from_secs(60);
    for failed_attempts in [7u32, 8, 10, 100, 1_000, u32::MAX] {
        for seed in [0u64, 1, 42, u64::MAX] {
            let sample = voice_actor_load_backoff_jittered(failed_attempts, seed);
            assert!(
                sample >= Duration::from_millis(1) && sample <= cap,
                "failures {failed_attempts} seed {seed}: {sample:?} outside [1 ms, {cap:?}]"
            );
        }
    }
}

#[test]
fn voice_actor_jitter_spreads_guild_retries_across_seeds() {
    // Three consecutive failures have a 4 s base: after a store outage every
    // guild actor retries in lockstep today.
    let distinct: HashSet<Duration> = (0..64u64)
        .map(|seed| voice_actor_load_backoff_jittered(3, seed))
        .collect();
    assert!(
        distinct.len() >= 8,
        "only {} distinct delays across 64 seeds",
        distinct.len()
    );
}

#[test]
fn voice_actor_jitter_is_deterministic_per_seed() {
    assert_eq!(
        voice_actor_load_backoff_jittered(3, 42),
        voice_actor_load_backoff_jittered(3, 42)
    );
    assert_eq!(
        voice_actor_load_backoff_jittered(1, 7),
        voice_actor_load_backoff_jittered(1, 7)
    );
}

// ---------------------------------------------------------------------------
// (2) reconnect schedule: exposed ranges, seed determinism, budget, storm bound
// ---------------------------------------------------------------------------

#[test]
fn reconnect_defaults_pin_the_storm_contract() {
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
fn reconnect_schedule_carries_equal_jitter_within_cap_windows() {
    // Windows derived independently of `reconnect_delay`: grown base capped
    // at max, sample in [capped/2, capped] (equal jitter keeps a floor under
    // each wait so restarting shards cannot collapse onto near-zero waits).
    for attempt in 0..DEFAULT_MAX_ATTEMPTS {
        let grown = DEFAULT_BASE_DELAY.saturating_mul(1u32 << attempt.min(31));
        let capped = grown.min(DEFAULT_MAX_DELAY);
        let half = capped / 2;
        for seed in 0..16u64 {
            let policy = ReconnectPolicy {
                seed,
                ..ReconnectPolicy::default()
            };
            let sample = reconnect_delay(&policy, attempt)
                .unwrap_or_else(|| panic!("attempt {attempt} unexpectedly past budget"));
            assert!(
                sample >= half && sample <= capped,
                "attempt {attempt} seed {seed}: {sample:?} outside [{half:?}, {capped:?}]"
            );
        }
    }
}

#[test]
fn reconnect_schedule_spreads_shards_across_seeds() {
    let distinct: HashSet<Duration> = (0..64u64)
        .map(|seed| {
            let policy = ReconnectPolicy {
                seed,
                ..ReconnectPolicy::default()
            };
            reconnect_delay(&policy, 0).expect("attempt 0 is within budget")
        })
        .collect();
    assert!(
        distinct.len() >= 8,
        "only {} distinct delays across 64 seeds",
        distinct.len()
    );
}

#[test]
fn reconnect_schedule_is_deterministic_per_seed() {
    let first: Vec<Duration> = (0..5u32)
        .map(|attempt| {
            reconnect_delay(&ReconnectPolicy::default(), attempt).expect("within budget")
        })
        .collect();
    let second: Vec<Duration> = (0..5u32)
        .map(|attempt| {
            reconnect_delay(&ReconnectPolicy::default(), attempt).expect("within budget")
        })
        .collect();
    assert_eq!(first, second);
}

#[test]
fn reconnect_budget_ends_retry() {
    let policy = ReconnectPolicy::default();
    assert!(reconnect_delay(&policy, DEFAULT_MAX_ATTEMPTS - 1).is_some());
    assert!(reconnect_delay(&policy, DEFAULT_MAX_ATTEMPTS).is_none());
    assert!(reconnect_delay(&policy, DEFAULT_MAX_ATTEMPTS + 1).is_none());
    assert!(reconnect_delay(&policy, u32::MAX).is_none());
}

#[test]
fn reconnect_storm_yields_bounded_iterations_per_tick() {
    // Storm bound: the smallest scheduled wait floors the loop rate. 250 ms
    // allows at most 4 iterations per 1 s tick (the 1 s default base with
    // equal jitter floors each wait at 500 ms; the threshold keeps 2x slack
    // for future base tuning). Bare `continue` has no floor at all.
    let mut floor = Duration::MAX;
    for attempt in 0..DEFAULT_MAX_ATTEMPTS {
        for seed in 0..64u64 {
            let policy = ReconnectPolicy {
                seed,
                ..ReconnectPolicy::default()
            };
            let sample = reconnect_delay(&policy, attempt).expect("within budget");
            floor = floor.min(sample);
        }
    }
    assert!(
        floor >= Duration::from_millis(250),
        "storm floor {floor:?} under 250 ms: more than 4 iterations/s possible"
    );
}
