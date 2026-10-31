#![cfg(test)]
//! Builder-owned shutdown-drain regressions for the internal-action receiver.
//!
//! These tests pin the two review findings on the bounded receipt-child drain:
//! a fresh claim made after the drain latch refuses `busy` and releases its
//! provably-never-sent claim so the same key may retry, and the Axum drain
//! shares the single shutdown deadline with the child drain (the supervisor
//! must never drop the receiver before its child-abort path runs).
//!
//! Unlike `receipt_lifecycle` (protected acceptance matrix, test-author owned),
//! this module is ordinary builder coverage and may evolve with the code.
//!
//! All tests use only the guarded disposable test database (`database()`),
//! a scripted in-process event effect, and loopback TCP for the real
//! `BoundReceiver::serve` path. No external service is contacted.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Scripted event effect parked on a release barrier. `entered` flips when
/// the first (gate-holding) child reaches the effect; the effect completes
/// only after `release` flips, so tests control the shutdown/drain
/// interleaving deterministically without sleeps on the critical path.
struct BarrierEventMutate {
    calls: AtomicUsize,
    entered_tx: tokio::sync::watch::Sender<bool>,
    release_rx: tokio::sync::watch::Receiver<bool>,
}

impl BarrierEventMutate {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl EventMutateEffect for BarrierEventMutate {
    fn execute_mutation<'a>(
        &'a self,
        _: &'a str,
        _: &'a EventCall,
    ) -> BoxFuture<'a, Result<Value, EventActionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered_tx.send_replace(true);
            let mut release = self.release_rx.clone();
            while !*release.borrow() {
                if release.changed().await.is_err() {
                    break;
                }
            }
            Ok(serde_json::json!({"event_id": CREATE_EVENT_ID}))
        })
    }
}

struct DrainRig {
    state: Arc<ReceiverState>,
    barrier: Arc<BarrierEventMutate>,
    entered_rx: tokio::sync::watch::Receiver<bool>,
    release_tx: tokio::sync::watch::Sender<bool>,
}

fn drain_rig(pool: sqlx::PgPool, capacity: usize, released: bool) -> DrainRig {
    let (entered_tx, entered_rx) = tokio::sync::watch::channel(false);
    let (release_tx, release_rx) = tokio::sync::watch::channel(released);
    let barrier = Arc::new(BarrierEventMutate {
        calls: AtomicUsize::new(0),
        entered_tx,
        release_rx,
    });
    let mutates: Arc<dyn EventMutateEffect> = barrier.clone();
    let reads: Arc<dyn EventReadEffect> = Arc::new(MockEventRead::default());
    let effect: Arc<dyn ActionEffect> = Arc::new(MockEffect::new(MockOutcome::Success));
    let mut receiver = ReceiverState::new(
        config(),
        pool,
        effect,
        Arc::new(MockEffect::new(MockOutcome::Success)),
        reads,
        mutates,
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    );
    receiver.capacity = Arc::new(Semaphore::new(capacity));
    // A gate wait far longer than any probe timeout: an admitted second child
    // queued behind the gate cannot refuse on its own, so a prompt `busy`
    // answer can only come from the drain latch.
    receiver.event_gate_wait = Duration::from_secs(30);
    DrainRig {
        state: Arc::new(receiver),
        barrier,
        entered_rx,
        release_tx,
    }
}

async fn wait_entered(rx: &mut tokio::sync::watch::Receiver<bool>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !*rx.borrow() {
            rx.changed().await.expect("entered sender dropped");
        }
    })
    .await
    .expect("child reached the scripted effect within 10s");
}

/// The drain latch is asynchronous: callers that release the gate-holding
/// child must wait for serve to observe shutdown first, or the queued child
/// could claim pre-latch and flake.
async fn wait_drain_latch(state: &ReceiverState) {
    for _ in 0..400 {
        if state.draining.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("serve latched drain mode within 10s of the shutdown request");
}

async fn completed_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency WHERE state = 'completed'")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn state_count(pool: &sqlx::PgPool, state: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency WHERE state = $1")
        .bind(state)
        .fetch_one(pool)
        .await
        .unwrap()
}

type ServeTask = tokio::task::JoinHandle<std::io::Result<()>>;

/// Poll the serve task for `dur`; `None` means it is still running. A
/// completed result is captured, never re-polled.
async fn poll_serve(task: &mut ServeTask, dur: Duration) -> Option<std::io::Result<()>> {
    match tokio::time::timeout(dur, task).await {
        Ok(joined) => Some(joined.expect("serve task panicked")),
        Err(_) => None,
    }
}

async fn bound_serve(
    state: Arc<ReceiverState>,
) -> (
    std::net::SocketAddr,
    tokio::sync::watch::Sender<bool>,
    ServeTask,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let receiver = BoundReceiver { listener, state };
    let task = tokio::spawn(receiver.serve(shutdown_rx));
    (addr, shutdown_tx, task)
}

/// Serialize a `signed()` request onto a real loopback socket and hold the
/// connection open, so the test exercises `BoundReceiver::serve` (listener,
/// graceful shutdown) rather than only the in-process router.
async fn send_signed_tcp(
    addr: std::net::SocketAddr,
    raw: &str,
    intent: &str,
) -> tokio::net::TcpStream {
    let request = signed(raw, "old", intent);
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, MAX_BODY_BYTES).await.unwrap();
    let mut head = format!(
        "POST {ACTIONS_PATH} HTTP/1.1\r\nhost: {addr}\r\ncontent-length: {}\r\nconnection: close\r\n",
        bytes.len()
    );
    for (name, value) in parts.headers.iter() {
        head.push_str(&format!("{name}: {}\r\n", value.to_str().unwrap()));
    }
    head.push_str("\r\n");
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(&bytes).await.unwrap();
    stream.flush().await.unwrap();
    stream
}

async fn read_status(stream: &mut tokio::net::TcpStream) -> u16 {
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut buf))
        .await
        .expect("response arrived within 10s")
        .unwrap();
    let text = String::from_utf8_lossy(&buf);
    text.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no HTTP status in response: {text:.200}"))
}

#[tokio::test]
async fn drain_latch_refuses_fresh_claim_and_releases_for_retry() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let mut rig = drain_rig(db.pool().clone(), MAX_REQUESTS, false);
    let (_addr, shutdown_tx, mut serve_task) = bound_serve(rig.state.clone()).await;
    // The first child parks in its effect holding the event gate; the second
    // request queues behind it and cannot claim until the first finishes.
    let raw_first = upsert_payload("drain-guard-first");
    let first = tokio::spawn(answer(
        router(rig.state.clone()),
        signed(&raw_first, "old", "intent-drain-guard-first"),
    ));
    wait_entered(&mut rig.entered_rx).await;
    let raw_second = upsert_payload("drain-guard-second");
    let second = tokio::spawn(answer(
        router(rig.state.clone()),
        signed(&raw_second, "old", "intent-drain-guard-second"),
    ));
    shutdown_tx.send_replace(true);
    wait_drain_latch(&rig.state).await;
    rig.release_tx.send_replace(true);
    let (first_status, _, _) = first.await.unwrap();
    let (second_status, _, second_body) = second.await.unwrap();
    let serve_result = poll_serve(&mut serve_task, Duration::from_secs(10))
        .await
        .expect("serve drains once both children complete");
    assert_eq!(first_status, StatusCode::OK, "pre-latch child completes");
    assert_eq!(
        second_status,
        StatusCode::SERVICE_UNAVAILABLE,
        "post-latch fresh claim refuses"
    );
    assert_eq!(second_body["error"]["code"], "busy");
    assert_eq!(rig.barrier.calls(), 1, "the refused claim runs no effect");
    assert_eq!(
        state_count(db.pool(), "not_sent").await,
        1,
        "the provably-never-sent claim is released"
    );
    assert_eq!(completed_count(db.pool()).await, 1);
    assert!(
        serve_result.is_ok(),
        "clean drain completes Ok: {serve_result:?}"
    );
    // Simulated restart: the same key succeeds exactly once on fresh state.
    let rig2 = drain_rig(db.pool().clone(), MAX_REQUESTS, true);
    let (retry_status, _, retry_body) = answer(
        router(rig2.state.clone()),
        signed(&raw_second, "old", "intent-drain-guard-second"),
    )
    .await;
    assert_eq!(retry_status, StatusCode::OK);
    assert_eq!(
        retry_body["result"],
        serde_json::json!({"outcome": "created", "event_id": CREATE_EVENT_ID})
    );
    assert_eq!(rig2.barrier.calls(), 1, "retry runs the effect once");
    let (replay_status, replay_headers, _) = answer(
        router(rig2.state.clone()),
        signed(&raw_second, "old", "intent-drain-guard-second"),
    )
    .await;
    assert_eq!(replay_status, StatusCode::OK);
    assert_eq!(replay_headers["idempotent-replay"], "true");
    assert_eq!(rig2.barrier.calls(), 1, "replay never re-runs the effect");
    assert_eq!(
        completed_count(db.pool()).await,
        2,
        "the released intent completes on retry"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn server_phase_shares_shutdown_deadline_and_aborts_children() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let mut rig = drain_rig(db.pool().clone(), MAX_REQUESTS, false);
    let (addr, shutdown_tx, serve_task) = bound_serve(rig.state.clone()).await;
    // Hold the connection open: the admitted handler waits on its parked
    // child, so Axum graceful shutdown stays pending past the deadline.
    let raw = upsert_payload("drain-server-budget");
    let mut stream = send_signed_tcp(addr, &raw, "intent-drain-server-budget").await;
    wait_entered(&mut rig.entered_rx).await;
    shutdown_tx.send_replace(true);
    // The shared deadline — not the 20s handler timeout — ends serve, and
    // the child-abort path still runs.
    let serve_result = tokio::time::timeout(Duration::from_secs(15), serve_task)
        .await
        .expect("serve returns on the shared shutdown deadline, never the handler timeout")
        .expect("serve task panicked");
    let drain_error = serve_result
        .expect_err("serve must not report clean with a child undrained at the deadline");
    assert_eq!(drain_error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(completed_count(db.pool()).await, 0);
    assert_eq!(rig.barrier.calls(), 1);
    // Releasing the barrier now must complete nothing: the child was aborted.
    rig.release_tx.send_replace(true);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        completed_count(db.pool()).await,
        0,
        "an aborted child never persists a receipt"
    );
    // The durable fence stays occupied: replay sees uncertainty, never a
    // second effect.
    let (replay_status, _, replay_body) = answer(
        router(rig.state.clone()),
        signed(&raw, "old", "intent-drain-server-budget"),
    )
    .await;
    assert_eq!(replay_status, StatusCode::CONFLICT);
    assert_eq!(replay_body["error"]["code"], "in_progress");
    assert_eq!(rig.barrier.calls(), 1);
    // The held handler answered once its child was gone.
    assert_eq!(read_status(&mut stream).await, 409);
    db.close().await.unwrap();
}
