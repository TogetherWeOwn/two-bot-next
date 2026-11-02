//! Retry-budget and deterministic-base pins for the jitter rollout.
//!
//! GREEN-now controls: everything here passes on the unmodified tree and must
//! stay green after the Builder adds jitter. The deterministic [`backoff_ms`]
//! sequence is the jitter BOUND — jittered samples (pinned in
//! `jitter_backoff_jittered_acceptance.rs`) must never exceed it — and the
//! [`MAX_HTTP_TRIES`] budget is unchanged by jitter. Pure and offline: no
//! database, no network, no clock.
//!
//! These pins deliberately overlap the exact-value pins in
//! `pacing_backoff_acceptance.rs`: that file is Builder-editable, this one is
//! protected, so the base and budget stay pinned no matter how the Builder
//! reshapes the WIRED pins.
//!
//! Builder contract: keep these pins byte-identical. If jitter forces a bound
//! change, renegotiate on the card — never edit this file to loosen it.

use two_bot_core::action_outcomes::{backoff_ms, BACKOFF_BASE_MS, MAX_HTTP_TRIES};

#[test]
fn deterministic_base_sequence_is_the_jitter_bound() {
    assert_eq!(BACKOFF_BASE_MS, 500);
    let waits: Vec<u64> = (0..MAX_HTTP_TRIES).map(backoff_ms).collect();
    assert_eq!(waits, vec![500, 1_000, 2_000, 4_000, 8_000]);
}

#[test]
fn deterministic_base_saturates_instead_of_overflowing() {
    assert_eq!(backoff_ms(20), BACKOFF_BASE_MS * (1u64 << 20));
    assert_eq!(backoff_ms(u32::MAX), BACKOFF_BASE_MS * (1u64 << 20));
}

#[test]
fn http_try_budget_is_five() {
    assert_eq!(MAX_HTTP_TRIES, 5);
}
