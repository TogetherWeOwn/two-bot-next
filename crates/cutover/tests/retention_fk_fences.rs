//! Retention FK-child and replay-fence regression (not acceptance).
//!
//! Pins the three Paperclip Review findings on PR #923 that the acceptance
//! suite in `retention_windows.rs` does not cover without editing that file:
//! FK children must go before their parents, the operator-released replay
//! fence must survive a guild purge, and guard-refused deletions must be
//! reported as deferred/overdue instead of silent.

use two_bot_core::error_chain::RetentionStep;
use two_bot_cutover::retention::{
    handle_guild_delete, request_user_deletion, run_retention, RetentionMode,
};
use two_bot_testsupport::TestDatabase;

const GONE_GUILD: &str = "333333333333333333";
const OTHER_GUILD: &str = "444444444444444444";
const MEMBER: &str = "555555555555555555";

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the retention fence database suite"
            );
            eprintln!("SKIP retention fences: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .unwrap(),
    )
}

fn short_table(class_name: &str) -> &str {
    class_name.rsplit('.').next().unwrap_or(class_name)
}

/// Every foreign key whose parent is an erasure-plan table must either point
/// at a child that is also in the plan (deleted first, child before parent)
/// or ride an ON DELETE CASCADE. A missing child wedges the retention run
/// with 23503 and rolls back every other class.
#[tokio::test]
async fn fk_children_of_plan_tables_are_covered_or_cascade() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let plan = two_bot_cutover::member_erasure::plan();
    let plan_tables: std::collections::BTreeSet<&str> =
        plan.tables.iter().map(|e| e.table.as_str()).collect();
    let fks: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT conrelid::regclass::text, confrelid::regclass::text, confdeltype::text \
         FROM pg_constraint WHERE contype = 'f'",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut uncovered = Vec::new();
    for (child, parent, deltype) in &fks {
        let child = short_table(child);
        let parent = short_table(parent);
        if plan_tables.contains(parent) && !plan_tables.contains(child) && deltype != "c" {
            uncovered.push(format!("{child} -> {parent} ({deltype})"));
        }
    }
    assert!(
        uncovered.is_empty(),
        "FK children of plan tables must be in the plan or cascade: {uncovered:?}"
    );
    // The two wedges from the review must be covered.
    for table in ["self_role_exchanges", "moderation_channel_executions"] {
        assert!(
            plan_tables.contains(table),
            "{table} must be in the erasure plan"
        );
    }
    db.close().await.unwrap();
}

/// A guild purge deletes the FK children first (no 23503) and keeps the
/// operator-released replay fence in both the parent and the child.
#[tokio::test]
async fn guild_purge_deletes_fk_children_but_keeps_operator_fence() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // One member row so the purge has something to delete.
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(GONE_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    // Self-role audit + exchange child for the same guild.
    sqlx::query(
        "INSERT INTO self_role_audit
           (event_id, guild_id, panel_id, member_id, source_id, source, operation, outcome,
            added_role_ids, removed_role_ids, created_at)
         VALUES ('fence-evt-1', $1, 'p', $2, 's', 'button', 'assign', 'assigned', '[]', '[]', '2026-10-01T00:00:00Z')",
    )
    .bind(GONE_GUILD)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO self_role_exchanges (event_id, origin_generation, role_id, adding, compensating)
         VALUES ('fence-evt-1', 1, '101', TRUE, FALSE)",
    )
    .execute(pool)
    .await
    .unwrap();
    // Ordinary idempotency parent + execution child (both must go).
    sqlx::query(
        "INSERT INTO moderation_idempotency
           (guild_id, idempotency_key, action, request_hash, state, claimed_at, result_json)
         VALUES ($1, 'fence-ordinary', 'warn', 'h', 'done', '2026-10-01T00:00:00Z', '{}')",
    )
    .bind(GONE_GUILD)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO moderation_channel_executions (channel_id, guild_id, idempotency_key, claim_token)
         VALUES ('fence-chan-ordinary', $1, 'fence-ordinary', 'tok')",
    )
    .bind(GONE_GUILD)
    .execute(pool)
    .await
    .unwrap();
    // Operator-released fence parent + child (both must survive).
    sqlx::query(
        "INSERT INTO moderation_idempotency
           (guild_id, idempotency_key, action, request_hash, state, outcome, claimed_at, result_json)
         VALUES ($1, 'fence-released', 'warn', 'operator_released', 'done', 'operator_released', '2026-10-01T00:00:00Z', '{}')",
    )
    .bind(GONE_GUILD)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO moderation_channel_executions (channel_id, guild_id, idempotency_key, claim_token)
         VALUES ('fence-chan-released', $1, 'fence-released', 'tok')",
    )
    .bind(GONE_GUILD)
    .execute(pool)
    .await
    .unwrap();

    assert!(handle_guild_delete(pool, GONE_GUILD, None).await.unwrap());
    let report = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert!(
        report.guild_rows_purged > 0,
        "purge must delete rows, got {report:?}"
    );
    let gone: i64 = sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1")
        .bind(GONE_GUILD)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(gone, 0, "member row must be purged");
    let exchanges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM self_role_exchanges WHERE event_id = 'fence-evt-1'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        exchanges, 0,
        "exchange child must be purged before its audit row"
    );
    let audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM self_role_audit WHERE event_id = 'fence-evt-1'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        audits, 0,
        "audit parent must be purged after its exchange child"
    );
    let ordinary: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moderation_channel_executions WHERE channel_id = 'fence-chan-ordinary'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(ordinary, 0, "ordinary execution child must be purged");
    let fence_parent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moderation_idempotency WHERE guild_id = $1 AND idempotency_key = 'fence-released'",
    )
    .bind(GONE_GUILD)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        fence_parent, 1,
        "operator-released fence parent must survive the purge"
    );
    let fence_child: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moderation_channel_executions WHERE channel_id = 'fence-chan-released'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        fence_child, 1,
        "operator-released fence child must survive with its parent"
    );
    db.close().await.unwrap();
}

/// A guard-refused deletion stays pending and is reported as deferred (and
/// overdue when older than the SLA), never silently.
#[tokio::test]
async fn refused_deletion_is_deferred_and_overdue() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(OTHER_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    // In-flight fence so the guard refuses.
    sqlx::query(
        "INSERT INTO internal_idempotency
           (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id)
         VALUES (repeat('a', 64), repeat('b', 64), 'role.assign', repeat('c', 64), 'unknown', $1, $2, $2)",
    )
    .bind(OTHER_GUILD)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    let request_id = request_user_deletion(pool, OTHER_GUILD, MEMBER)
        .await
        .unwrap();
    // Backdate the request past the 72h SLA.
    sqlx::query(
        "UPDATE retention_deletion_requests SET requested_at = now() - make_interval(hours => 73) WHERE request_id = $1",
    )
    .bind(&request_id)
    .execute(pool)
    .await
    .unwrap();

    let dry = run_retention(pool, RetentionMode::DryRun).await.unwrap();
    assert_eq!(
        dry.deletions_deferred, 1,
        "dry run must report the guarded request as deferred, got {dry:?}"
    );
    assert_eq!(
        dry.deletions_overdue, 1,
        "dry run must report the backdated request as overdue, got {dry:?}"
    );
    assert_eq!(
        dry.deletions_completed, 0,
        "dry run must report what the live run will delete: a guarded request is deferred, not completed, got {dry:?}"
    );
    let live = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        live.deletions_completed, 0,
        "guarded request must not complete"
    );
    assert_eq!(
        dry.deletions_completed, live.deletions_completed,
        "dry and live runs must agree on completions for a guarded request, got dry {dry:?} live {live:?}"
    );
    assert_eq!(
        live.deletions_deferred, 1,
        "live run must report the refusal as deferred, got {live:?}"
    );
    assert_eq!(
        live.deletions_overdue, 1,
        "live run must report the overdue request, got {live:?}"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE request_id = $1 AND status = 'pending'",
    )
    .bind(&request_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(pending, 1, "refused request must stay pending");
    db.close().await.unwrap();
}

/// A failing guild purge returns `Ok` with a failure counter, not a silent
/// warn-only success: the guild stays queued with its rows intact, and the
/// next run reports the failure again instead of an all-zero success.
#[tokio::test]
async fn failing_guild_purge_is_counted_and_stays_queued() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(GONE_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    // Wedge: a NO ACTION foreign key onto the departed guild's member row so
    // the purge's member delete fails with 23503. The wedge table is outside
    // the erasure plan, so the purge never deletes the blocking row itself.
    // The column is not named `member_id`: an uncovered identity column is a
    // schema gap, which would defer the purge before the FK can fail.
    sqlx::query(
        "CREATE TABLE retention_purge_wedge (
           id SERIAL PRIMARY KEY,
           guild_id TEXT NOT NULL,
           held_ref TEXT NOT NULL,
           FOREIGN KEY (guild_id, held_ref) REFERENCES members (guild_id, member_id)
         )",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO retention_purge_wedge (guild_id, held_ref) VALUES ($1, $2)")
        .bind(GONE_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();

    assert!(handle_guild_delete(pool, GONE_GUILD, None).await.unwrap());
    let dry = run_retention(pool, RetentionMode::DryRun).await.unwrap();
    assert_eq!(
        dry.guild_purges_failed, 0,
        "dry run attempts no purges, so it reports no failures, got {dry:?}"
    );
    assert!(
        dry.guild_rows_purged > 0,
        "dry run must still report the wedged guild's pending rows, got {dry:?}"
    );

    let first = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        first.guild_purges_failed, 1,
        "a wedged guild purge must be counted, got {first:?}"
    );
    assert_eq!(
        first.guild_rows_purged, 0,
        "the failed guild contributes no purged rows, got {first:?}"
    );
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM retention_guild_purge_queue WHERE guild_id = $1")
            .bind(GONE_GUILD)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(queued, 1, "a failed purge must stay queued");
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1")
        .bind(GONE_GUILD)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        members, 1,
        "a failed purge must leave the guild's rows intact"
    );

    let second = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        second.guild_purges_failed, 1,
        "the stuck guild must fail again instead of reporting an all-zero success, got {second:?}"
    );
    assert_eq!(
        second.guild_rows_purged, 0,
        "no rows are purged while the wedge holds, got {second:?}"
    );
    db.close().await.unwrap();
}

/// A schema-coverage refusal defers the pending request instead of aborting
/// the run: time-window deletes commit, the gapped request stays pending,
/// every queued guild stays queued, and the request completes once the gap
/// is gone.
#[tokio::test]
async fn schema_gap_defers_request_without_rolling_back_time_window() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // One 31-day presence row: the time-window work that must commit.
    sqlx::query(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
         VALUES ($1, to_char(now() - interval '31 days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 10, NULL)",
    )
    .bind(OTHER_GUILD)
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
    // A queued departed guild: its purge must defer while the gap exists,
    // or the queue row would dequeue work the incomplete plan never deleted.
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(GONE_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    assert!(handle_guild_delete(pool, GONE_GUILD, None).await.unwrap());
    // Uncovered identity column outside the erasure plan: `owner_id` trips
    // the naming tripwire, so `schema_gaps` is nonempty for this run only.
    sqlx::query(
        "CREATE TABLE retention_schema_gap_probe (guild_id TEXT NOT NULL, owner_id TEXT NOT NULL)",
    )
    .execute(pool)
    .await
    .unwrap();

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
    assert_eq!(
        dry.guild_rows_purged, 0,
        "dry run must not promise a guild purge while gapped, got {dry:?}"
    );
    assert_eq!(
        dry.guild_purges_failed, 1,
        "dry run must report the gapped guild as failed, got {dry:?}"
    );

    let live = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        live.presence_raw_deleted, 1,
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
    assert_eq!(
        live.guild_rows_purged, 0,
        "no guild rows may purge while the gap exists, got {live:?}"
    );
    assert_eq!(
        live.guild_purges_failed, 1,
        "the queued guild must defer while the gap exists, got {live:?}"
    );
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM presence_probe WHERE guild_id = $1")
            .bind(OTHER_GUILD)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        remaining, 0,
        "the 31-day presence row must be gone: the run committed"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE request_id = $1 AND status = 'pending'",
    )
    .bind(&request_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(pending, 1, "the gapped request must stay pending");
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM retention_guild_purge_queue WHERE guild_id = $1")
            .bind(GONE_GUILD)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(queued, 1, "the queued guild must stay queued while gapped");
    let guild_members: i64 = sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1")
        .bind(GONE_GUILD)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        guild_members, 1,
        "the queued guild's rows must survive while gapped"
    );

    // Removing the gap unblocks the same request and guild on the next run.
    sqlx::query("DROP TABLE retention_schema_gap_probe")
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
    db.close().await.unwrap();
}

/// An unexpected FK failure while draining a user deletion reports the active
/// deletion-intake stage instead of inheriting the Execute-mode expiry stage.
#[tokio::test]
async fn deletion_drain_database_failure_reports_deletion_intake_step() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(GONE_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE retention_failure_probe (
            parent_guild TEXT NOT NULL,
            parent_member TEXT NOT NULL,
            FOREIGN KEY (parent_guild, parent_member)
                REFERENCES members (guild_id, member_id) ON DELETE RESTRICT
        )",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO retention_failure_probe VALUES ($1, $2)")
        .bind(GONE_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    request_user_deletion(pool, GONE_GUILD, MEMBER)
        .await
        .unwrap();

    let failure = run_retention(pool, RetentionMode::Execute)
        .await
        .unwrap_err();
    assert_eq!(failure.step(), RetentionStep::DeletionIntake);
    db.close().await.unwrap();
}
