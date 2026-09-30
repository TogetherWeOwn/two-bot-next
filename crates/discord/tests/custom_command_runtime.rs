#![cfg(feature = "db")]

//! Routed management + dynamic slash proof. REST is loopback-only; DB is
//! agent-testdb or an explicitly opted-in credential-free CI service.

#[allow(dead_code)]
mod common;

use std::sync::Arc;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{custom_command_store as store, InteractionRouter, RouterGates};
use two_bot_discord::{custom_commands::CustomCommandRuntime, ActionExecutor};

const MIGRATION: &str = include_str!("../../cutover/migrations/0130_custom_commands.sql");

fn gates(enabled: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        automations: enabled,
        moderation: false,
        scorecard: false,
        announcements: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn runtime(pool: sqlx::PgPool, mock: &MockRest, enabled: bool) -> CustomCommandRuntime {
    let mut router = InteractionRouter::new(gates(enabled));
    CustomCommandRuntime::register(&mut router);
    CustomCommandRuntime::new(
        pool,
        Arc::new(router),
        ActionExecutor::with_proxy("custom-command-fixture".to_owned(), Some(mock.origin()))
            .unwrap(),
        1111,
    )
}

fn slash(id: u64, name: &str, permissions: u64, options: &[(&str, &str)]) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": "custom-command-fixture", "version": 1,
        "guild_id": "2222", "channel": {"id": "4444", "type": 0, "name": "commands"},
        "authorizing_integration_owners": {"0": "2222"}, "entitlements": [],
        "member": {
            "permissions": permissions.to_string(), "roles": [], "deaf": false, "mute": false,
            "user": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null}
        },
        "data": {"id": "5555", "name": name, "type": 1,
            "options": options.iter().map(|(name, value)| json!({"name": name, "type": 3, "value": value})).collect::<Vec<_>>()}
    })).expect("valid Twilight interaction fixture")
}

fn test_options() -> PgConnectOptions {
    let ci_service = std::env::var("CI").as_deref() == Ok("true")
        && std::env::var("TWO_CUSTOM_COMMAND_TEST_CI").as_deref() == Ok("1");
    PgConnectOptions::new()
        .host(if ci_service {
            "127.0.0.1"
        } else {
            "agent-testdb"
        })
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test")
        .options([("statement_timeout", "5000ms")])
}

async fn test_pool() -> sqlx::PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                // The service commits its own transactions. Session-local tables
                // survive commits but disappear when this one-connection pool closes.
                sqlx::raw_sql(
                    "CREATE TEMP TABLE custom_command_scope (id INT); SET search_path = pg_temp;",
                )
                .execute(&mut *conn)
                .await?;
                sqlx::raw_sql(MIGRATION).execute(&mut *conn).await?;
                Ok(())
            })
        })
        .connect_with(test_options())
        .await
        .expect("authorized test DB")
}

fn bodies(mock: &MockRest) -> Vec<Value> {
    mock.requests()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

#[tokio::test]
async fn permissions_and_disabled_management_refuse_before_database_access() {
    let pool = PgPoolOptions::new().connect_lazy_with(test_options());
    for (enabled, permissions, expected) in [
        (true, 0, "Manage Server"),
        (false, 32, "Automations are disabled"),
    ] {
        let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
        let runtime = runtime(pool.clone(), &mock, enabled);
        assert!(runtime
            .handle_interaction(&slash(10, "command-list", permissions, &[]), None)
            .await
            .unwrap());
        let calls = bodies(&mock);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["type"], 4);
        assert_eq!(calls[0]["data"]["flags"], 64);
        assert!(calls[0]["data"]["content"]
            .as_str()
            .unwrap()
            .contains(expected));
        // Another feature's builtin is never answered here.
        assert!(!runtime
            .handle_interaction(&slash(11, "rank", 32, &[]), None)
            .await
            .unwrap());
        assert_eq!(mock.requests().len(), 1);
        mock.shutdown().await;
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn management_republishes_whole_registry_and_dynamic_execution_audits_delivery() {
    let pool = test_pool().await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({}))).await;
    let runtime = runtime(pool.clone(), &mock, true);
    runtime.sync_registry().await.unwrap();
    let options = [
        ("name", "faq"),
        ("template", "Hi {user} {username} in {server} {channel}"),
        ("text-trigger", "!FAQ"),
    ];
    assert!(runtime
        .handle_interaction(&slash(20, "command", 32, &options), Some("Test guild"))
        .await
        .unwrap());
    let row = store::get_command(&pool, "2222", "faq")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.text_trigger.as_deref(), Some("!faq"));
    assert!(runtime
        .handle_interaction(&slash(21, "faq", 0, &[]), Some("Test guild"))
        .await
        .unwrap());
    assert!(runtime
        .handle_interaction(&slash(22, "command-list", 32, &[]), None)
        .await
        .unwrap());
    assert!(runtime
        .handle_interaction(&slash(23, "command-remove", 32, &[("name", "faq")]), None)
        .await
        .unwrap());
    assert!(runtime
        .handle_interaction(&slash(24, "command-remove", 32, &[("name", "faq")]), None)
        .await
        .unwrap());
    let requests = mock.requests();
    let published = requests
        .iter()
        .filter(|request| request.method == "PUT")
        .map(|request| serde_json::from_slice::<Vec<Value>>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        published.len(),
        3,
        "READY, addition, deletion; absent delete does not republish"
    );
    for set in &published {
        assert!(set.iter().any(|command| command["name"] == "rank"));
        assert!(set.iter().any(|command| command["name"] == "command"));
    }
    assert!(!published[0].iter().any(|command| command["name"] == "faq"));
    assert!(published[1].iter().any(|command| command["name"] == "faq"));
    assert!(!published[2].iter().any(|command| command["name"] == "faq"));
    let edits = requests
        .iter()
        .filter(|request| request.method == "PATCH")
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert!(edits
        .iter()
        .any(|body| body["content"] == "Hi <@3333> tester in Test guild <#4444>"));
    for body in edits {
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    }
    let audit: Vec<(String, String)> =
        sqlx::query_as("SELECT action, outcome FROM automation_audit_log ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(audit.len(), 4);
    assert!(audit.contains(&("command.run".to_owned(), "ok".to_owned())));
    assert!(audit.contains(&("command.delete".to_owned(), "absent".to_owned())));
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn collision_and_audit_failure_do_not_mutate_and_publish_failure_is_honest() {
    let pool = test_pool().await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({}))).await;
    let runtime = runtime(pool.clone(), &mock, true);
    runtime
        .handle_interaction(
            &slash(
                30,
                "command",
                32,
                &[("name", "rank"), ("template", "shadow")],
            ),
            None,
        )
        .await
        .unwrap();
    assert!(store::get_command(&pool, "2222", "rank")
        .await
        .unwrap()
        .is_none());
    assert!(!mock
        .requests()
        .iter()
        .any(|request| request.method == "PUT"));
    // Reuse the same audit ID to force a transactional audit failure after write.
    assert!(runtime
        .handle_interaction(
            &slash(
                30,
                "command",
                32,
                &[("name", "faq"), ("template", "never committed")]
            ),
            None
        )
        .await
        .is_err());
    assert!(store::get_command(&pool, "2222", "faq")
        .await
        .unwrap()
        .is_none());
    mock.shutdown().await;

    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),          // defer
            ScriptedResponse::status(403),          // registry publish (no retry)
            ScriptedResponse::json(200, json!({})), // honest partial-result edit
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = self::runtime(pool.clone(), &mock, true);
    runtime
        .handle_interaction(
            &slash(31, "command", 32, &[("name", "faq"), ("template", "saved")]),
            None,
        )
        .await
        .unwrap();
    assert!(store::get_command(&pool, "2222", "faq")
        .await
        .unwrap()
        .is_some());
    assert!(bodies(&mock)[2]["content"]
        .as_str()
        .unwrap()
        .contains("Registry publication failed"));
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn disabled_and_unknown_dynamic_rows_and_failed_delivery() {
    let pool = test_pool().await;
    let input = two_bot_core::custom_commands::PutCommandInput {
        name: "faq".to_owned(),
        description: "FAQ".to_owned(),
        template: "hello".to_owned(),
        text_trigger: None,
    };
    two_bot_core::custom_command_service::put(
        &pool,
        true,
        "2222",
        "3333",
        &input,
        "seed",
        &two_bot_core::now_iso(),
    )
    .await
    .unwrap();
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let runtime = runtime(pool.clone(), &mock, false);
    assert!(runtime
        .handle_interaction(&slash(40, "faq", 0, &[]), None)
        .await
        .unwrap());
    assert_eq!(
        bodies(&mock)[0]["data"]["content"],
        "Automations are disabled on this server."
    );
    assert!(!runtime
        .handle_interaction(&slash(41, "unknown", 0, &[]), None)
        .await
        .unwrap());
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;

    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = self::runtime(pool.clone(), &mock, true);
    assert!(runtime
        .handle_interaction(&slash(42, "faq", 0, &[]), Some("Test guild"))
        .await
        .is_err());
    let fact: (String, String) = sqlx::query_as(
        "SELECT outcome, reason FROM automation_audit_log WHERE id = 'custom:run:42'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(fact, ("failed".to_owned(), "delivery_failed".to_owned()));
    assert_eq!(
        mock.requests().len(),
        2,
        "ambiguous delivery is not retried"
    );
    mock.shutdown().await;
    pool.close().await;
}
