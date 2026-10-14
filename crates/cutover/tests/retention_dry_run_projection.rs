//! Dry-run guild-row projection regression (not acceptance).
//!
//! The queued-guild estimate is taken from one read-only snapshot, while an
//! Execute run deletes in stages: time-window rows first, then eligible
//! pending user deletions, then the guild purge. Rows an earlier stage
//! removes must not be counted again in `guild_rows_purged`, or the dry-run
//! total overstates what Execute will delete. Guard-deferred users are the
//! exception: their rows survive the user-deletion stage and are purged
//! with the guild, so they stay in the guild projection.

use two_bot_cutover::retention::{
    handle_guild_delete, request_user_deletion, run_retention, RetentionMode,
};
use two_bot_testsupport::TestDatabase;

const GONE_GUILD: &str = "333333333333333333";
const MEMBER: &str = "555555555555555555";
const OTHER_MEMBER: &str = "666666666666666666";
const GUARDED_MEMBER: &str = "777777777777777777";

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the retention dry-run projection suite"
            );
            eprintln!("SKIP retention dry-run projection: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .unwrap(),
    )
}

/// Overlapping staged deletions in one queued guild: expired raw rows, an
/// eligible pending user deletion, and a guard-deferred pending deletion.
/// Dry-run per-class counts must equal the following Execute run exactly.
#[tokio::test]
async fn dry_run_guild_projection_agrees_with_execute_when_stages_overlap() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // presence_probe: one expired (time-window stage) and one fresh
    // (guild stage) for the queued guild.
    sqlx::query(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
         VALUES
           ($1, to_char(now() - interval '31 days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 10, NULL),
           ($1, to_char(now() - interval '1 day', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 11, NULL)",
    )
    .bind(GONE_GUILD)
    .execute(pool)
    .await
    .unwrap();
    // events: expired voice rows for the deletion user and another member
    // (voice stage), a fresh voice row for the deletion user (user stage),
    // and a fresh non-voice row for the other member (guild stage).
    for (member, event_type, age_days, key) in [
        (MEMBER, "voice_session_started", 31, "proj-voice-old-user"),
        (
            OTHER_MEMBER,
            "voice_session_started",
            31,
            "proj-voice-old-other",
        ),
        (MEMBER, "voice_session_started", 1, "proj-voice-fresh-user"),
        (OTHER_MEMBER, "member_joined", 1, "proj-plain-fresh-other"),
    ] {
        sqlx::query(
            "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
             VALUES ($3, $2, $1, now() - make_interval(days => $4), 'fixture', '{}', $5)",
        )
        .bind(GONE_GUILD)
        .bind(member)
        .bind(event_type)
        .bind(age_days)
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    }
    // operational_audit_log: expired (audit stage), fresh for the deletion
    // user (user stage), fresh for the other member (guild stage).
    for (member, entry, age_days) in [
        (OTHER_MEMBER, "proj-audit-old", 46),
        (MEMBER, "proj-audit-fresh-user", 1),
        (OTHER_MEMBER, "proj-audit-fresh-other", 1),
    ] {
        sqlx::query(
            "INSERT INTO operational_audit_log
               (entry_id, event_kind, guild_id, occurred_at, actor_id, target_id, metadata_json, created_at)
             VALUES ($3, 'fixture', $1, now() - make_interval(days => $4), $2, $2, '{}', now())",
        )
        .bind(GONE_GUILD)
        .bind(member)
        .bind(entry)
        .bind(age_days)
        .execute(pool)
        .await
        .unwrap();
    }
    // Member footprint for the deletion user, another member, and the
    // guarded member. Only the deletion user's rows go in the user stage;
    // the other two members' rows go in the guild stage.
    for member in [MEMBER, OTHER_MEMBER, GUARDED_MEMBER] {
        sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
            .bind(GONE_GUILD)
            .bind(member)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
             VALUES ($1, $2, 10, 10, now())",
        )
        .bind(GONE_GUILD)
        .bind(member)
        .execute(pool)
        .await
        .unwrap();
    }
    // In-flight fence: the guarded member's request defers, leaving their
    // rows (and the fence itself) to the guild stage.
    sqlx::query(
        "INSERT INTO internal_idempotency
           (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id)
         VALUES (repeat('a', 64), repeat('b', 64), 'role.assign', repeat('c', 64), 'unknown', $1, $2, $2)",
    )
    .bind(GONE_GUILD)
    .bind(GUARDED_MEMBER)
    .execute(pool)
    .await
    .unwrap();
    request_user_deletion(pool, GONE_GUILD, MEMBER)
        .await
        .unwrap();
    request_user_deletion(pool, GONE_GUILD, GUARDED_MEMBER)
        .await
        .unwrap();
    assert!(handle_guild_delete(pool, GONE_GUILD, None).await.unwrap());

    let dry = run_retention(pool, RetentionMode::DryRun).await.unwrap();
    let live = run_retention(pool, RetentionMode::Execute).await.unwrap();
    // Execute ground truth: 1 expired presence row, 2 expired voice rows,
    // 1 expired audit row. The guild stage then sees only the fresh
    // presence row, the fresh non-voice event, the fresh other-member
    // audit row, the other + guarded members and levels, and the fence
    // (8 rows); the deletion user's rows already went in earlier stages.
    assert_eq!(
        (
            live.presence_raw_deleted,
            live.voice_raw_deleted,
            live.audit_cache_deleted,
            live.guild_rows_purged,
            live.deletions_completed,
            live.deletions_deferred,
        ),
        (1, 2, 1, 8, 1, 1),
        "execute must delete each row once across stages, got {live:?}"
    );
    assert_eq!(
        (
            dry.presence_raw_deleted,
            dry.voice_raw_deleted,
            dry.audit_cache_deleted,
            dry.guild_rows_purged,
            dry.deletions_completed,
            dry.deletions_deferred,
        ),
        (
            live.presence_raw_deleted,
            live.voice_raw_deleted,
            live.audit_cache_deleted,
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
