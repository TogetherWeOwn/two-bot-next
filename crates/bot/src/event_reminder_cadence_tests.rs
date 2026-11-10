//! The optional urgent watch must yield to the next fixed-phase ordinary tick.

#![cfg(test)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn delayed_final_scan_does_not_discard_the_next_supervised_tick() {
    let epoch = tokio::time::Instant::now();
    let runs = Arc::new(AtomicUsize::new(0));
    let scans = Arc::new(AtomicUsize::new(0));
    let scan_dropped = Arc::new(AtomicBool::new(false));
    let scan_finished = Arc::new(AtomicBool::new(false));
    let action: JobAction = Arc::new({
        let runs = runs.clone();
        let scans = scans.clone();
        let scan_dropped = scan_dropped.clone();
        let scan_finished = scan_finished.clone();
        move || {
            let runs = runs.clone();
            let scans = scans.clone();
            let scan_dropped = scan_dropped.clone();
            let scan_finished = scan_finished.clone();
            Box::pin(async move {
                if runs.fetch_add(1, Ordering::SeqCst) > 0 {
                    return Ok(());
                }
                // Dispatch was delayed two seconds after the fixed T=0 slot.
                // The deadline must still be T=15, not action-start + 15.
                let watch_until = reminder_watch_deadline();
                assert_eq!(watch_until, epoch + Duration::from_secs(14));
                watch_urgent_reminders(watch_until, move |_| {
                    let scan = scans.fetch_add(1, Ordering::SeqCst);
                    let scan_dropped = scan_dropped.clone();
                    let scan_finished = scan_finished.clone();
                    async move {
                        if scan == 11 {
                            // At T=13, a final query/admission or pending probe
                            // would return at T=15.2, across the ordinary tick.
                            let _dropped = Dropped(scan_dropped);
                            tokio::time::sleep(Duration::from_millis(2_200)).await;
                            scan_finished.store(true, Ordering::SeqCst);
                        }
                        Ok((0, true))
                    }
                })
                .await
            })
        }
    });
    let mut registered = job(action);
    registered.startup_jitter = Duration::ZERO;
    let status = jobs::statuses(&NAMES, false);
    // Permit spawn validation's read, but delay dispatch's status write.
    let dispatch_lock = status.read().await;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let supervisor = tokio::spawn(jobs::supervise(vec![registered], status.clone(), rx));
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    drop(dispatch_lock);
    settle().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    for _ in 0..11 {
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
    }
    assert_eq!(scans.load(Ordering::SeqCst), 12);
    assert!(!scan_dropped.load(Ordering::SeqCst));
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert!(scan_dropped.load(Ordering::SeqCst));
    assert!(!scan_finished.load(Ordering::SeqCst));
    assert!(!status.read().await[NAMES[0]].running);
    assert!(status.read().await[NAMES[0]].last_success.is_some());
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(runs.load(Ordering::SeqCst), 2, "T=15 must not be lost");
    assert!(!scan_finished.load(Ordering::SeqCst), "no detached scan");
    stop.send(true).expect("stop supervisor");
    supervisor
        .await
        .expect("supervisor task")
        .expect("registered");
}

#[tokio::test(start_paused = true)]
async fn exhausted_watch_never_polls_a_scan_and_errors_still_propagate() {
    let scans = AtomicUsize::new(0);
    watch_urgent_reminders(tokio::time::Instant::now(), |_| {
        scans.fetch_add(1, Ordering::SeqCst);
        async { Ok((0, true)) }
    })
    .await
    .expect("exhausted optional watch is healthy");
    assert_eq!(scans.load(Ordering::SeqCst), 0);
    assert_eq!(
        watch_urgent_reminders(
            tokio::time::Instant::now() + Duration::from_secs(1),
            |_| async { Err(ErrorClass::Database) },
        )
        .await,
        Err(ErrorClass::Database),
        "a completed scan failure is not a timeout or a successful watch"
    );
}
