//! Retention rolling-window acceptance (failing first, tests only).
//!
//! Parent acceptance under test (exact thresholds, do not weaken):
//! 1. Raw presence/voice rows roll off at <= 30 days while aggregates
//!    (counts, streaks) remain long-term; audit-log cache entries expire at
//!    <= 45 days.
//! 2. An actual guild removal enqueues a guild-data purge; an outage/unavailable
//!    payload does NOT.
//! 3. User deletion requests complete in under 72 hours and produce a receipt.
//! 4. The retention job reports per-class metrics and supports a dry run that
//!    changes nothing.
//!
//! These tests are RED on the current head: there is no `retention` module yet
//! (repo grep shows no retention job, no rolling-window DELETE, no guild-purge
//! job beyond the in-memory voice-room `remove_guild`, and no deletion receipt).
//! The Builder sibling implements production code only and must NOT edit this
//! file; these tests pass unmodified once the contract below exists.
//!
//! ## Builder contract (implement exactly this surface in `two-bot-cutover`)
//!
//! ```rust,ignore
//! pub enum RetentionMode { DryRun, Execute }
//! pub struct RetentionReport {
//!     pub presence_raw_deleted: u64,
//!     pub voice_raw_deleted: u64,
//!     pub audit_cache_deleted: u64,
//!     pub guild_rows_purged: u64,
//!     pub deletions_completed: u64,
//!     pub duration_ms: u64,
//! }
//! impl RetentionReport { pub fn total_deleted(&self) -> u64 }
//! pub struct DeletionReceipt {
//!     pub request_id: String,
//!     pub receipt_id: String,
//!     pub requested_at_unix: i64,
//!     pub completed_at_unix: i64,
//! }
//! pub async fn run_retention(pool: &PgPool, mode: RetentionMode)
//!     -> Result<RetentionReport, impl std::error::Error>;
//! pub async fn handle_guild_delete(pool: &PgPool, guild_id: &str, unavailable: Option<bool>)
//!     -> Result<bool, impl std::error::Error>; // true = purge enqueued
//! pub async fn request_user_deletion(pool: &PgPool, guild_id: &str, user_id: &str)
//!     -> Result<String, impl std::error::Error>; // returns request id
//! pub async fn fulfill_user_deletion(pool: &PgPool, request_id: &str)
//!     -> Result<DeletionReceipt, impl std::error::Error>;
//! ```
//!
//! Table mapping on the current schema (latest migration tree under
//! `crates/cutover/migrations`):
//! * Presence raw = `presence_probe` rows (`observed_at` is ISO-8601 TEXT;
//!   lexicographic comparison is chronological for that format).
//! * Voice raw = `events` rows with a voice-session `event_type`
//!   (`voice_session_started` / `voice_session_ended`), keyed by `occurred_at`.
//! * Long-term aggregates = `member_levels` (counts) and `member_ranks`
//!   (streaks/ladders): never time-purged, however old.
//! * Audit-log cache = `operational_audit_log` rows keyed by `occurred_at`.
//!   Member-scoped audit tables (e.g. `moderation_audit`) stay under the
//!   per-member erasure path, not time-driven retention.
//! * `run_retention(Execute)` also drains the guild-purge queue filled by
//!   `handle_guild_delete` and completes pending user-deletion requests.
//!
//! ## Reuse boundary (adjacent consent-driven work, not re-implemented here)
//!
//! Per-member suppression/purge semantics belong to the `member_erasure`
//! predicate set; these tests reuse its row-identity intuition (two guilds x
//! two members, survivors must be byte-identical) but assert only removal,
//! timing, and receipts. Guild analytics toggles are out of scope.
//!
//! ## Pinned coverage limits (do not silently claim more)
//!
//! * The cited data-growth analysis report is absent from this environment, so
//!   no threshold here is derived from it; every day-count below comes from the
//!   parent acceptance text. The public audit-log reference is
//!   <https://docs.discord.com/developers/resources/audit-log> (re-verify its
//!   current text before quoting service-side day counts anywhere).
//! * No append-only gateway event-store tables exist on the current schema, so
//!   voice-raw coverage is pinned to `events` as seeded here.
//! * Producer paths are NOT covered: gateway ingestion cadence, backfill/replay
//!   writes, the live-voice in-memory `remove_guild` handoff, and the gateway
//!   call site that must invoke `handle_guild_delete` on real dispatches. The
//!   Builder wires the call site; live-gateway proof is follow-up work.
//! * Fixed fake snowflakes only. Disposable test databases via
//!   `two-bot-testsupport`; never staging or production bindings, never real
//!   user data, no secrets in code or comments.

use sqlx::PgPool;
use two_bot_cutover::retention::{
    cancel_guild_purge, fulfill_user_deletion, handle_guild_delete, request_user_deletion,
    run_retention, DeletionReceipt, RetentionMode, RetentionReport,
};
use two_bot_testsupport::TestDatabase;

const GUILD_A: &str = "111111111111111111";
const GUILD_B: &str = "222222222222222222";
const GONE_GUILD: &str = "333333333333333333";
const MEMBER: &str = "444444444444444444";
const OTHER_MEMBER: &str = "555555555555555555";

const VOICE_STARTED: &str = "voice_session_started";
const VOICE_ENDED: &str = "voice_session_ended";

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

/// Presence raw rows older than 30 days roll off; rows inside the window stay.
#[tokio::test]
async fn presence_raw_older_than_30d_purges_while_fresh_rows_survive() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
         VALUES
           ($1, to_char(now() - interval '31 days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 10, NULL),
           ($1, to_char(now() - interval '1 day', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 12, NULL)",
    )
    .bind(GUILD_A)
    .execute(pool)
    .await
    .unwrap();

    let report: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        report.presence_raw_deleted, 1,
        "exactly the 31-day presence row must go"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM presence_probe WHERE observed_at >
             to_char(now() - interval '30 days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
        )
        .await,
        1,
        "the 1-day presence row must survive"
    );
    assert_eq!(
        count(pool, "SELECT count(*) FROM presence_probe").await,
        1,
        "no other presence row may be touched"
    );
    db.close().await.unwrap();
}

/// Voice raw rows older than 30 days roll off; rows inside the window stay.
#[tokio::test]
async fn voice_raw_older_than_30d_purges_while_fresh_rows_survive() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
         VALUES
           ($2, $3, $1, now() - interval '31 days', 'fixture', '{}', 'ret-voice-old-start'),
           ($4, $3, $1, now() - interval '31 days', 'fixture', '{}', 'ret-voice-old-end'),
           ($2, $3, $1, now() - interval '1 day', 'fixture', '{}', 'ret-voice-fresh-start'),
           ($4, $3, $1, now() - interval '1 day', 'fixture', '{}', 'ret-voice-fresh-end')",
    )
    .bind(GUILD_A)
    .bind(VOICE_STARTED)
    .bind(MEMBER)
    .bind(VOICE_ENDED)
    .execute(pool)
    .await
    .unwrap();

    let report: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        report.voice_raw_deleted, 2,
        "both 31-day voice rows must go"
    );
    let survivors: Vec<String> =
        sqlx::query_scalar("SELECT idempotency_key FROM events ORDER BY idempotency_key")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(
        survivors,
        vec![
            "ret-voice-fresh-end".to_owned(),
            "ret-voice-fresh-start".to_owned()
        ],
        "only the fresh voice rows must survive"
    );
    db.close().await.unwrap();
}

/// Long-term aggregates (counts, streaks) survive the raw purge however old.
#[tokio::test]
async fn long_term_aggregates_survive_the_raw_purge() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
         VALUES ($1, $2, 150, 150, now() - interval '60 days')",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO member_ranks (guild_id, member_id, updated_at)
         VALUES ($1, $2, now() - interval '60 days')",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();

    let report: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(report.presence_raw_deleted, 0);
    assert_eq!(report.voice_raw_deleted, 0);
    let (xp, message_xp): (i64, i64) = sqlx::query_as(
        "SELECT xp, message_xp FROM member_levels WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        (xp, message_xp),
        (150, 150),
        "aggregate counts must be kept"
    );
    assert_eq!(
        count(pool, "SELECT count(*) FROM member_ranks").await,
        1,
        "rank/streak rows must be kept"
    );
    db.close().await.unwrap();
}

/// Audit-log cache rows older than 45 days expire; rows inside the window stay.
#[tokio::test]
async fn audit_cache_older_than_45d_purges_while_fresh_rows_survive() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO operational_audit_log
           (entry_id, event_kind, guild_id, occurred_at, actor_id, target_id, metadata_json, created_at)
         VALUES
           ('ret-audit-old', 'fixture', $1, now() - interval '46 days', $2, $2, '{}', now()),
           ('ret-audit-fresh', 'fixture', $1, now() - interval '1 day', $2, $2, '{}', now())",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();

    let report: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        report.audit_cache_deleted, 1,
        "exactly the 46-day audit-cache row must go"
    );
    let survivors: Vec<String> =
        sqlx::query_scalar("SELECT entry_id FROM operational_audit_log ORDER BY entry_id")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(
        survivors,
        vec!["ret-audit-fresh".to_owned()],
        "only the fresh audit-cache row must survive"
    );
    db.close().await.unwrap();
}

/// A real guild removal enqueues a purge (drained by the next run); an
/// outage/unavailable payload must NOT enqueue anything.
#[tokio::test]
async fn real_guild_delete_enqueues_purge_but_outage_payload_does_not() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    for guild in [GONE_GUILD, GUILD_B] {
        sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
            .bind(guild)
            .bind(MEMBER)
            .execute(pool)
            .await
            .unwrap();
    }

    assert!(
        handle_guild_delete(pool, GONE_GUILD, None).await.unwrap(),
        "a remove without an unavailable flag is a real removal"
    );
    assert!(
        handle_guild_delete(pool, GONE_GUILD, Some(false))
            .await
            .unwrap(),
        "an explicit available flag is a real removal"
    );
    assert!(
        !handle_guild_delete(pool, GUILD_B, Some(true))
            .await
            .unwrap(),
        "an unavailable/outage payload must not enqueue a purge"
    );

    let report: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert!(
        report.guild_rows_purged > 0,
        "the enqueued guild purge must delete rows, got {report:?}"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM members WHERE guild_id = '333333333333333333'"
        )
        .await,
        0,
        "the removed guild's rows must be purged"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM members WHERE guild_id = '222222222222222222'"
        )
        .await,
        1,
        "the outage-flagged guild's rows must survive"
    );
    db.close().await.unwrap();
}

/// A rejoin cancels the queued purge: the guild's new and long-term rows survive
/// the next run.
#[tokio::test]
async fn cancelled_guild_purge_keeps_guild_rows() {
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

    assert!(
        handle_guild_delete(pool, GONE_GUILD, None).await.unwrap(),
        "a real removal enqueues a purge"
    );
    assert!(
        cancel_guild_purge(pool, GONE_GUILD).await.unwrap(),
        "a rejoin cancels the queued purge"
    );
    assert!(
        !cancel_guild_purge(pool, GONE_GUILD).await.unwrap(),
        "a second cancel removes nothing"
    );

    let report: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        report.guild_rows_purged, 0,
        "a cancelled purge must delete nothing, got {report:?}"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM members WHERE guild_id = '333333333333333333'"
        )
        .await,
        1,
        "the rejoined guild's rows must survive"
    );
    assert_eq!(
        count(pool, "SELECT count(*) FROM member_levels").await,
        1,
        "the rejoined guild's aggregates must survive"
    );
    db.close().await.unwrap();
}

/// A user deletion request completes in under 72 hours and yields a receipt.
#[tokio::test]
async fn user_deletion_request_completes_within_72h_and_yields_receipt() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    for (guild, member) in [
        (GUILD_A, MEMBER),
        (GUILD_A, OTHER_MEMBER),
        (GUILD_B, MEMBER),
    ] {
        sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
            .bind(guild)
            .bind(member)
            .execute(pool)
            .await
            .unwrap();
    }
    // Full-footprint fixture: the receipt must cover more than the members
    // row (member_levels counts and member events for the requesting user).
    for (guild, member, xp) in [
        (GUILD_A, MEMBER, 150),
        (GUILD_A, OTHER_MEMBER, 250),
        (GUILD_B, MEMBER, 350),
    ] {
        sqlx::query(
            "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
             VALUES ($1, $2, $3, $3, now())",
        )
        .bind(guild)
        .bind(member)
        .bind(xp)
        .execute(pool)
        .await
        .unwrap();
    }
    for (guild, member, key) in [
        (GUILD_A, MEMBER, "ret-del-own"),
        (GUILD_A, OTHER_MEMBER, "ret-del-other"),
        (GUILD_B, MEMBER, "ret-del-elsewhere"),
    ] {
        sqlx::query(
            "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
             VALUES ('voice_session_started', $2, $1, now(), 'fixture', '{}', $3)",
        )
        .bind(guild)
        .bind(member)
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    }

    let request_id: String = request_user_deletion(pool, GUILD_A, MEMBER).await.unwrap();
    assert!(!request_id.is_empty(), "the request must have an id");
    let receipt: DeletionReceipt = fulfill_user_deletion(pool, &request_id).await.unwrap();
    assert_eq!(receipt.request_id, request_id);
    assert!(
        !receipt.receipt_id.is_empty(),
        "fulfilment must produce a receipt artifact id"
    );
    assert!(
        receipt.completed_at_unix >= receipt.requested_at_unix,
        "completion cannot precede the request"
    );
    assert!(
        receipt.completed_at_unix - receipt.requested_at_unix < 72 * 3600,
        "the deletion SLA is under 72 hours"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM members WHERE guild_id = '111111111111111111'"
        )
        .await,
        1,
        "only the requesting member's row goes; the other member survives"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM members WHERE guild_id = '222222222222222222'"
        )
        .await,
        1,
        "the other guild's rows survive"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM member_levels WHERE guild_id = '111111111111111111' AND member_id = '444444444444444444'"
        )
        .await,
        0,
        "the requesting user's aggregate counts must go with the receipt"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM member_levels WHERE guild_id = '111111111111111111' AND member_id = '555555555555555555'"
        )
        .await,
        1,
        "the other member's aggregates survive"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM events WHERE guild_id = '111111111111111111' AND member_id = '444444444444444444'"
        )
        .await,
        0,
        "the requesting user's events must go with the receipt"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM events WHERE guild_id = '111111111111111111' AND member_id = '555555555555555555'"
        )
        .await,
        1,
        "the other member's events survive"
    );
    db.close().await.unwrap();
}

/// The run reports per-class metrics; a dry run reports but deletes nothing.
#[tokio::test]
async fn retention_run_reports_per_class_metrics_and_dry_run_changes_nothing() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
         VALUES
           ($1, to_char(now() - interval '31 days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 10, NULL),
           ($1, to_char(now() - interval '32 days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 11, NULL),
           ($1, to_char(now() - interval '1 day', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 12, NULL)",
    )
    .bind(GUILD_A)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
         VALUES
           ($2, $3, $1, now() - interval '31 days', 'fixture', '{}', 'ret-metric-old'),
           ($2, $3, $1, now() - interval '1 day', 'fixture', '{}', 'ret-metric-fresh')",
    )
    .bind(GUILD_A)
    .bind(VOICE_STARTED)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO operational_audit_log
           (entry_id, event_kind, guild_id, occurred_at, actor_id, target_id, metadata_json, created_at)
         VALUES ('ret-metric-audit-old', 'fixture', $1, now() - interval '46 days', $2, $2, '{}', now())",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();

    let dry: RetentionReport = run_retention(pool, RetentionMode::DryRun).await.unwrap();
    assert_eq!(dry.presence_raw_deleted, 2, "dry run must report {dry:?}");
    assert_eq!(dry.voice_raw_deleted, 1, "dry run must report {dry:?}");
    assert_eq!(dry.audit_cache_deleted, 1, "dry run must report {dry:?}");
    assert_eq!(
        dry.total_deleted(),
        dry.presence_raw_deleted
            + dry.voice_raw_deleted
            + dry.audit_cache_deleted
            + dry.guild_rows_purged,
        "the total must equal the per-class counts"
    );
    assert_eq!(
        count(pool, "SELECT count(*) FROM presence_probe").await,
        3,
        "dry run must delete nothing"
    );
    assert_eq!(
        count(pool, "SELECT count(*) FROM events").await,
        2,
        "dry run must delete nothing"
    );
    assert_eq!(
        count(pool, "SELECT count(*) FROM operational_audit_log").await,
        1,
        "dry run must delete nothing"
    );

    let live: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        (
            live.presence_raw_deleted,
            live.voice_raw_deleted,
            live.audit_cache_deleted
        ),
        (2, 1, 1),
        "the live run must delete exactly what the dry run reported"
    );
    assert_eq!(
        count(pool, "SELECT count(*) FROM presence_probe").await,
        1,
        "only the fresh presence row survives the live run"
    );
    db.close().await.unwrap();
}

/// Retention is idempotent: the second run after a full pass is a no-op.
#[tokio::test]
async fn second_retention_run_is_a_noop() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
         VALUES ($1, to_char(now() - interval '31 days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 10, NULL)",
    )
    .bind(GUILD_A)
    .execute(pool)
    .await
    .unwrap();

    let first: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(first.presence_raw_deleted, 1);
    let second: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        second.total_deleted(),
        0,
        "the second run must delete nothing, got {second:?}"
    );
    assert_eq!(second.deletions_completed, 0);
    assert_eq!(second.guild_rows_purged, 0);
    db.close().await.unwrap();
}

/// A user deletion with an unresolved replay guard stays pending with no
/// receipt: the rows and the request survive until the guard settles.
#[tokio::test]
async fn user_deletion_with_unresolved_guard_stays_pending_without_receipt() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    for (guild, member) in [(GUILD_A, MEMBER), (GUILD_A, OTHER_MEMBER)] {
        sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
            .bind(guild)
            .bind(member)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
             VALUES ($1, $2, 150, 150, now())",
        )
        .bind(guild)
        .bind(member)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
             VALUES ('voice_session_started', $2, $1, now(), 'fixture', '{}', $3)",
        )
        .bind(guild)
        .bind(member)
        .bind(format!("ret-guard-{member}"))
        .execute(pool)
        .await
        .unwrap();
    }
    // In-flight replay fence for the requesting user only.
    sqlx::query(
        "INSERT INTO internal_idempotency
           (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id)
         VALUES (repeat('a', 64), repeat('b', 64), 'role.assign', repeat('c', 64), 'unknown', $1, $2, $2)",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    // Active anti-nuke containment for the same user.
    sqlx::query(
        "INSERT INTO containment_events
           (audit_entry_id, guild_id, executor_id, action, target_id, weight, occurred_at, state, reason, created_at)
         VALUES ('ret-guard-evt-1', $1, $2, 'ban', $2, 1, '2026-10-01T00:00:00Z', 'contain', 'fixture', '2026-10-01T00:00:00Z')",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO containment_incidents
           (id, guild_id, executor_id, trigger_audit_entry_id, heat, state, result_json, started_at)
         VALUES ('ret-guard-inc-1', $1, $2, 'ret-guard-evt-1', 3, 'containing', '{}', '2026-10-01T00:00:00Z')",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();

    let request_id = request_user_deletion(pool, GUILD_A, MEMBER).await.unwrap();
    let err = fulfill_user_deletion(pool, &request_id).await.unwrap_err();
    assert!(
        format!("{err}").contains("unresolved erasure replay guard")
            || format!("{err:?}").contains("unresolved erasure replay guard"),
        "fulfilment must refuse on the guard, got {err:?}"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM member_levels WHERE guild_id = '111111111111111111' AND member_id = '444444444444444444'"
        )
        .await,
        1,
        "the guarded user's rows must survive the refusal"
    );
    assert_eq!(
        count(pool, "SELECT count(*) FROM internal_idempotency").await,
        1,
        "the in-flight fence must survive"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE request_id = $1 AND status = 'pending'",
    )
    .bind(&request_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(pending, 1, "the refused request must stay pending");
    let receipted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE request_id = $1 AND status = 'completed'",
    )
    .bind(&request_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(receipted, 0, "no receipt may be issued while guarded");

    // The scheduled drain must also skip the guarded request, not roll back
    // the run: no deletion completes, user rows survive.
    let report: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        report.deletions_completed, 0,
        "a guarded request must not count as completed, got {report:?}"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM member_levels WHERE guild_id = '111111111111111111' AND member_id = '444444444444444444'"
        )
        .await,
        1,
        "the guarded user's rows must survive the scheduled run"
    );

    // Settle both fences; fulfilment must then succeed with a receipt.
    sqlx::query(
        "UPDATE internal_idempotency SET state = 'completed', response_code = 'success',
         http_status = 200, resource_id = actor_id, affected = 1 WHERE guild_id = $1 AND actor_id = $2",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE containment_incidents SET state = 'contained', completed_at = '2026-10-01T00:00:01Z'
         WHERE guild_id = $1 AND executor_id = $2",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    let receipt: DeletionReceipt = fulfill_user_deletion(pool, &request_id).await.unwrap();
    assert_eq!(receipt.request_id, request_id);
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM member_levels WHERE guild_id = '111111111111111111' AND member_id = '444444444444444444'"
        )
        .await,
        0,
        "the settled user's rows must go with the receipt"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM member_levels WHERE guild_id = '111111111111111111' AND member_id = '555555555555555555'"
        )
        .await,
        1,
        "the other member's rows must survive"
    );
    db.close().await.unwrap();
}

/// A departed guild's purge covers every plan table (including `event_store`)
/// while the other guild is untouched, and a pending deletion for the
/// departed guild completes with a receipt instead of vanishing.
#[tokio::test]
async fn guild_purge_covers_every_plan_table_and_preserves_receipts() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::raw_sql(include_str!("fixtures/member_erasure.sql"))
        .execute(pool)
        .await
        .unwrap();
    // Clear the fixture's auto-seeded ledger so the pending drain is exactly
    // the one request below; otherwise the drain would complete four requests
    // before the purge runs.
    sqlx::query("DELETE FROM retention_deletion_requests")
        .execute(pool)
        .await
        .unwrap();
    const FIX_GONE: &str = "1545644954272137297";
    const FIX_OTHER: &str = "1545644954272137298";
    const FIX_USER: &str = "123456789012345678";
    const FIX_USER_2: &str = "123456789012345679";
    // Every shared-fixture capture must start inside both raw-event TTLs,
    // with its receipt timestamp and timestamp-bearing replay key aligned.
    // Dedicated raw-expiry tests seed their own deliberately old captures.
    for guild in [FIX_GONE, FIX_OTHER] {
        let fresh_captures: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM event_store WHERE guild_id = $1
             AND event_time > now() + interval '15 minutes' - interval '30 days'
             AND event_time <= now() AND received_at = event_time
             AND right(dedup_key, 20) = to_char(event_time AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
        )
        .bind(guild)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(
            fresh_captures, 10,
            "guild {guild} must start with ten fresh captures and matching receipt times/replay keys"
        );
        sqlx::query(
            "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
             VALUES ($1, to_char(now() - interval '1 day', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 10, NULL)",
        )
        .bind(guild)
        .execute(pool)
        .await
        .unwrap();
    }
    // Snapshot the surviving guild's per-table counts before the purge.
    let plan = two_bot_cutover::member_erasure::plan();
    let mut other_before: Vec<(String, i64)> = Vec::new();
    for entry in &plan.tables {
        let guild_count: i64 = if entry.table == "lfg_signups" || entry.table == "lfg_roles" {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {} WHERE lfg_id IN (SELECT id FROM lfg_posts WHERE guild_id = $1)",
                entry.table
            )))
            .bind(FIX_OTHER)
            .fetch_one(pool)
            .await
            .unwrap()
        } else if entry.table == "feed_deliveries" {
            sqlx::query_scalar(sqlx::AssertSqlSafe(
                "SELECT count(*) FROM feed_deliveries WHERE feed_id IN (SELECT id FROM feed_relays WHERE guild_id = $1)".to_owned(),
            ))
            .bind(FIX_OTHER)
            .fetch_one(pool)
            .await
            .unwrap()
        } else if entry.table == "self_role_exchanges" {
            sqlx::query_scalar(sqlx::AssertSqlSafe(
                "SELECT count(*) FROM self_role_exchanges WHERE event_id IN (SELECT event_id FROM self_role_audit WHERE guild_id = $1)".to_owned(),
            ))
            .bind(FIX_OTHER)
            .fetch_one(pool)
            .await
            .unwrap()
        } else if entry.table == "starboard_starrers" {
            // BIGINT snowflake column against a decimal TEXT bind: cast the
            // bind, never the indexed column (same as the erasure predicate).
            sqlx::query_scalar(sqlx::AssertSqlSafe(
                "SELECT count(*) FROM starboard_starrers WHERE guild_id = $1::bigint".to_owned(),
            ))
            .bind(FIX_OTHER)
            .fetch_one(pool)
            .await
            .unwrap()
        } else if entry.table == "event_voice_presence" {
            // Direct by seeded event id, not through the mirror join: an
            // orphaned span (its event deleted from the mirror) must still
            // be counted, or the purge looks complete while rows leak.
            sqlx::query_scalar(
                "SELECT count(*) FROM event_voice_presence WHERE event_id IN ($1, $2)",
            )
            .bind(format!("ev-{FIX_OTHER}:{FIX_USER}"))
            .bind(format!("ev-{FIX_OTHER}:{FIX_USER_2}"))
            .fetch_one(pool)
            .await
            .unwrap()
        } else if entry.table == "retention_deletion_requests" {
            0
        } else {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {} WHERE guild_id = $1",
                entry.table
            )))
            .bind(FIX_OTHER)
            .fetch_one(pool)
            .await
            .unwrap()
        };
        other_before.push((entry.table.clone(), guild_count));
    }

    let request_id = request_user_deletion(pool, FIX_GONE, FIX_USER)
        .await
        .unwrap();
    assert!(
        handle_guild_delete(pool, FIX_GONE, None).await.unwrap(),
        "a real removal enqueues a purge"
    );
    let report: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        report.deletions_completed, 1,
        "the departed guild's pending deletion must complete, not vanish, got {report:?}"
    );
    assert!(
        report.guild_rows_purged > 0,
        "the purge must delete rows, got {report:?}"
    );
    let receipted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE request_id = $1 AND status = 'completed'",
    )
    .bind(&request_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        receipted, 1,
        "the pending request for the departed guild must have a receipt"
    );
    // Every plan table except the ledger must hold zero rows for the gone guild.
    for entry in &plan.tables {
        if entry.table == "retention_deletion_requests" {
            continue;
        }
        let gone_count: i64 = if entry.table == "lfg_signups" || entry.table == "lfg_roles" {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {} WHERE lfg_id IN (SELECT id FROM lfg_posts WHERE guild_id = $1)",
                entry.table
            )))
            .bind(FIX_GONE)
            .fetch_one(pool)
            .await
            .unwrap()
        } else if entry.table == "feed_deliveries" {
            sqlx::query_scalar(sqlx::AssertSqlSafe(
                "SELECT count(*) FROM feed_deliveries WHERE feed_id IN (SELECT id FROM feed_relays WHERE guild_id = $1)".to_owned(),
            ))
            .bind(FIX_GONE)
            .fetch_one(pool)
            .await
            .unwrap()
        } else if entry.table == "self_role_exchanges" {
            sqlx::query_scalar(sqlx::AssertSqlSafe(
                "SELECT count(*) FROM self_role_exchanges WHERE event_id IN (SELECT event_id FROM self_role_audit WHERE guild_id = $1)".to_owned(),
            ))
            .bind(FIX_GONE)
            .fetch_one(pool)
            .await
            .unwrap()
        } else if entry.table == "starboard_starrers" {
            // BIGINT snowflake column against a decimal TEXT bind: cast the
            // bind, never the indexed column (same as the erasure predicate).
            sqlx::query_scalar(sqlx::AssertSqlSafe(
                "SELECT count(*) FROM starboard_starrers WHERE guild_id = $1::bigint".to_owned(),
            ))
            .bind(FIX_GONE)
            .fetch_one(pool)
            .await
            .unwrap()
        } else if entry.table == "event_voice_presence" {
            // Direct by seeded event id, not through the mirror join: an
            // orphaned span (its event deleted from the mirror) must still
            // be counted, or the purge looks complete while rows leak.
            sqlx::query_scalar(
                "SELECT count(*) FROM event_voice_presence WHERE event_id IN ($1, $2)",
            )
            .bind(format!("ev-{FIX_GONE}:{FIX_USER}"))
            .bind(format!("ev-{FIX_GONE}:{FIX_USER_2}"))
            .fetch_one(pool)
            .await
            .unwrap()
        } else {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {} WHERE guild_id = $1",
                entry.table
            )))
            .bind(FIX_GONE)
            .fetch_one(pool)
            .await
            .unwrap()
        };
        assert_eq!(
            gone_count, 0,
            "purged guild must leave zero rows in {}",
            entry.table
        );
    }
    let event_store_gone: i64 =
        sqlx::query_scalar("SELECT count(*) FROM event_store WHERE guild_id = $1")
            .bind(FIX_GONE)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        event_store_gone, 0,
        "the purged guild's event_store rows must go"
    );
    let event_store_other: i64 =
        sqlx::query_scalar("SELECT count(*) FROM event_store WHERE guild_id = $1")
            .bind(FIX_OTHER)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        event_store_other, 10,
        "the surviving guild's event_store rows must stay (2 members x 5 rows: 1 presence + 3 S8 + 1 S7 invite)"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM presence_probe WHERE guild_id = '1545644954272137297'"
        )
        .await,
        0,
        "the purged guild's probe rows must go"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM presence_probe WHERE guild_id = '1545644954272137298'"
        )
        .await,
        1,
        "the surviving guild's probe rows must stay"
    );
    // The surviving guild's plan-table rows must be byte-identical in count.
    for (table, before) in &other_before {
        if table == "retention_deletion_requests" {
            continue;
        }
        let after: i64 = if table == "lfg_signups" || table == "lfg_roles" {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {table} WHERE lfg_id IN (SELECT id FROM lfg_posts WHERE guild_id = $1)"
            )))
            .bind(FIX_OTHER)
            .fetch_one(pool)
            .await
            .unwrap()
        } else if table == "feed_deliveries" {
            sqlx::query_scalar(sqlx::AssertSqlSafe(
                "SELECT count(*) FROM feed_deliveries WHERE feed_id IN (SELECT id FROM feed_relays WHERE guild_id = $1)".to_owned(),
            ))
            .bind(FIX_OTHER)
            .fetch_one(pool)
            .await
            .unwrap()
        } else if table == "self_role_exchanges" {
            sqlx::query_scalar(sqlx::AssertSqlSafe(
                "SELECT count(*) FROM self_role_exchanges WHERE event_id IN (SELECT event_id FROM self_role_audit WHERE guild_id = $1)".to_owned(),
            ))
            .bind(FIX_OTHER)
            .fetch_one(pool)
            .await
            .unwrap()
        } else if table == "starboard_starrers" {
            // BIGINT snowflake column against a decimal TEXT bind: cast the
            // bind, never the indexed column (same as the erasure predicate).
            sqlx::query_scalar(sqlx::AssertSqlSafe(
                "SELECT count(*) FROM starboard_starrers WHERE guild_id = $1::bigint".to_owned(),
            ))
            .bind(FIX_OTHER)
            .fetch_one(pool)
            .await
            .unwrap()
        } else if table == "event_voice_presence" {
            // Direct by seeded event id, not through the mirror join: an
            // orphaned span (its event deleted from the mirror) must still
            // be counted, or the purge looks complete while rows leak.
            sqlx::query_scalar(
                "SELECT count(*) FROM event_voice_presence WHERE event_id IN ($1, $2)",
            )
            .bind(format!("ev-{FIX_OTHER}:{FIX_USER}"))
            .bind(format!("ev-{FIX_OTHER}:{FIX_USER_2}"))
            .fetch_one(pool)
            .await
            .unwrap()
        } else {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {table} WHERE guild_id = $1"
            )))
            .bind(FIX_OTHER)
            .fetch_one(pool)
            .await
            .unwrap()
        };
        assert_eq!(
            after, *before,
            "surviving guild's {table} must be untouched"
        );
    }
    db.close().await.unwrap();
}
