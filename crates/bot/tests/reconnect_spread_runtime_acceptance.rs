//! The Twilight receive arm needs only a small decorrelation wait, not the
//! independent supervised runner's exponential schedule or exhaustion policy.
//! Deleting the helper's sleep fails the first poll; stacking the supervised
//! schedule (including its past-budget 30 s fallback) fails the upper bound.
//! Wiring/bypassing the awaited arm call is pinned in reconnect_adoption_guard.

use futures_util::poll;
use std::{collections::HashSet, task::Poll, time::Duration};
use tokio::time::{advance, Instant};
use two_bot::gateway_backoff::wait_reconnect_spread;

async fn requested_spread(seed: u64, attempt: u32) -> Duration {
    let started = Instant::now();
    let wait = wait_reconnect_spread(seed, attempt);
    tokio::pin!(wait);
    assert!(
        poll!(wait.as_mut()).is_pending(),
        "helper must await a real timer"
    );
    advance(Duration::from_millis(499)).await;
    assert!(
        poll!(wait.as_mut()).is_pending(),
        "spread completed before 500 ms"
    );
    // Tokio rounds timers to millisecond ticks. Observe completion in 1 ms
    // steps and allow at most 3 ms beyond the requested delay, not seconds.
    for _ in 0..504 {
        advance(Duration::from_millis(1)).await;
        if let Poll::Ready(requested) = poll!(wait.as_mut()) {
            assert!(
                (Duration::from_millis(500)..=Duration::from_secs(1)).contains(&requested),
                "seed {seed}, attempt {attempt}: requested {requested:?} is not a non-growing [500 ms, 1 s] spread"
            );
            let elapsed = started.elapsed();
            assert!(
                elapsed >= requested,
                "returned before its requested wait: {elapsed:?} < {requested:?}"
            );
            assert!(
                elapsed <= requested + Duration::from_millis(3),
                "helper slept longer than its returned duration: {elapsed:?} vs {requested:?}"
            );
            return requested;
        }
    }
    panic!(
        "seed {seed}, attempt {attempt}: helper exceeded the 1 s spread plus timer rounding; do not stack the supervised reconnect schedule"
    );
}

#[tokio::test(start_paused = true)]
async fn reconnect_spread_awaits_bounded_timers_through_and_past_the_supervised_budget() {
    for attempt in [0, 1, 2, 5, 9, 10, 11, 100, u32::MAX] {
        for seed in [0, 1, 42, u64::MAX] {
            requested_spread(seed, attempt).await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn reconnect_spread_replays_each_seed_and_attempt_and_decorrelates_seeds() {
    for attempt in [0, 9, 10, u32::MAX] {
        let mut draws = HashSet::new();
        for seed in 0..16 {
            let first = requested_spread(seed, attempt).await;
            let replay = requested_spread(seed, attempt).await;
            assert_eq!(
                first, replay,
                "same seed/attempt must replay the requested delay"
            );
            draws.insert(first);
        }
        assert!(
            draws.len() >= 2,
            "all seeds selected the same spread at attempt {attempt}"
        );
    }
}
