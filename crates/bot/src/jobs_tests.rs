use super::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn job(name: &'static str, action: JobAction) -> Job {
    Job {
        name,
        cadence: Duration::from_secs(10),
        cadence_source: None,
        startup_jitter: Duration::from_secs(2),
        timeout: Duration::from_secs(30),
        action,
    }
}

fn measured_job(
    job: Job,
    status: SharedStatus,
    shutdown: watch::Receiver<bool>,
    metrics: Arc<Metrics>,
) -> JoinHandle<()> {
    tokio::spawn(async move { run_job(job, status, shutdown, &metrics).await })
}

fn counting(count: &Arc<AtomicUsize>, duration: Duration) -> JobAction {
    let count = Arc::clone(count);
    Arc::new(move || {
        let count = Arc::clone(&count);
        Box::pin(async move {
            count.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(duration).await;
            Ok(())
        })
    })
}

#[tokio::test(start_paused = true)]
async fn cadence_keeps_startup_phase_and_jitter_is_bounded() {
    for cadence in [Duration::from_millis(100), Duration::from_secs(60)] {
        for sample in [0, 1, 100, 5000, 5001, u64::MAX] {
            assert!(startup_jitter(cadence, sample) <= cadence.min(Duration::from_secs(5)));
        }
    }
    let count = Arc::new(AtomicUsize::new(0));
    let status = statuses(&["counter"], false);
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(supervise(
        vec![job("counter", counting(&count, Duration::ZERO))],
        status.clone(),
        rx,
    ));
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(status.read().await["counter"].last_success.is_some());
    tokio::time::advance(Duration::from_secs(9)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn overrunning_job_skips_deadlines_without_overlap_or_catchup() {
    let count = Arc::new(AtomicUsize::new(0));
    let status = statuses(&["slow"], false);
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(supervise(
        vec![job("slow", counting(&count, Duration::from_secs(15)))],
        status,
        rx,
    ));
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    tokio::time::advance(Duration::from_secs(10)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn delayed_supervisor_does_not_replay_a_busy_deadline() {
    let count = Arc::new(AtomicUsize::new(0));
    let status = statuses(&["slow"], false);
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(supervise(
        vec![job("slow", counting(&count, Duration::from_secs(15)))],
        status,
        rx,
    ));
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    // Jump across both the busy deadline and completion before the loop runs.
    tokio::time::advance(Duration::from_secs(16)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(4)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn timeout_cancels_attempt_and_next_success_resets_failure_streak() {
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let action: JobAction = Arc::new(move || {
        let c = c.clone();
        Box::pin(async move {
            if c.fetch_add(1, Ordering::SeqCst) == 0 {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    });
    let status = statuses(&["timeout"], false);
    let (stop, rx) = watch::channel(false);
    let mut timeout_job = job("timeout", action);
    timeout_job.timeout = Duration::from_secs(3);
    let task = tokio::spawn(supervise(vec![timeout_job], status.clone(), rx));
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    settle().await;
    assert_eq!(
        status.read().await["timeout"].last_error_class,
        Some(ErrorClass::Timeout)
    );
    assert_eq!(status.read().await["timeout"].consecutive_failures, 1);
    tokio::time::advance(Duration::from_secs(7)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(status.read().await["timeout"].last_error_class, None);
    assert_eq!(status.read().await["timeout"].consecutive_failures, 0);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn panics_in_future_and_factory_do_not_kill_other_jobs() {
    let count = Arc::new(AtomicUsize::new(0));
    let status = statuses(&["panic", "factory", "good"], false);
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(supervise(
        vec![
            job(
                "panic",
                Arc::new(|| Box::pin(async { panic!("synthetic panic") })),
            ),
            job("factory", Arc::new(|| panic!("synthetic factory panic"))),
            job("good", counting(&count, Duration::ZERO)),
        ],
        status.clone(),
        rx,
    ));
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    for name in ["panic", "factory"] {
        assert_eq!(
            status.read().await[name].last_error_class,
            Some(ErrorClass::Panic)
        );
        assert_eq!(status.read().await[name].consecutive_failures, 1);
    }
    tokio::time::advance(Duration::from_secs(10)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(status.read().await["panic"].consecutive_failures, 2);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_drops_inflight_future_and_starts_no_more_work() {
    struct DropFlag(Arc<AtomicUsize>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicUsize::new(0));
    let d = dropped.clone();
    let action: JobAction = Arc::new(move || {
        let flag = DropFlag(d.clone());
        Box::pin(async move {
            let _flag = flag;
            std::future::pending().await
        })
    });
    let status = statuses(&["pending"], false);
    let metrics = Arc::new(Metrics::default());
    let before = metrics.render(None);
    let (stop, rx) = watch::channel(false);
    let task = measured_job(job("pending", action), status.clone(), rx, metrics.clone());
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    assert!(status.read().await["pending"].running);
    stop.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(!status.read().await["pending"].running);
    assert_eq!(
        metrics.render(None),
        before,
        "shutdown is not a job outcome"
    );
    tokio::time::advance(Duration::from_secs(100)).await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn shutdown_preserves_completed_success_error_timeout_and_panics() {
    for close_channel in [false, true] {
        for case in 0..5 {
            let starts = Arc::new(AtomicUsize::new(0));
            let count = starts.clone();
            let action: JobAction = Arc::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
                if case == 4 {
                    panic!("completed factory panic");
                }
                Box::pin(async move {
                    match case {
                        0 => Ok(()),
                        1 => Err(ErrorClass::Database),
                        2 => std::future::pending().await,
                        _ => panic!("completed future panic"),
                    }
                })
            });
            let status = statuses(&["presence_probe"], false);
            let metrics = Metrics::default();
            let before = metrics.render(None);
            let (stop, rx) = watch::channel(false);
            let mut fixture = job("presence_probe", action);
            fixture.startup_jitter = Duration::ZERO;
            fixture.timeout = Duration::from_secs(3);
            let mut supervisor = std::pin::pin!(run_job(fixture, status.clone(), rx, &metrics));
            // Poll only to start the attempt, then leave the supervisor unpolled
            // until both its join and shutdown branches are ready.
            assert!(futures_util::poll!(supervisor.as_mut()).is_pending());
            settle().await;
            if case == 2 {
                tokio::time::advance(Duration::from_secs(3)).await;
                settle().await;
            }
            assert_eq!(starts.load(Ordering::SeqCst), 1);
            assert!(status.read().await["presence_probe"].running);
            assert_eq!(metrics.render(None), before);
            if !close_channel {
                stop.send(true).unwrap();
            }
            drop(stop);
            supervisor.await;

            let expected_error = match case {
                0 => None,
                1 => Some(ErrorClass::Database),
                2 => Some(ErrorClass::Timeout),
                _ => Some(ErrorClass::Panic),
            };
            let current = &status.read().await["presence_probe"];
            let failures = u64::from(expected_error.is_some());
            let successes = 1 - failures;
            assert!(!current.running);
            assert_eq!(current.last_error_class, expected_error);
            assert_eq!(current.consecutive_failures, failures);
            assert_eq!(current.last_success.is_some(), expected_error.is_none());
            let seconds = current.last_success.unwrap_or_default() / 1_000;
            if expected_error.is_none() {
                assert!(seconds > 0);
            }
            let text = metrics.render(None);
            assert!(text.contains(&format!(
                "two_bot_job_runs_total{{job=\"presence_probe\",outcome=\"success\"}} {successes}\n"
            )));
            assert!(text.contains(&format!(
                "two_bot_job_runs_total{{job=\"presence_probe\",outcome=\"failure\"}} {failures}\n"
            )));
            assert!(text.contains(&format!(
                "two_bot_job_consecutive_failures{{job=\"presence_probe\"}} {failures}\n"
            )));
            assert!(text.contains(&format!(
                "two_bot_job_last_success_timestamp_seconds{{job=\"presence_probe\"}} {seconds}\n"
            )));
            assert_eq!(starts.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_while_waiting_for_status_lock_never_starts_an_attempt() {
    for close_channel in [false, true] {
        let count = Arc::new(AtomicUsize::new(0));
        let status = statuses(&["good"], false);
        let locked = status.read().await;
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(supervise(
            vec![job("good", counting(&count, Duration::ZERO))],
            status.clone(),
            rx,
        ));
        settle().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        if !close_channel {
            stop.send(true).unwrap();
        }
        drop(stop);
        settle().await;
        drop(locked);
        task.await.unwrap().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let current = &status.read().await["good"];
        assert_eq!(current.last_start, None);
        assert!(!current.running);
    }
}

/// Covers every registered supervisor name with an independent metrics label:
/// website/community, audit, scheduled-message, feed, self-role recovery,
/// the member unban sweep and automod expiry.
#[tokio::test(start_paused = true)]
async fn every_registered_job_reports_success_in_seconds() {
    let names: Vec<_> = crate::website_jobs::NAMES
        .into_iter()
        .chain(crate::community_jobs::NAMES)
        .chain(crate::audit_runtime::NAMES)
        .chain(crate::scheduled_jobs::NAMES)
        .chain([
            crate::feed_jobs::NAME,
            crate::self_role_handlers::RECOVERY_JOB_NAME,
        ])
        .chain(crate::member_runtime::NAMES)
        .chain([crate::automod_gateway::JOB_NAME])
        .collect();
    let status = statuses(&names, false);
    let metrics = Arc::new(Metrics::default());
    let (stop, rx) = watch::channel(false);
    let mut tasks = Vec::new();
    for name in &names {
        assert!(metrics::JOBS.contains(name), "missing metric label: {name}");
        tasks.push(measured_job(
            job(name, Arc::new(|| Box::pin(async { Ok(()) }))),
            status.clone(),
            rx.clone(),
            metrics.clone(),
        ));
    }
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    let text = metrics.render(None);
    assert!(text.contains("# TYPE two_bot_job_runs_total counter\n"));
    assert!(text.contains("# TYPE two_bot_job_consecutive_failures gauge\n"));
    for name in names {
        let seconds = status.read().await[name].last_success.unwrap() / 1_000;
        assert!(seconds > 0);
        assert!(text.contains(&format!(
            "two_bot_job_runs_total{{job=\"{name}\",outcome=\"success\"}} 1\n"
        )));
        assert!(text.contains(&format!(
            "two_bot_job_runs_total{{job=\"{name}\",outcome=\"failure\"}} 0\n"
        )));
        assert!(text.contains(&format!(
            "two_bot_job_last_success_timestamp_seconds{{job=\"{name}\"}} {seconds}\n"
        )));
        assert!(text.contains(&format!(
            "two_bot_job_consecutive_failures{{job=\"{name}\"}} 0\n"
        )));
    }
    stop.send(true).unwrap();
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn failed_attempts_preserve_last_success_and_next_success_resets_metrics() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let action: JobAction = Arc::new(move || {
        let attempts = attempts.clone();
        Box::pin(async move {
            match attempts.fetch_add(1, Ordering::SeqCst) {
                1 | 2 => Err(ErrorClass::Database),
                _ => Ok(()),
            }
        })
    });
    let status = statuses(&["presence_probe"], false);
    let metrics = Arc::new(Metrics::default());
    let (stop, rx) = watch::channel(false);
    let task = measured_job(
        job("presence_probe", action),
        status.clone(),
        rx,
        metrics.clone(),
    );
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    let first_success = status.read().await["presence_probe"].last_success.unwrap();
    for (successes, failures, streak) in [(1, 1, 1), (1, 2, 2), (2, 2, 0)] {
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        let current = &status.read().await["presence_probe"];
        assert_eq!(current.consecutive_failures, streak);
        if streak > 0 {
            assert_eq!(current.last_success, Some(first_success));
        }
        let text = metrics.render(None);
        assert!(text.contains(&format!(
            "two_bot_job_runs_total{{job=\"presence_probe\",outcome=\"success\"}} {successes}\n"
        )));
        assert!(text.contains(&format!(
            "two_bot_job_runs_total{{job=\"presence_probe\",outcome=\"failure\"}} {failures}\n"
        )));
        assert!(text.contains(&format!(
            "two_bot_job_consecutive_failures{{job=\"presence_probe\"}} {streak}\n"
        )));
        assert!(text.contains(&format!(
            "two_bot_job_last_success_timestamp_seconds{{job=\"presence_probe\"}} {}\n",
            current.last_success.unwrap() / 1_000
        )));
    }
    stop.send(true).unwrap();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn timeout_and_panics_report_failure_with_bounded_labels() {
    let names = ["timeout\"\nsecret=value", "panic", "factory"];
    let status = statuses(&names, false);
    let metrics = Arc::new(Metrics::default());
    let before = metrics.render(None).lines().count();
    let (stop, rx) = watch::channel(false);
    let mut timeout_job = job(names[0], Arc::new(|| Box::pin(std::future::pending())));
    timeout_job.timeout = Duration::from_secs(3);
    let jobs = [
        timeout_job,
        job(names[1], Arc::new(|| Box::pin(async { panic!("fixture") }))),
        job(names[2], Arc::new(|| panic!("factory fixture"))),
    ];
    let tasks: Vec<_> = jobs
        .into_iter()
        .map(|job| measured_job(job, status.clone(), rx.clone(), metrics.clone()))
        .collect();
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    settle().await;
    let text = metrics.render(None);
    assert_eq!(text.lines().count(), before);
    // `invalid_secret` is a legitimate bounded kind: match the hostile
    // `secret=value` injection marker, not the bare substring.
    assert!(!text.contains("secret="));
    assert!(!text.contains("job=\"panic\""));
    assert!(!text.contains("job=\"factory\""));
    assert!(text.contains("two_bot_job_runs_total{job=\"other\",outcome=\"failure\"} 3\n"));
    assert!(text.contains("two_bot_job_runs_total{job=\"other\",outcome=\"success\"} 0\n"));
    assert!(text.contains("two_bot_job_last_success_timestamp_seconds{job=\"other\"} 0\n"));
    assert!(text.contains("two_bot_job_consecutive_failures{job=\"other\"} 3\n"));
    stop.send(true).unwrap();
    for task in tasks {
        task.await.unwrap();
    }
}

fn dynamic_job(name: &'static str, action: JobAction, cadence_secs: &Arc<AtomicU64>) -> Job {
    let source = Arc::clone(cadence_secs);
    Job {
        name,
        cadence: Duration::from_secs(cadence_secs.load(Ordering::SeqCst)),
        cadence_source: Some(Arc::new(move || {
            Duration::from_secs(source.load(Ordering::SeqCst))
        })),
        startup_jitter: Duration::ZERO,
        timeout: Duration::from_secs(30),
        action,
    }
}

/// A changed cadence source reschedules the supervisor's next tick to
/// last-run + new cadence: a decrease lands one new interval after the last
/// run instead of the old 60 s slot, an unchanged source keeps the phase, and
/// an increase pushes the already-pending tick out. No run is duplicated or
/// skipped across a transition.
#[tokio::test(start_paused = true)]
async fn dynamic_cadence_reschedules_the_next_tick_without_restart() {
    let count = Arc::new(AtomicUsize::new(0));
    let cadence_secs = Arc::new(AtomicU64::new(60));
    let status = statuses(&["dynamic"], false);
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(supervise(
        vec![dynamic_job(
            "dynamic",
            counting(&count, Duration::ZERO),
            &cadence_secs,
        )],
        status,
        rx,
    ));
    settle().await;
    // The zero-jitter first tick runs immediately (t=0).
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Unchanged source: the fixed phase holds, the next run is at t=60.
    tokio::time::advance(Duration::from_secs(59)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Decrease at t=59, before the pending t=60 slot: nothing runs until the
    // rescheduled slot would already have passed, so exactly one prompt run
    // fires once the supervisor observes the change, then the 20 s phase
    // anchored at the last run (t=0) continues at t=80.
    cadence_secs.store(20, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2, "one run at the old slot");
    tokio::time::advance(Duration::from_secs(19)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2, "no catch-up burst");
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        3,
        "t=80: last run t=60 + 20 s"
    );

    // Increase at t=80: the pending t=100 tick moves out to t=80 + 100 s.
    cadence_secs.store(100, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(99)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 3, "increase defers the tick");
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        4,
        "t=180: last run t=80 + 100 s"
    );
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

/// A decrease observed long after last-run + new cadence fires one prompt
/// run (no replay of every missed slot), then continues on the new phase.
#[tokio::test(start_paused = true)]
async fn dynamic_cadence_decrease_past_due_runs_once_promptly() {
    let count = Arc::new(AtomicUsize::new(0));
    let cadence_secs = Arc::new(AtomicU64::new(3600));
    let status = statuses(&["dynamic"], false);
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(supervise(
        vec![dynamic_job(
            "dynamic",
            counting(&count, Duration::ZERO),
            &cadence_secs,
        )],
        status,
        rx,
    ));
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1, "first run at t=0");

    // The interval drops to 60 s at t=300; the pending t=3600 slot must not
    // hold the decrease hostage: one run fires promptly on observation.
    tokio::time::advance(Duration::from_secs(300)).await;
    settle().await;
    cadence_secs.store(60, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(30)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2, "one prompt run, no burst");
    // The new phase keeps producing one run per 60 s window thereafter.
    tokio::time::advance(Duration::from_secs(60)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 3);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

/// A decrease that becomes due while the action still runs is remembered and
/// fires promptly when the action completes, instead of waiting a full new
/// cadence. No overlap: the prompt run starts only after completion, and no
/// burst follows.
#[tokio::test(start_paused = true)]
async fn dynamic_cadence_prompt_during_active_runs_after_completion() {
    let count = Arc::new(AtomicUsize::new(0));
    let cadence_secs = Arc::new(AtomicU64::new(3600));
    let status = statuses(&["dynamic"], false);
    let (stop, rx) = watch::channel(false);
    // NOTE: the dynamic fixture times out actions at 30 s, so the long action
    // must stay under that (25 s) to remain active across the change.
    let task = tokio::spawn(supervise(
        vec![dynamic_job(
            "dynamic",
            counting(&count, Duration::from_secs(25)),
            &cadence_secs,
        )],
        status,
        rx,
    ));
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1, "first run at t=0");

    // The 25 s action still holds the slot at t=15; dropping to 10 s makes
    // last-run (t=0) + 10 s already past, so the recheck creates a prompt
    // tick that must not be lost to the active-action guard.
    tokio::time::advance(Duration::from_secs(15)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1, "action still running");
    cadence_secs.store(10, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "prompt tick waits for the active action, no overlap"
    );

    // At t=25 the action completes; the remembered prompt runs immediately
    // instead of deferring to the discarded tick's next slot (t=30).
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "prompt run fires on completion"
    );
    // Skip still collapses bursts: nothing else fires in this window. The
    // prompt run itself holds the slot for 25 s, so the next 10 s tick lands
    // at t=55 (first slot after the prompt action completes).
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2, "no catch-up burst");
    for _ in 0..5 {
        tokio::time::advance(Duration::from_secs(5)).await;
        settle().await;
    }
    assert_eq!(count.load(Ordering::SeqCst), 3, "next slot after prompt");
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn already_stopped_or_closed_signal_never_starts_a_job() {
    for initially_stopped in [false, true] {
        let count = Arc::new(AtomicUsize::new(0));
        let (stop, rx) = watch::channel(initially_stopped);
        drop(stop);
        supervise(
            vec![job("good", counting(&count, Duration::ZERO))],
            statuses(&["good"], false),
            rx,
        )
        .await
        .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn supervise_fails_fast_on_an_unregistered_name() {
    let status = statuses(&["good"], false);
    let (_stop, rx) = watch::channel(false);
    let error = supervise(
        vec![job("ghost", Arc::new(|| Box::pin(async { Ok(()) })))],
        status.clone(),
        rx,
    )
    .await
    .expect_err("an unregistered name fails at spawn, never in a supervisor task");
    assert_eq!(error.to_string(), "unregistered job: ghost");
    assert!(!status.read().await.contains_key("ghost"));
    assert!(
        !status.read().await["good"].running,
        "no task spawns after a registration failure"
    );
}
