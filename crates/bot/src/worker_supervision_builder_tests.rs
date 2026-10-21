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
