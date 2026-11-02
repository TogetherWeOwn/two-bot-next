//! Exercise RoomHttp::send_request against the existing loopback fixture.
//! Replacing the sub-cap wait with sleep_until(until) must collapse every
//! observed send to the same tick and fail the spread assertion.
//!
//! No RNG-injection seam exists in RoomHttp. Use 64 independent request draws,
//! not a flaky assertion on a single draw. The oracle records actual sends,
//! and drains all in-flight loopback IO WITHOUT advancing virtual time. Thus
//! staggered TCP/response progress cannot masquerade as retry jitter.

#![cfg(test)]

use super::*;
use std::{future::Future, pin::Pin, task::Poll};

async fn settle_io<F>(requests: &mut [Option<Pin<Box<F>>>], api: &RoomHttp)
where
    F: Future<Output = Result<(), RoomHttpError>>,
{
    // Wall time is only a bounded failure guard for the local socket fixture;
    // it never advances Tokio's paused clock or affects the timing oracle.
    let wall_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        for request in requests.iter_mut() {
            if let Some(future) = request {
                if let Poll::Ready(result) = futures_util::poll!(future.as_mut()) {
                    result.expect("loopback request must succeed exactly once");
                    *request = None;
                }
            }
        }
        if api.in_flight.lock().unwrap().live.is_empty() {
            return;
        }
        assert!(
            std::time::Instant::now() < wall_deadline,
            "loopback IO did not drain at frozen time; do not advance clocks through outstanding transport work"
        );
        // This runnable task prevents paused-time auto-advance while Hyper
        // and Axum make explicit IO progress at the current timer tick.
        tokio::task::yield_now().await;
    }
}

async fn observed_sends(wait: Duration, samples: usize) -> Vec<Duration> {
    let mock = Mock::start(vec![response(204, Value::Null); samples + 1]).await;
    // Establish the loopback connection while time is real. No pending HTTP
    // request/timeout is allowed to auto-advance the subsequent paused clock.
    mock.api.delete_room(500, || true).await.unwrap();
    mock.state.recorded.lock().unwrap().clear();
    tokio::time::pause();
    let started = Instant::now();
    let deadline = started + wait;
    *mock.api.global_not_before.lock().unwrap() = Some(deadline);
    let mut requests: Vec<_> = (0..samples)
        .map(|index| {
            let api = mock.api.clone();
            Some(Box::pin(async move {
                api.delete_room(600 + index as u64, || true).await
            }))
        })
        .collect();
    // Poll EVERY request before the first advance, so all shared-deadline
    // sleeps are armed at the same instant, not when the IO driver is ready.
    settle_io(&mut requests, &mock.api).await;
    assert_eq!(
        Instant::now(),
        started,
        "fixture auto-advanced before sleeps were armed"
    );
    if !wait.is_zero() {
        assert!(
            mock.state.recorded.lock().unwrap().is_empty(),
            "sent before the shared global deadline"
        );
        let before = wait - Duration::from_millis(1);
        tokio::time::advance(before).await;
        settle_io(&mut requests, &mock.api).await;
        assert!(
            mock.state.recorded.lock().unwrap().is_empty(),
            "sent before the shared global deadline"
        );
    }
    // 10 ms observation ticks; the extra 20 ms is solely timer-wheel/tick
    // rounding, not seconds of unaccounted socket scheduling. In-flight IO
    // must be fully drained at each tick before ANY subsequent advance.
    let upper = started + wait + Duration::from_secs(5) + Duration::from_millis(20);
    while requests.iter().any(Option::is_some) && Instant::now() < upper {
        let target = (Instant::now() + Duration::from_millis(10)).min(upper);
        tokio::time::advance(target.saturating_duration_since(Instant::now())).await;
        settle_io(&mut requests, &mock.api).await;
        assert_eq!(
            Instant::now(),
            target,
            "loopback IO caused implicit paused-time advancement"
        );
    }
    assert!(
        requests.iter().all(Option::is_none),
        "request did not send within the bounded spread"
    );
    let recorded = mock.state.recorded.lock().unwrap();
    assert_eq!(
        recorded.len(),
        samples,
        "each operation must send exactly one real HTTP request"
    );
    let paths: std::collections::HashSet<_> = recorded
        .iter()
        .map(|request| request.path.as_str())
        .collect();
    assert_eq!(
        paths.len(),
        samples,
        "each sampled operation must reach the wire"
    );
    let offsets = recorded
        .iter()
        .map(|request| {
            assert_eq!(request.method, Method::DELETE);
            assert!(request.bot_authenticated);
            assert!(request.at >= deadline, "global wait sent early");
            assert!(
                request.at <= upper,
                "global spread exceeded its deadline + 5 s bound"
            );
            request.at.duration_since(deadline)
        })
        .collect();
    tokio::time::resume();
    offsets
}

#[tokio::test]
async fn global_sub_cap_runtime_sends_after_deadline_with_bounded_independent_spread() {
    let offsets = observed_sends(Duration::from_secs(3), 64).await;
    let distinct: std::collections::HashSet<_> = offsets.iter().copied().collect();
    let earliest = *offsets.iter().min().unwrap();
    let latest = *offsets.iter().max().unwrap();
    assert!(
        distinct.len() >= 4,
        "{} distinct actual send ticks: sub-cap waits still share an unjittered deadline",
        distinct.len()
    );
    assert!(
        latest - earliest >= Duration::from_millis(500),
        "actual sends spread only {:?}; sleep_until(until) must not satisfy the runtime oracle",
        latest - earliest
    );
}

#[tokio::test]
async fn global_expired_deadline_runtime_sends_without_added_spread() {
    let offsets = observed_sends(Duration::ZERO, 4).await;
    assert!(
        offsets.iter().all(|offset| offset.is_zero()),
        "expired deadlines must not add a spread wait"
    );
}

#[tokio::test]
async fn global_at_cap_runtime_sends_at_cap_without_added_spread_or_handoff() {
    let offsets = observed_sends(Duration::from_secs(30), 4).await;
    assert!(
        offsets
            .iter()
            .all(|offset| *offset <= Duration::from_millis(20)),
        "at-cap waits must remain unjittered; >30 s still belongs to the RateLimited queue handoff"
    );
}
