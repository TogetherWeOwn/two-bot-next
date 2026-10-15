//! Retention deferral and dry-run acceptance (M5.4, Test Author suite).
//!
//! Locks three shipped fixes with one focused test per criterion, using
//! fixtures distinct from the existing suites (`retention_fk_fences.rs`,
//! `retention_schema_gap_acceptance.rs` for #958/#962,
//! `retention_dry_run_projection.rs` for #960,
//! `retention_guild_purge_guard.rs` for #959):
//!
//! 1. A schema-gapped deletion defers without rolling back the run: the
//!    time-window stage commits, the gapped request stays pending, and both
//!    modes report it as a distinguishable schema-gap deferral.
//! 2. Dry-run counts exclude staged deletes: per-class counts and the total
//!    agree exactly with the following Execute run (no double-count).
//! 3. A guild purge is refused while replay guards are unresolved: no rows
//!    go, the guild stays queued as deferred (never failed), and the purge
//!    proceeds once the guard settles. This suite pins the containment
//!    fence; the Builder-owned suite pins the idempotency-intent fence.
//!
//! Each test fails without its fix and passes with it (verified by reading
//! the pre-fix parents: pre-#958 the pending drain hits
//! `Err(err) => return Err(err)`, aborting the run and rolling back the
//! time-window deletes, so the live `unwrap` panics; pre-#960 the dry-run
//! guild projection counts pre-stage snapshot rows the earlier stages
//! remove, so dry and live per-class counts disagree; pre-#959
//! `purge_guild` deletes with no guard probe, so rows vanish and the queue
//! dequeues). Test files only; no production code.
//!
//! The paired Builder card (ledger/property tests plus minimal seams) builds
//! on this branch without editing this file; these tests pass unmodified.
//!
//! Fixed fake snowflakes only. Disposable test databases via
//! `two-bot-testsupport`; never staging or production bindings, never real
//! user data, no secrets in code or comments. Without
//! `TWO_TEST_DATABASE_URL` each test skips, so `cargo test` stays green
//! offline and CI proves the behavior.

use two_bot_cutover::retention::{
    handle_guild_delete, request_user_deletion, run_retention, RetentionMode, RetentionReport,
};
use two_bot_testsupport::TestDatabase;

const DEFER_GUILD: &str = "111111111111111111";
const DEFER_MEMBER: &str = "121212121212121212";
const COUNT_GUILD: &str = "141414141414141414";
const COUNT_MEMBER: &str = "151515151515151515";
const GUARD_GUILD: &str = "161616161616161616";
const GUARD_MEMBER: &str = "171717171717171717";

async fn database(suite: &str) -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the retention deferral acceptance suite"
            );
            eprintln!("SKIP {suite}: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .unwrap(),
    )
}

/// A schema-gapped deletion defers without rolling back the run: the
/// presence time-window stage commits while the gapped request stays
/// pending, reported as a schema-gap deferral in both modes.
#[tokio::test]
async fn schema_gapped_deletion_defers_ledger_without_rolling_back_run() {
    let Some(db) = database("schema-gap deferral ledger").await else {
        return;
    };
    let pool = db.pool();
    // One expired presence row (time-window stage) for the gapped guild.
    sqlx::query(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
         VALUES ($1, to_char(now() - interval '31 days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 10, NULL)",
    )
    .bind(DEFER_GUILD)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(DEFER_GUILD)
        .bind(DEFER_MEMBER)
        .execute(pool)
        .await
        .unwrap();
    let request_id = request_user_deletion(pool, DEFER_GUILD, DEFER_MEMBER)
        .await
        .unwrap();
    // `owner_id` trips the identity-column tripwire while the table stays
    // outside the erasure plan, so `schema_gaps` is nonempty for this run.
    // Distinct probe table from the #962 acceptance suite on purpose.
    sqlx::query(
        "CREATE TABLE retention_deferral_acceptance_probe (guild_id TEXT NOT NULL, owner_id TEXT NOT NULL)",
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
        "dry run must distinguish the gap deferral from a guard, got {dry:?}"
    );
    assert_eq!(
        dry.deletions_completed, 0,
        "dry run must not promise a completion while gapped, got {dry:?}"
    );

    // Pre-fix this returns Err (the schema refusal aborts the run and rolls
    // the presence delete back); post-fix it commits the time window and
    // defers only the gapped request.
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
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM presence_probe WHERE guild_id = $1")
            .bind(DEFER_GUILD)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        remaining, 0,
        "the expired presence row must be gone: the run committed"
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

/// Dry-run counts exclude staged deletes: with an expired presence row, an
/// eligible pending deletion, and a queued purge overlapping in one guild,
/// dry-run per-class counts and the total agree exactly with Execute.
#[tokio::test]
async fn dry_run_counts_exclude_staged_deletes_without_double_count() {
    let Some(db) = database("dry-run staged-delete counts").await else {
        return;
    };
    let pool = db.pool();
    // presence_probe: one expired (time-window stage) and one fresh (guild
    // stage) row for the queued guild.
    sqlx::query(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
         VALUES
           ($1, to_char(now() - interval '31 days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 10, NULL),
           ($1, to_char(now() - interval '1 day', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 11, NULL)",
    )
    .bind(COUNT_GUILD)
    .execute(pool)
    .await
    .unwrap();
    // Member footprint for the deletion user: the user-deletion stage takes
    // these rows, so the guild stage must not count them again.
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(COUNT_GUILD)
        .bind(COUNT_MEMBER)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
         VALUES ($1, $2, 10, 10, now())",
    )
    .bind(COUNT_GUILD)
    .bind(COUNT_MEMBER)
    .execute(pool)
    .await
    .unwrap();
    request_user_deletion(pool, COUNT_GUILD, COUNT_MEMBER)
        .await
        .unwrap();
    assert!(
        handle_guild_delete(pool, COUNT_GUILD, None).await.unwrap(),
        "a real removal enqueues a purge"
    );

    let dry = run_retention(pool, RetentionMode::DryRun).await.unwrap();
    let live = run_retention(pool, RetentionMode::Execute).await.unwrap();
    // Execute ground truth: 1 expired presence row in the time-window
    // stage, the eligible deletion completes, and only the fresh presence
    // row is left for the guild stage.
    assert_eq!(
        (
            live.presence_raw_deleted,
            live.deletions_completed,
            live.deletions_deferred
        ),
        (1, 1, 0),
        "execute must delete each row once across stages, got {live:?}"
    );
    assert_eq!(
        (
            dry.presence_raw_deleted,
            dry.guild_rows_purged,
            dry.deletions_completed,
            dry.deletions_deferred,
        ),
        (
            live.presence_raw_deleted,
            live.guild_rows_purged,
            live.deletions_completed,
            live.deletions_deferred,
        ),
        "dry run must project execute per class, got dry {dry:?} live {live:?}"
    );
    assert_eq!(
        dry.total_deleted(),
        live.total_deleted(),
        "dry-run total must not double-count staged deletions, got dry {dry:?} live {live:?}"
    );
    db.close().await.unwrap();
}

/// A guild purge is refused while a containment guard is unresolved: the dry
/// run and the live run both defer with rows untouched and the guild queued,
/// and the purge proceeds once the incident settles.
#[tokio::test]
async fn guild_purge_refused_while_containment_guard_unresolved() {
    let Some(db) = database("containment-guard purge refusal").await else {
        return;
    };
    let pool = db.pool();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(GUARD_GUILD)
        .bind(GUARD_MEMBER)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
         VALUES ($1, $2, 150, 150, now())",
    )
    .bind(GUARD_GUILD)
    .bind(GUARD_MEMBER)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
         VALUES ($1, to_char(now() - interval '1 day', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 10, NULL)",
    )
    .bind(GUARD_GUILD)
    .execute(pool)
    .await
    .unwrap();
    // Active containment incident for the departed guild: a replay fence the
    // purge must not wipe, or a retry could release the block.
    sqlx::query(
        "INSERT INTO containment_events
           (audit_entry_id, guild_id, executor_id, action, target_id, weight, occurred_at, state, reason, created_at)
         VALUES ('deferral-acceptance-evt', $1, $2, 'voice.kick', $2, 3, '2026-10-01T00:00:00Z', 'contain', 'fixture', '2026-10-01T00:00:00Z')",
    )
    .bind(GUARD_GUILD)
    .bind(GUARD_MEMBER)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO containment_incidents
           (id, guild_id, executor_id, trigger_audit_entry_id, heat, state, started_at)
         VALUES ('deferral-acceptance-inc', $1, $2, 'deferral-acceptance-evt', 3, 'containing', '2026-10-01T00:00:00Z')",
    )
    .bind(GUARD_GUILD)
    .bind(GUARD_MEMBER)
    .execute(pool)
    .await
    .unwrap();
    assert!(
        handle_guild_delete(pool, GUARD_GUILD, None).await.unwrap(),
        "a real removal enqueues a purge"
    );
    // Backdate the queue row so the deferred-age signal has a known floor.
    sqlx::query(
        "UPDATE retention_guild_purge_queue SET enqueued_at = clock_timestamp() - interval '2 hours' WHERE guild_id = $1",
    )
    .bind(GUARD_GUILD)
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

    // Pre-fix the purge deletes with no guard probe; post-fix it refuses
    // with rows untouched and the guild queued.
    let live: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        live.guild_rows_purged, 0,
        "a guarded purge must delete nothing, got {live:?}"
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
    for (table, sql) in [
        (
            "member_levels",
            "SELECT count(*) FROM member_levels WHERE guild_id = '161616161616161616'",
        ),
        (
            "presence_probe",
            "SELECT count(*) FROM presence_probe WHERE guild_id = '161616161616161616'",
        ),
        (
            "containment_incidents",
            "SELECT count(*) FROM containment_incidents WHERE guild_id = '161616161616161616'",
        ),
        (
            "purge queue",
            "SELECT count(*) FROM retention_guild_purge_queue WHERE guild_id = '161616161616161616'",
        ),
    ] {
        let rows: i64 = sqlx::query_scalar(sql).fetch_one(pool).await.unwrap();
        assert_eq!(rows, 1, "the guarded guild's {table} must survive the refused purge");
    }

    // Settle the incident through its own path (never by deleting the fence
    // row); the next run must purge the guild.
    sqlx::query(
        "UPDATE containment_incidents SET state = 'contained' WHERE id = 'deferral-acceptance-inc'",
    )
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
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM retention_guild_purge_queue WHERE guild_id = $1")
            .bind(GUARD_GUILD)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(queued, 0, "the settled guild must dequeue");
    db.close().await.unwrap();
}
