//! Acceptance: a mutating CLI registry replacement must refuse to silently
//! remove remote commands that are in neither the complete known builtin
//! namespace nor the desired set. Loopback fixtures only; no real Discord,
//! database, or gateway. Environment inheritance is disabled, mirroring the
//! existing `commands_cli` binary tests.
//!
//! Contract summary (the implementation must satisfy this file unchanged):
//! - `commands publish --apply` refuses (exit 1, exactly one GET, zero PUT)
//!   when the registry snapshot it just read contains an unrecognized
//!   `type/name` identity absent from the desired set.
//! - `--json` (the opt-in agent mode used by sibling CLIs) prints one JSON
//!   document on stdout: `ok`, `current_hash`, `desired_hash` (64-hex),
//!   `removals` entries `{id, type, name, removal_class}` with
//!   `removal_class` of `known_builtin` or `unrecognized`, and on refusal an
//!   `error` object `{code: "unrecognized_command_removal", next_action}`.
//! - `diff` and publish-without-`--apply` stay read-only (exit 0 on drift)
//!   and name unrecognized removals as unrecognized in both output modes.
//! - Known-but-currently-desired-out builtins (feature-gated off, including
//!   the voice set) are still removable by an ordinary `--apply`.
//! - Failed/malformed reads never become an empty registry or a PUT, and
//!   error output never echoes the token or a remote error body.
//! - There is no allow-delete / force bypass flag.

use std::{
    collections::{BTreeSet, VecDeque},
    process::{Output, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    process::Command,
};
use two_bot_core::commands::core_commands;
use two_bot_core::feature_commands::{
    announcement_commands, automation_commands, scorecard_attendance_command,
};
use two_bot_core::moderation::moderation_commands;
use two_bot_core::voice_rooms::voice_commands;
use two_bot_discord::publish_commands;

const APP_ID: u64 = 1111;
const GUILD_ID: u64 = 2222;
const TOKEN: &str = "removal-guard-synthetic-token";
const UNRECOGNIZED: &str = "zz-unrecognized-custom";
const REMOTE_ERROR_SENTINEL: &str = "REMOTE-ERROR-BODY-SENTINEL-must-not-leak";

/// One scripted loopback response, served in order; requests past the end
/// of the script get the mock's default response.
struct ScriptedResponse {
    status: u16,
    body: Vec<u8>,
}

impl ScriptedResponse {
    fn status(status: u16) -> Self {
        Self {
            status,
            body: Vec::new(),
        }
    }

    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            body: serde_json::to_vec(&body).expect("serialize fixture"),
        }
    }
}

#[derive(Debug, Clone)]
struct RestRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// Minimal loopback REST double: records every request and answers from the
/// script. Self-contained so this file needs no shared-helper module.
struct MockRest {
    origin: String,
    recorded: Arc<Mutex<Vec<RestRequest>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockRest {
    async fn start(script: Vec<ScriptedResponse>, default: ScriptedResponse) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let origin = format!("http://{}", listener.local_addr().expect("mock addr"));
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(script)));
        let default = Arc::new(default);
        let handle = {
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let recorded = Arc::clone(&recorded);
                    let queue = Arc::clone(&queue);
                    let default = Arc::clone(&default);
                    tokio::spawn(async move {
                        let mut buf: Vec<u8> = Vec::new();
                        let mut chunk = [0u8; 8192];
                        // Read until the header block is complete.
                        let header_end = loop {
                            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break pos + 4;
                            }
                            match socket.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        };
                        let headers = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                        let mut lines = headers.split("\r\n");
                        let request_line = lines.next().unwrap_or_default();
                        let mut parts = request_line.split_whitespace();
                        let method = parts.next().unwrap_or_default().to_owned();
                        let path = parts.next().unwrap_or_default().to_owned();
                        let content_length = lines
                            .filter_map(|line| line.split_once(':'))
                            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        while buf.len() < header_end + content_length {
                            match socket.read(&mut chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                        let body = buf[header_end..].to_vec();
                        recorded
                            .lock()
                            .expect("recorded")
                            .push(RestRequest { method, path, body });
                        let (status, response_body) = {
                            let next = queue.lock().expect("queue").pop_front();
                            match next {
                                Some(scripted) => (scripted.status, scripted.body),
                                None => (default.status, default.body.clone()),
                            }
                        };
                        let reason = match status {
                            200 => "OK",
                            403 => "Forbidden",
                            _ => "Status",
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            response_body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.write_all(&response_body).await;
                        let _ = socket.shutdown().await;
                    });
                }
            })
        };
        Self {
            origin,
            recorded,
            handle,
        }
    }

    fn origin(&self) -> String {
        self.origin.clone()
    }

    fn requests(&self) -> Vec<RestRequest> {
        self.recorded.lock().expect("recorded").clone()
    }

    async fn shutdown(self) {
        self.handle.abort();
    }
}

fn bot(mock: &MockRest) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .env_clear()
        .env("DISCORD_API_BASE", mock.origin())
        .env("LISTEN_ADDR", "127.0.0.1:0")
        .env("DISCORD_TOKEN", TOKEN)
        .env("GUILD_ID", GUILD_ID.to_string())
        .env("DISCORD_APPLICATION_ID", APP_ID.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
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

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        stdout(output),
        String::from_utf8(output.stderr.clone()).expect("UTF-8 stderr")
    )
}

fn stdout_json(output: &Output) -> Value {
    let text = stdout(output);
    serde_json::from_str(text.trim())
        .unwrap_or_else(|error| panic!("--json stdout must be one JSON document ({error}): {text}"))
}

fn compiled_registry() -> Value {
    serde_json::to_value(publish_commands(&core_commands())).unwrap()
}

/// The complete known builtin slash namespace across every compiled catalog,
/// independent of feature gates. Derived from the same authoritative
/// definition sources the router publishes from, never a hand-written list.
fn complete_builtin_names() -> BTreeSet<String> {
    let scorecard = [scorecard_attendance_command()];
    core_commands()
        .iter()
        .chain(scorecard.iter())
        .chain(automation_commands().iter())
        .chain(announcement_commands().iter())
        .chain(moderation_commands().iter())
        .chain(voice_commands().iter())
        .map(|def| def.name.clone())
        .collect()
}

fn assert_unrecognized_fixture_is_really_unknown() {
    assert!(
        !complete_builtin_names().contains(UNRECOGNIZED),
        "fixture name must stay outside every builtin catalog"
    );
}

/// A known builtin that the default (all feature gates off) desired set does
/// not contain: removing it is the intentional-disable case.
fn known_disabled_name() -> String {
    let core: BTreeSet<String> = core_commands().iter().map(|d| d.name.clone()).collect();
    automation_commands()
        .iter()
        .map(|d| d.name.clone())
        .find(|name| !core.contains(name))
        .expect("an automation builtin outside the core set")
}

/// A voice-set builtin that appears in no other catalog, so only treating the
/// voice catalog as known can let its removal through with voice off.
fn known_voice_only_name() -> String {
    let scorecard = [scorecard_attendance_command()];
    let non_voice: BTreeSet<String> = core_commands()
        .iter()
        .chain(scorecard.iter())
        .chain(automation_commands().iter())
        .chain(announcement_commands().iter())
        .chain(moderation_commands().iter())
        .map(|def| def.name.clone())
        .collect();
    voice_commands()
        .iter()
        .map(|d| d.name.clone())
        .find(|name| !non_voice.contains(name))
        .expect("a voice-only builtin")
}

fn remote_command(id: u32, kind: u64, name: &str) -> Value {
    json!({
        "id": (8000 + id).to_string(),
        "type": kind,
        "name": name,
        "description": if kind == 1 { "Out-of-band command." } else { "" },
        "default_member_permissions": null,
        "version": "1",
    })
}

/// Simulate server-assigned metadata, defaults and reversed command ordering
/// (same normalization the baseline `commands_cli` tests rely on).
fn matching_registry() -> Value {
    let mut wire = compiled_registry();
    let commands = wire.as_array_mut().unwrap();
    for (index, command) in commands.iter_mut().enumerate() {
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

fn assert_all_gets(requests: &[RestRequest], expected: usize) {
    assert_eq!(requests.len(), expected, "request count: {requests:?}");
    for request in requests {
        assert_eq!(
            request.method, "GET",
            "no PUT may be sent, saw {} {}",
            request.method, request.path
        );
    }
}

fn assert_hash(value: &Value, field: &str) -> String {
    let hash = value[field]
        .as_str()
        .unwrap_or_else(|| panic!("{field} must be a string in {value}"));
    assert_eq!(hash.len(), 64, "{field} must be a SHA-256 hex digest");
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "{field} must be hex: {hash}"
    );
    hash.to_owned()
}

fn removal_entry<'a>(value: &'a Value, name: &str) -> &'a Value {
    value["removals"]
        .as_array()
        .unwrap_or_else(|| panic!("removals must be an array in {value}"))
        .iter()
        .find(|entry| entry["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("removals must name {name:?} in {value}"))
}

#[tokio::test]
async fn apply_refuses_unrecognized_removal_with_zero_put() {
    assert_unrecognized_fixture_is_really_unknown();
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            json!([remote_command(1, 1, UNRECOGNIZED)]),
        )],
        ScriptedResponse::status(403),
    )
    .await;
    let mut command = bot(&mock);
    command.args(["commands", "publish", "--apply"]);
    let output = run(command).await;
    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    let rendered = combined(&output);
    assert!(
        rendered.to_lowercase().contains("unrecognized"),
        "the refusal must name the cause: {rendered}"
    );
    assert!(
        rendered.contains(&format!("1/{UNRECOGNIZED}")),
        "the refusal must name the type/name identity: {rendered}"
    );
    assert!(!rendered.contains("Published full guild registry."));
    assert!(!rendered.contains(TOKEN));
    // One read of the snapshot the diff was computed from; no second,
    // disconnected preflight read and no PUT.
    assert_all_gets(&mock.requests(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn apply_refuses_unrecognized_removal_with_json_contract() {
    assert_unrecognized_fixture_is_really_unknown();
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            json!([remote_command(7, 1, UNRECOGNIZED)]),
        )],
        ScriptedResponse::status(403),
    )
    .await;
    let mut command = bot(&mock);
    command.args(["commands", "publish", "--apply", "--json"]);
    let output = run(command).await;
    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    let value = stdout_json(&output);
    assert_eq!(value["ok"], json!(false), "{value}");
    let current = assert_hash(&value, "current_hash");
    let desired = assert_hash(&value, "desired_hash");
    assert_ne!(current, desired, "removal drift must show in the hashes");
    let entry = removal_entry(&value, UNRECOGNIZED);
    assert_eq!(entry["id"], json!("8007"), "stable remote ID: {entry}");
    assert_eq!(entry["type"], json!(1), "command type: {entry}");
    assert_eq!(
        entry["removal_class"],
        json!("unrecognized"),
        "removal class: {entry}"
    );
    assert_eq!(
        value["error"]["code"],
        json!("unrecognized_command_removal"),
        "stable error code: {value}"
    );
    let next_action = value["error"]["next_action"]
        .as_str()
        .unwrap_or_else(|| panic!("error.next_action must be a string in {value}"));
    assert!(
        !next_action.trim().is_empty() && next_action.len() <= 300,
        "next_action must be present and bounded: {next_action:?}"
    );
    assert!(!combined(&output).contains(TOKEN));
    assert_all_gets(&mock.requests(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn repeated_apply_refusals_are_idempotent_with_zero_put() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!([remote_command(2, 1, UNRECOGNIZED)])),
            ScriptedResponse::json(200, json!([remote_command(2, 1, UNRECOGNIZED)])),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    for _ in 0..2 {
        let mut command = bot(&mock);
        command.args(["commands", "publish", "--apply"]);
        let output = run(command).await;
        assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
        assert!(!stdout(&output).contains("Published full guild registry."));
    }
    assert_all_gets(&mock.requests(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn same_name_with_different_command_type_is_unrecognized() {
    // A context-menu command (type 2) sharing a core slash name is a different
    // stable identity; no builtin publishes type 2, so removal must refuse.
    let core_name = core_commands()[0].name.clone();
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            json!([remote_command(3, 2, &core_name)]),
        )],
        ScriptedResponse::status(403),
    )
    .await;
    let mut command = bot(&mock);
    command.args(["commands", "publish", "--apply", "--json"]);
    let output = run(command).await;
    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    let value = stdout_json(&output);
    let entry = removal_entry(&value, &core_name);
    assert_eq!(entry["type"], json!(2), "{entry}");
    assert_eq!(entry["removal_class"], json!("unrecognized"), "{entry}");
    assert_eq!(
        value["error"]["code"],
        json!("unrecognized_command_removal"),
        "{value}"
    );
    assert_all_gets(&mock.requests(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn mixed_known_and_unrecognized_refuses_and_classifies_both() {
    let known = known_disabled_name();
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            json!([
                remote_command(4, 1, &known),
                remote_command(5, 1, UNRECOGNIZED),
            ]),
        )],
        ScriptedResponse::status(403),
    )
    .await;
    let mut command = bot(&mock);
    command.args(["commands", "publish", "--apply", "--json"]);
    let output = run(command).await;
    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    let value = stdout_json(&output);
    assert_eq!(
        removal_entry(&value, &known)["removal_class"],
        json!("known_builtin"),
        "the disabled builtin stays classified as known: {value}"
    );
    assert_eq!(
        removal_entry(&value, UNRECOGNIZED)["removal_class"],
        json!("unrecognized"),
        "{value}"
    );
    // One unrecognized removal refuses the whole replacement, including the
    // otherwise-allowed known removal riding in the same diff.
    assert_all_gets(&mock.requests(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn diff_and_dry_run_report_unrecognized_without_writing() {
    for args in [
        vec!["commands", "diff"],
        vec!["commands", "publish"],
        vec!["commands", "diff", "--json"],
        vec!["commands", "publish", "--json"],
    ] {
        let json_mode = args.contains(&"--json");
        let mock = MockRest::start(
            vec![ScriptedResponse::json(
                200,
                json!([remote_command(6, 1, UNRECOGNIZED)]),
            )],
            ScriptedResponse::status(403),
        )
        .await;
        let mut command = bot(&mock);
        command.args(&args);
        let output = run(command).await;
        // Read-only inspection keeps the existing drift-is-success exit code.
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: {}",
            combined(&output)
        );
        if json_mode {
            let value = stdout_json(&output);
            assert_eq!(value["ok"], json!(true), "{args:?}: {value}");
            assert_hash(&value, "current_hash");
            assert_hash(&value, "desired_hash");
            assert_eq!(
                removal_entry(&value, UNRECOGNIZED)["removal_class"],
                json!("unrecognized"),
                "{args:?}: {value}"
            );
        } else {
            let rendered = stdout(&output);
            assert!(
                rendered.contains(&format!("- 1/{UNRECOGNIZED}")),
                "{args:?}: the removal stays visible: {rendered}"
            );
            assert!(
                rendered.to_lowercase().contains("unrecognized"),
                "{args:?}: the removal must be marked unrecognized: {rendered}"
            );
        }
        assert!(!stdout(&output).contains("Published full guild registry."));
        assert_all_gets(&mock.requests(), 1);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn failed_and_malformed_get_never_put_and_stay_redacted_in_json() {
    for response in [
        ScriptedResponse::json(
            403,
            json!({"message": REMOTE_ERROR_SENTINEL, "code": 50001}),
        ),
        ScriptedResponse {
            body: format!("broken JSON {REMOTE_ERROR_SENTINEL}").into_bytes(),
            ..ScriptedResponse::status(200)
        },
    ] {
        let mock = MockRest::start(vec![response], ScriptedResponse::status(403)).await;
        let mut command = bot(&mock);
        command.args(["commands", "publish", "--apply", "--json"]);
        let output = run(command).await;
        assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
        let rendered = combined(&output);
        assert!(
            !rendered.contains(REMOTE_ERROR_SENTINEL),
            "leaked: {rendered}"
        );
        assert!(!rendered.contains(TOKEN), "leaked: {rendered}");
        assert!(!rendered.contains("Published full guild registry."));
        let value = stdout_json(&output);
        assert_eq!(value["ok"], json!(false), "{value}");
        let code = value["error"]["code"]
            .as_str()
            .unwrap_or_else(|| panic!("error.code must be a string in {value}"));
        assert!(!code.is_empty(), "a stable error code is required: {value}");
        assert_ne!(
            code, "unrecognized_command_removal",
            "an unreadable registry is not a classified removal: {value}"
        );
        let next_action = value["error"]["next_action"]
            .as_str()
            .unwrap_or_else(|| panic!("error.next_action must be a string in {value}"));
        assert!(!next_action.trim().is_empty(), "{value}");
        assert_all_gets(&mock.requests(), 1);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn hostile_remote_name_is_bounded_in_json_output() {
    // Far beyond Discord's own name limit, with characters that would break
    // naive line-oriented or unescaped rendering.
    let hostile = format!("zz-hostile\"\n\\{}", "x".repeat(4000));
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            json!([remote_command(8, 1, &hostile)]),
        )],
        ScriptedResponse::status(403),
    )
    .await;
    let mut command = bot(&mock);
    command.args(["commands", "publish", "--apply", "--json"]);
    let output = run(command).await;
    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    let text = stdout(&output);
    assert!(
        text.len() <= 8192,
        "JSON output must stay bounded, got {} bytes",
        text.len()
    );
    let value = stdout_json(&output);
    assert_eq!(
        value["error"]["code"],
        json!("unrecognized_command_removal"),
        "{value}"
    );
    let names: Vec<&str> = value["removals"]
        .as_array()
        .expect("removals array")
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    assert_eq!(names.len(), 1, "{value}");
    assert!(
        names[0].chars().count() <= 256,
        "a hostile name must be truncated before reporting, got {} chars",
        names[0].chars().count()
    );
    assert_all_gets(&mock.requests(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn unknown_remote_fields_do_not_disable_classification() {
    let mut unknown = remote_command(9, 1, UNRECOGNIZED);
    unknown
        .as_object_mut()
        .unwrap()
        .insert("future_field".into(), json!({"nested": [1, 2, 3]}));
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!([unknown]))],
        ScriptedResponse::status(403),
    )
    .await;
    let mut command = bot(&mock);
    command.args(["commands", "publish", "--apply", "--json"]);
    let output = run(command).await;
    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    let value = stdout_json(&output);
    assert_eq!(
        value["error"]["code"],
        json!("unrecognized_command_removal"),
        "forward-compatible fields must not turn classification off: {value}"
    );
    assert_all_gets(&mock.requests(), 1);
    mock.shutdown().await;
}

// ---- Controls: behavior the baseline already has and the guard must keep. ----

#[tokio::test]
async fn control_known_disabled_builtin_removal_still_publishes() {
    let known = known_disabled_name();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!([remote_command(10, 1, &known)])),
            ScriptedResponse::json(200, matching_registry()),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    let mut command = bot(&mock);
    command.args(["commands", "publish", "--apply"]);
    let output = run(command).await;
    assert!(output.status.success(), "{}", combined(&output));
    assert!(stdout(&output).contains("Published full guild registry."));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].method, "PUT");
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(body, compiled_registry());
    mock.shutdown().await;
}

#[tokio::test]
async fn control_known_disabled_voice_removal_still_publishes() {
    let voice_only = known_voice_only_name();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!([remote_command(11, 1, &voice_only)])),
            ScriptedResponse::json(200, matching_registry()),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    // TWO_VOICE is unset: the voice name is known but not desired, exactly the
    // "disable the feature, publish the smaller set" operator flow.
    let mut command = bot(&mock);
    command.args(["commands", "publish", "--apply"]);
    let output = run(command).await;
    assert!(output.status.success(), "{}", combined(&output));
    assert!(stdout(&output).contains("Published full guild registry."));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(requests[1].method, "PUT");
    mock.shutdown().await;
}

#[tokio::test]
async fn control_no_drift_apply_sends_no_put() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, matching_registry())],
        ScriptedResponse::status(403),
    )
    .await;
    let mut command = bot(&mock);
    command.args(["commands", "publish", "--apply"]);
    let output = run(command).await;
    assert!(output.status.success(), "{}", combined(&output));
    assert!(stdout(&output).contains("Skipped PUT: registry hash matches."));
    assert_all_gets(&mock.requests(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn control_no_allow_delete_bypass_flag_exists() {
    for flag in ["--allow-delete", "--allow-unrecognized-removal", "--force"] {
        let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
        let mut command = bot(&mock);
        command.args(["commands", "publish", "--apply", flag]);
        let output = run(command).await;
        assert_eq!(
            output.status.code(),
            Some(2),
            "{flag} must stay an unknown option: {}",
            combined(&output)
        );
        assert!(
            mock.requests().is_empty(),
            "{flag}: a rejected bypass flag must precede any REST"
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn control_live_guild_json_refusal_precedes_rest() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!([]))).await;
    let mut command = bot(&mock);
    command
        .env("GUILD_ID", two_bot_cutover::LIVE_GUILD_ID)
        .args(["commands", "publish", "--apply", "--json"]);
    let output = run(command).await;
    assert!(
        !output.status.success(),
        "the live guild must be refused: {}",
        combined(&output)
    );
    assert!(
        mock.requests().is_empty(),
        "the live-guild fence must precede token/REST use even in JSON mode"
    );
    mock.shutdown().await;
}
