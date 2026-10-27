//! TOG-10860: the shipped binary's dry-run, live fence and opt-in boot path.
//! Environment inheritance is disabled; every REST request goes to loopback.
//! Boot uses an invalid synthetic DB URL which fails locally after command sync.

#[allow(dead_code)]
#[path = "../../discord/tests/common/mod.rs"]
mod common;

use std::{
    process::{Output, Stdio},
    time::Duration,
};

use common::{MockRest, RestRequest, ScriptedResponse, APP_ID, GUILD_ID};
use serde_json::{json, Value};
use tokio::process::Command;
use two_bot_core::commands::core_commands;
use two_bot_discord::publish_commands;

const TOKEN: &str = "commands-cli-synthetic-token";
const INVALID_DB: &str = "synthetic-database-must-not-connect";

fn bot(mock: &MockRest) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .env_clear()
        .env("DISCORD_API_BASE", mock.origin())
        .env("LISTEN_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Drop kills a timed-out child; wait_with_output reaps normal exits.
        .kill_on_drop(true);
    // Keep coverage instrumentation, not inherited application configuration.
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    command
}

fn credentials(command: &mut Command) {
    command
        .env("DISCORD_TOKEN", TOKEN)
        .env("GUILD_ID", GUILD_ID.to_string())
        .env("DISCORD_APPLICATION_ID", APP_ID.to_string());
}

async fn run(mut command: Command) -> Output {
    let child = command.spawn().expect("start two-bot test binary");
    tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .expect("two-bot must terminate; timeout kills the child")
        .expect("collect binary output")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("UTF-8 stderr")
}

fn compiled_registry() -> Value {
    serde_json::to_value(publish_commands(&core_commands())).unwrap()
}

/// Simulate server-assigned metadata, defaults and reversed command ordering.
fn matching_registry() -> Value {
    let mut wire = compiled_registry();
    let commands = wire.as_array_mut().unwrap();
    for (index, command) in commands.iter_mut().enumerate() {
        // The server echoes the writable permission gate; only everyone-commands
        // come back null. Forcing null on a gated command would fake drift.
        let gated = command
            .get("default_member_permissions")
            .is_some_and(|value| !value.is_null());
        let object = command.as_object_mut().unwrap();
        object.insert("id".into(), json!((8000 + index).to_string()));
        object.insert("application_id".into(), json!(APP_ID.to_string()));
        object.insert("guild_id".into(), json!(GUILD_ID.to_string()));
        object.insert("version".into(), json!("9876"));
        object.insert("nsfw".into(), json!(false));
        if !gated {
            object.insert("default_member_permissions".into(), Value::Null);
        }
        object.insert("name_localizations".into(), json!({}));
        object.insert("description_localizations".into(), json!({}));
        object.entry("options").or_insert(json!([]));
        for option in command["options"].as_array_mut().unwrap() {
            let object = option.as_object_mut().unwrap();
            // The server echoes back required flags and fixed choices: only
            // fill in Discord's defaults where the compiled option omits
            // them, so a required choice option (like `/reopen kind`)
            // still matches its own echo.
            object.entry("required").or_insert(json!(false));
            object.insert("autocomplete".into(), json!(false));
            object.entry("choices").or_insert(json!([]));
            object.insert("name_localizations".into(), json!({}));
            object.insert("description_localizations".into(), json!({}));
        }
    }
    commands.reverse();
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
        Some(format!("Bot {TOKEN}").as_str())
    );
}

#[tokio::test]
async fn diff_and_publish_default_to_get_only_without_opening_a_database() {
    for verb in ["diff", "publish"] {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(200, json!([]))],
            ScriptedResponse::status(403),
        )
        .await;
        let mut command = bot(&mock);
        credentials(&mut command);
        command
            .args(["commands", verb])
            .env("DATABASE_URL", INVALID_DB);
        let output = run(command).await;
        assert!(output.status.success(), "{verb}: {}", stderr(&output));
        let rendered = stdout(&output);
        for expected in [
            "current hash:",
            "compiled hash:",
            "+ 1/rank",
            "+ 1/leaderboard",
            "+ 1/help",
            "+ 1/reopen",
            "+ 1/welcome-preview",
            "+ 1/goodbye-preview",
            "Dry run: no commands written.",
        ] {
            assert!(
                rendered.contains(expected),
                "{verb}: missing {expected:?} in {rendered}"
            );
        }
        assert!(!rendered.contains("Published full guild registry."));
        let requests = mock.requests();
        assert_eq!(
            requests.len(),
            1,
            "{verb} cannot mutate or fall through to gateway boot"
        );
        assert_get(&requests[0]);
        assert!(!rendered.contains(TOKEN));
        assert!(!stderr(&output).contains(TOKEN));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn publish_apply_is_a_full_put_and_a_new_process_skips_normalized_match() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(
                200,
                json!([{
                    // Intentional feature-disable removal, not an unknown custom command.
                    "type": 1, "name": two_bot_core::feature_commands::automation_commands()[0].name,
                    "description": "Disabled builtin.",
                    "default_member_permissions": null, "version": "1",
                }]),
            ),
            ScriptedResponse::json(200, matching_registry()),
            ScriptedResponse::json(200, matching_registry()),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    let mut command = bot(&mock);
    credentials(&mut command);
    command
        .args(["commands", "publish", "--apply"])
        .env("DATABASE_URL", INVALID_DB);
    let output = run(command).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Published full guild registry."));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_get(&requests[0]);
    assert_eq!(requests[1].method, "PUT");
    assert_eq!(requests[1].path, registry_path());
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(
        body,
        compiled_registry(),
        "complete feature-gated builtin set, not a patch"
    );

    let mut command = bot(&mock);
    credentials(&mut command);
    command.args(["commands", "publish", "--apply"]);
    let output = run(command).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("No command drift."));
    assert!(stdout(&output).contains("Skipped PUT: registry hash matches."));
    assert!(!stdout(&output).contains("Published full guild registry."));
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        3,
        "new process GETs but does not re-publish a match"
    );
    assert_get(&requests[2]);
    mock.shutdown().await;
}

#[tokio::test]
async fn live_guild_and_leading_zero_spellings_are_fenced_before_credentials_or_rest() {
    let operations: &[&[&str]] = &[&["diff"], &["publish"], &["publish", "--apply"]];
    for guild in [
        two_bot_cutover::LIVE_GUILD_ID.to_owned(),
        format!("000{}", two_bot_cutover::LIVE_GUILD_ID),
    ] {
        for operation in operations {
            for include_credentials in [false, true] {
                for use_flag in [false, true] {
                    let mock =
                        MockRest::start(vec![], ScriptedResponse::json(200, json!([]))).await;
                    let mut command = bot(&mock);
                    if include_credentials {
                        credentials(&mut command);
                    }
                    command.arg("commands").args(*operation);
                    if use_flag {
                        command.args(["--guild-id", &guild]);
                    } else {
                        command.env("GUILD_ID", &guild);
                    }
                    let output = run(command).await;
                    assert_eq!(output.status.code(), Some(2));
                    let error = stderr(&output);
                    assert!(
                        error.contains("Refusing live guild"),
                        "must fence even without application/token: {error}"
                    );
                    assert!(!error.contains("must be a nonzero"));
                    assert!(!error.contains("cannot configure command REST client"));
                    assert!(
                        mock.requests().is_empty(),
                        "live IDs must be rejected before ANY REST, even dry-run GET"
                    );
                    mock.shutdown().await;
                }
            }
        }
    }
}

#[tokio::test]
async fn command_help_needs_no_credentials_and_sends_no_rest() {
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["commands", "--help"],
        vec!["commands", "diff", "--help"],
        vec!["commands", "publish", "-h"],
    ] {
        let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
        let mut command = bot(&mock);
        command.args(&args);
        let output = run(command).await;
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        let help = stdout(&output);
        for expected in [
            "two-bot commands diff",
            "two-bot commands publish [--apply]",
            "--allow-live-guild",
            "DISCORD_TOKEN",
            "TWO_DATABASE_URL",
            "no gateway is started",
        ] {
            assert!(
                help.contains(expected),
                "{args:?}: missing {expected:?} in {help}"
            );
        }
        assert!(mock.requests().is_empty());
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn failed_and_malformed_cli_get_do_not_publish_or_claim_success() {
    for response in [
        ScriptedResponse::json(403, json!({"message": "Forbidden", "code": 50001})),
        ScriptedResponse {
            body: b"broken JSON".to_vec(),
            ..ScriptedResponse::status(200)
        },
    ] {
        let mock = MockRest::start(vec![response], ScriptedResponse::status(403)).await;
        let mut command = bot(&mock);
        credentials(&mut command);
        command.args(["commands", "publish", "--apply"]);
        let output = run(command).await;
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains("command registry request failed"));
        assert!(!stdout(&output).contains("Published full guild registry."));
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_get(&requests[0]);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn real_opt_in_boot_fetches_matching_registry_without_put_before_local_db_failure() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, matching_registry())],
        ScriptedResponse::status(403),
    )
    .await;
    let mut command = bot(&mock);
    credentials(&mut command);
    command
        .env("TWO_COMMANDS_PUBLISH_ON_BOOT", "1")
        .env("DATABASE_URL", INVALID_DB)
        .env("RUST_LOG", "two_bot=info");
    // No subcommand: this runs main's actual gateway initialization branch.
    // connect() rejects the synthetic URL before a DB socket or migrations.
    let output = run(command).await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "boot stops safely at local DB validation"
    );
    assert!(
        stdout(&output).contains("boot command registry synchronized"),
        "must finish sync before the intentional DB failure: {} {}",
        stdout(&output),
        stderr(&output)
    );
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        1,
        "boot performs GET/hash comparison and skips PUT on normalized match"
    );
    assert_get(&requests[0]);
    mock.shutdown().await;
}

#[tokio::test]
async fn opt_in_boot_with_voice_on_keeps_the_voice_set_in_the_put() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, matching_registry()),
            ScriptedResponse::json(200, matching_registry()),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    let mut command = bot(&mock);
    credentials(&mut command);
    command
        .env("TWO_COMMANDS_PUBLISH_ON_BOOT", "1")
        .env("TWO_VOICE", "1")
        .env("DATABASE_URL", INVALID_DB)
        .env("RUST_LOG", "two_bot=info");
    // The scripted PUT answer is not a full receipt, so boot exits at the sync;
    // the request body is what this test pins.
    let output = run(command).await;
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "GET shows drift, so one PUT follows");
    assert_get(&requests[0]);
    assert_eq!(requests[1].method, "PUT");
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let names: Vec<_> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command["name"].as_str().unwrap().to_owned())
        .collect();
    // The early sync runs before the voice sink exists. Dropping the voice set
    // here would delete and recreate it on every healthy restart.
    for definition in two_bot_core::voice_rooms::voice_commands() {
        assert!(names.contains(&definition.name), "{}", definition.name);
    }
    assert!(!names.iter().any(|name| name == "templateassistant"));
    mock.shutdown().await;
}

#[tokio::test]
async fn json_and_human_unknown_removals_are_inspectable_but_never_applied() {
    for json in [false, true] {
        for apply in [false, true] {
            let mock = MockRest::start(
                vec![ScriptedResponse::json(
                    200,
                    json!([{
                        "id": "9001", "type": 1, "name": "zz-custom",
                        "description": "Fixture.", "version": "1"
                    }]),
                )],
                ScriptedResponse::status(403),
            )
            .await;
            let mut command = bot(&mock);
            credentials(&mut command);
            command.args(["commands", "publish"]);
            if json {
                command.arg("--json");
            }
            if apply {
                command.arg("--apply");
            }
            let output = run(command).await;
            assert_eq!(output.status.code(), Some(i32::from(apply)));
            if json {
                let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
                assert_eq!(report["ok"], !apply);
                assert_eq!(report["removals"][0]["id"], "9001");
                assert_eq!(report["removals"][0]["removal_class"], "unrecognized");
                if apply {
                    assert_eq!(report["error"]["code"], "unrecognized_command_removal");
                }
            } else {
                assert!(stdout(&output).contains("unrecognized"));
                assert!(stdout(&output).contains("1/zz-custom"));
            }
            assert_eq!(mock.requests().len(), 1);
            assert_get(&mock.requests()[0]);
            mock.shutdown().await;
        }
    }
}

#[tokio::test]
async fn json_success_reports_dry_run_apply_and_no_drift_without_extra_reads() {
    for (operation, current, expected_methods, applied) in [
        (vec!["diff", "--json"], json!([]), vec!["GET"], false),
        (
            vec!["publish", "--apply", "--json"],
            json!([]),
            vec!["GET", "PUT"],
            true,
        ),
        (
            vec!["publish", "--apply", "--json"],
            matching_registry(),
            vec!["GET"],
            false,
        ),
    ] {
        let mock = MockRest::start(
            vec![
                ScriptedResponse::json(200, current),
                ScriptedResponse::json(200, matching_registry()),
            ],
            ScriptedResponse::status(403),
        )
        .await;
        let mut command = bot(&mock);
        credentials(&mut command);
        command.arg("commands").args(operation);
        let output = run(command).await;
        assert!(output.status.success(), "{}", stderr(&output));
        let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["ok"], true);
        assert_eq!(report["applied"], applied);
        assert_eq!(report["error"], Value::Null);
        assert_eq!(report["has_unrecognized_removals"], false);
        assert!(stderr(&output).is_empty());
        assert_eq!(
            mock.requests()
                .iter()
                .map(|r| r.method.as_str())
                .collect::<Vec<_>>(),
            expected_methods
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn strict_json_flags_and_configuration_errors_never_reach_rest() {
    for args in [
        vec!["diff", "--apply", "--json"],
        vec!["publish", "--json", "--json"],
        vec!["publish", "--apply", "--apply", "--json"],
        vec!["publish", "--json", "true"],
        vec!["publish", "--json", "--json=true"],
        vec!["publish", "--json", "--guild-id"],
        vec!["publish", "--json", "--help"],
        vec!["publish", "--json", "synthetic-argument-secret"],
    ] {
        let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
        let mut command = bot(&mock);
        credentials(&mut command);
        command.arg("commands").args(&args);
        let output = run(command).await;
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
        assert_eq!(report["ok"], false);
        assert_eq!(report["current_hash"], Value::Null);
        assert_eq!(report["removed_count"], Value::Null);
        assert_eq!(report["error"]["code"], "configuration_error");
        assert!(!report["error"]["next_action"].as_str().unwrap().is_empty());
        assert!(!stdout(&output).contains("synthetic-argument-secret"));
        assert!(stderr(&output).is_empty());
        assert!(mock.requests().is_empty());
        mock.shutdown().await;
    }
    // The live fence still takes precedence over missing token/application.
    let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
    let mut command = bot(&mock);
    command
        .env("GUILD_ID", two_bot_cutover::LIVE_GUILD_ID)
        .args(["commands", "publish", "--apply", "--json"]);
    let output = run(command).await;
    assert_eq!(output.status.code(), Some(2));
    let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["error"]["code"], "configuration_error");
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn ambiguous_registry_identities_fail_before_put_and_failed_put_is_not_success() {
    let mut empty_name = matching_registry();
    empty_name[0]["name"] = json!("");
    let mut duplicate = matching_registry();
    // Writable drift forces PUT if canonicalization stops rejecting duplicates;
    // a matching snapshot would fail later cache validation and mask that bug.
    duplicate[0]["description"] = json!("Out-of-band duplicate description.");
    let repeated = duplicate[0].clone();
    duplicate.as_array_mut().unwrap().push(repeated);
    for (current, expected_methods) in [
        (empty_name, vec!["GET"]),
        (duplicate, vec!["GET"]),
        (json!([]), vec!["GET", "PUT"]),
    ] {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(200, current)],
            ScriptedResponse::status(403),
        )
        .await;
        let mut command = bot(&mock);
        credentials(&mut command);
        command.args(["commands", "publish", "--apply", "--json"]);
        let output = run(command).await;
        assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
        let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
        assert_eq!(report["ok"], false);
        assert_eq!(report["applied"], false);
        assert_eq!(report["error"]["code"], "registry_request_failed");
        assert_eq!(
            mock.requests()
                .iter()
                .map(|r| r.method.as_str())
                .collect::<Vec<_>>(),
            expected_methods
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn boot_without_opt_in_sends_no_registry_requests() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
    let mut command = bot(&mock);
    credentials(&mut command);
    command.env("DATABASE_URL", INVALID_DB);
    let output = run(command).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        mock.requests().is_empty(),
        "default boot must not publish or even fetch commands"
    );
    mock.shutdown().await;
}
