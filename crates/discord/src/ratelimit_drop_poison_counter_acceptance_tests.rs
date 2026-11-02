//! Drop-only counted recovery for `ResponseAccounting::drop` + loud live settle.
//!
//! Contract under test (follow-up to #1530): a poisoned `ResponseAccounting::drop`
//! still retires its entries (same assertions as
//! `ratelimit_drop_poison_acceptance_tests.rs`) and additionally emits exactly
//! one `two_bot_drop_poison_total{guard="response_accounting"}` per taken
//! poisoned path (bare-only=1, global-only=1, both-headers=2 — a drop taking
//! both header paths counts twice, once per recovered lock). Healthy drops
//! leave zero delta. Poisoned live settle (`ResponseAccounting::finish` via
//! the bare or global path) PANICS without counting — fail loud. Poisoned
//! `observe_bucket` with bare Retry-After and `observe_global` also panic, but
//! via their pre-existing header/begin `.expect` acquisitions (already loud on
//! main); they are pinned as loud controls, not RED.
//!
//! RED on main (silent `into_inner`, no counter series): drop counter
//! assertions fail (series missing), live `finish` panic assertions fail
//! (silent settle). GREEN after Builder (drop-only `recover_drop_poison` +
//! loud `.expect`). Deltas use the global with a serializing lock — never
//! absolute counts (parallel tests share the global).
#![cfg(test)]

use super::*;
use crate::drop_poison_test_serial::DROP_POISON_SERIAL;
use std::panic::{catch_unwind, AssertUnwindSafe};

const GUARD: &str = "response_accounting";

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

fn poison(guard: &RateLimitGuard) {
    let holder = catch_unwind(AssertUnwindSafe(|| {
        let _held = guard.state.lock().expect("healthy REST guard state");
        std::panic::panic_any("poison the REST guard state");
    }));
    assert!(holder.is_err());
    assert!(guard.state.is_poisoned());
}

fn global_headers() -> http::HeaderMap {
    http::HeaderMap::from_iter([
        (
            http::header::HeaderName::from_static("x-ratelimit-global"),
            http::HeaderValue::from_static("true"),
        ),
        (
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("2"),
        ),
    ])
}

fn bare_headers() -> http::HeaderMap {
    http::HeaderMap::from_iter([(
        http::header::RETRY_AFTER,
        http::HeaderValue::from_static("2"),
    )])
}

async fn global_drop_with_counter(poisoned: bool, existing_deadline: bool, expected_delta: u64) {
    let guard = RateLimitGuard::new(GuardConfig::default()).expect("REST guard");
    let headers = global_headers();
    let accounting = ResponseAccounting::new(&guard, 429, &headers, true);
    let at = accounting.observed_at;
    let own = accounting.global_header.expect("provisional global entry");
    let other = guard.begin_global(Some(30.0), at);
    if existing_deadline {
        guard.state.lock().expect("healthy state").global_until =
            Some(at + Duration::from_secs(20));
    }
    let expected_deadline = if existing_deadline {
        at + Duration::from_secs(20)
    } else {
        at + Duration::from_millis(2_250)
    };
    assert_eq!(guard.snapshot().pending_global_responses, 2);
    let changed = guard.changed.notified();
    tokio::pin!(changed);
    changed.as_mut().enable();
    if poisoned {
        poison(&guard);
    }

    let (outcome, before, after) = {
        let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = drop_poison_count(GUARD);
        let outcome = catch_unwind(AssertUnwindSafe(|| drop(accounting)));
        let after = drop_poison_count(GUARD);
        (outcome, before, after)
    };
    assert!(
        outcome.is_ok(),
        "global ResponseAccounting::drop must not panic"
    );
    {
        let state = guard
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(!state.pending_global.iter().any(|(id, _)| *id == own));
        assert_eq!(state.pending_global.len(), 1, "only this response settles");
        assert_eq!(state.pending_global[0].0, other);
        assert_eq!(
            state.global_until,
            Some(expected_deadline),
            "commit this response's timing without shortening an existing restriction"
        );
        assert_eq!(
            RateLimitGuard::global_deadline(&state, at),
            Some(at + Duration::from_millis(30_250)),
            "the overlapping response still governs admission"
        );
    }
    tokio::time::timeout(Duration::from_secs(1), changed)
        .await
        .expect("settling a response wakes admission waiters");
    let (before, after) = (
        before.expect("poison counter series must render"),
        after.expect("poison counter series must render"),
    );
    assert_eq!(
        after.saturating_sub(before),
        expected_delta,
        "global drop must emit exactly {expected_delta} {GUARD} count(s)"
    );
}

async fn bare_drop_with_counter(poisoned: bool, expected_delta: u64) {
    let guard = RateLimitGuard::new(GuardConfig::default()).expect("REST guard");
    let headers = bare_headers();
    let mut accounting = ResponseAccounting::new(&guard, 429, &headers, true);
    let at = accounting.observed_at;
    accounting.track_bare_bucket("GET /fixture", &headers);
    {
        let mut state = guard.state.lock().expect("healthy state");
        state
            .routes
            .0
            .insert("GET /fixture".into(), "learned-bucket".into());
    }
    if poisoned {
        poison(&guard);
    }

    let (outcome, before, after) = {
        let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = drop_poison_count(GUARD);
        let outcome = catch_unwind(AssertUnwindSafe(|| drop(accounting)));
        let after = drop_poison_count(GUARD);
        (outcome, before, after)
    };
    assert!(
        outcome.is_ok(),
        "bare ResponseAccounting::drop must not panic"
    );
    let state = guard
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    assert_eq!(
        state.routes.0.get("GET /fixture").map(String::as_str),
        Some("learned-bucket")
    );
    assert_eq!(
        state.buckets.0.get("learned-bucket").copied(),
        Some(at + Duration::from_millis(2_250)),
        "dropped body still records retry-after plus padding"
    );
    assert_eq!(
        state.buckets.0.len(),
        1,
        "no duplicate raw-route restriction"
    );
    assert!(
        state.pending_global.is_empty(),
        "a bare retry is not global"
    );
    assert!(state.global_until.is_none());
    let (before, after) = (
        before.expect("poison counter series must render"),
        after.expect("poison counter series must render"),
    );
    assert_eq!(
        after.saturating_sub(before),
        expected_delta,
        "bare drop must emit exactly {expected_delta} {GUARD} count(s)"
    );
}

async fn both_drop_with_counter(poisoned: bool, expected_delta: u64) {
    let guard = RateLimitGuard::new(GuardConfig::default()).expect("REST guard");
    let global = global_headers();
    let bare = bare_headers();
    let mut accounting = ResponseAccounting::new(&guard, 429, &global, true);
    let at = accounting.observed_at;
    let own = accounting.global_header.expect("provisional global entry");
    accounting.track_bare_bucket("GET /both", &bare);
    assert!(
        accounting.bare_route.is_some(),
        "both-headers fixture must take the bare path"
    );
    let other = guard.begin_global(Some(30.0), at);
    assert_eq!(guard.snapshot().pending_global_responses, 2);
    let changed = guard.changed.notified();
    tokio::pin!(changed);
    changed.as_mut().enable();
    // Insert AFTER begin/snapshot: refresh prunes routes whose bucket is not
    // yet recorded (orphaned), so an earlier insert would be reaped before the
    // drop under test.
    {
        let mut state = guard.state.lock().expect("healthy state");
        state
            .routes
            .0
            .insert("GET /both".into(), "learned-bucket".into());
    }
    if poisoned {
        poison(&guard);
    }

    let (outcome, before, after) = {
        let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = drop_poison_count(GUARD);
        let outcome = catch_unwind(AssertUnwindSafe(|| drop(accounting)));
        let after = drop_poison_count(GUARD);
        (outcome, before, after)
    };
    assert!(
        outcome.is_ok(),
        "both-headers ResponseAccounting::drop must not panic"
    );
    {
        let state = guard
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(!state.pending_global.iter().any(|(id, _)| *id == own));
        assert_eq!(state.pending_global.len(), 1, "only this response settles");
        assert_eq!(state.pending_global[0].0, other);
        assert_eq!(
            state.global_until,
            Some(at + Duration::from_millis(2_250)),
            "commit this response's global timing"
        );
        assert_eq!(
            state.buckets.0.get("learned-bucket").copied(),
            Some(at + Duration::from_millis(2_250)),
            "dropped body still records bare retry-after plus padding"
        );
    }
    tokio::time::timeout(Duration::from_secs(1), changed)
        .await
        .expect("settling a response wakes admission waiters");
    let (before, after) = (
        before.expect("poison counter series must render"),
        after.expect("poison counter series must render"),
    );
    assert_eq!(
        after.saturating_sub(before),
        expected_delta,
        "both-headers drop must emit exactly {expected_delta} {GUARD} counts (one per path)"
    );
}

#[tokio::test(start_paused = true)]
async fn healthy_bare_drop_leaves_counter_at_zero_delta() {
    bare_drop_with_counter(false, 0).await;
}

#[tokio::test(start_paused = true)]
async fn poisoned_bare_drop_counts_exactly_one() {
    bare_drop_with_counter(true, 1).await;
}

#[tokio::test(start_paused = true)]
async fn healthy_global_drop_leaves_counter_at_zero_delta() {
    global_drop_with_counter(false, false, 0).await;
}

#[tokio::test(start_paused = true)]
async fn poisoned_global_drop_counts_exactly_one() {
    global_drop_with_counter(true, false, 1).await;
}

#[tokio::test(start_paused = true)]
async fn healthy_both_headers_drop_leaves_counter_at_zero_delta() {
    both_drop_with_counter(false, 0).await;
}

#[tokio::test(start_paused = true)]
async fn poisoned_both_headers_drop_counts_exactly_two() {
    both_drop_with_counter(true, 2).await;
}

#[tokio::test(start_paused = true)]
async fn poisoned_live_bare_settle_panics_without_counter() {
    let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let guard = RateLimitGuard::new(GuardConfig::default()).expect("REST guard");
    let headers = bare_headers();
    let mut accounting = ResponseAccounting::new(&guard, 429, &headers, true);
    accounting.track_bare_bucket("GET /fixture", &headers);
    poison(&guard);
    let before = drop_poison_count(GUARD).unwrap_or(0);
    // Empty body carries no timing/scope signal, so `finish` takes the bare
    // path (`observe_bare_retry_after`): silent today, loud after Builder.
    let response = crate::executor::RawResponse {
        status: 429,
        retry_after_header: None,
        body: br#"{}"#.to_vec(),
        completion: None,
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| accounting.finish(&response)));
    let after = drop_poison_count(GUARD).unwrap_or(0);
    // `finish` took `bare_route` before the loud call, so this Drop settles
    // nothing and counts nothing either way; hold the serial through it.
    drop(accounting);
    assert!(
        outcome.is_err(),
        "poisoned live bare settle must panic (fail loud)"
    );
    assert_eq!(after, before, "loud settle must not emit {GUARD}");
}

#[tokio::test(start_paused = true)]
async fn poisoned_live_global_settle_panics_without_counter() {
    let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let guard = RateLimitGuard::new(GuardConfig::default()).expect("REST guard");
    let headers = global_headers();
    let mut accounting = ResponseAccounting::new(&guard, 429, &headers, true);
    assert!(
        accounting.global_header.is_some(),
        "global settle fixture must hold a provisional entry"
    );
    poison(&guard);
    let before = drop_poison_count(GUARD).unwrap_or(0);
    // Existing provisional id: `finish` calls `finish_global` directly (no
    // `begin_global` expect in between): silent today, loud after Builder.
    let response = crate::executor::RawResponse {
        status: 429,
        retry_after_header: None,
        body: br#"{"global":true}"#.to_vec(),
        completion: None,
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| accounting.finish(&response)));
    let after = drop_poison_count(GUARD).unwrap_or(0);
    drop(accounting);
    assert!(
        outcome.is_err(),
        "poisoned live global settle must panic (fail loud)"
    );
    assert_eq!(after, before, "loud settle must not emit {GUARD}");
}

#[tokio::test(start_paused = true)]
async fn poisoned_live_observe_bucket_with_bare_retry_after_panics_without_counter() {
    let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let guard = RateLimitGuard::new(GuardConfig::default()).expect("REST guard");
    poison(&guard);
    let before = drop_poison_count(GUARD).unwrap_or(0);
    let headers = bare_headers();
    // Loud control: panics today via `observe_bucket_headers` `.expect`
    // (before reaching the bare path), still panics after Builder.
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        guard.observe_bucket("GET /fixture", &headers);
    }));
    let after = drop_poison_count(GUARD).unwrap_or(0);
    assert!(outcome.is_err(), "poisoned live observe_bucket must panic");
    assert_eq!(after, before, "loud observe must not emit {GUARD}");
}

#[tokio::test(start_paused = true)]
async fn poisoned_live_observe_global_panics_without_counter() {
    let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let guard = RateLimitGuard::new(GuardConfig::default()).expect("REST guard");
    poison(&guard);
    let before = drop_poison_count(GUARD).unwrap_or(0);
    // Loud control: panics today via `begin_global` `.expect` (before
    // reaching `finish_global`), still panics after Builder.
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        guard.observe_global(Some(2.0));
    }));
    let after = drop_poison_count(GUARD).unwrap_or(0);
    assert!(outcome.is_err(), "poisoned live observe_global must panic");
    assert_eq!(after, before, "loud observe must not emit {GUARD}");
}
