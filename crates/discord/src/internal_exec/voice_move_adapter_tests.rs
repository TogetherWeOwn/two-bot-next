use super::*;
use crate::{
    executor::{HyperTransport, PacingLane, PACE_INTERVAL_MS},
    strict_transport_fixture::{loopback, read_request, Admission},
};
use std::sync::{atomic::Ordering, Arc};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::{timeout, Instant},
};
use two_bot_core::send_admission::{AdmissionError, SendAdmission, SendCooldown};

const TOKEN: &str = "voice-patch-fixture";
const WAIT: Duration = Duration::from_secs(3);

fn executor(origin: &str, admission: Arc<dyn SendAdmission>) -> ActionExecutor {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let origin = Some(origin.to_owned());
    let transport = HyperTransport::with_admission(TOKEN.into(), origin.clone(), admission)
        .unwrap()
        .with_test_guard();
    ActionExecutor::assemble(TOKEN.into(), origin, transport).unwrap()
}

async fn move_member(action: &ActionExecutor) -> VoiceMovePatch {
    action.move_voice_member("1", "2", "3").await.unwrap()
}

async fn dispatch(
    action: &ActionExecutor,
    listener: &TcpListener,
) -> (JoinHandle<VoiceMovePatch>, TcpStream) {
    let action = action.clone();
    let sending = tokio::spawn(async move { move_member(&action).await });
    let (mut socket, _) = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    let request = read_request(&mut socket).await;
    let (headers, body) = std::str::from_utf8(&request)
        .unwrap()
        .split_once("\r\n\r\n")
        .unwrap();
    assert!(headers.starts_with("PATCH /api/v10/guilds/1/members/2 HTTP/1.1\r\n"));
    assert!(headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("authorization: Bot voice-patch-fixture")));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).unwrap(),
        serde_json::json!({"channel_id": "3"})
    );
    (sending, socket)
}

async fn reply(socket: &mut TcpStream, status: u16, header: &str, len: usize, body: &str) {
    let wire = format!("HTTP/1.1 {status} Fixture\r\nConnection: close\r\n{header}Content-Length: {len}\r\n\r\n{body}");
    socket.write_all(wire.as_bytes()).await.unwrap();
}

async fn finish(sending: JoinHandle<VoiceMovePatch>) -> VoiceMovePatch {
    timeout(WAIT, sending).await.unwrap().unwrap()
}

async fn assert_no_socket(listener: &TcpListener) {
    assert!(timeout(Duration::from_millis(30), listener.accept())
        .await
        .is_err());
}

async fn exchange(
    gate: Arc<Admission>,
    response: Option<(u16, &str, usize, &str)>,
    stall: bool,
) -> VoiceMovePatch {
    let (listener, origin) = loopback().await;
    let action = executor(&origin, gate);
    let (sending, mut socket) = dispatch(&action, &listener).await;
    if let Some((status, header, len, body)) = response {
        reply(&mut socket, status, header, len, body).await;
    }
    let stalled_socket = stall.then_some(socket);
    let result = finish(sending).await;
    assert_eq!(action.requests(), 1);
    assert_no_socket(&listener).await;
    drop(stalled_socket);
    result
}

#[tokio::test]
async fn voice_patch_incomplete_bodies_preserve_only_definitive_rejections() {
    for (status, expected) in [
        (400, VoiceMovePatch::Stale),
        (403, VoiceMovePatch::Denied),
        (404, VoiceMovePatch::Stale),
        (200, VoiceMovePatch::Unavailable),
    ] {
        for stall in [false, true] {
            let gate = Arc::new(Admission::new(TOKEN, None, None));
            let result = exchange(gate.clone(), Some((status, "", 10, "")), stall).await;
            assert_eq!(result, expected);
            assert_eq!(
                gate.completed.lock().unwrap().as_slice(),
                (status != 200).then_some(None).as_slice()
            );
        }
    }
}

#[tokio::test]
async fn voice_patch_requires_confirmed_success_and_preserves_uncertainty() {
    use VoiceMovePatch::{Denied, Moved, Unavailable};
    for (status, stall_headers, stall_completion, outcome, completes) in [
        (Some(204), false, false, Moved, true),
        (Some(204), false, true, Unavailable, true),
        (Some(302), false, false, Unavailable, false),
        (Some(500), false, false, Unavailable, false),
        (Some(403), false, true, Denied, true),
        (None, true, false, Unavailable, false),
        (None, false, false, Unavailable, false),
    ] {
        let mut gate = Admission::new(TOKEN, None, None);
        gate.stall_completion = stall_completion;
        let gate = Arc::new(gate);
        let response = status.map(|code| (code, "", if code == 403 { 6 } else { 0 }, ""));
        assert_eq!(
            exchange(gate.clone(), response, stall_headers).await,
            outcome
        );
        assert_eq!(gate.completed.lock().unwrap().len(), usize::from(completes));
    }
}

fn limited(retry_after_ms: Option<u64>, settlement_confirmed: bool) -> VoiceMovePatch {
    VoiceMovePatch::Limited {
        retry_after_ms,
        settlement_confirmed,
    }
}

#[tokio::test]
async fn voice_patch_429_preserves_best_timing_separate_from_settlement() {
    for (body, len, stalled_body, stalled_settlement, delay, confirmed) in [
        (r#"{"retry_after":3}"#, 17, false, false, Some(3000), true),
        (r#"{"retry_after":3}"#, 17, false, true, Some(3000), false),
        ("bad json", 8, false, false, Some(1200), true),
        ("", 10, false, false, Some(1200), true),
        ("", 10, true, false, Some(1200), false),
        ("", 0, false, false, None, true),
    ] {
        let mut gate = Admission::new(TOKEN, None, None);
        gate.stall_completion = stalled_settlement;
        let gate = Arc::new(gate);
        let header = delay.map_or("", |_| "Retry-After: 1.2\r\n");
        let result = exchange(gate.clone(), Some((429, header, len, body)), stalled_body).await;
        assert_eq!(result, limited(delay, confirmed));
        let expected = (!stalled_body)
            .then(|| Some(delay.map_or(SendCooldown::Indefinite, SendCooldown::FiniteMs)));
        assert_eq!(
            gate.completed.lock().unwrap().as_slice(),
            expected.as_slice()
        );
    }
}

#[tokio::test]
async fn voice_patch_pre_wire_refusals_are_typed() {
    for (refusal, expected) in [
        (Some(AdmissionError::Blocked), VoiceMoveNotSent::Blocked),
        (Some(AdmissionError::Storage), VoiceMoveNotSent::Storage),
        (None, VoiceMoveNotSent::Guard),
    ] {
        let (listener, origin) = loopback().await;
        let gate = Arc::new(Admission::new(TOKEN, None, refusal));
        let action = executor(&origin, gate.clone());
        if refusal.is_none() {
            action.inner.transport.guard.observe_status(401, true);
        }
        assert_eq!(
            action.move_voice_member("1", "2", "3").await.unwrap(),
            VoiceMovePatch::NotSent(expected)
        );
        assert_eq!(gate.strict_admitted.load(Ordering::Relaxed), 0);
        assert_no_socket(&listener).await;
    }
}

#[tokio::test]
async fn voice_patch_rejects_an_expired_budget_after_strict_admission() {
    let (listener, origin) = loopback().await;
    let gate = Arc::new(Admission::new(TOKEN, None, None));
    let action = executor(&origin, gate.clone());
    let request = voice_move::prepare_voice_move(&action.inner.factory, "1", "2", "3").unwrap();
    assert_eq!(
        action.send_voice_move(&request, Instant::now()).await,
        VoiceMovePatch::NotSent(VoiceMoveNotSent::Deadline)
    );
    assert_eq!(gate.strict_admitted.load(Ordering::Relaxed), 1);
    assert!(gate.completed.lock().unwrap().is_empty());
    assert_no_socket(&listener).await;
}

#[tokio::test]
async fn voice_patch_uses_the_shared_paced_lane() {
    let (listener, origin) = loopback().await;
    let mut action = executor(&origin, Arc::new(Admission::new(TOKEN, None, None)));
    let (probe, mut admissions) = tokio::sync::mpsc::unbounded_channel();
    Arc::get_mut(&mut action.inner).unwrap().pacing_probe = Some(probe);
    for _ in 0..2 {
        let (sending, mut socket) = dispatch(&action, &listener).await;
        reply(&mut socket, 204, "", 0, "").await;
        assert_eq!(finish(sending).await, VoiceMovePatch::Moved);
    }
    let first = admissions.try_recv().unwrap();
    let second = admissions.try_recv().unwrap();
    assert_eq!(first.lane, PacingLane::Shared);
    assert_eq!(second.lane, PacingLane::Shared);
    assert!(second.at.duration_since(first.at) >= Duration::from_millis(PACE_INTERVAL_MS));
    assert_eq!(action.requests(), 2);
    assert_no_socket(&listener).await;
}

#[cfg(feature = "db-tests")]
mod postgres {
    use super::*;
    use two_bot_core::send_admission::PgSendAdmission;
    use two_bot_testsupport::TestDatabase;

    async fn lane(db: &TestDatabase) -> (i64, bool, Option<i64>, bool) {
        sqlx::query_as("SELECT generation, in_flight, pending_settlement_generation, indefinite FROM public.discord_send_admission")
            .fetch_one(db.pool()).await.unwrap()
    }

    async fn voice_fence(fail_completion: bool) {
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test database URL required");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap();
        let sender = db.independent_pool().await.unwrap();
        let (listener, origin) = loopback().await;
        let gate = Arc::new(PgSendAdmission::new(sender.clone(), TOKEN).unwrap());
        let (sending, mut socket) = dispatch(&executor(&origin, gate), &listener).await;
        // Own the listener throughout. Once the PATCH arrives, close storage
        // before releasing a complete 429, or leave its body unwritten.
        if fail_completion {
            timeout(WAIT, sender.close()).await.unwrap();
            let body = r#"{"retry_after":10}"#;
            reply(&mut socket, 429, "Retry-After: 1\r\n", body.len(), body).await;
        } else {
            reply(&mut socket, 429, "", 20, "").await;
        }
        let result = finish(sending).await;
        let fenced = lane(&db).await;
        let (generation, in_flight, pending, indefinite) = fenced;
        let armed = in_flight && pending == Some(generation);
        if fail_completion {
            assert_eq!(result, limited(Some(10_000), false));
            assert!(armed);
        } else {
            assert!(matches!(
                result,
                VoiceMovePatch::Limited {
                    retry_after_ms: None,
                    ..
                }
            ));
            assert!(armed || indefinite);
        }
        sqlx::query("UPDATE public.discord_send_admission SET in_flight_since_ms = 0")
            .execute(db.pool())
            .await
            .unwrap();
        let restart = db.independent_pool().await.unwrap();
        let gate = Arc::new(PgSendAdmission::new(restart.clone(), TOKEN).unwrap());
        assert_eq!(
            move_member(&executor(&origin, gate)).await,
            VoiceMovePatch::NotSent(VoiceMoveNotSent::Blocked)
        );
        assert_eq!(lane(&db).await, fenced);
        assert_no_socket(&listener).await;
        drop(socket);
        restart.close().await;
        sender.close().await;
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn admission_voice_stalled_429_keeps_durable_fence_after_lease_expiry() {
        voice_fence(false).await;
    }

    #[tokio::test]
    async fn admission_voice_429_completion_failure_keeps_fence_after_lease_expiry() {
        voice_fence(true).await;
    }
}
