//! REST boundary contracts from two-bot@bffccf3; all HTTP is loopback fixtures.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};
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
