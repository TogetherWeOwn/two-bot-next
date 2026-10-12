//! REST boundary contracts from two-bot@bffccf3; all HTTP is loopback fixtures.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse, APP_ID, GUILD_ID};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use twilight_http::{request::Request, routing::Route};
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};
use two_bot_discord::executor::{self_roles::SelfRoleRestError, HyperTransport};
use two_bot_discord::ActionExecutor;

fn ms(value: &str) -> i64 {
    two_bot_core::parse_iso_millis(value).expect("timestamp")
}

// Source: two-bot@bffccf3 test/unit.membership-rest-observation.test.ts:9.
#[tokio::test]
async fn roster_evidence_precedes_delayed_headers_and_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buf[..n]);
        }
        assert!(String::from_utf8_lossy(&request)
            .starts_with("GET /api/v10/guilds/2222/members?limit=1000 "));
        tokio::time::sleep(Duration::from_millis(30)).await;
        let headers_at = two_bot_core::now_iso();
        let body = r#"[{"user":{"id":"3333"},"joined_at":"2026-09-29T00:00:00.000Z"}]"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let body_at = two_bot_core::now_iso();
        stream.write_all(body.as_bytes()).await.unwrap();
        (headers_at, body_at)
    });
    let exec = ActionExecutor::with_proxy("fixture-token".into(), Some(origin)).unwrap();
    let (data, observed_at) = exec
        .get_json_observed("/guilds/2222/members?limit=1000")
        .await
        .unwrap()
        .unwrap();
    let (headers_at, body_at) = server.await.unwrap();
    assert_eq!(data[0]["user"]["id"], "3333");
    assert!(ms(&observed_at) < ms(&headers_at));
    assert!(ms(&headers_at) < ms(&body_at));
    assert_eq!(exec.requests(), 1);
}

// Source: two-bot@bffccf3 test/unit.membership-rest-observation.test.ts:31.
#[tokio::test]
async fn retried_roster_evidence_uses_successful_attempt_start() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut successful_headers_at = String::new();
        let mut successful_body_at = String::new();
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
            }
            assert!(String::from_utf8_lossy(&request)
                .starts_with("GET /api/v10/guilds/2222/members?limit=1000 "));
            if attempt == 0 {
                stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                continue;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            successful_headers_at = two_bot_core::now_iso();
            let body = r#"[{"user":{"id":"3333"}}]"#;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
            successful_body_at = two_bot_core::now_iso();
            stream.write_all(body.as_bytes()).await.unwrap();
        }
        (successful_headers_at, successful_body_at)
    });
    let exec = ActionExecutor::with_proxy("fixture-token".into(), Some(origin)).unwrap();
    let began = two_bot_core::now_iso();
    let (data, observed_at) = exec
        .get_json_observed("/guilds/2222/members?limit=1000")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(data[0]["user"]["id"], "3333");
    assert!(
        ms(&observed_at) - ms(&began) >= 500,
        "failed attempt's timestamp must not survive retry"
    );
    let (headers_at, body_at) = server.await.unwrap();
    assert!(
        ms(&observed_at) < ms(&headers_at),
        "successful response headers must not replace request-start evidence"
    );
    assert!(ms(&headers_at) < ms(&body_at));
    assert_eq!(exec.requests(), 2);
}

// Source: two-bot@bffccf3 test/unit.membership-rest-observation.test.ts:55.
#[tokio::test]
async fn failed_roster_page_has_no_evidence_and_ordinary_get_is_data_only() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403)],
        ScriptedResponse::json(200, serde_json::json!({"value":1})),
    )
    .await;
    let exec = ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin())).unwrap();
    assert!(exec
        .get_json_observed("/guilds/2222/members?limit=1000")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        exec.get_json("/channels/4444").await.unwrap(),
        Some(serde_json::json!({"value":1}))
    );
    mock.shutdown().await;
}

/// A read whose headers never arrive must fail once its attempts each hit the
/// moderation bound instead of parking its caller. The clock is paused so the
/// bounds elapse without wall time; the mock's 600 s delay can never win.
#[tokio::test(start_paused = true)]
async fn observed_read_with_stalled_headers_times_out_each_attempt() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, serde_json::json!([])).delayed(Duration::from_secs(600)),
    )
    .await;
    let exec = ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin())).unwrap();
    let began = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        exec.get_json_observed("/guilds/2222/roles"),
    )
    .await
    .expect("a stalled roles read must not outlive the moderation bound");
    assert!(result.is_err(), "stall is an error, not absent: {result:?}");
    let budget = u64::from(two_bot_core::MAX_HTTP_TRIES);
    assert_eq!(
        mock.requests().len() as u64,
        budget,
        "one bound per attempt"
    );
    assert!(began.elapsed() < Duration::from_secs(5 * budget + 10));
    mock.shutdown().await;
}

/// Success control for the bound above: a prompt answer is unaffected.
#[tokio::test]
async fn observed_read_with_prompt_headers_still_returns_data() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, serde_json::json!([{"id":"1"}])),
    )
    .await;
    let exec = ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin())).unwrap();
    let (data, _) = exec
        .get_json_observed("/guilds/2222/roles")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(data[0]["id"], "1");
    mock.shutdown().await;
}

/// Single-flight lane mirroring `PgSendAdmission` take-or-`Blocked`
/// semantics: the first `admit` takes the lane and later ones are `Blocked`
/// until the holder's `complete` releases it.
#[derive(Debug)]
struct SingleFlightLane {
    key: TokenKey,
    held: Arc<Mutex<bool>>,
    admits: AtomicU32,
    blocked: AtomicU32,
    released: Arc<AtomicU32>,
}

struct SingleFlightRelease {
    held: Arc<Mutex<bool>>,
    released: Arc<AtomicU32>,
}

impl SendCompletion for SingleFlightRelease {
    fn complete(
        self: Box<Self>,
        _cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        *self.held.lock().unwrap() = false;
        self.released.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

impl SendAdmission for SingleFlightLane {
    fn token_key(&self) -> &TokenKey {
        &self.key
    }

    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        self.admits.fetch_add(1, Ordering::SeqCst);
        if *self.held.lock().unwrap() {
            self.blocked.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(AdmissionError::Blocked) })
        } else {
            *self.held.lock().unwrap() = true;
            let held = Arc::clone(&self.held);
            let released = Arc::clone(&self.released);
            Box::pin(async move {
                // Real admission pays latency (a DB round trip) before
                // granting. An instant grant lets nested wire timers tie and
                // hides a missing outer margin: the outer abort would win the
                // tie and orphan the permit, but the test would still pass.
                // This pause pins the ordering — only a header timer that
                // lives inside the permit scope (where a GET releases the
                // lane) keeps the retries `Blocked`-free.
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(AdmissionPermit::new(Box::new(SingleFlightRelease {
                    held,
                    released,
                })))
            })
        }
    }
}

/// A governed read whose headers never arrive must retry on the wire instead
/// of being `Blocked` by its own orphaned permit, and the lane must be free
/// afterwards so the next send reaches the wire. The clock is paused so the
/// bounds elapse without wall time; the mock's 600 s delay can never win.
#[tokio::test(start_paused = true)]
async fn governed_observed_read_with_stalled_headers_retries_each_attempt_and_releases_lane() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, serde_json::json!([])).delayed(Duration::from_secs(600)),
    )
    .await;
    let lane = Arc::new(SingleFlightLane {
        key: TokenKey::for_bot_token("fixture-token").unwrap(),
        held: Arc::new(Mutex::new(false)),
        admits: AtomicU32::new(0),
        blocked: AtomicU32::new(0),
        released: Arc::new(AtomicU32::new(0)),
    });
    let exec = ActionExecutor::with_admission(
        "fixture-token".to_owned(),
        Some(mock.origin()),
        Arc::clone(&lane) as Arc<dyn SendAdmission>,
    )
    .expect("governed executor builds against loopback");
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        exec.get_json_observed("/guilds/2222/roles"),
    )
    .await
    .expect("a stalled roles read must not outlive the moderation bound");
    assert!(result.is_err(), "stall is an error, not absent: {result:?}");
    let budget = u64::from(two_bot_core::MAX_HTTP_TRIES);
    assert_eq!(
        mock.requests().len() as u64,
        budget,
        "one wire request per attempt"
    );
    assert_eq!(
        lane.admits.load(Ordering::SeqCst) as u64,
        budget,
        "one admission per attempt"
    );
    assert_eq!(
        lane.blocked.load(Ordering::SeqCst),
        0,
        "retries must not be Blocked by the read's own permit"
    );
    assert!(
        !*lane.held.lock().unwrap(),
        "a timed-out GET must release the lane"
    );
    assert_eq!(
        lane.released.load(Ordering::SeqCst) as u64,
        budget,
        "each timed-out GET attempt releases its permit"
    );
    // The lane is immediately usable again: a direct admit succeeds (a held
    // lane would return `Blocked`). Retries already proved wire reachability
    // (one wire request per attempt, none `Blocked`).
    let permit = lane
        .admit()
        .await
        .expect("the lane must admit once the timed-out GETs release it");
    permit.complete(None).await.expect("test lane completion");
    mock.shutdown().await;
}

/// Durable admission shares the caller's absolute deadline: it is a Postgres
/// statement on a pooled connection, not a local lock, so an admit that
/// sleeps past the moderation budget must time out without reaching the
/// wire — not resolve late, past the budget the caller exists to protect.
/// A lane whose admit sleeps 6 s against a prompt mock: five attempts, zero
/// wire requests, lane never held.
#[tokio::test(start_paused = true)]
async fn governed_observed_read_with_sleeping_admit_times_out_without_wire() {
    #[derive(Debug)]
    struct SleepingLane {
        key: TokenKey,
        held: Arc<Mutex<bool>>,
        admits: AtomicU32,
    }

    impl SendAdmission for SleepingLane {
        fn token_key(&self) -> &TokenKey {
            &self.key
        }

        fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
            self.admits.fetch_add(1, Ordering::SeqCst);
            let held = Arc::clone(&self.held);
            Box::pin(async move {
                // Sleep past the 5 s moderation budget: the transport must
                // abort this admit at the absolute deadline instead of
                // waiting it out and sending late.
                tokio::time::sleep(Duration::from_secs(6)).await;
                *held.lock().unwrap() = true;
                Ok(AdmissionPermit::new(Box::new(SingleFlightRelease {
                    held,
                    released: Arc::new(AtomicU32::new(0)),
                })))
            })
        }
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, serde_json::json!([]))).await;
    let lane = Arc::new(SleepingLane {
        key: TokenKey::for_bot_token("fixture-token").unwrap(),
        held: Arc::new(Mutex::new(false)),
        admits: AtomicU32::new(0),
    });
    let exec = ActionExecutor::with_admission(
        "fixture-token".to_owned(),
        Some(mock.origin()),
        Arc::clone(&lane) as Arc<dyn SendAdmission>,
    )
    .expect("governed executor builds against loopback");
    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        exec.get_json_observed("/guilds/2222/roles"),
    )
    .await
    .expect("a sleeping admit must not outlive the moderation bound");
    let elapsed = started.elapsed();
    assert!(result.is_err(), "stall is an error, not absent: {result:?}");
    // Five 5 s admit deadlines plus 0.5/1/2/4 s backoff settle at ~32.5 s.
    // An admit outside the deadline sleeps the full 6 s per attempt (~37.5 s),
    // so the bound pins the deadline to the admit path, not just the outcome.
    assert!(
        elapsed < Duration::from_secs(35),
        "five bounded admits must settle under 35 s, took {elapsed:?}"
    );
    assert_eq!(
        mock.requests().len(),
        0,
        "an admit that never resolves must never reach the wire"
    );
    assert_eq!(
        lane.admits.load(Ordering::SeqCst) as u64,
        u64::from(two_bot_core::MAX_HTTP_TRIES),
        "one timed-out admission per attempt"
    );
    assert!(
        !*lane.held.lock().unwrap(),
        "a timed-out admit must never take the lane"
    );
    mock.shutdown().await;
}

/// A GET whose response stalls must release the lane where the permit is in
/// scope: otherwise the retries are `Blocked` by the read's own orphaned
/// permit until the 60 s lease. Five wire requests, zero `Blocked`, lane
/// free and re-admittable afterwards. Under a paused clock the virtual time
/// jumps to the attempt deadline before loopback headers are processed, so
/// this always ends in the header arm; the body arms below are pinned at the
/// transport in real time instead.
#[tokio::test(start_paused = true)]
async fn governed_observed_read_with_stalled_body_releases_lane() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, serde_json::json!([])).delayed_body(Duration::from_secs(600)),
    )
    .await;
    let lane = Arc::new(SingleFlightLane {
        key: TokenKey::for_bot_token("fixture-token").unwrap(),
        held: Arc::new(Mutex::new(false)),
        admits: AtomicU32::new(0),
        blocked: AtomicU32::new(0),
        released: Arc::new(AtomicU32::new(0)),
    });
    let exec = ActionExecutor::with_admission(
        "fixture-token".to_owned(),
        Some(mock.origin()),
        Arc::clone(&lane) as Arc<dyn SendAdmission>,
    )
    .expect("governed executor builds against loopback");
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        exec.get_json_observed("/guilds/2222/roles"),
    )
    .await
    .expect("a stalled body must not outlive the attempt budgets");
    assert!(result.is_err(), "stall is an error, not absent: {result:?}");
    let budget = u64::from(two_bot_core::MAX_HTTP_TRIES);
    assert_eq!(
        mock.requests().len() as u64,
        budget,
        "one wire request per attempt"
    );
    assert_eq!(
        lane.blocked.load(Ordering::SeqCst),
        0,
        "retries must not be Blocked by the read's own body-stalled permit"
    );
    assert!(
        !*lane.held.lock().unwrap(),
        "a body-timed-out GET must release the lane"
    );
    assert_eq!(
        lane.released.load(Ordering::SeqCst) as u64,
        budget,
        "each body-timed-out GET attempt releases its permit"
    );
    let permit = lane
        .admit()
        .await
        .expect("the lane must admit once the body-timed-out GETs release it");
    permit.complete(None).await.expect("test lane completion");
    mock.shutdown().await;
}

/// A GET whose headers arrive but whose body stalls must take the body-timeout
/// arm and release the lane: headers land in milliseconds on loopback, so a
/// short real-time deadline proves the error is the body bound (not the header
/// one) and that the wait stays near the deadline instead of the 30 s stall.
/// Deleting the release in the body-timeout arm orphans the permit (a second
/// admit is `Blocked`, the lane stays held).
#[tokio::test]
async fn transport_stalled_body_times_out_and_releases_lane() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, serde_json::json!([])).delayed_body(Duration::from_secs(30)),
    )
    .await;
    let lane = Arc::new(SingleFlightLane {
        key: TokenKey::for_bot_token("fixture-token").unwrap(),
        held: Arc::new(Mutex::new(false)),
        admits: AtomicU32::new(0),
        blocked: AtomicU32::new(0),
        released: Arc::new(AtomicU32::new(0)),
    });
    let transport = HyperTransport::with_admission(
        "fixture-token".to_owned(),
        Some(mock.origin()),
        Arc::clone(&lane) as Arc<dyn SendAdmission>,
    )
    .expect("governed transport builds against loopback");
    let request = Request::from_route(&Route::GetGuildRoles { guild_id: 2222 });
    let started = tokio::time::Instant::now();
    let result = transport
        .send_request(&request, Some(started + Duration::from_secs(2)))
        .await;
    let elapsed = started.elapsed();
    assert_eq!(
        result.map(|_| ()),
        Err("read body timed out".to_owned()),
        "headers arrived, so the stall must report the body bound"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "the body bound must win near the 2 s deadline, not the 30 s stall: {elapsed:?}"
    );
    assert_eq!(mock.requests().len(), 1, "one wire request");
    assert!(
        !*lane.held.lock().unwrap(),
        "a body-timed-out GET must release the lane"
    );
    assert_eq!(
        lane.released.load(Ordering::SeqCst),
        1,
        "the body-timed-out attempt releases its permit"
    );
    let permit = lane
        .admit()
        .await
        .expect("the lane must admit once the body-timed-out GET releases it");
    assert_eq!(
        lane.blocked.load(Ordering::SeqCst),
        0,
        "no Blocked on the read's own permit"
    );
    permit.complete(None).await.expect("test lane completion");
    mock.shutdown().await;
}

/// A stalled mutation's outcome is unknown, so its lane stays held and a
/// follow-up send is `Blocked` until the uncertainty is resolved.
async fn post_stall_holds_lane(response: ScriptedResponse, expected: &str) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock = MockRest::start(vec![], response).await;
    let lane = Arc::new(SingleFlightLane {
        key: TokenKey::for_bot_token("fixture-token").unwrap(),
        held: Arc::new(Mutex::new(false)),
        admits: AtomicU32::new(0),
        blocked: AtomicU32::new(0),
        released: Arc::new(AtomicU32::new(0)),
    });
    let transport = HyperTransport::with_admission(
        "fixture-token".to_owned(),
        Some(mock.origin()),
        Arc::clone(&lane) as Arc<dyn SendAdmission>,
    )
    .expect("governed transport builds against loopback");
    let request = Request::builder(&Route::CreateMessage { channel_id: 3333 })
        .body(b"{}".to_vec())
        .build()
        .expect("message request builds");
    let result = transport
        .send_request(
            &request,
            Some(tokio::time::Instant::now() + Duration::from_millis(600)),
        )
        .await;
    assert_eq!(result.map(|_| ()), Err(expected.to_owned()));
    assert_eq!(
        lane.released.load(Ordering::SeqCst),
        0,
        "an unanswered mutation must not release its permit"
    );
    assert!(
        *lane.held.lock().unwrap(),
        "an unanswered mutation must keep the lane held"
    );
    assert!(
        matches!(lane.admit().await, Err(AdmissionError::Blocked)),
        "a follow-up send must stay Blocked while the mutation is uncertain"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn transport_post_stalled_headers_keeps_lane_held() {
    post_stall_holds_lane(
        ScriptedResponse::json(200, serde_json::json!({})).delayed(Duration::from_secs(30)),
        "read headers timed out",
    )
    .await;
}

#[tokio::test]
async fn transport_post_stalled_body_keeps_lane_held() {
    post_stall_holds_lane(
        ScriptedResponse::json(200, serde_json::json!({})).delayed_body(Duration::from_secs(30)),
        "read body timed out",
    )
    .await;
}

/// A GET whose body is truncated on the wire (fewer bytes than declared, then
/// close) must take the body-unavailable arm and release the lane, like a
/// stalled body. Deleting the release in that arm orphans the permit. An
/// inline server drives this: it closes with a byte missing, so the body
/// read errors instead of timing out.
/// (The scripted mock keeps a short connection open, which lands in the
/// timeout arm above — not this one.)
#[tokio::test]
async fn transport_truncated_body_releases_lane() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind truncate");
    let origin = format!("http://{}", listener.local_addr().expect("addr"));
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut buf = Vec::with_capacity(1024);
        let mut chunk = [0u8; 1024];
        while let Ok(n) = stream.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let body = b"[]";
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len() + 1,
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(body).await;
        // Drop the socket with a byte missing: the body read errors.
    });
    let lane = Arc::new(SingleFlightLane {
        key: TokenKey::for_bot_token("fixture-token").unwrap(),
        held: Arc::new(Mutex::new(false)),
        admits: AtomicU32::new(0),
        blocked: AtomicU32::new(0),
        released: Arc::new(AtomicU32::new(0)),
    });
    let transport = HyperTransport::with_admission(
        "fixture-token".to_owned(),
        Some(origin),
        Arc::clone(&lane) as Arc<dyn SendAdmission>,
    )
    .expect("governed transport builds against loopback");
    let request = Request::from_route(&Route::GetGuildRoles { guild_id: 2222 });
    let result = transport
        .send_request(
            &request,
            Some(tokio::time::Instant::now() + Duration::from_secs(10)),
        )
        .await;
    assert_eq!(
        result.map(|_| ()),
        Err("Discord response body unavailable".to_owned()),
        "a truncated body must report unavailability, not a partial read"
    );
    assert!(
        !*lane.held.lock().unwrap(),
        "a truncated GET body must release the lane"
    );
    assert_eq!(
        lane.released.load(Ordering::SeqCst),
        1,
        "the truncated attempt releases its permit"
    );
    let permit = lane
        .admit()
        .await
        .expect("the lane must admit once the truncated GET releases it");
    assert_eq!(
        lane.blocked.load(Ordering::SeqCst),
        0,
        "no Blocked on the read's own permit"
    );
    permit.complete(None).await.expect("test lane completion");
    handle.abort();
}

/// The self-role GET travels the budgeted wire path without an outer timer:
/// a stall must time out inside the permit scope and release the lane.
/// Single attempt, so exactly one wire request, one admission, one release.
#[tokio::test(start_paused = true)]
async fn governed_self_role_read_with_stalled_headers_releases_lane() {
    const CHANNEL: &str = "100000000000000007";
    const MESSAGE: &str = "100000000000000008";

    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(
            200,
            serde_json::json!({"id": MESSAGE, "channel_id": CHANNEL}),
        )
        .delayed(Duration::from_secs(600)),
    )
    .await;
    let lane = Arc::new(SingleFlightLane {
        key: TokenKey::for_bot_token("fixture-token").unwrap(),
        held: Arc::new(Mutex::new(false)),
        admits: AtomicU32::new(0),
        blocked: AtomicU32::new(0),
        released: Arc::new(AtomicU32::new(0)),
    });
    let exec = ActionExecutor::with_admission(
        "fixture-token".to_owned(),
        Some(mock.origin()),
        Arc::clone(&lane) as Arc<dyn SendAdmission>,
    )
    .expect("governed executor builds against loopback");
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        exec.fetch_self_role_message(CHANNEL, MESSAGE),
    )
    .await
    .expect("a stalled self-role read must not outlive the moderation bound");
    assert_eq!(
        result,
        Err(SelfRoleRestError::Ambiguous),
        "a stall stays Ambiguous, never a parsed partial snapshot"
    );
    assert_eq!(mock.requests().len(), 1, "one wire request");
    assert_eq!(lane.admits.load(Ordering::SeqCst), 1, "one admission");
    assert_eq!(
        lane.blocked.load(Ordering::SeqCst),
        0,
        "no Blocked on the read's own permit"
    );
    assert!(
        !*lane.held.lock().unwrap(),
        "a timed-out self-role GET must release the lane"
    );
    assert_eq!(
        lane.released.load(Ordering::SeqCst),
        1,
        "the timed-out GET attempt releases its permit"
    );
    let permit = lane
        .admit()
        .await
        .expect("the lane must admit once the timed-out GET releases it");
    permit.complete(None).await.expect("test lane completion");
    mock.shutdown().await;
}

/// The registry GET (`guild_commands`) travels the same budgeted wire path as
/// the observed reads: a stall must time out inside the permit scope and
/// release the lane instead of orphaning it. Single attempt (no retry on
/// timeouts there), so exactly one wire request, one admission, one release.
#[tokio::test(start_paused = true)]
async fn governed_guild_commands_with_stalled_headers_releases_lane() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, serde_json::json!([])).delayed(Duration::from_secs(600)),
    )
    .await;
    let lane = Arc::new(SingleFlightLane {
        key: TokenKey::for_bot_token("fixture-token").unwrap(),
        held: Arc::new(Mutex::new(false)),
        admits: AtomicU32::new(0),
        blocked: AtomicU32::new(0),
        released: Arc::new(AtomicU32::new(0)),
    });
    let exec = ActionExecutor::with_admission(
        "fixture-token".to_owned(),
        Some(mock.origin()),
        Arc::clone(&lane) as Arc<dyn SendAdmission>,
    )
    .expect("governed executor builds against loopback");
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        exec.guild_commands(APP_ID, GUILD_ID),
    )
    .await
    .expect("a stalled registry read must not outlive the moderation bound");
    assert!(
        result.is_err(),
        "stall is an error, not an empty registry: {result:?}"
    );
    assert_eq!(mock.requests().len(), 1, "one wire request");
    assert_eq!(lane.admits.load(Ordering::SeqCst), 1, "one admission");
    assert_eq!(
        lane.blocked.load(Ordering::SeqCst),
        0,
        "no Blocked on the read's own permit"
    );
    assert!(
        !*lane.held.lock().unwrap(),
        "a timed-out registry GET must release the lane"
    );
    assert_eq!(
        lane.released.load(Ordering::SeqCst),
        1,
        "the timed-out GET attempt releases its permit"
    );
    let permit = lane
        .admit()
        .await
        .expect("the lane must admit once the timed-out GET releases it");
    permit.complete(None).await.expect("test lane completion");
    mock.shutdown().await;
}
