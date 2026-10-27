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
//! A schema-coverage gap defers every queued guild purge the same way (the
//! queue row stays, counted in `guild_purges_failed`), because a purge built
//! from an incomplete plan would dequeue work it did not fully delete.
//! Gap-driven deletion deferrals are counted separately in
//! `schema_gap_deferrals` with the exact gaps in the warn log.
//! The user-deletion deletes run in the same transaction that settles the
//! receipt, excluding only the in-flight `retention_deletion_requests` row
//! itself. The time-driven deletes never touch the member-scoped audit tables
//! (`moderation_audit` and friends) beyond that user-deletion path, which
//! stays under the erasure predicates. Guild analytics toggles and opt-out
//! suppression live elsewhere and are not duplicated here.
//!
//! ## Coverage limits (do not silently claim more)
//!
//! * Voice-raw coverage covers `events` rows with a voice-session
//!   `event_type` — the producer's real `voice_session_start` /
//!   `voice_session_end` values plus the historical `voice_session_started` /
//!   `voice_session_ended` spellings already stored — and `event_store`
//!   capture rows. In `event_store`, exactly three families carry an
//!   age-based TTL, selected by `event_time` (the persisted event/observation
//!   instant), never `received_at`, so a recent backfill/replay insertion
//!   does not extend old data's lifetime: `PRESENCE_UPDATE`,
//!   `VOICE_STATE_UPDATE` and `VOICE_SERVER_UPDATE` at 30 days, and the
//!   cached Discord audit entries (`GUILD_AUDIT_LOG_ENTRY_CREATE`) at 45
//!   days. Every other `event_store` family has no age TTL here and is
//!   removed per-user via the erasure plan and per-guild via the guild
//!   purge below. Capture/backfill writers stay insert-only; this expiry is
//!   the one narrow, documented retention exception to the capture's
//!   append-only shape (`event_store` is an ordinary table with no
//!   append-only trigger, and no trigger is disabled, grant widened or
//!   table truncated to make expiry work). Production activation of the
//!   executing mode remains gated on independent security validation of
//!   that exception; the supervised job's default stays dry run.
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
use time::{Duration, OffsetDateTime};
use two_bot_core::error_chain::{
    db_cause, render_retention_failure, RetentionStep, STORE_UNAVAILABLE_CLASS,
};
use two_bot_core::stream_live_store::{
    count_expired_delivered_live, expire_delivered_live, DELIVERED_DEDUP_RETENTION_DAYS,
};

// Store class token for retention failure lines: the shared
// `STORE_UNAVAILABLE_CLASS` from core, so this side cannot drift from the
// bot gateway's `FailureClass::StoreUnavailable` token (equality is pinned
// by the bot `backfill_retention_chains` test).

/// Raw presence/voice rows older than this roll off.
pub const PRESENCE_RAW_RETENTION_DAYS: i32 = 30;
/// Voice-session `events` rows older than this roll off.
pub const VOICE_RAW_RETENTION_DAYS: i32 = 30;
/// Audit-log cache rows older than this expire.
pub const AUDIT_CACHE_RETENTION_DAYS: i32 = 45;
/// User deletion requests complete well inside this many hours.
pub const DELETION_SLA_HOURS: i64 = 72;
/// Retention job cadence and raw-expiry maximum-age headroom.
pub const RETENTION_CADENCE_SECONDS: u64 = 900;
/// Maximum raw event-store rows selected by one expiry statement.
const MAX_RAW_EXPIRY_ROWS_PER_BATCH: u32 = 1_000;

/// Voice-session `events.event_type` values the time window selects: the
/// producer's real funnel values plus the historical spellings already
/// stored before the producer values were pinned.
const FUNNEL_VOICE_EVENT_TYPES: &str =
    "'voice_session_start', 'voice_session_end', 'voice_session_started', 'voice_session_ended'";
/// `event_store.event_type` for raw presence captures (Discord dispatch name).
const CAPTURE_PRESENCE: &str = "PRESENCE_UPDATE";
/// `event_store.event_type` values for raw voice captures, including the
/// anonymous voice-server rows that carry no user id.
const CAPTURE_VOICE: [&str; 2] = ["VOICE_STATE_UPDATE", "VOICE_SERVER_UPDATE"];
/// `event_store.event_type` for cached Discord audit-log entries.
const CAPTURE_AUDIT: &str = "GUILD_AUDIT_LOG_ENTRY_CREATE";

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
    /// Raw presence rows deleted (older than 30 days): `presence_probe`
    /// rows plus `event_store` `PRESENCE_UPDATE` captures by `event_time`.
    pub presence_raw_deleted: u64,
    /// Raw voice rows deleted (older than 30 days): voice-session `events`
    /// rows plus `event_store` voice captures by `event_time`.
    pub voice_raw_deleted: u64,
    /// Audit-cache rows deleted (older than 45 days):
    /// `operational_audit_log` rows plus `event_store` cached Discord
    /// audit entries by `event_time`.
    pub audit_cache_deleted: u64,
    /// Delivered `stream_live_claims` go-live fences deleted (stamped
    /// earlier than [`DELIVERED_DEDUP_RETENTION_DAYS`] ago). Only delivered
    /// rows are eligible: never-started and started-but-ambiguous rows stay
    /// fenced however old, so a retry inside the horizon still dedups.
    pub stream_live_delivered_expired: u64,
    /// Guild-scoped rows purged for departed guilds.
    pub guild_rows_purged: u64,
    /// Queued guild purges that failed with a real error in this run,
    /// leaving the guild queued. Guard-deferred purges are NOT counted
    /// here; see `guild_purges_deferred`. A nonzero value with an `Ok`
    /// return means a departed guild's rows survived a storage or
    /// constraint error (see the warn log for the guild id).
    pub guild_purges_failed: u64,
    /// Queued guild purges deferred by an unresolved replay guard in this
    /// run, in both modes. Deferred guilds stay queued and the purge itself
    /// does not delete their rows until guards settle; the independent raw
    /// event-store age policy can still remove eligible captures. Unlike
    /// `guild_purges_failed` this needs no incident response, only guard
    /// settlement (see `purge_guild` for
    /// the per-guard settlement path). The dry run probes the same guards,
    /// so it reports exactly what the live run will defer, including guards
    /// of members with a pending user-deletion request: those rows are
    /// replay fences, not stale data, so they defer the guild too.
    pub guild_purges_deferred: u64,
    /// Age of the longest-waiting deferred guild purge, in seconds since
    /// `retention_guild_purge_queue.enqueued_at`. Zero when nothing was
    /// deferred this run. Operators watch this alongside
    /// `guild_purges_deferred`: a growing age means a departed guild's rows
    /// wait on a guard that may never settle on its own.
    pub guild_purges_deferred_oldest_secs: u64,
    /// User-deletion requests completed by this run.
    pub deletions_completed: u64,
    /// Pending deletion requests refused by a replay guard or by schema
    /// coverage in this run, staying pending with no receipt. A stuck fence
    /// or an unplanned identity column can keep a request here beyond the
    /// deletion SLA; see `deletions_overdue`.
    pub deletions_deferred: u64,
    /// Pending deletions deferred specifically because schema coverage was
    /// incomplete (subset of `deletions_deferred`). Distinguishes a code-fix
    /// need (uncovered identity column / stale plan) from transient
    /// replay-guard deferrals; the warn log's `schema_gaps` field lists the
    /// exact gaps for this run.
    pub schema_gap_deferrals: u64,
    /// Pending deletion requests older than [`DELETION_SLA_HOURS`], whether
    /// deferred or not. A nonzero value is the signal that the under-72-hour
    /// wording does not hold for every request.
    pub deletions_overdue: u64,
    /// More pending requests remain beyond this run's probed prefix. Pending
    /// rows are read in TOP-25 keyset pages and at most 25 eligible requests
    /// complete per run. Deferred rows do not starve later eligible work.
    /// Per-request counts cover the probed prefix; COUNT-based overdue counts
    /// remain exact. This fixed field is the truncation marker.
    pub pending_truncated: bool,
    /// Wall-clock run time.
    pub duration_ms: u64,
}

impl RetentionReport {
    /// Rows deleted across the time-window and guild-purge classes. The
    /// classes are disjoint in both modes: Execute deletes each row in the
    /// first stage that selects it, and the dry-run guild projection
    /// excludes rows the preceding time-window and eligible user-deletion
    /// stages would remove, including rows removed only by
    /// `ON DELETE CASCADE` from a parent the user stage deletes.
    /// Completed user deletions are receipts, not row
    /// counts, and are reported separately in `deletions_completed`;
    /// deferred and overdue requests are pending-work signals, not
    /// deletions, and are excluded, as are failed and guard-deferred guild
    /// purges (`guild_purges_failed`, `guild_purges_deferred`).
    #[must_use]
    pub fn total_deleted(&self) -> u64 {
        self.presence_raw_deleted
            + self.voice_raw_deleted
            + self.audit_cache_deleted
            + self.stream_live_delivered_expired
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

/// Render a deletion-intake failure line for the warn sites below (user-
/// deletion deferrals, schema-gap purges, replay-guard deferrals, purge
/// failures). One helper owns the step attribution so it is pinned in one
/// place: `deletion_intake_failure_pins_its_step` fails if any site ever
/// renders a different step, where the substring-only wiring pin would stay
/// green.
fn deletion_intake_failure(chain: &str) -> String {
    render_retention_failure(
        STORE_UNAVAILABLE_CLASS,
        RetentionStep::DeletionIntake,
        chain,
    )
}

/// A retention run failure with its stage and a fixed, safe cause token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionFailure {
    step: RetentionStep,
    cause: &'static str,
}

impl RetentionFailure {
    fn from_sqlx(step: RetentionStep, error: &sqlx::Error) -> Self {
        Self {
            step,
            cause: db_cause(error),
        }
    }

    /// Stage active when the database operation failed.
    #[must_use]
    pub const fn step(self) -> RetentionStep {
        self.step
    }

    /// Fixed database-cause token; never includes database error text.
    #[must_use]
    pub const fn cause(self) -> &'static str {
        self.cause
    }
}

/// Run the retention job: apply rolling-window deletes, pending user-deletion
/// completion, and the guild-purge drain, then attempt one globally capped raw
/// event-store batch. Dry run reports the full raw-expiry backlog and changes
/// nothing. Execute runs raw expiry only after core retention succeeds. The core
/// transaction commits separately from the raw batch; raw-stage failure is
/// logged and returned as a `RetentionFailure` (step plus fixed cause token)
/// for the scheduler to map to `ErrorClass::Database` and retry next cadence,
/// while already committed core work remains complete. User-deletion
/// and queue writes happen in ONE transaction with the same lock/timeout
/// discipline as `erase_member`; each pending deletion
/// runs in its own savepoint so one guard or schema-coverage refusal leaves
/// that request pending without rolling back the time-window deletes, and
/// each guild purge runs in its own savepoint so one bad guild stays queued
/// without rolling back the rest of the run. Pending deletions drain before
/// guild purges so a departed guild's pending receipts complete against
/// pre-purge rows, and purges leave the deletion ledger to the receipt path.
/// Guard- or schema-refused requests are counted in `deletions_deferred`
/// (and logged at warn level) and SLA-breaching pendings in
/// `deletions_overdue`, in both modes. Guard-deferred guild purges are
/// counted in `guild_purges_deferred` with the longest wait in
/// `guild_purges_deferred_oldest_secs`, in both modes, so the dry run
/// previews exactly what the live run will defer. Schema gaps are computed
/// once per run: gap-deferred deletions are counted separately in
/// `schema_gap_deferrals` with the gap list in the warn log, and every queued
/// guild stays queued (counted in `guild_purges_failed`) while gaps exist.
pub async fn run_retention(
    pool: &PgPool,
    mode: RetentionMode,
) -> Result<RetentionReport, RetentionFailure> {
    let mut step = match mode {
        RetentionMode::DryRun => RetentionStep::DryRun,
        RetentionMode::Execute => RetentionStep::Expiry,
    };
    run_retention_inner(pool, mode, &mut step)
        .await
        .map_err(|error| RetentionFailure::from_sqlx(step, &error))
}

async fn run_retention_inner(
    pool: &PgPool,
    mode: RetentionMode,
    active_step: &mut RetentionStep,
) -> Result<RetentionReport, sqlx::Error> {
    let started = std::time::Instant::now();
    let at = OffsetDateTime::now_utc();
    let next_sweep_deadline = at + Duration::seconds(RETENTION_CADENCE_SECONDS as i64);
    let core = run_retention_core(pool, mode, active_step);
    let raw_expiry = async {
        if mode == RetentionMode::DryRun {
            count_raw_expiry_candidates(pool, next_sweep_deadline).await
        } else {
            let batch = run_raw_expiry_batch(
                pool,
                mode,
                at,
                next_sweep_deadline,
                MAX_RAW_EXPIRY_ROWS_PER_BATCH,
            )
            .await?;
            if batch.total_deleted() > 0 {
                tracing::info!(
                    message = "retention_raw_expiry_committed",
                    mode = ?mode,
                    presence_raw_deleted = batch.presence_raw_deleted,
                    voice_raw_deleted = batch.voice_raw_deleted,
                    audit_cache_deleted = batch.audit_cache_deleted,
                    "raw retention batch committed",
                );
            }
            Ok(batch)
        }
    };

    let (mut report, raw_result) = run_retention_stages(core, raw_expiry).await?;
    match raw_result {
        Ok(raw_expiry) => {
            report.presence_raw_deleted += raw_expiry.presence_raw_deleted;
            report.voice_raw_deleted += raw_expiry.voice_raw_deleted;
            report.audit_cache_deleted += raw_expiry.audit_cache_deleted;
        }
        Err(error) => {
            if mode == RetentionMode::Execute {
                *active_step = RetentionStep::Expiry;
            }
            let step = *active_step;
            let failure = render_retention_failure(STORE_UNAVAILABLE_CLASS, step, db_cause(&error));
            tracing::warn!(
                message = "retention_raw_expiry_failed",
                mode = ?mode,
                raw_expiry_failed = true,
                class = STORE_UNAVAILABLE_CLASS,
                step = step.as_str(),
                failure = %failure,
                presence_raw_deleted = report.presence_raw_deleted,
                voice_raw_deleted = report.voice_raw_deleted,
                audit_cache_deleted = report.audit_cache_deleted,
                stream_live_delivered_expired = report.stream_live_delivered_expired,
                guild_rows_purged = report.guild_rows_purged,
                guild_purges_failed = report.guild_purges_failed,
                guild_purges_deferred = report.guild_purges_deferred,
                guild_purges_deferred_oldest_secs = report.guild_purges_deferred_oldest_secs,
                deletions_completed = report.deletions_completed,
                deletions_deferred = report.deletions_deferred,
                schema_gap_deferrals = report.schema_gap_deferrals,
                deletions_overdue = report.deletions_overdue,
                pending_truncated = report.pending_truncated,
                duration_ms = started.elapsed().as_millis() as u64,
                "core retention completed; raw expiry failed and will retry on the next cadence",
            );
            return Err(error);
        }
    }
    report.duration_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

async fn run_retention_stages<Core, Raw>(
    core: Core,
    raw_expiry: Raw,
) -> Result<(RetentionReport, Result<RetentionReport, sqlx::Error>), sqlx::Error>
where
    Core: std::future::Future<Output = Result<RetentionReport, sqlx::Error>>,
    Raw: std::future::Future<Output = Result<RetentionReport, sqlx::Error>>,
{
    let core = core.await?;
    let raw_expiry = raw_expiry.await;
    Ok((core, raw_expiry))
}

struct PendingSelection {
    eligible: Vec<(String, String, String)>,
    deferred: u64,
    truncated: bool,
}

/// Select at most 25 eligible requests without an unbounded queue Vec. The
/// keyset advances past guarded rows; otherwise 25 permanently guarded head
/// rows would prevent every later deletion from ever reaching the drain.
/// Dry-run, execute and raw-expiry projection share this exact selection.
async fn select_pending(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    schema_gap: bool,
) -> Result<PendingSelection, sqlx::Error> {
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE status = 'pending'",
    )
    .fetch_one(&mut **tx)
    .await?;
    let mut selection = PendingSelection {
        eligible: Vec::new(),
        deferred: 0,
        truncated: false,
    };
    if schema_gap {
        selection.deferred = total as u64;
        return Ok(selection);
    }
    let mut after = None::<String>;
    let mut probed = 0u64;
    while selection.eligible.len() < 25 {
        // TOP-25 plus one lookahead. No whole-queue materialization, including
        // on an all-guarded queue. request_id is a unique, stable keyset.
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT request_id, guild_id, user_id FROM retention_deletion_requests
             WHERE status = 'pending' AND ($1::text IS NULL OR request_id > $1)
             ORDER BY request_id LIMIT 26",
        )
        .bind(after.as_deref())
        .fetch_all(&mut **tx)
        .await?;
        let more = rows.len() > 25;
        for (request_id, guild_id, user_id) in rows.into_iter().take(25) {
            after = Some(request_id.clone());
            probed += 1;
            if crate::member_erasure::has_unresolved_guards(tx, &guild_id, &user_id).await? {
                selection.deferred += 1;
            } else {
                selection.eligible.push((request_id, guild_id, user_id));
                if selection.eligible.len() == 25 {
                    break;
                }
            }
        }
        if !more {
            break;
        }
    }
    selection.truncated = probed < total as u64;
    Ok(selection)
}

async fn run_retention_core(
    pool: &PgPool,
    mode: RetentionMode,
    active_step: &mut RetentionStep,
) -> Result<RetentionReport, sqlx::Error> {
    let started = std::time::Instant::now();
    let mut tx = pool.begin().await?;
    let report = match mode {
        RetentionMode::DryRun => {
            *active_step = RetentionStep::DryRun;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await?;
            let presence = count_presence_raw(&mut tx).await?;
            let voice = count_voice_raw(&mut tx).await?;
            let audit = count_audit_cache(&mut tx).await?;
            let live_expired =
                count_expired_delivered_live(&mut *tx, DELIVERED_DEDUP_RETENTION_DAYS).await?;
            let overdue = count_overdue_deletions(&mut tx).await?;
            // A schema-coverage refusal defers every pending request (the
            // live drain rolls back only that request's savepoint), so the
            // dry run mirrors it: with gaps, all pendings are deferred.
            // Guild purges defer the same way: a purge built from an
            // incomplete plan would dequeue work it did not fully delete.
            // Split pendings before projecting guild rows: an Execute run
            // deletes eligible users' rows in the user-deletion stage, ahead
            // of the guild purge, so those rows must not be counted again as
            // guild rows. Guard-deferred requests stay pending, and any
            // unresolved guard in the guild defers its purge too.
            let gaps = crate::member_erasure::schema_gaps(&mut tx).await?;
            let schema_gap = !gaps.is_empty();
            let selected = select_pending(&mut tx, schema_gap).await?;
            let deferred = selected.deferred;
            let eligible: Vec<(String, String)> = selected
                .eligible
                .iter()
                .map(|(_, guild_id, user_id)| (guild_id.clone(), user_id.clone()))
                .collect();
            // With schema gaps a purge built from an incomplete plan would
            // dequeue work it did not fully delete, so every queued guild
            // stays queued (failed, not purged) and guard probing is
            // skipped. Without gaps the guild projection probes the same
            // guild-wide replay guards as the live run (see
            // count_queued_guild_rows): guarded guilds contribute no rows
            // and count as deferred, never as purged. Unpredictable storage
            // errors stay unreported here (the dry run deletes nothing to
            // fail on).
            let (guild_rows, guild_failed, guild_deferred, guild_deferred_oldest) = if schema_gap {
                let queued: i64 =
                    sqlx::query_scalar("SELECT count(*) FROM retention_guild_purge_queue")
                        .fetch_one(&mut *tx)
                        .await?;
                (0, queued as u64, 0, 0)
            } else {
                let (rows, deferred_guilds, deferred_oldest) =
                    count_queued_guild_rows(&mut tx, &eligible).await?;
                (rows, 0, deferred_guilds, deferred_oldest)
            };
            tx.commit().await?;
            RetentionReport {
                presence_raw_deleted: presence,
                voice_raw_deleted: voice,
                audit_cache_deleted: audit,
                stream_live_delivered_expired: live_expired,
                guild_rows_purged: guild_rows,
                // The dry run reports what the live run will delete: guarded
                // requests stay pending, so they are deferred, not completed;
                // guarded guild purges are deferred, never failed; and gapped
                // guilds stay queued, so they are failed, not purged.
                guild_purges_failed: guild_failed,
                guild_purges_deferred: guild_deferred,
                guild_purges_deferred_oldest_secs: guild_deferred_oldest,
                deletions_completed: selected.eligible.len() as u64,
                deletions_deferred: deferred,
                schema_gap_deferrals: if schema_gap { deferred } else { 0 },
                deletions_overdue: overdue,
                pending_truncated: selected.truncated,
                duration_ms: started.elapsed().as_millis() as u64,
            }
        }
        RetentionMode::Execute => {
            *active_step = RetentionStep::Expiry;
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
            let presence = sqlx::query("DELETE FROM presence_probe WHERE observed_at < to_char(now() - make_interval(days => $1), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')")
                .bind(PRESENCE_RAW_RETENTION_DAYS)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            let voice = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM events WHERE event_type IN ({FUNNEL_VOICE_EVENT_TYPES}) AND occurred_at < now() - make_interval(days => $1)"
            )))
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
            // Delivered go-live fences past the dedup horizon go in the same
            // time-window stage: the store predicate keeps every row a live
            // claim or send could reference, and this never runs on the send
            // path.
            let live_expired =
                expire_delivered_live(&mut *tx, DELIVERED_DEDUP_RETENTION_DAYS).await?;
            let overdue = count_overdue_deletions(&mut tx).await?;
            // Schema gaps are computed once per run, before either drain. A
            // gap means no purge or deletion can certify completeness, so
            // every pending request defers and every queued guild stays
            // queued; the exact gaps ride the warn logs for the code fix.
            *active_step = RetentionStep::DeletionIntake;
            let schema_gaps = crate::member_erasure::schema_gaps(&mut tx).await?;
            let schema_gapped = !schema_gaps.is_empty();
            // Pending deletions drain BEFORE guild purges so a departed
            // guild's pending receipts complete against pre-purge rows.
            // Each request runs in its own savepoint: a guard or
            // schema-coverage refusal rolls back only that request, leaving
            // it pending, while other failures abort the run.
            let selected = select_pending(&mut tx, schema_gapped).await?;
            let pendings = selected.eligible;
            let mut completed = 0u64;
            let mut deferred = selected.deferred;
            let mut gap_deferred = if schema_gapped { selected.deferred } else { 0 };
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
                        if is_schema_gap_refusal(&err) {
                            gap_deferred += 1;
                        }
                        deferred_ids.push(request_id.clone());
                    }
                    Err(err) => return Err(err),
                }
            }
            if deferred > 0 {
                let failure =
                    deletion_intake_failure("replay guard or schema gap deferred the request");
                tracing::warn!(
                    deferred = deferred,
                    schema_gap_deferrals = gap_deferred,
                    overdue = overdue,
                    requests = ?deferred_ids,
                    schema_gaps = ?schema_gaps,
                    class = STORE_UNAVAILABLE_CLASS,
                    step = RetentionStep::DeletionIntake.as_str(),
                    failure = %failure,
                    "retention user deletions deferred by unresolved replay guard or schema coverage"
                );
            }
            let guilds: Vec<(String, i64)> = sqlx::query_as(
            "SELECT guild_id, (EXTRACT(EPOCH FROM (clock_timestamp() - enqueued_at)))::BIGINT FROM retention_guild_purge_queue",
        )
        .fetch_all(&mut *tx)
        .await?;
            let mut guild_rows = 0u64;
            let mut guild_failed = 0u64;
            let mut guild_deferred = 0u64;
            let mut guild_deferred_oldest = 0u64;
            // Schema gaps are gated once up front: a purge built from an
            // incomplete plan would dequeue work it did not fully delete, so
            // every queued guild stays queued while gaps exist. Otherwise
            // each guild drains in its own savepoint with the same guard
            // settlement the dry-run projection reports.
            if schema_gapped {
                guild_failed = guilds.len() as u64;
                if !guilds.is_empty() {
                    let failure = deletion_intake_failure("schema gap deferred the purge");
                    tracing::warn!(
                        guilds = ?guilds,
                        schema_gaps = ?schema_gaps,
                        class = STORE_UNAVAILABLE_CLASS,
                        step = RetentionStep::DeletionIntake.as_str(),
                        failure = %failure,
                        "retention guild purges deferred by schema coverage, kept queued"
                    );
                }
            } else {
                for (guild, waited_secs) in &guilds {
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
                            // Gaps are pre-gated above, so a deferrable
                            // refusal here is an unresolved replay guard:
                            // keep the guild queued as deferred, not failed.
                            if is_deferrable_refusal(&err) {
                                guild_deferred += 1;
                                guild_deferred_oldest =
                                    guild_deferred_oldest.max((*waited_secs).max(0) as u64);
                                let failure =
                                    deletion_intake_failure("replay guard deferred the purge");
                                tracing::warn!(
                                    guild_id = %guild,
                                    waited_secs = (*waited_secs).max(0),
                                    class = STORE_UNAVAILABLE_CLASS,
                                    step = RetentionStep::DeletionIntake.as_str(),
                                    failure = %failure,
                                    "retention guild purge deferred by unresolved replay guard, kept queued"
                                );
                            } else {
                                guild_failed += 1;
                                let failure = deletion_intake_failure(db_cause(&err));
                                tracing::warn!(
                                    guild_id = %guild,
                                    class = STORE_UNAVAILABLE_CLASS,
                                    step = RetentionStep::DeletionIntake.as_str(),
                                    failure = %failure,
                                    "retention guild purge failed, kept queued"
                                );
                            }
                        }
                    }
                }
            }
            tx.commit().await?;
            RetentionReport {
                presence_raw_deleted: presence,
                voice_raw_deleted: voice,
                audit_cache_deleted: audit,
                stream_live_delivered_expired: live_expired,
                guild_rows_purged: guild_rows,
                guild_purges_failed: guild_failed,
                guild_purges_deferred: guild_deferred,
                guild_purges_deferred_oldest_secs: guild_deferred_oldest,
                deletions_completed: completed,
                deletions_deferred: deferred,
                schema_gap_deferrals: gap_deferred,
                deletions_overdue: overdue,
                pending_truncated: selected.truncated,
                duration_ms: started.elapsed().as_millis() as u64,
            }
        }
    };
    Ok(report)
}

/// Count the full bounded-policy backlog for the lifecycle's dry-run report.
/// Execute deletes at most one capped batch of this eligible set per run; the
/// read-only projection must not report only the first batch as the total
/// would-delete count.
///
/// Execute stages run time-window, then eligible user deletions, then guild
/// purges, and only then the capped raw batch (see [`run_retention`] and
/// [`run_retention_stages`]). The dry-run count therefore excludes rows those
/// preceding stages would already delete, mirroring the dry-run core: captures
/// matching an eligible (guard-clear, schema-covered) pending deletion settle
/// as receipts, never as raw row counts, and captures in a queued guild whose
/// purge would succeed are purged as guild rows. Guard-deferred and
/// schema-gapped guilds stay queued, and guard-deferred deletions stay
/// pending, so the independent raw age policy still removes those captures
/// and they remain counted here.
async fn count_raw_expiry_candidates(
    pool: &PgPool,
    next_sweep_deadline: OffsetDateTime,
) -> Result<RetentionReport, sqlx::Error> {
    let started = std::time::Instant::now();
    let presence_cutoff =
        next_sweep_deadline - Duration::days(i64::from(PRESENCE_RAW_RETENTION_DAYS));
    let voice_cutoff = next_sweep_deadline - Duration::days(i64::from(VOICE_RAW_RETENTION_DAYS));
    let audit_cutoff = next_sweep_deadline - Duration::days(i64::from(AUDIT_CACHE_RETENTION_DAYS));
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await?;
    // Mirror the dry-run core's preceding-stage settlement: a schema-coverage
    // gap defers every pending deletion and every queued guild purge, so with
    // gaps nothing precedes the raw batch and no row is excluded.
    let schema_gapped = !crate::member_erasure::schema_gaps(&mut tx)
        .await?
        .is_empty();
    let mut eligible_pairs: Vec<(String, String)> = Vec::new();
    let mut queued_unguarded: Vec<String> = Vec::new();
    if !schema_gapped {
        eligible_pairs = select_pending(&mut tx, false)
            .await?
            .eligible
            .into_iter()
            .map(|(_, guild_id, user_id)| (guild_id, user_id))
            .collect();
        let queued: Vec<String> =
            sqlx::query_scalar("SELECT guild_id FROM retention_guild_purge_queue")
                .fetch_all(&mut *tx)
                .await?;
        for guild_id in &queued {
            if !crate::member_erasure::has_unresolved_guild_guards(&mut tx, guild_id).await? {
                queued_unguarded.push(guild_id.clone());
            }
        }
    }
    // Keep SQL shape constant as the pending-deletion queue grows. Separate
    // per-class counts also let the composite event_type/event_time index
    // provide a bounded range scan for each expiring family.
    let eligible_guilds: Vec<String> = eligible_pairs
        .iter()
        .map(|(guild_id, _)| guild_id.clone())
        .collect();
    let eligible_users: Vec<String> = eligible_pairs
        .iter()
        .map(|(_, user_id)| user_id.clone())
        .collect();
    let presence = count_raw_expiry_class(
        &mut tx,
        CAPTURE_PRESENCE,
        presence_cutoff,
        &queued_unguarded,
        &eligible_guilds,
        &eligible_users,
    )
    .await?;
    let voice_state = count_raw_expiry_class(
        &mut tx,
        CAPTURE_VOICE[0],
        voice_cutoff,
        &queued_unguarded,
        &eligible_guilds,
        &eligible_users,
    )
    .await?;
    let voice_server = count_raw_expiry_class(
        &mut tx,
        CAPTURE_VOICE[1],
        voice_cutoff,
        &queued_unguarded,
        &eligible_guilds,
        &eligible_users,
    )
    .await?;
    let audit = count_raw_expiry_class(
        &mut tx,
        CAPTURE_AUDIT,
        audit_cutoff,
        &queued_unguarded,
        &eligible_guilds,
        &eligible_users,
    )
    .await?;
    let voice = voice_state + voice_server;
    tx.commit().await?;
    Ok(RetentionReport {
        presence_raw_deleted: presence as u64,
        voice_raw_deleted: voice as u64,
        audit_cache_deleted: audit as u64,
        stream_live_delivered_expired: 0,
        guild_rows_purged: 0,
        guild_purges_failed: 0,
        guild_purges_deferred: 0,
        guild_purges_deferred_oldest_secs: 0,
        deletions_completed: 0,
        deletions_deferred: 0,
        schema_gap_deferrals: 0,
        deletions_overdue: 0,
        pending_truncated: false,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

async fn count_raw_expiry_class(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event_type: &str,
    cutoff: OffsetDateTime,
    queued_guilds: &[String],
    eligible_guilds: &[String],
    eligible_users: &[String],
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM event_store
         WHERE event_type = $1 AND event_time <= $2
           AND guild_id <> ALL($3::text[])
           AND NOT EXISTS (
               SELECT 1 FROM unnest($4::text[], $5::text[]) AS eligible(guild_id, user_id)
               WHERE event_store.guild_id = eligible.guild_id
                 AND (event_store.actor_id = eligible.user_id
                   OR event_store.target_id = eligible.user_id
                   OR event_store.user_id = eligible.user_id
                   OR event_store.inviter_id = eligible.user_id)
           )",
    )
    .bind(event_type)
    .bind(cutoff)
    .bind(queued_guilds.to_vec())
    .bind(eligible_guilds.to_vec())
    .bind(eligible_users.to_vec())
    .fetch_one(&mut **tx)
    .await
}

/// One bounded batch of raw `event_store` expiry for the existing retention
/// lifecycle: at most `max_rows` rows across the presence, voice and
/// audit-cache classes combined (oldest `event_time` first), in a single
/// transaction that either commits every reported row or none.
///
/// Cutoff semantics give the maximum-age bound its cadence headroom: a row
/// is eligible when `event_time <= next_sweep_deadline - retention_window`
/// (30 days for presence/voice, 45 for the audit cache), compared as UTC
/// instants with equality inclusive, so a row that would cross its maximum
/// age before the next scheduled sweep is removed by this one. Selection
/// is by `event_time` only; `received_at` never extends a row's lifetime.
/// `at` is the caller's sweep instant and `next_sweep_deadline` must lie
/// after it; a zero `max_rows` or a nonpositive horizon is refused before
/// any write.
///
/// A caller that needs to drain a backlog may repeat calls until one reports
/// fewer than `max_rows` rows; each call commits at most one batch. The
/// production lifecycle intentionally runs one batch per cadence so overdue
/// user deletions and guild purges still run when raw captures accumulate.
/// The lifecycle owner supplies the deadline from its real cadence (no second
/// scheduler lives here). Dry run reports at most one batch, not the full
/// backlog; the `run_retention` lifecycle dry run reports the full count.
/// Dry run writes nothing. Only the three raw-capture classes are reported; every
/// other report field is zero because guild purges and user deletions stay
/// with [`run_retention`].
pub async fn run_raw_expiry_batch(
    pool: &PgPool,
    mode: RetentionMode,
    at: OffsetDateTime,
    next_sweep_deadline: OffsetDateTime,
    max_rows: u32,
) -> Result<RetentionReport, sqlx::Error> {
    if max_rows == 0 || next_sweep_deadline <= at {
        return Err(sqlx::Error::InvalidArgument(
            "raw expiry batch requires a positive row budget and a deadline after the sweep instant"
                .into(),
        ));
    }
    let started = std::time::Instant::now();
    let presence_cutoff =
        next_sweep_deadline - Duration::days(i64::from(PRESENCE_RAW_RETENTION_DAYS));
    let voice_cutoff = next_sweep_deadline - Duration::days(i64::from(VOICE_RAW_RETENTION_DAYS));
    let audit_cutoff = next_sweep_deadline - Duration::days(i64::from(AUDIT_CACHE_RETENTION_DAYS));
    // Restrict each branch by its indexed event_type before applying the
    // timestamp range. A single OR predicate on event_time walks the entire
    // event_time index through non-expiring families on every cadence.
    let candidates = "WITH candidates AS (\
        (SELECT id, event_time, event_type FROM event_store WHERE event_type = $5 AND event_time <= $1 ORDER BY event_time, id LIMIT $4) \
        UNION ALL \
        (SELECT id, event_time, event_type FROM event_store WHERE event_type = $6 AND event_time <= $2 ORDER BY event_time, id LIMIT $4) \
        UNION ALL \
        (SELECT id, event_time, event_type FROM event_store WHERE event_type = $7 AND event_time <= $2 ORDER BY event_time, id LIMIT $4) \
        UNION ALL \
        (SELECT id, event_time, event_type FROM event_store WHERE event_type = $8 AND event_time <= $3 ORDER BY event_time, id LIMIT $4)\
    )";
    let mut tx = pool.begin().await?;
    let selected: Vec<String> = match mode {
        RetentionMode::DryRun => {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await?;
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "{candidates} SELECT event_type FROM candidates ORDER BY event_time, id LIMIT $4"
            )))
            .bind(presence_cutoff)
            .bind(voice_cutoff)
            .bind(audit_cutoff)
            .bind(i64::from(max_rows))
            .bind(CAPTURE_PRESENCE)
            .bind(CAPTURE_VOICE[0])
            .bind(CAPTURE_VOICE[1])
            .bind(CAPTURE_AUDIT)
            .fetch_all(&mut *tx)
            .await?
        }
        RetentionMode::Execute => {
            sqlx::query("SET LOCAL lock_timeout = '5s'")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SET LOCAL statement_timeout = '15s'")
                .execute(&mut *tx)
                .await?;
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "{candidates} DELETE FROM event_store USING (SELECT id FROM candidates ORDER BY event_time, id LIMIT $4) AS batch WHERE event_store.id = batch.id RETURNING event_store.event_type"
            )))
            .bind(presence_cutoff)
            .bind(voice_cutoff)
            .bind(audit_cutoff)
            .bind(i64::from(max_rows))
            .bind(CAPTURE_PRESENCE)
            .bind(CAPTURE_VOICE[0])
            .bind(CAPTURE_VOICE[1])
            .bind(CAPTURE_AUDIT)
            .fetch_all(&mut *tx)
            .await?
        }
    };
    tx.commit().await?;
    let mut report = RetentionReport {
        presence_raw_deleted: 0,
        voice_raw_deleted: 0,
        audit_cache_deleted: 0,
        stream_live_delivered_expired: 0,
        guild_rows_purged: 0,
        guild_purges_failed: 0,
        guild_purges_deferred: 0,
        guild_purges_deferred_oldest_secs: 0,
        deletions_completed: 0,
        deletions_deferred: 0,
        schema_gap_deferrals: 0,
        deletions_overdue: 0,
        pending_truncated: false,
        duration_ms: started.elapsed().as_millis() as u64,
    };
    for event_type in &selected {
        if event_type == CAPTURE_PRESENCE {
            report.presence_raw_deleted += 1;
        } else if CAPTURE_VOICE.contains(&event_type.as_str()) {
            report.voice_raw_deleted += 1;
        } else if event_type == CAPTURE_AUDIT {
            report.audit_cache_deleted += 1;
        }
    }
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
    display.contains(crate::member_erasure::REPLAY_GUARD_REFUSAL)
        || debug.contains(crate::member_erasure::REPLAY_GUARD_REFUSAL)
        || is_schema_gap_refusal(err)
}

/// True when the error is the shared schema-coverage refusal (subset of
/// [`is_deferrable_refusal`): the request deferred because the erasure plan
/// does not cover the live schema, not because of a transient replay fence.
fn is_schema_gap_refusal(err: &sqlx::Error) -> bool {
    let display = format!("{err}");
    let debug = format!("{err:?}");
    display.contains(crate::member_erasure::SCHEMA_GAP_REFUSAL)
        || debug.contains(crate::member_erasure::SCHEMA_GAP_REFUSAL)
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
            crate::member_erasure::SCHEMA_GAP_REFUSAL.into(),
        ));
    }
    if crate::member_erasure::has_unresolved_guards(tx, guild_id, user_id).await? {
        return Err(sqlx::Error::InvalidArgument(
            crate::member_erasure::REPLAY_GUARD_REFUSAL.into(),
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

/// Guild-scope predicate for one erasure-plan table: child tables without
/// their own `guild_id` purge via their parent's guild scope, and the
/// operator-released replay fence is never selected. Table names come from
/// the reviewed embedded plan, never caller input. Binds $1 = guild.
fn guild_purge_predicate(table: &str) -> String {
    match table {
        // Starboard uses BIGINT IDs; the retention queue supplies decimal text.
        "starboard_starrers" | "starboard_reaction_cursors" => "guild_id = $1::bigint".to_owned(),
        "lfg_signups" | "lfg_roles" => {
            "lfg_id IN (SELECT id FROM lfg_posts WHERE guild_id = $1)".to_owned()
        }
        "feed_deliveries" => {
            "feed_id IN (SELECT id FROM feed_relays WHERE guild_id = $1)".to_owned()
        }
        "self_role_exchanges" => {
            "event_id IN (SELECT event_id FROM self_role_audit WHERE guild_id = $1)".to_owned()
        }
        // Presence spans carry no guild column: scope through the
        // scheduled-event mirror, mirroring the erasure-plan predicate.
        "event_voice_presence" => {
            "event_id IN (SELECT event_id FROM scheduled_events WHERE guild_id = $1)".to_owned()
        }
        // Keep the operator-released replay fence: only executions whose
        // parent idempotency row is itself purged go here.
        "moderation_channel_executions" => {
            "guild_id = $1 AND idempotency_key IN (SELECT idempotency_key FROM moderation_idempotency WHERE guild_id = $1 AND (state IS DISTINCT FROM 'done' OR outcome IS DISTINCT FROM 'operator_released'))"
                .to_owned()
        }
        // Mirrors the erasure-plan predicate: done/operator_released rows are
        // an indefinite replay fence and are never selected.
        "moderation_idempotency" => {
            "guild_id = $1 AND (state IS DISTINCT FROM 'done' OR outcome IS DISTINCT FROM 'operator_released')"
                .to_owned()
        }
        _ => "guild_id = $1".to_owned(),
    }
}

/// Guild-scoped delete for one erasure-plan table.
fn guild_purge_sql(table: &str) -> String {
    format!("DELETE FROM {table} WHERE {}", guild_purge_predicate(table))
}

/// Time-window conditions an Execute run applies to this table before the
/// guild purge, as SQL over caller-numbered day-interval placeholders
/// starting at `first_placeholder`; empty when the table has no
/// time-window stage. Mirrors the Execute deletes in [`run_retention`] so
/// the dry-run guild projection can exclude rows that stage would already
/// have removed. `event_store` has no arm here on purpose: its raw-expiry
/// batch runs AFTER the guild purge in [`run_retention_stages`], so a queued
/// guild's time-eligible captures are purged as guild rows in Execute and the
/// dry-run projection must count them as guild rows too. The dry-run raw
/// count ([`count_raw_expiry_candidates`]) excludes those guild and
/// eligible-user rows instead, keeping the per-class buckets disjoint.
fn guild_projection_time_exclusions(table: &str, first_placeholder: usize) -> Vec<(String, i32)> {
    match table {
        "events" => vec![(
            format!(
                "event_type IN ({FUNNEL_VOICE_EVENT_TYPES}) AND occurred_at < now() - make_interval(days => ${first_placeholder})"
            ),
            VOICE_RAW_RETENTION_DAYS,
        )],
        "operational_audit_log" => vec![(
            format!("occurred_at < now() - make_interval(days => ${first_placeholder})"),
            AUDIT_CACHE_RETENTION_DAYS,
        )],
        _ => Vec::new(),
    }
}

/// `ON DELETE CASCADE` parents of one erasure-plan table, as
/// `(parent table, join condition)` pairs with fully qualified columns.
/// Mirrors the CASCADE foreign keys in migrations 0170, 0180, 0210, 0413,
/// 0416 and 0418: every CASCADE child the guild purge counts. The
/// remaining CASCADE child, `self_role_exchange_baselines` (0206), is not
/// a plan table and is never counted by the purge, so it needs no
/// exclusion. A child row whose parent the user-deletion stage deletes is
/// removed by the cascade in an Execute run even when the child's own
/// plan predicate does not match the deleted member (another member's
/// block or grant in the deleted member's room), so the dry-run guild
/// projection excludes it via the parent's own plan predicate; several
/// children (`lfg_signups`, `lfg_roles`, `feed_deliveries`) already fold
/// that parent condition into their predicate, and the explicit exclusion
/// keeps them correct if the predicate changes.
fn guild_projection_cascade_parents(table: &str) -> &'static [(&'static str, &'static str)] {
    match table {
        "lfg_signups" => &[
            ("lfg_posts", "lfg_posts.id = lfg_signups.lfg_id"),
            (
                "lfg_roles",
                "lfg_roles.lfg_id = lfg_signups.lfg_id AND lfg_roles.role_key = lfg_signups.role_key",
            ),
        ],
        "lfg_roles" => &[("lfg_posts", "lfg_posts.id = lfg_roles.lfg_id")],
        "feed_deliveries" => &[("feed_relays", "feed_relays.id = feed_deliveries.feed_id")],
        "ticket_transcripts" => &[("tickets", "tickets.id = ticket_transcripts.ticket_id")],
        "voice_room_blocks" => &[(
            "voice_rooms",
            "voice_rooms.guild_id = voice_room_blocks.guild_id AND voice_rooms.channel_id = voice_room_blocks.room_channel_id",
        )],
        "voice_join_grants" => &[(
            "voice_rooms",
            "voice_rooms.guild_id = voice_join_grants.guild_id AND voice_rooms.channel_id = voice_join_grants.room_channel_id",
        )],
        "voice_owner_grants" => &[(
            "voice_rooms",
            "voice_rooms.guild_id = voice_owner_grants.guild_id AND voice_rooms.channel_id = voice_owner_grants.channel_id",
        )],
        _ => &[],
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
/// Refuses before the guild-purge DELETEs while any replay guard is unresolved
/// anywhere in the guild (see `has_unresolved_guild_guards` in
/// `crate::member_erasure`, which takes no member exclusions): the purge keeps
/// the guild queued and its rows untouched until the guards settle. The
/// separate age-expiry stage can still remove eligible raw event-store captures.
/// The caller reports the refusal via `guild_purges_deferred` (with the wait in
/// `guild_purges_deferred_oldest_secs`), never `guild_purges_failed`.
/// `not_sent` internal intents deliberately block too: a `not_sent` row is a
/// live same-key retry permit (the owning caller may still re-claim it), and
/// purging the row would destroy the dedup record so a later retry executes
/// as a brand-new intent. Settling a deferred guild is always through the
/// guard's own path, never by deleting the guard row: `not_sent` intents
/// settle when the owning caller retries the same key, and self-role,
/// automod-claim, and containment guards settle through their own runtime
/// lifecycles. For `unknown` and stale `in_flight` internal intents,
/// `InternalActionStore::reconcile` is NOT an operator path (it needs the
/// caller's exact raw payload bytes); the shipped operator exit is
/// `two-bot reconcile-event`, which only lists and resolves
/// `event.upsert` / `event.cancel` intents by id with independently-proven
/// evidence, and only for the staging allowlisted guild. Any other action
/// or guild has no shipped operator settlement today, so the guild stays
/// queued and the deferred counters are the visibility. There is no
/// auto-expiry; a growing `guild_purges_deferred_oldest_secs` is the
/// escalation signal.
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
    if crate::member_erasure::has_unresolved_guild_guards(tx, guild_id).await? {
        return Err(sqlx::Error::InvalidArgument(
            crate::member_erasure::REPLAY_GUARD_REFUSAL.into(),
        ));
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
    let probe: i64 = sqlx::query_scalar("SELECT count(*) FROM presence_probe WHERE observed_at < to_char(now() - make_interval(days => $1), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')")
        .bind(PRESENCE_RAW_RETENTION_DAYS)
        .fetch_one(&mut **tx)
        .await?;
    Ok(probe as u64)
}

async fn count_voice_raw(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, sqlx::Error> {
    let funnel: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM events WHERE event_type IN ({FUNNEL_VOICE_EVENT_TYPES}) AND occurred_at < now() - make_interval(days => $1)"
    )))
    .bind(VOICE_RAW_RETENTION_DAYS)
    .fetch_one(&mut **tx)
    .await?;
    Ok(funnel as u64)
}

async fn count_audit_cache(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, sqlx::Error> {
    let operational: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operational_audit_log WHERE occurred_at < now() - make_interval(days => $1)",
    )
    .bind(AUDIT_CACHE_RETENTION_DAYS)
    .fetch_one(&mut **tx)
    .await?;
    Ok(operational as u64)
}

/// One dynamically-numbered bind for a guild-projection count.
enum ProjectionBind<'a> {
    Text(&'a str),
    Days(i32),
}

/// Rows the guild-purge stage of an Execute run would delete, without
/// deleting anything. Mirrors [`purge_guild`]: every plan table except the
/// deletion ledger, plus the raw probe. Execute runs the purge LAST, so the
/// projection excludes rows the preceding stages would already remove —
/// expired time-window rows and the guild's eligible (not guard-deferred)
/// pending user deletions, including `ON DELETE CASCADE` children of
/// parents those deletions remove — instead of counting rows twice across
/// per-class buckets. Exclusions use `IS NOT TRUE` so a NULL time or member
/// column keeps the row, matching a DELETE whose WHERE is not true. Plan
/// predicates are evaluated against this snapshot; the plan's
/// child-before-parent order keeps every referenced parent visible at the
/// same point in an Execute run, so the snapshot and staged evaluations
/// agree.
///
/// Guilds with any unresolved replay guard contribute no rows: they are
/// reported as deferred (with the longest queue wait) so the dry run
/// previews exactly what the live run will defer.
async fn count_queued_guild_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    eligible_deletions: &[(String, String)],
) -> Result<(u64, u64, u64), sqlx::Error> {
    let queued: Vec<(String, i64)> = sqlx::query_as(
        "SELECT guild_id, (EXTRACT(EPOCH FROM (clock_timestamp() - enqueued_at)))::BIGINT FROM retention_guild_purge_queue",
    )
    .fetch_all(&mut **tx)
    .await?;
    let erasure = crate::member_erasure::plan();
    let mut total = 0u64;
    let mut deferred_guilds = 0u64;
    let mut deferred_oldest = 0u64;
    for (guild, waited_secs) in &queued {
        if crate::member_erasure::has_unresolved_guild_guards(&mut *tx, guild).await? {
            deferred_guilds += 1;
            deferred_oldest = deferred_oldest.max((*waited_secs).max(0) as u64);
            continue;
        }
        let mut eligible_users: Vec<&str> = eligible_deletions
            .iter()
            .filter(|(pending_guild, _)| pending_guild == guild)
            .map(|(_, user_id)| user_id.as_str())
            .collect();
        eligible_users.sort_unstable();
        eligible_users.dedup();
        // Table names and predicates come from the reviewed embedded plan,
        // never caller input. The probe is not a plan table, so the
        // user-deletion stage never touches it; only its own time window
        // runs ahead of the purge.
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM presence_probe WHERE guild_id = $1 AND (observed_at < to_char(now() - make_interval(days => $2), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')) IS NOT TRUE",
        )
        .bind(guild)
        .bind(PRESENCE_RAW_RETENTION_DAYS)
        .fetch_one(&mut **tx)
        .await?;
        total += count as u64;
        for entry in &erasure.tables {
            if entry.table == "retention_deletion_requests" {
                continue;
            }
            let mut sql = format!(
                "SELECT count(*) FROM {} WHERE {}",
                entry.table,
                guild_purge_predicate(&entry.table)
            );
            let mut binds: Vec<ProjectionBind<'_>> = vec![ProjectionBind::Text(guild)];
            let mut next_placeholder = 2usize;
            for (condition, days) in
                guild_projection_time_exclusions(&entry.table, next_placeholder)
            {
                sql.push_str(&format!(" AND ({condition}) IS NOT TRUE"));
                binds.push(ProjectionBind::Days(days));
                next_placeholder += 1;
            }
            for user_id in &eligible_users {
                // The plan predicate binds $1 = guild, $2 = member; move the
                // member bind to this query's next placeholder. The only
                // other `$` occurrences in plan predicates are regex end
                // anchors (`...|$)`), which never read `$2`.
                let user_placeholder = next_placeholder;
                let predicate = entry
                    .predicate
                    .replace("$2", &format!("${user_placeholder}"));
                sql.push_str(&format!(" AND ({predicate}) IS NOT TRUE"));
                // Cascade children: a row the user stage removes only
                // because it deletes the row's CASCADE parent (matched by
                // the parent's own plan predicate, evaluated inside the
                // EXISTS so its unqualified columns bind to the parent)
                // never reaches the guild stage. Shares this user's bind.
                for (parent_table, join_condition) in guild_projection_cascade_parents(&entry.table)
                {
                    let Some(parent) = erasure
                        .tables
                        .iter()
                        .find(|plan| plan.table == *parent_table)
                    else {
                        continue;
                    };
                    let parent_predicate = parent
                        .predicate
                        .replace("$2", &format!("${user_placeholder}"));
                    sql.push_str(&format!(
                        " AND NOT EXISTS (SELECT 1 FROM {parent_table} WHERE {join_condition} AND ({parent_predicate}))"
                    ));
                }
                binds.push(ProjectionBind::Text(user_id));
                next_placeholder += 1;
            }
            let mut query = sqlx::query_scalar(sqlx::AssertSqlSafe(sql));
            for bind in binds {
                query = match bind {
                    ProjectionBind::Text(value) => query.bind(value),
                    ProjectionBind::Days(days) => query.bind(days),
                };
            }
            let count: i64 = query.fetch_one(&mut **tx).await?;
            total += count as u64;
        }
    }
    Ok((total, deferred_guilds, deferred_oldest))
}

#[cfg(test)]
mod raw_expiry_producer_tests {
    use super::{CAPTURE_AUDIT, CAPTURE_PRESENCE, CAPTURE_VOICE, FUNNEL_VOICE_EVENT_TYPES};
    use twilight_model::gateway::event::EventType as Dispatch;
    use two_bot_core::funnel::EventType as Funnel;

    #[test]
    fn expiry_literals_match_their_producer_enum_names() {
        assert_eq!(CAPTURE_PRESENCE, Dispatch::PresenceUpdate.name().unwrap());
        assert_eq!(CAPTURE_VOICE[0], Dispatch::VoiceStateUpdate.name().unwrap());
        assert_eq!(
            CAPTURE_VOICE[1],
            Dispatch::VoiceServerUpdate.name().unwrap()
        );
        assert_eq!(
            CAPTURE_AUDIT,
            Dispatch::GuildAuditLogEntryCreate.name().unwrap()
        );
        for (event, expected) in [
            (Funnel::VoiceSessionStart, "voice_session_start"),
            (Funnel::VoiceSessionEnd, "voice_session_end"),
        ] {
            assert_eq!(event.as_str(), expected);
            assert!(FUNNEL_VOICE_EVENT_TYPES.contains(&format!("'{expected}'")));
        }
        for historical in ["voice_session_started", "voice_session_ended"] {
            assert!(
                FUNNEL_VOICE_EVENT_TYPES.contains(&format!("'{historical}'")),
                "historical voice event spelling {historical} must remain covered",
            );
        }
    }
}

#[cfg(test)]
mod retention_stage_order_tests {
    use super::{run_retention_stages, RetentionReport};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn empty_report() -> RetentionReport {
        RetentionReport {
            presence_raw_deleted: 0,
            voice_raw_deleted: 0,
            audit_cache_deleted: 0,
            stream_live_delivered_expired: 0,
            guild_rows_purged: 0,
            guild_purges_failed: 0,
            guild_purges_deferred: 0,
            guild_purges_deferred_oldest_secs: 0,
            deletions_completed: 0,
            deletions_deferred: 0,
            schema_gap_deferrals: 0,
            deletions_overdue: 0,
            pending_truncated: false,
            duration_ms: 0,
        }
    }

    #[tokio::test]
    async fn core_failure_prevents_raw_expiry_mutation() {
        let raw_ran = AtomicBool::new(false);
        let core = async { Err::<RetentionReport, _>(sqlx::Error::RowNotFound) };
        let raw_expiry = async {
            raw_ran.store(true, Ordering::SeqCst);
            Ok::<_, sqlx::Error>(empty_report())
        };

        let result = run_retention_stages(core, raw_expiry).await;

        assert!(matches!(result, Err(sqlx::Error::RowNotFound)));
        assert!(!raw_ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn raw_expiry_failure_does_not_skip_core_retention() {
        let core_ran = AtomicBool::new(false);
        let core = async {
            core_ran.store(true, Ordering::SeqCst);
            let mut report = empty_report();
            report.deletions_completed = 2;
            Ok::<_, sqlx::Error>(report)
        };
        let raw_expiry = async {
            assert!(core_ran.load(Ordering::SeqCst));
            Err(sqlx::Error::RowNotFound)
        };

        let (report, raw_result) = run_retention_stages(core, raw_expiry)
            .await
            .expect("core report survives raw-expiry failure");

        assert_eq!(report.deletions_completed, 2);
        assert!(matches!(raw_result, Err(sqlx::Error::RowNotFound)));
        assert!(core_ran.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_abort_step_follows_mode() {
        let pool = PgPool::connect_lazy("postgres://unused:unused@localhost/unused").unwrap();
        pool.close().await;

        for (mode, expected_step) in [
            (RetentionMode::DryRun, RetentionStep::DryRun),
            (RetentionMode::Execute, RetentionStep::Expiry),
        ] {
            let failure = run_retention(&pool, mode).await.unwrap_err();
            assert_eq!(failure.step(), expected_step);
            assert_eq!(failure.cause(), "pool_closed");
        }
    }

    #[test]
    fn deletion_intake_failure_pins_its_step() {
        // Every warn site renders through `deletion_intake_failure`, so this
        // pins all four at once: a mutant swapping in any other step fails
        // here, where the substring-only wiring pin would stay green.
        let line = deletion_intake_failure("pool_timeout");
        assert!(
            line.contains("step=deletion_intake"),
            "deletion-intake warns must attribute DeletionIntake: {line:?}"
        );
        for other in ["step=expiry", "step=dry_run"] {
            assert!(
                !line.contains(other),
                "deletion-intake warns must not attribute {other}: {line:?}"
            );
        }
        assert!(
            line.contains(&format!("class={STORE_UNAVAILABLE_CLASS} ")),
            "deletion-intake warns must carry the shared store class: {line:?}"
        );
    }
}

#[cfg(test)]
mod ledger_tests {
    //! Offline ledger laws for retention deferral and dry-run counting.
    //!
    //! The DB-backed acceptance suites prove the behavior against a live
    //! schema. These offline tests pin refusal classification, purge predicate
    //! shape, and report aggregation without a database. The #958 savepoint
    //! behavior and #960 projection's agreement with Execute are exercised by
    //! the acceptance suites; the offline total test only checks that equal
    //! row-class inputs produce equal aggregate totals.

    use super::*;

    /// Deterministic xorshift generator for property sweeps without new deps.
    struct Ladder(u64);

    impl Ladder {
        fn next(&mut self) -> u64 {
            let mut x = self.0 | 1;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    fn report(values: [u64; 12]) -> RetentionReport {
        let [presence, voice, audit, live_expired, guild_rows, failed, deferred_guilds, oldest_secs, completed, deferred, gap, overdue] =
            values;
        RetentionReport {
            presence_raw_deleted: presence,
            voice_raw_deleted: voice,
            audit_cache_deleted: audit,
            stream_live_delivered_expired: live_expired,
            guild_rows_purged: guild_rows,
            guild_purges_failed: failed,
            guild_purges_deferred: deferred_guilds,
            guild_purges_deferred_oldest_secs: oldest_secs,
            deletions_completed: completed,
            deletions_deferred: deferred,
            schema_gap_deferrals: gap,
            deletions_overdue: overdue,
            pending_truncated: false,
            duration_ms: 0,
        }
    }

    #[test]
    fn empty_run_deletes_nothing() {
        let idle = report([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(idle.total_deleted(), 0);
    }

    #[test]
    fn total_counts_each_row_once_and_nothing_else() {
        // Row classes stay disjoint while receipts, deferrals, failures and
        // overdue signals vary freely: none of them may leak into the total.
        let mut rng = Ladder(0x1234_5678_9abc_def0);
        for _ in 0..256 {
            let (presence, voice, audit, live_expired, guild_rows) = (
                rng.below(1_000_000),
                rng.below(1_000_000),
                rng.below(1_000_000),
                rng.below(1_000_000),
                rng.below(1_000_000),
            );
            let raffle = report([
                presence,
                voice,
                audit,
                live_expired,
                guild_rows,
                rng.next(),
                rng.next(),
                rng.next(),
                rng.next(),
                rng.next(),
                rng.next(),
                rng.next(),
            ]);
            assert_eq!(
                raffle.total_deleted(),
                presence + voice + audit + live_expired + guild_rows,
                "the total must sum exactly the five row classes, got {raffle:?}"
            );
        }
    }

    #[test]
    fn dry_and_execute_totals_agree_on_shared_classes() {
        // Report aggregation is mode-independent: when callers provide the
        // same row-class counts, mode-specific signals do not change totals.
        // The DB acceptance suite exercises the actual #960 projection logic.
        let mut rng = Ladder(0x0bad_f00d_dead_beef);
        for _ in 0..256 {
            let (presence, voice, audit, live_expired, guild_rows) = (
                rng.below(1_000_000),
                rng.below(1_000_000),
                rng.below(1_000_000),
                rng.below(1_000_000),
                rng.below(1_000_000),
            );
            let dry = report([
                presence,
                voice,
                audit,
                live_expired,
                guild_rows,
                rng.next(),
                rng.next(),
                rng.next(),
                rng.next(),
                rng.next(),
                rng.next(),
                rng.next(),
            ]);
            let live = report([
                presence,
                voice,
                audit,
                live_expired,
                guild_rows,
                rng.next(),
                rng.next(),
                rng.next(),
                rng.next(),
                rng.next(),
                rng.next(),
                rng.next(),
            ]);
            assert_eq!(
                dry.total_deleted(),
                live.total_deleted(),
                "equal row classes must agree on the total, dry {dry:?} vs live {live:?}"
            );
        }
    }

    #[test]
    fn deferral_decision_table_keeps_known_refusals_and_aborts_the_rest() {
        // The #958 law: these two refusals roll back only their request's
        // savepoint and stay pending; anything else aborts the run.
        let gap = sqlx::Error::InvalidArgument(crate::member_erasure::SCHEMA_GAP_REFUSAL.into());
        assert!(is_deferrable_refusal(&gap));
        assert!(is_schema_gap_refusal(&gap));

        let guard =
            sqlx::Error::InvalidArgument(crate::member_erasure::REPLAY_GUARD_REFUSAL.into());
        assert!(is_deferrable_refusal(&guard));
        assert!(!is_schema_gap_refusal(&guard));

        for fatal in [
            sqlx::Error::RowNotFound,
            sqlx::Error::PoolClosed,
            sqlx::Error::InvalidArgument("boom".into()),
            sqlx::Error::InvalidArgument(String::new()),
        ] {
            assert!(
                !is_deferrable_refusal(&fatal),
                "{fatal:?} must abort the run, not defer"
            );
            assert!(!is_schema_gap_refusal(&fatal));
        }
    }

    #[test]
    fn gap_deferral_is_a_strict_subset_of_deferrable() {
        // Gap-deferred requests are always a subset of deferred requests, and
        // the guard refusal proves the subset is strict: deferrable without
        // being a gap.
        let guard =
            sqlx::Error::InvalidArgument(crate::member_erasure::REPLAY_GUARD_REFUSAL.into());
        assert!(is_deferrable_refusal(&guard) && !is_schema_gap_refusal(&guard));

        for err in [
            sqlx::Error::InvalidArgument(crate::member_erasure::SCHEMA_GAP_REFUSAL.into()),
            sqlx::Error::InvalidArgument(
                "erase_member: erasure schema coverage incomplete: retention_deferral_acceptance_probe.owner_id"
                    .into(),
            ),
            guard,
            sqlx::Error::RowNotFound,
            sqlx::Error::PoolClosed,
        ] {
            if is_schema_gap_refusal(&err) {
                assert!(
                    is_deferrable_refusal(&err),
                    "a gap deferral must also be deferrable, got {err:?}"
                );
            }
        }
    }

    #[test]
    fn refusal_matching_is_exact_not_fuzzy() {
        // Producers and classifiers share these constants, preventing their
        // refusal wording from drifting independently. Near-misses must not
        // silently flip a deferral into a run abort (or a fatal error into one).
        for near_miss in [
            "erasure schema coverage",
            "schema coverage incomplete",
            "coverage incomplete",
            "unresolved erasure guard",
            "unresolved replay guard",
            "UNRESOLVED ERASURE REPLAY GUARD",
            "erasure schema coverage Complete",
        ] {
            let err = sqlx::Error::InvalidArgument(near_miss.into());
            assert!(
                !is_deferrable_refusal(&err),
                "{near_miss:?} must not classify"
            );
            assert!(!is_schema_gap_refusal(&err));
        }
        // Context-wrapped producer strings still defer: callers may add the
        // path that raised the refusal.
        let wrapped = sqlx::Error::InvalidArgument(format!(
            "purge_guild: {} for guild 123",
            crate::member_erasure::REPLAY_GUARD_REFUSAL
        ));
        assert!(is_deferrable_refusal(&wrapped));
        assert!(!is_schema_gap_refusal(&wrapped));
    }

    #[test]
    fn projection_excludes_every_execute_time_window_stage() {
        // The #960 coverage law: every Execute time-window delete on a
        // guild-purged table has a dry-run projection exclusion with the same
        // retention window, and no other table grows a stray exclusion that
        // would hide rows from the projection.
        let (events_condition, events_days) = guild_projection_time_exclusions("events", 2)
            .into_iter()
            .next()
            .expect("events needs a projection exclusion");
        assert!(events_condition.contains("$2"), "{events_condition}");
        assert_eq!(events_days, VOICE_RAW_RETENTION_DAYS);

        let (audit_condition, audit_days) =
            guild_projection_time_exclusions("operational_audit_log", 2)
                .into_iter()
                .next()
                .expect("operational_audit_log needs a projection exclusion");
        assert!(audit_condition.contains("$2"), "{audit_condition}");
        assert_eq!(audit_days, AUDIT_CACHE_RETENTION_DAYS);

        let mut without_exclusion: Vec<String> = crate::member_erasure::plan()
            .tables
            .iter()
            .map(|entry| entry.table.clone())
            .collect();
        without_exclusion.push("presence_probe".to_owned());
        for table in without_exclusion {
            if table == "events" || table == "operational_audit_log" {
                continue;
            }
            assert!(
                guild_projection_time_exclusions(&table, 2).is_empty(),
                "{table} must not grow a projection exclusion"
            );
        }
    }

    #[test]
    fn projection_placeholders_track_caller_numbering() {
        // The exclusion binds the caller's placeholder, so stacked user
        // exclusions keep numbering without collision.
        let (condition, days) = guild_projection_time_exclusions("events", 4)
            .into_iter()
            .next()
            .expect("events needs a projection exclusion");
        assert!(condition.contains("$4"), "{condition}");
        assert!(!condition.contains("$2"), "{condition}");
        assert_eq!(days, VOICE_RAW_RETENTION_DAYS);
    }

    #[test]
    fn guild_purge_predicate_shape_covers_plan_tables_except_the_ledger() {
        // Pin the generated predicate shape for plan tables other than the
        // deletion ledger. Runtime ledger preservation is covered by DB-backed
        // acceptance tests; this offline check does not execute purge_guild.
        let plan = crate::member_erasure::plan();
        assert!(
            plan.tables
                .iter()
                .any(|entry| entry.table == "retention_deletion_requests"),
            "the ledger skip must stay meaningful"
        );
        for entry in &plan.tables {
            if entry.table == "retention_deletion_requests" {
                continue;
            }
            let predicate = guild_purge_predicate(&entry.table);
            assert!(
                predicate.contains("$1"),
                "{table} purge must bind the guild",
                table = entry.table
            );
            let sql = guild_purge_sql(&entry.table);
            assert!(sql.starts_with("DELETE FROM "), "{sql} must be a delete");
            assert!(sql.contains(&predicate), "{sql} must apply its predicate");
        }
    }

    #[test]
    fn purge_predicates_keep_the_operator_released_replay_fence() {
        // Done/operator-released idempotency rows are an indefinite replay
        // fence: the purge must never select them.
        for table in ["moderation_idempotency", "moderation_channel_executions"] {
            let predicate = guild_purge_predicate(table);
            assert!(
                predicate.contains(
                    "(state IS DISTINCT FROM 'done' OR outcome IS DISTINCT FROM 'operator_released')"
                ),
                "{table} must exclude the released fence rows: {predicate}"
            );
        }
    }

    #[test]
    fn cascade_exclusions_reference_live_plan_parents() {
        // Every CASCADE parent the projection excludes through must resolve
        // to a live plan table, so a plan rename fails here instead of
        // silently double-counting cascade-removed rows.
        let plan = crate::member_erasure::plan();
        let names: Vec<&str> = plan
            .tables
            .iter()
            .map(|entry| entry.table.as_str())
            .collect();
        let mut covered = 0;
        for child in [
            "lfg_signups",
            "lfg_roles",
            "feed_deliveries",
            "ticket_transcripts",
            "voice_room_blocks",
            "voice_join_grants",
            "voice_owner_grants",
        ] {
            for (parent, _) in guild_projection_cascade_parents(child) {
                assert!(
                    names.contains(parent),
                    "{child} references a missing plan table {parent}"
                );
                covered += 1;
            }
        }
        assert!(covered > 0, "the cascade map must not go empty");
        let signups: Vec<&str> = guild_projection_cascade_parents("lfg_signups")
            .iter()
            .map(|(parent, _)| *parent)
            .collect();
        assert!(signups.contains(&"lfg_posts"));
        assert!(guild_projection_cascade_parents("members").is_empty());
        assert!(guild_projection_cascade_parents("presence_probe").is_empty());
    }
}
