//! Protected acceptance contract for `/command-clone source:<name> destination:<name>`.
//! Exercise the real public runtime, router, service, store, audit transaction
//! and full-set publisher. Only Discord is doubled; every DB is disposable.
//! Baseline: clone is not a management command, so ownership/admission is RED.
#![cfg(feature = "db")]

#[path = "support/command_clone_registry.rs"]
mod clone_registry;
#[allow(dead_code)]
mod common;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::PgPool;
use twilight_model::{application::interaction::Interaction, channel::Message};
use two_bot_core::{
    custom_command_store as store,
    custom_commands::{
        reserved_command_names, AuditRecord, AutomationMessageAcceptance, StoredCommand,
    },
    InteractionRouter, RouterGates,
};
use two_bot_discord::{
    custom_commands::{CustomCommandError, CustomCommandRuntime, TextCommandOutcome},
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const ACTOR: &str = "3333";
const OLD_AT: &str = "2026-10-01T00:00:00Z";
const TEMPLATE: &str = "Saved private response café 🦀 for {username} in {server} / {channel}";
const DESCRIPTION: &str = "Saved response description";
const PREFIX_MESSAGE: u64 = 8001;

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

fn interaction(id: u64, command: &str, options: Value) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": format!("clone-fixture-{id}"), "version": 1,
        "guild_id": GUILD, "channel": {"id": "4444", "type": 0},
        "authorizing_integration_owners": {"0": GUILD}, "entitlements": [],
        "member": {
            "permissions": "32", "roles": [], "joined_at": null,
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": ACTOR, "username": "tester", "discriminator": "0000", "avatar": null}
        },
        "data": {"id": "5555", "name": command, "type": 1, "options": options}
    }))
    .expect("valid Twilight command interaction")
}

fn clone_request(id: u64, source: &str, destination: &str) -> Interaction {
    interaction(
        id,
        "command-clone",
        json!([
            {"name": "source", "type": 3, "value": source},
            {"name": "destination", "type": 3, "value": destination}
        ]),
    )
}

fn prefix_message(id: u64, content: &str, actor: &str) -> Message {
    serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": GUILD, "channel_id": "4444",
        "author": {"id": actor, "username": "tester", "discriminator": "0000", "avatar": null, "bot": false},
        "content": content, "timestamp": "2026-10-01T00:00:00.000000+00:00", "edited_timestamp": null,
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false, "type": 0
    }))
    .expect("valid human prefix message")
}

struct Fixture {
    db: TestDatabase,
    http: MockRest,
    runtime: CustomCommandRuntime,
    executor: ActionExecutor,
    fail_publication: Arc<AtomicBool>,
}

impl Fixture {
    async fn new(enabled_source: bool) -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("TWO_TEST_DATABASE_URL must name the disposable test bootstrap; never skip");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("isolated migrated disposable DB");
        seed(db.pool(), GUILD, "source", enabled_source, Some("!source")).await;
        seed(db.pool(), GUILD, "occupied", true, Some("!occupied")).await;
        seed(db.pool(), "9999", "foreign-only", true, Some("!foreign")).await;
        store::audit(
            db.pool(),
            &AuditRecord::run(GUILD, "7777", "source", true, None),
            "source-run-history",
            OLD_AT,
        )
        .await
        .expect("source run history");
        assert!(store::claim_text_attempt(
            db.pool(),
            GUILD,
            "7777",
            "source",
            PREFIX_MESSAGE,
            OLD_AT
        )
        .await
        .expect("source delivery/replay fence"));
        let fail_publication = Arc::new(AtomicBool::new(false));
        let failure = Arc::clone(&fail_publication);
        let http = MockRest::with_responder(move |request| {
            if is_publish(request) {
                return if failure.load(Ordering::SeqCst) {
                    ScriptedResponse::status(403)
                } else {
                    ScriptedResponse::status(200)
                };
            }
            match (request.method.as_str(), request.path.as_str()) {
                ("POST", path) if path.ends_with("/callback") => ScriptedResponse::status(204),
                ("PATCH", path) if path.ends_with("/messages/@original") => {
                    ScriptedResponse::json(200, json!({"id": "9000"}))
                }
                ("POST", "/api/v10/channels/4444/messages") => {
                    ScriptedResponse::json(200, json!({"id": "9001"}))
                }
                _ => ScriptedResponse::status(403),
            }
        })
        .await;
        let executor =
            ActionExecutor::with_proxy("clone-disposable-fixture".to_owned(), Some(http.origin()))
                .expect("loopback-only shared executor");
        let runtime = runtime(db.pool().clone(), executor.clone(), true);
        Self {
            db,
            http,
            runtime,
            executor,
            fail_publication,
        }
    }

    async fn definitions(&self) -> Vec<Value> {
        sqlx::query_scalar("SELECT to_jsonb(c) FROM automation_commands c ORDER BY guild_id, name")
            .fetch_all(self.db.pool())
            .await
            .expect("full definitions including provenance")
    }

    async fn audits(&self) -> Vec<Value> {
        sqlx::query_scalar("SELECT to_jsonb(a) FROM automation_audit_log a ORDER BY id")
            .fetch_all(self.db.pool())
            .await
            .expect("all audit/history/fence columns")
    }

    async fn close(self) {
        drop(self.runtime);
        drop(self.executor);
        self.http.shutdown().await;
        self.db
            .close()
            .await
            .expect("drop only owned disposable DB");
    }
}

fn runtime(pool: PgPool, executor: ActionExecutor, enabled: bool) -> CustomCommandRuntime {
    let mut router = InteractionRouter::new(gates(enabled));
    CustomCommandRuntime::register(&mut router);
    CustomCommandRuntime::new(pool, Arc::new(router), executor, 1111)
}

async fn seed(pool: &PgPool, guild: &str, name: &str, enabled: bool, trigger: Option<&str>) {
    store::put_command(
        pool,
        &StoredCommand {
            guild_id: guild.to_owned(),
            name: name.to_owned(),
            description: DESCRIPTION.to_owned(),
            template: TEMPLATE.to_owned(),
            text_trigger: trigger.map(str::to_owned),
            enabled,
        },
        "6666",
        "7777",
        OLD_AT,
        "2026-10-02T00:00:00Z",
    )
    .await
    .expect("seed preexisting saved response");
}

fn is_publish(request: &RestRequest) -> bool {
    request.method == "PUT" && request.path == "/api/v10/applications/1111/guilds/2222/commands"
}

fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("wire JSON")
}

fn private_reply(requests: &[RestRequest], id: u64) -> String {
    let callback = format!("/interactions/{id}/clone-fixture-{id}/callback");
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.path.ends_with(&callback))
        .collect();
    assert_eq!(
        callbacks.len(),
        1,
        "one acknowledgement, not a fallback or retry"
    );
    assert_eq!(callbacks[0].method, "POST");
    let callback = body(callbacks[0]);
    assert!(callback["type"] == 4 || callback["type"] == 5);
    assert_eq!(
        callback["data"]["flags"].as_u64().expect("private flags") & 64,
        64
    );
    assert_safe_mentions(&callback["data"]);
    let content = if callback["type"] == 5 {
        let suffix = format!("/webhooks/1111/clone-fixture-{id}/messages/@original");
        let edits: Vec<_> = requests
            .iter()
            .filter(|r| r.path.ends_with(&suffix))
            .collect();
        assert_eq!(edits.len(), 1, "one bounded deferred completion");
        assert_eq!(edits[0].method, "PATCH");
        let edit = body(edits[0]);
        assert_safe_mentions(&edit);
        edit["content"]
            .as_str()
            .expect("completion content")
            .to_owned()
    } else {
        callback["data"]["content"]
            .as_str()
            .expect("refusal content")
            .to_owned()
    };
    assert!(!content.is_empty());
    assert!(content.encode_utf16().count() <= 2000);
    assert!(
        !requests
            .iter()
            .any(|r| r.method == "POST" && r.path.contains("/channels/")),
        "management must not send into a public channel"
    );
    content
}

fn assert_safe_mentions(data: &Value) {
    assert_eq!(data["allowed_mentions"]["parse"], json!([]));
    for key in ["users", "roles"] {
        assert!(
            data["allowed_mentions"][key].is_null() || data["allowed_mentions"][key] == json!([])
        );
    }
    assert_ne!(data["allowed_mentions"]["replied_user"], true);
}

fn definition<'a>(rows: &'a [Value], guild: &str, name: &str) -> &'a Value {
    rows.iter()
        .find(|row| row["guild_id"] == guild && row["name"] == name)
        .unwrap_or_else(|| panic!("missing admitted destination {guild}/{name}"))
}

fn assert_success(result: Result<bool, CustomCommandError>) {
    assert!(
        matches!(result, Ok(true)),
        "clone must be owned by actual management dispatch: {result:?}"
    );
}

fn assert_copy(rows: &[Value], before: &[Value], name: &str, enabled: bool) {
    assert_eq!(
        definition(rows, GUILD, "source"),
        definition(before, GUILD, "source"),
        "source must remain byte-equivalent, including all provenance"
    );
    let destination = definition(rows, GUILD, name);
    assert_eq!(destination["template"], TEMPLATE);
    assert_eq!(destination["description"], DESCRIPTION);
    assert_eq!(destination["enabled"], enabled);
    assert_eq!(
        destination["text_trigger"],
        Value::Null,
        "never copy a unique trigger"
    );
    assert_eq!(destination["created_by"], ACTOR);
    assert_eq!(destination["updated_by"], ACTOR);
    assert_eq!(destination["created_at"], destination["updated_at"]);
    assert_ne!(
        destination["created_at"],
        definition(before, GUILD, "source")["created_at"]
    );
    for previous in before {
        assert!(
            rows.contains(previous),
            "no preexisting row may be rewritten"
        );
    }
    assert_eq!(rows.len(), before.len() + 1);
}

fn assert_clone_audit(audits: &[Value], before: &[Value], destination: &str) {
    for previous in before {
        assert!(
            audits.contains(previous),
            "source history and replay fences must be immutable"
        );
    }
    let added: Vec<_> = audits.iter().filter(|row| !before.contains(row)).collect();
    assert_eq!(
        added.len(),
        1,
        "one mutation fact, no copied run or attempt history"
    );
    let fact = added[0];
    assert_eq!(fact["guild_id"], GUILD);
    assert_eq!(fact["actor_id"], ACTOR);
    assert_eq!(fact["target_key"], destination);
    assert_eq!(fact["outcome"], "ok");
    assert!(fact["action"] == "command.clone" || fact["action"] == "command.create");
    assert!(fact["reason"].is_null() || fact["reason"].as_str().is_some_and(|s| s.len() <= 64));
    let serialized = serde_json::to_string(fact).expect("audit JSON");
    for private in [
        TEMPLATE,
        DESCRIPTION,
        "!source",
        "Saved private response",
        "{username}",
    ] {
        assert!(!serialized.contains(private), "audit must be content-free");
    }
}

#[tokio::test]
async fn enabled_clone_copies_saved_fields_without_trigger_history_or_source_mutation() {
    let f = Fixture::new(true).await;
    let before = f.definitions().await;
    let history = f.audits().await;
    let result = f
        .runtime
        .handle_interaction(
            &clone_request(6001, "source", "copy"),
            Some("Fixture Guild"),
        )
        .await;
    let rows = f.definitions().await;
    let audits = f.audits().await;
    let requests = f.http.requests();
    f.close().await;
    assert_success(result);
    assert_copy(&rows, &before, "copy", true);
    assert_clone_audit(&audits, &history, "copy");
    let reply = private_reply(&requests, 6001).to_lowercase();
    assert!(reply.contains("saved") && reply.contains("copy"));
    assert!(reply.contains("enabled"), "explain the copied saved state");
    assert!(
        reply.contains("trigger") && (reply.contains("not copied") || reply.contains("omitted")),
        "explain why the unique trigger was not copied"
    );
    let publications: Vec<_> = requests.iter().filter(|r| is_publish(r)).collect();
    assert_eq!(publications.len(), 1);
    let published = body(publications[0]);
    let names: Vec<_> = published
        .as_array()
        .expect("full-set registry")
        .iter()
        .map(|d| d["name"].as_str().expect("published command name"))
        .collect();
    for required in [
        "help",
        "command",
        "command-inspect",
        "command-clone",
        "source",
        "occupied",
        "copy",
    ] {
        assert!(
            names.contains(&required),
            "full-set publication lost {required}"
        );
    }
    assert!(!names.contains(&"foreign-only"));
    assert_eq!(names.iter().filter(|name| **name == "copy").count(), 1);
}

#[tokio::test]
async fn disabled_clone_preserves_disabled_state_and_never_executes_or_enters_picker() {
    let f = Fixture::new(false).await;
    let before = f.definitions().await;
    let result = f
        .runtime
        .handle_interaction(
            &clone_request(6002, "source", "sleeping-copy"),
            Some("Fixture Guild"),
        )
        .await;
    let history_after_clone = f.audits().await;
    let synchronized = f.runtime.sync_registry().await;
    let slash = f
        .runtime
        .handle_interaction(
            &interaction(6003, "sleeping-copy", json!([])),
            Some("Fixture Guild"),
        )
        .await;
    let prefix = f
        .runtime
        .handle_message(
            &prefix_message(8002, "!sleeping-copy", "8888"),
            AutomationMessageAcceptance::AutomodDisabled,
            true,
            Some("Fixture Guild"),
        )
        .await;
    let prefix_source = f
        .runtime
        .handle_message(
            &prefix_message(8003, "!source", "8889"),
            AutomationMessageAcceptance::AutomodDisabled,
            true,
            Some("Fixture Guild"),
        )
        .await;
    let rows = f.definitions().await;
    let audits = f.audits().await;
    let requests = f.http.requests();
    f.close().await;
    assert_success(result);
    assert_copy(&rows, &before, "sleeping-copy", false);
    assert!(synchronized.is_ok());
    assert!(
        requests.iter().any(is_publish),
        "prove disabled omission against a real full-set PUT"
    );
    assert!(
        matches!(slash, Ok(false)),
        "disabled destination cannot execute via slash"
    );
    assert!(matches!(prefix, Ok(TextCommandOutcome::Ignored)));
    assert!(matches!(prefix_source, Ok(TextCommandOutcome::Ignored)));
    assert_eq!(
        audits, history_after_clone,
        "disabled execution must create no run/claim facts"
    );
    let reply = private_reply(&requests, 6002).to_lowercase();
    assert!(reply.contains("disabled") && reply.contains("trigger"));
    assert!(!requests
        .iter()
        .any(|r| r.path.contains("clone-fixture-6003")));
    for publication in requests.iter().filter(|r| is_publish(r)) {
        let value = body(publication);
        let names: Vec<_> = value
            .as_array()
            .expect("registry")
            .iter()
            .map(|d| d["name"].as_str())
            .collect();
        assert!(!names.contains(&Some("source")));
        assert!(!names.contains(&Some("sleeping-copy")));
    }
}

#[tokio::test]
async fn enabled_destination_runs_through_real_slash_but_does_not_acquire_a_text_trigger() {
    let f = Fixture::new(true).await;
    let cloned = f
        .runtime
        .handle_interaction(
            &clone_request(6004, "source", "copy"),
            Some("Fixture Guild"),
        )
        .await;
    let start = f.http.requests().len();
    let executed = f
        .runtime
        .handle_interaction(&interaction(6005, "copy", json!([])), Some("Fixture Guild"))
        .await;
    let execution_requests = f.http.requests()[start..].to_vec();
    let no_trigger = f
        .runtime
        .handle_message(
            &prefix_message(8004, "!copy ignored args", "8890"),
            AutomationMessageAcceptance::AutomodDisabled,
            true,
            Some("Fixture Guild"),
        )
        .await;
    let replay = f
        .runtime
        .handle_message(
            &prefix_message(PREFIX_MESSAGE, "!source", "7777"),
            AutomationMessageAcceptance::AutomodDisabled,
            true,
            Some("Fixture Guild"),
        )
        .await;
    let audits = f.audits().await;
    f.close().await;
    assert_success(cloned);
    assert_success(executed);
    let edit = execution_requests
        .iter()
        .find(|r| r.method == "PATCH")
        .expect("rendered slash completion");
    let value = body(edit);
    assert_eq!(
        value["content"],
        "Saved private response café 🦀 for tester in Fixture Guild / <#4444>"
    );
    assert_safe_mentions(&value);
    assert!(matches!(no_trigger, Ok(TextCommandOutcome::Ignored)));
    assert!(
        matches!(replay, Ok(TextCommandOutcome::AlreadyAttempted)),
        "cloning cannot reset source replay fences"
    );
    assert_eq!(
        audits
            .iter()
            .filter(|a| a["action"] == "command.run" && a["target_key"] == "copy")
            .count(),
        1
    );
    assert_eq!(
        audits
            .iter()
            .filter(|a| a["action"] == "command.text_attempt")
            .count(),
        1
    );
}

async fn refusal_case(source: &str, destination: &str, hint: &str) {
    let f = Fixture::new(true).await;
    let before = f.definitions().await;
    let history = f.audits().await;
    let result = f
        .runtime
        .handle_interaction(
            &clone_request(6100, source, destination),
            Some("Fixture Guild"),
        )
        .await;
    let rows = f.definitions().await;
    let audits = f.audits().await;
    let requests = f.http.requests();
    f.close().await;
    assert_success(result);
    assert_eq!(
        rows, before,
        "refusal must never overwrite or partly create"
    );
    for fact in history {
        assert!(audits.contains(&fact), "history must survive refusal");
    }
    assert!(
        !audits
            .iter()
            .any(|a| a["outcome"] == "ok" && a["actor_id"] == ACTOR),
        "a refused clone must not claim success"
    );
    assert!(
        !requests.iter().any(is_publish),
        "a refused clone must not publish"
    );
    let reply = private_reply(&requests, 6100).to_lowercase();
    assert!(
        reply.contains(hint),
        "actionable {hint} refusal, got {reply:?}"
    );
    assert!(!reply.contains(TEMPLATE));
    assert!(!reply.contains("saved /"));
}

macro_rules! refusal_test {
    ($name:ident, $source:expr, $destination:expr, $hint:expr) => {
        #[tokio::test]
        async fn $name() {
            refusal_case($source, $destination, $hint).await;
        }
    };
}

refusal_test!(
    missing_source_refuses_without_creating,
    "missing",
    "copy",
    "found"
);
refusal_test!(
    foreign_source_is_not_visible_or_copyable,
    "foreign-only",
    "copy",
    "found"
);
refusal_test!(same_name_is_not_an_update, "source", "source", "exist");
refusal_test!(
    normalized_same_name_is_not_an_update,
    "SOURCE",
    "source",
    "exist"
);
refusal_test!(
    existing_destination_is_never_overwritten,
    "source",
    "occupied",
    "exist"
);
refusal_test!(
    builtin_destination_is_reserved,
    "source",
    "ping",
    "reserved"
);
refusal_test!(
    voice_destination_is_reserved_even_when_voice_is_off,
    "source",
    "setup",
    "reserved"
);
refusal_test!(
    management_destination_is_reserved,
    "source",
    "command-clone",
    "reserved"
);
refusal_test!(invalid_source_name_refuses, "bad name", "copy", "name");
refusal_test!(
    invalid_destination_name_refuses,
    "source",
    "bad/name",
    "name"
);
refusal_test!(empty_source_name_refuses, "", "copy", "name");
refusal_test!(empty_destination_name_refuses, "source", "", "name");
refusal_test!(
    overlong_source_name_refuses,
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "copy",
    "name"
);
refusal_test!(
    overlong_destination_name_refuses,
    "source",
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "name"
);

#[tokio::test]
async fn mixed_case_names_normalize_and_boundary_length_is_admitted() {
    let f = Fixture::new(true).await;
    let before = f.definitions().await;
    let destination = "123456789012345678901234567890_-";
    assert_eq!(destination.len(), 32);
    let result = f
        .runtime
        .handle_interaction(
            &clone_request(6200, "SOURCE", &destination.to_uppercase()),
            Some("Fixture Guild"),
        )
        .await;
    let rows = f.definitions().await;
    f.close().await;
    assert_success(result);
    assert_copy(&rows, &before, destination, true);
}

#[tokio::test]
async fn forged_missing_duplicate_or_wrong_typed_options_never_select_a_partial_value() {
    let f = Fixture::new(true).await;
    let before = f.definitions().await;
    let mut observations = Vec::new();
    for (index, options) in [
        json!([]),
        json!([{"name":"source","type":3,"value":"source"}]),
        json!([{"name":"destination","type":3,"value":"copy"}]),
        json!([{"name":"source","type":4,"value":7},{"name":"destination","type":3,"value":"copy"}]),
        json!([{"name":"source","type":3,"value":"source"},{"name":"destination","type":4,"value":7}]),
        json!([{"name":"source","type":3,"value":"source"},{"name":"source","type":3,"value":"occupied"},{"name":"destination","type":3,"value":"copy"}]),
        json!([{"name":"source","type":3,"value":"source"},{"name":"destination","type":3,"value":"copy"},{"name":"destination","type":3,"value":"other"}]),
    ].into_iter().enumerate() {
        let id = 6300 + index as u64;
        let result = f.runtime.handle_interaction(&interaction(id, "command-clone", options), Some("Fixture Guild")).await;
        observations.push((id, result));
    }
    let rows = f.definitions().await;
    let requests = f.http.requests();
    f.close().await;
    for (id, result) in observations {
        assert_success(result);
        let reply = private_reply(&requests, id).to_lowercase();
        assert!(
            reply.contains("source")
                || reply.contains("destination")
                || reply.contains("option")
                || reply.contains("name")
        );
        assert!(!reply.contains("saved /"));
    }
    assert_eq!(rows, before);
    assert!(!requests.iter().any(is_publish));
}

#[tokio::test]
async fn a_saved_reserved_source_is_not_a_backdoor_around_clone_name_validation() {
    let f = Fixture::new(true).await;
    seed(f.db.pool(), GUILD, "setup", true, None).await;
    let before = f.definitions().await;
    let result = f
        .runtime
        .handle_interaction(&clone_request(6399, "setup", "copy"), Some("Fixture Guild"))
        .await;
    let rows = f.definitions().await;
    let requests = f.http.requests();
    f.close().await;
    assert_success(result);
    assert_eq!(rows, before);
    assert!(private_reply(&requests, 6399)
        .to_lowercase()
        .contains("reserved"));
    assert!(!requests.iter().any(is_publish));
}

#[tokio::test]
async fn invalid_saved_template_or_description_cannot_be_cloned() {
    for (template, description) in [
        ("{usre}".to_owned(), DESCRIPTION.to_owned()),
        (TEMPLATE.to_owned(), "😀".repeat(51)),
        (String::new(), DESCRIPTION.to_owned()),
        ("😀".repeat(1001), DESCRIPTION.to_owned()),
        (TEMPLATE.to_owned(), String::new()),
    ] {
        let f = Fixture::new(true).await;
        sqlx::query("UPDATE automation_commands SET template = $1, description = $2 WHERE guild_id = $3 AND name = 'source'")
            .bind(template).bind(description).bind(GUILD).execute(f.db.pool()).await.expect("legacy invalid fixture");
        let before = f.definitions().await;
        let result = f
            .runtime
            .handle_interaction(
                &clone_request(6400, "source", "copy"),
                Some("Fixture Guild"),
            )
            .await;
        let rows = f.definitions().await;
        let requests = f.http.requests();
        f.close().await;
        assert_success(result);
        assert_eq!(
            rows, before,
            "copy must revalidate saved content, not trust old writes"
        );
        let reply = private_reply(&requests, 6400).to_lowercase();
        assert!(
            reply.contains("template")
                || reply.contains("description")
                || reply.contains("placeholder")
        );
        assert!(!requests.iter().any(is_publish));
    }
}

#[tokio::test]
async fn permission_and_feature_refusals_precede_store_access_and_are_private() {
    let f = Fixture::new(true).await;
    // A missing table turns an accidental pre-refusal lookup into an error.
    sqlx::query("ALTER TABLE automation_commands RENAME TO clone_hidden_commands")
        .execute(f.db.pool())
        .await
        .expect("fixture-only lookup tripwire");
    let disabled = runtime(f.db.pool().clone(), f.executor.clone(), false);
    let mut denied = clone_request(6500, "source", "copy");
    denied.member.as_mut().expect("guild member").permissions =
        Some(twilight_model::guild::Permissions::empty());
    let unauthorized = f
        .runtime
        .handle_interaction(&denied, Some("Fixture Guild"))
        .await;
    let feature = disabled
        .handle_interaction(
            &clone_request(6501, "source", "copy"),
            Some("Fixture Guild"),
        )
        .await;
    let requests = f.http.requests();
    drop(disabled);
    f.close().await;
    assert_success(unauthorized);
    assert_success(feature);
    let denied_reply = private_reply(&requests, 6500).to_lowercase();
    assert!(denied_reply.contains("manage") || denied_reply.contains("permission"));
    let disabled_reply = private_reply(&requests, 6501).to_lowercase();
    assert!(disabled_reply.contains("disabled"));
    assert!(!requests.iter().any(is_publish));
}

#[tokio::test]
async fn foreign_guild_missing_guild_and_foreign_application_remain_zero_io() {
    let f = Fixture::new(true).await;
    let before = f.definitions().await;
    let history = f.audits().await;
    let mut outcomes = Vec::new();
    for mode in 0..3 {
        let mut request = clone_request(6510 + mode, "source", "copy");
        match mode {
            0 => request.guild_id = Some(twilight_model::id::Id::new(9999)),
            1 => request.guild_id = None,
            _ => request.application_id = twilight_model::id::Id::new(9999),
        }
        outcomes.push(
            f.runtime
                .handle_interaction(&request, Some("Fixture Guild"))
                .await,
        );
    }
    let rows = f.definitions().await;
    let audits = f.audits().await;
    let requests = f.http.requests();
    f.close().await;
    for outcome in outcomes {
        assert!(matches!(outcome, Ok(false)));
    }
    assert_eq!(rows, before);
    assert_eq!(audits, history);
    assert!(requests.is_empty());
}

#[tokio::test]
async fn audit_failure_rolls_back_destination_and_preserves_history() {
    let f = Fixture::new(true).await;
    let before = f.definitions().await;
    let history = f.audits().await;
    sqlx::raw_sql("CREATE FUNCTION clone_reject_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture audit refused'; END $$; CREATE TRIGGER clone_audit_failure BEFORE INSERT ON automation_audit_log FOR EACH ROW EXECUTE FUNCTION clone_reject_audit()")
        .execute(f.db.pool()).await.expect("fixture-only audit fault");
    let result = f
        .runtime
        .handle_interaction(
            &clone_request(6600, "source", "copy"),
            Some("Fixture Guild"),
        )
        .await;
    let rows = f.definitions().await;
    let audits = f.audits().await;
    let requests = f.http.requests();
    f.close().await;
    assert!(
        matches!(result, Err(CustomCommandError::Storage)),
        "audit failure must propagate, not silently ignore clone: {result:?}"
    );
    assert_eq!(
        rows, before,
        "destination INSERT and audit must share a transaction"
    );
    assert_eq!(audits, history);
    assert!(!requests.iter().any(is_publish));
    for request in requests.iter().filter(|r| r.method == "PATCH") {
        assert!(!body(request)["content"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("saved"));
    }
}

#[tokio::test]
async fn failed_destination_insert_cannot_leave_a_success_fact_or_publication() {
    let f = Fixture::new(true).await;
    let before = f.definitions().await;
    let history = f.audits().await;
    sqlx::raw_sql("CREATE FUNCTION clone_reject_insert() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.name = 'copy' THEN RAISE EXCEPTION 'fixture insert refused'; END IF; RETURN NEW; END $$; CREATE TRIGGER clone_insert_failure BEFORE INSERT ON automation_commands FOR EACH ROW EXECUTE FUNCTION clone_reject_insert()")
        .execute(f.db.pool()).await.expect("fixture-only mutation fault");
    let result = f
        .runtime
        .handle_interaction(
            &clone_request(6601, "source", "copy"),
            Some("Fixture Guild"),
        )
        .await;
    let rows = f.definitions().await;
    let audits = f.audits().await;
    let requests = f.http.requests();
    f.close().await;
    assert!(
        matches!(result, Err(CustomCommandError::Storage)),
        "failed INSERT must reach the real store: {result:?}"
    );
    assert_eq!(rows, before);
    assert_eq!(audits, history);
    assert!(!requests.iter().any(is_publish));
}

#[tokio::test]
async fn committed_clone_with_failed_publication_reports_saved_and_safe_recovery() {
    let f = Fixture::new(true).await;
    f.runtime
        .sync_registry()
        .await
        .expect("baseline confirmed publication");
    let confirmed: Vec<_> = f
        .runtime
        .published_commands()
        .expect("confirmed snapshot")
        .iter()
        .map(|d| d.name.clone())
        .collect();
    let before = f.definitions().await;
    let history = f.audits().await;
    let start = f.http.requests().len();
    f.fail_publication.store(true, Ordering::SeqCst);
    let result = f
        .runtime
        .handle_interaction(
            &clone_request(6700, "source", "copy"),
            Some("Fixture Guild"),
        )
        .await;
    let rows = f.definitions().await;
    let audits = f.audits().await;
    let failed_requests = f.http.requests()[start..].to_vec();
    let after_failure: Vec<_> = f
        .runtime
        .published_commands()
        .expect("old confirmed snapshot")
        .iter()
        .map(|d| d.name.clone())
        .collect();
    f.fail_publication.store(false, Ordering::SeqCst);
    let recovery = f.runtime.sync_registry().await;
    let recovered: Vec<_> = f
        .runtime
        .published_commands()
        .expect("recovered snapshot")
        .iter()
        .map(|d| d.name.clone())
        .collect();
    let after_recovery = f.audits().await;
    f.close().await;
    assert_success(result);
    assert_copy(&rows, &before, "copy", true);
    assert_clone_audit(&audits, &history, "copy");
    assert_eq!(
        after_failure, confirmed,
        "unconfirmed PUT cannot advance picker discovery"
    );
    let reply = private_reply(&failed_requests, 6700).to_lowercase();
    assert!(
        reply.contains("saved")
            && reply.contains("picker")
            && reply.contains("not")
            && reply.contains("synchronized"),
        "truthful partial outcome: {reply}"
    );
    assert!(
        reply.contains("sync")
            && (reply.contains("retry")
                || reply.contains("try")
                || reply.contains("ask")
                || reply.contains("restart")),
        "include a safe next action, not just a failure label: {reply}"
    );
    for lie in ["no change", "not saved", "rolled back", "try cloning again"] {
        assert!(!reply.contains(lie));
    }
    assert_eq!(
        failed_requests.iter().filter(|r| is_publish(r)).count(),
        1,
        "no automatic publication retry"
    );
    assert!(recovery.is_ok());
    assert!(recovered.contains(&"copy".to_owned()));
    assert_eq!(
        after_recovery, audits,
        "resync is not a second mutation or execution"
    );
}

async fn fill_to(f: &Fixture, count: usize) {
    let existing: i64 =
        sqlx::query_scalar("SELECT count(*) FROM automation_commands WHERE guild_id = $1")
            .bind(GUILD)
            .fetch_one(f.db.pool())
            .await
            .expect("guild count");
    for index in existing as usize..count {
        // Disabled definitions still reserve capacity.
        seed(f.db.pool(), GUILD, &format!("filler-{index}"), false, None).await;
    }
}

#[tokio::test]
async fn quota_includes_disabled_rows_and_refuses_without_publishing() {
    let f = Fixture::new(true).await;
    let limit = 100usize
        .checked_sub(reserved_command_names().len())
        .expect("registry leaves room");
    assert!(limit > 2);
    fill_to(&f, limit).await;
    let before = f.definitions().await;
    let result = f
        .runtime
        .handle_interaction(
            &clone_request(6800, "source", "copy"),
            Some("Fixture Guild"),
        )
        .await;
    let rows = f.definitions().await;
    let requests = f.http.requests();
    f.close().await;
    assert_success(result);
    assert_eq!(rows, before);
    let reply = private_reply(&requests, 6800).to_lowercase();
    assert!(reply.contains("limit") || reply.contains("capacity"));
    assert!(!requests.iter().any(is_publish));
}

async fn race(final_slot: bool) {
    let f = Fixture::new(true).await;
    let limit = 100usize
        .checked_sub(reserved_command_names().len())
        .expect("registry capacity");
    if final_slot {
        fill_to(&f, limit - 1).await;
    }
    let before = f.definitions().await;
    let history = f.audits().await;
    let peer_pool =
        f.db.independent_pool()
            .await
            .expect("independent worker pool");
    let peer = runtime(peer_pool.clone(), f.executor.clone(), true);
    // Hold the actual guild lock until both public calls have reached it. This
    // is a contended test, not two sequential calls labeled as concurrent.
    let mut held = f.db.pool().begin().await.expect("guild lock holder");
    store::lock_command_capacity(&mut held, GUILD)
        .await
        .expect("hold guild serialization");
    let left = clone_request(6900, "source", "copy-a");
    let right = clone_request(6901, "source", if final_slot { "copy-b" } else { "copy-a" });
    let (results, contended) = tokio::join!(
        async {
            tokio::join!(
                f.runtime.handle_interaction(&left, Some("Fixture Guild")),
                peer.handle_interaction(&right, Some("Fixture Guild"))
            )
        },
        async {
            let contended = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let waiters: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted AND database = (SELECT oid FROM pg_database WHERE datname = current_database())")
                        .fetch_one(&peer_pool).await.expect("observe disposable guild-lock waiters");
                    if waiters >= 2 { break; }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.is_ok();
            held.rollback()
                .await
                .expect("release fixture-held guild lock");
            contended
        }
    );
    let rows = f.definitions().await;
    let audits = f.audits().await;
    let requests = f.http.requests();
    drop(peer);
    peer_pool.close().await;
    f.close().await;
    assert!(
        contended,
        "both clone calls must reach existing transaction/guild serialization"
    );
    assert_success(results.0);
    assert_success(results.1);
    let created: Vec<_> = rows
        .iter()
        .filter(|row| {
            row["guild_id"] == GUILD && (row["name"] == "copy-a" || row["name"] == "copy-b")
        })
        .collect();
    assert_eq!(
        created.len(),
        1,
        "exactly one contender admitted, never overwrite or quota overshoot"
    );
    assert_copy(
        &rows,
        &before,
        created[0]["name"].as_str().expect("destination"),
        true,
    );
    let success_facts: Vec<_> = audits
        .iter()
        .filter(|a| !history.contains(a) && a["outcome"] == "ok")
        .collect();
    assert_eq!(
        success_facts.len(),
        1,
        "loser must not be an upsert success"
    );
    let replies = [
        private_reply(&requests, 6900),
        private_reply(&requests, 6901),
    ];
    assert_eq!(
        replies
            .iter()
            .filter(|r| r.to_lowercase().contains("saved"))
            .count(),
        1
    );
    let published: Vec<_> = requests.iter().filter(|r| is_publish(r)).collect();
    assert_eq!(published.len(), 1, "only the admitted write republishes");
}

#[tokio::test]
async fn independent_workers_cloning_to_the_same_name_admit_one_create_only() {
    race(false).await;
}

#[tokio::test]
async fn independent_workers_competing_for_the_final_slot_admit_one_destination() {
    race(true).await;
}

#[tokio::test]
async fn shared_runtime_full_set_publication_never_loses_an_admitted_clone() {
    let f = Fixture::new(true).await;
    let shared = f.runtime.clone();
    let left = clone_request(7000, "source", "copy-a");
    let right = clone_request(7001, "occupied", "copy-b");
    let results = tokio::join!(
        f.runtime.handle_interaction(&left, Some("Fixture Guild")),
        shared.handle_interaction(&right, Some("Fixture Guild"))
    );
    let rows = f.definitions().await;
    let requests = f.http.requests();
    let snapshot = f
        .runtime
        .published_commands()
        .map(|defs| defs.iter().map(|d| d.name.clone()).collect::<Vec<_>>());
    drop(shared);
    f.close().await;
    assert_success(results.0);
    assert_success(results.1);
    assert_eq!(
        rows.iter()
            .filter(|r| r["guild_id"] == GUILD && (r["name"] == "copy-a" || r["name"] == "copy-b"))
            .count(),
        2
    );
    let publications: Vec<_> = requests.iter().filter(|r| is_publish(r)).collect();
    assert_eq!(publications.len(), 2);
    let last = body(publications[1]);
    let names: Vec<_> = last
        .as_array()
        .expect("last full set")
        .iter()
        .map(|d| d["name"].as_str())
        .collect();
    for name in [
        "help",
        "command-clone",
        "source",
        "occupied",
        "copy-a",
        "copy-b",
    ] {
        assert!(names.contains(&Some(name)), "latest full set lost {name}");
    }
    let snapshot = snapshot.expect("confirmed published discovery");
    assert!(snapshot.contains(&"copy-a".to_owned()) && snapshot.contains(&"copy-b".to_owned()));
}

#[tokio::test]
async fn store_insert_collision_after_validation_cannot_fall_back_to_overwriting_put() {
    let f = Fixture::new(true).await;
    let source_before = f.definitions().await;
    let history = f.audits().await;
    let peer =
        f.db.independent_pool()
            .await
            .expect("independent unsynchronized writer");
    // Pause only the cloning actor's INSERT after its absent-name check. The
    // independent legacy writer does not take the guild lock and wins the
    // unique key while that INSERT waits. An upsert would overwrite its row.
    sqlx::raw_sql("CREATE FUNCTION clone_insert_gate() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(74213, 1); RETURN NEW; END $$; CREATE TRIGGER clone_insert_gate BEFORE INSERT ON automation_commands FOR EACH ROW WHEN (NEW.name = 'copy' AND NEW.created_by = '3333') EXECUTE FUNCTION clone_insert_gate()")
        .execute(f.db.pool()).await.expect("fixture-only post-validation INSERT gate");
    let mut held = peer.begin().await.expect("hold insert gate");
    sqlx::query("SELECT pg_advisory_xact_lock(74213, 1)")
        .execute(&mut *held)
        .await
        .expect("held gate");
    let input = clone_request(7050, "source", "copy");
    let (result, (contended, rival)) = tokio::join!(
        f.runtime.handle_interaction(&input, Some("Fixture Guild")),
        async {
            let contended = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let waiters: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted AND database = (SELECT oid FROM pg_database WHERE datname = current_database())")
                        .fetch_one(&peer).await.expect("observe post-validation INSERT wait");
                    if waiters >= 1 { break; }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.is_ok();
            seed(&peer, GUILD, "copy", false, Some("!rival")).await;
            let rival: Value = sqlx::query_scalar("SELECT to_jsonb(c) FROM automation_commands c WHERE guild_id = $1 AND name = 'copy'")
                .bind(GUILD).fetch_one(&peer).await.expect("winner's complete row");
            held.rollback().await.expect("release INSERT gate");
            (contended, rival)
        }
    );
    let rows = f.definitions().await;
    let audits = f.audits().await;
    let requests = f.http.requests();
    peer.close().await;
    f.close().await;
    assert!(
        contended,
        "clone must reach the actual INSERT, not silently remain unknown"
    );
    assert!(
        matches!(result, Err(CustomCommandError::Storage) | Ok(true)),
        "bounded create conflict: {result:?}"
    );
    assert_eq!(
        definition(&rows, GUILD, "copy"),
        &rival,
        "create-only store operation must preserve every competing destination field"
    );
    assert_eq!(
        definition(&rows, GUILD, "source"),
        definition(&source_before, GUILD, "source")
    );
    for fact in &history {
        assert!(audits.contains(fact));
    }
    assert!(!audits
        .iter()
        .any(|a| !history.contains(a) && a["outcome"] == "ok"));
    assert!(!requests.iter().any(is_publish));
    assert!(!private_reply(&requests, 7050)
        .to_lowercase()
        .contains("saved"));
}

// Unchanged public behaviors are controls: a missing clone handler cannot
// satisfy the acceptance cases merely by making every management call fail.
#[tokio::test]
async fn control_existing_put_and_remove_still_save_publish_and_preserve_creator() {
    let f = Fixture::new(true).await;
    let put = interaction(
        7100,
        "command",
        json!([
            {"name":"name","type":3,"value":"source"},
            {"name":"template","type":3,"value":"Updated {username}"},
            {"name":"description","type":3,"value":"Updated description"}
        ]),
    );
    let saved = f
        .runtime
        .handle_interaction(&put, Some("Fixture Guild"))
        .await;
    let updated = f.definitions().await;
    let removed = f
        .runtime
        .handle_interaction(
            &interaction(
                7101,
                "command-remove",
                json!([
                    {"name":"name","type":3,"value":"source"}
                ]),
            ),
            Some("Fixture Guild"),
        )
        .await;
    let rows = f.definitions().await;
    let requests = f.http.requests();
    f.close().await;
    assert!(matches!(saved, Ok(true)));
    assert!(matches!(removed, Ok(true)));
    let source = definition(&updated, GUILD, "source");
    assert_eq!(source["template"], "Updated {username}");
    assert_eq!(source["created_by"], "6666");
    assert_eq!(source["updated_by"], ACTOR);
    assert!(!rows
        .iter()
        .any(|r| r["guild_id"] == GUILD && r["name"] == "source"));
    assert!(private_reply(&requests, 7100).starts_with("Saved /source."));
    assert!(private_reply(&requests, 7101).starts_with("Removed /source."));
    assert_eq!(requests.iter().filter(|r| is_publish(r)).count(), 2);
}

#[tokio::test]
async fn control_existing_management_permission_refusal_is_private_and_non_mutating() {
    let f = Fixture::new(true).await;
    let before = f.definitions().await;
    let history = f.audits().await;
    let mut request = interaction(
        7102,
        "command-remove",
        json!([{"name":"name","type":3,"value":"source"}]),
    );
    request.member.as_mut().expect("member").permissions =
        Some(twilight_model::guild::Permissions::empty());
    let result = f
        .runtime
        .handle_interaction(&request, Some("Fixture Guild"))
        .await;
    let rows = f.definitions().await;
    let audits = f.audits().await;
    let requests = f.http.requests();
    f.close().await;
    assert!(matches!(result, Ok(true)));
    assert_eq!(rows, before);
    assert_eq!(audits, history);
    let reply = private_reply(&requests, 7102).to_lowercase();
    assert!(reply.contains("manage") || reply.contains("permission"));
    assert!(!requests.iter().any(is_publish));
}
