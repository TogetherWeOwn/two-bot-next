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
//! claims unattempted. Every readable halt also sets the label-free
//! `two_bot_audit_delivery_halt` gauge (1 engaged, 0 cleared) for staleness
//! evaluation. Cancellation (timeout or shutdown) mid-row is safe: the
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
use tokio::sync::{watch, Mutex as AsyncMutex};
use two_bot_core::audit::{AuditChannelIds, AuditEvent};
use two_bot_core::audit_mirror::AuditMirror;
use two_bot_core::audit_service::{
    AuditMirrorService, DeliverOutcome, DrainReport, MirrorConfig, RecordOutcome,
};
use two_bot_core::audit_store::AuditStore;
use two_bot_core::settings::LiveSettings;
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

/// Layer the live snapshot over the boot deployment (store-first, like the
/// raid/join-risk/containment runtimes): stored rows win, deleted rows fall
/// back to the boot value. Only the three mirror destinations are read; every
/// other published key is ignored.
pub(crate) fn layered_vars(
    deployment: &HashMap<String, String>,
    guild_id: &str,
    live: Option<&LiveSettings>,
) -> HashMap<String, String> {
    let mut vars = deployment.clone();
    if let Some(live) = live {
        vars.extend(
            live.env_snapshot(Some(guild_id))
                .into_iter()
                .filter(|(key, _)| CHANNEL_KEYS.contains(&key.as_str())),
        );
    }
    vars
}

/// Mirror destinations from layered vars. Unset or blank keys are absent;
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
/// The handle is installed once per process; a second call reuses it. A
/// process with no destination at boot stays parked (the job is never
/// installed); every installed runtime refreshes its destinations live.
pub(crate) fn register(context: Arc<Context>, shutdown: watch::Receiver<bool>) -> Option<Job> {
    let deployment: HashMap<String, String> = std::env::vars()
        .filter(|(key, _)| CHANNEL_KEYS.contains(&key.as_str()))
        .collect();
    let runtime = match AuditRuntime::new(deployment, context.guild.clone(), connector(context)) {
        Ok(runtime) => runtime,
        Err(reason) => {
            match reason {
                "invalid_config" => tracing::warn!(job = JOB, reason, "job_disabled"),
                _ => tracing::info!(job = JOB, reason, "job_disabled"),
            }
            return None;
        }
    };
    let runtime = HANDLE.get_or_init(|| Arc::new(runtime)).clone();
    Some(retry_job(
        runtime,
        shutdown,
        jobs::startup_jitter(CADENCE, rand::random()),
    ))
}

/// The `audit_retry` supervisor job over one shared runtime. Rows whose store
/// write failed fail the run (`database`); every per-row Discord outcome is
/// the protocol working and counts as success.
pub(crate) fn retry_job<M: AuditMirror + Clone + 'static>(
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
    pool: PgPool,
    mirror: M,
    bot_user_id: String,
    /// Destinations the cached service was built for; a mismatch rebuilds it.
    channels: AuditChannelIds,
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
    /// Boot deployment values for the three destinations; the live snapshot
    /// layers over these on every refresh.
    deployment: HashMap<String, String>,
    guild: String,
    connect: Connect<M>,
    /// Current destinations (boot, then live). Re-resolved whenever the
    /// published snapshot moves; a malformed stored row keeps the last good
    /// destinations rather than parking a running mirror.
    channels: Mutex<AuditChannelIds>,
    /// Published snapshot revision the destinations were last resolved from;
    /// `None` before any live reader exists.
    live_revision: Mutex<Option<i64>>,
    /// Connection parts (pool, mirror, bot id) built once on first success; a
    /// failed connect caches nothing. The service rebuilds only when the
    /// destinations move, so halt-transition memory survives idle sweeps.
    /// Shared through `Arc`: record and sweep clone the current wiring under
    /// a brief lock, then run without holding it, so a paced multi-row sweep
    /// never queues gateway records behind it.
    wired: AsyncMutex<Option<Arc<Wired<M>>>>,
    /// Halt state seen by the previous sweep, for transition logs only —
    /// every sweep reads the store.
    halted: Mutex<Option<bool>>,
}

impl<M: AuditMirror + Clone> AuditRuntime<M> {
    pub(crate) fn new(
        deployment: HashMap<String, String>,
        guild: String,
        connect: Connect<M>,
    ) -> Result<Self, &'static str> {
        let channels = resolve_channels(&layered_vars(
            &deployment,
            &guild,
            crate::settings_jobs::live().as_ref(),
        ))?;
        Ok(Self {
            deployment,
            guild,
            connect,
            channels: Mutex::new(channels),
            live_revision: Mutex::new(None),
            wired: AsyncMutex::new(None),
            halted: Mutex::new(None),
        })
    }

    /// Re-resolve destinations when the published snapshot moved. Pure apart
    /// from the process-wide live reader; production passes the poller's
    /// snapshot, tests pass an explicit pair.
    pub(crate) fn refresh_channels_with(&self, live: Option<&LiveSettings>) {
        let revision = live.map(LiveSettings::revision);
        if *self.live_revision.lock().unwrap_or_else(|e| e.into_inner()) == revision {
            return;
        }
        *self.live_revision.lock().unwrap_or_else(|e| e.into_inner()) = revision;
        let vars = layered_vars(&self.deployment, &self.guild, live);
        match resolve_channels(&vars) {
            Ok(next) => {
                let mut current = self.channels.lock().unwrap_or_else(|e| e.into_inner());
                if *current != next {
                    tracing::info!(job = JOB, "setting_changed: audit destinations");
                    *current = next;
                }
            }
            Err("disabled") => {
                let next = AuditChannelIds::default();
                let mut current = self.channels.lock().unwrap_or_else(|e| e.into_inner());
                if *current != next {
                    tracing::info!(job = JOB, "audit destinations disabled live");
                    *current = next;
                }
            }
            Err(reason) => {
                tracing::warn!(
                    job = JOB,
                    reason,
                    "audit destinations unusable; keeping the last good"
                );
            }
        }
    }

    /// Destinations for tests without a database or a poller.
    #[cfg(test)]
    pub(crate) fn channels_for_test(&self) -> AuditChannelIds {
        self.channels.lock().unwrap().clone()
    }

    /// Halt memory for tests: the state the previous sweep reported, `None`
    /// before the first sweep. The `/metrics` gauge mirrors these same
    /// transitions through the process-global registry.
    #[cfg(test)]
    pub(crate) fn halted_for_test(&self) -> Option<bool> {
        *self.halted.lock().unwrap()
    }

    fn build_wired(
        pool: PgPool,
        mirror: M,
        bot_user_id: String,
        channels: AuditChannelIds,
        guild: &str,
    ) -> Wired<M> {
        let configured = [&channels.audit, &channels.voice, &channels.moderation]
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        let config = MirrorConfig {
            channels: channels.clone(),
            configured,
            mirror_guild_id: Some(guild.to_owned()),
            bot_user_id: bot_user_id.clone(),
        };
        Wired {
            store: AuditStore::new(&pool),
            service: AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config),
            pool,
            mirror,
            bot_user_id,
            channels,
        }
    }

    async fn wired(&self) -> Result<Arc<Wired<M>>, ErrorClass> {
        self.refresh_channels_with(crate::settings_jobs::live().as_ref());
        // Refresh outside the slot lock so a concurrent channel change lands
        // on the next call instead of racing this one.
        let current = self
            .channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut slot = self.wired.lock().await;
        match slot.as_ref() {
            Some(wired) if wired.channels == current => {}
            Some(wired) => {
                let guild = self.guild.clone();
                *slot = Some(Arc::new(Self::build_wired(
                    wired.pool.clone(),
                    wired.mirror.clone(),
                    wired.bot_user_id.clone(),
                    current,
                    &guild,
                )));
            }
            None => {
                let Parts {
                    pool,
                    mirror,
                    bot_user_id,
                } = (self.connect)().await?;
                let guild = self.guild.clone();
                *slot = Some(Arc::new(Self::build_wired(
                    pool,
                    mirror,
                    bot_user_id,
                    current,
                    &guild,
                )));
            }
        }
        Ok(slot.as_ref().expect("wired set above").clone())
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
    /// Every readable halt also sets `two_bot_audit_delivery_halt` (1 while
    /// engaged, 0 once cleared) so the kill-switch state is visible to
    /// staleness evaluation; an unreadable halt keeps the last reported
    /// state instead of fabricating a clear.
    pub(crate) async fn sweep(&self) -> Result<Sweep, ErrorClass> {
        let wired = self.wired().await?;
        let Ok(halt) = wired.store.delivery_halt().await else {
            tracing::warn!(job = JOB, "audit_retry_halt_unreadable");
            return Err(ErrorClass::Database);
        };
        two_bot_core::metrics::global().set_audit_delivery_halt(halt.is_some());
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
