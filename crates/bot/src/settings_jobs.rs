//! `guild_settings` hot reload: the 15 s version poll on the job supervisor
//! (TOG-10898). Legacy polls the transactional revision every
//! [`POLL_SECONDS`] and swaps `liveCfg` when it moves; here the swap target is
//! a `watch`-published `Arc<SettingsCache>` behind [`live`], so feature
//! runtimes (automod lists, raid thresholds, channel IDs) read the latest
//! snapshot without waiting for database work or cache construction and never
//! see a half-rebuilt cache.
//!
//! Each tick GETs the /ops record first, then the cheap marks query; only a
//! moved revision or row count pays for the consistent snapshot load. A
//! malformed snapshot (empty `guild_id`/`key`, duplicate `(guild_id, key)` —
//! impossible under the table PK but not under a hand-edited or restored row
//! set) is refused whole: the previous snapshot stays published and the tick
//! fails as [`ErrorClass::Configuration`].
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
#[cfg_attr(not(test), allow(dead_code))] // Feature consumers are wired in follow-up slices.
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
        cadence_source: None,
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
        // The /ops override layer polls first and always, independent of
        // pool init: the incident lever must land during a Postgres outage
        // too (the tick still fails as Database below), and a guild
        // early-return must never skip the ops read. Fully-qualified core
        // items keep this file's header line numbers (cited by the
        // configuration reference) unshifted.
        if let Some(fetched) = poll_ops_record().await {
            let mut writer = self.writer.lock().await;
            let outcome = writer.apply_ops(fetched.as_ref().ok(), std::time::Instant::now());
            log_ops(&outcome, fetched.as_ref().err());
        }
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

/// Fetch one /ops record, or `None` when the layer is disabled (no URL).
/// Fetch failures resolve to `Some(Err(..))` so the writer can age the last
/// good record toward expiry; only a missing URL skips the apply.
///
/// The URL is process-global per tick. Threaded lib-test sweeps must run
/// the env-mutating ops acceptance case separately from the supervised
/// guild poll (fixture DDL locks do not isolate test bodies). Nextest uses
/// per-test processes; the authenticated wiring probe also owns a child
/// process so its URL and token cannot leak into another poll.
async fn poll_ops_record(
) -> Option<Result<two_bot_core::settings::OpsRecord, two_bot_core::ops_fetch::OpsFetchError>> {
    let Some(url) = two_bot_core::ops_fetch::ops_url_from_env() else {
        if std::env::var(two_bot_core::ops_fetch::OPS_CONFIG_URL_ENV).is_ok() {
            // Set but off-shape: loud, so a paste error surfaces instead of
            // silently disabling the incident lever.
            tracing::warn!("ops_config_url_invalid");
        }
        return None;
    };
    let token = two_bot_core::ops_fetch::ops_token_from_env();
    Some(
        two_bot_core::ops_fetch::fetch_ops_record(
            &url,
            token.as_deref(),
            two_bot_core::ops_fetch::OPS_FETCH_TIMEOUT,
        )
        .await,
    )
}

/// Spec-named ops lines (Slice 0a): `ops_overrides_applied` per applied
/// epoch with key names only, plus the stale/expiry/fetch-failure lines.
/// Heartbeats and transparent reads stay silent. Values never render.
fn log_ops(
    outcome: &two_bot_core::settings::OpsApplyOutcome,
    fetch_error: Option<&two_bot_core::ops_fetch::OpsFetchError>,
) {
    use two_bot_core::settings::OpsApplyOutcome as Outcome;
    match outcome {
        Outcome::Applied { epoch, keys } => {
            tracing::info!(epoch, keys = ?keys, "ops_overrides_applied");
        }
        Outcome::Heartbeat { .. } | Outcome::Transparent => {}
        Outcome::StaleIgnored { epoch, current } => {
            tracing::warn!(epoch, current, "ops_overrides_stale_epoch");
        }
        Outcome::FetchFailedKeptGood => {
            if let Some(error) = fetch_error {
                // Kind-only: the error carries no URL, token, or values.
                tracing::warn!(error = %error, "ops_overrides_fetch_failed");
            }
        }
        Outcome::Expired => {
            tracing::warn!("ops_overrides_expired");
        }
    }
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
#[path = "settings_consumer_wiring_tests.rs"]
mod consumer_wiring_tests;

#[cfg(test)]
#[path = "settings_jobs_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "settings_trio_live_log_acceptance_tests.rs"]
mod trio_live_log_acceptance_tests;

#[cfg(test)]
#[path = "settings_community_live_log_acceptance_tests.rs"]
mod community_live_log_acceptance_tests;

#[cfg(test)]
mod ops_log_tests {
    use super::*;

    #[tokio::test]
    async fn ops_env_bearer_reaches_authenticated_poll() {
        use two_bot_core::ops_fetch::{OpsFetchError, OPS_CONFIG_TOKEN_ENV, OPS_CONFIG_URL_ENV};

        const CHILD_MODE: &str = "OPS_FETCH_WIRING_MODE";
        const TEST_NAME: &str =
            "settings_jobs::ops_log_tests::ops_env_bearer_reaches_authenticated_poll";
        let Ok(mode) = std::env::var(CHILD_MODE) else {
            // Each env case runs in its own process. No variable is mutated
            // in the threaded parent, including the coverage environment.
            for mode in ["unset", "empty", "set"] {
                let output = tokio::task::spawn_blocking(move || {
                    let mut command = std::process::Command::new(
                        std::env::current_exe().expect("test executable"),
                    );
                    command
                        .args([TEST_NAME, "--exact", "--nocapture"])
                        .env(CHILD_MODE, mode)
                        .env_remove(OPS_CONFIG_TOKEN_ENV);
                    match mode {
                        "empty" => {
                            command.env(OPS_CONFIG_TOKEN_ENV, "");
                        }
                        "set" => {
                            command.env(OPS_CONFIG_TOKEN_ENV, "fixture-token");
                        }
                        _ => {}
                    }
                    command.output().expect("isolated ops poll probe")
                })
                .await
                .expect("probe task");
                assert!(
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                    "isolated ops poll probe ({mode}) failed: {} {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
            return;
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("authenticated ops mock");
        let url = format!(
            "http://{}/ops/config",
            listener.local_addr().expect("mock address")
        );
        let app = axum::Router::new().route(
            "/ops/config",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                use axum::response::IntoResponse;
                if headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    != Some("Bearer fixture-token")
                {
                    return axum::http::StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "epoch": 7,
                    "overrides": {"TWO_AUTOMOD_MENTION_LIMIT": "9"},
                    "deploymentId": "mock-ops-deployment",
                    "baseline": {},
                    "enforcement": "pending",
                }))
                .into_response()
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock serve");
        });
        std::env::set_var(OPS_CONFIG_URL_ENV, url);
        let fetched = poll_ops_record().await.expect("configured ops fetch");
        if mode == "set" {
            let record = fetched.expect("env bearer authenticates the production poll");
            assert_eq!(record.epoch, 7);
            assert_eq!(
                record
                    .overrides
                    .get("TWO_AUTOMOD_MENTION_LIMIT")
                    .map(String::as_str),
                Some("9")
            );
        } else {
            assert_eq!(fetched, Err(OpsFetchError::Status(401)));
        }
        server.abort();
    }

    /// Covers every `log_ops` arm without touching the process environment
    /// or a database (the apply-side semantics are pinned in core and the
    /// tick-level behavior in the protected acceptance module). Log text is
    /// intentionally unpinned; the match only ever renders epochs, key
    /// names, and the kind-only fetch error.
    #[test]
    fn ops_log_lines_cover_every_outcome() {
        use two_bot_core::settings::OpsApplyOutcome as Outcome;
        for outcome in [
            Outcome::Applied {
                epoch: 1,
                keys: vec!["TWO_AUTOMOD_MENTION_LIMIT".to_owned()],
            },
            Outcome::Heartbeat { epoch: 1 },
            Outcome::Transparent,
            Outcome::StaleIgnored {
                epoch: 3,
                current: 5,
            },
            Outcome::FetchFailedKeptGood,
            Outcome::Expired,
        ] {
            log_ops(&outcome, None);
        }
        log_ops(
            &Outcome::FetchFailedKeptGood,
            Some(&two_bot_core::ops_fetch::OpsFetchError::Timeout),
        );
    }
}
