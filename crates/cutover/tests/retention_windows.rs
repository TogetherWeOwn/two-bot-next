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
    fulfill_user_deletion, handle_guild_delete, request_user_deletion, run_retention,
    DeletionReceipt, RetentionMode, RetentionReport,
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
