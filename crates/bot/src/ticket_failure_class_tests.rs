//! Unit coverage for pre-reservation ticket failure classes.
//!
//! The member-visible replies for lane-admission timeouts and untracked
//! channels are pinned by the protected acceptance suite; this module pins
//! the error-class mapping for the same variants so the changed `class()`
//! arm stays executed. It also pins the reply distinction Paperclip Review
//! required: only the lane-admission variant (`LaneBusy`) uses the
//! no-recovery copy, while the cumulative-deadline `Timeout` keeps the
//! recovery guidance for operations interrupted after work was stored.
//! The exact `LaneBusy`/`NoTicket` reply text is pinned below; `Timeout`
//! keeps the generic fallback the copy-pin acceptance suite already pins.
//! No DB, no Discord, no async.
use super::*;

#[test]
fn no_ticket_failure_classes_as_rest() {
    assert_eq!(Failure::NoTicket.class(), ErrorClass::Rest);
}

#[test]
fn timeout_failures_keep_timeout_class() {
    assert_eq!(Failure::Timeout.class(), ErrorClass::Timeout);
    assert_eq!(Failure::LaneBusy.class(), ErrorClass::Timeout);
    assert_eq!(
        Failure::Rest(DiscordError::Timeout).class(),
        ErrorClass::Timeout
    );
}

#[test]
fn lane_busy_uses_no_recovery_retry_later_copy() {
    let reply = Failure::LaneBusy.reply();
    let lower = reply.to_lowercase();
    assert!(
        lower.contains("try again"),
        "lane admission must stay a truthful retry-later refusal: {reply:?}"
    );
    for banned in ["recover", "unfinished", "will retry", "will be retried"] {
        assert!(
            !lower.contains(banned),
            "lane admission must not promise recovery ({banned:?}): {reply:?}"
        );
    }
}

#[test]
fn cumulative_timeout_keeps_recovery_guidance() {
    // `bounded(BUTTON_TIMEOUT, ...)` can expire after `reserve`/`begin_close`
    // stored a state that recovery resumes, so this reply must keep the
    // recovery guidance the lane-admission copy omits.
    let reply = Failure::Timeout.reply();
    let lower = reply.to_lowercase();
    assert!(
        lower.contains("recover"),
        "cumulative timeout must keep recovery guidance: {reply:?}"
    );
    assert!(
        lower.contains("try again"),
        "cumulative timeout must keep retry guidance: {reply:?}"
    );
}

#[test]
fn lane_busy_and_no_ticket_replies_are_pinned() {
    assert_eq!(
        Failure::LaneBusy.reply(),
        "Ticket action timed out waiting for another operation. Try again later."
    );
    assert_eq!(Failure::NoTicket.reply(), "This channel is not a ticket.");
}

#[test]
fn claim_conflicts_name_winner_or_lifecycle_without_recovery_promise() {
    let owned = Failure::TicketClaimedBy("501".into()).reply();
    assert_eq!(owned, "This ticket is already claimed by <@501>.");
    let not_open = Failure::TicketNotOpen.reply();
    assert_eq!(not_open, "This ticket is not open.");
    assert!(
        !owned.to_lowercase().contains("or not open"),
        "ownership and lifecycle refusals must not be conflated: {owned:?}"
    );
    assert!(
        !not_open.to_lowercase().contains("already claimed"),
        "lifecycle refusal must not name an ownership conflict: {not_open:?}"
    );
    for reply in [&owned, &not_open] {
        let lower = reply.to_lowercase();
        for banned in ["recover", "unfinished", "will retry", "will be retried"] {
            assert!(
                !lower.contains(banned),
                "claim refusal must not promise recovery ({banned:?}): {reply:?}"
            );
        }
    }
    assert_eq!(
        Failure::TicketClaimedBy("501".into()).class(),
        ErrorClass::Rest
    );
    assert_eq!(Failure::TicketNotOpen.class(), ErrorClass::Rest);
}
