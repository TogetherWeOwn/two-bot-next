//! Dropped response bodies still commit admission restrictions after poison.
#![cfg(test)]

use super::*;
use crate::drop_poison_test_serial::DROP_POISON_SERIAL;
use std::panic::{catch_unwind, AssertUnwindSafe};

fn poison(guard: &RateLimitGuard) {
    let holder = catch_unwind(AssertUnwindSafe(|| {
        let _held = guard.state.lock().expect("healthy REST guard state");
        std::panic::panic_any("poison the REST guard state");
    }));
    assert!(holder.is_err());
    assert!(guard.state.is_poisoned());
}

async fn global_drop(poisoned: bool, existing_deadline: bool) {
    let guard = RateLimitGuard::new(GuardConfig::default()).expect("REST guard");
    let headers = http::HeaderMap::from_iter([
        (
            http::header::HeaderName::from_static("x-ratelimit-global"),
            http::HeaderValue::from_static("true"),
        ),
        (
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("2"),
        ),
    ]);
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
    // Register before drop: notify_waiters does not retain an unobserved permit.
    let changed = guard.changed.notified();
    tokio::pin!(changed);
    changed.as_mut().enable();
    if poisoned {
        poison(&guard);
    }

    // The poison counter series is process-global: hold the shared lock so
    // this drop cannot land inside a counter test's delta window.
    let outcome = {
        let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        catch_unwind(AssertUnwindSafe(|| drop(accounting)))
    };
    assert!(
        outcome.is_ok(),
        "global ResponseAccounting::drop must not panic"
    );
    {
        // Poison is sticky; inspection must not introduce a separate panic in
        // snapshot/admit, neither of which is the destructor under test.
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
}

async fn bare_drop(poisoned: bool) {
    let guard = RateLimitGuard::new(GuardConfig::default()).expect("REST guard");
    let headers = http::HeaderMap::from_iter([(
        http::header::RETRY_AFTER,
        http::HeaderValue::from_static("2"),
    )]);
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

    // The poison counter series is process-global: hold the shared lock so
    // this drop cannot land inside a counter test's delta window.
    let outcome = {
        let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        catch_unwind(AssertUnwindSafe(|| drop(accounting)))
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
}

#[tokio::test(start_paused = true)]
async fn healthy_global_response_drop_settles_own_entry_and_retains_deadlines() {
    global_drop(false, true).await;
}

#[tokio::test(start_paused = true)]
async fn healthy_global_response_drop_commits_its_header_deadline() {
    global_drop(false, false).await;
}

#[tokio::test(start_paused = true)]
async fn poisoned_global_response_drop_settles_own_entry_and_retains_deadlines() {
    global_drop(true, true).await;
}

#[tokio::test(start_paused = true)]
async fn poisoned_global_response_drop_commits_its_header_deadline() {
    global_drop(true, false).await;
}

#[tokio::test(start_paused = true)]
async fn healthy_bare_response_drop_records_the_learned_bucket_restriction() {
    bare_drop(false).await;
}

#[tokio::test(start_paused = true)]
async fn poisoned_bare_response_drop_records_the_learned_bucket_restriction() {
    bare_drop(true).await;
}
