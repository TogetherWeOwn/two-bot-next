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

use std::{sync::Arc, time::Duration};

use sqlx::PgPool;
use two_bot_core::game_activity_store;
use two_bot_cutover::retention::{run_retention, RetentionMode};

use crate::backfill_retention_chains::render_retention_failure;
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
                // Stale per-guild game-activity aggregates expire on this same
                // cadence, never on the hourly flush/send path: quiet guilds
                // stop accumulating rows while active guilds keep every fresh
                // row. Execute deletes; drier modes count the same rows and
                // log them under the same event, so the dry run previews
                // exactly what execute will delete. Runs after the report
                // above so an expiry failure never discards the retention
                // report unlogged.
                expire_stale_game_aggregates(&pool, mode).await?;
                Ok(())
            })
        }),
    }
}

/// Delete (execute) or preview (any drier mode) stale per-guild
/// game-activity aggregates. The listing carries each stale guild's row
/// count under the same predicate the delete uses, so the dry run sums
/// exactly what execute deletes with no extra query. Deletes stay one
/// guild scope at a time, so one guild's failure never rolls back another's;
/// any failure returns [`ErrorClass::Database`] for a bounded retry.
/// Expiry is idempotent and the horizon parser only yields values inside
/// `1..=MAX_AGGREGATE_RETENTION_DAYS`, so a misconfigured setting falls back
/// to the safe default instead of widening into a wipe.
async fn expire_stale_game_aggregates(
    pool: &PgPool,
    mode: RetentionMode,
) -> Result<(), ErrorClass> {
    let horizon = game_activity_store::aggregate_retention_days_from_setting(
        std::env::var(game_activity_store::AGGREGATE_RETENTION_DAYS_SETTING)
            .ok()
            .as_deref(),
    );
    let guilds = game_activity_store::list_aggregate_guilds(pool, horizon)
        .await
        .map_err(|_| aggregate_expiry_failed())?;
    let mut stale = 0u64;
    if mode == RetentionMode::Execute {
        for (guild_id, _) in &guilds {
            stale += game_activity_store::expire_stale_guild_aggregates(pool, guild_id, horizon)
                .await
                .map_err(|_| aggregate_expiry_failed())?;
        }
    } else {
        stale = guilds.iter().map(|(_, count)| *count).sum();
    }
    if stale > 0 {
        tracing::info!(
            message = "game_activity_aggregate_expiry",
            stale_rows = stale,
            horizon_days = horizon,
            mode = ?mode,
            "sweep",
        );
    }
    Ok(())
}

/// Single log site for aggregate-expiry store failures: one guild's failure
/// aborts the tick as [`ErrorClass::Database`] so the supervisor retries on
/// the next bounded opportunity.
fn aggregate_expiry_failed() -> ErrorClass {
    tracing::warn!("game_activity_aggregate_expiry_failed");
    ErrorClass::Database
}
