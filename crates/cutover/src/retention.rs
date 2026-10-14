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
//! predicate set. The user-deletion path reuses its full footprint AND its
//! safety gates: it shares the schema-coverage check and the unresolved
//! replay-guard refusal with `erase_member`, so an in-flight idempotency
//! intent, unsettled self-role/automod claim, or active containment leaves
//! the request pending with no receipt. Every pending request drains in its
//! own savepoint, so one refusal never rolls back the time-window deletes.
//! The user-deletion deletes run in the same transaction that settles the
//! receipt, excluding only the in-flight `retention_deletion_requests` row
//! itself. The time-driven deletes never touch the member-scoped audit tables
//! (`moderation_audit` and friends) beyond that user-deletion path, which
//! stays under the erasure predicates. Guild analytics toggles and opt-out
//! suppression live elsewhere and are not duplicated here.
//!
//! ## Coverage limits (do not silently claim more)
//!
//! * Voice-raw coverage is pinned to `events` rows with a voice-session
//!   `event_type`; the append-only `event_store` capture (0428/0432) persists
//!   with no age-based TTL and is removed per-user via the erasure plan and
//!   per-guild via the guild purge below.
//! * Guild purges cover every guild-keyed erasure-plan table (plus
//!   `presence_probe`) and leave `retention_deletion_requests` to the receipt
//!   path, so a pending deletion for a departed guild still completes with a
//!   receipt instead of vanishing.
//! * Long-term aggregates (`member_levels` counts, `member_ranks` streaks)
//!   are never time-purged; a departed guild's aggregates go only through
//!   the guild-purge queue, and a rejoin cancels the queued purge via
//!   [`cancel_guild_purge`] before the next run.
//! * Producer paths: the live gateway invokes [`handle_guild_delete`] on
//!   real `GUILD_DELETE` dispatches (passing the dispatch's `unavailable`
//!   flag through, so outage payloads never queue) and
//!   [`cancel_guild_purge`] on available `GUILD_CREATE`/rejoin, ahead of the
//!   dispatch checkpoint so a failed write replays after restart. The
//!   supervised retention job (bounded cadence, dry run unless explicitly
//!   configured to execute) schedules [`run_retention`]. Gateway ingestion
//!   cadence, backfill/replay writes, and the live-voice in-memory
//!   `remove_guild` handoff remain owned by their own paths, not duplicated
//!   here.
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
    /// Queued guild purges that failed in this run, leaving the guild queued.
    /// A nonzero value with an `Ok` return is the signal that a departed
    /// guild's rows survived the run (see the warn log for the guild id).
    pub guild_purges_failed: u64,
    /// User-deletion requests completed by this run.
    pub deletions_completed: u64,
    /// Pending deletion requests refused by a replay guard or by schema
    /// coverage in this run, staying pending with no receipt. A stuck fence
    /// or an unplanned identity column can keep a request here beyond the
    /// deletion SLA; see `deletions_overdue`.
    pub deletions_deferred: u64,
    /// Pending deletion requests older than [`DELETION_SLA_HOURS`], whether
    /// deferred or not. A nonzero value is the signal that the under-72-hour
    /// wording does not hold for every request.
    pub deletions_overdue: u64,
    /// Wall-clock run time.
    pub duration_ms: u64,
}

impl RetentionReport {
    /// Rows deleted across the time-window and guild-purge classes.
    /// Completed user deletions are receipts, not row counts, and are
    /// reported separately in `deletions_completed`; deferred and overdue
    /// requests are pending-work signals, not deletions, and are excluded,
    /// as are failed guild purges (`guild_purges_failed`).
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

/// Run the retention job: rolling-window deletes, pending user-deletion
/// completion, then guild-purge drain. Dry run reports counts and changes
/// nothing. Deletion and queue writes happen in ONE transaction with the
/// same lock/timeout discipline as `erase_member`; each pending deletion
/// runs in its own savepoint so one guard or schema-coverage refusal leaves
/// that request pending without rolling back the time-window deletes, and
/// each guild purge runs in its own savepoint so one bad guild stays queued
/// without rolling back the rest of the run. Pending deletions drain before
/// guild purges so a departed guild's pending receipts complete against
/// pre-purge rows, and purges leave the deletion ledger to the receipt path.
/// Guard- or schema-refused requests are counted in `deletions_deferred`
/// (and logged at warn level) and SLA-breaching pendings in
/// `deletions_overdue`, in both modes.
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
            let overdue = count_overdue_deletions(&mut tx).await?;
            let pendings: Vec<(String, String)> = sqlx::query_as(
                "SELECT guild_id, user_id FROM retention_deletion_requests WHERE status = 'pending'",
            )
            .fetch_all(&mut *tx)
            .await?;
            // A schema-coverage refusal defers every pending request (the
            // live drain rolls back only that request's savepoint), so the
            // dry run mirrors it: with gaps, all pendings are deferred.
            let schema_gap = !crate::member_erasure::schema_gaps(&mut tx)
                .await?
                .is_empty();
            let mut deferred = 0u64;
            for (guild_id, user_id) in &pendings {
                if schema_gap
                    || crate::member_erasure::has_unresolved_guards(&mut tx, guild_id, user_id)
                        .await?
                {
                    deferred += 1;
                }
            }
            tx.commit().await?;
            RetentionReport {
                presence_raw_deleted: presence,
                voice_raw_deleted: voice,
                audit_cache_deleted: audit,
                guild_rows_purged: guild_rows,
                // The dry run reports what the live run will delete: guarded
                // requests stay pending, so they are deferred, not completed.
                guild_purges_failed: 0,
                deletions_completed: (pending as u64).saturating_sub(deferred),
                deletions_deferred: deferred,
                deletions_overdue: overdue,
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
            // Same sorted-lock discipline as erase_member, covering every
            // table the time-window deletes, guild purges, and per-user
            // deletions can touch. The plan tables plus the raw probe and
            // the purge queue, sorted so concurrent erasures lock in the
            // same order.
            let erasure_plan = crate::member_erasure::plan();
            let mut lock_tables: Vec<&str> = erasure_plan
                .tables
                .iter()
                .map(|entry| entry.table.as_str())
                .collect();
            lock_tables.push("presence_probe");
            lock_tables.push("retention_guild_purge_queue");
            lock_tables.sort_unstable();
            lock_tables.dedup();
            let lock_sql = format!(
                "LOCK TABLE {} IN SHARE ROW EXCLUSIVE MODE",
                lock_tables.join(", ")
            );
            sqlx::query(sqlx::AssertSqlSafe(lock_sql))
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
            let overdue = count_overdue_deletions(&mut tx).await?;
            // Pending deletions drain BEFORE guild purges so a departed
            // guild's pending receipts complete against pre-purge rows.
            // Each request runs in its own savepoint: a guard or
            // schema-coverage refusal rolls back only that request, leaving
            // it pending, while other failures abort the run.
            let pendings: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT request_id, guild_id, user_id FROM retention_deletion_requests WHERE status = 'pending'",
            )
            .fetch_all(&mut *tx)
            .await?;
            let mut completed = 0u64;
            let mut deferred = 0u64;
            let mut deferred_ids: Vec<String> = Vec::new();
            for (request_id, guild_id, user_id) in &pendings {
                sqlx::query("SAVEPOINT retention_pending")
                    .execute(&mut *tx)
                    .await?;
                match complete_request(&mut tx, request_id, guild_id, user_id).await {
                    Ok(_) => {
                        sqlx::query("RELEASE SAVEPOINT retention_pending")
                            .execute(&mut *tx)
                            .await?;
                        completed += 1;
                    }
                    Err(err) if is_deferrable_refusal(&err) => {
                        sqlx::query("ROLLBACK TO SAVEPOINT retention_pending")
                            .execute(&mut *tx)
                            .await?;
                        sqlx::query("RELEASE SAVEPOINT retention_pending")
                            .execute(&mut *tx)
                            .await?;
                        deferred += 1;
                        deferred_ids.push(request_id.clone());
                    }
                    Err(err) => return Err(err),
                }
            }
            if !deferred_ids.is_empty() {
                tracing::warn!(
                    deferred = deferred,
                    overdue = overdue,
                    requests = ?deferred_ids,
                    "retention user deletions deferred by unresolved replay guard or schema coverage"
                );
            }
            let guilds: Vec<String> =
                sqlx::query_scalar("SELECT guild_id FROM retention_guild_purge_queue")
                    .fetch_all(&mut *tx)
                    .await?;
            let mut guild_rows = 0u64;
            let mut guild_failed = 0u64;
            for guild in &guilds {
                sqlx::query("SAVEPOINT retention_guild")
                    .execute(&mut *tx)
                    .await?;
                match purge_guild(&mut tx, guild).await {
                    Ok(rows) => {
                        sqlx::query("RELEASE SAVEPOINT retention_guild")
                            .execute(&mut *tx)
                            .await?;
                        guild_rows += rows;
                    }
                    Err(err) => {
                        sqlx::query("ROLLBACK TO SAVEPOINT retention_guild")
                            .execute(&mut *tx)
                            .await?;
                        sqlx::query("RELEASE SAVEPOINT retention_guild")
                            .execute(&mut *tx)
                            .await?;
                        guild_failed += 1;
                        tracing::warn!(
                            guild_id = %guild,
                            error = %format!("{err:?}"),
                            "retention guild purge failed, kept queued"
                        );
                    }
                }
            }
            tx.commit().await?;
            RetentionReport {
                presence_raw_deleted: presence,
                voice_raw_deleted: voice,
                audit_cache_deleted: audit,
                guild_rows_purged: guild_rows,
                guild_purges_failed: guild_failed,
                deletions_completed: completed,
                deletions_deferred: deferred,
                deletions_overdue: overdue,
                duration_ms: started.elapsed().as_millis() as u64,
            }
        }
    };
    Ok(report)
}

/// Record a guild removal. A dispatch with no `unavailable` flag (or an
/// explicit available flag) is a real removal and enqueues a guild-data
/// purge; an unavailable/outage payload never purges. Returns true when a
/// purge was enqueued. A later rejoin must call [`cancel_guild_purge`] so the
/// next run does not wipe the guild's new and long-term rows.
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

/// Cancel a queued guild purge (e.g. on `GUILD_CREATE`/rejoin before the next
/// run). Returns true when a queued purge was removed; the guild's rows are
/// left untouched. Wire this to the gateway rejoin path alongside
/// [`handle_guild_delete`].
pub async fn cancel_guild_purge(pool: &PgPool, guild_id: &str) -> Result<bool, sqlx::Error> {
    validate_snowflake(guild_id)?;
    let removed = sqlx::query("DELETE FROM retention_guild_purge_queue WHERE guild_id = $1")
        .bind(guild_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(removed > 0)
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
/// immediate unless a replay guard is unresolved or schema coverage is
/// incomplete, in which case the request stays pending with no receipt and
/// is reported as deferred by [`run_retention`]; the under-72-hour SLA holds
/// for unguarded, covered requests. Shares `erase_member`'s locks and
/// refusal gates via `complete_request`: a refused request returns an error
/// with no receipt and stays pending.
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
    // Same sorted-lock discipline as erase_member for the tables
    // complete_request touches (every plan table, including the ledger).
    let erasure_plan = crate::member_erasure::plan();
    let mut tables: Vec<&str> = erasure_plan
        .tables
        .iter()
        .map(|entry| entry.table.as_str())
        .collect();
    tables.sort_unstable();
    let lock_sql = format!(
        "LOCK TABLE {} IN SHARE ROW EXCLUSIVE MODE",
        tables.join(", ")
    );
    sqlx::query(sqlx::AssertSqlSafe(lock_sql))
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

/// True when the error is a deferrable per-request refusal (the request must
/// stay pending, not fail the run): the shared replay-guard refusal or the
/// shared schema-coverage refusal. Matches on the message text so both the
/// erasure and retention paths stay in sync without coupling to sqlx internals.
fn is_deferrable_refusal(err: &sqlx::Error) -> bool {
    let display = format!("{err}");
    let debug = format!("{err:?}");
    display.contains("unresolved erasure replay guard")
        || debug.contains("unresolved erasure replay guard")
        || display.contains("erasure schema coverage incomplete")
        || debug.contains("erasure schema coverage incomplete")
}

/// Delete the requesting member's full footprint and settle the request,
/// returning the receipt fields. Shared by [`fulfill_user_deletion`] and the
/// [`run_retention`] pending drain so both paths honor the same receipt and
/// the same safety gates as `erase_member`.
async fn complete_request(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request_id: &str,
    guild_id: &str,
    user_id: &str,
) -> Result<(String, i64, i64), sqlx::Error> {
    // Share erase_member's safety gates: refuse on schema drift or unresolved
    // replay fences, leaving the request pending with no receipt. The caller
    // (single fulfilment or per-request savepoint) decides whether a refusal
    // is swallowed or fatal.
    if !crate::member_erasure::schema_gaps(tx).await?.is_empty() {
        return Err(sqlx::Error::InvalidArgument(
            "erasure schema coverage incomplete".into(),
        ));
    }
    if crate::member_erasure::has_unresolved_guards(tx, guild_id, user_id).await? {
        return Err(sqlx::Error::InvalidArgument(
            "unresolved erasure replay guard".into(),
        ));
    }
    // Full erasure-plan footprint in the same transaction: every plan table's
    // (guild, member) rows for this user go, not just the `members` row, so
    // the receipt certifies an actual deletion. The plan also covers
    // `retention_deletion_requests`, but the in-flight request row is excluded
    // here and settled into a completed receipt below; other pending requests
    // for the same user stay pending for their own fulfilment.
    let erasure = crate::member_erasure::plan();
    for entry in &erasure.tables {
        if entry.table == "retention_deletion_requests" {
            continue;
        }
        // Table names and predicates come from the reviewed embedded plan,
        // never caller input; predicates bind $1 = guild, $2 = member.
        let sql = format!("DELETE FROM {} WHERE {}", entry.table, entry.predicate);
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(guild_id)
            .bind(user_id)
            .execute(&mut **tx)
            .await?;
    }
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

/// Guild-scoped delete for one erasure-plan table: child tables without
/// their own `guild_id` purge via their parent's guild scope, and the
/// operator-released replay fence is never selected. Table names come from
/// the reviewed embedded plan, never caller input.
fn guild_purge_sql(table: &str) -> String {
    match table {
        "lfg_signups" | "lfg_roles" => format!(
            "DELETE FROM {table} WHERE lfg_id IN (SELECT id FROM lfg_posts WHERE guild_id = $1)"
        ),
        "feed_deliveries" => {
            "DELETE FROM feed_deliveries WHERE feed_id IN (SELECT id FROM feed_relays WHERE guild_id = $1)"
                .to_owned()
        }
        "self_role_exchanges" => {
            "DELETE FROM self_role_exchanges WHERE event_id IN (SELECT event_id FROM self_role_audit WHERE guild_id = $1)"
                .to_owned()
        }
        // Keep the operator-released replay fence: only executions whose
        // parent idempotency row is itself purged go here.
        "moderation_channel_executions" => {
            "DELETE FROM moderation_channel_executions WHERE guild_id = $1 AND idempotency_key IN (SELECT idempotency_key FROM moderation_idempotency WHERE guild_id = $1 AND (state IS DISTINCT FROM 'done' OR outcome IS DISTINCT FROM 'operator_released'))"
                .to_owned()
        }
        // Mirrors the erasure-plan predicate: done/operator_released rows are
        // an indefinite replay fence and are never selected.
        "moderation_idempotency" => {
            "DELETE FROM moderation_idempotency WHERE guild_id = $1 AND (state IS DISTINCT FROM 'done' OR outcome IS DISTINCT FROM 'operator_released')"
                .to_owned()
        }
        _ => format!("DELETE FROM {table} WHERE guild_id = $1"),
    }
}

/// Guild-scoped count for one erasure-plan table, mirroring [`guild_purge_sql`].
fn guild_purge_count_sql(table: &str) -> String {
    match table {
        "lfg_signups" | "lfg_roles" => format!(
            "SELECT count(*) FROM {table} WHERE lfg_id IN (SELECT id FROM lfg_posts WHERE guild_id = $1)"
        ),
        "feed_deliveries" => {
            "SELECT count(*) FROM feed_deliveries WHERE feed_id IN (SELECT id FROM feed_relays WHERE guild_id = $1)"
                .to_owned()
        }
        "self_role_exchanges" => {
            "SELECT count(*) FROM self_role_exchanges WHERE event_id IN (SELECT event_id FROM self_role_audit WHERE guild_id = $1)"
                .to_owned()
        }
        "moderation_channel_executions" => {
            "SELECT count(*) FROM moderation_channel_executions WHERE guild_id = $1 AND idempotency_key IN (SELECT idempotency_key FROM moderation_idempotency WHERE guild_id = $1 AND (state IS DISTINCT FROM 'done' OR outcome IS DISTINCT FROM 'operator_released'))"
                .to_owned()
        }
        "moderation_idempotency" => {
            "SELECT count(*) FROM moderation_idempotency WHERE guild_id = $1 AND (state IS DISTINCT FROM 'done' OR outcome IS DISTINCT FROM 'operator_released')"
                .to_owned()
        }
        _ => format!("SELECT count(*) FROM {table} WHERE guild_id = $1"),
    }
}

/// Pending deletion requests older than [`DELETION_SLA_HOURS`]: the overdue
/// signal for both run modes. Uses the same clock as the request ledger.
async fn count_overdue_deletions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, sqlx::Error> {
    let overdue: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE status = 'pending' AND requested_at < now() - make_interval(hours => $1::int)",
    )
    .bind(DELETION_SLA_HOURS)
    .fetch_one(&mut **tx)
    .await?;
    Ok(overdue as u64)
}

/// Purge one departed guild's rows and dequeue it, returning rows purged.
/// Covers every guild-keyed erasure-plan table (in plan order, child before
/// parent) plus the raw `presence_probe`, and leaves
/// `retention_deletion_requests` to the receipt path so a pending deletion
/// for a departed guild still completes with a receipt instead of vanishing.
/// Re-checks the queue row under the caller's lock: a guild whose purge was
/// cancelled via [`cancel_guild_purge`] (rejoin before the run) keeps its rows.
async fn purge_guild(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    guild_id: &str,
) -> Result<u64, sqlx::Error> {
    let still_queued: Option<String> =
        sqlx::query_scalar("SELECT guild_id FROM retention_guild_purge_queue WHERE guild_id = $1")
            .bind(guild_id)
            .fetch_optional(&mut **tx)
            .await?;
    if still_queued.is_none() {
        return Ok(0);
    }
    let mut purged = 0u64;
    purged += sqlx::query("DELETE FROM presence_probe WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    let erasure = crate::member_erasure::plan();
    for entry in &erasure.tables {
        if entry.table == "retention_deletion_requests" {
            continue;
        }
        // Table names come from the reviewed embedded plan, never caller input.
        let sql = guild_purge_sql(&entry.table);
        purged += sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(guild_id)
            .execute(&mut **tx)
            .await?
            .rows_affected();
    }
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
/// Mirrors [`purge_guild`]: every plan table except the deletion ledger, plus
/// the raw probe.
async fn count_queued_guild_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, sqlx::Error> {
    let guilds: Vec<String> =
        sqlx::query_scalar("SELECT guild_id FROM retention_guild_purge_queue")
            .fetch_all(&mut **tx)
            .await?;
    let erasure = crate::member_erasure::plan();
    let mut total = 0u64;
    for guild in &guilds {
        // Table names come from the reviewed embedded plan, never caller input.
        let sql = "SELECT count(*) FROM presence_probe WHERE guild_id = $1".to_owned();
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(guild)
            .fetch_one(&mut **tx)
            .await?;
        total += count as u64;
        for entry in &erasure.tables {
            if entry.table == "retention_deletion_requests" {
                continue;
            }
            let sql = guild_purge_count_sql(&entry.table);
            let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .bind(guild)
                .fetch_one(&mut **tx)
                .await?;
            total += count as u64;
        }
    }
    Ok(total)
}
