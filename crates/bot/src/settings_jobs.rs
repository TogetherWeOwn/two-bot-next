//! `guild_settings` hot reload: the 15 s version poll on the job supervisor
//! (TOG-10898). Legacy polls the transactional revision every
//! [`POLL_SECONDS`] and swaps `liveCfg` when it moves; here the swap target is
//! a `watch`-published `Arc<SettingsCache>` behind [`live`], so feature
//! runtimes (automod lists, raid thresholds, channel IDs) read the latest
//! snapshot without lock contention and never see a half-rebuilt cache.
//!
//! Each tick is the cheap marks query first; only a moved revision or row
//! count pays for the consistent snapshot load. A malformed snapshot (empty
//! `guild_id`/`key`, duplicate `(guild_id, key)` — impossible under the table
//! PK but not under a hand-edited or restored row set) is refused whole: the
//! previous snapshot stays published and the tick fails as
//! [`ErrorClass::Configuration`].
//!
//! Applied swaps log `settings_applied {version, keys}` — key names only —
//! plus one `setting_changed` per hot key with from/to, one
//! `settings_restart_required` per cold key (stored, never hot-applied), and
//! one `setting_ignored_not_applied` per env-only/unknown row.

use std::{
    collections::HashSet,
    sync::{Arc, OnceLock},
    time::Duration,
};

use sqlx::PgPool;
use tokio::sync::{Mutex, OnceCell};
use two_bot_core::settings::{
    live_channel, IgnoreReason, LiveSettings, LiveSettingsWriter, RefreshReport, SettingsSnapshot,
    POLL_SECONDS,
};
use two_bot_cutover::settings::SettingsStore;

use crate::jobs::{self, ErrorClass, Job};

pub const NAME: &str = "settings";

/// Bounded attempt budget: far under the 15 s cadence so a wedged poll can
/// never starve the next one.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The process-wide reader published by the first registered poll job.
/// `serve` runs once per process; a second registration keeps the first.
static LIVE: OnceLock<LiveSettings> = OnceLock::new();

/// Latest published settings snapshot for feature runtimes, or `None` while
/// the poll job is unregistered/parked (gateway prerequisites missing). A
/// `Some` reader before the first successful poll sees the empty revision-0
/// cache, so lookups fall through to the environment either way.
#[must_use]
pub fn live() -> Option<LiveSettings> {
    LIVE.get().cloned()
}

/// The supervised job: fixed-phase `POLL_SECONDS` cadence, jittered start,
/// per-attempt [`TIMEOUT`], panic-isolated by the supervisor like every job.
#[must_use]
pub fn job(database_url: &str) -> Job {
    let poll = Arc::new(Poll::new(database_url));
    let cadence = cadence();
    Job {
        name: NAME,
        cadence,
        startup_jitter: jobs::startup_jitter(cadence, rand::random()),
        timeout: TIMEOUT,
        action: Arc::new(move || {
            let poll = Arc::clone(&poll);
            Box::pin(async move { poll.tick().await })
        }),
    }
}

/// Cadence overrides are compiled only into tests, never the deployed binary.
fn cadence() -> Duration {
    #[cfg(test)]
    if let Some(millis) = std::env::var("TWO_TEST_SETTINGS_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
    {
        return Duration::from_millis(millis);
    }
    Duration::from_secs(POLL_SECONDS)
}

struct Poll {
    url: String,
    /// Lazily connected on the first tick so a parked/misconfigured process
    /// never opens a pool.
    pool: OnceCell<PgPool>,
    /// Serialize publish with the writer's mutable refresh state; ticks are
    /// already single-lane via the supervisor, the mutex only orders them.
    writer: Mutex<LiveSettingsWriter>,
}

impl Poll {
    fn new(url: &str) -> Self {
        let (writer, reader) = live_channel();
        let _ = LIVE.set(reader);
        Self {
            url: url.to_owned(),
            pool: OnceCell::new(),
            writer: Mutex::new(writer),
        }
    }

    #[cfg(test)]
    fn with_pool(pool: PgPool) -> (Self, LiveSettings) {
        let (writer, reader) = live_channel();
        let cell = OnceCell::new();
        assert!(cell.set(pool).is_ok(), "fresh OnceCell");
        (
            Self {
                url: String::new(),
                pool: cell,
                writer: Mutex::new(writer),
            },
            reader,
        )
    }

    async fn tick(&self) -> Result<(), ErrorClass> {
        let pool = self
            .pool
            .get_or_try_init(|| async {
                // Runtime is DML-only; the operator migrates before startup.
                let db =
                    two_bot_cutover::connect(&self.url, two_bot_cutover::DB_POOL_MAX_DEFAULT, true)
                        .await
                        .map_err(|_| ErrorClass::Database)?;
                Ok::<_, ErrorClass>(db.pool().clone())
            })
            .await?;
        poll_once(pool, &self.writer).await
    }
}

async fn poll_once(pool: &PgPool, writer: &Mutex<LiveSettingsWriter>) -> Result<(), ErrorClass> {
    let store = SettingsStore::new(pool);
    let (revision, rows) = store.poll_marks().await.map_err(|_| ErrorClass::Database)?;
    {
        let writer = writer.lock().await;
        if !writer.needs_refresh(revision, rows) {
            return Ok(());
        }
    }
    let snapshot = store
        .load_snapshot()
        .await
        .map_err(|_| ErrorClass::Database)?;
    if let Err(reason) = validate_snapshot(&snapshot) {
        // Refuse the whole snapshot: a partial apply could pair a new value
        // with a stale neighbouring row.
        tracing::error!(reason, "settings_snapshot_rejected");
        return Err(ErrorClass::Configuration);
    }
    let report = writer.lock().await.publish(&snapshot);
    log_applied(&report);
    Ok(())
}

/// The PK makes duplicates impossible and NOT NULL keeps the columns
/// present, but nothing stops a hand-edited or restored row from carrying an
/// empty `guild_id`/`key`, and nothing proves the snapshot came from this
/// schema at all. Check before trusting the rebuild.
fn validate_snapshot(snapshot: &SettingsSnapshot) -> Result<(), &'static str> {
    let mut seen = HashSet::with_capacity(snapshot.rows.len());
    for row in &snapshot.rows {
        if row.guild_id.is_empty() || row.key.is_empty() {
            return Err("empty guild_id or key");
        }
        if !seen.insert((&row.guild_id, &row.key)) {
            return Err("duplicate guild_id/key pair");
        }
    }
    Ok(())
}

/// Spec-named change lines (TOG-10898): `settings_applied` once per published
/// swap with key names only — never values of secret-class keys — then the
/// per-class detail lines.
fn log_applied(report: &RefreshReport) {
    if !report.changed {
        return;
    }
    let mut keys: Vec<&str> = report
        .hot
        .iter()
        .chain(report.cold.iter())
        .map(|change| change.key.as_str())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    tracing::info!(version = report.to_revision, keys = ?keys, "settings_applied");
    for change in &report.hot {
        // Hot keys are never secret-class; legacy parity logs the values.
        tracing::info!(
            key = change.key.as_str(),
            guild_id = change.guild_id.as_str(),
            from = change.old.as_deref().unwrap_or("(unset)"),
            to = change.new.as_deref().unwrap_or("(unset)"),
            "setting_changed"
        );
    }
    for change in &report.cold {
        tracing::info!(
            key = change.key.as_str(),
            guild_id = change.guild_id.as_str(),
            "settings_restart_required"
        );
    }
    for ignored in &report.ignored {
        let reason = match ignored.reason {
            IgnoreReason::EnvOnly => "env_only",
            IgnoreReason::Unknown => "unknown",
        };
        tracing::warn!(
            key = ignored.key.as_str(),
            guild_id = ignored.guild_id.as_str(),
            reason,
            "setting_ignored_not_applied"
        );
    }
}

#[cfg(test)]
#[path = "settings_jobs_tests.rs"]
mod tests;
