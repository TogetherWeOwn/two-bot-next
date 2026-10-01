//! Saved-session boot uses the production RESUMED dispatch, without READY or
//! a database connection. The shared executor talks only to the local mock.

use super::*;
use crate::{
    discord_test_common::{MockRest, ScriptedResponse},
    ticket_runtime::{TicketConfig, TicketRuntime},
};
use serde_json::json;
use std::time::Duration;

fn runtime(mock: &MockRest) -> (Arc<CommandRuntime>, Arc<TicketRuntime>) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    let executor =
        ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap();
    let tickets = Arc::new(
        TicketRuntime::new(
            pool.clone(),
            executor.clone(),
            TicketConfig {
                guild_id: "100".into(),
                category_id: "200".into(),
                panel_channel_id: "700".into(),
                staff_role_id: "300".into(),
                cooldown_seconds: 15,
            },
        )
        .unwrap(),
    );
    (
        CommandRuntime::with_tickets(pool, executor, Arc::clone(&tickets)),
        tickets,
    )
}

async fn wait_for(check: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("resumed dispatch completed");
}

#[tokio::test]
async fn resumed_without_ready_initializes_bot_user_and_wakes_tickets_independently_of_registry() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"400","bot":true})),
            ScriptedResponse::json(200, json!({"id":"900"})),
            ScriptedResponse::status(200),
        ],
        ScriptedResponse::json(200, json!({"id":"400","bot":true})),
    )
    .await;
    let (runtime, tickets) = runtime(&mock);
    assert_eq!(tickets.readiness_for_test(), (0, 0));
    // Registry synchronization cannot block ticket startup. This also fixes
    // mock request order without assuming the detached tasks' scheduling order.
    let registry = runtime.registry_synced.lock().await;
    runtime.dispatch(&Event::Resumed);
    wait_for(|| tickets.readiness_for_test() == (400, 1)).await;
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(mock.requests()[0].path, "/api/v10/users/@me");
    drop(registry);
    runtime.publish_registry(None).await;
    assert_eq!(tickets.readiness_for_test(), (400, 1));
    assert_eq!(mock.requests()[1].path, "/api/v10/applications/@me");
    assert_eq!(
        mock.requests()[2].path,
        "/api/v10/applications/900/guilds/100/commands"
    );
    // A later resume wakes ticket maintenance even when registry sync is a
    // no-op; its application id must never become the bot's author identity.
    runtime.dispatch(&Event::Resumed);
    wait_for(|| tickets.readiness_for_test() == (400, 2)).await;
    assert_eq!(mock.requests().len(), 4);
    assert!(mock
        .requests()
        .iter()
        .filter(|r| r.method == "GET")
        .all(|r| { r.path == "/api/v10/users/@me" || r.path == "/api/v10/applications/@me" }));
    mock.shutdown().await;
}

#[tokio::test]
async fn failed_resumed_identity_neither_initializes_nor_changes_ticket_readiness() {
    for response in [
        ScriptedResponse::status(403),
        ScriptedResponse::status(429),
        ScriptedResponse::status(500),
        ScriptedResponse::json(200, json!({"id":"900"})),
        ScriptedResponse::json(200, json!({"id":"400","bot":false})),
        ScriptedResponse::json(200, json!({"id":"0","bot":true})),
    ] {
        let mock = MockRest::start(vec![], response).await;
        let (runtime, tickets) = runtime(&mock);
        runtime.ready_tickets_after_resume().await;
        assert_eq!(tickets.readiness_for_test(), (0, 0));
        tickets.on_ready(401);
        runtime.ready_tickets_after_resume().await;
        assert_eq!(tickets.readiness_for_test(), (401, 1));
        assert_eq!(mock.requests().len(), 2, "single attempt per lookup");
        assert!(mock
            .requests()
            .iter()
            .all(|r| r.method == "GET" && r.path == "/api/v10/users/@me"));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn resumed_identity_lookup_is_cancelled_and_rejected_by_ticket_shutdown_scope() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, json!({"id":"400","bot":true}))
            .delayed(Duration::from_millis(200)),
    )
    .await;
    let (runtime, tickets) = runtime(&mock);
    let supervisor = runtime.start_tickets().unwrap();
    // Isolate the managed identity task from unrelated registry publication.
    *runtime.registry_synced.lock().await = true;
    runtime.dispatch(&Event::Resumed);
    wait_for(|| mock.requests().len() == 1).await;
    supervisor.shutdown().await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(tickets.readiness_for_test(), (0, 0));
    runtime.dispatch(&Event::Resumed);
    runtime.publish_registry(None).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        mock.requests().len(),
        1,
        "no detached lookup after shutdown"
    );
    assert_eq!(tickets.readiness_for_test(), (0, 0));
    mock.shutdown().await;
}
