use super::*;
use crate::{
    executor::{HyperTransport, PacingLane::Shared, PACE_INTERVAL_MS},
    strict_transport_fixture::{loopback, read_request, Admission},
};
use std::sync::{atomic::Ordering::Relaxed, Arc};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    time::timeout,
};
use two_bot_core::send_admission::{AdmissionError, SendAdmission, SendCooldown};
use VoiceMoveNotSent::{Blocked, Deadline, Guard, Storage};
use VoiceMovePatch::{Denied, Moved, NotSent, Stale, Unavailable};

const TOKEN: &str = "voice-patch-fixture";
const WAIT: Duration = Duration::from_secs(3);
const MEMBER: &str = r#"{"user":{"id":"2","username":"fixture","discriminator":"0"},"roles":[],"flags":0,"deaf":false,"mute":false}"#;
type Sending = tokio::task::JoinHandle<VoiceMovePatch>;
type Completion = Option<SendCooldown>;

fn executor(origin: &str, gate: Arc<dyn SendAdmission>) -> ActionExecutor {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let origin = Some(origin.to_owned());
    let transport = HyperTransport::with_admission(TOKEN.into(), origin.clone(), gate)
        .unwrap()
        .with_test_guard();
    ActionExecutor::assemble(TOKEN.into(), origin, transport).unwrap()
}

async fn move_member(action: &ActionExecutor) -> VoiceMovePatch {
    action.move_voice_member("1", "2", "3").await.unwrap()
}

async fn dispatch(action: &ActionExecutor, listener: &TcpListener) -> (Sending, TcpStream) {
    let action = action.clone();
    let sending = tokio::spawn(async move { move_member(&action).await });
    let (mut socket, _) = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    let wire = read_request(&mut socket).await;
    let text = std::str::from_utf8(&wire).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("PATCH /api/v10/guilds/1/members/2 HTTP/1.1\r\n"));
    let auth = "authorization: Bot voice-patch-fixture";
    assert!(head.lines().any(|line| line.eq_ignore_ascii_case(auth)));
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body, serde_json::json!({"channel_id": "3"}));
    (sending, socket)
}

fn response(status: u16, header: &str, len: usize, body: &str) -> String {
    format!("HTTP/1.1 {status} Fixture\r\nConnection: close\r\n{header}Content-Length: {len}\r\n\r\n{body}")
}

async fn finish(sending: Sending) -> VoiceMovePatch {
    timeout(WAIT, sending).await.unwrap().unwrap()
}

async fn assert_no_socket(listener: &TcpListener) {
    let accepted = timeout(Duration::from_millis(30), listener.accept()).await;
    assert!(accepted.is_err());
}

async fn exchange(wire: &str, stall: bool, stuck: bool) -> (VoiceMovePatch, Vec<Completion>) {
    let (listener, origin) = loopback().await;
    let mut gate = Admission::new(TOKEN, None, None);
    gate.stall_completion = stuck;
    let gate = Arc::new(gate);
    let action = executor(&origin, gate.clone());
    let (sending, mut socket) = dispatch(&action, &listener).await;
    socket.write_all(wire.as_bytes()).await.unwrap();
    let _held = stall.then_some(socket);
    let result = finish(sending).await;
    assert_eq!(action.requests(), 1);
    assert_no_socket(&listener).await;
    let completions = std::mem::take(&mut *gate.completed.lock().unwrap());
    (result, completions)
}

#[tokio::test]
async fn voice_patch_requires_a_confirmed_member_receipt() {
    let wrong_member = MEMBER.replace(r#""id":"2""#, r#""id":"9""#);
    for (status, body, missing, stall, stuck, outcome, completes) in [
        (200, MEMBER, 0, false, false, Moved, true),
        (200, MEMBER, 0, false, true, Unavailable, true),
        (200, &wrong_member, 0, false, false, Unavailable, false),
        (200, "{}", 0, false, false, Unavailable, false),
        (200, "", 10, true, false, Unavailable, false),
        (200, "", 10, false, false, Unavailable, false),
        (202, MEMBER, 0, false, false, Unavailable, false),
        (204, "", 0, false, false, Unavailable, false),
        (302, "", 0, false, false, Unavailable, false),
        (500, "", 0, false, false, Unavailable, false),
        (0, "", 0, true, false, Unavailable, false), // Dispatched; stalled headers.
        (0, "", 0, false, false, Unavailable, false),
        (400, "", 10, true, false, Stale, true),
        (403, "", 10, true, false, Denied, true),
        (404, "", 10, true, false, Stale, true),
        (403, "", 10, true, true, Denied, true),
    ] {
        let mut wire = String::new();
        if status != 0 {
            wire = response(status, "", body.len() + missing, body);
        }
        let (result, completed) = exchange(&wire, stall, stuck).await;
        assert_eq!(result, outcome);
        assert_eq!(completed, completes.then_some(None).as_slice());
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
    let numeric = "Retry-After: 1.2\r\n";
    let date = "Retry-After: Thu, 01 Jan 2099 00:00:00 GMT\r\n";
    let fallback = "Retry-After: invalid\r\nX-RateLimit-Reset-After: 2\r\n";
    let longer_reset = "Retry-After: 1\r\nX-RateLimit-Reset-After: 2\r\n";
    for (head, body, missing, stall, stuck, mut delay) in [
        (numeric, r#"{"retry_after":3}"#, 0, false, false, Some(3000)),
        (numeric, r#"{"retry_after":3}"#, 0, false, true, Some(3000)),
        (numeric, "bad json", 0, false, false, Some(1200)),
        (numeric, "", 10, false, false, Some(1200)),
        (numeric, "", 10, true, false, Some(1200)),
        ("", "", 0, false, false, None),
        (fallback, "", 0, false, false, Some(2000)),
        (longer_reset, "", 0, false, false, Some(2000)),
        (date, "", 0, false, false, None),
    ] {
        let wire = response(429, head, body.len() + missing, body);
        let (result, completed) = exchange(&wire, stall, stuck).await;
        if head == date {
            if let VoiceMovePatch::Limited { retry_after_ms, .. } = result {
                delay = retry_after_ms;
            }
            assert!(delay.is_some_and(|ms| ms > 2000));
        }
        assert_eq!(result, limited(delay, !stall && !stuck));
        let cooldown = delay.map_or(SendCooldown::Indefinite, SendCooldown::FiniteMs);
        assert_eq!(completed, (!stall).then_some(Some(cooldown)).as_slice());
    }
}

#[tokio::test(start_paused = true)]
async fn voice_patch_pre_wire_refusals_never_connect() {
    for (refusal, pace, flip, why) in [
        (Some(AdmissionError::Blocked), false, false, Blocked),
        (Some(AdmissionError::Storage), false, false, Storage),
        (None, false, false, Guard),    // Closed before pacing.
        (None, true, false, Deadline),  // Pacing lock is held.
        (None, false, false, Deadline), // Durable claim not acknowledged.
        (None, false, true, Guard),     // Guard closes during claim.
    ] {
        let (listener, origin) = loopback().await;
        let mut gate = Admission::new(TOKEN, None, refusal);
        gate.resume_strict = Some(tokio::sync::Semaphore::new(0));
        let gate = Arc::new(gate);
        let action = executor(&origin, gate.clone());
        let guard = &action.inner.transport.guard;
        if why == Guard && !flip {
            guard.observe_status(401, true);
        }
        let _pace = pace.then(|| action.inner.pace_last_at.try_lock().unwrap());
        let mut sending = std::pin::pin!(move_member(&action));
        let pending = why == Deadline || flip;
        if pending {
            assert!(futures_util::poll!(&mut sending).is_pending());
            assert_eq!(gate.strict_admitted.load(Relaxed), usize::from(!pace));
            if flip {
                guard.observe_status(401, true);
                gate.resume_strict.as_ref().unwrap().add_permits(1);
            } else {
                tokio::time::advance(Duration::from_millis(VOICE_MOVE_TIMEOUT_MS)).await;
            }
        }
        assert_eq!(sending.await, NotSent(why));
        let claims = gate.strict_admitted.load(Relaxed);
        assert_eq!(claims, usize::from(pending && !pace));
        let completed = gate.completed.lock().unwrap().clone();
        assert_eq!(completed.as_slice(), flip.then_some(None).as_slice());
        assert_no_socket(&listener).await;
    }
}

#[tokio::test]
async fn voice_patch_uses_the_shared_paced_lane() {
    let (listener, origin) = loopback().await;
    let mut action = executor(&origin, Arc::new(Admission::new(TOKEN, None, None)));
    let (probe, mut admissions) = tokio::sync::mpsc::unbounded_channel();
    Arc::get_mut(&mut action.inner).unwrap().pacing_probe = Some(probe);
    let wire = response(200, "", MEMBER.len(), MEMBER);
    for _ in 0..2 {
        let (sending, mut socket) = dispatch(&action, &listener).await;
        socket.write_all(wire.as_bytes()).await.unwrap();
        assert_eq!(finish(sending).await, Moved);
    }
    let first = admissions.try_recv().unwrap();
    let second = admissions.try_recv().unwrap();
    assert_eq!((first.lane, second.lane), (Shared, Shared));
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

    async fn voice_fence(fail: bool) {
        let url = std::env::var("TWO_TEST_DATABASE_URL").expect("TWO_TEST_DATABASE_URL");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap();
        let sender = db.independent_pool().await.unwrap();
        let (listener, origin) = loopback().await;
        let gate = Arc::new(PgSendAdmission::new(sender.clone(), TOKEN).unwrap());
        let (sending, mut socket) = dispatch(&executor(&origin, gate), &listener).await;
        // Fail completion after the PATCH arrives, or leave the 429 body stalled.
        let wire = if fail {
            timeout(WAIT, sender.close()).await.unwrap();
            let body = r#"{"retry_after":10}"#;
            response(429, "Retry-After: 1\r\n", body.len(), body)
        } else {
            response(429, "", 20, "")
        };
        socket.write_all(wire.as_bytes()).await.unwrap();
        let result = finish(sending).await;
        assert_eq!(result, limited(fail.then_some(10_000), false));
        let fenced = lane(&db).await;
        assert_eq!(fenced, (fenced.0, true, Some(fenced.0), false));
        sqlx::query("UPDATE public.discord_send_admission SET in_flight_since_ms = 0")
            .execute(db.pool())
            .await
            .unwrap();
        let restart = db.independent_pool().await.unwrap();
        let gate = Arc::new(PgSendAdmission::new(restart.clone(), TOKEN).unwrap());
        let restarted = executor(&origin, gate);
        assert_eq!(move_member(&restarted).await, NotSent(Blocked));
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
