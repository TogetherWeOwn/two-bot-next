//! Schema-gap deferral acceptance for PR #958 (not regression).
//!
//! Pins the shipped behavior: a schema-coverage refusal defers only the
//! gapped deletions, time-window work already done is not rolled back, and
//! the run completes. Each test fails without the fix (the pre-fix run
//! aborts with the schema error instead of returning a deferral report) and
//! passes with it. Test files only; no production code.
//!
//! Split from the `retention_fk_fences.rs` regression on purpose: one
//! focused test per acceptance criterion, a distinct probe table
//! (`retention_gap_acceptance_probe`), a distinct time-window class
//! (audit cache, not presence), and distinct guild/member IDs.

use two_bot_cutover::retention::{
    handle_guild_delete, request_user_deletion, run_retention, RetentionMode,
};
use two_bot_testsupport::TestDatabase;

const GONE_GUILD: &str = "777777777777777777";
const OTHER_GUILD: &str = "888888888888888888";
const MEMBER: &str = "999999999999999999";

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the retention acceptance database suite"
            );
            eprintln!("SKIP schema-gap acceptance: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .unwrap(),
    )
}

async fn create_gap_probe(pool: &sqlx::PgPool) {
    // `owner_id` trips the identity-column tripwire while the table stays
    // outside the erasure plan, so `schema_gaps` is nonempty for this run.
    sqlx::query(
        "CREATE TABLE retention_gap_acceptance_probe (guild_id TEXT NOT NULL, owner_id TEXT NOT NULL)",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// The gapped request defers instead of aborting the run: dry and live both
/// report it as deferred (distinguished as a schema gap), with no
/// completion and the request left pending.
#[tokio::test]
async fn schema_gapped_deletion_defers_instead_of_aborting() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(OTHER_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    let request_id = request_user_deletion(pool, OTHER_GUILD, MEMBER)
        .await
        .unwrap();
    create_gap_probe(pool).await;

    let dry = run_retention(pool, RetentionMode::DryRun).await.unwrap();
    assert_eq!(
        dry.deletions_deferred, 1,
        "dry run must report the schema-gapped request as deferred, got {dry:?}"
    );
    assert_eq!(
        dry.schema_gap_deferrals, 1,
        "dry run must distinguish the gap deferral, got {dry:?}"
    );
    assert_eq!(
        dry.deletions_completed, 0,
        "dry run must not promise a completion while gapped, got {dry:?}"
    );

    // Pre-fix this returns Err (the schema refusal aborts the run); post-fix
    // it returns a deferral report.
    let live = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        live.deletions_completed, 0,
        "the gapped request must not complete, got {live:?}"
    );
    assert_eq!(
        live.deletions_deferred, 1,
        "the gapped request must defer, not abort the run, got {live:?}"
    );
    assert_eq!(
        live.schema_gap_deferrals, 1,
        "the gap deferral must be distinguishable from a guard, got {live:?}"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE request_id = $1 AND status = 'pending'",
    )
    .bind(&request_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(pending, 1, "the gapped request must stay pending");
    db.close().await.unwrap();
}

/// Time-window work commits despite the gap: a 46-day audit-cache row is
/// deleted and stays deleted while the gapped request defers.
#[tokio::test]
async fn schema_gap_does_not_roll_back_completed_time_window_work() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO operational_audit_log
           (entry_id, event_kind, guild_id, occurred_at, actor_id, target_id, metadata_json, created_at)
         VALUES ('gap-acceptance-old', 'fixture', $1, now() - interval '46 days', $2, $2, '{}', now())",
    )
    .bind(OTHER_GUILD)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(OTHER_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    let request_id = request_user_deletion(pool, OTHER_GUILD, MEMBER)
        .await
        .unwrap();
    create_gap_probe(pool).await;

    // Pre-fix this returns Err and rolls the audit delete back; post-fix it
    // commits the time window and defers only the gapped request.
    let live = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        live.audit_cache_deleted, 1,
        "time-window deletes must commit despite the schema gap, got {live:?}"
    );
    assert_eq!(
        live.deletions_completed, 0,
        "the gapped request must not complete, got {live:?}"
    );
    assert_eq!(
        live.deletions_deferred, 1,
        "the gapped request must defer, not abort the run, got {live:?}"
    );
    assert_eq!(
        live.schema_gap_deferrals, 1,
        "the gap deferral must be distinguishable from a guard, got {live:?}"
    );
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operational_audit_log WHERE entry_id = 'gap-acceptance-old'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        remaining, 0,
        "the 46-day audit row must be gone: the run committed"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE request_id = $1 AND status = 'pending'",
    )
    .bind(&request_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(pending, 1, "the gapped request must stay pending");
    db.close().await.unwrap();
}

/// The gapped run completes (no abort, queued guild stays queued) and the
/// very next run after gap removal settles the request and purges the guild.
#[tokio::test]
async fn gapped_run_completes_and_settles_after_gap_removal() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(OTHER_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    let request_id = request_user_deletion(pool, OTHER_GUILD, MEMBER)
        .await
        .unwrap();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(GONE_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    assert!(handle_guild_delete(pool, GONE_GUILD, None).await.unwrap());
    create_gap_probe(pool).await;

    // Pre-fix this returns Err; post-fix the run completes with everything
    // gapped deferred and queued.
    let live = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        live.deletions_deferred, 1,
        "the gapped request must defer, not abort the run, got {live:?}"
    );
    assert_eq!(
        live.schema_gap_deferrals, 1,
        "the gap deferral must be distinguishable from a guard, got {live:?}"
    );
    assert_eq!(
        live.guild_rows_purged, 0,
        "no guild rows may purge while the gap exists, got {live:?}"
    );
    assert_eq!(
        live.guild_purges_failed, 1,
        "the queued guild must defer while the gap exists, got {live:?}"
    );
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM retention_guild_purge_queue WHERE guild_id = $1")
            .bind(GONE_GUILD)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(queued, 1, "the queued guild must stay queued while gapped");

    sqlx::query("DROP TABLE retention_gap_acceptance_probe")
        .execute(pool)
        .await
        .unwrap();
    let settled = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        settled.deletions_completed, 1,
        "the deferred request must complete once the gap is gone, got {settled:?}"
    );
    assert_eq!(settled.deletions_deferred, 0);
    assert_eq!(settled.schema_gap_deferrals, 0);
    assert_eq!(
        settled.guild_purges_failed, 0,
        "the deferred guild must purge once the gap is gone, got {settled:?}"
    );
    assert!(
        settled.guild_rows_purged > 0,
        "the deferred guild's rows must purge once the gap is gone, got {settled:?}"
    );
    let completed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE request_id = $1 AND status = 'completed'",
    )
    .bind(&request_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(completed, 1, "the deferred request must yield a receipt");
    db.close().await.unwrap();
}
