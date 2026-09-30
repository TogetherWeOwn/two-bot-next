//! Fixed-phase periodic jobs. Busy deadlines are discarded, never queued.

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use serde::Serialize;
use tokio::{
    sync::{watch, RwLock},
    task::{JoinHandle, JoinSet},
    time::{Instant, MissedTickBehavior},
};

pub type JobFuture = Pin<Box<dyn Future<Output = Result<(), ErrorClass>> + Send>>;
pub type JobAction = Arc<dyn Fn() -> JobFuture + Send + Sync>;
pub type SharedStatus = Arc<RwLock<BTreeMap<String, JobStatus>>>;

/// Fixed classes only: never expose database URLs, REST bodies or panic payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Database,
    Rest,
    Configuration,
    Timeout,
    Panic,
}

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
    pub cadence: Duration,
    pub startup_jitter: Duration,
    pub timeout: Duration,
    pub action: JobAction,
}

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

/// Bounded startup offset in [0, min(cadence, 5 seconds)]. The caller samples
/// once per job, so all later deadlines keep that phase, without cadence drift.
pub fn startup_jitter(cadence: Duration, sample: u64) -> Duration {
    let bound = cadence.min(Duration::from_secs(5)).as_millis() as u64;
    Duration::from_millis(sample % (bound + 1))
}

pub async fn supervise(jobs: Vec<Job>, status: SharedStatus, shutdown: watch::Receiver<bool>) {
    let mut tasks = JoinSet::new();
    for job in jobs {
        assert!(!job.cadence.is_zero(), "job cadence must be nonzero");
        tasks.spawn(run_job(job, Arc::clone(&status), shutdown.clone()));
    }
    while tasks.join_next().await.is_some() {}
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

async fn run_job(job: Job, status: SharedStatus, mut shutdown: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval_at(Instant::now() + job.startup_jitter, job.cadence);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut active: Option<JoinHandle<Result<(), ErrorClass>>> = None;
    let mut completed_at = None;
    loop {
        tokio::select! {
            biased;
            _ = stopped(&mut shutdown) => break,
            result = async { active.as_mut().expect("guarded active task").await }, if active.is_some() => {
                active = None;
                completed_at = Some(Instant::now());
                let result = result.unwrap_or(Err(ErrorClass::Panic));
                let mut statuses = status.write().await;
                let current = statuses.get_mut(job.name).expect("registered job");
                current.running = false;
                match result {
                    Ok(()) => {
                        current.last_success = Some(timestamp());
                        current.last_error_class = None;
                        current.consecutive_failures = 0;
                    }
                    Err(class) => {
                        current.last_error_class = Some(class);
                        current.consecutive_failures = current.consecutive_failures.saturating_add(1);
                        tracing::warn!(job = job.name, error_class = ?class, "periodic job failed");
                    }
                }
            }
            deadline = interval.tick() => {
                if active.is_some() || completed_at.is_some_and(|end| deadline < end) { continue; }
                let mut statuses = tokio::select! {
                    biased;
                    _ = stopped(&mut shutdown) => break,
                    statuses = status.write() => statuses,
                };
                // Cancellation may have arrived while the lock became available.
                if *shutdown.borrow() || shutdown.has_changed().is_err() { break; }
                let current = statuses.get_mut(job.name).expect("registered job");
                current.last_start = Some(timestamp());
                current.running = true;
                let action = Arc::clone(&job.action);
                let timeout = job.timeout;
                active = Some(tokio::spawn(async move {
                    // Construct inside the task too: a panicking factory is isolated.
                    tokio::time::timeout(timeout, async move { action().await }).await
                        .unwrap_or(Err(ErrorClass::Timeout))
                }));
            }
        }
    }
    if let Some(task) = active {
        task.abort();
        let _ = task.await;
    }
    if let Some(current) = status.write().await.get_mut(job.name) {
        current.running = false;
    }
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
