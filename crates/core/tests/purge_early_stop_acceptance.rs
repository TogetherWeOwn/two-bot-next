//! Acceptance: a purge that stops early says why, in the member reply.
//!
//! Sometimes Discord refuses or rate-limits a delete part-way through a
//! purge after some messages are already gone. The purge then stops and
//! reports the count deleted so far. That early stop must be visible: the
//! reply carries a fixed-vocabulary stop reason next to the count, while a
//! purge that ran to completion keeps today's reply text byte for byte.
//!
//! Contract pinned here (framework-free, public API only):
//! `PurgeStopReason::{Refused, RateLimited}` with `name()` returning
//! `"refused"` / `"rate_limited"`, and `ChannelOutcome::Purged` carrying
//! `stop_reason: Option<PurgeStopReason>` alongside `affected`.

use two_bot_core::channel_moderation::{moderation_result_text, ChannelOutcome, PurgeStopReason};

#[test]
fn stop_reason_names_are_the_fixed_vocabulary() {
    assert_eq!(PurgeStopReason::Refused.name(), "refused");
    assert_eq!(PurgeStopReason::RateLimited.name(), "rate_limited");
}

#[test]
fn full_purge_reply_text_is_unchanged() {
    let outcome = ChannelOutcome::Purged {
        affected: 5,
        stop_reason: None,
    };
    assert_eq!(outcome.name(), "purged");
    assert_eq!(
        moderation_result_text(outcome),
        "Moderation action completed: purged (5)."
    );
}

#[test]
fn early_stop_reply_text_carries_the_count_and_the_reason() {
    let refused = ChannelOutcome::Purged {
        affected: 1,
        stop_reason: Some(PurgeStopReason::Refused),
    };
    // The audit outcome stays "purged"; only the reason is new information.
    assert_eq!(refused.name(), "purged");
    assert_eq!(
        moderation_result_text(refused),
        "Moderation action completed: purged (1, stopped early: refused)."
    );

    let rate_limited = ChannelOutcome::Purged {
        affected: 37,
        stop_reason: Some(PurgeStopReason::RateLimited),
    };
    assert_eq!(rate_limited.name(), "purged");
    assert_eq!(
        moderation_result_text(rate_limited),
        "Moderation action completed: purged (37, stopped early: rate_limited)."
    );
}
