//! Tests-first contract for the retention runtime, not a retention implementation.
//!
//! The missing `retention_jobs` adapter is an intentional unresolved dependency.
//! See docs/testing/retention-runtime.md for the required adapter surface,
//! runtime wiring, and the limits of the injected-action scheduler tests.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::{watch, Notify};
use two_bot_cutover::retention::RetentionMode;

use crate::{
    jobs::{self, ErrorClass, Job, JobAction},
    retention_jobs::{job, mode_from_setting},
    server, website_jobs,
};

mod aggregate_cursor_early;
mod aggregate_paging;
mod boot;
mod database;

#[test]
fn missing_configuration_is_dry_run_and_only_exact_execute_opts_in() {
    assert_eq!(mode_from_setting(None), Ok(Some(RetentionMode::DryRun)));
    assert_eq!(
        mode_from_setting(Some("dry-run")),
        Ok(Some(RetentionMode::DryRun))
    );
    assert_eq!(
        mode_from_setting(Some("execute")),
        Ok(Some(RetentionMode::Execute))
    );
    assert_eq!(mode_from_setting(Some("off")), Ok(None));
    for invalid in [
        "", "true", "1", "EXECUTE", " execute", "execute ", "unknown",
    ] {
        assert_eq!(
            mode_from_setting(Some(invalid)),
            Err(ErrorClass::Configuration),
            "invalid input must not silently enable destructive work: {invalid:?}"
        );
    }
}

fn fixture_job() -> Job {
    // Lazy and never used: cadence tests replace only the action on the REAL
    // retention registration. No network or environment credential is consulted.
    let pool = PgPoolOptions::new().connect_lazy_with(
        PgConnectOptions::new_without_pgpass()
            .host("agent-testdb")
            .port(5432)
            .username("agent_test")
            .password("")
            .database("two_bot_test_unused"),
    );
    let mut registered = job(pool, RetentionMode::DryRun);
    assert_eq!(registered.name, "retention");
    assert!(registered.cadence >= Duration::from_secs(60));
    assert!(registered.cadence <= Duration::from_secs(3600));
    assert!(!registered.timeout.is_zero());
    assert!(registered.timeout <= registered.cadence);
    assert!(registered.startup_jitter <= Duration::from_secs(5));
    registered.startup_jitter = Duration::ZERO;
    registered
}

async fn settle() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

async fn advance(duration: Duration) {
    tokio::time::advance(duration).await;
    settle().await;
}

fn serve(
    registered: Job,
) -> (
    watch::Sender<bool>,
    jobs::SharedStatus,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let status = jobs::statuses(&[registered.name], false);
    let (stop, rx) = watch::channel(false);
    let owner = tokio::spawn(website_jobs::serve_jobs(
        vec![registered],
        status.clone(),
        stop.clone(),
        async move {
            server::shutdown_requested(rx).await;
            Ok(())
        },
    ));
    (stop, status, owner)
}

#[tokio::test(start_paused = true)]
async fn retention_cadence_retries_errors_only_at_the_next_bounded_opportunity() {
    let mut registered = fixture_job();
    let cadence = registered.cadence;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    registered.action = Arc::new(move || {
        let first = observed.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if first {
                Err(ErrorClass::Database)
            } else {
                Ok(())
            }
        })
    });
    let (stop, status, owner) = serve(registered);
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        status.read().await["retention"].last_error_class,
        Some(ErrorClass::Database)
    );
    assert_eq!(status.read().await["retention"].consecutive_failures, 1);
    advance(cadence - Duration::from_millis(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "no immediate error retry");
    advance(Duration::from_millis(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(status.read().await["retention"].consecutive_failures, 0);
    assert_eq!(status.read().await["retention"].last_error_class, None);
    assert!(status.read().await["retention"].last_success.is_some());
    stop.send_replace(true);
    owner.await.unwrap().unwrap();
    advance(cadence * 10).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

struct InFlight(Arc<AtomicUsize>);
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn retention_busy_ticks_never_overlap_or_catch_up_after_release() {
    let mut registered = fixture_job();
    let cadence = registered.cadence;
    // Stretch the attempt budget ONLY to exercise overrun handling in the same
    // supervisor; fixture_job above independently checks the production bound.
    registered.timeout = cadence * 10;
    let calls = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let action: JobAction = {
        let (calls, active, peak, release) =
            (calls.clone(), active.clone(), peak.clone(), release.clone());
        Arc::new(move || {
            let (calls, active, peak, release) =
                (calls.clone(), active.clone(), peak.clone(), release.clone());
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                let _in_flight = InFlight(active);
                release.notified().await;
                Ok(())
            })
        })
    };
    registered.action = action;
    let (stop, status, owner) = serve(registered);
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    advance(cadence * 3 + cadence / 2).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(peak.load(Ordering::SeqCst), 1);
    release.notify_one();
    settle().await;
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no replay of busy deadlines"
    );
    advance(cadence / 2 - Duration::from_millis(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    advance(Duration::from_millis(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(peak.load(Ordering::SeqCst), 1);
    stop.send_replace(true);
    owner.await.unwrap().unwrap();
    assert_eq!(
        active.load(Ordering::SeqCst),
        0,
        "shutdown joins cancellation; no detached attempt"
    );
    assert!(!status.read().await["retention"].running);
    advance(cadence * 20).await;
    release.notify_one();
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn retention_timeout_cancels_then_retries_without_detached_work() {
    let mut registered = fixture_job();
    let cadence = registered.cadence;
    let budget = registered.timeout;
    let calls = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    registered.action = {
        let (calls, active) = (calls.clone(), active.clone());
        Arc::new(move || {
            let (calls, active) = (calls.clone(), active.clone());
            Box::pin(async move {
                let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                active.fetch_add(1, Ordering::SeqCst);
                let _in_flight = InFlight(active);
                if first {
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        })
    };
    let (stop, status, owner) = serve(registered);
    settle().await;
    advance(budget).await;
    assert_eq!(active.load(Ordering::SeqCst), 0);
    // Equal deadlines can be observed in either task order: if the cadence
    // tick sees the attempt still active, it is skipped, not caught up later.
    if budget < cadence {
        assert_eq!(
            status.read().await["retention"].last_error_class,
            Some(ErrorClass::Timeout)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        advance(cadence - budget).await;
    } else if calls.load(Ordering::SeqCst) == 1 {
        assert_eq!(
            status.read().await["retention"].last_error_class,
            Some(ErrorClass::Timeout)
        );
        advance(cadence - Duration::from_millis(1)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no catch-up after a busy tick"
        );
        advance(Duration::from_millis(1)).await;
    }
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(status.read().await["retention"].last_error_class, None);
    stop.send_replace(true);
    owner.await.unwrap().unwrap();
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn retention_already_stopped_owner_never_starts_work() {
    let mut registered = fixture_job();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    registered.action = Arc::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    });
    let status = jobs::statuses(&[registered.name], false);
    let (stop, _) = watch::channel(true);
    website_jobs::serve_jobs(vec![registered], status.clone(), stop, async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!status.read().await["retention"].running);
}
