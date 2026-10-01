//! Runtime adapters for the three community jobs (docs/community-jobs.md):
//! the hourly presence probe, the Monday 06:15 UTC community scorecard tick,
//! and the hourly inactivity sweep. Registration honors the legacy env gates:
//! `TWO_PRESENCE_PROBE` on unless `0`, `TWO_COMMUNITY_SCORECARD` off unless `1`,
//! `TWO_INACTIVITY_DAYS` defaulting to 14. A gated-off or misconfigured job
//! produces no [`Job`]; its name comes back parked for the readyz status map
//! and it writes nothing.
//!
//! The scorecard consumes each Monday attempt before the run starts, so one
//! process fires at most once per ISO week. A restart re-fires the tick but
//! `community_scorecard_runs` claims the same idempotency key, so the second
//! run is reused rather than duplicated. Inactivity is read-only by
//! construction: the outcome type carries no channel/message/DM field.

use std::{collections::HashMap, sync::Arc, time::Duration};

use sqlx::PgPool;
use tokio::sync::Mutex;
use two_bot_core::community_store::{
    mark_stream_coverage, run_closed_week, CommunityStoreError, ScorecardLoad,
};
use two_bot_core::{
    bot_floor_due, decide_probe_cycle, format_iso_millis, inactivity_store, now_iso,
    parse_inactivity_days, parse_iso_millis, presence_store, previous_closed_week,
    sanitize_presence_count, scorecard_tick, ClassifierConfig, ScorecardGates,
    BOT_FLOOR_MAX_AGE_MS, COMMUNITY_FACT_TYPES, INACTIVITY_SWEEP_INTERVAL_MS,
    PRESENCE_PROBE_INTERVAL_MS, SCORECARD_TICK_INTERVAL_MS,
};
use two_bot_discord::executor::ActionExecutor;

use crate::{
    jobs::{self, ErrorClass, Job},
    website_jobs::{bot_floor_scan, get, Context},
};

pub const NAMES: [&str; 3] = ["presence_probe", "community_scorecard", "inactivity"];

#[cfg(test)]
#[path = "community_jobs_tests.rs"]
mod tests;

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    PresenceProbe,
    Scorecard,
    Inactivity,
}

/// Cadences are fixed in the deployed binary; time-controlled tests drive
/// [`run_once`] with an injected `now_ms` instead of the supervisor clock.
fn cadence(kind: Kind) -> Duration {
    Duration::from_millis(match kind {
        Kind::PresenceProbe => PRESENCE_PROBE_INTERVAL_MS,
        Kind::Scorecard => SCORECARD_TICK_INTERVAL_MS,
        Kind::Inactivity => INACTIVITY_SWEEP_INTERVAL_MS,
    })
}

/// Process-fixed job state, resolved once at registration so a mid-run env
/// edit cannot split a week's behavior.
pub(crate) struct State {
    /// Fact-capture start, stamped at registration (legacy `captureStartedAt`).
    /// A process that booted mid-week honestly reports partial coverage, which
    /// is what makes a mid-week start fail closed with `INGESTION_INCOMPLETE`.
    capture_started_at: String,
    classifier_version: String,
    /// Scorecard gates; unreachable when the scorecard job is parked.
    gates: ScorecardGates,
    /// `TWO_INACTIVITY_DAYS`; unreachable when the inactivity job is parked.
    inactivity_days: u64,
    /// `YYYY-MM-DD` of the consumed Monday attempt. `scorecard_tick` enforces
    /// at-most-once per ISO week per process; the runs-table claim dedupes
    /// restarts.
    last_attempted_week: Mutex<Option<String>>,
}

/// A job the env gates refuse to register.
pub(crate) struct Parked {
    pub name: &'static str,
    /// Fixed identifier only: "disabled" for a gate, "invalid_config" for a
    /// value that failed to parse. Never the raw env value.
    pub reason: &'static str,
}

/// Resolved env gates, one decision per job.
struct Gates {
    presence: bool,
    scorecard: Option<ScorecardGates>,
    inactivity_days: Option<u64>,
}

fn resolve_gates(vars: &HashMap<String, String>) -> (Gates, Vec<Parked>) {
    let mut parked = Vec::new();
    let presence = vars.get("TWO_PRESENCE_PROBE").is_none_or(|v| v != "0");
    if !presence {
        parked.push(Parked {
            name: "presence_probe",
            reason: "disabled",
        });
    }
    let scorecard = match ScorecardGates::from_map(vars) {
        Ok(gates) if gates.enabled => Some(gates),
        Ok(_) => {
            parked.push(Parked {
                name: "community_scorecard",
                reason: "disabled",
            });
            None
        }
        Err(_) => {
            parked.push(Parked {
                name: "community_scorecard",
                reason: "invalid_config",
            });
            None
        }
    };
    let inactivity_days =
        match parse_inactivity_days(vars.get("TWO_INACTIVITY_DAYS").map(String::as_str)) {
            Ok(days) => Some(days),
            Err(_) => {
                parked.push(Parked {
                    name: "inactivity",
                    reason: "invalid_config",
                });
                None
            }
        };
    (
        Gates {
            presence,
            scorecard,
            inactivity_days,
        },
        parked,
    )
}

/// What registration produced: supervised jobs plus the gated names the caller
/// must mark parked in the readyz status map.
pub(crate) struct Registration {
    pub jobs: Vec<Job>,
    pub parked: Vec<&'static str>,
}

/// Read the env gates once and build the supervised jobs.
pub(crate) fn register(context: Arc<Context>) -> Registration {
    let (gates, parked) = resolve_gates(&std::env::vars().collect());
    let mut parked_names = Vec::with_capacity(parked.len());
    for Parked { name, reason } in parked {
        match reason {
            "invalid_config" => {
                tracing::warn!(job = name, reason, "job_disabled");
            }
            _ => {
                tracing::info!(job = name, reason, "job_disabled");
            }
        }
        parked_names.push(name);
    }
    let state = Arc::new(State {
        capture_started_at: now_iso(),
        classifier_version: ClassifierConfig::from_env().version,
        // Parked jobs never read these; the fallbacks are unreachable.
        gates: gates.scorecard.unwrap_or(ScorecardGates {
            enabled: false,
            recommendations_enabled: false,
            correction_cycles: 0,
        }),
        inactivity_days: gates.inactivity_days.unwrap_or(0),
        last_attempted_week: Mutex::new(None),
    });
    let mut out = Vec::new();
    for (name, kind, enabled) in [
        ("presence_probe", Kind::PresenceProbe, gates.presence),
        (
            "community_scorecard",
            Kind::Scorecard,
            gates.scorecard.is_some(),
        ),
        (
            "inactivity",
            Kind::Inactivity,
            gates.inactivity_days.is_some(),
        ),
    ] {
        if !enabled {
            continue;
        }
        let context = context.clone();
        let state = state.clone();
        let cadence = cadence(kind);
        out.push(Job {
            name,
            cadence,
            startup_jitter: jobs::startup_jitter(cadence, rand::random()),
            timeout: Duration::from_secs(120),
            action: Arc::new(move || {
                let context = context.clone();
                let state = state.clone();
                Box::pin(async move {
                    run_once(
                        kind,
                        context.pool().await?,
                        &context.rest,
                        &context.guild,
                        &state,
                        now_ms(),
                    )
                    .await
                })
            }),
        });
    }
    Registration {
        jobs: out,
        parked: parked_names,
    }
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// One supervised attempt with the clock injected. Each job holds its own
/// lane: none of the three shares the website denominator lock.
pub(crate) async fn run_once(
    kind: Kind,
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    state: &State,
    now_ms: i64,
) -> Result<(), ErrorClass> {
    match kind {
        Kind::PresenceProbe => presence_tick(pool, rest, guild, now_ms).await,
        Kind::Scorecard => scorecard_once(pool, guild, state, now_ms).await,
        Kind::Inactivity => inactivity_tick(pool, state, now_ms).await,
    }
}

/// One probe cycle (legacy `runProbeCycle`). A transport failure is a failed
/// attempt; an unusable count is a successful attempt that writes nothing —
/// the series reads as "the times we successfully looked". A failed member
/// listing keeps the presence row with a NULL floor rather than losing the
/// cycle.
async fn presence_tick(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    now_ms: i64,
) -> Result<(), ErrorClass> {
    let counts = get(rest, &format!("/guilds/{guild}?with_counts=true")).await?;
    // Sanitize before the roster read: a missing/negative count is a failed
    // read and consumes no bot-floor listing budget.
    let presence = sanitize_presence_count(counts["approximate_presence_count"].as_i64());
    if presence.is_none() {
        // Return before the roster read: a failed presence write consumes no
        // bot-floor listing budget.
        tracing::warn!(job = "presence_probe", "presence_probe_read_failed");
        return Ok(());
    }
    let last_floor_ms = presence_store::last_bot_floor_at(pool, guild)
        .await
        .map_err(|_| ErrorClass::Database)?
        .and_then(|at| parse_iso_millis(&at));
    let fresh_floor = if bot_floor_due(last_floor_ms, now_ms, BOT_FLOOR_MAX_AGE_MS) {
        match bot_floor_scan(rest, guild).await {
            Ok(scan) => Some(scan),
            Err(_) => {
                tracing::warn!(job = "presence_probe", "presence_probe_bot_floor_failed");
                None
            }
        }
    } else {
        None
    };
    let decision = decide_probe_cycle(presence, last_floor_ms, fresh_floor, now_ms);
    let recorded =
        presence_store::record_reading(pool, guild, decision, &format_iso_millis(now_ms))
            .await
            .map_err(|_| ErrorClass::Database)?;
    if recorded {
        tracing::info!(job = "presence_probe", "presence_probe_recorded");
    }
    Ok(())
}

/// Consume this Monday's attempt, returning the week key to run. `None` when
/// out of window or this process already attempted this Monday. The attempt is
/// consumed before the run so a mid-run failure cannot refire inside one boot.
async fn consume_attempt(state: &State, now_ms: i64) -> Option<String> {
    let mut attempted = state.last_attempted_week.lock().await;
    let key = scorecard_tick(now_ms, attempted.as_deref())?;
    *attempted = Some(key.clone());
    Some(key)
}

/// One scorecard tick (legacy `startCommunityScorecardJob`): at most one
/// attempt per ISO week, coverage marked from the process's capture start
/// before scoring, the runs table deduping any restart re-fire.
async fn scorecard_once(
    pool: &PgPool,
    guild: &str,
    state: &State,
    now_ms: i64,
) -> Result<(), ErrorClass> {
    let Some(week_key) = consume_attempt(state, now_ms).await else {
        return Ok(());
    };
    let (week_start, week_end) = previous_closed_week(now_ms);
    let generated_at = format_iso_millis(now_ms);
    for stream in COMMUNITY_FACT_TYPES {
        mark_stream_coverage(
            pool,
            guild,
            stream,
            &state.capture_started_at,
            &week_end,
            &generated_at,
        )
        .await
        .map_err(|_| ErrorClass::Database)?;
    }
    let (_json, reused, alert_emitted) = run_closed_week(
        pool,
        ScorecardLoad {
            guild_id: guild.to_owned(),
            classifier_version: state.classifier_version.clone(),
            week_start,
            week_end,
            watermark: 0,
            generated_at,
            recommendations_enabled: state.gates.recommendations_enabled,
            correction_cycles: state.gates.correction_cycles,
        },
    )
    .await
    .map_err(|error| match error {
        CommunityStoreError::Build(_) | CommunityStoreError::UnknownFactType(_) => {
            ErrorClass::Configuration
        }
        CommunityStoreError::Db(_) => ErrorClass::Database,
    })?;
    tracing::info!(
        job = "community_scorecard",
        week = week_key,
        reused,
        alert_emitted,
        "community_scorecard_completed"
    );
    Ok(())
}

/// One quiet-member sweep (legacy `flagInactive`). Read-only: the outcome
/// carries no channel/message/DM field, so it cannot feed a send path.
async fn inactivity_tick(pool: &PgPool, state: &State, now_ms: i64) -> Result<(), ErrorClass> {
    let outcome =
        inactivity_store::run_sweep(pool, &format_iso_millis(now_ms), state.inactivity_days)
            .await
            .map_err(|_| ErrorClass::Database)?;
    tracing::info!(
        job = "inactivity",
        days = state.inactivity_days,
        flagged = outcome.flagged.len(),
        "inactivity_scan"
    );
    Ok(())
}
