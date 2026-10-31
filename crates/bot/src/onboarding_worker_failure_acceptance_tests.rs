//! Acceptance: an onboarding worker failure keeps its cause.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch,
//! keep every test here verbatim, and make the RED tests pass by changing
//! production code only (`crates/bot/src/gateway.rs`, `onboarding_queue`).
//! Do not edit the fixtures or assertions: they are the spec.
//!
//! Today `onboarding_queue` collapses every onboarding `RuntimeError`
//! (seven variants), any join failure or panic, into one fixed string,
//! and labels the 90-second worker timeout as `RuntimeError::Discord`.
//! An operator reading the gateway failure line cannot tell a config
//! refusal from a store outage from a hung worker.
//!
//! The contract under test. The builder adds, in `crate::gateway`:
//!
//! ```ignore
//! pub(crate) enum OnboardingWorkerFailure {
//!     Runtime(crate::onboarding::RuntimeError),
//!     Timeout,
//!     Join,
//! }
//!
//! pub(crate) fn onboarding_worker_failure(
//!     cause: OnboardingWorkerFailure,
//! ) -> sqlx::Error
//! ```
//!
//! `onboarding_worker_failure` returns
//! `sqlx::Error::InvalidArgument(message)` where `message` is exactly
//! one fixed literal per cause (see `CASES` below): the existing
//! `onboarding worker failed` line, a parenthesized cause word, and the
//! unchanged `; durable job retained for bounded restart recovery`
//! suffix. No literal carries payload: no guild, member or channel
//! text, no ids, no error rendering.
//!
//! `onboarding_queue` routes through it: the 90-second timeout yields
//! `OnboardingWorkerFailure::Timeout` (never `RuntimeError::Discord`);
//! a runtime error yields `OnboardingWorkerFailure::Runtime(error)`;
//! a `join_next` error (panic or cancellation) yields
//! `OnboardingWorkerFailure::Join`; the queue returns
//! `Err(onboarding_worker_failure(cause))`.
//!
//! Unchanged control behavior: the 90-second timeout stays, the worker
//! limit stays, and a failed job is still not finished —
//! `finish_onboarding_job` runs only for the interrupted-interaction
//! receipt and for a successful job, so the durable job is retained
//! for bounded restart recovery.
//!
//! Offline only: pure unit tests plus a source scan of `gateway.rs`.
//! No database, no live Discord.

use std::collections::HashSet;

use two_bot_core::onboarding::MembershipTrigger;

use crate::gateway::{onboarding_worker_failure, OnboardingWorkerFailure};
use crate::onboarding::{OnboardingJob, RuntimeError};

/// Every cause an onboarding worker can fail with, and the exact fixed
/// literal the gateway failure line must carry for it.
fn cases() -> Vec<(OnboardingWorkerFailure, &'static str)> {
    vec![
        (
            OnboardingWorkerFailure::Runtime(RuntimeError::Config),
            "onboarding worker failed (config); durable job retained for bounded restart recovery",
        ),
        (
            OnboardingWorkerFailure::Runtime(RuntimeError::Settings),
            "onboarding worker failed (settings); durable job retained for bounded restart recovery",
        ),
        (
            OnboardingWorkerFailure::Runtime(RuntimeError::Identity),
            "onboarding worker failed (identity); durable job retained for bounded restart recovery",
        ),
        (
            OnboardingWorkerFailure::Runtime(RuntimeError::Member),
            "onboarding worker failed (member); durable job retained for bounded restart recovery",
        ),
        (
            OnboardingWorkerFailure::Runtime(RuntimeError::Role),
            "onboarding worker failed (role); durable job retained for bounded restart recovery",
        ),
        (
            OnboardingWorkerFailure::Runtime(RuntimeError::Discord),
            "onboarding worker failed (discord); durable job retained for bounded restart recovery",
        ),
        (
            OnboardingWorkerFailure::Runtime(RuntimeError::Store),
            "onboarding worker failed (store); durable job retained for bounded restart recovery",
        ),
        (
            OnboardingWorkerFailure::Timeout,
            "onboarding worker failed (timeout); durable job retained for bounded restart recovery",
        ),
        (
            OnboardingWorkerFailure::Join,
            "onboarding worker failed (join); durable job retained for bounded restart recovery",
        ),
    ]
}

fn message(cause: OnboardingWorkerFailure) -> String {
    let error = onboarding_worker_failure(cause);
    assert!(
        matches!(&error, sqlx::Error::InvalidArgument(_)),
        "worker failure must stay typed, got {error:?}"
    );
    match error {
        sqlx::Error::InvalidArgument(message) => message,
        error => unreachable!("worker failure must stay typed, got {error:?}"),
    }
}

/// RED: each `RuntimeError` variant keeps its own literal instead of
/// collapsing into the one shared string.
#[test]
fn onboarding_worker_failure_keeps_each_runtime_cause() {
    for (cause, expected) in cases().into_iter().take(7) {
        assert_eq!(message(cause), expected);
    }
}

/// RED: the 90-second timeout and the join/panic path are causes of
/// their own. The timeout literal must not be the Discord literal:
/// today the timeout is mislabeled `RuntimeError::Discord`.
#[test]
fn onboarding_worker_failure_distinguishes_timeout_and_join() {
    let timeout = message(OnboardingWorkerFailure::Timeout);
    let join = message(OnboardingWorkerFailure::Join);
    let discord = message(OnboardingWorkerFailure::Runtime(RuntimeError::Discord));
    assert_eq!(
        timeout,
        "onboarding worker failed (timeout); durable job retained for bounded restart recovery"
    );
    assert_eq!(
        join,
        "onboarding worker failed (join); durable job retained for bounded restart recovery"
    );
    assert_ne!(timeout, discord, "a timeout is not a Discord failure");
    assert_ne!(join, discord, "a join failure is not a Discord failure");
    assert_ne!(timeout, join);
}

/// RED: the nine literals are pairwise distinct and payload-free. The
/// helper takes no payload input, and the literals prove it: no digits
/// (no snowflake ids), no mentions, no channel syntax, bounded length.
#[test]
fn onboarding_worker_failure_literals_carry_no_payload() {
    let messages: Vec<String> = cases()
        .into_iter()
        .map(|(cause, _)| message(cause))
        .collect();
    let distinct: HashSet<&String> = messages.iter().collect();
    assert_eq!(
        distinct.len(),
        messages.len(),
        "every cause needs its own literal, got {messages:?}"
    );
    for message in &messages {
        assert!(
            !message.chars().any(|ch| ch.is_ascii_digit()),
            "literal must carry no id payload, got {message:?}"
        );
        assert!(
            !message.contains('@') && !message.contains('#') && !message.contains('<'),
            "literal must carry no member or channel payload, got {message:?}"
        );
        assert!(
            message.len() < 120,
            "literal must stay a fixed line, got {message:?}"
        );
    }
}

/// RED wiring: `onboarding_queue` actually routes through the mapping.
/// A helper that exists but is never called keeps the collapse.
#[test]
fn onboarding_queue_routes_failures_through_the_cause_mapping() {
    let source = include_str!("gateway.rs");
    let queue = source
        .split("async fn onboarding_queue")
        .nth(1)
        .expect("gateway.rs keeps onboarding_queue")
        .split("pub fn build_shard")
        .next()
        .expect("onboarding_queue precedes build_shard");

    assert!(
        queue.contains("onboarding_worker_failure("),
        "onboarding_queue must surface failures through onboarding_worker_failure"
    );
    assert!(
        queue.contains("OnboardingWorkerFailure::Timeout"),
        "the 90-second timeout must yield the Timeout cause"
    );
    assert!(
        queue.contains("OnboardingWorkerFailure::Join"),
        "a join failure or panic must yield the Join cause"
    );
    assert!(
        !source.contains("map_err(|_| crate::onboarding::RuntimeError::Discord)"),
        "the timeout must no longer be mislabeled as a Discord failure"
    );
    assert!(
        !source.contains("\"onboarding worker failed; durable job retained"),
        "the collapsed single-cause literal must be gone"
    );

    // Controls the builder must not move: the 90-second worker timeout
    // and finishing a job only on the receipt and success paths (a
    // failed job stays durable for bounded restart recovery).
    assert!(
        queue.contains("Duration::from_secs(90)"),
        "the worker timeout stays at 90 seconds"
    );
    assert_eq!(
        queue.matches("finish_onboarding_job").count(),
        2,
        "finish stays on the interrupted-receipt and success paths only; \
         a failed job is retained, never finished"
    );
}

/// Control (green before and after): a durable onboarding job survives
/// serialization for restart recovery, which is what "durable job
/// retained" promises when a worker fails.
#[test]
fn durable_onboarding_job_is_retained_for_restart_recovery_control() {
    let job = OnboardingJob::Welcome {
        guild_id: 22,
        member_id: 44,
        bot: false,
        pending: false,
        trigger: MembershipTrigger::GateCleared,
        roles: vec!["77".to_owned()],
    };
    let payload = job.durable_payload().expect("welcome job serializes");
    match OnboardingJob::recover(&payload).expect("payload parses") {
        Some(OnboardingJob::Welcome {
            guild_id,
            member_id,
            roles,
            ..
        }) => {
            assert_eq!(guild_id, 22);
            assert_eq!(member_id, 44);
            assert_eq!(roles, vec!["77".to_owned()]);
        }
        _ => panic!("a retained welcome job must recover as a welcome job"),
    }
}
