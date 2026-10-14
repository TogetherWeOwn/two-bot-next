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
use two_bot_cutover::retention::{run_retention, RetentionMode};

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
/// mode and all counters. A partial `Ok` (deferred/overdue deletions or
/// failed guild purges) logs at WARN so it is never silently reduced to
/// scheduler success. Schema-coverage gaps defer as `Ok` (counted in
/// `schema_gap_deferrals` for deletions and `guild_purges_failed` for queued
/// guilds, with the gap list in the warn log) and retry after the plan is
/// repaired; only SQL/connection failures return [`ErrorClass::Database`]
/// to the supervisor and stay retryable.
#[must_use]
pub fn job(pool: PgPool, mode: RetentionMode) -> Job {
    let cadence = CADENCE;
    Job {
        name: NAME,
        cadence,
        startup_jitter: jobs::startup_jitter(cadence, rand::random()),
        timeout: TIMEOUT,
        action: Arc::new(move || {
            let pool = pool.clone();
            Box::pin(async move {
                let report = run_retention(&pool, mode)
                    .await
                    .map_err(|_| ErrorClass::Database)?;
                let partial = report.guild_purges_failed > 0
                    || report.deletions_deferred > 0
                    || report.deletions_overdue > 0;
                if partial {
                    tracing::warn!(
                        message = "retention_run",
                        mode = ?mode,
                        presence_raw_deleted = report.presence_raw_deleted,
                        voice_raw_deleted = report.voice_raw_deleted,
                        audit_cache_deleted = report.audit_cache_deleted,
                        guild_rows_purged = report.guild_rows_purged,
                        guild_purges_failed = report.guild_purges_failed,
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
                        guild_rows_purged = report.guild_rows_purged,
                        guild_purges_failed = report.guild_purges_failed,
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
