#![cfg(test)]
//! Protected failing-first age-bound acceptance through the landed timestamp seam.
//! No live Discord or database: the REST fixture is loopback-only and the pool
//! is closed before use. A fresh snapshot must reach publication (`Database`);
//! a stale snapshot must skip successfully with `StaleSnapshot` before storage.
//! This proves retention by excluding every storage operation, not by asserting
//! on a test-written imitation of the mirror. Persisted byte equality needs the
//! builder's disposable-DB integration coverage; normalization is pinned here.

use super::*;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use two_bot_core::{EventStatus, ScheduledEvent};

fn response_rows() -> Value {
    json!([
        {
            "id": "3000", "guild_id": "2222", "name": "Café 🎉",
            "scheduled_start_time": "2026-10-25T19:00:00Z", "status": 1,
            "channel_id": "4444", "description": "Line one\nLine two — unchanged"
        },
        {
            "id": "3001", "guild_id": "2222", "name": "Second event",
            "scheduled_start_time": "2026-10-26T20:30:00.000Z", "status": 2,
            "channel_id": null, "description": null
        }
    ])
}

/// Only the slow boundaries are substituted: real executor, response parsing,
/// normalization and snapshot orchestration run. Closing the lazy pool prevents
/// DNS, sockets and database writes even if the production age gate is missing.
async fn age_attempt(body: Value, age_seconds: i64) -> (Result<(), ErrorClass>, String) {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, body)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    let pool = unconnected_pool();
    pool.close().await;
    let (_stop, shutdown) = watch::channel(false);
    let capture = tracing_capture::global();
    let start = capture.text().len();
    let observed_at = (OffsetDateTime::now_utc() - time::Duration::seconds(age_seconds))
        .format(&Rfc3339)
        .unwrap();
    let began = std::time::Instant::now();
    let outcome = snapshot_events_once(&pool, &rest, "2222", &shutdown, &observed_at).await;
    let elapsed = began.elapsed();
    let logs = capture.text()[start..].to_owned();
    let requests = mock.requests();
    mock.shutdown().await;
    assert!(
        elapsed < Duration::from_secs(1),
        "fixture exceeded its one-second boundary margin; not age-bound RED: {elapsed:?}"
    );
    if age_seconds < 300 {
        assert_eq!(requests.len(), 1, "fresh snapshots must be read");
    } else {
        assert!(requests.len() <= 1, "a stale snapshot must not retry");
    }
    for request in requests {
        assert_eq!(request.method, "GET", "no Discord mutation");
    }
    assert_eq!(pool.size(), 0, "no database connection was opened");
    (outcome, logs)
}

fn assert_stale_skip(outcome: Result<(), ErrorClass>, logs: &str) {
    assert_eq!(
        outcome,
        Ok(()),
        "301-second-old snapshot must skip before storage; Database means the age gate is missing"
    );
    assert!(
        logs.lines().any(|line| line.contains("=StaleSnapshot")),
        "a successful no-op must report the exact StaleSnapshot outcome, not silently drop the read: {logs}"
    );
}

#[tokio::test]
async fn age_299_seconds_accepts_every_row_and_reaches_publication() {
    let body = response_rows();
    let raw: Vec<_> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|row| raw_event(row, "2222").unwrap())
        .collect();
    assert_eq!(
        normalize_events(&raw).unwrap(),
        vec![
            ScheduledEvent {
                id: "3000".to_owned(),
                name: "Café 🎉".to_owned(),
                starts_at: "2026-10-25T19:00:00.000Z".to_owned(),
                channel_id: Some("4444".to_owned()),
                description: Some("Line one\nLine two — unchanged".to_owned()),
                status: EventStatus::Scheduled,
            },
            ScheduledEvent {
                id: "3001".to_owned(),
                name: "Second event".to_owned(),
                starts_at: "2026-10-26T20:30:00.000Z".to_owned(),
                channel_id: None,
                description: None,
                status: EventStatus::Active,
            },
        ],
        "all normalized fields, UTF-8 bytes, nulls and row order stay unchanged"
    );
    let (outcome, _) = age_attempt(body, 299).await;
    assert_eq!(
        outcome,
        Err(ErrorClass::Database),
        "5 min minus 1 sec is accepted, not skipped"
    );
}

#[tokio::test]
async fn age_301_seconds_skips_whole_snapshot_before_any_publication() {
    let (outcome, logs) = age_attempt(response_rows(), 301).await;
    assert_stale_skip(outcome, &logs);
}

#[tokio::test]
async fn age_301_seconds_empty_snapshot_cannot_delete_last_good_rows() {
    let (outcome, logs) = age_attempt(json!([]), 301).await;
    assert_stale_skip(outcome, &logs);
}

#[tokio::test]
async fn age_301_seconds_at_row_cap_cannot_publish_a_truncated_prefix() {
    let rows: Vec<_> = (0..100)
        .map(|index| {
            let mut row = event();
            row["id"] = json!((3000 + index).to_string());
            row
        })
        .collect();
    let (outcome, logs) = age_attempt(json!(rows), 301).await;
    assert_stale_skip(outcome, &logs);
}

#[tokio::test]
async fn age_fresh_empty_snapshot_still_attempts_the_intentional_clear() {
    let (outcome, _) = age_attempt(json!([]), 0).await;
    assert_eq!(
        outcome,
        Err(ErrorClass::Database),
        "a fresh empty read must publish, unlike a stale empty read"
    );
}
