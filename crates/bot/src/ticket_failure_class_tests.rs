//! Unit coverage for pre-reservation ticket failure classes.
//!
//! The member-visible replies for lane-admission timeouts and untracked
//! channels are pinned by the protected acceptance suite; this module pins
//! the error-class mapping for the same variants so the changed `class()`
//! arm stays executed. No DB, no Discord, no async.
use super::*;

#[test]
fn no_ticket_failure_classes_as_rest() {
    assert_eq!(Failure::NoTicket.class(), ErrorClass::Rest);
}

#[test]
fn timeout_failures_keep_timeout_class() {
    assert_eq!(Failure::Timeout.class(), ErrorClass::Timeout);
    assert_eq!(
        Failure::Rest(DiscordError::Timeout).class(),
        ErrorClass::Timeout
    );
}
