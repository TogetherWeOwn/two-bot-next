#![cfg(test)]
//! Protected fail-open acceptance: unparseable delivery timestamps refuse.
//!
//! Paired build card implements the fix; the Builder must not edit this file.
//! `receipt_ms` currently falls back to `0` on unparseable input, silently
//! anchoring age/latency math at epoch. The contract pinned here:
//!
//! - `receipt_ms` becomes fallible (`Option<u64>` or `Result<u64, E>`):
//!   unparseable input (garbage, empty, malformed ISO, pre-epoch) is an
//!   error, never epoch 0. The `acceptance_result` adapter below accepts
//!   today's `u64` shape as well as both fallible shapes, so this file
//!   compiles before and after the fix and fails behaviorally today.
//! - Well-formed input still returns the same millis as today.
//!
//! The Builder changes `receipt_ms` itself plus its two gateway call sites
//! (delivery receipt and partial-edit receipt) and the existing
//! `receipt_clock_parses_or_falls_back_to_zero` pin, which encodes the old
//! fail-open behavior. Whether a refused timestamp drops the delivery or a
//! documented unknown-marker flows through the callers is the Builder's
//! choice; no unknown may ever read as epoch 0.

use std::fmt::{Debug, Display};

use crate::automod_gateway::receipt_ms;

/// Shape-flexible verdict over `receipt_ms`: today's `u64` reads as `Ok`,
/// both fallible shapes pass their refusal through. The tests below fail
/// today because garbage reads as `Ok(0)`; after the fix the same calls
/// carry the refusal.
trait ReceiptOutcome {
    fn acceptance_result(self) -> Result<u64, String>;
}

impl ReceiptOutcome for u64 {
    fn acceptance_result(self) -> Result<u64, String> {
        Ok(self)
    }
}

impl ReceiptOutcome for Option<u64> {
    fn acceptance_result(self) -> Result<u64, String> {
        self.ok_or_else(|| "delivery timestamp refused: unparseable".to_owned())
    }
}

impl<E: Debug + Display> ReceiptOutcome for Result<u64, E> {
    fn acceptance_result(self) -> Result<u64, String> {
        self.map_err(|error| format!("{error}; {error:?}"))
    }
}

#[test]
fn garbage_timestamp_refuses_instead_of_epoch_zero() {
    for bad in ["garbage", "", "not-a-time", "  "] {
        let verdict = receipt_ms(bad).acceptance_result();
        assert!(
            verdict.is_err(),
            "unparseable timestamp {bad:?} must refuse, got {verdict:?}"
        );
    }
}

#[test]
fn malformed_iso_timestamp_refuses_instead_of_epoch_zero() {
    // Impossible calendar fields and pre-epoch instants (negative millis are
    // no valid receipt either): every one refuses, none anchors at 0.
    for bad in [
        "2026-13-02T00:00:00.000Z",
        "2026-10-40T25:99:99.000Z",
        "0000-01-01T00:00:00.000Z",
        "2026-10-02",
    ] {
        let verdict = receipt_ms(bad).acceptance_result();
        assert!(
            verdict.is_err(),
            "malformed timestamp {bad:?} must refuse, got {verdict:?}"
        );
    }
}

#[test]
fn well_formed_timestamp_returns_millis() {
    // Control: the healthy path parses exactly as today.
    assert_eq!(
        receipt_ms("2026-10-02T00:00:00.000Z").acceptance_result(),
        Ok(1_790_899_200_000)
    );
}
