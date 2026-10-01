// Keep the real adapter/transport in these unit tests: cfg(test) exposes only
// committed admissions, not the lane mutex or configurable production intervals.
#[allow(dead_code)]
#[path = "../../tests/common/mod.rs"]
mod common;

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use common::{MockRest, ScriptedResponse};
use tokio::{sync::mpsc, time::Instant};
use two_bot_core::audit::delivery_nonce;
use two_bot_core::audit_mirror::AuditMirror;

use crate::executor::{ActionExecutor, PacingAdmission, PacingLane, PACE_INTERVAL_MS};
use crate::test_clock::{advance, mark_progress, stall_watchdog, ClockHold, STALL_GRACE};

const CHANNEL: &str = "4444";

fn executor_for(mock: &MockRest) -> (ActionExecutor, mpsc::UnboundedReceiver<PacingAdmission>) {
    ActionExecutor::with_pacing_probe("s5-mirror-token".to_owned(), Some(mock.origin())).unwrap()
}

fn shared_admission(admissions: &mut mpsc::UnboundedReceiver<PacingAdmission>) -> Instant {
    let admission = admissions
        .try_recv()
        .expect("completed call committed admission");
    assert_eq!(admission.lane, PacingLane::Shared);
    mark_progress();
    admission.at
}

fn no_admission(admissions: &mut mpsc::UnboundedReceiver<PacingAdmission>) {
    assert_eq!(
        admissions.try_recv().unwrap_err(),
        mpsc::error::TryRecvError::Empty
    );
}

#[tokio::test(start_paused = true)]
async fn mirror_posts_share_the_executor_read_pacing_lane_despite_delayed_observation() {
    stall_watchdog(STALL_GRACE, async {
        let _clock = ClockHold::start().await;
        let (mock, gate) = MockRest::start_with_first_receipt_gate(
            vec![ScriptedResponse::json(200, serde_json::json!([]))],
            ScriptedResponse::json(200, serde_json::json!({"id": "640"})),
        )
        .await;
        mark_progress();
        let (exec, mut admissions) = executor_for(&mock);
        let nonce = delivery_nonce(&mock.origin());
        let start = Instant::now();
        let history = exec.channel_history(CHANNEL, None, 100);
        tokio::pin!(history);
        tokio::select! {
            result = &mut history => panic!("gated receipt completed early: {result:?}"),
            entered = gate.entered => entered.unwrap(),
        }
        let first = shared_admission(&mut admissions);
        assert_eq!(first, start);
        assert!(mock.requests().is_empty());
        // Admission was at t0, but the observer intentionally sees it at t100.
        advance(Duration::from_millis(100)).await;
        gate.release.send(()).unwrap();
        history.await.unwrap();
        mark_progress();
        assert_eq!(Instant::now(), first + Duration::from_millis(100));

        let mut stamps = vec![first];
        for index in 0..10 {
            let last = *stamps.last().unwrap();
            let authorized = AtomicBool::new(false);
            let content = format!("post-{index}");
            let post = exec.post_mirror_checked(CHANNEL, &content, &nonce, async {
                authorized.store(true, Ordering::SeqCst);
                Ok::<(), ()>(())
            });
            tokio::pin!(post);
            assert!(futures_util::poll!(&mut post).is_pending());
            advance(last + Duration::from_millis(109) - Instant::now()).await;
            assert!(futures_util::poll!(&mut post).is_pending());
            assert!(
                !authorized.load(Ordering::SeqCst),
                "authorization must follow the pacing floor"
            );
            no_admission(&mut admissions);
            advance(Duration::from_millis(2)).await;
            assert_eq!(post.await, Ok(Ok("640".to_owned())));
            let at = shared_admission(&mut admissions);
            assert_eq!(Instant::now(), at, "socket I/O advanced virtual time");
            assert_eq!(at, last + Duration::from_millis(111));
            stamps.push(at);
        }
        assert!(stamps.windows(2).all(|pair| {
            pair[1].duration_since(pair[0]) >= Duration::from_millis(PACE_INTERVAL_MS)
        }));
        assert!(stamps[10].duration_since(first) >= Duration::from_millis(1100));
        let requests = mock.requests();
        assert_eq!(requests.len(), 11);
        assert_eq!(exec.requests(), 11);
        assert_eq!(requests[0].method, "GET");
        for (index, request) in requests[1..].iter().enumerate() {
            assert_eq!(request.method, "POST");
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["content"], format!("post-{index}"));
        }
        assert_eq!(requests[0].received_at, first + Duration::from_millis(100));
        assert_eq!(
            requests[1]
                .received_at
                .duration_since(requests[0].received_at),
            Duration::from_millis(11),
            "observer lag compresses receipt gaps despite correct admission pacing"
        );
        no_admission(&mut admissions);
        mock.shutdown().await;
    })
    .await
    .expect("shared-lane fixture must complete under the independent watchdog");
}

#[tokio::test(start_paused = true)]
async fn checked_post_authorizes_after_pacing_and_refusal_sends_nothing() {
    stall_watchdog(STALL_GRACE, async {
        let _clock = ClockHold::start().await;
        let mock = MockRest::start(
            vec![ScriptedResponse::json(200, serde_json::json!([]))],
            ScriptedResponse::json(200, serde_json::json!({"id": "640"})),
        )
        .await;
        mark_progress();
        let (exec, mut admissions) = executor_for(&mock);
        let nonce = delivery_nonce(&mock.origin());
        exec.channel_history(CHANNEL, None, 100).await.unwrap();
        let read_at = shared_admission(&mut admissions);
        let authorized = AtomicBool::new(false);
        let refused = exec.post_mirror_checked(CHANNEL, "refused", &nonce, async {
            authorized.store(true, Ordering::SeqCst);
            Err::<(), _>("claim lost while waiting")
        });
        tokio::pin!(refused);
        assert!(futures_util::poll!(&mut refused).is_pending());
        advance(Duration::from_millis(109)).await;
        assert!(futures_util::poll!(&mut refused).is_pending());
        assert!(!authorized.load(Ordering::SeqCst));
        no_admission(&mut admissions);
        advance(Duration::from_millis(2)).await;
        assert_eq!(refused.await, Err("claim lost while waiting"));
        assert!(authorized.load(Ordering::SeqCst));
        no_admission(&mut admissions);
        assert_eq!(
            mock.requests().len(),
            1,
            "only the earlier history GET reached the wire"
        );
        assert_eq!(exec.requests(), 1);

        // Refusal does not consume the lane's next slot. No further advance is
        // needed for a valid sender at this already-eligible virtual instant.
        assert_eq!(
            exec.post_mirror(CHANNEL, "accepted", &nonce).await,
            Ok("640".to_owned())
        );
        let accepted_at = shared_admission(&mut admissions);
        assert_eq!(accepted_at, read_at + Duration::from_millis(111));
        assert_eq!(Instant::now(), accepted_at);
        assert_eq!(mock.requests().len(), 2);
        assert_eq!(exec.requests(), 2);
        let body: serde_json::Value = serde_json::from_slice(&mock.requests()[1].body).unwrap();
        assert_eq!(body["content"], "accepted");
        no_admission(&mut admissions);
        mock.shutdown().await;
    })
    .await
    .expect("late-authorization fixture must complete under the independent watchdog");
}

#[tokio::test(start_paused = true)]
async fn slow_authorization_cannot_be_overtaken_by_another_mirror_post_or_read() {
    stall_watchdog(STALL_GRACE, async {
        let _clock = ClockHold::start().await;
        let mock = MockRest::start(
            vec![
                ScriptedResponse::json(200, serde_json::json!({"id": "640"})),
                ScriptedResponse::json(200, serde_json::json!({"id": "641"})),
                ScriptedResponse::json(200, serde_json::json!([])),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        mark_progress();
        let (exec, mut admissions) = executor_for(&mock);
        let other = exec.clone();
        let nonce = delivery_nonce(&mock.origin());
        let start = Instant::now();
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let first = exec.post_mirror_checked(CHANNEL, "first", &nonce, async {
            entered.send(()).unwrap();
            released.await.unwrap();
            Ok::<(), ()>(())
        });
        tokio::pin!(first);
        tokio::select! {
            result = &mut first => panic!("held authorization completed early: {result:?}"),
            entered = waiting => entered.unwrap(),
        }
        // Explicitly poll lock acquisition in order; spawning order is not FIFO.
        let second = other.post_mirror(CHANNEL, "second", &nonce);
        let history = other.channel_history(CHANNEL, None, 100);
        tokio::pin!(second, history);
        assert!(futures_util::poll!(&mut second).is_pending());
        assert!(futures_util::poll!(&mut history).is_pending());
        advance(Duration::from_millis(150)).await;
        assert!(futures_util::poll!(&mut first).is_pending());
        assert!(futures_util::poll!(&mut second).is_pending());
        assert!(futures_util::poll!(&mut history).is_pending());
        no_admission(&mut admissions);
        assert!(mock.requests().is_empty());
        release.send(()).unwrap();
        assert_eq!(first.await, Ok(Ok("640".to_owned())));
        let first_at = shared_admission(&mut admissions);
        assert_eq!(first_at, start + Duration::from_millis(150));
        assert!(futures_util::poll!(&mut second).is_pending());
        advance(Duration::from_millis(109)).await;
        assert!(futures_util::poll!(&mut second).is_pending());
        assert!(futures_util::poll!(&mut history).is_pending());
        no_admission(&mut admissions);
        advance(Duration::from_millis(2)).await;
        assert_eq!(second.await, Ok("641".to_owned()));
        let second_at = shared_admission(&mut admissions);
        assert_eq!(
            second_at.duration_since(first_at),
            Duration::from_millis(111)
        );
        assert!(futures_util::poll!(&mut history).is_pending());
        advance(Duration::from_millis(109)).await;
        assert!(futures_util::poll!(&mut history).is_pending());
        no_admission(&mut admissions);
        advance(Duration::from_millis(2)).await;
        history.await.unwrap();
        mark_progress();
        let read_at = shared_admission(&mut admissions);
        assert_eq!(
            read_at.duration_since(second_at),
            Duration::from_millis(111)
        );
        assert_eq!(Instant::now(), read_at);
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(exec.requests(), 3);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[2].method, "GET");
        for (request, content) in requests.iter().zip(["first", "second"]) {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["content"], content);
        }
        no_admission(&mut admissions);
        mock.shutdown().await;
    })
    .await
    .expect("lane-order fixture must complete under the independent watchdog");
}
