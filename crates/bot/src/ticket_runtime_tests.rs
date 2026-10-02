//! Mock-only proofs; lazy testdb pool never connects in these tests.
//! Timer proofs use Tokio's paused clock, not elapsed wall time.
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

#[test]
fn ordinary_messages_without_optional_components_do_not_block_panel_ensure() {
    let messages = panels(vec![
        json!({"author":{"id":"500"},"content":"ordinary message"}),
    ])
    .ok()
    .expect("ordinary Discord message");
    assert!(panel_needed("400", &messages));
    assert!(panels(vec![json!({"author":{"id":"500"},"components":{}})]).is_err());
}

#[tokio::test]
async fn ticket_history_fetches_all_pages_and_keeps_attachments_and_total_count() {
    let first: Vec<_> = (901..=1000).rev().map(message).collect();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!(first)),
            ScriptedResponse::json(200, json!([message(900), message(899)])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let snapshot = runtime(&mock)
        .capture_history("600")
        .await
        .ok()
        .expect("complete history");
    assert_eq!(snapshot.message_count, 102);
    assert!(snapshot
        .content
        .starts_with("[1970-01-01T00:00:00.899Z] member: message-899 https://example.test/899"));
    assert!(snapshot
        .content
        .ends_with("message-1000 https://example.test/1000"));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|request| request.method == "GET" && request.path.contains("limit=100")));
    assert!(requests[1].path.contains("before=901"));
    mock.shutdown().await;
}

#[tokio::test]
async fn truncated_transcript_still_fetches_later_pages_and_counts_every_message() {
    let mut first: Vec<_> = (901..=1000).rev().map(message).collect();
    first[0]["content"] = json!("😀".repeat(110_000));
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!(first)),
            ScriptedResponse::json(200, json!([message(900)])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let snapshot = runtime(&mock)
        .capture_history("600")
        .await
        .ok()
        .expect("complete capped history");
    assert_eq!(snapshot.message_count, 101);
    let retained = snapshot
        .content
        .strip_suffix("\n[transcript truncated]")
        .expect("truncation marker");
    assert!(retained.encode_utf16().count() <= MAX_TRANSCRIPT_UTF16_UNITS);
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn partial_duplicate_malformed_and_nonprogressing_history_never_complete() {
    let first: Vec<_> = (901..=1000).rev().map(message).collect();
    for second in [
        ScriptedResponse::status(500),
        ScriptedResponse::json(200, json!([message(999)])),
        ScriptedResponse::json(200, json!([message(900), message(900)])),
        ScriptedResponse::json(
            200,
            json!([{"id":"900","content":"missing attachments and timestamp"}]),
        ),
    ] {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(200, json!(first)), second],
            ScriptedResponse::status(500),
        )
        .await;
        assert!(runtime(&mock).capture_history("600").await.is_err());
        assert_eq!(mock.requests().len(), 2);
        assert!(mock
            .requests()
            .iter()
            .all(|request| request.method == "GET"));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn exact_full_page_requires_a_final_empty_page() {
    let first: Vec<_> = (901..=1000).rev().map(message).collect();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!(first)),
            ScriptedResponse::json(200, json!([])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let snapshot = runtime(&mock)
        .capture_history("600")
        .await
        .ok()
        .expect("final empty page");
    assert_eq!(snapshot.message_count, 100);
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

fn panel_channel() -> Value {
    let allow = (Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY).bits();
    json!({"id":"700","guild_id":"100","type":0,"permission_overwrites":[
        {"id":"400","type":1,"allow":allow.to_string(),"deny":"0"}
    ]})
}
fn panel(author: &str) -> Value {
    json!({"author":{"id":author},"components":[{"type":1,"components":[{"type":2,"custom_id":TICKET_OPEN_ID}]}]})
}

#[tokio::test]
async fn panel_ensure_trusts_only_this_bot_and_is_idempotent() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, panel_channel()),
            ScriptedResponse::json(200, json!([panel("500")])),
            ScriptedResponse::json(200, json!({"id":"800"})),
            ScriptedResponse::json(200, panel_channel()),
            ScriptedResponse::json(200, json!([panel("400")])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&mock);
    assert!(runtime.ensure_panel().await.is_ok());
    assert!(runtime.ensure_panel().await.is_ok());
    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );
    assert!(requests[1].path.contains("limit=50"));
    mock.shutdown().await;
}

#[tokio::test]
async fn panel_ensure_refuses_foreign_guild_and_access_failures_without_posting() {
    for response in [
        ScriptedResponse::json(200, json!({"id":"700","guild_id":"999","type":0})),
        ScriptedResponse::json(200, json!({"id":"700","guild_id":"100","type":2})),
        ScriptedResponse::json(403, json!({"code":50013})),
    ] {
        let mock = MockRest::start(vec![], response).await;
        assert!(runtime(&mock).ensure_panel().await.is_err());
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(mock.requests()[0].method, "GET");
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn history_requires_explicit_bot_view_and_read_overwrite() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&mock);
    let permissions = (Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY).bits();
    let doc = json!({"permission_overwrites":[{"id":"400","type":1,"allow":permissions.to_string(),"deny":"0"}]});
    assert!(runtime.require_history_access(&doc).is_ok());
    for doc in [
        json!({"permission_overwrites":[]}),
        json!({"permission_overwrites":[{"id":"400","type":0,"allow":permissions.to_string(),"deny":"0"}]}),
        json!({"permission_overwrites":[{"id":"400","type":1,"allow":permissions.to_string(),"deny":Permissions::READ_MESSAGE_HISTORY.bits().to_string()}]}),
    ] {
        assert!(runtime.require_history_access(&doc).is_err());
    }
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}
