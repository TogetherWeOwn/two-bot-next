#![cfg(feature = "db")]

//! Protected acceptance: inspect a saved response without running it.
//!
//! Boundary contract: `/command-inspect name:<name>` defaults to `summary`;
//! `view:template` and `view:preview` are separate private responses. Optional
//! `arguments` is a UTF-16-bounded (500-unit) sample of ordinary invocation
//! arguments. The existing four-variable language ignores arguments: accepting
//! a sample MUST NOT introduce `{args}` or a second interpolation pass.
//!
//! Visible view headings are `Summary /name`, `Saved template /name`, and
//! `Preview /name`. The body is plain, markdown-escaped untrusted text; headings
//! and correction examples are trusted UI. A full view that cannot fit with its
//! heading must explicitly refuse or mark truncation, never silently clip.
//!
//! These call the actual Twilight interaction -> CustomCommandRuntime -> router
//! -> ActionExecutor boundary. Only Discord HTTP is doubled, on loopback. The
//! concrete runtime accepts PgPool, not a store trait, so real disposable-store
//! fixtures replace a fake lookup; full-row snapshots catch writes and timestamp
//! changes. No source changes, skip, new renderer, or test-only runtime adapter.

#[allow(dead_code)]
mod common;

use std::sync::Arc;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use twilight_model::{application::interaction::Interaction, channel::Message, id::Id};
use two_bot_core::{
    command_permissions::{command_permission, CommandSurface},
    custom_command_store as store,
    custom_commands::{
        builtin_command_names, reserved_command_names, validate_put_input,
        AutomationMessageAcceptance, CommandError, PutCommandInput, StoredCommand,
    },
    InteractionRouter, RouterGates,
};
use two_bot_discord::{
    custom_commands::{CustomCommandError, CustomCommandRuntime, TextCommandOutcome},
    interactions::publish_commands,
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

#[path = "support/custom_command_trigger_eligibility.rs"]
mod trigger_eligibility;

const MANAGEMENT: &str = "command-inspect";
const TOKEN: &str = "inspect-acceptance-fixture";

#[path = "support/custom_command_draft_acceptance.rs"]
mod draft_acceptance;

fn gates(enabled: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        automations: enabled,
        moderation: false,
        voice: false,
        voice_assistant: false,
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
        ActionExecutor::with_proxy(TOKEN.to_owned(), Some(mock.origin()))
            .expect("loopback executor"),
        1111,
    )
}

fn interaction(id: u64, permissions: u64, options: Vec<Value>) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": TOKEN, "version": 1,
        "guild_id": "2222", "channel": {"id": "4444", "type": 0, "name": "commands"},
        "authorizing_integration_owners": {"0": "2222"}, "entitlements": [],
        "member": {
            "permissions": permissions.to_string(), "roles": [], "joined_at": null,
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null}
        },
        "data": {"id": "5555", "name": MANAGEMENT, "type": 1, "options": options}
    }))
    .expect("valid Twilight fixture")
}

fn option(name: &str, value: &str) -> Value {
    json!({"name": name, "type": 3, "value": value})
}

fn inspect(id: u64, name: &str, view: Option<&str>, arguments: Option<&str>) -> Interaction {
    let mut options = vec![option("name", name)];
    if let Some(view) = view {
        options.push(option("view", view));
    }
    if let Some(arguments) = arguments {
        options.push(option("arguments", arguments));
    }
    interaction(id, 32, options)
}

fn prefix(id: u64) -> Message {
    serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": "2222", "channel_id": "4444", "type": 0,
        "author": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null},
        "content": "!faq one two", "timestamp": "2026-10-23T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false,
        "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false
    }))
    .expect("valid prefix fixture")
}

async fn discord() -> MockRest {
    MockRest::with_responder(|request| match request.method.as_str() {
        "POST" if request.path.ends_with("/callback") => ScriptedResponse::status(204),
        "PATCH" if request.path.ends_with("/messages/@original") => {
            ScriptedResponse::json(200, json!({"id": "9000"}))
        }
        "POST" if request.path.ends_with("/channels/4444/messages") => {
            ScriptedResponse::json(200, json!({"id": "9001"}))
        }
        _ => ScriptedResponse::status(403),
    })
    .await
}

struct Observed {
    result: Result<bool, CustomCommandError>,
    requests: Vec<RestRequest>,
    id: u64,
}

async fn observe(
    runtime: &CustomCommandRuntime,
    mock: &MockRest,
    input: &Interaction,
    server: Option<&str>,
) -> Observed {
    let start = mock.requests().len();
    let result = runtime.handle_interaction(input, server).await;
    Observed {
        result,
        requests: mock.requests()[start..].to_vec(),
        id: input.id.get(),
    }
}

fn no_mentions(body: &Value) {
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    for key in ["users", "roles"] {
        assert!(
            body["allowed_mentions"][key].is_null() || body["allowed_mentions"][key] == json!([]),
            "no explicit mention allowlist: {body}"
        );
    }
    assert_ne!(body["allowed_mentions"]["replied_user"], true);
}

fn private_content(observed: &Observed) -> String {
    assert!(
        matches!(observed.result, Ok(true)),
        "management dispatch must own /{MANAGEMENT}: {:?}",
        observed.result
    );
    let callback = format!("/interactions/{}/{TOKEN}/callback", observed.id);
    let edit = format!("/webhooks/1111/{TOKEN}/messages/@original");
    // A DM, follow-up, channel send, registry read/write or second callback is
    // not a private initial response. Check every request, not just the last.
    assert!(
        observed.requests.iter().all(|request| {
            (request.method == "POST" && request.path.ends_with(&callback))
                || (request.method == "PATCH" && request.path.ends_with(&edit))
        }),
        "unexpected execution/registration traffic: {:?}",
        observed.requests
    );
    let callbacks: Vec<_> = observed
        .requests
        .iter()
        .filter(|request| request.method == "POST")
        .collect();
    assert_eq!(callbacks.len(), 1, "one private acknowledgement");
    let response: Value = serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    assert_eq!(response["data"]["flags"].as_u64().unwrap_or(0) & 64, 64);
    no_mentions(&response["data"]);
    let body = if response["type"] == 5 {
        assert_eq!(observed.requests.len(), 2, "one defer and one completion");
        let completion = observed
            .requests
            .iter()
            .find(|request| request.method == "PATCH")
            .expect("deferred response must complete");
        let body: Value = serde_json::from_slice(&completion.body).expect("completion JSON");
        no_mentions(&body);
        body
    } else {
        assert_eq!(response["type"], 4);
        assert_eq!(observed.requests.len(), 1);
        response["data"].clone()
    };
    assert!(body["embeds"].is_null() || body["embeds"] == json!([]));
    assert!(body["attachments"].is_null() || body["attachments"] == json!([]));
    let content = body["content"].as_str().expect("content-only private view");
    assert!(!content.is_empty());
    assert!(
        content.encode_utf16().count() <= 2000,
        "Discord content limit"
    );
    content.to_owned()
}

fn view_body(content: &str, heading: &str) -> String {
    let (actual, body) = content.split_once('\n').expect("heading and separate body");
    assert_eq!(actual, heading, "trusted heading must not be template text");
    body.to_owned()
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create isolated migrated test DB")
}

async fn seed(pool: &sqlx::PgPool, guild: &str, name: &str, template: &str, enabled: bool) {
    store::put_command(
        pool,
        &StoredCommand {
            guild_id: guild.to_owned(),
            name: name.to_owned(),
            description: "Saved FAQ description".to_owned(),
            template: template.to_owned(),
            text_trigger: Some(format!("!{name}")),
            enabled,
        },
        "3333",
        "3333",
        "2026-10-01T00:00:00Z",
        "2026-10-02T00:00:00Z",
    )
    .await
    .expect("seed saved definition, including disabled/imported rows");
}

async fn definitions(pool: &sqlx::PgPool) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(c) FROM automation_commands c ORDER BY guild_id, name")
        .fetch_all(pool)
        .await
        .expect("snapshot ALL definition fields, including creator/timestamps")
}

async fn execution_facts(pool: &sqlx::PgPool) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT to_jsonb(a) FROM automation_audit_log a WHERE action IN ('command.run', 'command.text_attempt') ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("read execution claims and results")
}

// A wrong registry type/gate/reservation makes the workflow undiscoverable or
// allows a custom row to bypass management authority, even if a helper works.
#[test]
fn inspect_is_a_guild_only_management_command_with_three_bounded_views() {
    let router = InteractionRouter::new(gates(true));
    let definitions = router.publish_set(&[]).expect("complete registry");
    let wire = serde_json::to_value(publish_commands(&definitions)).expect("Discord command wire");
    let definition = wire
        .as_array()
        .unwrap()
        .iter()
        .find(|definition| definition["name"] == MANAGEMENT)
        .expect("publish the inspect workflow, not just a helper");
    assert_eq!(definition["default_member_permissions"], "32");
    assert!(
        !definitions
            .iter()
            .find(|definition| definition.name == MANAGEMENT)
            .expect("core guild-only definition")
            .dm_permission
    );
    let options = definition["options"].as_array().expect("options");
    assert_eq!(
        options.len(),
        4,
        "one workflow with exclusive saved/draft input"
    );
    let name = options
        .iter()
        .find(|value| value["name"] == "name")
        .unwrap();
    assert_eq!(name["type"], 3);
    assert_ne!(
        name["required"], true,
        "draft input must not require a saved name"
    );
    assert_eq!(name["max_length"], 32);
    let draft = options
        .iter()
        .find(|value| value["name"] == "draft")
        .expect("discover unsaved preview on the existing inspection surface");
    assert_eq!(draft["type"], 3);
    assert_ne!(draft["required"], true);
    assert_eq!(draft["max_length"], 2000);
    let view = options
        .iter()
        .find(|value| value["name"] == "view")
        .unwrap();
    assert_eq!(view["type"], 3);
    assert_ne!(view["required"], true);
    let choices: Vec<_> = view["choices"]
        .as_array()
        .expect("bounded views, not arbitrary action names")
        .iter()
        .map(|choice| choice["value"].as_str().unwrap())
        .collect();
    assert_eq!(choices, ["summary", "template", "preview"]);
    let arguments = options
        .iter()
        .find(|value| value["name"] == "arguments")
        .unwrap();
    assert_eq!(arguments["type"], 3);
    assert_ne!(arguments["required"], true);
    assert_eq!(arguments["max_length"], 500);
    assert!(!InteractionRouter::new(gates(false))
        .publish_set(&[])
        .unwrap()
        .iter()
        .any(|definition| definition.name == MANAGEMENT));
    let permission = command_permission(MANAGEMENT).expect("live authorization table row");
    assert_eq!(permission.required_permissions, 32);
    assert_eq!(permission.surface, CommandSurface::BuiltinSlash);
    assert!(builtin_command_names().contains(MANAGEMENT));
    assert!(reserved_command_names().contains(MANAGEMENT));
    assert!(matches!(
        validate_put_input(
            &PutCommandInput {
                name: MANAGEMENT.to_owned(),
                description: "Do not shadow management".to_owned(),
                template: "No".to_owned(),
                text_trigger: None,
            },
            &reserved_command_names(),
        ),
        Err(CommandError::ReservedName(_))
    ));
}

#[tokio::test]
async fn inspect_live_permission_and_automation_refusals_precede_lookup() {
    let pool = PgPoolOptions::new().connect_lazy_with(
        PgConnectOptions::new()
            .host("agent-testdb")
            .username("agent_test")
            .password(""),
    );
    pool.close().await;
    // A missed early gate sees a closed pool, never a real DB connection.
    for (enabled, permissions, expected) in [
        (true, 0, "Manage Server"),
        (true, 8, "Manage Server"),
        (false, 32, "Automations are disabled"),
    ] {
        let mock = discord().await;
        let runtime = runtime(pool.clone(), &mock, enabled);
        let observed = observe(
            &runtime,
            &mock,
            &interaction(
                100,
                permissions,
                vec![option("name", "faq"), option("view", "preview")],
            ),
            Some("TWO"),
        )
        .await;
        mock.shutdown().await;
        let content = private_content(&observed);
        assert!(
            content.contains(expected),
            "truthful early refusal: {content}"
        );
        assert!(!content.contains("temporarily unavailable"));
    }
}

#[tokio::test]
async fn inspect_foreign_guild_dm_and_wrong_application_are_unowned_and_silent() {
    let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new().host("agent-testdb"));
    pool.close().await;
    let mock = discord().await;
    let runtime = runtime(pool, &mock, true);
    for scope in ["guild", "dm", "application"] {
        let mut input = inspect(110, "faq", Some("preview"), None);
        match scope {
            "guild" => input.guild_id = Some(Id::new(7777)),
            "dm" => input.guild_id = None,
            "application" => input.application_id = Id::new(9999),
            _ => unreachable!(),
        }
        assert!(matches!(
            runtime.handle_interaction(&input, Some("TWO")).await,
            Ok(false)
        ));
    }
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn inspect_lookup_outage_is_not_a_missing_definition_or_a_run() {
    let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new().host("agent-testdb"));
    pool.close().await;
    let mock = discord().await;
    let runtime = runtime(pool, &mock, true);
    let observed = observe(
        &runtime,
        &mock,
        &inspect(120, "faq", Some("preview"), None),
        Some("TWO"),
    )
    .await;
    assert!(runtime.published_commands().is_none());
    mock.shutdown().await;
    let content = private_content(&observed);
    assert!(content.contains("temporarily unavailable"), "{content}");
    assert!(content.contains("try again"), "{content}");
    assert!(!content.contains("No command") && !content.contains("not found"));
    assert!(!content.contains("PoolClosed") && !content.contains("agent-testdb"));
}

// Removing the draft-only view guard must not turn a saved-only view into a
// successful draft preview, even though both view names pass option parsing.
async fn draft_saved_only_view_refuses_without_sql_or_effects(view: &str) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind SQL-access tripwire");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_millis(100))
        .connect_lazy_with(
            PgConnectOptions::new()
                .host("127.0.0.1")
                .port(listener.local_addr().expect("tripwire address").port())
                .username("preview_fixture")
                .database("preview_fixture")
                .ssl_mode(sqlx::postgres::PgSslMode::Disable),
        );
    let mock = discord().await;
    let runtime = runtime(pool.clone(), &mock, true);
    let observed = observe(
        &runtime,
        &mock,
        &interaction(
            490,
            32,
            vec![option("draft", "Hello"), option("view", view)],
        ),
        Some("TWO"),
    )
    .await;
    let publication = runtime.published_commands();
    pool.close().await;
    mock.shutdown().await;
    let listener = listener.into_std().expect("nonblocking SQL tripwire");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "draft {view} refusal attempted storage access"
    );
    assert!(publication.is_none(), "refusal never republishes commands");
    let content = private_content(&observed);
    assert_eq!(
        content,
        "Invalid draft view: only preview is available for unsaved text. Use /command-inspect draft:Hello view:preview",
        "draft {view} must reach the saved-only view refusal"
    );
    assert!(!content.contains("Draft preview"));
    for request in &observed.requests {
        let wire: Value = serde_json::from_slice(&request.body).expect("private response JSON");
        let data = if request.method == "POST" {
            &wire["data"]
        } else {
            &wire
        };
        assert!(
            data["components"].is_null() || data["components"] == json!([]),
            "refusal must not expose mutation controls"
        );
    }
}

#[tokio::test]
async fn draft_summary_view_refuses_without_sql_or_effects() {
    draft_saved_only_view_refuses_without_sql_or_effects("summary").await;
}

#[tokio::test]
async fn draft_template_view_refuses_without_sql_or_effects() {
    draft_saved_only_view_refuses_without_sql_or_effects("template").await;
}

#[tokio::test]
async fn inspect_forged_names_views_and_arguments_refuse_before_any_lookup() {
    let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new().host("agent-testdb"));
    pool.close().await;
    let mock = discord().await;
    let runtime = runtime(pool, &mock, true);
    let mut cases: Vec<(&str, Vec<Value>)> = vec![
        ("name", vec![]),
        ("name", vec![json!({"name":"name", "type":4, "value":7})]),
        ("name", vec![option("name", "faq"), option("name", "other")]),
        (
            "view",
            vec![option("name", "faq"), option("view", "execute")],
        ),
        (
            "view",
            vec![
                option("name", "faq"),
                json!({"name":"view", "type":4, "value":1}),
            ],
        ),
        (
            "view",
            vec![
                option("name", "faq"),
                option("view", "preview"),
                option("view", "template"),
            ],
        ),
        (
            "arguments",
            vec![
                option("name", "faq"),
                option("view", "preview"),
                json!({"name":"arguments", "type":4, "value":7}),
            ],
        ),
        (
            "arguments",
            vec![
                option("name", "faq"),
                option("arguments", "one"),
                option("arguments", "two"),
            ],
        ),
        (
            "arguments",
            vec![
                option("name", "faq"),
                option("arguments", &"😀".repeat(251)),
            ],
        ),
        (
            "arguments",
            vec![option("name", "faq"), option("arguments", &"a".repeat(501))],
        ),
    ];
    for name in [
        "",
        " bad",
        "bad name",
        "f/aq",
        "@everyone",
        "../faq",
        "世界",
        "command-inspect",
    ] {
        cases.push(("name", vec![option("name", name)]));
    }
    cases.push(("name", vec![option("name", &"a".repeat(33))]));
    let mut observations = Vec::new();
    for (index, (field, options)) in cases.into_iter().enumerate() {
        let input = interaction(200 + index as u64, 32, options);
        observations.push((field, observe(&runtime, &mock, &input, Some("TWO")).await));
    }
    mock.shutdown().await;
    for (field, observed) in observations {
        let content = private_content(&observed);
        assert!(
            content.to_lowercase().contains(field),
            "identify invalid {field}: {content}"
        );
        assert!(
            content.contains("/command-inspect"),
            "usable correction example: {content}"
        );
        assert!(
            !content.contains("temporarily unavailable"),
            "validate before store access"
        );
        assert!(
            !content.contains("Preview /faq\n"),
            "never treat malformed input as a preview"
        );
    }
}

#[tokio::test]
async fn inspect_summary_template_and_preview_are_separate_read_only_views() {
    let db = database().await;
    seed(
        db.pool(),
        "2222",
        "faq",
        "Hi {user} ({username}) in {server} {channel}",
        true,
    )
    .await;
    seed(
        db.pool(),
        "7777",
        "faq",
        "FOREIGN TEMPLATE MUST NEVER LEAK",
        true,
    )
    .await;
    let before = definitions(db.pool()).await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock, true);
    let mut observations = Vec::new();
    for (id, view) in [
        (300, None),
        (301, Some("summary")),
        (302, Some("template")),
        (303, Some("preview")),
    ] {
        observations.push(
            observe(
                &runtime,
                &mock,
                &inspect(id, "FAQ", view, None),
                Some("TWO"),
            )
            .await,
        );
    }
    let after = definitions(db.pool()).await;
    let facts = execution_facts(db.pool()).await;
    let publication = runtime.published_commands();
    mock.shutdown().await;
    db.close()
        .await
        .expect("fixture teardown before assertions");
    assert_eq!(
        after, before,
        "no definition, state, creator or timestamp mutation"
    );
    assert!(
        facts.is_empty(),
        "no command.run or durable text attempt: {facts:?}"
    );
    assert!(
        publication.is_none(),
        "inspection never republishes the registry"
    );
    let content: Vec<_> = observations.iter().map(private_content).collect();
    assert_eq!(content[0], content[1], "summary is the default");
    let summary = view_body(&content[0], "Summary /faq");
    assert!(summary.contains("Saved FAQ description"));
    assert!(summary.to_lowercase().contains("enabled"));
    assert!(summary.contains("!faq"));
    assert!(summary.contains("/command-inspect name:faq view:template"));
    assert!(summary.contains("/command-inspect name:faq view:preview"));
    assert!(
        summary.contains("/command name:faq template:"),
        "usable correction example"
    );
    assert!(
        !summary.contains("Hi {user}"),
        "summary does not bundle a template body"
    );
    assert_eq!(
        view_body(&content[2], "Saved template /faq"),
        "Hi {user} ({username}) in {server} {channel}"
    );
    assert_eq!(
        view_body(&content[3], "Preview /faq"),
        "Hi <@3333> (tester) in TWO <#4444>"
    );
    assert!(content
        .iter()
        .all(|value| !value.contains("FOREIGN TEMPLATE")));
}

#[tokio::test]
async fn inspect_disabled_saved_definitions_are_still_inspectable_and_never_enabled() {
    let db = database().await;
    seed(
        db.pool(),
        "2222",
        "faq",
        "Paused response {username}",
        false,
    )
    .await;
    let before = definitions(db.pool()).await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock, true);
    let mut observations = Vec::new();
    for (index, view) in ["summary", "template", "preview"].into_iter().enumerate() {
        observations.push(
            observe(
                &runtime,
                &mock,
                &inspect(320 + index as u64, "faq", Some(view), None),
                Some("TWO"),
            )
            .await,
        );
    }
    let after = definitions(db.pool()).await;
    let facts = execution_facts(db.pool()).await;
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
    assert_eq!(after, before);
    assert!(facts.is_empty());
    let content: Vec<_> = observations.iter().map(private_content).collect();
    assert!(content[0].to_lowercase().contains("disabled"));
    assert_eq!(
        view_body(&content[1], "Saved template /faq"),
        "Paused response {username}"
    );
    assert_eq!(
        view_body(&content[2], "Preview /faq"),
        "Paused response tester"
    );
}

#[tokio::test]
async fn inspect_missing_other_guild_only_and_deleted_definitions_are_truthful_misses() {
    let db = database().await;
    seed(db.pool(), "7777", "faq", "FOREIGN SECRET FIXTURE", true).await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock, true);
    let mut observations = Vec::new();
    for (id, name) in [(340, "missing"), (341, "faq")] {
        observations.push(
            observe(
                &runtime,
                &mock,
                &inspect(id, name, Some("preview"), None),
                Some("TWO"),
            )
            .await,
        );
    }
    seed(
        db.pool(),
        "2222",
        "faq",
        "OLD DEFINITION MUST NOT BE CACHED",
        true,
    )
    .await;
    let exists = observe(
        &runtime,
        &mock,
        &inspect(342, "faq", Some("preview"), None),
        Some("TWO"),
    )
    .await;
    store::delete_command(db.pool(), "2222", "faq")
        .await
        .expect("fixture deletion");
    let before = definitions(db.pool()).await;
    observations.push(
        observe(
            &runtime,
            &mock,
            &inspect(343, "faq", Some("template"), None),
            Some("TWO"),
        )
        .await,
    );
    let after = definitions(db.pool()).await;
    let facts = execution_facts(db.pool()).await;
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
    assert_eq!(after, before);
    assert!(facts.is_empty());
    assert_eq!(
        view_body(&private_content(&exists), "Preview /faq"),
        "OLD DEFINITION MUST NOT BE CACHED"
    );
    for observed in observations {
        let content = private_content(&observed);
        let lower = content.to_lowercase();
        assert!(
            lower.contains("not found") || lower.contains("no command"),
            "truthful miss: {content}"
        );
        assert!(
            content.contains("/command-list"),
            "discover/correct the saved name"
        );
        assert!(!content.contains("FOREIGN") && !content.contains("OLD DEFINITION"));
        assert!(!content.contains("temporarily unavailable"));
    }
}

#[tokio::test]
async fn inspect_preview_preserves_the_four_variable_single_pass_language_with_sample_arguments() {
    let db = database().await;
    seed(db.pool(), "2222", "faq", "literal {User} {}", true).await;
    seed(
        db.pool(),
        "2222",
        "multi",
        "{user}|{username}|{server}|{channel}|{{user}}",
        true,
    )
    .await;
    let before = definitions(db.pool()).await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock, true);
    let mut observations = Vec::new();
    for (index, arguments) in [
        None,
        Some(""),
        Some("one"),
        Some("one two 三"),
        Some("{server} @everyone"),
    ]
    .into_iter()
    .enumerate()
    {
        observations.push(
            observe(
                &runtime,
                &mock,
                &inspect(360 + index as u64, "faq", Some("preview"), arguments),
                Some("TWO"),
            )
            .await,
        );
    }
    let multi = observe(
        &runtime,
        &mock,
        &inspect(370, "multi", Some("preview"), Some("one two")),
        Some("TWO"),
    )
    .await;
    let mut single_pass = inspect(371, "multi", Some("preview"), None);
    single_pass
        .member
        .as_mut()
        .unwrap()
        .user
        .as_mut()
        .unwrap()
        .name = "{server}".to_owned();
    let single_pass = observe(&runtime, &mock, &single_pass, Some("TWO")).await;
    let mut other_context = inspect(372, "multi", Some("preview"), Some("one two"));
    let actor = other_context
        .member
        .as_mut()
        .unwrap()
        .user
        .as_mut()
        .unwrap();
    actor.id = Id::new(6666);
    actor.name = "三".to_owned();
    other_context.channel.as_mut().unwrap().id = Id::new(5556);
    let other_context = observe(&runtime, &mock, &other_context, Some("世界")).await;
    let after = definitions(db.pool()).await;
    let facts = execution_facts(db.pool()).await;
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
    assert_eq!(after, before);
    assert!(facts.is_empty());
    for observed in observations {
        assert_eq!(
            view_body(&private_content(&observed), "Preview /faq"),
            "literal {User} {}"
        );
    }
    assert_eq!(
        view_body(&private_content(&multi), "Preview /multi"),
        "<@3333>\\|tester\\|TWO\\|<#4444>\\|{<@3333>}"
    );
    assert_eq!(
        view_body(&private_content(&single_pass), "Preview /multi"),
        "<@3333>\\|{server}\\|TWO\\|<#4444>\\|{<@3333>}"
    );
    assert_eq!(
        view_body(&private_content(&other_context), "Preview /multi"),
        "<@6666>\\|三\\|世界\\|<#5556>\\|{<@6666>}"
    );
}

#[tokio::test]
async fn inspect_hostile_saved_member_and_argument_text_cannot_forge_trusted_ui() {
    let db = database().await;
    seed(
        db.pool(),
        "2222",
        "faq",
        "**FORGED** ``` @everyone <@&8888> [click](https://example.invalid) {username}",
        true,
    )
    .await;
    sqlx::query("UPDATE automation_commands SET description = '**FORGED** ``` @everyone' WHERE guild_id = '2222'")
        .execute(db.pool()).await.expect("fixture hostile description");
    let before = definitions(db.pool()).await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock, true);
    let mut observations = Vec::new();
    for (index, view) in ["summary", "template", "preview"].into_iter().enumerate() {
        let mut input = inspect(
            390 + index as u64,
            "faq",
            Some(view),
            Some("**ARGUMENT** ``` @here <@9999> {user}"),
        );
        input.member.as_mut().unwrap().user.as_mut().unwrap().name = "**MEMBER** ```".to_owned();
        observations.push(observe(&runtime, &mock, &input, Some("TWO")).await);
    }
    let after = definitions(db.pool()).await;
    let facts = execution_facts(db.pool()).await;
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
    assert_eq!(after, before);
    assert!(facts.is_empty());
    let content: Vec<_> = observations.iter().map(private_content).collect();
    for value in &content {
        assert!(
            !value.contains("**FORGED**") && !value.contains("```"),
            "untrusted markdown escaped: {value}"
        );
        assert!(value.contains("\\*\\*FORGED\\*\\*"));
        assert!(!value.contains("[click](https://example.invalid)"));
        assert!(!value.contains("**ARGUMENT**"));
    }
    let preview = view_body(&content[2], "Preview /faq");
    assert!(preview.contains("\\*\\*MEMBER\\*\\* \\`\\`\\`"));
    assert!(
        preview.contains("@everyone"),
        "mention safety comes from explicit empty allowed_mentions"
    );
    assert!(preview.contains("<@&8888>"));
}

fn full_or_explicitly_bounded(content: &str, full: &str) {
    if !content.contains(full) {
        let lower = content.to_lowercase();
        assert!(
            lower.contains("truncated")
                || lower.contains("too long")
                || lower.contains("cannot fit"),
            "never display a misleading partial saved/rendered body: {content}"
        );
    }
    assert!(content.encode_utf16().count() <= 2000);
}

#[tokio::test]
async fn inspect_unicode_limits_include_headings_and_escaped_output_in_every_view() {
    let db = database().await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock, true);
    let mut observations = Vec::new();
    for (index, (name, template)) in [
        ("a".repeat(32), "😀".repeat(1000)),
        ("ascii".to_owned(), "x".repeat(2000)),
        ("escaped".to_owned(), "*".repeat(1990)),
        ("expansion".to_owned(), "{username}".repeat(200)),
    ]
    .into_iter()
    .enumerate()
    {
        seed(db.pool(), "2222", &name, &template, true).await;
        let before = definitions(db.pool()).await;
        let mut views = Vec::new();
        for (offset, view) in ["summary", "template", "preview"].into_iter().enumerate() {
            let mut input = inspect(
                420 + (index * 3 + offset) as u64,
                &name,
                Some(view),
                Some(&"😀".repeat(250)),
            );
            input.member.as_mut().unwrap().user.as_mut().unwrap().name = "😀".repeat(32);
            views.push(observe(&runtime, &mock, &input, Some("TWO")).await);
        }
        let after = definitions(db.pool()).await;
        observations.push((template, before, after, views));
    }
    let facts = execution_facts(db.pool()).await;
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
    assert!(facts.is_empty());
    for (template, before, after, views) in observations {
        assert_eq!(after, before);
        let content: Vec<_> = views.iter().map(private_content).collect();
        assert!(content[0].starts_with("Summary /"));
        assert!(content[0].contains("Saved FAQ description"));
        let escaped = template.replace('*', "\\*");
        full_or_explicitly_bounded(&content[1], &escaped);
        if template.contains("{username}") {
            full_or_explicitly_bounded(&content[2], &"😀".repeat(6400));
        } else {
            full_or_explicitly_bounded(&content[2], &escaped);
        }
    }
}

#[tokio::test]
async fn inspect_invalid_saved_templates_refuse_without_rendering_or_running() {
    let db = database().await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock, true);
    let mut observations = Vec::new();
    // Postgres's scalar-length constraint permits 1001 astral characters;
    // the shared input validator correctly refuses their 2002 UTF-16 units.
    for (index, template) in [
        "Hello {unknown}".to_owned(),
        "{args}".to_owned(),
        "😀".repeat(1001),
    ]
    .into_iter()
    .enumerate()
    {
        seed(db.pool(), "2222", "faq", &template, true).await;
        let before = definitions(db.pool()).await;
        let observed = observe(
            &runtime,
            &mock,
            &inspect(460 + index as u64, "faq", Some("preview"), Some("one two")),
            Some("TWO"),
        )
        .await;
        let after = definitions(db.pool()).await;
        observations.push((before, after, observed));
    }
    let facts = execution_facts(db.pool()).await;
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
    assert!(facts.is_empty());
    for (before, after, observed) in observations {
        assert_eq!(after, before);
        let content = private_content(&observed);
        assert!(content.to_lowercase().contains("template"));
        assert!(
            !content.starts_with("Preview /faq\n"),
            "invalid definition is not a rendered preview"
        );
        assert!(
            content.contains("/command name:faq template:"),
            "actionable correction"
        );
        assert!(!content.contains("temporarily unavailable"));
    }
}

#[tokio::test]
async fn inspect_preview_does_not_spend_text_cooldown_or_claim_and_ordinary_prefix_still_runs() {
    let db = database().await;
    seed(db.pool(), "2222", "faq", "Hello {username}", true).await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock, true);
    let clone = runtime.clone();
    let before = definitions(db.pool()).await;
    let first = observe(
        &runtime,
        &mock,
        &inspect(480, "faq", Some("preview"), Some("one two")),
        Some("TWO"),
    )
    .await;
    let second = observe(
        &clone,
        &mock,
        &inspect(481, "faq", Some("preview"), None),
        Some("TWO"),
    )
    .await;
    let preview_facts = execution_facts(db.pool()).await;
    let preview_definitions = definitions(db.pool()).await;
    let start = mock.requests().len();
    // No clock advance: a preview mistakenly admitted to the actor execution
    // window would turn this first real invocation into CoolingDown.
    let delivered = clone
        .handle_message(
            &prefix(482),
            AutomationMessageAcceptance::Unmatched,
            true,
            Some("TWO"),
        )
        .await;
    let replay = runtime
        .handle_message(
            &prefix(482),
            AutomationMessageAcceptance::Unmatched,
            true,
            Some("TWO"),
        )
        .await;
    let next = runtime
        .handle_message(
            &prefix(483),
            AutomationMessageAcceptance::Unmatched,
            true,
            Some("TWO"),
        )
        .await;
    let execution_requests = mock.requests()[start..].to_vec();
    let facts = execution_facts(db.pool()).await;
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
    assert_eq!(preview_definitions, before);
    assert!(
        preview_facts.is_empty(),
        "no run claim or result for either preview"
    );
    assert_eq!(
        view_body(&private_content(&first), "Preview /faq"),
        "Hello tester"
    );
    assert_eq!(
        view_body(&private_content(&second), "Preview /faq"),
        "Hello tester"
    );
    assert!(
        matches!(delivered, Ok(TextCommandOutcome::Delivered)),
        "{delivered:?}"
    );
    assert!(
        matches!(replay, Ok(TextCommandOutcome::AlreadyAttempted)),
        "{replay:?}"
    );
    assert!(
        matches!(next, Ok(TextCommandOutcome::CoolingDown)),
        "{next:?}"
    );
    assert_eq!(
        execution_requests.len(),
        1,
        "only the real first invocation sends"
    );
    assert_eq!(execution_requests[0].method, "POST");
    assert!(execution_requests[0]
        .path
        .ends_with("/channels/4444/messages"));
    let body: Value = serde_json::from_slice(&execution_requests[0].body).unwrap();
    assert_eq!(body["content"], "Hello tester");
    no_mentions(&body);
    assert_eq!(
        facts.len(),
        2,
        "only one text attempt and its one real command.run result"
    );
    assert!(facts
        .iter()
        .any(|fact| fact["action"] == "command.run" && fact["outcome"] == "ok"));
    assert!(facts
        .iter()
        .all(|fact| fact["id"] != "custom:run:480" && fact["id"] != "custom:run:481"));
}
