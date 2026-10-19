//! TOG-10860: read-before-write guild registry synchronization over loopback REST.
//! All command changes below are synthetic fixtures, not builtin definition changes.

#[allow(dead_code)]
mod common;

use std::time::Duration;

use common::{MockRest, RestRequest, ScriptedResponse, APP_ID, GUILD_ID};
use serde_json::{json, Value};
use twilight_model::application::command::Command;
use two_bot_core::commands::{CommandChoice, CommandDefinition, CommandOption, CommandOptionType};
use two_bot_discord::{publish_commands, ActionExecutor, DiscordError};

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("registry-test-token".to_owned(), Some(mock.origin()))
        .expect("local REST executor")
}

fn desired() -> Vec<Command> {
    publish_commands(&[
        CommandDefinition::new("inspect", "Inspect a member.")
            .permissions(4)
            .options(vec![
                CommandOption::new("count", "Number to inspect.", CommandOptionType::Integer)
                    .required()
                    .int_range(1, 10),
                CommandOption::new("mode", "Inspection mode.", CommandOptionType::String).choices(
                    vec![
                        CommandChoice {
                            name: "Fast".into(),
                            value: "fast".into(),
                        },
                        CommandChoice {
                            name: "Full".into(),
                            value: "full".into(),
                        },
                    ],
                ),
            ]),
        CommandDefinition::new("added", "New command."),
        CommandDefinition::new("stable", "Unchanged command."),
    ])
}

/// Discord adds metadata/defaults and need not preserve command-list order.
fn fetched_registry(commands: &[Command]) -> Value {
    let mut wire = serde_json::to_value(commands).unwrap();
    let list = wire.as_array_mut().unwrap();
    for (index, command) in list.iter_mut().enumerate() {
        let object = command.as_object_mut().unwrap();
        object.insert("id".into(), json!((9000 + index).to_string()));
        object.insert("application_id".into(), json!(APP_ID.to_string()));
        object.insert("guild_id".into(), json!(GUILD_ID.to_string()));
        object.insert("version".into(), json!("9999"));
        object.insert("nsfw".into(), json!(false));
        object.insert("name_localizations".into(), json!({}));
        object.insert("description_localizations".into(), json!({}));
        // These are global-only fields and must not create guild drift.
        object.insert("dm_permission".into(), json!(true));
        object.insert("contexts".into(), json!([0, 1]));
        object.insert("integration_types".into(), json!([0]));
        object.entry("options").or_insert(json!([]));
        for option in command["options"].as_array_mut().unwrap() {
            let object = option.as_object_mut().unwrap();
            object.entry("required").or_insert(json!(false));
            object.insert("autocomplete".into(), json!(false));
            object.entry("choices").or_insert(json!([]));
            object.insert("name_localizations".into(), json!({}));
            object.insert("description_localizations".into(), json!({}));
        }
    }
    list.reverse();
    wire
}

fn drifted_registry(commands: &[Command]) -> Value {
    let mut wire = fetched_registry(commands);
    let list = wire.as_array_mut().unwrap();
    list.retain(|command| command["name"] != "added");
    let inspect = list
        .iter_mut()
        .find(|command| command["name"] == "inspect")
        .unwrap();
    inspect["description"] = json!("Old inspection description.");
    inspect["default_member_permissions"] = json!("8");
    inspect["options"][0]["description"] = json!("Old count description.");
    inspect["options"][0]["max_value"] = json!(5);
    inspect["options"][1]["choices"]
        .as_array_mut()
        .unwrap()
        .reverse();
    list.push(json!({
        "type": 1, "name": "retired", "description": "Removed command.",
        "version": "777", "default_member_permissions": null,
    }));
    wire
}

fn registry_with_inspect_permissions(commands: &[Command], permission: Value) -> Value {
    let mut wire = fetched_registry(commands);
    let inspect = wire
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|command| command["name"] == "inspect")
        .unwrap();
    inspect["default_member_permissions"] = permission;
    wire
}

fn registry_path() -> String {
    format!("/api/v10/applications/{APP_ID}/guilds/{GUILD_ID}/commands")
}

fn assert_get(request: &RestRequest) {
    assert_eq!(request.method, "GET");
    let (path, query) = request.path.split_once('?').expect("localization query");
    assert_eq!(path, registry_path());
    assert!(query
        .split('&')
        .any(|pair| pair == "with_localizations=true"));
    assert!(request.body.is_empty());
    assert_eq!(
        request.header("authorization"),
        Some("Bot registry-test-token")
    );
}

fn assert_full_put(request: &RestRequest, commands: &[Command]) {
    assert_eq!(request.method, "PUT");
    assert_eq!(request.path, registry_path());
    let body: Value = serde_json::from_slice(&request.body).expect("bulk PUT JSON");
    assert_eq!(
        body,
        serde_json::to_value(commands).unwrap(),
        "publish the full compiled set, not just additions or changed fields"
    );
    assert_eq!(body.as_array().unwrap().len(), 3);
    assert!(body
        .as_array()
        .unwrap()
        .iter()
        .any(|command| command["name"] == "stable"));
    assert!(!body
        .as_array()
        .unwrap()
        .iter()
        .any(|command| command["name"] == "retired"));
}

#[tokio::test]
async fn guild_commands_fetches_complete_registry_with_localizations() {
    let commands = desired();
    let wire = fetched_registry(&commands);
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, wire.clone())],
        ScriptedResponse::status(403),
    )
    .await;
    let fetched = executor(&mock)
        .guild_commands(APP_ID, GUILD_ID)
        .await
        .unwrap();
    let expected: Vec<Command> = serde_json::from_value(wire).unwrap();
    assert_eq!(fetched, expected);
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_get(&requests[0]);
    mock.shutdown().await;
}

#[tokio::test]
async fn dry_run_renders_add_remove_options_permissions_and_descriptions_without_put() {
    let commands = desired();
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, drifted_registry(&commands))],
        ScriptedResponse::status(403),
    )
    .await;
    let (diff, applied) = executor(&mock)
        .sync_guild_commands(APP_ID, GUILD_ID, &commands, false)
        .await
        .unwrap();
    assert!(!applied);
    assert_ne!(diff.current_hash, diff.compiled_hash);
    assert_eq!(diff.added.len(), 1);
    assert_eq!(diff.removed.len(), 1);
    assert_eq!(diff.changed.len(), 1);
    let rendered = diff.render();
    for expected in [
        "+ 1/added",
        "- 1/retired",
        "~ 1/inspect",
        "description: \"Old inspection description.\" -> \"Inspect a member.\"",
        "default_member_permissions: \"8\" -> \"4\"",
        "options[0].description: \"Old count description.\" -> \"Number to inspect.\"",
        "options[0].max_value: 5 -> 10",
        "options[1].choices[0].value: \"full\" -> \"fast\"",
        "options[1].choices[1].value: \"fast\" -> \"full\"",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected:?} in {rendered}"
        );
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "dry run never writes");
    assert_get(&requests[0]);
    mock.shutdown().await;
}

fn identity_interaction(
    id: u64,
    name: &str,
) -> twilight_model::application::interaction::Interaction {
    serde_json::from_value(json!({
        "id": "7000", "application_id": APP_ID.to_string(), "type": 2,
        "token": "registry-identity-fixture", "authorizing_integration_owners": {},
        "entitlements": [], "guild_id": GUILD_ID.to_string(),
        "data": {"id": id.to_string(), "name": name, "type": 1, "guild_id": GUILD_ID.to_string()}
    }))
    .unwrap()
}

#[tokio::test]
async fn identity_snapshot_uses_matching_get_and_then_the_new_put_receipt() {
    let commands = desired();
    let mut replaced = fetched_registry(&commands);
    for command in replaced.as_array_mut().unwrap() {
        let old: u64 = command["id"].as_str().unwrap().parse().unwrap();
        command["id"] = json!((old + 10000).to_string());
    }
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, fetched_registry(&commands)),
            ScriptedResponse::json(200, drifted_registry(&commands)),
            ScriptedResponse::json(200, replaced),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    let executor = executor(&mock);
    let identities = executor.command_identities();
    executor
        .sync_guild_commands(APP_ID, GUILD_ID, &commands, true)
        .await
        .unwrap();
    assert_eq!(
        identities
            .slash_name(&identity_interaction(9000, "wrong-name"))
            .as_deref(),
        Some("inspect")
    );
    assert_eq!(mock.requests().len(), 1, "hash match sends no PUT");
    executor
        .sync_guild_commands(APP_ID, GUILD_ID, &commands, true)
        .await
        .unwrap();
    assert_eq!(
        identities
            .slash_name(&identity_interaction(19000, "wrong-name"))
            .as_deref(),
        Some("inspect")
    );
    assert_eq!(
        identities.slash_name(&identity_interaction(9000, "inspect")),
        None
    );
    assert_eq!(mock.requests().len(), 3);
    mock.shutdown().await;
}

#[tokio::test]
async fn failed_publish_does_not_install_the_pre_write_get_or_bad_receipt() {
    for receipt in [
        ScriptedResponse::status(403),
        ScriptedResponse::json(200, json!([])),
    ] {
        let commands = desired();
        let mut drift = drifted_registry(&commands);
        let inspect = drift
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|command| command["name"] == "inspect")
            .unwrap();
        inspect["id"] = json!("19000");
        let mock = MockRest::start(
            vec![
                ScriptedResponse::json(200, fetched_registry(&commands)),
                ScriptedResponse::json(200, drift),
                receipt,
            ],
            ScriptedResponse::status(403),
        )
        .await;
        let executor = executor(&mock);
        executor
            .sync_guild_commands(APP_ID, GUILD_ID, &commands, true)
            .await
            .unwrap();
        assert!(executor
            .sync_guild_commands(APP_ID, GUILD_ID, &commands, true)
            .await
            .is_err());
        let identities = executor.command_identities();
        assert_eq!(
            identities
                .slash_name(&identity_interaction(9000, "wrong-name"))
                .as_deref(),
            Some("inspect")
        );
        assert_eq!(
            identities.slash_name(&identity_interaction(19000, "inspect")),
            None
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn direct_publication_records_ids_for_dynamic_republish_callers() {
    let commands = desired();
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, fetched_registry(&commands))],
        ScriptedResponse::status(403),
    )
    .await;
    let executor = executor(&mock);
    executor
        .publish_guild_commands(APP_ID, GUILD_ID, &commands)
        .await
        .unwrap();
    assert_eq!(
        executor
            .command_identities()
            .slash_name(&identity_interaction(9000, "wrong-name"))
            .as_deref(),
        Some("inspect")
    );
    assert_eq!(mock.requests().len(), 1);
    assert_full_put(&mock.requests()[0], &commands);
    mock.shutdown().await;
}

#[tokio::test]
async fn apply_bulk_overwrites_full_registry_then_fresh_executor_skips_matching_registry() {
    let commands = desired();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, drifted_registry(&commands)),
            ScriptedResponse::json(200, fetched_registry(&commands)),
            ScriptedResponse::json(200, fetched_registry(&commands)),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    let first = executor(&mock);
    let (diff, applied) = first
        .sync_guild_commands(APP_ID, GUILD_ID, &commands, true)
        .await
        .unwrap();
    assert!(applied);
    assert!(!diff.is_empty());
    drop(first);

    // No cached hash survives: a boot-like new executor must GET and normalize.
    let (diff, applied) = executor(&mock)
        .sync_guild_commands(APP_ID, GUILD_ID, &commands, true)
        .await
        .unwrap();
    assert!(!applied);
    assert!(diff.is_empty());
    assert_eq!(diff.current_hash, diff.compiled_hash);
    assert!(diff.render().contains("No command drift."));
    let requests = mock.requests();
    assert_eq!(requests.len(), 3, "GET, full PUT, fresh GET only");
    assert_get(&requests[0]);
    assert_full_put(&requests[1], &commands);
    assert_get(&requests[2]);
    mock.shutdown().await;
}

#[tokio::test]
async fn same_executor_refetches_and_repairs_out_of_band_drift() {
    let commands = desired();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, fetched_registry(&commands)),
            ScriptedResponse::json(200, drifted_registry(&commands)),
            ScriptedResponse::json(200, fetched_registry(&commands)),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    let executor = executor(&mock);
    let (diff, applied) = executor
        .sync_guild_commands(APP_ID, GUILD_ID, &commands, true)
        .await
        .unwrap();
    assert!(diff.is_empty());
    assert!(!applied);
    let (diff, applied) = executor
        .sync_guild_commands(APP_ID, GUILD_ID, &commands, true)
        .await
        .unwrap();
    assert!(!diff.is_empty());
    assert!(applied, "matching prior hash cannot hide an external edit");
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_get(&requests[0]);
    assert_get(&requests[1]);
    assert_full_put(&requests[2], &commands);
    mock.shutdown().await;
}

#[tokio::test]
async fn unknown_permission_bits_remain_visible_in_dry_run_and_trigger_full_apply() {
    let commands = desired();
    // This permission bit is outside Twilight's known flags. GET must retain
    // it rather than silently normalizing 4 | (1 << 48) down to 4.
    let raw_permissions = (4_u64 | (1_u64 << 48)).to_string();
    assert_eq!(raw_permissions, "281474976710660");
    for apply in [false, true] {
        let mock = MockRest::start(
            vec![
                ScriptedResponse::json(
                    200,
                    registry_with_inspect_permissions(&commands, json!(raw_permissions)),
                ),
                ScriptedResponse::json(200, fetched_registry(&commands)),
            ],
            ScriptedResponse::status(403),
        )
        .await;
        let (diff, applied) = executor(&mock)
            .sync_guild_commands(APP_ID, GUILD_ID, &commands, apply)
            .await
            .unwrap();
        assert_eq!(applied, apply);
        assert_ne!(diff.current_hash, diff.compiled_hash);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert_eq!(diff.changed.len(), 1);
        let fields = &diff.changed["1/inspect"];
        assert_eq!(fields.len(), 1, "only the unknown permission bit differs");
        assert_eq!(fields[0].path, "default_member_permissions");
        assert_eq!(fields[0].before, json!("281474976710660"));
        assert_eq!(fields[0].after, json!("4"));
        assert!(diff
            .render()
            .contains("default_member_permissions: \"281474976710660\" -> \"4\""));
        let requests = mock.requests();
        assert_eq!(requests.len(), if apply { 2 } else { 1 });
        assert_get(&requests[0]);
        if apply {
            assert_full_put(&requests[1], &commands);
        }
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn failed_or_malformed_get_never_falls_back_to_empty_registry_or_put() {
    let malformed = ScriptedResponse {
        body: b"not JSON".to_vec(),
        ..ScriptedResponse::status(200)
    };
    for (label, response, expected_requests) in [
        (
            "forbidden",
            ScriptedResponse::json(403, json!({"message": "Forbidden", "code": 50001})),
            1,
        ),
        ("upstream exhausted", ScriptedResponse::status(503), 5),
        ("invalid JSON", malformed, 1),
        (
            "wrong top-level shape",
            ScriptedResponse::json(200, json!({"commands": []})),
            1,
        ),
        (
            "invalid command",
            ScriptedResponse::json(200, json!([{"name": "broken"}])),
            1,
        ),
        (
            "malformed permission bitfield",
            ScriptedResponse::json(
                200,
                registry_with_inspect_permissions(&desired(), json!("not-a-bitfield")),
            ),
            1,
        ),
        (
            "overflow permission bitfield",
            ScriptedResponse::json(
                200,
                registry_with_inspect_permissions(&desired(), json!("18446744073709551616")),
            ),
            1,
        ),
        (
            "numeric permission bitfield",
            ScriptedResponse::json(200, registry_with_inspect_permissions(&desired(), json!(4))),
            1,
        ),
        (
            "boolean permission bitfield",
            ScriptedResponse::json(
                200,
                registry_with_inspect_permissions(&desired(), json!(true)),
            ),
            1,
        ),
        ("empty successful body", ScriptedResponse::status(204), 1),
    ] {
        let mock = MockRest::start(vec![], response).await;
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            executor(&mock).sync_guild_commands(APP_ID, GUILD_ID, &desired(), true),
        )
        .await
        .expect("bounded GET failure");
        assert!(
            result.is_err(),
            "{label} must fail rather than claim publication"
        );
        if label == "forbidden" {
            assert!(matches!(result, Err(DiscordError::Rejected(_))));
        }
        let requests = mock.requests();
        assert_eq!(requests.len(), expected_requests, "{label}");
        for request in &requests {
            assert_get(request);
        }
        assert!(
            requests.iter().all(|request| request.method != "PUT"),
            "{label}: uncertain reads are never overwritten"
        );
        mock.shutdown().await;
    }
}
