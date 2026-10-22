//! Acceptance: `UserSlot` releases on a poisoned lock instead of panicking.
//!
//! Contract under test: dropping a slot whose counts mutex is poisoned
//! releases the held count (and preserves unrelated members) instead of
//! panicking inside `drop`, and emits the fixed-vocabulary
//! `two_bot_drop_poison_total{guard="user_slot"}` counter. The healthy drop
//! keeps its exact behavior.
//!
//! RED on unfixed code: the poisoned drop panics (caught, never aborting the
//! runner), the counter series does not render, and the Drop-body scan in
//! `command_runtime_poison_acceptance` flags this guard. GREEN on a correct
//! implementation: all three tests pass.
//!
//! Poison uses the in-repo holder idiom (eventsub snapshot precedent): panic
//! while holding the lock inside `catch_unwind`. A second live slot is
//! deliberately forgotten before asserting, so an unfixed second panic during
//! unwinding cannot abort the suite runner.
#![cfg(test)]

use std::sync::Mutex;

use super::UserSlots;

const GUARD: &str = "user_slot";
const HOLDER: u64 = 7;
const OTHER: u64 = 8;

/// Poison `mutex` via a caught holder panic. The panic never escapes: the
/// runner only sees the poisoned lock.
fn poison<T>(mutex: &Mutex<T>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = mutex.lock().expect("holder takes the healthy lock");
        std::panic::panic_any("poison the slot lock");
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

#[test]
fn healthy_slot_drop_releases_exactly_one_count() {
    let slots = UserSlots::default();
    let first = slots.acquire(HOLDER).expect("first slot");
    let _second = slots.acquire(HOLDER).expect("second slot");
    drop(first);
    assert_eq!(
        slots.0.lock().expect("healthy lock").get(&HOLDER),
        Some(&1),
        "dropping one slot releases exactly one count"
    );
}

#[test]
fn poisoned_slot_drop_releases_instead_of_panicking() {
    let slots = UserSlots::default();
    let slot = slots.acquire(HOLDER).expect("slot under test");
    let other = slots.acquire(OTHER).expect("unrelated slot");
    poison(&slots.0);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(slot)));
    // `other` must never drop while the lock is poisoned: on unfixed code a
    // second panic during this assertion's unwinding would abort the runner.
    std::mem::forget(other);
    assert!(
        outcome.is_ok(),
        "poisoned UserSlot drop must release, not panic"
    );
    let counts = slots
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        counts.get(&HOLDER).is_none(),
        "poisoned drop still releases the held count"
    );
    assert_eq!(
        counts.get(&OTHER),
        Some(&1),
        "poisoned drop preserves unrelated members"
    );
}

#[test]
fn poisoned_slot_drop_emits_fixed_vocabulary_counter() {
    let slots = UserSlots::default();
    let slot = slots.acquire(HOLDER).expect("slot under test");
    poison(&slots.0);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(slot)));
    let count = drop_poison_count(GUARD).expect("poison counter series must render");
    assert!(
        count >= 1,
        "poisoned drop must emit {GUARD} counter, saw {count}"
    );
}
