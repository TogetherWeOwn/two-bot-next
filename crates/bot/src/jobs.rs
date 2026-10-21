//! Fixed-phase periodic jobs. Busy deadlines are discarded, never queued.

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use serde::Serialize;
use tokio::{
    sync::{watch, RwLock},
    task::{JoinHandle, JoinSet},
    time::{Instant, MissedTickBehavior},
};
use two_bot_core::metrics::{self, Metrics};

pub use two_bot_core::metrics::JobErrorClass as ErrorClass;

pub type JobFuture = Pin<Box<dyn Future<Output = Result<(), ErrorClass>> + Send>>;
pub type JobAction = Arc<dyn Fn() -> JobFuture + Send + Sync>;
/// Live cadence reader for jobs whose period can change without a restart.
/// Must be cheap and non-blocking: the supervisor calls it on its own lane.
pub type CadenceSource = Arc<dyn Fn() -> Duration + Send + Sync>;
pub type SharedStatus = Arc<RwLock<BTreeMap<String, JobStatus>>>;

#[derive(Debug, Clone, Default, Serialize)]
pub struct JobStatus {
    pub parked: bool,
    pub running: bool,
    /// Unix epoch milliseconds (not the monotonic scheduling clock).
    pub last_start: Option<u64>,
    pub last_success: Option<u64>,
    pub last_error_class: Option<ErrorClass>,
    pub consecutive_failures: u64,
}

pub struct Job {
    pub name: &'static str,
    /// Boot cadence: the supervisor's period at startup, and the fallback
    /// whenever [`Job::cadence_source`] is absent or reports no change.
    pub cadence: Duration,
    /// Optional live cadence. When set, the supervisor re-reads it at every
    /// tick and on a bounded recheck while waiting; a changed value
    /// reschedules the next tick to last-run + new cadence (one prompt run
    /// if that instant is already past, never a catch-up burst), an
    /// unchanged value preserves the fixed phase. A zero value is ignored.
    pub cadence_source: Option<CadenceSource>,
    pub startup_jitter: Duration,
    pub timeout: Duration,
    pub action: JobAction,
}

/// How often the supervisor re-reads a dynamic job's cadence source while
/// waiting for its next tick, so a decrease below the pending slot is
/// observed promptly instead of at the old deadline.
const CADENCE_RECHECK: Duration = Duration::from_secs(5);

pub fn statuses(names: &[&str], parked: bool) -> SharedStatus {
    Arc::new(RwLock::new(
        names
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    JobStatus {
                        parked,
                        ..JobStatus::default()
                    },
                )
            })
            .collect(),
    ))
}

/// A supervised job whose name has no status entry. Fixed vocabulary naming
/// the job, so a registration mismatch reads as one greppable line instead of
/// a supervisor-task panic. Constructed only on the cold error path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnregisteredJob {
    pub name: &'static str,
}

impl std::fmt::Display for UnregisteredJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unregistered job: {}", self.name)
    }
}

impl std::error::Error for UnregisteredJob {}

/// Every job name must resolve to a status entry before any task spawns. The
/// first missing name wins; the healthy path is unchanged. [`supervise`] and
/// [`crate::website_jobs::serve_jobs`] both call this at spawn time, which is
/// what makes the status lookups in [`record_completion`], the dispatch tick
/// and the unpark loop unreachable on a missing entry: the status map is
/// created once at boot and entries are never removed at runtime.
pub fn check_registered(
    jobs: &[Job],
    statuses: &BTreeMap<String, JobStatus>,
) -> Result<(), UnregisteredJob> {
    for job in jobs {
        if !statuses.contains_key(job.name) {
            return Err(UnregisteredJob { name: job.name });
        }
    }
    Ok(())
}

/// Bounded startup offset in [0, min(cadence, 5 seconds)]. The caller samples
/// once per job, so all later deadlines keep that phase, without cadence drift.
pub fn startup_jitter(cadence: Duration, sample: u64) -> Duration {
    let bound = cadence.min(Duration::from_secs(5)).as_millis() as u64;
    Duration::from_millis(sample % (bound + 1))
}

pub async fn supervise(
    jobs: Vec<Job>,
    status: SharedStatus,
    shutdown: watch::Receiver<bool>,
) -> Result<(), UnregisteredJob> {
    // Fail fast before any task spawns: a name mismatch is a registration bug,
    // not a runtime outcome, and must never surface as a supervisor-task panic.
    // The guard is scoped: the read lock must be released before tasks spawn,
    // or their first status write would block on it forever.
    {
        let statuses = status.read().await;
        check_registered(&jobs, &statuses)?;
    }
    let mut tasks = JoinSet::new();
    for job in jobs {
        assert!(!job.cadence.is_zero(), "job cadence must be nonzero");
        tasks.spawn(run_job(
            job,
            Arc::clone(&status),
            shutdown.clone(),
            metrics::global(),
        ));
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

async fn record_completion(
    name: &str,
    status: &SharedStatus,
    metrics: &Metrics,
    result: Result<(), ErrorClass>,
) {
    let mut statuses = status.write().await;
    // Spawn-time validation (`check_registered`, called by `supervise`) proves
    // every supervised name resolves, and entries are never removed at runtime.
    let Some(current) = statuses.get_mut(name) else {
        unreachable!("supervise validated every job name at spawn: {name}");
    };
    current.running = false;
    match result {
        Ok(()) => {
            let completed = timestamp();
            current.last_success = Some(completed);
            current.last_error_class = None;
            current.consecutive_failures = 0;
            metrics.job_success(name, completed / 1_000);
        }
        Err(class) => {
            current.last_error_class = Some(class);
            current.consecutive_failures = current.consecutive_failures.saturating_add(1);
            metrics.job_failure(name, class);
            tracing::warn!(job = name, error_class = ?class, "periodic job failed");
        }
    }
}

/// The source's cadence when it moved away from `current`; updates `current`
/// in that case. Absent source, zero and unchanged values are all "no move",
/// so a static job's fixed phase is never disturbed.
fn refreshed_cadence(source: &Option<CadenceSource>, current: &mut Duration) -> Option<Duration> {
    let next = source.as_ref()?();
    if next.is_zero() || next == *current {
        return None;
    }
    *current = next;
    Some(next)
}

fn cadence_interval(first_tick: Instant, cadence: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval_at(first_tick, cadence);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval
}

async fn run_job(
    job: Job,
    status: SharedStatus,
    mut shutdown: watch::Receiver<bool>,
    metrics: &Metrics,
) {
    let first_deadline = Instant::now() + job.startup_jitter;
    let mut cadence = job.cadence;
    let mut interval = cadence_interval(first_deadline, cadence);
    let mut recheck = cadence_interval(first_deadline + CADENCE_RECHECK, CADENCE_RECHECK);
    // Scheduled instant of the tick that started the latest run: the anchor
    // a reschedule counts the new cadence from. Before the first run, a
    // changed cadence applies from the first tick instead of moving it.
    let mut last_run_tick: Option<Instant> = None;
    let mut active: Option<JoinHandle<Result<(), ErrorClass>>> = None;
    let mut completed_at = None;
    // A live-cadence decrease that becomes due while an action runs cannot
    // fire immediately (no overlap): remember one pending prompt run and
    // re-arm it when the action completes, instead of deferring by a full
    // new cadence. A single flag is enough because Skip collapses bursts.
    let mut pending_live_run = false;
    loop {
        tokio::select! {
            biased;
            _ = stopped(&mut shutdown) => break,
            result = async { active.as_mut().expect("guarded active task").await }, if active.is_some() => {
                active = None;
                completed_at = Some(Instant::now());
                if pending_live_run {
                    pending_live_run = false;
                    interval = cadence_interval(Instant::now(), cadence);
                }
                record_completion(job.name, &status, metrics, result.unwrap_or(Err(ErrorClass::Panic))).await;
            }
            deadline = interval.tick() => {
                if active.is_some() || completed_at.is_some_and(|end| deadline < end) { continue; }
                let now = Instant::now();
                let mut run_tick = deadline;
                if let Some(next) = refreshed_cadence(&job.cadence_source, &mut cadence) {
                    if let Some(last_run) = last_run_tick {
                        let due = last_run + next;
                        if due > now {
                            // This is an obsolete tick from the old cadence.
                            // Do not run early just because the old interval
                            // became ready before the cadence recheck.
                            interval = cadence_interval(due, next);
                            continue;
                        }
                        // The new interval was already due. Run once now and
                        // anchor the next tick to this prompt run, not the
                        // stale timer deadline.
                        run_tick = now;
                        interval = cadence_interval(now + next, next);
                    } else {
                        // Keep the first run immediate; use the new value for
                        // the cadence after it.
                        interval = cadence_interval(deadline + next, next);
                    }
                }
                let mut statuses = tokio::select! {
                    biased;
                    _ = stopped(&mut shutdown) => break,
                    statuses = status.write() => statuses,
                };
                // Cancellation may have arrived while the lock became available.
                if *shutdown.borrow() || shutdown.has_changed().is_err() { break; }
                // Spawn-time validation (`check_registered`, called by
                // `supervise`) proves every supervised name resolves, and
                // entries are never removed at runtime.
                let Some(current) = statuses.get_mut(job.name) else {
                    unreachable!("supervise validated every job name at spawn: {}", job.name);
                };
                current.last_start = Some(timestamp());
                current.running = true;
                drop(statuses);
                last_run_tick = Some(run_tick);
                let action = Arc::clone(&job.action);
                let timeout = job.timeout;
                active = Some(tokio::spawn(async move {
                    // Construct inside the task too: a panicking factory is isolated.
                    tokio::time::timeout(timeout, async move { action().await }).await
                        .unwrap_or(Err(ErrorClass::Timeout))
                }));
            }
            _ = recheck.tick(), if job.cadence_source.is_some() => {
                // Between ticks: a changed cadence reschedules the pending
                // tick to last-run + new cadence. If that instant is already
                // past, the run fires once promptly and the phase re-anchors
                // at it (not at the stale grid, which consumers gating on
                // their last actual run would still refuse); Skip collapses
                // any further missed slots, so the job never bursts.
                if let Some(next) = refreshed_cadence(&job.cadence_source, &mut cadence) {
                    let now = Instant::now();
                    let (first, prompt) = last_run_tick.map_or((first_deadline, false), |tick| {
                        let due = tick + next;
                        if due <= now { (now, true) } else { (due, false) }
                    });
                    // An idle prompt fires through the reset interval itself;
                    // only an active slot needs a remembered run for later.
                    pending_live_run = prompt && active.is_some();
                    interval = cadence_interval(first, next);
                }
            }
        }
    }
    if let Some(task) = active {
        task.abort();
        // Abort does not cancel an already-completed attempt. Preserve its
        // outcome when the biased shutdown branch wins over a ready join.
        match task.await {
            Err(error) if error.is_cancelled() => {}
            result => {
                record_completion(
                    job.name,
                    &status,
                    metrics,
                    result.unwrap_or(Err(ErrorClass::Panic)),
                )
                .await;
            }
        }
    }
    if let Some(current) = status.write().await.get_mut(job.name) {
        current.running = false;
    }
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
