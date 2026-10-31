//! Acceptance: stale scheduled completions are audited on every exit.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch,
//! keep every test here verbatim, and make the RED tests pass by changing
//! production code only (the scheduled ticker's completion handling). Do
//! not edit the fixtures or assertions: they are the spec.
//!
//! The contract under test: every claimed occurrence ends in exactly one
//! `scheduled.run` audit row, even when the claim goes stale mid-flight.
//! The success path already audits `stale_completion` when the definition
//! is replaced while Discord is posting. Two failure exits must match it:
//!
//! - RED: a permanent refusal whose claim is stale (the row was replaced
//!   or cancelled before the run could be recorded) audits
//!   `stale_completion` with the refusal reason, instead of vanishing.
//! - RED: a posted run that cannot be recorded and whose claim is then
//!   lost still audits the unrecorded outcome
//!   (`persistence_failed_cleaned` when the orphan delete lands), instead
//!   of staying silent because the row was not retained.
//!
//! Positive controls (green before and after): a permanent refusal on a
//! held claim audits `post_failed`, an unrecorded post on a retained
//! claim audits `persistence_failed_cleaned`, and a successful post with
//! a stale claim audits `stale_completion` with no reason. Retained-row
//! audits must not change.
//!
//! Offline only: a disposable test database plus the scripted REST
//! double. No live Discord.

use serde_json::{json, Value};
use two_bot_core::{delete_scheduled, get_scheduled, put_scheduled, ScheduledWrite};
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
        eprintln!("SKIP scheduled stale-audit acceptance: TWO_TEST_DATABASE_URL is not set");
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

// RED on current main: the permanent-refusal branch records the run only
// when the claim still holds; a definition replaced mid-post clears the
// claim, the recording lands nowhere, and no audit row is written.
#[tokio::test]
async fn permanent_refusal_with_replaced_definition_audits_stale_completion() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "refused-stale", T0 - 1_000).await;
    let (mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);

    let ticker = {
        let (pool, rest) = (pool.clone(), rest.clone());
        tokio::spawn(async move { tick(&pool, &rest, GUILD, || T0).await })
    };
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("the post reaches Discord before the row is replaced");
    // The claim goes stale first: replacing the definition clears it.
    seed(&pool, "refused-stale", T0 + 600_000).await;
    gate.release();
    ticker.await.unwrap().expect("tick");

    assert_eq!(posts(&mock).len(), 1, "the refusal is never re-posted");
    let replaced = row(&pool, "refused-stale").await;
    assert!(replaced.enabled, "the new definition stands");
    assert_eq!(replaced.next_run_at, format_iso_ms(T0 + 600_000));
    assert_eq!(replaced.claim_token, None);
    assert_eq!(
        audits(&pool, "refused-stale").await,
        [audit("stale_completion", Some("discord_rejected"))],
        "a permanently refused post whose claim went stale is audited, not silent"
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

// RED on current main: the same silent exit when the row is cancelled
// (deleted) while the refused post is in flight.
#[tokio::test]
async fn permanent_refusal_with_cancelled_definition_audits_stale_completion() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "refused-cancelled", T0 - 1_000).await;
    let (mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);

    let ticker = {
        let (pool, rest) = (pool.clone(), rest.clone());
        tokio::spawn(async move { tick(&pool, &rest, GUILD, || T0).await })
    };
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("the post reaches Discord before the row is cancelled");
    assert!(
        delete_scheduled(&pool, GUILD, "refused-cancelled")
            .await
            .expect("cancel scheduled row"),
        "the claimed row is deleted mid-post"
    );
    gate.release();
    ticker.await.unwrap().expect("tick");

    assert_eq!(posts(&mock).len(), 1, "the refusal is never re-posted");
    assert!(
        get_scheduled(&pool, GUILD, "refused-cancelled")
            .await
            .expect("read cancelled row")
            .is_none(),
        "the cancelled definition stays gone"
    );
    assert_eq!(
        audits(&pool, "refused-cancelled").await,
        [audit("stale_completion", Some("discord_rejected"))],
        "a permanently refused post whose row was cancelled is audited, not silent"
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

// RED on current main: recording the posted run fails, the orphan delete
// lands, but the claim is lost in between (definition replaced during
// the delete), so the re-queue retains nothing and the unrecorded
// outcome is never audited.
#[tokio::test]
async fn unrecorded_post_with_lost_claim_audits_the_unrecorded_outcome() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "unrecorded-stale", T0 - 1_000).await;
    refuse_run_recording(&pool).await;
    // The gate holds the orphan DELETE (the last scripted response): the
    // window between the failed recording and the re-queue attempt.
    let (mock, gate) = MockRest::start_gated(
        vec![posted(), ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);

    let ticker = {
        let (pool, rest) = (pool.clone(), rest.clone());
        tokio::spawn(async move { tick(&pool, &rest, GUILD, || T0).await })
    };
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("the orphan delete follows the failed recording");
    // The claim is lost before the re-queue: replacing clears it.
    seed(&pool, "unrecorded-stale", T0 + 600_000).await;
    gate.release();
    let outcome = ticker.await.unwrap();
    assert!(
        matches!(outcome, Err(ErrorClass::Database)),
        "a failed recording still fails the attempt, got {outcome:?}"
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, "DELETE");
    assert_eq!(requests[1].path, "/api/v10/channels/4444/messages/9001");
    assert_eq!(posts(&mock).len(), 1, "the orphan is never re-posted");
    let replaced = row(&pool, "unrecorded-stale").await;
    assert!(replaced.enabled, "the new definition stands");
    assert_eq!(replaced.next_run_at, format_iso_ms(T0 + 600_000));
    assert_eq!(
        audits(&pool, "unrecorded-stale").await,
        [audit("persistence_failed_cleaned", Some("database"))],
        "the unrecorded outcome is audited even when the claim was lost"
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

// Positive control (green throughout): the retained-claim permanent
// refusal keeps its exact current audit.
#[tokio::test]
async fn permanent_refusal_on_held_claim_still_audits_post_failed() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "refused-held", T0 - 1_000).await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);

    tick(pool, &rest, GUILD, || T0).await.expect("tick");

    assert_eq!(posts(&mock).len(), 1);
    let refused = row(pool, "refused-held").await;
    assert!(!refused.enabled, "a refused one-shot completes");
    assert_eq!(refused.last_message_id, None);
    assert_eq!(
        audits(pool, "refused-held").await,
        [audit("post_failed", Some("discord_rejected"))]
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

// Positive control (green throughout): the retained-claim unrecorded
// post keeps its exact current audit and re-queue.
#[tokio::test]
async fn unrecorded_post_with_retained_claim_still_audits_persistence_failure() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "unrecorded-held", T0 - 1_000).await;
    refuse_run_recording(&pool).await;
    let (mock, gate) = MockRest::start_gated(
        vec![posted(), ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);

    let ticker = {
        let (pool, rest) = (pool.clone(), rest.clone());
        tokio::spawn(async move { tick(&pool, &rest, GUILD, || T0).await })
    };
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("the orphan delete follows the failed recording");
    gate.release();
    let outcome = ticker.await.unwrap();
    assert!(
        matches!(outcome, Err(ErrorClass::Database)),
        "a failed recording still fails the attempt, got {outcome:?}"
    );

    assert_eq!(posts(&mock).len(), 1, "the orphan is never re-posted");
    let retained = row(&pool, "unrecorded-held").await;
    assert!(retained.enabled, "the unrecorded one-shot stays queued");
    assert_eq!(retained.next_run_at, format_iso_ms(T0));
    assert_eq!(retained.claim_token, None);
    assert_eq!(
        retained.occurrence_nonce, None,
        "the cleaned orphan's nonce is not kept"
    );
    assert_eq!(
        audits(&pool, "unrecorded-held").await,
        [audit("persistence_failed_cleaned", Some("database"))]
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

// Positive control (green throughout): the success path's stale-claim
// audit is the behavior the failure exits must match.
#[tokio::test]
async fn successful_post_with_replaced_definition_still_audits_stale_completion() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "posted-stale", T0 - 1_000).await;
    let (mock, gate) = MockRest::start_gated(vec![posted()], ScriptedResponse::status(204)).await;
    let rest = executor(&mock);

    let ticker = {
        let (pool, rest) = (pool.clone(), rest.clone());
        tokio::spawn(async move { tick(&pool, &rest, GUILD, || T0).await })
    };
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("the post reaches Discord before the row is replaced");
    seed(&pool, "posted-stale", T0 + 600_000).await;
    gate.release();
    ticker.await.unwrap().expect("tick");

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, "DELETE");
    assert_eq!(requests[1].path, "/api/v10/channels/4444/messages/9001");
    let replaced = row(&pool, "posted-stale").await;
    assert!(replaced.enabled, "the new definition stands");
    assert_eq!(replaced.next_run_at, format_iso_ms(T0 + 600_000));
    assert_eq!(
        audits(&pool, "posted-stale").await,
        [audit("stale_completion", None)]
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}
