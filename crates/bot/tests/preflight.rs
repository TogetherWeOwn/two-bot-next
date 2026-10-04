//! Real binary acceptance against the existing scripted Discord REST double.

#[allow(dead_code)]
#[path = "../../discord/tests/common/mod.rs"]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use std::time::Duration;
use twilight_model::guild::Permissions;

// The fixture is the staging pair so boot activation permits every capability
// and the portal-parity checks below exercise env-driven intents. The token's
// first segment is the base64 of the staging application id, never a secret;
// the intent-refusal regression overrides it with an unparseable token.
const TOKEN: &str = "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.preflight-fixture-not-a-credential";
const GUILD: u64 = 1545644954272137297;
const BOT: u64 = 1469137636663758888;
const BOT_ROLE: u64 = 4444;
const TARGET_ROLE: u64 = 5555;
const CHANNEL: u64 = 6666;

fn user() -> Value {
    json!({"id": BOT.to_string(), "username": TOKEN, "discriminator": "0001", "bot": true, "mfa_enabled": false})
}

fn role(id: u64, permissions: Permissions, position: u64, managed: bool) -> Value {
    json!({
        "id": id.to_string(), "name": TOKEN, "color": 0, "colors": {"primary_color": 0},
        "hoist": false, "position": position, "managed": managed, "mentionable": false,
        "permissions": permissions.bits().to_string(), "flags": 0
    })
}

fn permissions() -> Permissions {
    Permissions::MANAGE_GUILD
        | Permissions::VIEW_CHANNEL
        | Permissions::CREATE_INVITE
        | Permissions::MANAGE_ROLES
        | Permissions::MANAGE_EVENTS
        | Permissions::SEND_MESSAGES
        | Permissions::EMBED_LINKS
        | Permissions::MANAGE_MESSAGES
}

fn script(
    base: Permissions,
    flags: u64,
    target_position: u64,
    managed: bool,
    channel: Value,
) -> Vec<ScriptedResponse> {
    script_channels(base, flags, target_position, managed, vec![channel])
}

fn script_channels(
    base: Permissions,
    flags: u64,
    target_position: u64,
    managed: bool,
    channels: Vec<Value>,
) -> Vec<ScriptedResponse> {
    let mut bodies = vec![
        user(),
        json!({"id": BOT.to_string(), "name": TOKEN, "description": "", "bot_public": true,
            "bot_require_code_grant": false, "verify_key": "fixture", "flags": flags}),
        json!({"user": user(), "roles": [BOT_ROLE.to_string()], "deaf": false, "mute": false, "flags": 0}),
        json!([
            role(GUILD, Permissions::empty(), 0, false),
            role(BOT_ROLE, base, 10, true),
            role(TARGET_ROLE, Permissions::empty(), target_position, managed)
        ]),
        json!([]),
    ];
    bodies.extend(channels);
    bodies
        .into_iter()
        .map(|body| ScriptedResponse::json(200, body))
        .collect()
}

fn channel() -> Value {
    json!({"id": CHANNEL.to_string(), "guild_id": GUILD.to_string(), "name": TOKEN, "type": 0, "permission_overwrites": []})
}

const CATEGORY: u64 = 7777;

/// Ticket category destination: a GuildCategory (type 4) in the target guild.
fn category_channel() -> Value {
    json!({"id": CATEGORY.to_string(), "guild_id": GUILD.to_string(), "name": TOKEN, "type": 4, "permission_overwrites": []})
}

/// The same category slot served as a text channel: must fail as a mismatch.
fn category_channel_as_text() -> Value {
    json!({"id": CATEGORY.to_string(), "guild_id": GUILD.to_string(), "name": TOKEN, "type": 0, "permission_overwrites": []})
}

async fn cli(mock: &MockRest, args: &[&str], vars: &[(&str, &str)]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .env_clear()
        .arg("preflight")
        .args(args)
        .env("DISCORD_TOKEN", TOKEN)
        .env("GUILD_ID", GUILD.to_string())
        .env("TWO_ONBOARDING_MODE", "session")
        .env("DISCORD_STAFF_ALERT_CHANNEL_ID", CHANNEL.to_string())
        .env("DISCORD_PREFLIGHT_API_BASE", mock.origin())
        // A usable service URL must never be opened by this REST-only command.
        .env("DATABASE_URL", "postgresql://must-not-be-used.invalid/bot")
        .kill_on_drop(true);
    for (key, value) in vars {
        // Test-only sentinel: prove absent-primary alias selection without
        // weakening the helper's default of a present primary.
        if *key == "__REMOVE_DISCORD_TOKEN__" {
            command.env_remove("DISCORD_TOKEN");
        } else {
            command.env(key, value);
        }
    }
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("preflight bounded")
        .unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains(TOKEN));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(TOKEN));
    assert!(mock
        .requests()
        .iter()
        .all(|request| request.method == "GET" && request.body.is_empty()));
    output
}

async fn mock(script: Vec<ScriptedResponse>) -> MockRest {
    MockRest::start(
        script,
        ScriptedResponse::json(404, json!({"message": "unexpected request", "code": 10003})),
    )
    .await
}

#[tokio::test]
async fn pass_warn_fail_exit_codes_and_json_match_the_real_cli() {
    let deny = json!([{"id": GUILD.to_string(), "type": 0, "allow": "0", "deny": Permissions::VIEW_CHANNEL.bits().to_string()}]);
    for (name, base, overwrites, expected_code, expected_warning) in [
        ("pass", permissions(), json!([]), 0, false),
        ("warn", Permissions::ADMINISTRATOR, deny.clone(), 0, true),
        ("fail", permissions(), deny, 1, false),
    ] {
        let mut body = channel();
        body["permission_overwrites"] = overwrites;
        let mock = mock(script(base, 1 << 15, 1, false, body)).await;
        let output = cli(&mock, &["--json"], &[]).await;
        assert_eq!(
            output.status.code(),
            Some(expected_code),
            "{name}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["ready"], expected_code == 0);
        assert_eq!(report["exit_code"], expected_code);
        assert_eq!(report["warnings"].as_u64().unwrap() > 0, expected_warning);
        assert_eq!(
            mock.requests()
                .iter()
                .map(|request| request.path.clone())
                .collect::<Vec<_>>(),
            vec![
                "/api/v10/users/@me".to_string(),
                "/api/v10/applications/@me".to_string(),
                format!("/api/v10/guilds/{GUILD}/members/{BOT}"),
                format!("/api/v10/guilds/{GUILD}/roles"),
                format!("/api/v10/guilds/{GUILD}/invites"),
                format!("/api/v10/channels/{CHANNEL}"),
            ]
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn human_table_and_unused_privileged_intent_warn_without_failing() {
    let mock = mock(script(
        permissions(),
        (1 << 14) | (1 << 18),
        1,
        false,
        channel(),
    ))
    .await;
    let output = cli(&mock, &[], &[]).await;
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("STATUS  CHECK") && text.contains("PASS") && text.contains("WARN"));
    assert!(text.contains("Message Content intent") && text.contains("1 warn"));
    mock.shutdown().await;
}

fn ticket_triple(staff: &str) -> Vec<(&str, &str)> {
    vec![
        ("DISCORD_TICKET_CATEGORY_ID", "7777"),
        ("DISCORD_TICKET_STAFF_ROLE_ID", staff),
        ("DISCORD_TICKET_PANEL_CHANNEL_ID", "6666"),
    ]
}

#[tokio::test]
async fn intents_follow_automod_and_the_ticket_triple() {
    for (flags, vars, bodies, code) in [
        (0, vec![], vec![channel()], 1),
        (1 << 15, vec![("TWO_AUTOMOD", "1")], vec![channel()], 1),
        (
            (1 << 15) | (1 << 19),
            vec![("TWO_AUTOMOD", "1")],
            vec![channel()],
            0,
        ),
        (
            1 << 15,
            ticket_triple("5555"),
            vec![channel(), category_channel()],
            1,
        ),
        (
            (1 << 15) | (1 << 18),
            ticket_triple("5555"),
            vec![channel(), category_channel()],
            0,
        ),
    ] {
        let mock = mock(script_channels(permissions(), flags, 1, false, bodies)).await;
        let output = cli(&mock, &["--json"], &vars).await;
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn refused_activation_requests_no_privileged_message_content_intent() {
    let mock = mock(script(permissions(), 1 << 14, 1, false, channel())).await;
    // An identity the fence cannot recognize must never request the
    // privileged intent: with automod enabled but no portal grant, a
    // fence-less runtime would request MESSAGE_CONTENT and the ungranted
    // intent check fails the binary; the refusal keeps requested=false.
    let output = cli(
        &mock,
        &["--json"],
        &[
            ("DISCORD_TOKEN", "preflight-fixture-not-a-credential"),
            ("TWO_AUTOMOD", "1"),
        ],
    )
    .await;
    assert_eq!(output.status.code(), Some(0));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["check"] == "Message Content intent")
        .expect("message content check present");
    assert_eq!(check["status"], "PASS");
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .contains("runtime requested=false"),
        "{}",
        check["detail"]
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn roles_fail_for_missing_managed_or_high_targets_even_with_administrator() {
    for (id, position, managed, expected) in [
        ("5555", 1, false, 0),
        ("7777", 1, false, 1),
        ("5555", 10, false, 0),
        ("4444", 10, false, 1),
        ("5555", 11, false, 1),
        ("5555", 1, true, 1),
    ] {
        let mock = mock(script(
            Permissions::ADMINISTRATOR,
            1 << 14,
            position,
            managed,
            channel(),
        ))
        .await;
        let output = cli(
            &mock,
            &["--json", "--level-role-ids", id],
            &[("TWO_ONBOARDING_MODE", "legacy")],
        )
        .await;
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{id}/{position}/{managed}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        mock.shutdown().await;
    }
}

/// TOG-11802: the ticket staff role reuses the hierarchy gate (missing,
/// managed, @everyone, or not strictly below the bot fails closed), and the
/// ticket category slot requires GuildCategory. Static guidance only; the
/// report must never echo the fake token.
#[tokio::test]
async fn ticket_staff_role_and_category_fail_closed() {
    // PASS path: staff role below the bot + GuildCategory destination.
    // (Named pass_mock so later mock(...) calls in this fn still resolve
    // to the helper instead of the local binding.)
    let pass_mock = mock(script_channels(
        permissions(),
        (1 << 15) | (1 << 18),
        1,
        false,
        vec![channel(), category_channel()],
    ))
    .await;
    let output = cli(&pass_mock, &["--json"], &ticket_triple("5555")).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    pass_mock.shutdown().await;

    // Staff-role failures: missing (deleted), managed, @everyone, and above
    // the bot. Equal position still passes here: Twilight orders equal
    // positions by ascending snowflake, and 5555 sorts below the bot's 4444
    // (covered by the PASS path at position 1 and the existing equal-position
    // level-reward case). Each case needs a role snapshot carrying the
    // referenced role except the missing one.
    for (staff, position, managed) in [
        ("7777", 1, false),  // missing from the role snapshot
        ("5555", 1, true),   // managed (integration/bot role)
        ("2222", 0, false),  // @everyone must never be staff
        ("5555", 11, false), // above the bot
    ] {
        let mock = mock(script_channels(
            permissions(),
            (1 << 15) | (1 << 18),
            position,
            managed,
            vec![channel(), category_channel()],
        ))
        .await;
        let output = cli(&mock, &["--json"], &ticket_triple(staff)).await;
        assert_eq!(
            output.status.code(),
            Some(1),
            "{staff}/{position}/{managed}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            report["checks"].as_array().unwrap().iter().any(|check| check["check"]
                == format!("role {staff}")
                && check["status"] == "FAIL"),
            "{staff}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        mock.shutdown().await;
    }

    // Category mismatch: the category slot served a text channel.
    // (Distinct binding names: each `let mock = mock(...)` initializer
    // must resolve to the helper, not an earlier MockRest binding.)
    let mismatch_mock = mock(script_channels(
        permissions(),
        (1 << 15) | (1 << 18),
        1,
        false,
        vec![channel(), category_channel_as_text()],
    ))
    .await;
    let output = cli(&mismatch_mock, &["--json"], &ticket_triple("5555")).await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["check"] == format!("channel {CATEGORY}") && check["status"] == "FAIL"));
    mismatch_mock.shutdown().await;

    // Partial triple: only one of the three ticket keys set.
    let partial_mock = mock(script(
        permissions(),
        (1 << 15) | (1 << 18),
        1,
        false,
        channel(),
    ))
    .await;
    let output = cli(
        &partial_mock,
        &["--json"],
        &[("DISCORD_TICKET_STAFF_ROLE_ID", "5555")],
    )
    .await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["check"] == "ticket configuration" && check["status"] == "FAIL"));
    partial_mock.shutdown().await;
}

#[tokio::test]
async fn self_role_catalogue_collects_roles_and_panel_channels() {
    let panels = json!([{"channelId": CHANNEL.to_string(), "options": [{"roleId": TARGET_ROLE.to_string()}]}]).to_string();
    let mock = mock(script(permissions(), 1 << 15, 11, false, channel())).await;
    let output = cli(&mock, &["--json"], &[("TWO_SELF_ROLE_PANELS", &panels)]).await;
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["check"] == "role 5555" && check["status"] == "FAIL"));
    mock.shutdown().await;
}

#[tokio::test]
async fn legacy_requires_an_explicit_reward_export_but_session_does_not() {
    for (args, expected) in [
        (vec!["--json"], 1),
        (vec!["--json", "--level-role-ids", ""], 0),
    ] {
        let mock = mock(script(permissions(), 1 << 15, 1, false, channel())).await;
        let output = cli(&mock, &args, &[("TWO_ONBOARDING_MODE", "legacy")]).await;
        assert_eq!(output.status.code(), Some(expected));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn wrong_guild_wrong_type_embed_and_manage_message_denials_fail() {
    for variant in ["guild", "type", "embed", "manage"] {
        let mut body = channel();
        match variant {
            "guild" => body["guild_id"] = json!("9999"),
            "type" => body["type"] = json!(4),
            "embed" => {
                body["permission_overwrites"] = json!([{"id": GUILD.to_string(), "type": 0, "allow": "0", "deny": Permissions::EMBED_LINKS.bits().to_string()}])
            }
            "manage" => {
                body["permission_overwrites"] = json!([{"id": BOT.to_string(), "type": 1, "allow": "0", "deny": Permissions::MANAGE_MESSAGES.bits().to_string()}])
            }
            _ => unreachable!(),
        }
        let mock = mock(script(permissions(), (1 << 15) | (1 << 18), 1, false, body)).await;
        let output = cli(&mock, &["--json"], &[("TWO_AUTOMOD", "1")]).await;
        assert_eq!(output.status.code(), Some(1), "{variant}");
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn rejected_token_stops_and_does_not_echo_response_or_try_a_fallback() {
    let mock = mock(vec![ScriptedResponse::json(
        401,
        json!({"message": TOKEN, "code": 0}),
    )])
    .await;
    let output = cli(
        &mock,
        &["--json"],
        &[("DISCORD_BOT_TOKEN", "unused-fixture-token")],
    )
    .await;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("HTTP 401"));
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn denied_application_and_channel_reads_are_failures_not_successful_skips() {
    for (index, status) in [(1, 403), (5, 404)] {
        let mut responses = script(permissions(), 1 << 15, 1, false, channel());
        responses[index] = ScriptedResponse::json(status, json!({"message": TOKEN, "code": 0}));
        let mock = mock(responses).await;
        let output = cli(&mock, &["--json"], &[]).await;
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(mock.requests().len(), index + 1);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn present_empty_primary_never_selects_the_alias() {
    let mock = mock(vec![]).await;
    let output = cli(
        &mock,
        &["--json"],
        &[
            ("DISCORD_TOKEN", ""),
            ("DISCORD_BOT_TOKEN", "usable-fixture-token"),
        ],
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ready"], false);
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn absent_primary_still_selects_the_alias() {
    let mock = mock(script(permissions(), 1 << 15, 1, false, channel())).await;
    let output = cli(
        &mock,
        &["--json"],
        &[
            ("__REMOVE_DISCORD_TOKEN__", "1"),
            ("DISCORD_BOT_TOKEN", TOKEN),
        ],
    )
    .await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn invalid_configuration_and_nonloopback_seams_fail_before_rest() {
    for vars in [
        vec![("DISCORD_TOKEN", "")],
        vec![("GUILD_ID", "0")],
        vec![("DISCORD_STAFF_ALERT_CHANNEL_ID", "secret-not-an-id")],
        vec![("TWO_SELF_ROLE_PANELS", "not-json")],
        vec![("DISCORD_PREFLIGHT_API_BASE", "https://example.com")],
        vec![("DISCORD_PREFLIGHT_API_BASE", "http://192.0.2.1:80")],
    ] {
        let mock = mock(vec![]).await;
        let output = cli(&mock, &["--json"], &vars).await;
        assert_eq!(output.status.code(), Some(2));
        assert!(mock.requests().is_empty());
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn live_checks_without_admission_authority_refuse_before_rest() {
    let mock = mock(vec![]).await;
    let output = cli(&mock, &["--json"], &[("DISCORD_PREFLIGHT_API_BASE", "")]).await;
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).contains("TWO_DATABASE_URL"));
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn preflight_429_is_one_attempt_and_stops_the_check_sequence() {
    let mock = mock(vec![ScriptedResponse::json(
        429,
        json!({"retry_after":0.001,"global":true}),
    )])
    .await;
    let output = cli(&mock, &["--json"], &[]).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("HTTP 429"));
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn help_and_invalid_arguments_need_no_credentials_or_network() {
    let mock = mock(vec![]).await;
    let output = cli(&mock, &["--help"], &[("DISCORD_TOKEN", "")]).await;
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("two-bot preflight"));
    let output = cli(&mock, &["--unknown", "--json"], &[]).await;
    assert_eq!(output.status.code(), Some(2));
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}
