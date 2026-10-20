#![cfg(feature = "db")]

//! Builder regressions for first-write-wins slash-run audit facts. Discord
//! traffic stays on loopback; TestDatabase enforces the disposable DB boundary.

#[allow(dead_code)]
mod common;

use std::sync::Arc;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    custom_command_store as store, custom_commands::StoredCommand, InteractionRouter, RouterGates,
};
use two_bot_discord::{
    custom_commands::{CustomCommandError, CustomCommandRuntime},
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

async fn fixture() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create disposable fixture");
    store::put_command(
        db.pool(),
        &StoredCommand {
            guild_id: "2222".to_owned(),
            name: "faq".to_owned(),
            description: "FAQ".to_owned(),
            template: "private template marker {user}".to_owned(),
            text_trigger: None,
            enabled: true,
        },
        "3333",
        "3333",
        "2026-10-20T00:00:00Z",
        "2026-10-20T00:00:00Z",
    )
    .await
    .expect("seed command");
    db
}

fn runtime(pool: sqlx::PgPool, mock: &MockRest) -> CustomCommandRuntime {
    let mut router = InteractionRouter::new(RouterGates {
        configured_guild: Some(2222),
        automations: true,
        scorecard: false,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    CustomCommandRuntime::register(&mut router);
    CustomCommandRuntime::new(
        pool,
        Arc::new(router),
        ActionExecutor::with_proxy("slash-replay-fixture".to_owned(), Some(mock.origin()))
            .expect("loopback executor"),
        1111,
    )
}

fn slash(id: u64) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": "slash-replay-fixture", "version": 1,
        "guild_id": "2222", "channel": {"id": "4444", "type": 0},
        "authorizing_integration_owners": {"0": "2222"}, "entitlements": [],
        "member": {
            "permissions": "0", "roles": [], "joined_at": null,
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null}
        },
        "data": {"id": "5555", "name": "faq", "type": 1}
    }))
    .expect("valid interaction fixture")
}

async fn audit_rows(pool: &sqlx::PgPool) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT to_jsonb(a) FROM automation_audit_log a WHERE action = 'command.run' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("read complete audit facts")
}

#[tokio::test]
async fn refused_slash_reentry_returns_delivery_and_preserves_first_audit_fact() {
    let db = fixture().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
    let interaction = slash(140);
    let first = runtime(db.pool().clone(), &mock)
        .handle_interaction(&interaction, Some("Test guild"))
        .await;
    assert!(matches!(first, Err(CustomCommandError::Delivery)));
    let before = audit_rows(db.pool()).await;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0]["id"], "custom:run:140");
    assert_eq!(before[0]["guild_id"], "2222");
    assert_eq!(before[0]["actor_id"], "3333");
    assert_eq!(before[0]["target_key"], "faq");
    assert_eq!(before[0]["outcome"], "failed");
    assert_eq!(before[0]["reason"], "delivery_failed");
    assert!(!before[0].to_string().contains("private template marker"));

    // A fresh runtime has no process-local deduplication to hide a duplicate
    // insert. Both calls still attempt their defer; neither retries it itself.
    let second = runtime(db.pool().clone(), &mock)
        .handle_interaction(&interaction, Some("Test guild"))
        .await;
    assert!(matches!(second, Err(CustomCommandError::Delivery)));
    assert_eq!(audit_rows(db.pool()).await, before, "including created_at");
    assert_eq!(mock.requests().len(), 2, "one defer per explicit call");
    assert!(mock
        .requests()
        .iter()
        .all(|request| { request.method == "POST" && request.path.ends_with("/callback") }));
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn refused_slash_audit_still_propagates_non_duplicate_storage_failure() {
    let db = fixture().await;
    sqlx::query(
        "ALTER TABLE automation_audit_log ADD CONSTRAINT reject_run_fixture CHECK (action <> 'command.run')",
    )
    .execute(db.pool())
    .await
    .expect("inject fixture-only audit failure");
    let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
    assert!(matches!(
        runtime(db.pool().clone(), &mock)
            .handle_interaction(&slash(142), Some("Test guild"))
            .await,
        Err(CustomCommandError::Storage)
    ));
    assert!(audit_rows(db.pool()).await.is_empty());
    assert_eq!(mock.requests().len(), 1, "no retry after the failed audit");
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn successful_reentry_after_refusal_preserves_the_first_fact_without_storage_error() {
    let db = fixture().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(403),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "9000"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let interaction = slash(141);
    assert!(matches!(
        runtime(db.pool().clone(), &mock)
            .handle_interaction(&interaction, Some("Test guild"))
            .await,
        Err(CustomCommandError::Delivery)
    ));
    let before = audit_rows(db.pool()).await;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0]["outcome"], "failed");
    assert_eq!(before[0]["reason"], "delivery_failed");

    assert!(runtime(db.pool().clone(), &mock)
        .handle_interaction(&interaction, Some("Test guild"))
        .await
        .expect("current delivery succeeds, duplicate audit is not Storage"));
    assert_eq!(
        audit_rows(db.pool()).await,
        before,
        "first fact is immutable"
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 3, "refused defer, accepted defer, edit");
    assert_eq!(requests[2].method, "PATCH");
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[2].body).expect("edit body")["content"],
        "private template marker <@3333>"
    );
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
}
