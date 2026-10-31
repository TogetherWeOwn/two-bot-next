#![cfg(test)]
//! Protected acceptance matrix: receipt-finishing event-mutation children
//! must be bounded and drained by the receiver that admitted them.
//!
//! Production code must be changed to satisfy these tests; do not edit this
//! file to make a build pass. Test corrections belong to the test author.
//!
//! The event-mutation child is spawned so an admitted mutation can finish
//! its receipt after the HTTP client is gone. That survival is required, but
//! it must not be unbounded or ownerless:
//!
//! - `detached_children_hold_receiver_capacity_until_receipt_persists`:
//!   capacity is held through child completion/receipt persistence, not just
//!   the HTTP response lifetime. A client abort (which drops the handler
//!   future exactly like the handler request timeout drops `receive`) must
//!   not free admission for unbounded further children.
//! - `receiver_shutdown_joins_child_during_effect_before_reporting_clean`
//!   and `receiver_shutdown_joins_child_during_receipt_persistence`:
//!   shutdown stops new admissions and joins in-flight children, both while
//!   the Discord effect runs and while terminal persistence (event-key
//!   mapping / receipt finish) runs. Clean completion is reported only
//!   after tracked work has drained.
//! - `receiver_shutdown_deadline_is_bounded_and_never_reports_clean_or_allows_second_effect`:
//!   the drain wait is bounded by the existing shutdown budget; at the
//!   deadline the receiver must not report clean completion, must preserve
//!   the durable claim fence, and must never permit a second effect.
//! - Controls (expected to pass before and after): clean shutdown with no
//!   new admissions afterwards, cancelled-client receipt replay without a
//!   second effect and with capacity restored, and the signature gate still
//!   refusing while a child is in flight.
//!
//! All tests use only the guarded disposable test database (`database()`),
//! a scripted in-process event effect, and loopback TCP for the real
//! `BoundReceiver::serve` path. No external service is contacted.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Scripted event effect parked on a release barrier. `entered` flips when
/// the first (gate-holding) child reaches the effect; the effect completes
/// only after `release` flips, so tests control effect vs persistence
/// phases deterministically without sleeps on the critical path.
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

struct LifecycleRig {
    state: Arc<ReceiverState>,
    barrier: Arc<BarrierEventMutate>,
    entered_rx: tokio::sync::watch::Receiver<bool>,
    release_tx: tokio::sync::watch::Sender<bool>,
}

fn lifecycle_rig(pool: sqlx::PgPool, capacity: usize, released: bool) -> LifecycleRig {
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
    // A gate wait far longer than any probe timeout: an admitted probe child
    // queued behind the gate cannot complete on its own during a probe, so a
    // prompt `busy` answer can only come from held capacity.
    receiver.event_gate_wait = Duration::from_secs(30);
    LifecycleRig {
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

async fn completed_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency WHERE state = 'completed'")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn wait_completed(pool: &sqlx::PgPool, want: i64) -> i64 {
    let mut completed = completed_count(pool).await;
    for _ in 0..400 {
        if completed >= want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        completed = completed_count(pool).await;
    }
    completed
}

async fn wait_capacity_restored(state: &ReceiverState, want: usize) -> usize {
    for _ in 0..400 {
        let available = state.capacity.available_permits();
        if available >= want {
            return available;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    state.capacity.available_permits()
}

/// Serialize a `signed()` request onto a real loopback socket so the tests
/// exercise `BoundReceiver::serve` (listener, graceful shutdown, client
/// disconnect) rather than only the in-process router.
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

type ServeTask = tokio::task::JoinHandle<std::io::Result<()>>;

/// Poll the serve task for `dur`; `None` means it is still running (the
/// expected shape while a tracked child is unfinished). A completed result
/// is captured, never re-polled.
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

#[tokio::test]
async fn detached_children_hold_receiver_capacity_until_receipt_persists() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let mut rig = lifecycle_rig(db.pool().clone(), 1, false);
    let raw = upsert_payload("capacity-held");
    let client = tokio::spawn(answer(
        router(rig.state.clone()),
        signed(&raw, "old", "intent-capacity-held"),
    ));
    wait_entered(&mut rig.entered_rx).await;
    // Dropping the handler future is what a client disconnect and the
    // handler request timeout have in common; the admitted child survives
    // by design and must keep holding the one capacity slot.
    client.abort();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut probes = Vec::new();
    for probe in 0..2 {
        let probe_raw = upsert_payload(&format!("capacity-probe-{probe}"));
        let probe_result = tokio::time::timeout(
            Duration::from_millis(1500),
            answer(
                router(rig.state.clone()),
                signed(&probe_raw, "old", &format!("intent-capacity-probe-{probe}")),
            ),
        )
        .await;
        probes.push(probe_result);
    }
    // Cleanup before assertions so no parked child leaks past the test.
    rig.release_tx.send_replace(true);
    let completed = wait_completed(db.pool(), 1).await;
    let restored = wait_capacity_restored(&rig.state, 1).await;
    let calls = rig.barrier.calls();
    for (probe, probe_result) in probes.into_iter().enumerate() {
        let (status, _, body) = probe_result.unwrap_or_else(|_| {
            panic!(
                "probe {probe} was admitted while the detached child still held \
                 the only capacity slot (no prompt busy refusal)"
            )
        });
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "probe {probe} must be refused busy while the child is unfinished"
        );
        assert_eq!(body["error"]["code"], "busy", "probe {probe}");
    }
    assert_eq!(
        calls, 1,
        "refused probes must not spawn further mutation effects"
    );
    assert_eq!(completed, 1, "the admitted child persists its receipt");
    assert_eq!(restored, 1, "capacity returns only after completion");
    db.close().await.unwrap();
}

#[tokio::test]
async fn receiver_shutdown_joins_child_during_effect_before_reporting_clean() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let mut rig = lifecycle_rig(db.pool().clone(), MAX_REQUESTS, false);
    let (addr, shutdown_tx, mut serve_task) = bound_serve(rig.state.clone()).await;
    let raw = upsert_payload("drain-effect");
    let stream = send_signed_tcp(addr, &raw, "intent-drain-effect").await;
    wait_entered(&mut rig.entered_rx).await;
    drop(stream);
    shutdown_tx.send_replace(true);
    let early = poll_serve(&mut serve_task, Duration::from_millis(400)).await;
    let still_draining = early.is_none();
    rig.release_tx.send_replace(true);
    let serve_result = match early {
        Some(result) => result,
        None => poll_serve(&mut serve_task, Duration::from_secs(10))
            .await
            .expect("serve finishes once the drained child completes"),
    };
    let completed = completed_count(db.pool()).await;
    let store = InternalActionStore::new(db.pool().clone());
    let mapped = store
        .event_id_for_key(staging_guild(), "drain-effect")
        .await
        .unwrap();
    assert!(
        still_draining,
        "serve reported completion while the admitted child was still in its effect"
    );
    assert!(
        serve_result.is_ok(),
        "clean drain completes Ok: {serve_result:?}"
    );
    assert_eq!(completed, 1, "receipt persisted before serve returned");
    assert_eq!(mapped.as_deref(), Some(CREATE_EVENT_ID));
    db.close().await.unwrap();
}

#[tokio::test]
async fn receiver_shutdown_joins_child_during_receipt_persistence() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let mut rig = lifecycle_rig(db.pool().clone(), MAX_REQUESTS, false);
    let (addr, shutdown_tx, mut serve_task) = bound_serve(rig.state.clone()).await;
    let raw = upsert_payload("drain-persist");
    let stream = send_signed_tcp(addr, &raw, "intent-drain-persist").await;
    wait_entered(&mut rig.entered_rx).await;
    // Hold the terminal-persistence tables so the child, once its effect
    // returns, blocks in event-key mapping / receipt finish rather than in
    // the effect. The claim already committed before the effect ran.
    let mut lock_tx = db.pool().begin().await.unwrap();
    sqlx::query(
        "LOCK TABLE internal_event_keys, internal_idempotency, internal_action_log \
         IN ACCESS EXCLUSIVE MODE",
    )
    .execute(&mut *lock_tx)
    .await
    .unwrap();
    drop(stream);
    rig.release_tx.send_replace(true);
    let mut blocked_in_persistence = false;
    for _ in 0..200 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_locks l JOIN pg_class c ON c.oid = l.relation \
             WHERE NOT l.granted AND c.relname IN \
             ('internal_event_keys', 'internal_idempotency', 'internal_action_log')",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        if waiting > 0 {
            blocked_in_persistence = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    shutdown_tx.send_replace(true);
    let early = poll_serve(&mut serve_task, Duration::from_millis(400)).await;
    let still_draining = early.is_none();
    lock_tx.rollback().await.unwrap();
    let serve_result = match early {
        Some(result) => result,
        None => poll_serve(&mut serve_task, Duration::from_secs(10))
            .await
            .expect("serve finishes once persistence unblocks and drains"),
    };
    let completed = completed_count(db.pool()).await;
    let store = InternalActionStore::new(db.pool().clone());
    let mapped = store
        .event_id_for_key(staging_guild(), "drain-persist")
        .await
        .unwrap();
    assert!(
        blocked_in_persistence,
        "setup: child never blocked in terminal persistence"
    );
    assert!(
        still_draining,
        "serve reported completion while the child was still persisting its receipt"
    );
    assert!(
        serve_result.is_ok(),
        "clean drain completes Ok: {serve_result:?}"
    );
    assert_eq!(completed, 1, "receipt persisted before serve returned");
    assert_eq!(mapped.as_deref(), Some(CREATE_EVENT_ID));
    db.close().await.unwrap();
}

#[tokio::test]
async fn receiver_shutdown_deadline_is_bounded_and_never_reports_clean_or_allows_second_effect() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let mut rig = lifecycle_rig(db.pool().clone(), MAX_REQUESTS, false);
    let state_for_replay = rig.state.clone();
    let (addr, shutdown_tx, serve_task) = bound_serve(rig.state.clone()).await;
    let raw = upsert_payload("drain-deadline");
    let stream = send_signed_tcp(addr, &raw, "intent-drain-deadline").await;
    wait_entered(&mut rig.entered_rx).await;
    drop(stream);
    shutdown_tx.send_replace(true);
    // The child is never released before the deadline: serve must still
    // return within the existing bounded shutdown budget (allow generous
    // headroom over it), but must not report clean completion.
    let serve_result = tokio::time::timeout(Duration::from_secs(25), serve_task)
        .await
        .expect("serve returns within a bounded shutdown budget, never an unbounded wait")
        .expect("serve task panicked");
    let completed_at_return = completed_count(db.pool()).await;
    // Whatever the deadline did to the child, the durable fence decides the
    // replay: releasing the effect now must not produce a second effect.
    rig.release_tx.send_replace(true);
    let _ = wait_completed(db.pool(), 1).await;
    let (replay_status, _, _) = answer(
        router(state_for_replay),
        signed(&raw, "old", "intent-drain-deadline"),
    )
    .await;
    let calls = rig.barrier.calls();
    assert_eq!(
        completed_at_return, 0,
        "no receipt was persisted when the deadline expired"
    );
    assert!(
        serve_result.is_err(),
        "serve must not report clean completion with tracked work undrained at the deadline"
    );
    assert!(
        matches!(replay_status, StatusCode::OK | StatusCode::CONFLICT),
        "replay is the stored receipt or durable uncertainty, got {replay_status}"
    );
    assert_eq!(
        calls, 1,
        "cancellation/deadline never permits a second effect"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn clean_shutdown_after_completed_receipt_and_no_new_admission() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let rig = lifecycle_rig(db.pool().clone(), MAX_REQUESTS, true);
    let (addr, shutdown_tx, mut serve_task) = bound_serve(rig.state.clone()).await;
    let raw = upsert_payload("drain-clean");
    let mut stream = send_signed_tcp(addr, &raw, "intent-drain-clean").await;
    let status = read_status(&mut stream).await;
    shutdown_tx.send_replace(true);
    let serve_result = poll_serve(&mut serve_task, Duration::from_secs(10))
        .await
        .expect("serve with no in-flight children drains promptly");
    // After shutdown, a fresh mutation must not be admitted: either the
    // socket is refused or no claim/effect appears for it.
    if let Ok(mut late) = tokio::net::TcpStream::connect(addr).await {
        let late_raw = upsert_payload("drain-late");
        let request = signed(&late_raw, "old", "intent-drain-late");
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
        let _ = late.write_all(head.as_bytes()).await;
        let _ = late.write_all(&bytes).await;
        let mut sink = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), late.read_to_end(&mut sink)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let claims: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(status, 200, "admitted mutation completes before shutdown");
    assert!(
        serve_result.is_ok(),
        "clean shutdown completes Ok: {serve_result:?}"
    );
    assert_eq!(claims, 1, "no new mutation claim after shutdown");
    assert_eq!(rig.barrier.calls(), 1, "no new effect after shutdown");
    db.close().await.unwrap();
}

#[tokio::test]
async fn cancelled_client_receipt_replays_without_second_effect_and_capacity_is_restored() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let mut rig = lifecycle_rig(db.pool().clone(), 1, false);
    let raw = upsert_payload("cancel-replay");
    let client = tokio::spawn(answer(
        router(rig.state.clone()),
        signed(&raw, "old", "intent-cancel-replay"),
    ));
    wait_entered(&mut rig.entered_rx).await;
    client.abort();
    rig.release_tx.send_replace(true);
    assert_eq!(
        wait_completed(db.pool(), 1).await,
        1,
        "admitted work persists its receipt without its client"
    );
    assert_eq!(wait_capacity_restored(&rig.state, 1).await, 1);
    let (status, headers, replay) = answer(
        router(rig.state.clone()),
        signed(&raw, "old", "intent-cancel-replay"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(
        replay["result"],
        serde_json::json!({"outcome": "created", "event_id": CREATE_EVENT_ID})
    );
    assert_eq!(rig.barrier.calls(), 1, "replay never re-runs the effect");
    let fresh_raw = upsert_payload("cancel-fresh");
    let (fresh_status, _, _) = answer(
        router(rig.state.clone()),
        signed(&fresh_raw, "old", "intent-cancel-fresh"),
    )
    .await;
    assert_eq!(
        fresh_status,
        StatusCode::OK,
        "capacity is usable again after the drained child completed"
    );
    assert_eq!(
        rig.barrier.calls(),
        2,
        "only the fresh mutation adds an effect"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn signature_gate_still_refuses_while_child_in_flight() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let mut rig = lifecycle_rig(db.pool().clone(), MAX_REQUESTS, false);
    let raw = upsert_payload("gate-inflight");
    let client = tokio::spawn(answer(
        router(rig.state.clone()),
        signed(&raw, "old", "intent-gate-inflight"),
    ));
    wait_entered(&mut rig.entered_rx).await;
    let mut forged = signed(&raw, "old", "intent-gate-forged");
    forged.headers_mut().insert(
        "x-two-signature",
        HeaderValue::from_static("sha256=bad-signature"),
    );
    let (status, _, body) = answer(router(rig.state.clone()), forged).await;
    rig.release_tx.send_replace(true);
    let (client_status, _, _) = client.await.unwrap();
    let claims: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert_eq!(client_status, StatusCode::OK);
    assert_eq!(claims, 1, "the forged request takes no claim");
    assert_eq!(rig.barrier.calls(), 1, "the forged request runs no effect");
    db.close().await.unwrap();
}
