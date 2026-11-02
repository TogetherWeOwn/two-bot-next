#![cfg(feature = "db")]

//! Protected reply fixtures through the real router, service and executor.
//! Only Discord HTTP is doubled; storage uses an isolated disposable database.
//! A valid underscore-containing name is still Markdown, even after validation.

#[allow(dead_code)]
mod common;

use std::sync::Arc;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    custom_command_store as store, custom_commands::StoredCommand, InteractionRouter, RouterGates,
};
use two_bot_discord::{custom_commands::CustomCommandRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const TOKEN: &str = "echo-acceptance-fixture";

fn runtime(pool: sqlx::PgPool, mock: &MockRest) -> CustomCommandRuntime {
    let mut router = InteractionRouter::new(RouterGates {
        configured_guild: Some(2222),
        automations: true,
        moderation: false,
        voice: false,
        voice_assistant: false,
        scorecard: false,
        announcements: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    CustomCommandRuntime::register(&mut router);
    CustomCommandRuntime::new(
        pool,
        Arc::new(router),
        ActionExecutor::with_proxy(TOKEN.into(), Some(mock.origin())).expect("loopback executor"),
        1111,
    )
}

fn slash(id: u64, command: &str, name: &str) -> Interaction {
    let mut options = vec![json!({"name": "name", "type": 3, "value": name})];
    if command == "command" {
        options.push(json!({"name": "template", "type": 3, "value": "Saved answer"}));
    }
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": TOKEN, "version": 1,
        "guild_id": "2222", "channel": {"id": "4444", "type": 0, "name": "commands"},
        "authorizing_integration_owners": {"0": "2222"}, "entitlements": [],
        "member": {
            "permissions": "32", "roles": [], "joined_at": null,
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null}
        },
        "data": {"id": "5555", "name": command, "type": 1, "options": options}
    }))
    .expect("Twilight management interaction")
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("isolated migrated database")
}

async fn discord() -> MockRest {
    MockRest::with_responder(|request| {
        if request.method == "POST" && request.path.ends_with("/callback") {
            ScriptedResponse::status(204)
        } else if request.method == "PATCH" && request.path.ends_with("/messages/@original") {
            ScriptedResponse::json(200, json!({"id": "9000"}))
        } else if request.method == "PUT"
            && request.path == "/api/v10/applications/1111/guilds/2222/commands"
        {
            let mut rows: Value = serde_json::from_slice(&request.body).expect("registry JSON");
            for (index, row) in rows
                .as_array_mut()
                .expect("registry list")
                .iter_mut()
                .enumerate()
            {
                row["id"] = json!((8000 + index).to_string());
                row["application_id"] = json!("1111");
                row["version"] = json!("1");
                row["type"] = json!(1);
            }
            ScriptedResponse::json(200, rows)
        } else {
            ScriptedResponse::status(403)
        }
    })
    .await
}

async fn seed(pool: &sqlx::PgPool, name: &str, enabled: bool) {
    store::put_command(
        pool,
        &StoredCommand {
            guild_id: "2222".into(),
            name: name.into(),
            description: "Saved response".into(),
            template: "Saved answer".into(),
            text_trigger: None,
            enabled,
        },
        "3333",
        "3333",
        "2026-10-01T00:00:00Z",
        "2026-10-01T00:00:00Z",
    )
    .await
    .expect("seed valid stored definition");
}

async fn observe(
    runtime: &CustomCommandRuntime,
    mock: &MockRest,
    input: Interaction,
) -> Vec<RestRequest> {
    let start = mock.requests().len();
    assert!(runtime
        .handle_interaction(&input, None)
        .await
        .expect("routed management reply"));
    mock.requests()[start..].to_vec()
}

fn content(requests: &[RestRequest], publishes: usize) -> String {
    assert_eq!(
        requests.iter().filter(|r| r.method == "PUT").count(),
        publishes
    );
    assert!(
        requests.iter().all(|r| {
            (r.method == "POST" && r.path.ends_with("/callback"))
                || (r.method == "PATCH" && r.path.ends_with("/messages/@original"))
                || (r.method == "PUT"
                    && r.path == "/api/v10/applications/1111/guilds/2222/commands")
        }),
        "management must not send a public channel message: {requests:?}"
    );
    let callbacks: Vec<_> = requests.iter().filter(|r| r.method == "POST").collect();
    assert_eq!(callbacks.len(), 1);
    let callback: Value = serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    assert_eq!(callback["data"]["flags"].as_u64().unwrap_or(0) & 64, 64);
    assert_eq!(callback["data"]["allowed_mentions"]["parse"], json!([]));
    let body = if callback["type"] == 5 {
        let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
        assert_eq!(edits.len(), 1, "finish the deferral exactly once");
        serde_json::from_slice::<Value>(&edits[0].body).expect("edit JSON")
    } else {
        assert_eq!(callback["type"], 4);
        assert_eq!(requests.len(), 1 + publishes);
        callback["data"].clone()
    };
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    for key in ["users", "roles"] {
        assert!(
            body["allowed_mentions"][key].is_null() || body["allowed_mentions"][key] == json!([])
        );
    }
    assert_ne!(body["allowed_mentions"]["replied_user"], true);
    assert!(body["embeds"].is_null() || body["embeds"] == json!([]));
    let text = body["content"].as_str().expect("text receipt");
    assert!(text.encode_utf16().count() <= 2000);
    text.to_owned()
}

#[tokio::test]
async fn absent_delete_escapes_markdown_backticks_links_and_newlines() {
    let db = database().await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock);
    let mut observed = Vec::new();
    for (index, (name, escaped, stripped)) in [
        ("**forged**", "\\*\\*forged\\*\\*", "forged"),
        ("`forged`", "\\`forged\\`", "forged"),
        ("bad\nforged\rline", "bad forged line", "bad forged line"),
        (
            "[forged](example.org)",
            "\\[forged\\](example.org)",
            "forgedexample.org",
        ),
        ("back\\slash", "back\\\\slash", "backslash"),
    ]
    .into_iter()
    .enumerate()
    {
        observed.push((
            observe(
                &runtime,
                &mock,
                slash(100 + index as u64, "command-remove", name),
            )
            .await,
            escaped,
            stripped,
        ));
    }
    let audit: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT action, outcome, reason FROM automation_audit_log ORDER BY id")
            .fetch_all(db.pool())
            .await
            .expect("audit outcomes");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_commands")
        .fetch_one(db.pool())
        .await
        .expect("row count");
    mock.shutdown().await;
    drop(runtime);
    db.close().await.expect("explicit teardown");
    assert_eq!(rows, 0);
    assert_eq!(
        audit,
        vec![("command.delete".into(), "absent".into(), None); 5]
    );
    for (requests, escaped, stripped) in observed {
        let text = content(&requests, 0);
        assert!(
            [
                format!("No command /{escaped} found."),
                format!("No command /{stripped} found."),
            ]
            .contains(&text),
            "active or unreadable command echo: {text:?}"
        );
    }
}

#[tokio::test]
async fn absent_delete_mentions_are_neutral_and_cannot_escape_into_markdown() {
    let db = database().await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock);
    let requests = observe(
        &runtime,
        &mock,
        slash(200, "command-remove", "`<@&8888> @everyone\n**forged**"),
    )
    .await;
    mock.shutdown().await;
    drop(runtime);
    db.close().await.expect("explicit teardown");
    let text = content(&requests, 0);
    assert!(
        [
            "No command /\\`<@&8888> @\u{200b}everyone \\*\\*forged\\*\\* found.",
            "No command /@&8888 @\u{200b}everyone forged found.",
        ]
        .contains(&text.as_str()),
        "unsafe mention-shaped echo: {text:?}"
    );
}

#[tokio::test]
async fn valid_underscore_names_are_inert_in_save_remove_and_toggle_receipts() {
    let db = database().await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock);
    let mut observed = Vec::new();
    for (index, (command, expected, publishes)) in [
        ("command-enable", "No command /\\_\\_forged\\_\\_ found.", 0),
        (
            "command-disable",
            "No command /\\_\\_forged\\_\\_ found.",
            0,
        ),
        ("command", "Saved /\\_\\_forged\\_\\_.", 1),
        (
            "command-enable",
            "/\\_\\_forged\\_\\_ is already enabled.",
            0,
        ),
        ("command-disable", "Disabled /\\_\\_forged\\_\\_.", 1),
        (
            "command-disable",
            "/\\_\\_forged\\_\\_ is already disabled.",
            0,
        ),
        ("command-enable", "Enabled /\\_\\_forged\\_\\_.", 1),
        ("command-remove", "Removed /\\_\\_forged\\_\\_.", 1),
    ]
    .into_iter()
    .enumerate()
    {
        observed.push((
            observe(
                &runtime,
                &mock,
                slash(300 + index as u64, command, "__forged__"),
            )
            .await,
            expected,
            publishes,
        ));
    }
    mock.shutdown().await;
    drop(runtime);
    db.close().await.expect("explicit teardown");
    for (requests, expected, publishes) in observed {
        assert_eq!(content(&requests, publishes), expected);
    }
}

#[tokio::test]
async fn ordinary_names_and_invalid_toggle_refusals_remain_byte_identical() {
    let db = database().await;
    seed(db.pool(), "faq", true).await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock);
    let mut observed = Vec::new();
    for (index, (command, name, expected, publishes)) in [
        ("command-enable", "faq", "/faq is already enabled.", 0),
        ("command-disable", "faq", "Disabled /faq.", 1),
        ("command-disable", "faq", "/faq is already disabled.", 0),
        ("command-enable", "faq", "Enabled /faq.", 1),
        ("command-remove", "faq", "Removed /faq.", 1),
        ("command-remove", "faq", "No command /faq found.", 0),
        ("command-enable", "faq", "No command /faq found.", 0),
        (
            "command-disable",
            "`bad`\n@everyone",
            "Command name must match ^[a-z0-9_-]{1,32}$.",
            0,
        ),
        (
            "command-enable",
            "`bad`\n@everyone",
            "Command name must match ^[a-z0-9_-]{1,32}$.",
            0,
        ),
        ("command", "faq", "Saved /faq.", 1),
    ]
    .into_iter()
    .enumerate()
    {
        observed.push((
            observe(&runtime, &mock, slash(400 + index as u64, command, name)).await,
            expected,
            publishes,
        ));
    }
    mock.shutdown().await;
    drop(runtime);
    db.close().await.expect("explicit teardown");
    for (requests, expected, publishes) in observed {
        assert_eq!(content(&requests, publishes), expected);
    }
}
