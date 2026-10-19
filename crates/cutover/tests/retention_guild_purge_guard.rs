//! Guild-purge replay-guard regression (Builder-owned, TOG-24247).
//!
//! `purge_guild` must check unresolved replay guards across the entire guild
//! before deleting any guild rows; a guarded guild stays queued with rows
//! untouched until the guards settle. This file is owned by the purge-guard
//! fix, not the Test Author's `retention_windows` acceptance suite, which is
//! left unmodified.
//!
//! Fixed fake snowflakes only. Disposable test databases via
//! `two-bot-testsupport`; never staging or production bindings, never real
//! user data, no secrets in code or comments.

use sqlx::PgPool;
use two_bot_cutover::retention::{
    handle_guild_delete, request_user_deletion, run_retention, RetentionMode, RetentionReport,
};
use two_bot_testsupport::TestDatabase;

const GONE_GUILD: &str = "333333333333333333";
const MEMBER: &str = "444444444444444444";
const PENDING_MEMBER: &str = "555555555555555555";
const GUARDED_OTHER: &str = "666666666666666666";

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the retention database suite"
            );
            eprintln!("SKIP retention integration: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .unwrap(),
    )
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// A queued guild purge with an unresolved replay guard deletes nothing and
/// stays queued; once the guard settles the purge proceeds.
#[tokio::test]
async fn guild_purge_with_unresolved_guard_keeps_rows_queued() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(GONE_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
         VALUES ($1, $2, 150, 150, now())",
    )
    .bind(GONE_GUILD)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
         VALUES ($1, to_char(now() - interval '1 day', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 10, NULL)",
    )
    .bind(GONE_GUILD)
    .execute(pool)
    .await
    .unwrap();
    // In-flight idempotency intent for the departed guild.
    sqlx::query(
        "INSERT INTO internal_idempotency
           (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id)
         VALUES (repeat('a', 64), repeat('b', 64), 'role.assign', repeat('c', 64), 'unknown', $1, $2, $2)",
    )
    .bind(GONE_GUILD)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    assert!(
        handle_guild_delete(pool, GONE_GUILD, None).await.unwrap(),
        "a real removal enqueues a purge"
    );
    // Backdate the queue row so the deferred-age signal has a known floor.
    sqlx::query(
        "UPDATE retention_guild_purge_queue SET enqueued_at = clock_timestamp() - interval '2 hours' WHERE guild_id = $1",
    )
    .bind(GONE_GUILD)
    .execute(pool)
    .await
    .unwrap();

    // The dry run must preview exactly what the live run will do: no rows,
    // no failure, one guard deferral with the queue age attached.
    let dry: RetentionReport = run_retention(pool, RetentionMode::DryRun).await.unwrap();
    assert_eq!(
        dry.guild_rows_purged, 0,
        "the dry run must not report a guarded guild's rows as purged, got {dry:?}"
    );
    assert_eq!(
        dry.guild_purges_failed, 0,
        "a guard deferral is not a failure, got {dry:?}"
    );
    assert_eq!(
        dry.guild_purges_deferred, 1,
        "the dry run must report the guarded guild as deferred, got {dry:?}"
    );
    assert!(
        dry.guild_purges_deferred_oldest_secs >= 7200,
        "the dry run must report the queue age, got {dry:?}"
    );

    let report: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        report.guild_rows_purged, 0,
        "a guarded purge must delete nothing, got {report:?}"
    );
    assert_eq!(
        report.guild_purges_failed, 0,
        "a guard deferral is not a failure, got {report:?}"
    );
    assert_eq!(
        report.guild_purges_deferred, 1,
        "a guarded purge must stay queued as deferred, got {report:?}"
    );
    assert_eq!(
        (dry.guild_rows_purged, dry.guild_purges_deferred),
        (report.guild_rows_purged, report.guild_purges_deferred),
        "the live run must delete and defer exactly what the dry run reported, dry {dry:?} vs live {report:?}"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM internal_idempotency WHERE guild_id = '333333333333333333'"
        )
        .await,
        1,
        "the in-flight intent must survive the guarded purge"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM member_levels WHERE guild_id = '333333333333333333'"
        )
        .await,
        1,
        "the guild's rows must survive the guarded purge"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM presence_probe WHERE guild_id = '333333333333333333'"
        )
        .await,
        1,
        "the guild's probe rows must survive the guarded purge"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM retention_guild_purge_queue WHERE guild_id = '333333333333333333'"
        )
        .await,
        1,
        "the guarded guild must stay queued"
    );

    // Settle the fence; the next run must purge the guild.
    sqlx::query(
        "UPDATE internal_idempotency SET state = 'completed', response_code = 'success',
         http_status = 200, resource_id = actor_id, affected = 1 WHERE guild_id = $1 AND actor_id = $2",
    )
    .bind(GONE_GUILD)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    let settled: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert!(
        settled.guild_rows_purged > 0,
        "the settled purge must delete rows, got {settled:?}"
    );
    assert_eq!(
        (settled.guild_purges_failed, settled.guild_purges_deferred),
        (0, 0),
        "the settled purge must report no failure and no deferral, got {settled:?}"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM retention_guild_purge_queue WHERE guild_id = '333333333333333333'"
        )
        .await,
        0,
        "the settled guild must dequeue"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM member_levels WHERE guild_id = '333333333333333333'"
        )
        .await,
        0,
        "the settled guild's rows must go"
    );
    db.close().await.unwrap();
}

/// Seed two members in the queued guild with a pending user-deletion request
/// for `PENDING_MEMBER` only. The guarded member holds no request: its guard
/// row must still defer the guild purge in both modes.
async fn seed_unrelated_pending(pool: &PgPool) {
    for member in [PENDING_MEMBER, GUARDED_OTHER] {
        sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
            .bind(GONE_GUILD)
            .bind(member)
            .execute(pool)
            .await
            .unwrap();
    }
    request_user_deletion(pool, GONE_GUILD, PENDING_MEMBER)
        .await
        .unwrap();
    assert!(
        handle_guild_delete(pool, GONE_GUILD, None).await.unwrap(),
        "a real removal enqueues a purge"
    );
}

/// An unrelated pending deletion must not hide another member's unresolved
/// guard: the dry run and the live run must both defer the guild with rows
/// untouched, and the guard row must survive.
async fn assert_unrelated_pending_still_defers(pool: &PgPool, guard_count_sql: &'static str) {
    let dry: RetentionReport = run_retention(pool, RetentionMode::DryRun).await.unwrap();
    assert_eq!(
        dry.guild_rows_purged, 0,
        "an unrelated pending deletion must not let the dry run purge a guarded guild, got {dry:?}"
    );
    assert_eq!(
        dry.guild_purges_failed, 0,
        "a guard deferral is not a failure, got {dry:?}"
    );
    assert_eq!(
        dry.guild_purges_deferred, 1,
        "the dry run must defer the guarded guild, got {dry:?}"
    );
    let live: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        live.guild_rows_purged, 0,
        "an unrelated pending deletion must not let the live run purge a guarded guild, got {live:?}"
    );
    assert_eq!(
        (live.guild_purges_failed, live.guild_purges_deferred),
        (0, 1),
        "the guarded guild must stay queued as deferred, got {live:?}"
    );
    assert_eq!(
        (dry.guild_rows_purged, dry.guild_purges_deferred),
        (live.guild_rows_purged, live.guild_purges_deferred),
        "the live run must defer exactly what the dry run reported, dry {dry:?} vs live {live:?}"
    );
    assert_eq!(
        count(pool, guard_count_sql).await,
        1,
        "the other member's guard row must survive the deferred purge"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM retention_guild_purge_queue WHERE guild_id = '333333333333333333'"
        )
        .await,
        1,
        "the guarded guild must stay queued"
    );
}

#[tokio::test]
async fn unrelated_pending_does_not_hide_internal_guard() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // Nullable target/resource stay NULL: the per-member predicate for the
    // pending member is NULL here, so a NOT (...) exclusion would hide it.
    sqlx::query(
        "INSERT INTO internal_idempotency
           (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id)
         VALUES (repeat('a', 64), repeat('b', 64), 'role.assign', repeat('c', 64), 'unknown', $1, $2)",
    )
    .bind(GONE_GUILD)
    .bind(GUARDED_OTHER)
    .execute(pool)
    .await
    .unwrap();
    seed_unrelated_pending(pool).await;
    assert_unrelated_pending_still_defers(
        pool,
        "SELECT count(*) FROM internal_idempotency WHERE guild_id = '333333333333333333'",
    )
    .await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn unrelated_pending_does_not_hide_moderation_guard() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // In-flight claim: NULL result_json with the key naming the guarded
    // member, so the pending member's predicate branch is NULL.
    sqlx::query(
        "INSERT INTO moderation_idempotency
           (guild_id, idempotency_key, action, request_hash, state, claimed_at)
         VALUES ($1, 'mod-guard-666666666666666666-k', 'warn', 'fixture', 'claimed', '2026-10-01T00:00:00Z')",
    )
    .bind(GONE_GUILD)
    .execute(pool)
    .await
    .unwrap();
    seed_unrelated_pending(pool).await;
    assert_unrelated_pending_still_defers(
        pool,
        "SELECT count(*) FROM moderation_idempotency WHERE guild_id = '333333333333333333'",
    )
    .await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn unrelated_pending_does_not_hide_containment_guard() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // Active incident with no result yet: the pending member's result_json
    // branch is NULL, so a NOT (...) exclusion would hide it.
    sqlx::query(
        "INSERT INTO containment_events
           (audit_entry_id, guild_id, executor_id, action, target_id, weight, occurred_at, state, reason, created_at)
         VALUES ('guard-evt-1', $1, $2, 'ban', $2, 1, '2026-10-01T00:00:00Z', 'contain', 'fixture', '2026-10-01T00:00:00Z')",
    )
    .bind(GONE_GUILD)
    .bind(GUARDED_OTHER)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO containment_incidents
           (id, guild_id, executor_id, trigger_audit_entry_id, heat, state, started_at)
         VALUES ('guard-inc-1', $1, $2, 'guard-evt-1', 3, 'containing', '2026-10-01T00:00:00Z')",
    )
    .bind(GONE_GUILD)
    .bind(GUARDED_OTHER)
    .execute(pool)
    .await
    .unwrap();
    seed_unrelated_pending(pool).await;
    assert_unrelated_pending_still_defers(
        pool,
        "SELECT count(*) FROM containment_incidents WHERE guild_id = '333333333333333333'",
    )
    .await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn unrelated_pending_does_not_hide_automod_guard() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // Unsettled claim with no stored author: the pending member's author
    // predicate branch is NULL, so a NOT (...) exclusion would hide it.
    sqlx::query(
        "INSERT INTO automod_delivery_claims
           (guild_id, message_id, delivery_kind, dry_run, request_hash, mutation_started)
         VALUES ($1, 'guard-msg-1', 'create', FALSE, 'fixture', TRUE)",
    )
    .bind(GONE_GUILD)
    .execute(pool)
    .await
    .unwrap();
    seed_unrelated_pending(pool).await;
    assert_unrelated_pending_still_defers(
        pool,
        "SELECT count(*) FROM automod_delivery_claims WHERE guild_id = '333333333333333333'",
    )
    .await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn unrelated_pending_does_not_hide_self_role_guard() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // Unsettled self-role effect for the other member.
    sqlx::query(
        "INSERT INTO self_role_audit
           (event_id, guild_id, panel_id, member_id, source_id, source, operation, outcome,
            added_role_ids, removed_role_ids, created_at)
         VALUES ('guard-evt-1', $1, 'p', $2, 's', 'button', 'assign', 'started', '[]', '[]', '2026-10-01T00:00:00Z')",
    )
    .bind(GONE_GUILD)
    .bind(GUARDED_OTHER)
    .execute(pool)
    .await
    .unwrap();
    seed_unrelated_pending(pool).await;
    assert_unrelated_pending_still_defers(
        pool,
        "SELECT count(*) FROM self_role_audit WHERE guild_id = '333333333333333333'",
    )
    .await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn unrelated_pending_does_not_hide_panel_claim_guard() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // Live panel-claim lease for the other member.
    sqlx::query(
        "INSERT INTO self_role_panel_claims
           (guild_id, member_id, panel_id, claim_token, claim_generation, processing_expires_at)
         VALUES ($1, $2, 'p', 'guard-claim', 1, clock_timestamp() + interval '1 hour')",
    )
    .bind(GONE_GUILD)
    .bind(GUARDED_OTHER)
    .execute(pool)
    .await
    .unwrap();
    seed_unrelated_pending(pool).await;
    assert_unrelated_pending_still_defers(
        pool,
        "SELECT count(*) FROM self_role_panel_claims WHERE guild_id = '333333333333333333'",
    )
    .await;
    db.close().await.unwrap();
}
