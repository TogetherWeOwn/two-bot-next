//! Acceptance: scheduled attempts are audited when completion or retry
//! storage fails.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch,
//! keep every test here verbatim, and make the RED tests pass by changing
//! production code only (the scheduled ticker's completion handling). Do
//! not edit the fixtures or assertions: they are the spec.
//!
//! The contract under test: every claimed occurrence ends in exactly one
//! `scheduled.run` audit attempt, even when the store write that records
//! the attempt fails. Three exits currently return `ErrorClass::Database`
//! before auditing:
//!
//! - RED: Discord accepted the post, recording the run fails, the orphan
//!   cleanup runs, and then the retry update also fails — the unrecorded
//!   outcome (`persistence_failed_cleaned` for a cleaned orphan,
//!   `persistence_failed_uncertain` for a surviving one, reason
//!   `database`) is still audited exactly once.
//! - RED: an ambiguous Discord failure (503) whose retry update fails audits
//!   `delivery_uncertain` with `discord_unavailable`. Proven no-effect
//!   retries (429, guard) keep `retry_scheduled` with their bounded reason.
//! - RED: a permanent Discord refusal whose completion update fails
//!   audits the branch outcome (`post_failed`) with `discord_rejected`.
//! - RED: when the audit write itself also fails, the attempted audit is
//!   visible through the existing bounded `scheduled audit write failed`
//!   warning (exactly once, naming the schedule) and the tick still
//!   returns the original `ErrorClass::Database`.
//!
//! The audit outcome names the occurrence's Discord-side result; it must
//! never be read as proof the failed store write landed. Every RED test
//! therefore also pins the un-recorded row state: the claim is still
//! held, `next_run_at` is still the claim lease (not a retry or
//! completion time), the nonce is unchanged, and no run was recorded.
//!
//! Positive controls already exist and must stay green unchanged: the
//! held-claim and stale-claim audits in
//! `scheduled_stale_audit_acceptance_tests.rs` and the success, retry
//! and refusal audits in `scheduled_jobs_tests.rs`. The direct
//! `Occurrence::audit` control at the bottom of this file is green
//! before and after: a failed audit write sets `audit_failed` and warns
//! instead of masking or panicking.
//!
//! Offline only: a disposable test database plus the scripted REST
//! double. No live Discord. Store failures are induced by scratch
//! triggers on the fixture database only: the completion update is the
//! only write that changes `last_run_at`, and the retry update is the
//! only claim-clearing write that leaves `last_run_at` unchanged, so
//! each write can be refused without breaking the initial claim. Audit
//! failures use a separate trigger on the audit table, so the
//! passing-audit cases prove the audit attempt itself.

use serde_json::{json, Value};
use two_bot_core::{get_scheduled, put_scheduled, ScheduledWrite};
use two_bot_testsupport::TestDatabase;

use super::*;

use crate::discord_test_common::{MockRest, ScriptedResponse};

const GUILD: &str = "3333";
const CHANNEL: &str = "4444";
const POST_PATH: &str = "/api/v10/channels/4444/messages";
/// 2026-09-21T…Z: any fixed instant works; the store compares ISO text.
const T0: u64 = 1_790_000_000_000;

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-job-test-token".to_owned(), Some(mock.origin())).unwrap()
}

fn posted() -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"id": "9001"}))
}

async fn fixture() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!(
            "SKIP scheduled store-failure audit acceptance: TWO_TEST_DATABASE_URL is not set"
        );
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated agent-testdb fixture"),
    )
}

async fn seed(pool: &PgPool, id: &str, due_at: u64) {
    let at = format_iso_ms(T0 - 3_600_000);
    let stored = put_scheduled(
        pool,
        &ScheduledWrite {
            id: id.to_owned(),
            guild_id: GUILD.to_owned(),
            channel_id: CHANNEL.to_owned(),
            body: "standup @everyone".to_owned(),
            next_run_at: format_iso_ms(due_at),
            interval_seconds: None,
            enabled: true,
            created_by: "5555".to_owned(),
            created_at: at.clone(),
            updated_by: "5555".to_owned(),
            updated_at: at,
        },
    )
    .await
    .expect("seed scheduled row");
    assert!(stored);
}

async fn row(pool: &PgPool, id: &str) -> ScheduledMessageRow {
    get_scheduled(pool, GUILD, id)
        .await
        .expect("read scheduled row")
        .expect("row exists")
}

async fn audits(pool: &PgPool, id: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT outcome, reason FROM automation_audit_log
          WHERE guild_id = $1 AND target_key = $2 AND action = 'scheduled.run'
          ORDER BY created_at, id",
    )
    .bind(GUILD)
    .bind(id)
    .fetch_all(pool)
    .await
    .expect("read audit rows")
}

fn audit(outcome: &str, reason: Option<&str>) -> (String, Option<String>) {
    (outcome.to_owned(), reason.map(str::to_owned))
}

fn posts(mock: &MockRest) -> Vec<Value> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == POST_PATH)
        .map(|request| serde_json::from_slice(&request.body).expect("JSON post body"))
        .collect()
}

/// The state a claimed row keeps when the recording write never landed:
/// still claimed at the lease horizon, nonce untouched, no run facts.
/// Asserting anything else would claim a requeue or completion that was
/// not recorded.
fn assert_still_claimed_unrecorded(unrecorded: &ScheduledMessageRow, post: &Value) {
    assert!(unrecorded.enabled, "the one-shot was never completed");
    assert_eq!(
        unrecorded.next_run_at,
        format_iso_ms(lease_until_ms(T0)),
        "next_run_at is still the claim lease, not a recorded retry time"
    );
    assert!(
        unrecorded.claim_token.is_some(),
        "the claim-clearing write never landed"
    );
    assert_eq!(
        unrecorded.occurrence_nonce.as_deref(),
        post["nonce"].as_str(),
        "the occurrence nonce is unchanged"
    );
    assert!(
        unrecorded.delivery_uncertain,
        "the pre-wire fence is retained when the recording write fails"
    );
    assert_eq!(unrecorded.last_run_at, None, "no run was recorded");
    assert_eq!(unrecorded.last_message_id, None);
}

/// Make recording a run fail the way a store error would: the completion
/// update sets `last_run_at`, the claim/retry updates never do, so a
/// trigger on that column refuses exactly the recording write.
async fn refuse_run_recording(pool: &PgPool) {
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_scheduled_completion() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.last_run_at IS DISTINCT FROM OLD.last_run_at THEN RAISE EXCEPTION 'fixture completion failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_scheduled_completion BEFORE UPDATE ON scheduled_messages FOR EACH ROW EXECUTE FUNCTION refuse_scheduled_completion();",
    )
    .execute(pool)
    .await
    .expect("install completion-refusing trigger");
}

/// Make the retry update fail the way a store error would: retry clears
/// a held claim without touching `last_run_at`. The claim write sets a
/// token instead of clearing one, and the completion write changes
/// `last_run_at`, so neither is refused.
async fn refuse_retry_update(pool: &PgPool) {
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_scheduled_retry() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF OLD.claim_token IS NOT NULL AND NEW.claim_token IS NULL AND NEW.last_run_at IS NOT DISTINCT FROM OLD.last_run_at THEN RAISE EXCEPTION 'fixture retry failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_scheduled_retry BEFORE UPDATE ON scheduled_messages FOR EACH ROW EXECUTE FUNCTION refuse_scheduled_retry();",
    )
    .execute(pool)
    .await
    .expect("install retry-refusing trigger");
}

/// Make audit writes fail separately from every schedule-store write, so
/// a passing audit in the sibling tests proves the attempt itself.
async fn refuse_audit_writes(pool: &PgPool) {
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_scheduled_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture audit failure'; END $$; CREATE TRIGGER fail_scheduled_audit BEFORE INSERT ON automation_audit_log FOR EACH ROW EXECUTE FUNCTION refuse_scheduled_audit();",
    )
    .execute(pool)
    .await
    .expect("install audit-refusing trigger");
}

fn audit_failure_warnings(since: usize, id: &str) -> Vec<String> {
    crate::tracing_capture::global().text()[since..]
        .lines()
        .filter(|line| line.contains("scheduled audit write failed") && line.contains(id))
        .map(str::to_owned)
        .collect()
}

// RED on current main: Discord accepted the post, recording the run
// fails, the orphan delete lands, and then the retry update fails too —
// the early database return skips the one audit row that records a
// message Discord accepted.
#[tokio::test]
async fn unrecorded_post_cleaned_orphan_and_failed_retry_audits_persistence_failure() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "unrecorded-retry-fails", T0 - 1_000).await;
    refuse_run_recording(pool).await;
    refuse_retry_update(pool).await;
    let mock = MockRest::start(
        vec![posted(), ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);

    let outcome = tick(pool, &rest, GUILD, || T0).await;
    assert!(
        matches!(outcome, Err(ErrorClass::Database)),
        "a failed retry update still fails the attempt, got {outcome:?}"
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, "DELETE");
    assert_eq!(requests[1].path, "/api/v10/channels/4444/messages/9001");
    let sent = posts(&mock);
    assert_eq!(sent.len(), 1, "the post is never repeated");
    assert_still_claimed_unrecorded(&row(pool, "unrecorded-retry-fails").await, &sent[0]);
    assert_eq!(
        audits(pool, "unrecorded-retry-fails").await,
        [audit("persistence_failed_cleaned", Some("database"))],
        "the unrecorded post is audited even when the retry update fails"
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

// RED on current main: the same failed recording and failed retry, but
// the orphan delete is refused so the message survives. The failed
// retry write changed nothing — the claim and nonce still sit on the
// row, so the lease-expiry re-run stays idempotent — yet no audit row
// records the surviving post.
#[tokio::test]
async fn unrecorded_post_surviving_orphan_and_failed_retry_audits_persistence_failure() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "unrecorded-surviving-retry-fails", T0 - 1_000).await;
    refuse_run_recording(pool).await;
    refuse_retry_update(pool).await;
    let mock = MockRest::start(
        vec![posted(), ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);

    let outcome = tick(pool, &rest, GUILD, || T0).await;
    assert!(
        matches!(outcome, Err(ErrorClass::Database)),
        "a failed retry update still fails the attempt, got {outcome:?}"
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, "DELETE");
    assert_eq!(requests[1].path, "/api/v10/channels/4444/messages/9001");
    let sent = posts(&mock);
    assert_eq!(sent.len(), 1, "the surviving orphan is never re-posted");
    assert_still_claimed_unrecorded(
        &row(pool, "unrecorded-surviving-retry-fails").await,
        &sent[0],
    );
    assert_eq!(
        audits(pool, "unrecorded-surviving-retry-fails").await,
        [audit("persistence_failed_uncertain", Some("database"))],
        "the surviving unrecorded post is audited even when the retry update fails"
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

// RED on current main: an ambiguous Discord failure (503) whose retry update
// fails returns the database error before the uncertain outcome can be
// audited, losing the bounded Discord reason with it.
#[tokio::test]
async fn uncertain_503_and_failed_retry_audits_delivery_uncertain_with_discord_reason() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "retryable-retry-fails", T0 - 1_000).await;
    refuse_retry_update(pool).await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(503)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);

    let outcome = tick(pool, &rest, GUILD, || T0).await;
    assert!(
        matches!(outcome, Err(ErrorClass::Database)),
        "a failed retry update still fails the attempt, got {outcome:?}"
    );

    let sent = posts(&mock);
    assert_eq!(sent.len(), 1, "the failed post is never repeated");
    assert_still_claimed_unrecorded(&row(pool, "retryable-retry-fails").await, &sent[0]);
    assert_eq!(
        audits(pool, "retryable-retry-fails").await,
        [audit("delivery_uncertain", Some("discord_unavailable"))],
        "the ambiguous failure is audited uncertain with its discord reason even when the retry update fails"
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

// RED on current main: a permanent refusal whose completion update
// fails warns and returns the database error, but the refusal itself is
// never audited.
#[tokio::test]
async fn permanent_refusal_and_failed_completion_audits_post_failed_with_discord_reason() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "refused-completion-fails", T0 - 1_000).await;
    refuse_run_recording(pool).await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);

    let outcome = tick(pool, &rest, GUILD, || T0).await;
    assert!(
        matches!(outcome, Err(ErrorClass::Database)),
        "a failed completion still fails the attempt, got {outcome:?}"
    );

    let sent = posts(&mock);
    assert_eq!(sent.len(), 1, "the refusal is never re-posted");
    assert_still_claimed_unrecorded(&row(pool, "refused-completion-fails").await, &sent[0]);
    assert_eq!(
        audits(pool, "refused-completion-fails").await,
        [audit("post_failed", Some("discord_rejected"))],
        "the permanent refusal is audited even when recording the run fails"
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

// RED on current main: the permanent-refusal exit above with the audit
// write failing too. Before the fix no audit is attempted, so the
// bounded audit-failure warning never fires; after it, exactly one
// attempt warns once and the original store error still surfaces.
#[tokio::test]
async fn permanent_refusal_failed_completion_and_failed_audit_warns_once_and_returns_database() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "refused-audit-fails", T0 - 1_000).await;
    refuse_run_recording(pool).await;
    refuse_audit_writes(pool).await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    let log_start = crate::tracing_capture::global().text().len();

    let outcome = tick(pool, &rest, GUILD, || T0).await;
    assert!(
        matches!(outcome, Err(ErrorClass::Database)),
        "the original store error is not masked by the audit failure, got {outcome:?}"
    );

    let sent = posts(&mock);
    assert_eq!(sent.len(), 1, "the refusal is never re-posted");
    assert_still_claimed_unrecorded(&row(pool, "refused-audit-fails").await, &sent[0]);
    assert!(
        audits(pool, "refused-audit-fails").await.is_empty(),
        "the refused audit write lands no row"
    );
    let warnings = audit_failure_warnings(log_start, "refused-audit-fails");
    assert_eq!(
        warnings.len(),
        1,
        "exactly one audit attempt warns once, got {warnings:?}"
    );
    assert!(
        warnings[0].contains("post_failed"),
        "the warning names the attempted outcome, got {:?}",
        warnings[0]
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

// Positive control (green throughout): `Occurrence::audit` already
// tolerates its own failure — a successful write leaves `audit_failed`
// clear and lands one row, then a refused write sets the flag and emits
// the existing bounded warning without landing a second row or
// panicking. The RED exits above can therefore audit before returning
// their store error without masking it.
#[tokio::test]
async fn audit_write_failure_sets_audit_failed_and_warns_after_a_successful_control() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "audit-direct", T0 - 1_000).await;
    let claimed = row(&pool, "audit-direct").await;
    let mock = MockRest::start(Vec::new(), ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    let now_iso = format_iso_ms(T0);
    let occurrence = Occurrence {
        pool: &pool,
        rest: &rest,
        row: &claimed,
        claim_token: "fixture-claim",
        now: T0,
        now_iso: &now_iso,
    };

    let mut audit_failed = false;
    occurrence
        .audit(
            &OccurrenceOutcome::Posted {
                message_id: Some("9001".to_owned()),
            },
            None,
            &mut audit_failed,
        )
        .await;
    assert!(
        !audit_failed,
        "a successful audit write keeps the flag clear"
    );
    assert_eq!(audits(&pool, "audit-direct").await, [audit("ok", None)]);

    refuse_audit_writes(&pool).await;
    let log_start = crate::tracing_capture::global().text().len();
    occurrence
        .audit(
            &OccurrenceOutcome::FailedPermanent,
            Some("discord_rejected"),
            &mut audit_failed,
        )
        .await;
    assert!(audit_failed, "a refused audit write sets audit_failed");
    assert_eq!(
        audits(&pool, "audit-direct").await,
        [audit("ok", None)],
        "the refused audit write lands no second row"
    );
    let warnings = audit_failure_warnings(log_start, "audit-direct");
    assert_eq!(
        warnings.len(),
        1,
        "exactly one bounded warning, got {warnings:?}"
    );
    assert!(
        warnings[0].contains("post_failed"),
        "the warning names the attempted outcome, got {:?}",
        warnings[0]
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}
