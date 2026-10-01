use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn job(name: &'static str, action: JobAction) -> Job {
    Job {
        name,
        cadence: Duration::from_secs(10),
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
    task.await.unwrap();
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
    task.await.unwrap();
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
    task.await.unwrap();
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
    task.await.unwrap();
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
    task.await.unwrap();
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
        task.await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let current = &status.read().await["good"];
        assert_eq!(current.last_start, None);
        assert!(!current.running);
    }
}

#[tokio::test(start_paused = true)]
async fn every_registered_job_reports_success_in_seconds() {
    let names: Vec<_> = crate::website_jobs::NAMES
        .into_iter()
        .chain(crate::community_jobs::NAMES)
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
    assert!(!text.contains("secret"));
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
        .await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}
