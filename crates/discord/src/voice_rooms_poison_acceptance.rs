//! Acceptance: `InFlightGuard` releases on a poisoned lock.
//!
//! Contract under test: dropping the guard whose in-flight-slots mutex is
//! poisoned still releases the attempt slot instead of panicking inside
//! `drop`, and emits the fixed-vocabulary
//! `two_bot_drop_poison_total{guard="in_flight_guard"}` counter. The healthy
//! drop keeps its exact behavior (slot release plus gauge republish).
//!
//! RED on unfixed code: the poisoned drop panics (caught, never aborting the
//! runner) and the counter series does not render. GREEN on a correct
//! implementation: all three tests pass.
//!
//! Note for the implementation: the drop path republishes the rest-age gauge
//! through the same mutex, so the republish must also survive the poisoned
//! lock; a fix that only recovers the slot removal still panics here.
#![cfg(test)]

use std::sync::{Arc, Mutex};

use super::{oldest_age_secs, InFlightRest};

const GUARD: &str = "in_flight_guard";

/// Poison `mutex` via a caught holder panic. The panic never escapes: the
/// runner only sees the poisoned lock.
fn poison<T>(mutex: &Mutex<T>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = mutex.lock().expect("holder takes the healthy lock");
        std::panic::panic_any("poison the in-flight lock");
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
fn healthy_in_flight_drop_releases_the_slot() {
    let slots = Arc::new(Mutex::new(InFlightRest::default()));
    let guard = InFlightRest::begin(&slots);
    assert_eq!(slots.lock().expect("healthy lock").live.len(), 1);
    drop(guard);
    {
        let inner = slots.lock().expect("healthy lock");
        assert!(inner.live.is_empty(), "dropped guard releases its slot");
    }
    assert_eq!(oldest_age_secs(&slots), 0, "empty slots publish zero age");
}

#[test]
fn poisoned_in_flight_drop_releases_instead_of_panicking() {
    let slots = Arc::new(Mutex::new(InFlightRest::default()));
    let guard = InFlightRest::begin(&slots);
    let id = guard.id;
    poison(&slots);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(guard)));
    assert!(
        outcome.is_ok(),
        "poisoned InFlightGuard drop must release, not panic"
    );
    let inner = slots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        !inner.live.contains_key(&id),
        "poisoned drop still releases its slot"
    );
}

#[test]
fn poisoned_in_flight_drop_emits_fixed_vocabulary_counter() {
    let slots = Arc::new(Mutex::new(InFlightRest::default()));
    let guard = InFlightRest::begin(&slots);
    poison(&slots);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(guard)));
    let count = drop_poison_count(GUARD).expect("poison counter series must render");
    assert!(
        count >= 1,
        "poisoned drop must emit {GUARD} counter, saw {count}"
    );
}
