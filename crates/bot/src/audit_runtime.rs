//! Runtime wiring for the operational-audit mirror (docs/audit-service.md
//! "Runtime"). One [`AuditRuntime`] per process owns the single
//! [`AuditMirrorService`] — store pool, mirror destinations and the persistent
//! kill switch — and backs both the `audit_retry` supervisor job and
//! [`AuditRuntime::record`], the entry point for the gateway and moderation
//! recorders (reached through [`handle`]).
//!
//! `audit_retry` runs every 30 s (legacy `audit.retryPending()` sweep): one
//! `drain_pending` call, which surfaces at most `MAX_PENDING_ROWS` (25) rows.
//! While the persistent halt is engaged the sweep claims nothing; a halt that
//! lands mid-sweep is honored per row by the service, which releases held
//! claims unattempted. Cancellation (timeout or shutdown) mid-row is safe: the
//! boundary is persisted before any POST, so the next owner reconciles.
//!
//! With none of `DISCORD_AUDIT_LOG_CHANNEL_ID`, `DISCORD_VOICE_LOG_CHANNEL_ID`
//! or `DISCORD_MODERATION_LOG_CHANNEL_ID` set the runtime is inert: no job, no
//! handle, `audit_retry` reports parked and nothing reads the audit tables. A
//! malformed value parks it the same way, logged as `invalid_config`.
//!
//! Logs carry entry IDs and counts only — never event content, metadata, raw
//! configuration values or store error text.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use futures_util::future::BoxFuture;
use sqlx::PgPool;
use tokio::sync::{watch, OnceCell};
use two_bot_core::audit::{AuditChannelIds, AuditEvent};
use two_bot_core::audit_mirror::AuditMirror;
use two_bot_core::audit_service::{
    AuditMirrorService, DeliverOutcome, DrainReport, MirrorConfig, RecordOutcome,
};
use two_bot_core::audit_store::AuditStore;
use two_bot_discord::executor::ActionExecutor;

use crate::{
    jobs::{self, ErrorClass, Job},
    server,
    website_jobs::Context,
};

pub const NAMES: [&str; 1] = ["audit_retry"];
const JOB: &str = NAMES[0];

/// Legacy retry sweep cadence (docs/parity.md §4).
pub(crate) const CADENCE: Duration = Duration::from_secs(30);
/// Below the cadence: a full batch at ~3 paced REST calls per row finishes in
/// seconds, and a wedged sweep is cut before the next deadline.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(25);

/// Destination keys in `AuditChannelIds` order (audit, voice, moderation).
const CHANNEL_KEYS: [&str; 3] = [
    "DISCORD_AUDIT_LOG_CHANNEL_ID",
    "DISCORD_VOICE_LOG_CHANNEL_ID",
    "DISCORD_MODERATION_LOG_CHANNEL_ID",
];

#[cfg(test)]
#[path = "audit_runtime_tests.rs"]
mod tests;

/// The production runtime, shared by the job and the recorders.
pub(crate) type AuditHandle = Arc<AuditRuntime<ActionExecutor>>;

static HANDLE: OnceLock<AuditHandle> = OnceLock::new();

/// The process's audit runtime once registration built one; `None` while
/// audit is unconfigured or the gateway prerequisites are missing.
// First callers: the gateway recording slice and TOG-10346 (moderation).
#[allow(dead_code)]
pub(crate) fn handle() -> Option<AuditHandle> {
    HANDLE.get().cloned()
}

/// Mirror destinations from the environment. Unset or blank keys are absent;
/// `Err` carries the fixed parked reason only, never the raw value.
pub(crate) fn resolve_channels(
    vars: &HashMap<String, String>,
) -> Result<AuditChannelIds, &'static str> {
    let mut ids = [None, None, None];
    for (slot, key) in ids.iter_mut().zip(CHANNEL_KEYS) {
        match vars.get(key) {
            Some(value) if !value.trim().is_empty() => {
                *slot = Some(snowflake(value).ok_or("invalid_config")?);
            }
            _ => {}
        }
    }
    let [audit, voice, moderation] = ids;
    if audit.is_none() && voice.is_none() && moderation.is_none() {
        return Err("disabled");
    }
    Ok(AuditChannelIds {
        audit,
        voice,
        moderation,
    })
}

/// Canonical nonzero decimal u64 only (no sign, padding or whitespace).
fn snowflake(value: &str) -> Option<String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0 && id.to_string() == value)
        .map(|_| value.to_owned())
}

/// Build the shared runtime and its `audit_retry` job, or log why it parks.
/// The handle is installed once per process; a second call reuses it.
pub(crate) fn register(context: Arc<Context>, shutdown: watch::Receiver<bool>) -> Option<Job> {
    let channels = match resolve_channels(&std::env::vars().collect()) {
        Ok(channels) => channels,
        Err(reason) => {
            match reason {
                "invalid_config" => tracing::warn!(job = JOB, reason, "job_disabled"),
                _ => tracing::info!(job = JOB, reason, "job_disabled"),
            }
            return None;
        }
    };
    let runtime = HANDLE
        .get_or_init(|| {
            let guild = context.guild.clone();
            Arc::new(AuditRuntime::new(channels, guild, connector(context)))
        })
        .clone();
    Some(retry_job(
        runtime,
        shutdown,
        jobs::startup_jitter(CADENCE, rand::random()),
    ))
}

/// The `audit_retry` supervisor job over one shared runtime. Rows whose store
/// write failed fail the run (`database`); every per-row Discord outcome is
/// the protocol working and counts as success.
pub(crate) fn retry_job<M: AuditMirror + 'static>(
    runtime: Arc<AuditRuntime<M>>,
    shutdown: watch::Receiver<bool>,
    startup_jitter: Duration,
) -> Job {
    Job {
        name: JOB,
        cadence: CADENCE,
        startup_jitter,
        timeout: TIMEOUT,
        action: Arc::new(move || {
            let runtime = runtime.clone();
            let shutdown = shutdown.clone();
            Box::pin(async move {
                tokio::select! {
                    biased;
                    _ = server::shutdown_requested(shutdown) => Ok(()),
                    result = runtime.sweep() => match result? {
                        Sweep::Drained(report) if !report.failed.is_empty() => {
                            Err(ErrorClass::Database)
                        }
                        _ => Ok(()),
                    },
                }
            })
        }),
    }
}

/// What the runtime needs from the process, resolved lazily on first use so
/// an unreachable database or REST endpoint never blocks boot.
pub(crate) struct Parts<M> {
    pub(crate) pool: PgPool,
    pub(crate) mirror: M,
    pub(crate) bot_user_id: String,
}

pub(crate) type Connect<M> =
    Box<dyn Fn() -> BoxFuture<'static, Result<Parts<M>, ErrorClass>> + Send + Sync>;

/// Production parts from the shared jobs pool and REST executor.
fn connector(context: Arc<Context>) -> Connect<ActionExecutor> {
    Box::new(move || {
        let context = context.clone();
        Box::pin(async move {
            let pool = context.pool().await?.clone();
            rest_parts(pool, context.rest.clone()).await
        })
    })
}

/// The executor is the mirror (sharing the jobs' pacing lanes); the bot's own
/// user id, which reconciliation matches authors against, comes from Discord.
async fn rest_parts(
    pool: PgPool,
    rest: ActionExecutor,
) -> Result<Parts<ActionExecutor>, ErrorClass> {
    let bot_user_id = rest
        .current_bot_user_id()
        .await
        .map_err(|_| ErrorClass::Rest)?;
    Ok(Parts {
        pool,
        mirror: rest,
        bot_user_id: bot_user_id.to_string(),
    })
}

struct Wired<M: AuditMirror> {
    store: AuditStore,
    service: AuditMirrorService<M>,
}

/// One sweep's result.
#[derive(Debug)]
pub(crate) enum Sweep {
    /// The persistent halt was engaged: nothing was claimed.
    Halted,
    Drained(DrainReport),
}

pub(crate) struct AuditRuntime<M: AuditMirror> {
    channels: AuditChannelIds,
    guild: String,
    connect: Connect<M>,
    /// Built once on first success; a failed connect caches nothing.
    wired: OnceCell<Wired<M>>,
    /// Halt state seen by the previous sweep, for transition logs only —
    /// every sweep reads the store.
    halted: Mutex<Option<bool>>,
}

impl<M: AuditMirror> AuditRuntime<M> {
    pub(crate) fn new(channels: AuditChannelIds, guild: String, connect: Connect<M>) -> Self {
        Self {
            channels,
            guild,
            connect,
            wired: OnceCell::new(),
            halted: Mutex::new(None),
        }
    }

    async fn wired(&self) -> Result<&Wired<M>, ErrorClass> {
        self.wired
            .get_or_try_init(|| async {
                let Parts {
                    pool,
                    mirror,
                    bot_user_id,
                } = (self.connect)().await?;
                let configured = [
                    &self.channels.audit,
                    &self.channels.voice,
                    &self.channels.moderation,
                ]
                .into_iter()
                .flatten()
                .cloned()
                .collect();
                let config = MirrorConfig {
                    channels: self.channels.clone(),
                    configured,
                    mirror_guild_id: Some(self.guild.clone()),
                    bot_user_id,
                };
                Ok(Wired {
                    store: AuditStore::new(&pool),
                    service: AuditMirrorService::new(AuditStore::new(&pool), mirror, config),
                })
            })
            .await
    }

    /// Durably record one event (route + insert) for the next sweep to
    /// deliver. A failed write is `database`; nothing is sent either way.
    // First callers: the gateway recording slice and TOG-10346 (moderation).
    #[allow(dead_code)]
    pub(crate) async fn record(&self, event: &AuditEvent) -> Result<RecordOutcome, ErrorClass> {
        self.wired()
            .await?
            .service
            .record(event)
            .await
            .map_err(|_| ErrorClass::Database)
    }

    /// One retry sweep: read the halt, then drain at most one bounded batch.
    pub(crate) async fn sweep(&self) -> Result<Sweep, ErrorClass> {
        let wired = self.wired().await?;
        let Ok(halt) = wired.store.delivery_halt().await else {
            tracing::warn!(job = JOB, "audit_retry_halt_unreadable");
            return Err(ErrorClass::Database);
        };
        if self.note_halt(halt.is_some()) {
            tracing::debug!(job = JOB, "audit_retry_halted");
            return Ok(Sweep::Halted);
        }
        let report = wired
            .service
            .drain_pending()
            .await
            .map_err(|_| ErrorClass::Database)?;
        log_report(&report);
        Ok(Sweep::Drained(report))
    }

    /// Log halt transitions between sweeps; returns `halted`.
    fn note_halt(&self, halted: bool) -> bool {
        let mut seen = self.halted.lock().unwrap_or_else(|e| e.into_inner());
        match (*seen, halted) {
            (None | Some(false), true) => tracing::warn!(job = JOB, "audit_mirror_halt_engaged"),
            (Some(true), false) => tracing::info!(job = JOB, "audit_mirror_halt_disengaged"),
            _ => {}
        }
        *seen = Some(halted);
        halted
    }
}

/// Per-outcome counts, plus entry IDs for rows that need an operator:
/// quarantines (with the bounded reason) and store failures (without text).
fn log_report(report: &DrainReport) {
    let mut counts = [0usize; 8];
    for (entry_id, outcome) in &report.deliveries {
        let slot = match outcome {
            DeliverOutcome::Delivered { .. } => 0,
            DeliverOutcome::Reconciled { .. } => 1,
            DeliverOutcome::Held => 2,
            DeliverOutcome::Deferred => 3,
            DeliverOutcome::Rejected => 4,
            DeliverOutcome::Ambiguous => 5,
            DeliverOutcome::Quarantined(reason) => {
                tracing::warn!(job = JOB, entry_id = %entry_id, reason = ?reason, "audit_entry_quarantined");
                6
            }
            DeliverOutcome::Unclaimed => 7,
        };
        counts[slot] += 1;
    }
    for (entry_id, _) in &report.failed {
        tracing::warn!(job = JOB, entry_id = %entry_id, "audit_entry_store_failed");
    }
    let rows = report.deliveries.len() + report.failed.len();
    if rows == 0 {
        tracing::debug!(job = JOB, "audit_retry_idle");
        return;
    }
    let [delivered, reconciled, held, deferred, rejected, ambiguous, quarantined, unclaimed] =
        counts;
    tracing::info!(
        job = JOB,
        rows,
        delivered,
        reconciled,
        held,
        deferred,
        rejected,
        ambiguous,
        quarantined,
        unclaimed,
        failed = report.failed.len(),
        "audit_retry_swept"
    );
}
