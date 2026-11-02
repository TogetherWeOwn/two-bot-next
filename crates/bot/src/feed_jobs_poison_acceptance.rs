//! Acceptance: `ScheduleRun` releases on a poisoned lock instead of panicking.
//!
//! Contract under test: dropping the run guard whose schedule mutex is
//! poisoned still finishes the schedule instead of panicking inside `drop`,
//! and emits the fixed-vocabulary
//! `two_bot_drop_poison_total{guard="schedule_run"}` counter. The healthy
//! drop keeps its exact behavior (finish on drop, including aborted runs).
//!
//! RED on unfixed code: the poisoned drop panics (caught, never aborting the
//! runner) and the counter series does not render. GREEN on a correct
//! implementation: all three tests pass.
//!
//! State is asserted behaviorally through `begin`, so no schedule internals
//! are pinned: a finished schedule admits its next slot, a running one
//! refuses re-entry.
#![cfg(test)]

use std::sync::{Arc, Mutex};

use two_bot_core::feeds::FeedPollSchedule;

use super::ScheduleRun;

const GUARD: &str = "schedule_run";
const INTERVAL_SECS: u64 = 60;
const FIRST_BEGIN_MS: i64 = 0;

/// Poison `mutex` via a caught holder panic. The panic never escapes: the
/// runner only sees the poisoned lock.
fn poison<T>(mutex: &Mutex<T>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = mutex.lock().expect("holder takes the healthy lock");
        std::panic::panic_any("poison the schedule lock");
    }));
    assert!(mutex.is_poisoned(), "holder must leave the lock poisoned");
}

/// Rendered `two_bot_drop_poison_total{guard="..."}` count, or `None` while
/// the series does not exist. Global counters are monotonic, so `>= 1` after
/// this test's own poisoned drop is order- and parallel-safe: only this
/// guard's label is read.
fn drop_poison_count(guard: &str) -> Option<u64> {
    let rendered = two_bot_core::metrics::global().render(None);
    let prefix = format!("two_bot_drop_poison_total{{guard=\"{guard}\"}} ");
    rendered.lines().find_map(|line| {
        line.strip_prefix(prefix.as_str())?
            .trim()
            .parse::<u64>()
            .ok()
    })
}

/// A schedule that has begun its first run, mirroring the supervisor path
/// (`begin` succeeds, then the run guard is held).
fn begun_schedule() -> Arc<Mutex<FeedPollSchedule>> {
    let schedule = Arc::new(Mutex::new(
        FeedPollSchedule::new(INTERVAL_SECS).expect("valid interval"),
    ));
    assert!(
        schedule.lock().expect("healthy lock").begin(FIRST_BEGIN_MS),
        "first begin takes the idle schedule"
    );
    schedule
}

fn next_slot_ms() -> i64 {
    FIRST_BEGIN_MS + INTERVAL_SECS as i64 * 1000
}

#[test]
fn healthy_schedule_run_drop_finishes() {
    let schedule = begun_schedule();
    assert!(
        !schedule.lock().expect("healthy lock").begin(next_slot_ms()),
        "running schedule refuses re-entry before the drop"
    );
    drop(ScheduleRun(Arc::clone(&schedule)));
    assert!(
        schedule.lock().expect("healthy lock").begin(next_slot_ms()),
        "dropped run finishes the schedule"
    );
}

#[test]
fn poisoned_schedule_run_drop_releases_instead_of_panicking() {
    let schedule = begun_schedule();
    poison(&schedule);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(ScheduleRun(Arc::clone(&schedule)));
    }));
    assert!(
        outcome.is_ok(),
        "poisoned ScheduleRun drop must release, not panic"
    );
    assert!(
        schedule
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .begin(next_slot_ms()),
        "poisoned drop still finishes the schedule"
    );
}

#[test]
fn poisoned_schedule_run_drop_emits_fixed_vocabulary_counter() {
    let schedule = begun_schedule();
    poison(&schedule);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(ScheduleRun(Arc::clone(&schedule)));
    }));
    let count = drop_poison_count(GUARD).expect("poison counter series must render");
    assert!(
        count >= 1,
        "poisoned drop must emit {GUARD} counter, saw {count}"
    );
}
