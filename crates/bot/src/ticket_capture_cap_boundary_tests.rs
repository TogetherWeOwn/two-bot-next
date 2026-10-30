#![cfg(test)]
//! Boundary pins for the ticket history fetch cap (review follow-up).
//!
//! The frozen acceptance suite covers a deep channel (51 pages) and a small
//! history (2 pages) but nothing on the cap boundary. These pins prove the
//! boundary probe distinguishes exactly-1000-message histories (complete,
//! no marker) from deeper ones (capped with the marker), and that a deep
//! close stores exactly the newest 1000 with the shared truncation marker.
//! All offline MockRest; no Discord, no database.

use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;

fn runtime(mock: &MockRest) -> TicketRuntime {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    let rest = ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap();
    let runtime = TicketRuntime::new(
        pool,
        rest,
        TicketConfig {
            guild_id: "100".into(),
            category_id: "200".into(),
            panel_channel_id: "700".into(),
            staff_role_id: "300".into(),
            cooldown_seconds: COOLDOWN_SECONDS,
        },
        std::collections::HashMap::new(),
    )
    .unwrap();
    runtime.set_bot_id(400);
    runtime
}

fn message(id: u64) -> Value {
    json!({
        "id":id.to_string(), "timestamp":two_bot_core::funnel::format_iso_millis(id as i64),
        "author":{"id":"500","username":"member","discriminator":"0"},
        "content":format!("message-{id}"), "attachments":[{"url":format!("https://example.test/{id}")}],
    })
}

/// Page `index` of the boundary channel: 100 ids, strictly descending from
/// 1201, so pages 0..10 plus a short probe cover every boundary shape.
fn full_page(index: usize) -> Vec<Value> {
    let top = 1201 - index as u64 * 100;
    (top - 99..=top).rev().map(message).collect()
}

fn history_fetches(mock: &MockRest) -> usize {
    mock.requests()
        .iter()
        .filter(|request| request.path.contains("limit=100"))
        .count()
}

#[tokio::test]
async fn exactly_thousand_messages_completes_without_marker() {
    let mut script: Vec<ScriptedResponse> = (0..10)
        .map(|index| ScriptedResponse::json(200, json!(full_page(index))))
        .collect();
    script.push(ScriptedResponse::json(200, json!([])));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let (snapshot, truncated) = runtime(&mock)
        .capture_history("600")
        .await
        .ok()
        .expect("exactly-1000 capture completes");
    assert!(!truncated, "exactly-1000 capture must report not truncated");
    assert_eq!(snapshot.message_count, 1000);
    assert!(
        !snapshot.content.to_lowercase().contains("truncat"),
        "a complete 1000-message history must not carry the truncation marker"
    );
    assert!(
        snapshot.content.contains("message-1201"),
        "newest message must be retained"
    );
    assert!(
        snapshot.content.contains("message-202"),
        "oldest message of the 1000 must be retained"
    );
    assert_eq!(history_fetches(&mock), 11);
    mock.shutdown().await;
}

#[tokio::test]
async fn ten_full_pages_plus_one_message_marks_capped() {
    let mut script: Vec<ScriptedResponse> = (0..10)
        .map(|index| ScriptedResponse::json(200, json!(full_page(index))))
        .collect();
    script.push(ScriptedResponse::json(200, json!([message(201)])));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let (snapshot, truncated) = runtime(&mock)
        .capture_history("600")
        .await
        .ok()
        .expect("1001-message capture completes");
    assert!(truncated, "1001-message capture must report truncated");
    assert_eq!(snapshot.message_count, 1000);
    assert!(
        snapshot.content.to_lowercase().contains("truncat"),
        "a 1001-message history must carry the truncation marker"
    );
    assert!(
        !snapshot.content.contains("message-201 "),
        "the probe page must not be stored past the 1000-message cap"
    );
    assert_eq!(history_fetches(&mock), 11);
    mock.shutdown().await;
}

#[tokio::test]
async fn deep_channel_stores_newest_thousand_with_marker() {
    let script: Vec<ScriptedResponse> = (0..12)
        .map(|index| ScriptedResponse::json(200, json!(full_page(index))))
        .collect();
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let (snapshot, truncated) = runtime(&mock)
        .capture_history("600")
        .await
        .ok()
        .expect("deep capture completes");
    assert!(truncated, "deep capture must report truncated");
    assert_eq!(
        snapshot.message_count, 1000,
        "deep close stores exactly the newest 1000"
    );
    assert!(
        snapshot.content.to_lowercase().contains("truncat"),
        "deep close must carry the truncation marker"
    );
    assert!(
        snapshot.content.contains("message-1201"),
        "newest message must be retained"
    );
    assert!(
        !snapshot.content.contains("message-102 "),
        "messages past the cap must not be stored"
    );
    assert_eq!(
        history_fetches(&mock),
        11,
        "deep capture stops after 10 data pages plus the boundary probe"
    );
    mock.shutdown().await;
}
