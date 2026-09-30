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
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(supervise(vec![job("pending", action)], status.clone(), rx));
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    assert!(status.read().await["pending"].running);
    stop.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(!status.read().await["pending"].running);
    tokio::time::advance(Duration::from_secs(100)).await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
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
