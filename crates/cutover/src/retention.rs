//! Rolling-window retention: raw roll-off, guild purge queue, deletion SLA.
//!
//! Thresholds come from the parent acceptance text (raw presence/voice rows
//! roll off at 30 days, audit-log cache at 45 days, user deletions complete
//! in under 72 hours). No threshold here is derived from any growth-analysis
//! report.
//!
//! ## Reuse boundary (adjacent consent-driven work, not re-implemented here)
//!
//! Per-member suppression/purge semantics belong to the [`crate::member_erasure`]
//! predicate set. This job reuses only its row identity: the user-deletion
//! path deletes the same `(guild_id, member_id)` row the erasure plan's
//! `members` entry names, and the time-driven deletes never touch the
//! member-scoped audit tables (`moderation_audit` and friends), which stay
//! under the per-member erasure path. Guild analytics toggles and opt-out
//! suppression live elsewhere and are not duplicated here.
//!
//! ## Coverage limits (do not silently claim more)
//!
//! * Voice-raw coverage is pinned to `events` rows with a voice-session
//!   `event_type`; no append-only gateway event-store tables exist on the
//!   current schema.
//! * Long-term aggregates (`member_levels` counts, `member_ranks` streaks)
//!   are never time-purged; a departed guild's aggregates go only through
//!   the guild-purge queue.
//! * Producer paths are NOT covered here: gateway ingestion cadence,
//!   backfill/replay writes, and the live-voice in-memory `remove_guild`
//!   handoff. The live gateway must invoke [`handle_guild_delete`] on real
//!   dispatches (passing the dispatch's `unavailable` flag through); wiring
//!   that call site and proving it against a live gateway is follow-up work.
//! * Retained control-plane records stay retained: `guild_settings`,
//!   `guild_settings_audit`, `audit_kill_switch`, `member_erasure_audit`,
//!   and operator-released `moderation_idempotency` replay-fence rows are
//!   never selected by any delete below.

use sqlx::PgPool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Raw presence/voice rows older than this roll off.
pub const PRESENCE_RAW_RETENTION_DAYS: i32 = 30;
/// Voice-session `events` rows older than this roll off.
pub const VOICE_RAW_RETENTION_DAYS: i32 = 30;
/// Audit-log cache rows older than this expire.
pub const AUDIT_CACHE_RETENTION_DAYS: i32 = 45;
/// User deletion requests complete well inside this many hours.
pub const DELETION_SLA_HOURS: i64 = 72;

/// How the run treats rows it finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionMode {
    /// Report per-class counts; delete nothing.
    DryRun,
    /// Delete, drain the guild-purge queue, complete pending deletions.
    Execute,
}

/// Per-class metrics for one retention run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionReport {
    /// Raw `presence_probe` rows deleted (older than 30 days).
    pub presence_raw_deleted: u64,
    /// Voice-session `events` rows deleted (older than 30 days).
    pub voice_raw_deleted: u64,
    /// `operational_audit_log` rows deleted (older than 45 days).
    pub audit_cache_deleted: u64,
    /// Guild-scoped rows purged for departed guilds.
    pub guild_rows_purged: u64,
    /// User-deletion requests completed by this run.
    pub deletions_completed: u64,
    /// Wall-clock run time.
    pub duration_ms: u64,
}

impl RetentionReport {
    /// Rows deleted across the time-window and guild-purge classes.
    /// Completed user deletions are receipts, not row counts, and are
    /// reported separately in `deletions_completed`.
    #[must_use]
    pub fn total_deleted(&self) -> u64 {
        self.presence_raw_deleted
            + self.voice_raw_deleted
            + self.audit_cache_deleted
            + self.guild_rows_purged
    }
}

/// Receipt for one honored user-deletion request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionReceipt {
    /// The request this receipt settles.
    pub request_id: String,
    /// The receipt artifact id.
    pub receipt_id: String,
    /// Request time, seconds since the Unix epoch.
    pub requested_at_unix: i64,
    /// Completion time, seconds since the Unix epoch.
    pub completed_at_unix: i64,
}

fn validate_snowflake(value: &str) -> Result<(), sqlx::Error> {
    if !crate::is_snowflake(value) || value.parse::<u64>().map_or(true, |id| id == 0) {
        return Err(sqlx::Error::InvalidArgument(
            "invalid retention identity".into(),
        ));
    }
    Ok(())
}

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(0);

fn next_request_id() -> String {
    let nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_nanos(),
        Err(_) => 0,
    };
    let seq = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    format!("del-{nanos:x}-{seq:x}")
}

/// Run the retention job: rolling-window deletes, guild-purge drain, and
/// pending user-deletion completion. Dry run reports counts and changes
/// nothing. Deletion and queue writes happen in ONE transaction with the
/// same lock/timeout discipline as `erase_member`.
pub async fn run_retention(
    pool: &PgPool,
    mode: RetentionMode,
) -> Result<RetentionReport, sqlx::Error> {
    let started = std::time::Instant::now();
    let mut tx = pool.begin().await?;
    let report = match mode {
        RetentionMode::DryRun => {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await?;
            let presence = count_presence_raw(&mut tx).await?;
            let voice = count_voice_raw(&mut tx).await?;
            let audit = count_audit_cache(&mut tx).await?;
            let guild_rows = count_queued_guild_rows(&mut tx).await?;
            let pending: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM retention_deletion_requests WHERE status = 'pending'",
            )
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            RetentionReport {
                presence_raw_deleted: presence,
                voice_raw_deleted: voice,
                audit_cache_deleted: audit,
                guild_rows_purged: guild_rows,
                deletions_completed: pending as u64,
                duration_ms: started.elapsed().as_millis() as u64,
            }
        }
        RetentionMode::Execute => {
            sqlx::query("SET LOCAL lock_timeout = '5s'")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SET LOCAL statement_timeout = '15s'")
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "LOCK TABLE events, member_levels, member_ranks, members, \
                 operational_audit_log, presence_probe, \
                 retention_deletion_requests, retention_guild_purge_queue \
                 IN SHARE ROW EXCLUSIVE MODE",
            )
            .execute(&mut *tx)
            .await?;
            let presence =
                sqlx::query("DELETE FROM presence_probe WHERE observed_at < to_char(now() - make_interval(days => $1), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')")
                    .bind(PRESENCE_RAW_RETENTION_DAYS)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            let voice = sqlx::query(
                "DELETE FROM events WHERE event_type IN ('voice_session_started', 'voice_session_ended') AND occurred_at < now() - make_interval(days => $1)",
            )
            .bind(VOICE_RAW_RETENTION_DAYS)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            let audit = sqlx::query(
                "DELETE FROM operational_audit_log WHERE occurred_at < now() - make_interval(days => $1)",
            )
            .bind(AUDIT_CACHE_RETENTION_DAYS)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            let guilds: Vec<String> =
                sqlx::query_scalar("SELECT guild_id FROM retention_guild_purge_queue")
                    .fetch_all(&mut *tx)
                    .await?;
            let mut guild_rows = 0u64;
            for guild in &guilds {
                guild_rows += purge_guild(&mut tx, guild).await?;
            }
            let pendings: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT request_id, guild_id, user_id FROM retention_deletion_requests WHERE status = 'pending'",
            )
            .fetch_all(&mut *tx)
            .await?;
            let mut completed = 0u64;
            for (request_id, guild_id, user_id) in &pendings {
                complete_request(&mut tx, request_id, guild_id, user_id).await?;
                completed += 1;
            }
            tx.commit().await?;
            RetentionReport {
                presence_raw_deleted: presence,
                voice_raw_deleted: voice,
                audit_cache_deleted: audit,
                guild_rows_purged: guild_rows,
                deletions_completed: completed,
                duration_ms: started.elapsed().as_millis() as u64,
            }
        }
    };
    Ok(report)
}

/// Record a guild removal. A dispatch with no `unavailable` flag (or an
/// explicit available flag) is a real removal and enqueues a guild-data
/// purge; an unavailable/outage payload never purges. Returns true when a
/// purge was enqueued.
pub async fn handle_guild_delete(
    pool: &PgPool,
    guild_id: &str,
    unavailable: Option<bool>,
) -> Result<bool, sqlx::Error> {
    validate_snowflake(guild_id)?;
    if unavailable == Some(true) {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO retention_guild_purge_queue (guild_id) VALUES ($1) ON CONFLICT (guild_id) DO NOTHING",
    )
    .bind(guild_id)
    .execute(pool)
    .await?;
    Ok(true)
}

/// Queue a user deletion request. Returns the request id.
pub async fn request_user_deletion(
    pool: &PgPool,
    guild_id: &str,
    user_id: &str,
) -> Result<String, sqlx::Error> {
    validate_snowflake(guild_id)?;
    validate_snowflake(user_id)?;
    let request_id = next_request_id();
    sqlx::query(
        "INSERT INTO retention_deletion_requests (request_id, guild_id, user_id) VALUES ($1, $2, $3)",
    )
    .bind(&request_id)
    .bind(guild_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(request_id)
}

/// Fulfil one queued deletion request and return its receipt. Completion is
/// immediate, so the under-72-hour SLA holds by construction.
pub async fn fulfill_user_deletion(
    pool: &PgPool,
    request_id: &str,
) -> Result<DeletionReceipt, sqlx::Error> {
    if request_id.trim().is_empty()
        || request_id.len() > 128
        || request_id.chars().any(|c| c.is_control())
    {
        return Err(sqlx::Error::InvalidArgument(
            "invalid deletion request id".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("LOCK TABLE members, retention_deletion_requests IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT guild_id, user_id FROM retention_deletion_requests WHERE request_id = $1 AND status = 'pending'",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((guild_id, user_id)) = row else {
        return Err(sqlx::Error::InvalidArgument(
            "unknown or settled deletion request".into(),
        ));
    };
    let (receipt_id, requested_at_unix, completed_at_unix) =
        complete_request(&mut tx, request_id, &guild_id, &user_id).await?;
    tx.commit().await?;
    Ok(DeletionReceipt {
        request_id: request_id.to_owned(),
        receipt_id,
        requested_at_unix,
        completed_at_unix,
    })
}

/// Delete the requesting member's row and settle the request, returning the
/// receipt fields. Shared by [`fulfill_user_deletion`] and the
/// [`run_retention`] pending drain so both paths honor the same receipt.
async fn complete_request(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request_id: &str,
    guild_id: &str,
    user_id: &str,
) -> Result<(String, i64, i64), sqlx::Error> {
    // Same (guild, member) identity as the erasure plan's `members` entry;
    // broader per-member tables stay on the erasure path.
    sqlx::query("DELETE FROM members WHERE guild_id = $1 AND member_id = $2")
        .bind(guild_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    let receipt_id = format!("rcpt-{request_id}");
    let row: Option<(i64, i64)> = sqlx::query_as(
        "UPDATE retention_deletion_requests SET status = 'completed', completed_at = clock_timestamp(), receipt_id = $2 \
         WHERE request_id = $1 AND status = 'pending' \
         RETURNING (EXTRACT(EPOCH FROM requested_at))::BIGINT, (EXTRACT(EPOCH FROM completed_at))::BIGINT",
    )
    .bind(request_id)
    .bind(&receipt_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((requested_at_unix, completed_at_unix)) = row else {
        return Err(sqlx::Error::InvalidArgument(
            "unknown or settled deletion request".into(),
        ));
    };
    Ok((receipt_id, requested_at_unix, completed_at_unix))
}

/// Purge one departed guild's rows and dequeue it, returning rows purged.
async fn purge_guild(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    guild_id: &str,
) -> Result<u64, sqlx::Error> {
    let mut purged = 0u64;
    purged += sqlx::query("DELETE FROM presence_probe WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    purged += sqlx::query("DELETE FROM events WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    purged += sqlx::query("DELETE FROM operational_audit_log WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    purged += sqlx::query("DELETE FROM member_levels WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    purged += sqlx::query("DELETE FROM member_ranks WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    purged += sqlx::query("DELETE FROM retention_deletion_requests WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    purged += sqlx::query("DELETE FROM members WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    sqlx::query("DELETE FROM retention_guild_purge_queue WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut **tx)
        .await?;
    Ok(purged)
}

async fn count_presence_raw(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, sqlx::Error> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM presence_probe WHERE observed_at < to_char(now() - make_interval(days => $1), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')")
        .bind(PRESENCE_RAW_RETENTION_DAYS)
        .fetch_one(&mut **tx)
        .await?;
    Ok(count as u64)
}

async fn count_voice_raw(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, sqlx::Error> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE event_type IN ('voice_session_started', 'voice_session_ended') AND occurred_at < now() - make_interval(days => $1)",
    )
    .bind(VOICE_RAW_RETENTION_DAYS)
    .fetch_one(&mut **tx)
    .await?;
    Ok(count as u64)
}

async fn count_audit_cache(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, sqlx::Error> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operational_audit_log WHERE occurred_at < now() - make_interval(days => $1)",
    )
    .bind(AUDIT_CACHE_RETENTION_DAYS)
    .fetch_one(&mut **tx)
    .await?;
    Ok(count as u64)
}

/// Rows the queued guild purges would delete, without deleting anything.
async fn count_queued_guild_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, sqlx::Error> {
    let guilds: Vec<String> =
        sqlx::query_scalar("SELECT guild_id FROM retention_guild_purge_queue")
            .fetch_all(&mut **tx)
            .await?;
    let mut total = 0u64;
    for guild in &guilds {
        for table in [
            "presence_probe",
            "events",
            "operational_audit_log",
            "member_levels",
            "member_ranks",
            "retention_deletion_requests",
            "members",
        ] {
            // Table names are a fixed list above, never caller input.
            let sql = format!("SELECT count(*) FROM {table} WHERE guild_id = $1");
            let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .bind(guild)
                .fetch_one(&mut **tx)
                .await?;
            total += count as u64;
        }
    }
    Ok(total)
}
