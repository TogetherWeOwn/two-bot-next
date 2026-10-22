//! A cleanup counter must not wait for an unrelated metrics writer/scrape.
#![cfg(test)]

use super::Metrics;
use std::sync::{mpsc, Arc};
use std::time::Duration;

#[test]
fn drop_poison_completes_before_the_values_mutex_is_released() {
    let metrics = Arc::new(Metrics::default());
    let held = metrics.0.lock().expect("hold the metrics Values mutex");
    let (started, entered) = mpsc::sync_channel(1);
    let (completed, receipt) = mpsc::sync_channel(1);
    let worker = std::thread::spawn({
        let metrics = Arc::clone(&metrics);
        move || {
            started.send(()).expect("announce counter call");
            metrics.drop_poison("command_dispatch_guard");
            completed.send(()).expect("announce counter completion");
        }
    });

    // Save outcomes, rather than asserting while the worker may be waiting
    // for this lock. The unfixed implementation must fail, not hang the suite.
    let entered = entered.recv_timeout(Duration::from_secs(5));
    let completed_while_held = receipt.recv_timeout(Duration::from_secs(5));
    drop(held);
    let joined = worker.join();

    assert!(entered.is_ok(), "worker reached the counter call");
    assert!(joined.is_ok(), "counter worker must not panic");
    let text = metrics.render(None);
    assert!(text.contains("two_bot_drop_poison_total{guard=\"command_dispatch_guard\"} 1\n"));
    assert!(
        completed_while_held.is_ok(),
        "drop_poison must finish BEFORE the Values mutex is released"
    );
}

#[test]
fn healthy_drop_poison_recording_is_local_to_each_metrics_instance() {
    let first = Metrics::default();
    let second = Metrics::default();
    first.drop_poison("user_slot");
    first.drop_poison("user_slot");
    second.drop_poison("in_flight_guard");
    let first = first.render(None);
    let second = second.render(None);
    assert!(first.contains("two_bot_drop_poison_total{guard=\"user_slot\"} 2\n"));
    assert!(first.contains("two_bot_drop_poison_total{guard=\"in_flight_guard\"} 0\n"));
    assert!(second.contains("two_bot_drop_poison_total{guard=\"user_slot\"} 0\n"));
    assert!(second.contains("two_bot_drop_poison_total{guard=\"in_flight_guard\"} 1\n"));
}
