//! Bounded retention scheduler adapter: the producer/scheduler seam for the
//! rolling-window retention in [`two_bot_cutover::retention`].
//!
//! The gateway lifecycle hook lives in [`crate::gateway`] (real
//! `GUILD_DELETE` dispatches queue a purge with their `unavailable` flag
//! preserved, available `GUILD_CREATE`/rejoin cancels it). This module owns
//! the supervised periodic job: safe configuration parsing plus the
//! [`job`] constructor the [`crate::website_jobs`] supervisor registers.
//!
//! Safety contract: anything but the exact `execute` opt-in is a dry run or
//! parked. A missing setting defaults to dry run; `off`, invalid values and
//! a missing database park the job without attempting destructive work.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use sqlx::PgPool;
use two_bot_core::game_activity_store;
use two_bot_cutover::retention::{run_retention, RetentionMode};

use crate::backfill_retention_chains::render_retention_failure;
use crate::db_log::db_class;
use crate::gateway_failure::FailureClass;
use crate::jobs::{self, ErrorClass, Job};

/// Process-environment key selecting the retention mode. Exact,
/// case-sensitive values only; see [`mode_from_setting`].
pub const MODE_SETTING: &str = "TWO_RETENTION_MODE";

/// Supervised job name, also the `/readyz` status entry.
pub const NAME: &str = "retention";

/// Bounded cadence: fifteen minutes, inside the 60 s..=1 h contract.
const CADENCE: Duration = Duration::from_secs(900);
/// Per-attempt budget: two minutes, positive and no greater than cadence.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Bounded aggregate-expiry page: at most this many candidate guilds visited
/// per pass, so one tick never walks the whole eligible list. Small enough
/// that a pass of full per-guild batches stays far inside [`TIMEOUT`].
const AGGREGATE_GUILDS_PER_PASS: u32 = 4;
/// Bounded per-guild delete batch: at most this many stale rows expired per
/// visited guild per pass, so an oversized guild drains across passes
/// without spending the whole attempt budget on one guild.
const AGGREGATE_ROWS_PER_GUILD_PER_PASS: u32 = 10;

/// Process-local aggregate-expiry continuation: the guild key the previous
/// pass visited last. The next pass resumes after it, so consecutive ticks
/// visit later eligible guilds instead of restarting at the first page; the
/// cursor advances in dry-run mode too, where nothing is deleted and only
/// the cursor makes progress. A restart begins again at the first page: rows
/// are never marked expired by the cursor, so at worst a fleet drains from
/// the top again, never with work silently skipped.
static AGGREGATE_EXPIRY_CURSOR: Mutex<Option<String>> = Mutex::new(None);

/// Parse the [`MODE_SETTING`] value. Missing means an active dry run (never
/// destructive); `off` parks the job; anything else but the exact `dry-run`
/// and `execute` literals is a configuration refusal that also parks.
pub fn mode_from_setting(value: Option<&str>) -> Result<Option<RetentionMode>, ErrorClass> {
    match value {
        None => Ok(Some(RetentionMode::DryRun)),
        Some("dry-run") => Ok(Some(RetentionMode::DryRun)),
        Some("execute") => Ok(Some(RetentionMode::Execute)),
        Some("off") => Ok(None),
        _ => Err(ErrorClass::Configuration),
    }
}

/// Build a lazy Postgres pool for the retention job without connecting: the
/// first tick fails as [`ErrorClass::Database`] (and retries on the next
/// bounded opportunity) when the database is unreachable. An invalid URL or
/// TLS refusal is a configuration error at registration time.
pub(crate) fn pool_from_url(url: &str) -> Result<PgPool, ErrorClass> {
    use sqlx::postgres::PgPoolOptions;
    use two_bot_core::database_tls::{self, TlsPolicy};

    two_bot_core::database_url::validate(url).map_err(|_| ErrorClass::Configuration)?;
    let tls = std::env::var_os(database_tls::POLICY_SETTING);
    let tls = TlsPolicy::from_setting(tls.as_ref().and_then(|v| v.to_str()))
        .map_err(|_| ErrorClass::Configuration)?;
    database_tls::enforce(url, tls).map_err(|_| ErrorClass::Configuration)?;
    let options =
        two_bot_core::database_url::connect_options(url).map_err(|_| ErrorClass::Configuration)?;
    let options = database_tls::apply(options, tls);
    Ok(PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect_lazy_with(options))
}

/// The supervised retention job: fixed-phase [`CADENCE`], jittered start,
/// per-attempt [`TIMEOUT`], panic-isolated by the supervisor like every job.
/// The real action calls the existing cutover retention operation; every
/// successful report emits one structured `retention_run` event with the
/// mode and all counters. A partial `Ok` (deferred/overdue deletions,
/// failed guild purges, or guard-deferred guild purges) logs at WARN so it
/// is never silently reduced to scheduler success. Schema-coverage gaps
/// defer as `Ok` (counted in `schema_gap_deferrals` for deletions and
/// `guild_purges_failed` for queued guilds, with the gap list in the warn
/// log) and retry after the plan is repaired; only SQL/connection failures
/// return [`ErrorClass::Database`] to the supervisor and stay retryable.
#[must_use]
pub fn job(pool: PgPool, mode: RetentionMode) -> Job {
    let cadence = CADENCE;
    Job {
        name: NAME,
        cadence,
        cadence_source: None,
        startup_jitter: jobs::startup_jitter(cadence, rand::random()),
        timeout: TIMEOUT,
        action: Arc::new(move || {
            let pool = pool.clone();
            Box::pin(async move {
                let report = run_retention(&pool, mode).await.map_err(|err| {
                    let step = err.step();
                    let failure =
                        render_retention_failure(FailureClass::StoreUnavailable, step, err.cause());
                    tracing::warn!(
                        message = "retention_run_failed",
                        mode = ?mode,
                        class = FailureClass::StoreUnavailable.as_str(),
                        step = step.as_str(),
                        failure = %failure,
                        "retention run aborted on a database error, retryable",
                    );
                    ErrorClass::Database
                })?;
                // Stale per-guild game-activity aggregates expire on this same
                // cadence, never on the hourly flush/send path: quiet guilds
                // stop accumulating rows while active guilds keep every fresh
                // row. Execute deletes; drier modes count the same rows and
                // log them under the same event, so the dry run previews
                // exactly what execute will delete. The expiry runs before the
                // success report below so a failed tick never leaves behind the
                // plain success line. When the expiry fails after `run_retention`
                // has already committed, the committed counters still get one
                // structured `retention_run` WARN with distinct text before
                // returning, so the report is never dropped.
                if let Err(expiry) = expire_stale_game_aggregates(&pool, mode).await {
                    tracing::warn!(
                        message = "retention_run",
                        mode = ?mode,
                        presence_raw_deleted = report.presence_raw_deleted,
                        voice_raw_deleted = report.voice_raw_deleted,
                        audit_cache_deleted = report.audit_cache_deleted,
                        stream_live_delivered_expired = report.stream_live_delivered_expired,
                        guild_rows_purged = report.guild_rows_purged,
                        guild_purges_failed = report.guild_purges_failed,
                        guild_purges_deferred = report.guild_purges_deferred,
                        guild_purges_deferred_oldest_secs =
                            report.guild_purges_deferred_oldest_secs,
                        deletions_completed = report.deletions_completed,
                        deletions_deferred = report.deletions_deferred,
                        schema_gap_deferrals = report.schema_gap_deferrals,
                        deletions_overdue = report.deletions_overdue,
                        duration_ms = report.duration_ms,
                        "retention run committed; aggregate expiry failed",
                    );
                    return Err(expiry);
                }
                let partial = report.guild_purges_failed > 0
                    || report.guild_purges_deferred > 0
                    || report.deletions_deferred > 0
                    || report.deletions_overdue > 0;
                if partial {
                    tracing::warn!(
                        message = "retention_run",
                        mode = ?mode,
                        presence_raw_deleted = report.presence_raw_deleted,
                        voice_raw_deleted = report.voice_raw_deleted,
                        audit_cache_deleted = report.audit_cache_deleted,
                        stream_live_delivered_expired = report.stream_live_delivered_expired,
                        guild_rows_purged = report.guild_rows_purged,
                        guild_purges_failed = report.guild_purges_failed,
                        guild_purges_deferred = report.guild_purges_deferred,
                        guild_purges_deferred_oldest_secs =
                            report.guild_purges_deferred_oldest_secs,
                        deletions_completed = report.deletions_completed,
                        deletions_deferred = report.deletions_deferred,
                        schema_gap_deferrals = report.schema_gap_deferrals,
                        deletions_overdue = report.deletions_overdue,
                        duration_ms = report.duration_ms,
                        "retention run completed with pending work",
                    );
                } else {
                    tracing::info!(
                        message = "retention_run",
                        mode = ?mode,
                        presence_raw_deleted = report.presence_raw_deleted,
                        voice_raw_deleted = report.voice_raw_deleted,
                        audit_cache_deleted = report.audit_cache_deleted,
                        stream_live_delivered_expired = report.stream_live_delivered_expired,
                        guild_rows_purged = report.guild_rows_purged,
                        guild_purges_failed = report.guild_purges_failed,
                        guild_purges_deferred = report.guild_purges_deferred,
                        guild_purges_deferred_oldest_secs =
                            report.guild_purges_deferred_oldest_secs,
                        deletions_completed = report.deletions_completed,
                        deletions_deferred = report.deletions_deferred,
                        schema_gap_deferrals = report.schema_gap_deferrals,
                        deletions_overdue = report.deletions_overdue,
                        duration_ms = report.duration_ms,
                        "retention run completed",
                    );
                }
                Ok(())
            })
        }),
    }
}

/// Delete (execute) or preview (any drier mode) stale per-guild
/// game-activity aggregates, one bounded page per pass. Each pass visits at
/// most [`AGGREGATE_GUILDS_PER_PASS`] candidate guilds in stable keyset
/// order, resuming after [`AGGREGATE_EXPIRY_CURSOR`], and deletes at most
/// [`AGGREGATE_ROWS_PER_GUILD_PER_PASS`] rows per visited guild. Consecutive
/// passes advance past already visited guilds — in dry-run mode too, where
/// nothing is deleted — and an oversized guild drains across passes with its
/// remaining rows retained. Deletes stay one guild scope at a time, so one
/// guild's bounded failure retains its rows for a later retry while the pass
/// moves on to the later guilds instead of starving them; any failure still
/// returns [`ErrorClass::Database`] for a bounded retry. The sweep carries
/// `guilds_visited`/`guilds_deferred` counts (no per-guild labels), so a
/// partial pass is distinguishable from a complete one; `stale_rows` covers
/// only the visited page. Dry-run totals preview the visited page under the
/// same predicate, not an exact future-delete promise under concurrent
/// writes. Expiry is idempotent and the horizon parser only yields values
/// inside `1..=MAX_AGGREGATE_RETENTION_DAYS`, so a misconfigured setting
/// falls back to the safe default instead of widening into a wipe.
async fn expire_stale_game_aggregates(
    pool: &PgPool,
    mode: RetentionMode,
) -> Result<(), ErrorClass> {
    let horizon = game_activity_store::aggregate_retention_days_from_setting(
        std::env::var(game_activity_store::AGGREGATE_RETENTION_DAYS_SETTING)
            .ok()
            .as_deref(),
    );
    let resume_after = AGGREGATE_EXPIRY_CURSOR
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    let mut page = match game_activity_store::list_aggregate_guild_page(
        pool,
        horizon,
        resume_after.as_deref(),
        AGGREGATE_GUILDS_PER_PASS,
    )
    .await
    {
        Ok(page) => page,
        Err(err) => {
            tracing::warn!(
                message = "game_activity_aggregate_expiry_failed",
                horizon_days = horizon,
                mode = ?mode,
                reason = aggregate_db_class(&err),
                "aggregate expiry listing failed, retryable",
            );
            return Err(ErrorClass::Database);
        }
    };
    if page.is_empty() && resume_after.is_some() {
        // The cursor ran past the end of the eligible list: wrap to the first
        // page inside this same pass, so every pass with remaining work visits
        // at least one guild instead of idling one tick per cycle boundary.
        page = match game_activity_store::list_aggregate_guild_page(
            pool,
            horizon,
            None,
            AGGREGATE_GUILDS_PER_PASS,
        )
        .await
        {
            Ok(page) => page,
            Err(err) => {
                tracing::warn!(
                    message = "game_activity_aggregate_expiry_failed",
                    horizon_days = horizon,
                    mode = ?mode,
                    reason = aggregate_db_class(&err),
                    "aggregate expiry listing failed, retryable",
                );
                return Err(ErrorClass::Database);
            }
        };
    }
    // An empty page here means no stale guild holds rows anywhere: still fall
    // through to the sweep event with zero counts, so "swept clean" stays
    // distinguishable from "expiry never ran".
    let (last_visited, deferred) = match page.last() {
        Some((last, _)) => {
            let last = last.clone();
            let deferred = match game_activity_store::count_stale_aggregate_guilds_after(
                pool,
                horizon,
                Some(&last),
            )
            .await
            {
                Ok(deferred) => deferred,
                Err(err) => {
                    tracing::warn!(
                        message = "game_activity_aggregate_expiry_failed",
                        horizon_days = horizon,
                        mode = ?mode,
                        reason = aggregate_db_class(&err),
                        "aggregate expiry deferred-count failed, retryable",
                    );
                    return Err(ErrorClass::Database);
                }
            };
            (Some(last), deferred)
        }
        None => (None, 0),
    };
    let mut stale = 0u64;
    let mut failed = false;
    if mode == RetentionMode::Execute {
        for (guild_id, _) in &page {
            match game_activity_store::expire_stale_guild_aggregates_batch(
                pool,
                guild_id,
                horizon,
                AGGREGATE_ROWS_PER_GUILD_PER_PASS,
            )
            .await
            {
                Ok(deleted) => stale = stale.saturating_add(deleted),
                Err(err) => {
                    // Identified like the unbounded path, but isolated: the
                    // failed guild keeps its rows for a later retry while the
                    // pass moves on, so one guild cannot starve later guilds.
                    tracing::warn!(
                        message = "game_activity_aggregate_expiry_failed",
                        guild_id = guild_id.as_str(),
                        horizon_days = horizon,
                        mode = ?mode,
                        reason = aggregate_db_class(&err),
                        "aggregate expiry delete failed, retryable",
                    );
                    failed = true;
                }
            }
        }
    } else {
        stale = page.iter().map(|(_, count)| *count).sum();
    }
    // Advance past the visited page even when a guild failed, and in dry-run
    // mode where nothing was deleted: a cursor pinned on a persistently
    // failing first page would starve every later guild. Failed rows stay in
    // the table, so the next cycle retries them; nothing is ever marked
    // expired except by an actual delete.
    if let Some(last_visited) = last_visited {
        AGGREGATE_EXPIRY_CURSOR
            .lock()
            .map(|mut guard| *guard = Some(last_visited))
            .unwrap_or_default();
    }
    // The sweep event always emits, even with zero stale rows, so a clean
    // sweep stays distinguishable from expiry never running.
    // `guilds_visited`/`guilds_deferred` are counts only, never per-guild
    // labels.
    tracing::info!(
        message = "game_activity_aggregate_expiry",
        stale_rows = stale,
        horizon_days = horizon,
        mode = ?mode,
        guilds_visited = page.len(),
        guilds_deferred = deferred,
        "sweep",
    );
    if failed {
        return Err(ErrorClass::Database);
    }
    Ok(())
}

/// Classify an aggregate-expiry store failure without its internals: the raw
/// `sqlx::Error` text can carry statement detail, so the failure line carries
/// only the fixed [`db_class`] token under the house `reason` field.
fn aggregate_db_class(err: &game_activity_store::GameActivityStoreError) -> &'static str {
    match err {
        game_activity_store::GameActivityStoreError::Db(inner) => db_class(inner),
    }
}
