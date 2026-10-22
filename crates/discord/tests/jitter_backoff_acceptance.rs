//! Sub-cap global-429 wake-spread contract (RED-first).
//!
//! RED now: `two_bot_discord::voice_rooms::jitter_sub_cap_wait` does not
//! exist yet, so this file fails to compile until the Builder adds it. That
//! compile failure IS the red evidence for the missing spread — sub-cap
//! global waits `sleep_until` the shared deadline today, so every guild actor
//! wakes together.
//!
//! Contract (seeded, pure, no `rand` in tests — the same shape as the bot
//! crate's `gateway_backoff` schedule):
//!
//! ```rust,ignore
//! pub fn jitter_sub_cap_wait(wait: Duration, seed: u64) -> Duration;
//! ```
//!
//! - `wait` is the remaining shared-deadline wait (at most the 30 s
//!   `GLOBAL_BACKOFF_MAX`; over-cap waits keep returning `RateLimited` to
//!   the action queue, pinned by the existing
//!   `far_future_global_backoff_returns_bounded_ratelimited` unit test, which
//!   must stay green).
//! - Returns the total sleep: never below `wait` (waking early only earns
//!   another 429), at most `wait + 5 s`, and never above the 30 s cap — so
//!   the cap and the queue-handoff behavior hold by construction.
//! - Distinct seeds spread wake times; the same seed replays the same sleep.
//!
//! Builder contract: implement the function and call it on the sub-cap sleep
//! path. If the signature or bounds need to change, renegotiate on the card
//! — never edit this file to loosen it.

use std::collections::HashSet;
use std::time::Duration;

use two_bot_discord::voice_rooms::jitter_sub_cap_wait;

/// Spread ceiling derived independently of the product code: 5 s of upward
/// spread, hard-capped at the 30 s global-backoff maximum.
fn spread_ceiling(wait: Duration) -> Duration {
    (wait + Duration::from_secs(5)).min(Duration::from_secs(30))
}

#[test]
fn sub_cap_waits_sleep_the_deadline_plus_bounded_spread() {
    for wait in [
        Duration::from_millis(100),
        Duration::from_secs(1),
        Duration::from_secs(10),
        Duration::from_secs(25),
        Duration::from_secs(29),
        Duration::from_secs(30),
    ] {
        let ceiling = spread_ceiling(wait);
        for seed in 0..32u64 {
            let sleep = jitter_sub_cap_wait(wait, seed);
            assert!(
                sleep >= wait && sleep <= ceiling,
                "wait {wait:?} seed {seed}: {sleep:?} outside [{wait:?}, {ceiling:?}]"
            );
        }
    }
}

#[test]
fn sub_cap_waits_spread_wake_times_across_seeds() {
    // A 10 s shared wait wakes every guild actor on the same instant today.
    let distinct: HashSet<Duration> = (0..64u64)
        .map(|seed| jitter_sub_cap_wait(Duration::from_secs(10), seed))
        .collect();
    assert!(
        distinct.len() >= 8,
        "only {} distinct wake times across 64 seeds",
        distinct.len()
    );
}

#[test]
fn sub_cap_waits_are_deterministic_per_seed() {
    assert_eq!(
        jitter_sub_cap_wait(Duration::from_secs(10), 42),
        jitter_sub_cap_wait(Duration::from_secs(10), 42)
    );
}

#[test]
fn zero_wait_stays_zero() {
    for seed in [0u64, 1, 42, u64::MAX] {
        assert_eq!(jitter_sub_cap_wait(Duration::ZERO, seed), Duration::ZERO);
    }
}

#[test]
fn at_cap_waits_are_unjittered() {
    // At exactly the 30 s handoff threshold the spread window is empty, so
    // the sleep equals the wait and the handoff comparison is undisturbed.
    for seed in 0..8u64 {
        assert_eq!(
            jitter_sub_cap_wait(Duration::from_secs(30), seed),
            Duration::from_secs(30)
        );
    }
}
