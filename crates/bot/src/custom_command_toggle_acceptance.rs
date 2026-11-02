//! Protected gateway dispatch acceptance: management and ordinary invocations
//! must use the shared runtime, not an uncalled toggle helper.

#![cfg(test)]

use super::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use twilight_model::gateway::payload::incoming::MessageCreate;
use two_bot_core::{custom_command_store, custom_commands::StoredCommand};
use two_bot_testsupport::TestDatabase;

fn command_id(name: &str) -> u64 {
    match name {
        "command-disable" => 60001,
        "command-enable" => 60002,
        "faq" => 60003,
        _ => 60004,
    }
}

fn invocation(id: u64, name: &str, permissions: Permissions) -> Interaction {
    let options = if name == "faq" {
        vec![]
    } else {
        vec![option("name", CommandOptionValue::String("faq".to_owned()))]
    };
    let mut input = from_member(slash(name, Some(CHANNEL), options), 88, permissions);
    input.id = Id::new(id);
    input.token = format!("gateway-toggle-{id}");
    if let Some(InteractionData::ApplicationCommand(data)) = input.data.as_mut() {
        data.id = Id::new(command_id(name));
    }
    input
}

fn receipt(enabled: bool) -> String {
    let custom = two_bot_core::CustomCommand {
        name: "faq".to_owned(),
        description: "Saved response".to_owned(),
        enabled,
    };
    let definitions = router_with_commands(gates(true, false))
        .publish_set(&[custom])
        .expect("full gateway registry");
    let mut value = serde_json::to_value(two_bot_discord::interactions::publish_commands(
        &definitions,
    ))
    .expect("command wire");
    for (index, entry) in value
        .as_array_mut()
        .expect("registry")
        .iter_mut()
        .enumerate()
    {
        let name = entry["name"].as_str().expect("name").to_owned();
        let id = match name.as_str() {
            "command-disable" | "command-enable" | "faq" => command_id(&name),
            _ => 70000 + index as u64,
        };
        entry["id"] = json!(id.to_string());
        entry["application_id"] = json!("1111");
        entry["version"] = json!("1");
        entry["type"] = json!(1);
    }
    serde_json::to_string(&value).expect("fixture receipt")
}

async fn settle(runtime: &Arc<CommandRuntime>) {
    wait_for(
        || (0..6).all(|lane| runtime.lane_in_flight(lane) == 0),
        "toggle gateway work settles",
    )
    .await;
}

fn private_completion(requests: &[RestRequest], id: u64) -> String {
    let selected: Vec<_> = requests
        .iter()
        .filter(|r| r.path.contains(&format!("gateway-toggle-{id}")))
        .collect();
    assert_eq!(
        selected.len(),
        2,
        "one defer and one completion, no double fallback: {selected:?}"
    );
    assert_eq!(selected[0].method, "POST");
    let ack: Value = serde_json::from_slice(&selected[0].body).expect("ack JSON");
    assert_eq!(ack["type"], 5);
    assert_eq!(ack["data"]["flags"], 64);
    assert_eq!(selected[1].method, "PATCH");
    let reply: Value = serde_json::from_slice(&selected[1].body).expect("reply JSON");
    assert_eq!(reply["allowed_mentions"]["parse"], json!([]));
    reply["content"]
        .as_str()
        .expect("human-readable saved state")
        .to_owned()
}

#[tokio::test]
async fn toggle_acceptance_gateway_dispatch_pauses_and_resumes_the_saved_response() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable bootstrap");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded migrated test DB");
    custom_command_store::put_command(
        db.pool(),
        &StoredCommand {
            guild_id: GUILD_S.to_owned(),
            name: "faq".to_owned(),
            description: "Saved response".to_owned(),
            template: "Original {username} in {server}".to_owned(),
            text_trigger: Some("!faq".to_owned()),
            enabled: true,
        },
        "66",
        "67",
        "2026-10-01T00:00:00Z",
        "2026-10-01T00:00:00Z",
    )
    .await
    .expect("real stored command");
    let before: Value = sqlx::query_scalar("SELECT to_jsonb(c) FROM automation_commands c")
        .fetch_one(db.pool())
        .await
        .expect("definition before pause");
    let (mock, origin) = MockRest::start_script(vec![
        RestResponse::status(204),
        RestResponse {
            status: 200,
            body: Some(receipt(false)),
            delay: Duration::ZERO,
        },
        RestResponse::status(200),
    ])
    .await;
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor_at(origin),
        router_with_commands(gates(true, false)),
        GUILD,
        true,
    );
    let guard = runtime.dispatch_guard();
    runtime.initialize_custom_commands_for_test(
        crate::gateway_commands::GatewayCommandConfig::from_map_for_activation(
            GUILD,
            &HashMap::from([
                ("TWO_AUTOMATIONS".to_owned(), "1".to_owned()),
                ("TWO_TEXT_COMMANDS".to_owned(), "1".to_owned()),
                ("TWO_AUTOMOD".to_owned(), "0".to_owned()),
            ]),
            false,
        )
        .expect("explicit command/acceptance gates"),
    );
    assert!(runtime.dispatch(&interaction_event(invocation(
        8101,
        "command-disable",
        Permissions::MANAGE_GUILD
    ))));
    settle(&runtime).await;
    let paused: Value = sqlx::query_scalar("SELECT to_jsonb(c) FROM automation_commands c")
        .fetch_one(db.pool())
        .await
        .expect("definition after dispatch");
    let mut prefix = message(8102, CHANNEL, false, Some(GUILD));
    prefix.content = "!faq ordinary arguments".to_owned();
    assert!(runtime.dispatch(&Event::MessageCreate(Box::new(MessageCreate(prefix)))));
    settle(&runtime).await;
    assert!(runtime.dispatch(&interaction_event(invocation(
        8103,
        "faq",
        Permissions::empty()
    ))));
    settle(&runtime).await;
    let paused_requests = mock.requests();
    // Use a second mock/runtime only to reset fixture scripting and cooldowns;
    // all authoritative state and replay fences stay in the same database.
    drop(guard);
    drop(runtime);
    mock.shutdown().await;
    let (mock, origin) = MockRest::start_script(vec![
        RestResponse::status(204),
        RestResponse {
            status: 200,
            body: Some(receipt(true)),
            delay: Duration::ZERO,
        },
        RestResponse::status(200),
    ])
    .await;
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor_at(origin),
        router_with_commands(gates(true, false)),
        GUILD,
        true,
    );
    let guard = runtime.dispatch_guard();
    runtime.initialize_custom_commands_for_test(
        crate::gateway_commands::GatewayCommandConfig::from_map_for_activation(
            GUILD,
            &HashMap::from([
                ("TWO_AUTOMATIONS".to_owned(), "1".to_owned()),
                ("TWO_TEXT_COMMANDS".to_owned(), "1".to_owned()),
                ("TWO_AUTOMOD".to_owned(), "0".to_owned()),
            ]),
            false,
        )
        .expect("same production composition seam"),
    );
    assert!(runtime.dispatch(&interaction_event(invocation(
        8104,
        "command-enable",
        Permissions::MANAGE_GUILD
    ))));
    settle(&runtime).await;
    let mut prefix = message(8105, CHANNEL, false, Some(GUILD));
    prefix.content = "!faq ordinary arguments".to_owned();
    assert!(runtime.dispatch(&Event::MessageCreate(Box::new(MessageCreate(prefix)))));
    settle(&runtime).await;
    assert!(runtime.dispatch(&interaction_event(invocation(
        8106,
        "faq",
        Permissions::empty()
    ))));
    settle(&runtime).await;
    let resumed: Value = sqlx::query_scalar("SELECT to_jsonb(c) FROM automation_commands c")
        .fetch_one(db.pool())
        .await
        .expect("definition after resume");
    let resumed_requests = mock.requests();
    drop(guard);
    drop(runtime);
    mock.shutdown().await;
    db.close()
        .await
        .expect("explicit teardown before behavioral RED assertions");

    assert_eq!(
        paused["enabled"], false,
        "gateway must dispatch pause, not just compile an unused helper"
    );
    assert_eq!(resumed["enabled"], true);
    for field in [
        "guild_id",
        "name",
        "description",
        "template",
        "text_trigger",
        "created_by",
        "created_at",
    ] {
        assert_eq!(paused[field], before[field], "preserve {field} on pause");
        assert_eq!(resumed[field], before[field], "preserve {field} on resume");
    }
    assert!(private_completion(&paused_requests, 8101)
        .to_ascii_lowercase()
        .contains("disabled"));
    assert!(private_completion(&resumed_requests, 8104)
        .to_ascii_lowercase()
        .contains("enabled"));
    assert!(
        !paused_requests
            .iter()
            .any(|r| r.path.ends_with("/channels/3333/messages")),
        "paused prefix never sends"
    );
    assert!(
        !paused_requests
            .iter()
            .filter(|r| r.path.contains("gateway-toggle-8103"))
            .any(|r| String::from_utf8_lossy(&r.body).contains("Original")),
        "stale slash never renders the saved response"
    );
    let posts: Vec<_> = resumed_requests
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/channels/3333/messages"))
        .collect();
    assert_eq!(
        posts.len(),
        1,
        "resume restores prefix without redefining it"
    );
    let post: Value = serde_json::from_slice(&posts[0].body).expect("prefix body");
    assert_eq!(post["content"], "Original member in Test guild");
    assert_eq!(post["allowed_mentions"]["parse"], json!([]));
    let slash = resumed_requests
        .iter()
        .find(|r| r.method == "PATCH" && r.path.contains("gateway-toggle-8106"))
        .expect("resumed dynamic slash completion");
    let slash: Value = serde_json::from_slice(&slash.body).expect("slash body");
    assert_eq!(slash["content"], "Original member in Test guild");
    assert_eq!(slash["allowed_mentions"]["parse"], json!([]));
    for requests in [&paused_requests, &resumed_requests] {
        assert_eq!(
            requests.iter().filter(|r| r.method == "PUT").count(),
            1,
            "each transition publishes exactly one full set"
        );
    }
}
