#![cfg(test)]
//! Strict HyperTransport seam on real Postgres: nothing reaches the wire
//! unless a strict claim is armed, and a 429 stays fenced until its exact
//! generation settles, however old the ordinary lease has become.
use super::*;
use crate::executor::{HyperTransport, StrictTransportError};
use tokio::time::Instant;

fn strict_request() -> twilight_http::request::Request {
    twilight_http::request::Request::builder(&twilight_http::routing::Route::CreateMessage {
        channel_id: CHANNEL.parse().unwrap(),
    })
    .body(br#"{"content":"fixture"}"#.to_vec())
    .build()
    .unwrap()
}

fn deadline() -> Option<Instant> {
    Some(Instant::now() + Duration::from_secs(10))
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct Lane {
    generation: i64,
    in_flight: bool,
    pending: Option<i64>,
    indefinite: bool,
    held: bool,
}

async fn lane(db: &TestDatabase) -> Lane {
    sqlx::query_as(
        "SELECT generation, in_flight, pending_settlement_generation AS pending, indefinite, \
                hold_until_ms > (extract(epoch FROM clock_timestamp()) * 1000)::bigint AS held \
         FROM public.discord_send_admission",
    )
    .fetch_one(db.pool())
    .await
    .unwrap()
}

/// Age the claim far beyond the ordinary lease, as a stalled sender would.
async fn age_past_lease(db: &TestDatabase) {
    sqlx::query("UPDATE public.discord_send_admission SET in_flight_since_ms = 0")
        .execute(db.pool())
        .await
        .unwrap();
}

async fn assert_fenced(gate: &PgSendAdmission, strict: &PgSendAdmission) {
    assert!(matches!(gate.admit().await, Err(AdmissionError::Blocked)));
    assert!(matches!(
        strict.admit_strict().await,
        Err(AdmissionError::Blocked)
    ));
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_strict_transport_blocked_lane_sends_zero_requests() {
    let db = database().await;
    let mock = MockDiscord::start(Reply::success()).await;
    let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
    let transport =
        HyperTransport::with_admission(token(), Some(mock.origin.clone()), gate.clone()).unwrap();
    let held = gate.admit().await.unwrap();
    let before = lane(&db).await;
    let blocked = transport
        .send_strict_request_headers(&strict_request(), deadline())
        .await
        .err();
    assert!(matches!(
        blocked,
        Some(StrictTransportError::Admission(AdmissionError::Blocked))
    ));
    assert_eq!(mock.count(), 0, "blocked strict send must stay pre-wire");
    assert_eq!(lane(&db).await, before, "refusal must not touch the lane");
    drop(held); // no release on drop; the ordinary claim stays occupied.
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_strict_transport_429_stalled_settlement_fences_past_lease() {
    let db = database().await;
    let second = db.independent_pool().await.unwrap();
    let mock = MockDiscord::start(Reply::new(429, r#"{"retry_after":120,"global":false}"#)).await;
    let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
    let other = PgSendAdmission::new(second.clone(), &token()).unwrap();
    let transport =
        HyperTransport::with_admission(token(), Some(mock.origin.clone()), gate).unwrap();
    let pending = transport
        .send_strict_request_headers(&strict_request(), deadline())
        .await
        .ok()
        .expect("strict claim must be armed and headers received");
    assert_eq!(mock.count(), 1);
    // Headers are in but settlement has not run: the exact claim is pending.
    let armed = lane(&db).await;
    assert!(armed.in_flight && !armed.indefinite);
    assert_eq!(armed.pending, Some(armed.generation));
    // The 429 arrived, yet an ordinary lease expiry must not reopen the lane.
    age_past_lease(&db).await;
    assert_fenced(&other, &other).await;
    assert_eq!(lane(&db).await, armed, "refused claims must not bump it");
    assert_eq!(pending.settle_status().await, 429);
    let settled = lane(&db).await;
    assert_eq!(settled.generation, armed.generation);
    assert!(!settled.in_flight, "exact generation settlement releases");
    assert_eq!(settled.pending, None);
    // Status-only settlement carries no body timing: the hold stays shared.
    assert!(settled.indefinite || settled.held);
    assert_fenced(&other, &other).await;
    assert_eq!(mock.count(), 1, "no second wire attempt after the 429");
    second.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_strict_transport_429_stalled_body_stays_fenced_until_collect() {
    let db = database().await;
    let second = db.independent_pool().await.unwrap();
    let mut reply = Reply::new(429, r#"{"retry_after":120,"global":false}"#);
    reply.headers = "retry-after: 120\r\n".to_owned();
    reply.stall_body = true;
    let mock = MockDiscord::start(reply).await;
    let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
    let other = PgSendAdmission::new(second.clone(), &token()).unwrap();
    let transport =
        HyperTransport::with_admission(token(), Some(mock.origin.clone()), gate).unwrap();
    let pending = transport
        .send_strict_request_headers(&strict_request(), deadline())
        .await
        .ok()
        .expect("strict claim must be armed and headers received");
    let armed = lane(&db).await;
    assert_eq!(armed.pending, Some(armed.generation));
    let observer = async {
        // The body is stalled mid-collect; the aged claim still cannot be taken.
        tokio::time::sleep(Duration::from_millis(300)).await;
        age_past_lease(&db).await;
        assert_fenced(&other, &other).await;
        assert_eq!(lane(&db).await, armed);
    };
    let (collected, ()) = tokio::join!(pending.collect(), observer);
    let (response, _global) = collected.unwrap();
    assert_eq!(response.status, 429);
    let settled = lane(&db).await;
    assert_eq!(settled.generation, armed.generation);
    assert!(!settled.in_flight && settled.pending.is_none());
    assert!(settled.held, "header retry-after survives the broken body");
    assert_fenced(&other, &other).await;
    assert_eq!(mock.count(), 1);
    second.close().await;
    db.close().await.unwrap();
}
