//! Acceptance: retention failure lines carry class + chain + step (TOG-25282).
//!
//! BUILDER CONTRACT (TOG-25285): this file is frozen. Build on the test
//! card's branch, keep every test here verbatim, and make the suite green by
//! changing production code only — promote `crate::backfill_retention_chains`
//! to the production build and replace the `render_retention_failure` body
//! per the algorithm there, then wire the `retention` guild-purge failure +
//! deferral warns and the `retention_jobs` database-error mapping through the
//! renderer (dropping the raw `error = %format!("{err:?}")` chain). Do not edit
//! the steps constructed
//! below, the hostile samples, or the assertions: they are the spec.
//!
//! Suite map (all offline: no DB, no live Discord):
//!
//! - step vocabulary pins (green now): every step is a distinct short token.
//! - class pin (green now): every gateway class renders with its token, so the
//!   builder's new variants are covered without touching this file.
//! - surface pins (RED now: the scaffold renders class + raw chain with no
//!   step): expiry, deletion intake, dry-run.
//! - red-team pin (RED now: hostile chains leak into the rendered line).
//! - wiring pins (RED now: `retention` never calls the renderer and still logs
//!   the raw error chain).

use crate::backfill_retention_chains::{render_retention_failure, RetentionStep};
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

/// A retention failure line carries its class, its step and its chain.
fn assert_failure_line(line: &str, class: FailureClass, step: RetentionStep) {
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
    for step in RetentionStep::ALL {
        assert!(is_token(step.as_str()), "{step:?}");
        assert!(seen.insert(step.as_str()), "duplicate token {step:?}");
    }
}

#[test]
fn step_list_is_exhaustive() {
    fn index(step: RetentionStep) -> usize {
        match step {
            RetentionStep::Expiry => 0,
            RetentionStep::DeletionIntake => 1,
            RetentionStep::DryRun => 2,
        }
    }
    for (position, step) in RetentionStep::ALL.into_iter().enumerate() {
        assert_eq!(index(step), position);
    }
    assert_eq!(RetentionStep::ALL.len(), 3);
}

#[test]
fn every_gateway_class_renders_with_its_token() {
    // The class vocabulary is read-only here: any class the builder adds is
    // covered the moment it joins `FailureClass::ALL`, with no edit to this
    // file.
    for class in FailureClass::ALL {
        let line = render_retention_failure(class, RetentionStep::Expiry, "pool_timeout");
        assert!(
            line.contains(class.as_str()),
            "class {class:?} must survive rendering: {line:?}"
        );
    }
}

#[test]
fn expiry_failures_carry_class_chain_step() {
    // The expiry stage classifies storage errors to fixed causes (see
    // `db_log`): the chain carries the cause, never the sqlx text.
    let chain = crate::db_log::db_class(&sqlx::Error::PoolTimedOut);
    assert_eq!(chain, "pool_timeout");
    let line =
        render_retention_failure(FailureClass::StoreUnavailable, RetentionStep::Expiry, chain);
    assert_failure_line(&line, FailureClass::StoreUnavailable, RetentionStep::Expiry);
    assert!(
        line.contains("pool_timeout"),
        "fixed expiry cause must survive: {line:?}"
    );
}

#[test]
fn deletion_intake_failures_carry_class_chain_step() {
    // Mirrors `retention::validate_snowflake`: a fixed refusal, no ids.
    let chain = "invalid retention identity";
    let line = render_retention_failure(
        FailureClass::StoreUnavailable,
        RetentionStep::DeletionIntake,
        chain,
    );
    assert_failure_line(
        &line,
        FailureClass::StoreUnavailable,
        RetentionStep::DeletionIntake,
    );
    assert!(
        line.contains("invalid retention identity"),
        "fixed intake refusal must survive: {line:?}"
    );
}

#[test]
fn dry_run_failures_carry_class_chain_step() {
    let chain = "schema gaps deferred the request";
    let line =
        render_retention_failure(FailureClass::StoreUnavailable, RetentionStep::DryRun, chain);
    assert_failure_line(&line, FailureClass::StoreUnavailable, RetentionStep::DryRun);
}

#[test]
fn chains_are_sanitized_no_secret_token_pii_or_member_id() {
    // Red-team sample: every hostile shape through every gateway class. The
    // scaffold renders the raw chain, so each of these leaks today.
    for class in FailureClass::ALL {
        for hostile in HOSTILE {
            let line = render_retention_failure(class, RetentionStep::DeletionIntake, hostile);
            assert!(
                !line.contains(hostile),
                "class {class:?} leaked {hostile:?}: {line:?}"
            );
        }
    }
}

/// The `retention` + `retention_jobs` sources as committed today, read at
/// compile time so the suite stays offline.
const RETENTION_RS: &str = include_str!("../../cutover/src/retention.rs");
const RETENTION_JOBS_RS: &str = include_str!("retention_jobs.rs");

#[test]
fn retention_failures_are_wired_to_the_chain_renderer() {
    assert!(
        RETENTION_RS.contains("render_retention_failure"),
        "the retention purge/deferral paths must render through \
         render_retention_failure (class + chain + step); the scaffold is unwired"
    );
    assert!(
        RETENTION_JOBS_RS.contains("render_retention_failure")
            || RETENTION_JOBS_RS.contains("backfill_retention_chains"),
        "the retention job error mapping must run through the chain contract; \
         the scaffold is unwired"
    );
}

#[test]
fn retention_logs_carry_no_raw_error_chain() {
    // `error = %format!("{err:?}")` logs the whole sqlx Display/Debug chain:
    // connection URLs and statement detail ride the log line. Matching on the
    // text for deferral classification (`is_deferrable_refusal`) stays — only
    // the log interpolation goes.
    assert!(
        !RETENTION_RS.contains("error = %format!(\"{err:?}\")"),
        "retention.rs still logs the raw error chain; wire the sanitized renderer"
    );
}
