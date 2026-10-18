//! Acceptance: backfill failure lines carry class + chain + step (TOG-25282).
//!
//! BUILDER CONTRACT (TOG-25285): this file is frozen. Build on the test
//! card's branch, keep every test here verbatim, and make the suite green by
//! changing production code only — promote `crate::backfill_retention_chains`
//! to the production build and replace the `render_backfill_failure` body per
//! the algorithm there, sanitize the store `Display`s so no raw sqlx text
//! reaches a chain, and wire the `ops_backfill` snapshot failure through the
//! renderer. Do not edit the
//! steps constructed below, the hostile samples, or the assertions: they are
//! the spec.
//!
//! Suite map (all offline: no DB, no live Discord):
//!
//! - step vocabulary pins (green now): every step is a distinct short token.
//! - class pin (green now): every gateway class renders with its token, so the
//!   builder's new variants are covered without touching this file.
//! - sanitizer unit pins (green now): `sanitize_chain` redacts the hostile
//!   shapes and keeps classified causes byte-identical.
//! - surface pins (RED now: the scaffold renders class + raw chain with no
//!   step): cursor checkpoint, history put/count, rate-limit client.
//! - step coverage pin (RED now: no step renders): every step carries its
//!   token, covering audit catch-up and snapshot read with no other offline
//!   surface.
//! - store Display pins (RED now: `{0}` passes raw sqlx text through).
//! - red-team pin (RED now: hostile chains leak into the rendered line).
//! - wiring pin (RED now: the snapshot failure never calls the renderer).

use two_bot_discord::executor::DiscordError;
use two_bot_store::backfill_cursors::BackfillCursorError;
use two_bot_store::backfill_history::BackfillHistoryError;

use crate::backfill_retention_chains::{render_backfill_failure, sanitize_chain, BackfillStep};
use crate::gateway_failure::FailureClass;

/// Fake secret URL (same low-entropy fixture shape as the gateway tests: never
/// a credential, never high-entropy, safe for secret scanners).
const SECRET_URL: &str = "postgres://user:hunter2@db.internal/app";
/// Fake bearer token: low-entropy, obviously synthetic.
const BEARER: &str = "Bearer test-bearer-xxxx";
/// Fake PII: reserved example domain, never a real address.
const EMAIL: &str = "operator@example.com";
/// Fake member id: 18-digit snowflake shape, never a real member.
const MEMBER_ID: &str = "987654321098765432";

const HOSTILE: [&str; 4] = [SECRET_URL, BEARER, EMAIL, MEMBER_ID];

fn is_token(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A backfill failure line carries its class, its step and its chain.
fn assert_failure_line(line: &str, class: FailureClass, step: BackfillStep) {
    assert!(
        line.contains(class.as_str()),
        "failure line must carry its class: {line:?}"
    );
    assert!(
        line.contains(step.as_str()),
        "failure line must carry its step: {line:?}"
    );
    assert!(
        line.contains("chain"),
        "failure line must carry its chain: {line:?}"
    );
}

#[test]
fn step_tokens_are_a_distinct_short_vocabulary() {
    let mut seen = std::collections::BTreeSet::new();
    for step in BackfillStep::ALL {
        assert!(is_token(step.as_str()), "{step:?}");
        assert!(seen.insert(step.as_str()), "duplicate token {step:?}");
    }
}

#[test]
fn step_list_is_exhaustive() {
    fn index(step: BackfillStep) -> usize {
        match step {
            BackfillStep::RateLimitClient => 0,
            BackfillStep::CursorCheckpoint => 1,
            BackfillStep::AuditCatchUp => 2,
            BackfillStep::HistoryPut => 3,
            BackfillStep::HistoryCount => 4,
            BackfillStep::SnapshotRead => 5,
        }
    }
    for (position, step) in BackfillStep::ALL.into_iter().enumerate() {
        assert_eq!(index(step), position);
    }
    assert_eq!(BackfillStep::ALL.len(), 6);
}

#[test]
fn every_gateway_class_renders_with_its_token() {
    // The class vocabulary is read-only here: any class the builder adds is
    // covered the moment it joins `FailureClass::ALL`, with no edit to this
    // file.
    for class in FailureClass::ALL {
        let line = render_backfill_failure(class, BackfillStep::CursorCheckpoint, "pool_timeout");
        assert!(
            line.contains(class.as_str()),
            "class {class:?} must survive rendering: {line:?}"
        );
    }
}

#[test]
fn every_step_carries_its_token() {
    for step in BackfillStep::ALL {
        let line = render_backfill_failure(FailureClass::StoreUnavailable, step, "pool_timeout");
        assert!(
            line.contains(step.as_str()),
            "step {step:?} must survive rendering: {line:?}"
        );
    }
}

#[test]
fn sanitize_chain_redacts_secret_token_pii_and_member_id() {
    for hostile in HOSTILE {
        let clean = sanitize_chain(hostile);
        assert!(
            !clean.contains(hostile),
            "sanitizer leaked {hostile:?}: {clean:?}"
        );
        assert!(
            clean.contains("[redacted]"),
            "sanitizer must mark the redaction: {clean:?}"
        );
    }
    let embedded = format!("fetch {MEMBER_ID} failed for {EMAIL}");
    assert!(
        !sanitize_chain(&embedded).contains(MEMBER_ID),
        "embedded member id must not survive: {embedded:?}"
    );
    assert!(
        !sanitize_chain(&embedded).contains(EMAIL),
        "embedded PII must not survive: {embedded:?}"
    );
}

#[test]
fn sanitize_chain_keeps_classified_causes_byte_identical() {
    for cause in [
        "pool_timeout",
        "rate_limited",
        "connection",
        "row_not_found",
    ] {
        assert_eq!(sanitize_chain(cause), cause, "cause {cause:?} mangled");
    }
}

#[test]
fn cursor_failures_carry_class_chain_step() {
    // Real validation failure: refused before any SQL, fixed message.
    let invalid = BackfillCursorError::Invalid("cursor must be a non-zero opaque id");
    let line = render_backfill_failure(
        FailureClass::StoreUnavailable,
        BackfillStep::CursorCheckpoint,
        &invalid.to_string(),
    );
    assert_failure_line(
        &line,
        FailureClass::StoreUnavailable,
        BackfillStep::CursorCheckpoint,
    );
    assert!(
        line.contains("cursor must be a non-zero opaque id"),
        "fixed validation cause must survive: {line:?}"
    );
}

#[test]
fn history_put_and_count_failures_carry_class_chain_step() {
    let put = BackfillHistoryError::Invalid("message, channel and author ids must be non-empty");
    let put_line = render_backfill_failure(
        FailureClass::StoreUnavailable,
        BackfillStep::HistoryPut,
        &put.to_string(),
    );
    assert_failure_line(
        &put_line,
        FailureClass::StoreUnavailable,
        BackfillStep::HistoryPut,
    );

    let count = BackfillHistoryError::Invalid("message count must be non-negative");
    let count_line = render_backfill_failure(
        FailureClass::StoreUnavailable,
        BackfillStep::HistoryCount,
        &count.to_string(),
    );
    assert_failure_line(
        &count_line,
        FailureClass::StoreUnavailable,
        BackfillStep::HistoryCount,
    );
}

#[test]
fn rate_limit_failures_carry_class_chain_step() {
    // The slice-1 rate-limit client surfaces 429s as this fixed cause: no
    // URL, no retry-after literal, never raw transport text.
    let chain = DiscordError::RateLimited.cause();
    assert_eq!(chain, "rate_limited");
    let line = render_backfill_failure(
        FailureClass::StoreUnavailable,
        BackfillStep::RateLimitClient,
        chain,
    );
    assert_failure_line(
        &line,
        FailureClass::StoreUnavailable,
        BackfillStep::RateLimitClient,
    );
    assert!(
        line.contains("rate_limited"),
        "fixed rate-limit cause must survive: {line:?}"
    );
}

#[test]
fn store_displays_do_not_passthrough_raw_sqlx_text() {
    // Today's `{0}` passthrough embeds the sqlx Display verbatim: a database
    // error carrying a connection URL leaks it into every failure line.
    let cursor = BackfillCursorError::Db(sqlx::Error::InvalidArgument(SECRET_URL.into()));
    let rendered = cursor.to_string();
    assert!(
        !rendered.contains(SECRET_URL),
        "BackfillCursorError::Db leaked the secret URL: {rendered:?}"
    );
    let history = BackfillHistoryError::Db(sqlx::Error::InvalidArgument(SECRET_URL.into()));
    let rendered = history.to_string();
    assert!(
        !rendered.contains(SECRET_URL),
        "BackfillHistoryError::Db leaked the secret URL: {rendered:?}"
    );
}

#[test]
fn chains_are_sanitized_no_secret_token_pii_or_member_id() {
    // Red-team sample: every hostile shape through every gateway class. The
    // scaffold renders the raw chain, so each of these leaks today.
    for class in FailureClass::ALL {
        for hostile in HOSTILE {
            let line = render_backfill_failure(class, BackfillStep::HistoryPut, hostile);
            assert!(
                !line.contains(hostile),
                "class {class:?} leaked {hostile:?}: {line:?}"
            );
        }
    }
}

/// The `ops_backfill` snapshot failure as committed today, read at compile
/// time so the suite stays offline.
const OPS_BACKFILL_RS: &str = include_str!("ops_backfill.rs");

#[test]
fn snapshot_failure_is_wired_to_the_chain_renderer() {
    assert!(
        OPS_BACKFILL_RS.contains("render_backfill_failure"),
        "the backfill snapshot failure must render through render_backfill_failure \
         (class + chain + step); the scaffold is unwired"
    );
}
