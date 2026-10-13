//! Retention FK-child and replay-fence regression (not acceptance).
//!
//! Pins the three Paperclip Review findings on PR #923 that the acceptance
//! suite in `retention_windows.rs` does not cover without editing that file:
//! FK children must go before their parents, the operator-released replay
//! fence must survive a guild purge, and guard-refused deletions must be
//! reported as deferred/overdue instead of silent.

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
    let live = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        live.deletions_completed, 0,
        "guarded request must not complete"
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
