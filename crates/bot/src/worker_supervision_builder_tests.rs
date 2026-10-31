#![cfg(test)]
//! Builder coverage for the shared supervised-worker core: the three join
//! outcomes of `raid_runtime::join_on_shutdown`. The protected acceptance
//! file pins the worker drain contract (bounded drain, joined handles, drop
//! counter) and the generation wait; these tests pin the supervisor paths no
//! protected test reaches, offline only.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::sync::watch;

use crate::raid_runtime::join_on_shutdown;

#[path = "worker_production_runtime_tests.rs"]
mod production_runtime_tests;

fn service_shutdown() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
}

async fn await_supervisor(supervisor: tokio::task::JoinHandle<()>, what: &str) {
    tokio::time::timeout(Duration::from_secs(10), supervisor)
        .await
        .expect(what)
        .expect("supervisor task stays healthy");
}

#[tokio::test]
async fn supervisor_joins_a_clean_worker_after_latch() {
    let (shutdown_tx, shutdown_rx) = service_shutdown();
    let latched = Arc::new(AtomicBool::new(false));
    let exited = Arc::new(AtomicBool::new(false));
    let read_latch = Arc::clone(&latched);
    let mark_exit = Arc::clone(&exited);
    // The worker runs until the supervisor latches its drain, then exits.
    let worker = tokio::spawn(async move {
        while !read_latch.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        mark_exit.store(true, Ordering::SeqCst);
    });
    let write_latch = Arc::clone(&latched);
    let supervisor = join_on_shutdown(worker, shutdown_rx, Duration::from_secs(5), move || {
        write_latch.store(true, Ordering::SeqCst);
    });
    shutdown_tx.send(true).expect("latch the service shutdown");
    await_supervisor(supervisor, "supervisor joins the latched worker").await;
    assert!(
        exited.load(Ordering::SeqCst),
        "latched worker runs to completion instead of aborting"
    );
}

#[tokio::test]
async fn supervisor_aborts_a_wedged_worker_after_budget() {
    struct DropGuard(Arc<AtomicBool>);
    impl Drop for DropGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let (shutdown_tx, shutdown_rx) = service_shutdown();
    let aborted = Arc::new(AtomicBool::new(false));
    let mark_drop = Arc::clone(&aborted);
    // A worker that never exits, even latched: the supervisor must abort it.
    let worker = tokio::spawn(async move {
        let _guard = DropGuard(mark_drop);
        std::future::pending::<()>().await;
    });
    let supervisor = join_on_shutdown(worker, shutdown_rx, Duration::from_millis(50), || {});
    shutdown_tx.send(true).expect("latch the service shutdown");
    await_supervisor(supervisor, "supervisor gives up after its budget").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !aborted.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("wedged worker is aborted, dropping its future");
}

#[tokio::test]
async fn supervisor_reaps_a_panicked_worker() {
    let (shutdown_tx, shutdown_rx) = service_shutdown();
    let worker = tokio::spawn(async {
        // Panic inside the drain join so the supervisor takes its error path.
        tokio::time::sleep(Duration::from_millis(20)).await;
        panic!("builder-test worker panic; the supervisor must reap it");
    });
    let supervisor = join_on_shutdown(worker, shutdown_rx, Duration::from_secs(5), || {});
    shutdown_tx.send(true).expect("latch the service shutdown");
    await_supervisor(supervisor, "supervisor reaps the panicked worker").await;
}

#[tokio::test]
async fn drain_queued_closes_the_inbox_so_late_sends_fail_closed() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    sender.send(1u32).await.expect("queue one item");
    let dropped = crate::raid_runtime::drain_queued(&mut receiver);
    assert_eq!(dropped, 1, "queued items are counted");
    assert!(
        matches!(
            sender.try_send(2),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_))
        ),
        "sends racing the drain fail at the observer boundary instead of landing in a dead channel uncounted"
    );
}

#[test]
fn join_budget_covers_the_slowest_healthy_in_flight_path() {
    // Slowest healthy path: the 1.5 s settings read plus one 12 s bounded
    // staff-post delivery (raid_runtime SETTINGS_READ_MAX/DELIVERY_MAX), with
    // headroom for the containment snapshot/removal sequence under a slow
    // Discord. A lower budget aborts healthy work as wedged.
    assert!(
        crate::raid_runtime::WORKER_JOIN_BUDGET >= Duration::from_secs(20),
        "WORKER_JOIN_BUDGET must stay above the slowest healthy in-flight path"
    );
}

#[tokio::test]
async fn service_join_waits_for_every_retained_supervisor() {
    let latch_mark = std::sync::Mutex::new(None);
    let finished = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut releases = Vec::new();
    let mut supervisors = Vec::new();
    let mut handles = Vec::new();
    for _ in 0..3 {
        let (release, released) = tokio::sync::oneshot::channel();
        releases.push(release);
        let finished = Arc::clone(&finished);
        let supervisor = tokio::spawn(async move {
            released.await.expect("release supervisor");
            finished.fetch_add(1, Ordering::SeqCst);
        });
        handles.push(supervisor.abort_handle());
        supervisors.push(supervisor);
    }
    let join = crate::join_worker_supervisors(supervisors, &latch_mark);
    tokio::pin!(join);
    assert!(futures_util::poll!(&mut join).is_pending());
    assert_eq!(finished.load(Ordering::SeqCst), 0);
    for release in releases {
        release.send(()).expect("supervisor still retained");
    }
    tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .expect("service reaps all released supervisors");
    assert_eq!(finished.load(Ordering::SeqCst), 3);
    assert!(handles.iter().all(tokio::task::AbortHandle::is_finished));
}

#[tokio::test]
async fn service_join_reaps_panic_and_continues_to_next_supervisor() {
    use tracing::instrument::WithSubscriber;

    if crate::tuning_test_support::isolated_log_test() {
        return;
    }
    let logs = crate::tuning_test_support::Capture::default();
    let finished = Arc::new(AtomicBool::new(false));
    let mark_finished = Arc::clone(&finished);
    let panic = tokio::spawn(async {
        panic!("synthetic supervisor panic");
    });
    let panic_handle = panic.abort_handle();
    let (release, released) = tokio::sync::oneshot::channel();
    let healthy = tokio::spawn(async move {
        released.await.expect("release healthy supervisor");
        mark_finished.store(true, Ordering::SeqCst);
    });
    let healthy_handle = healthy.abort_handle();
    let latch_mark = std::sync::Mutex::new(Some(std::time::Instant::now()));
    async {
        let join = crate::join_worker_supervisors(vec![panic, healthy], &latch_mark);
        tokio::pin!(join);
        assert!(futures_util::poll!(&mut join).is_pending());
        release
            .send(())
            .expect("healthy supervisor remains retained");
        tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .expect("panic must not prevent later joins");
    }
    .with_subscriber(logs.subscriber())
    .await;
    assert!(finished.load(Ordering::SeqCst));
    assert!(panic_handle.is_finished());
    assert!(healthy_handle.is_finished());
    let errors: Vec<_> = logs
        .records()
        .into_iter()
        .filter(|record| record.level == tracing::Level::ERROR)
        .collect();
    assert_eq!(errors.len(), 1, "one bounded panic diagnostic");
    assert_eq!(errors[0].fields["panicked"], "true");
}

#[tokio::test]
async fn service_join_expired_budget_aborts_and_drops_each_pending_task() {
    use tracing::instrument::WithSubscriber;

    if crate::tuning_test_support::isolated_log_test() {
        return;
    }
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let logs = crate::tuning_test_support::Capture::default();
    let mut supervisors = Vec::new();
    let mut handles = Vec::new();
    let mut drops = Vec::new();
    for _ in 0..2 {
        let drop_witness = Arc::new(AtomicBool::new(false));
        let dropped = Arc::clone(&drop_witness);
        let (started, running) = tokio::sync::oneshot::channel();
        let supervisor = tokio::spawn(async move {
            let _guard = Dropped(dropped);
            started.send(()).expect("task start observed");
            std::future::pending::<()>().await;
        });
        running.await.expect("task has entered its pending work");
        handles.push(supervisor.abort_handle());
        supervisors.push(supervisor);
        drops.push(drop_witness);
    }
    // This task has already finished before the deadline is checked. Even
    // with zero budget it must be reaped rather than classified as overdue.
    let complete = tokio::spawn(async {});
    let complete_handle = complete.abort_handle();
    while !complete_handle.is_finished() {
        tokio::task::yield_now().await;
    }
    supervisors.insert(0, complete);
    let expired =
        std::time::Instant::now() - (crate::shutdown::deadline() + Duration::from_secs(1));
    let latch_mark = std::sync::Mutex::new(Some(expired));
    tokio::time::timeout(
        Duration::from_secs(5),
        crate::join_worker_supervisors(supervisors, &latch_mark).with_subscriber(logs.subscriber()),
    )
    .await
    .expect("expired service budget must not grant fresh worker budgets");
    for witness in drops {
        assert!(
            witness.load(Ordering::SeqCst),
            "pending task is dropped before service join returns"
        );
    }
    assert!(complete_handle.is_finished());
    assert!(handles.iter().all(tokio::task::AbortHandle::is_finished));
    let errors: Vec<_> = logs
        .records()
        .into_iter()
        .filter(|record| record.level == tracing::Level::ERROR)
        .collect();
    assert_eq!(
        errors.len(),
        2,
        "only the two pending tasks exceed the budget"
    );
    for error in errors {
        assert_eq!(error.fields["event"], "worker_supervisor");
        assert_eq!(error.fields["error_class"], "worker_shutdown_join_exceeded");
        assert_eq!(error.fields["budget_ms"], "0");
    }
}
