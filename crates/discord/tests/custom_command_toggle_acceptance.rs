#![cfg(feature = "db")]

//! Protected behavioral acceptance for pausing/resuming a saved response.
//!
//! Public contract: /command-disable name:<name> and /command-enable name:<name>
//! are guild-only, Manage Server, automation-gated management operations. A
//! successful transition changes only enabled and update/audit metadata. A
//! repeated state is an explicit no-op, not an implicit registry repair. Failed
//! publication is a committed save, not a failed validation or a confirmed sync.
//!
//! The real router, service, Postgres store and executor run here; only Discord
//! HTTP is doubled. All DB fixtures use the guarded disposable test service.
//! No product adapter, schema change, ignored test or implementation is supplied.

#[allow(dead_code)]
mod common;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{postgres::PgConnectOptions, postgres::PgPoolOptions, PgPool};
use twilight_model::{application::interaction::Interaction, channel::Message};
use two_bot_core::{
    automation_transfer as transfer,
    command_permissions::{command_permission, CommandSurface},
    commands::{CustomCommand, RegistryError},
    custom_command_service as service, custom_command_store as store,
    custom_commands::{
        builtin_command_names, reserved_command_names, AutomationMessageAcceptance, CommandError,
        PutCommandInput, StoredCommand,
    },
    HandlerId, InteractionRouter, RouterGates, SlashContext, SlashOutcome,
    AUTOMATIONS_DISABLED_REPLY, MANAGE_SERVER_REQUIRED,
};
use two_bot_discord::{
    custom_commands::{CustomCommandError, CustomCommandRuntime, TextCommandOutcome},
    interactions::publish_commands,
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

const ENABLE: &str = "command-enable";
const DISABLE: &str = "command-disable";
const TEMPLATE: &str = "Saved {user} {username} in {server} {channel}";
const AT: &str = "2026-10-01T00:00:00Z";

fn gates(automations: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        automations,
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

fn runtime(pool: PgPool, executor: ActionExecutor, automations: bool) -> CustomCommandRuntime {
    let mut router = InteractionRouter::new(gates(automations));
    CustomCommandRuntime::register(&mut router);
    CustomCommandRuntime::new(pool, Arc::new(router), executor, 1111)
}

fn option(name: &str, value: &str) -> Value {
    json!({"name": name, "type": 3, "value": value})
}

fn interaction(id: u64, command: &str, permissions: u64, options: Vec<Value>) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": format!("toggle-fixture-{id}"), "version": 1,
        "guild_id": "2222", "channel": {"id": "4444", "type": 0, "name": "commands"},
        "authorizing_integration_owners": {"0": "2222"}, "entitlements": [],
        "member": {
            "permissions": permissions.to_string(), "roles": [], "joined_at": null,
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null}
        },
        "data": {"id": "5555", "name": command, "type": 1, "options": options}
    }))
    .expect("Twilight guild interaction fixture")
}

fn toggle(id: u64, enabled: bool, name: &str) -> Interaction {
    interaction(
        id,
        if enabled { ENABLE } else { DISABLE },
        32,
        vec![option("name", name)],
    )
}

fn prefix(id: u64, actor: u64) -> Message {
    serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": "2222", "channel_id": "4444", "type": 0,
        "author": {"id": actor.to_string(), "username": "tester", "discriminator": "0000", "avatar": null},
        "content": "!faq ordinary arguments", "timestamp": "2026-10-24T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false,
        "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false
    }))
    .expect("human prefix fixture")
}

#[derive(Default)]
struct WireFailures {
    publish: AtomicBool,
    ack: AtomicBool,
}

fn reply_to(request: &RestRequest, failures: &WireFailures) -> ScriptedResponse {
    match request.method.as_str() {
        "POST" if request.path.ends_with("/callback") => {
            ScriptedResponse::status(if failures.ack.load(Ordering::SeqCst) {
                403
            } else {
                204
            })
        }
        "PATCH" if request.path.ends_with("/messages/@original") => {
            ScriptedResponse::json(200, json!({"id": "9000"}))
        }
        "POST" if request.path.ends_with("/channels/4444/messages") => {
            ScriptedResponse::json(200, json!({"id": "9001"}))
        }
        "PUT" if request.path == "/api/v10/applications/1111/guilds/2222/commands" => {
            if failures.publish.load(Ordering::SeqCst) {
                return ScriptedResponse::status(403);
            }
            let mut receipt: Value =
                serde_json::from_slice(&request.body).expect("publication JSON");
            for (index, row) in receipt
                .as_array_mut()
                .expect("full-set publication")
                .iter_mut()
                .enumerate()
            {
                row["id"] = json!((8000 + index).to_string());
                row["application_id"] = json!("1111");
                row["version"] = json!("1");
                row["type"] = json!(1);
            }
            ScriptedResponse::json(200, receipt)
        }
        _ => ScriptedResponse::status(403),
    }
}

struct Fixture {
    db: TestDatabase,
    mock: MockRest,
    executor: ActionExecutor,
    runtime: CustomCommandRuntime,
    failures: Arc<WireFailures>,
}

impl Fixture {
    async fn new(enabled: bool) -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit disposable test bootstrap required");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("isolated migrated fixture, never application credentials");
        seed(db.pool(), "2222", "faq", "!faq", TEMPLATE, enabled).await;
        seed(
            db.pool(),
            "2222",
            "other",
            "!other",
            "Other saved response",
            true,
        )
        .await;
        seed(
            db.pool(),
            "9999",
            "faq",
            "!faq",
            "Foreign private definition",
            true,
        )
        .await;
        let failures = Arc::new(WireFailures::default());
        let policy = Arc::clone(&failures);
        let mock = MockRest::with_responder(move |request| reply_to(request, &policy)).await;
        let executor = ActionExecutor::with_proxy("toggle-fixture".to_owned(), Some(mock.origin()))
            .expect("loopback-only executor");
        let runtime = runtime(db.pool().clone(), executor.clone(), true);
        Self {
            db,
            mock,
            executor,
            runtime,
            failures,
        }
    }

    async fn observe(&self, input: &Interaction) -> Observed {
        observe(&self.runtime, &self.mock, input).await
    }

    async fn text(&self, id: u64, actor: u64) -> Result<TextCommandOutcome, CustomCommandError> {
        self.runtime
            .handle_message(
                &prefix(id, actor),
                AutomationMessageAcceptance::Unmatched,
                true,
                Some("Test guild"),
            )
            .await
    }

    async fn close(self) {
        drop(self.runtime);
        drop(self.executor);
        self.mock.shutdown().await;
        self.db
            .close()
            .await
            .expect("drop only the owned test database");
    }
}

async fn seed(
    pool: &PgPool,
    guild: &str,
    name: &str,
    trigger: &str,
    template: &str,
    enabled: bool,
) {
    store::put_command(
        pool,
        &StoredCommand {
            guild_id: guild.to_owned(),
            name: name.to_owned(),
            description: "Saved description".to_owned(),
            template: template.to_owned(),
            text_trigger: Some(trigger.to_owned()),
            enabled,
        },
        "66",
        "67",
        AT,
        AT,
    )
    .await
    .expect("seed through the real store");
}

async fn definitions(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(c) FROM automation_commands c ORDER BY guild_id, name")
        .fetch_all(pool)
        .await
        .expect("all fields, including creator and timestamps")
}

async fn facts(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(a) FROM automation_audit_log a ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("all immutable audit facts")
}

fn row(rows: &[Value]) -> &Value {
    rows.iter()
        .find(|row| row["guild_id"] == "2222" && row["name"] == "faq")
        .expect("paused is saved, not deleted")
}

fn immutable_fields(rows: &[Value]) -> Vec<Value> {
    let mut copy = rows.to_vec();
    for value in &mut copy {
        if value["guild_id"] == "2222" && value["name"] == "faq" {
            let fields = value.as_object_mut().expect("row");
            for field in ["enabled", "updated_by", "updated_at"] {
                fields.remove(field);
            }
        }
    }
    copy
}

struct Observed {
    result: Result<bool, CustomCommandError>,
    requests: Vec<RestRequest>,
    id: u64,
}

async fn observe(runtime: &CustomCommandRuntime, mock: &MockRest, input: &Interaction) -> Observed {
    let start = mock.requests().len();
    let result = runtime.handle_interaction(input, Some("Test guild")).await;
    Observed {
        result,
        requests: mock.requests()[start..].to_vec(),
        id: input.id.get(),
    }
}

fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("captured wire JSON")
}

fn no_mentions(value: &Value) {
    assert_eq!(value["allowed_mentions"]["parse"], json!([]));
    for kind in ["users", "roles"] {
        assert!(
            value["allowed_mentions"][kind].is_null()
                || value["allowed_mentions"][kind] == json!([])
        );
    }
    assert_ne!(value["allowed_mentions"]["replied_user"], true);
}

fn private_reply(observed: &Observed) -> String {
    assert!(
        matches!(observed.result, Ok(true)),
        "management must own the request: {:?}",
        observed.result
    );
    let callback = format!(
        "/interactions/{}/toggle-fixture-{}/callback",
        observed.id, observed.id
    );
    let edit = format!(
        "/webhooks/1111/toggle-fixture-{}/messages/@original",
        observed.id
    );
    let mut replies = Vec::new();
    for request in &observed.requests {
        if request.method == "PUT" {
            assert_eq!(
                request.path, "/api/v10/applications/1111/guilds/2222/commands",
                "only full guild replacement is permitted"
            );
        } else {
            assert!(
                (request.method == "POST" && request.path.ends_with(&callback))
                    || (request.method == "PATCH" && request.path.ends_with(&edit)),
                "no channel send, delete, DM or second owner: {request:?}"
            );
            replies.push(request);
        }
    }
    let first = replies.first().expect("one private acknowledgement");
    assert_eq!(first.method, "POST");
    let initial = body(first);
    assert_eq!(
        initial["data"]["flags"].as_u64().unwrap_or(0) & 64,
        64,
        "management is private"
    );
    no_mentions(&initial["data"]);
    let content = if initial["type"] == 5 {
        assert_eq!(replies.len(), 2, "defer then exactly one completion");
        assert_eq!(replies[1].method, "PATCH");
        let completion = body(replies[1]);
        no_mentions(&completion);
        completion["content"]
            .as_str()
            .expect("completion text")
            .to_owned()
    } else {
        assert_eq!(initial["type"], 4);
        assert_eq!(replies.len(), 1);
        initial["data"]["content"]
            .as_str()
            .expect("refusal text")
            .to_owned()
    };
    assert!(!content.is_empty());
    assert!(content.encode_utf16().count() <= 2000);
    content
}

fn publications(requests: &[RestRequest]) -> Vec<Value> {
    requests
        .iter()
        .filter(|r| r.method == "PUT")
        .map(body)
        .collect()
}

fn published_names(set: &Value) -> Vec<&str> {
    set.as_array()
        .expect("whole registry")
        .iter()
        .map(|r| r["name"].as_str().expect("command name"))
        .collect()
}

fn assert_full_set(set: &Value, enabled: bool) {
    let names = published_names(set);
    assert_eq!(
        names.iter().filter(|&&name| name == "faq").count(),
        usize::from(enabled)
    );
    assert_eq!(names.iter().filter(|&&name| name == "other").count(), 1);
    for builtin in InteractionRouter::new(gates(true))
        .publish_set(&[])
        .expect("gated builtins")
    {
        assert_eq!(
            names.iter().filter(|&&name| name == builtin.name).count(),
            1,
            "retain builtin /{}",
            builtin.name
        );
    }
    assert!(!names.contains(&"foreign"));
    assert!(names.len() <= 100);
}

fn changed_reply(observed: &Observed, enabled: bool) {
    changed_named_reply(observed, enabled, "faq");
}

fn changed_named_reply(observed: &Observed, enabled: bool, name: &str) {
    let text = private_reply(observed).to_ascii_lowercase();
    assert!(text.contains(name), "saved command name: {text}");
    assert!(
        text.contains(if enabled { "enabled" } else { "disabled" }),
        "name saved state: {text}"
    );
    assert!(
        !text.contains("removed") && !text.contains("deleted"),
        "pausing is not removal: {text}"
    );
}

fn unchanged_reply(observed: &Observed, enabled: bool) {
    changed_reply(observed, enabled);
    let text = private_reply(observed).to_ascii_lowercase();
    assert!(
        text.contains("already") || text.contains("unchanged") || text.contains("no change"),
        "explicit idempotent no-op: {text}"
    );
    assert!(
        publications(&observed.requests).is_empty(),
        "no-op must not republish"
    );
}

// Missing reservation or permissions would let a stored custom row impersonate
// management. Missing picker bounds would expose a different input contract.
#[test]
fn toggle_registry_is_bounded_guild_only_manage_server_and_reserved() {
    let defs = InteractionRouter::new(gates(true))
        .publish_set(&[])
        .expect("registry");
    let wire = serde_json::to_value(publish_commands(&defs)).expect("wire commands");
    for name in [DISABLE, ENABLE] {
        let definition = defs
            .iter()
            .find(|d| d.name == name)
            .expect("publish both lifecycle commands");
        assert!(!definition.dm_permission);
        let value = wire
            .as_array()
            .expect("commands")
            .iter()
            .find(|d| d["name"] == name)
            .expect("wire definition");
        assert_eq!(value["default_member_permissions"], "32");
        let options = value["options"].as_array().expect("name option");
        assert_eq!(
            options.len(),
            1,
            "this is not a replacement/editor workflow"
        );
        assert_eq!(options[0]["name"], "name");
        assert_eq!(options[0]["type"], 3);
        assert_eq!(options[0]["required"], true);
        assert_eq!(options[0]["max_length"], 32);
        let permission = command_permission(name).expect("runtime permission row");
        assert_eq!(permission.required_permissions, 32);
        assert_eq!(permission.surface, CommandSurface::BuiltinSlash);
        assert!(builtin_command_names().contains(name));
        assert!(reserved_command_names().contains(name));
        assert!(!InteractionRouter::new(gates(false))
            .publish_set(&[])
            .expect("gated registry")
            .iter()
            .any(|d| d.name == name));
    }
}

#[tokio::test]
async fn toggle_refuses_authority_and_feature_gates_before_store_access() {
    let pool = PgPoolOptions::new().connect_lazy_with(
        PgConnectOptions::new()
            .host("agent-testdb")
            .username("agent_test")
            .password(""),
    );
    pool.close().await;
    let mock = MockRest::with_responder(|r| reply_to(r, &WireFailures::default())).await;
    let executor = ActionExecutor::with_proxy("toggle-fixture".to_owned(), Some(mock.origin()))
        .expect("loopback executor");
    for (index, command) in [DISABLE, ENABLE].iter().enumerate() {
        for (enabled, permissions, expected) in [
            (true, 0, MANAGE_SERVER_REQUIRED),
            (false, 32, AUTOMATIONS_DISABLED_REPLY),
        ] {
            let service = runtime(pool.clone(), executor.clone(), enabled);
            let input = interaction(
                100 + index as u64,
                command,
                permissions,
                vec![option("name", "faq")],
            );
            let observed = observe(&service, &mock, &input).await;
            assert_eq!(
                private_reply(&observed),
                expected,
                "permission/gate precedes name validation, SQL and publication"
            );
            assert!(publications(&observed.requests).is_empty());
        }
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn toggle_foreign_application_guild_and_dm_stay_silent_before_store() {
    let pool = PgPoolOptions::new().connect_lazy_with(
        PgConnectOptions::new()
            .host("agent-testdb")
            .username("agent_test")
            .password(""),
    );
    pool.close().await;
    let mock = MockRest::with_responder(|r| reply_to(r, &WireFailures::default())).await;
    let executor = ActionExecutor::with_proxy("toggle-fixture".to_owned(), Some(mock.origin()))
        .expect("loopback executor");
    let service = runtime(pool, executor, true);
    for command in [DISABLE, ENABLE] {
        for scope in ["application", "guild", "dm"] {
            let mut input = interaction(110, command, 32, vec![option("name", "faq")]);
            match scope {
                "application" => input.application_id = twilight_model::id::Id::new(9999),
                "guild" => input.guild_id = Some(twilight_model::id::Id::new(9999)),
                _ => input.guild_id = None,
            }
            let observed = observe(&service, &mock, &input).await;
            assert!(matches!(observed.result, Ok(false)));
            assert!(
                observed.requests.is_empty(),
                "foreign requests produce no REST"
            );
        }
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn toggle_round_trip_retains_definition_and_replay_facts_and_restores_both_surfaces() {
    let f = Fixture::new(true).await;
    let before = definitions(f.db.pool()).await;
    let prefix_before = f.text(201, 3333).await;
    let slash_before = f.observe(&interaction(202, "faq", 0, vec![])).await;
    let run_facts = facts(f.db.pool()).await;
    let disabled = f.observe(&toggle(203, false, "FAQ")).await;
    let paused = definitions(f.db.pool()).await;
    let facts_after_disable = facts(f.db.pool()).await;
    let prefix_paused = f.text(204, 3334).await;
    let slash_paused = f.observe(&interaction(205, "faq", 0, vec![])).await;
    let inspect = f
        .observe(&interaction(
            206,
            "command-inspect",
            32,
            vec![option("name", "faq")],
        ))
        .await;
    let list = f
        .observe(&interaction(207, "command-list", 32, vec![]))
        .await;
    let facts_after_reads = facts(f.db.pool()).await;
    let enabled = f.observe(&toggle(208, true, "faq")).await;
    let resumed = definitions(f.db.pool()).await;
    let facts_after_enable = facts(f.db.pool()).await;
    let prefix_after = f.text(209, 3335).await;
    let slash_after = f.observe(&interaction(210, "faq", 0, vec![])).await;
    // A fresh runtime forgets process-local cooldowns, not durable attempts.
    let restarted = runtime(f.db.pool().clone(), f.executor.clone(), true);
    let replay = restarted
        .handle_message(
            &prefix(201, 3333),
            AutomationMessageAcceptance::Unmatched,
            true,
            Some("Test guild"),
        )
        .await;
    drop(restarted);
    let final_facts = facts(f.db.pool()).await;
    let requests = f.mock.requests();
    f.close().await;

    assert!(matches!(prefix_before, Ok(TextCommandOutcome::Delivered)));
    assert!(matches!(slash_before.result, Ok(true)));
    changed_reply(&disabled, false);
    assert_eq!(row(&paused)["enabled"], false);
    assert_eq!(row(&paused)["updated_by"], "3333");
    assert_ne!(row(&paused)["updated_at"], row(&before)["updated_at"]);
    assert_eq!(immutable_fields(&paused), immutable_fields(&before));
    assert!(matches!(prefix_paused, Ok(TextCommandOutcome::Ignored)));
    assert!(
        matches!(slash_paused.result, Ok(false)) && slash_paused.requests.is_empty(),
        "stale picker invocation cannot execute a paused response"
    );
    assert!(private_reply(&inspect).contains("Status: Disabled"));
    assert!(private_reply(&list).contains("/faq (or !faq) — off"));
    assert_eq!(
        facts_after_disable, facts_after_reads,
        "paused invocations and discovery create no attempt/run facts"
    );
    for fact in &run_facts {
        assert!(
            final_facts.contains(fact),
            "toggle cannot reset audit/replay history"
        );
    }
    for (old, new) in [
        (&run_facts, &facts_after_disable),
        (&facts_after_reads, &facts_after_enable),
    ] {
        let added: Vec<_> = new.iter().filter(|fact| !old.contains(fact)).collect();
        assert_eq!(
            added.len(),
            1,
            "one required mutation fact per actual transition"
        );
        assert_eq!(added[0]["guild_id"], "2222");
        assert_eq!(added[0]["actor_id"], "3333");
        assert_eq!(added[0]["target_key"], "faq");
        assert_eq!(added[0]["outcome"], "ok");
        assert_ne!(added[0]["action"], "command.run");
        assert_ne!(added[0]["action"], "command.text_attempt");
        assert!(
            !added[0].to_string().contains("Saved {user}"),
            "audit never stores template content"
        );
    }
    changed_reply(&enabled, true);
    assert_eq!(row(&resumed)["enabled"], true);
    assert_eq!(immutable_fields(&resumed), immutable_fields(&before));
    assert!(matches!(prefix_after, Ok(TextCommandOutcome::Delivered)));
    assert!(matches!(slash_after.result, Ok(true)));
    assert!(matches!(replay, Ok(TextCommandOutcome::AlreadyAttempted)));
    let sets = publications(&requests);
    assert_eq!(sets.len(), 2, "one full-set publish per actual transition");
    assert_full_set(&sets[0], false);
    assert_full_set(&sets[1], true);
    let posts: Vec<_> = requests
        .iter()
        .filter(|r| r.path.ends_with("/channels/4444/messages"))
        .collect();
    assert_eq!(posts.len(), 2, "no paused/replayed prefix send");
    assert_eq!(
        body(posts[0])["content"],
        "Saved <@3333> tester in Test guild <#4444>"
    );
    assert_eq!(
        body(posts[1])["content"],
        "Saved <@3335> tester in Test guild <#4444>"
    );
    for post in posts {
        no_mentions(&body(post));
    }
    let slash_edit = slash_after
        .requests
        .iter()
        .find(|r| r.method == "PATCH")
        .expect("resumed slash completes");
    assert_eq!(
        body(slash_edit)["content"],
        "Saved <@3333> tester in Test guild <#4444>"
    );
    no_mentions(&body(slash_edit));
}

#[tokio::test]
async fn toggle_repeating_current_state_is_explicit_and_does_not_rewrite_or_publish() {
    for enabled in [false, true] {
        let f = Fixture::new(enabled).await;
        let before = definitions(f.db.pool()).await;
        let one = f.observe(&toggle(301, enabled, "faq")).await;
        let two = f.observe(&toggle(302, enabled, "FAQ")).await;
        let after = definitions(f.db.pool()).await;
        let audit = facts(f.db.pool()).await;
        f.close().await;
        assert_eq!(
            before, after,
            "no-op preserves update timestamp and attribution too"
        );
        unchanged_reply(&one, enabled);
        unchanged_reply(&two, enabled);
        assert!(!audit
            .iter()
            .any(|a| a["action"] == "command.run" || a["action"] == "command.text_attempt"));
    }
}

#[tokio::test]
async fn toggle_invalid_missing_and_forged_options_never_reactivate_or_create_a_definition() {
    let f = Fixture::new(false).await;
    let before = definitions(f.db.pool()).await;
    let audit_before = facts(f.db.pool()).await;
    seed(
        f.db.pool(),
        "9999",
        "foreign_only",
        "!foreign_only",
        "Foreign secret",
        true,
    )
    .await;
    let with_foreign = definitions(f.db.pool()).await;
    let invalid = vec![
        vec![],
        vec![option("name", "")],
        vec![option("name", " ")],
        vec![option("name", "missing")],
        vec![option("name", "foreign_only")],
        vec![option("name", "!faq")],
        vec![option("name", "bad name")],
        vec![option("name", &"x".repeat(33))],
        vec![option("name", &"😀".repeat(17))],
        vec![json!({"name":"name", "type":4, "value":1})],
        vec![option("name", "faq"), option("name", "other")],
        vec![
            option("name", "faq"),
            json!({"name":"name", "type":5, "value":true}),
        ],
        vec![
            option("name", "faq"),
            option("template", "Injected replacement"),
        ],
        vec![option("name", "faq"), option("text-trigger", "!stolen")],
    ];
    let mut seen = Vec::new();
    for (index, options) in invalid.into_iter().enumerate() {
        for command in [ENABLE, DISABLE] {
            seen.push(
                f.observe(&interaction(
                    400 + index as u64,
                    command,
                    32,
                    options.clone(),
                ))
                .await,
            );
        }
    }
    let after = definitions(f.db.pool()).await;
    let audit_after = facts(f.db.pool()).await;
    f.close().await;
    assert_eq!(row(&before)["enabled"], false);
    assert_eq!(
        with_foreign, after,
        "refusals are zero-write, including foreign rows and timestamps"
    );
    assert_eq!(
        audit_before, audit_after,
        "no mutation/run facts for malformed or missing targets"
    );
    for observed in seen {
        let text = private_reply(&observed).to_ascii_lowercase();
        assert!(publications(&observed.requests).is_empty());
        assert!(
            !text.contains("saved /")
                && !text.contains("removed /")
                && !text.starts_with("enabled /")
        );
        assert!(
            text.contains("name")
                || text.contains("option")
                || text.contains("found")
                || text.contains("invalid"),
            "actionable refusal: {text}"
        );
        assert!(!text.contains("Foreign secret") && !text.contains("Injected replacement"));
    }
}

#[tokio::test]
async fn toggle_refuses_reserved_or_invalid_saved_definition_without_reactivation() {
    let f = Fixture::new(false).await;
    for name in ["help", ENABLE, DISABLE] {
        seed(
            f.db.pool(),
            "2222",
            name,
            &format!("!{name}"),
            "Imported reserved row",
            false,
        )
        .await;
    }
    let mut refusals = Vec::new();
    let mut checks = Vec::new();
    for (index, name) in ["help", ENABLE, DISABLE].iter().enumerate() {
        let before = definitions(f.db.pool()).await;
        refusals.push(f.observe(&toggle(501 + index as u64, true, name)).await);
        checks.push((before, definitions(f.db.pool()).await));
    }
    for (index, template, trigger) in [(0, "Unknown {args}", "!faq"), (1, TEMPLATE, "!rank")] {
        seed(f.db.pool(), "2222", "faq", trigger, template, false).await;
        let before = definitions(f.db.pool()).await;
        refusals.push(f.observe(&toggle(510 + index, true, "faq")).await);
        checks.push((before, definitions(f.db.pool()).await));
    }
    let requests = f.mock.requests();
    f.close().await;
    for (before, after) in checks {
        assert_eq!(
            before, after,
            "failed enable cannot reactivate invalid/imported definitions"
        );
    }
    for refused in refusals {
        assert!(!private_reply(&refused).is_empty());
    }
    assert!(publications(&requests).is_empty());
}

#[tokio::test]
async fn toggle_paused_trigger_remains_owned_and_existing_replacement_semantics_stay_live() {
    let f = Fixture::new(true).await;
    let disabled = f.observe(&toggle(601, false, "faq")).await;
    let paused = definitions(f.db.pool()).await;
    let conflict = service::put(
        f.db.pool(),
        true,
        "2222",
        "3333",
        &PutCommandInput {
            name: "rival".to_owned(),
            description: "Rival".to_owned(),
            template: "Rival body".to_owned(),
            text_trigger: Some("!faq".to_owned()),
        },
        "rival-conflict",
        AT,
    )
    .await;
    let after_conflict = definitions(f.db.pool()).await;
    let replaced = f
        .observe(&interaction(
            602,
            "command",
            32,
            vec![
                option("name", "faq"),
                option("template", "Explicit replacement"),
                option("description", "Replacement description"),
                option("text-trigger", "!replacement"),
            ],
        ))
        .await;
    let after_replace = definitions(f.db.pool()).await;
    f.close().await;
    changed_reply(&disabled, false);
    assert!(
        matches!(conflict, Err(service::ServiceError::TriggerInUse)),
        "pause cannot free a stored trigger for a rival"
    );
    assert_eq!(paused, after_conflict);
    assert_eq!(
        private_reply(&replaced),
        "Saved /faq.",
        "the separate replacement command is not redesigned"
    );
    assert_eq!(row(&after_replace)["enabled"], true);
    assert_eq!(row(&after_replace)["template"], "Explicit replacement");
    assert_eq!(
        row(&after_replace)["description"],
        "Replacement description"
    );
    assert_eq!(row(&after_replace)["text_trigger"], "!replacement");
    assert_eq!(row(&after_replace)["created_by"], "66");
    assert_eq!(
        row(&after_replace)["created_at"],
        row(&paused)["created_at"]
    );
}

#[tokio::test]
async fn toggle_at_capacity_can_resume_existing_definition_but_cannot_add_another() {
    let f = Fixture::new(false).await;
    // Independent pin: 67 published +33 custom =100. The reservation-only
    // `templateassistant` holds no guild slot, so storage uses 100-67=33, not
    // 100-68=32. Disabled rows count toward storage, not just picker entries.
    // Rationale: `reserved_command_names().len()` is 68 (67 published +1 held),
    // but `max_custom_commands(68)=32` would refuse the 33rd valid row. The
    // 34th row must still overflow; this test preserves that control.
    assert_eq!(
        reserved_command_names().len(),
        68,
        "68 reserved names pin the 32-vs-33 oracle: 67 published +1 held"
    );
    let capacity = 33;
    for index in 2..capacity {
        seed(
            f.db.pool(),
            "2222",
            &format!("c{index:02}"),
            &format!("!c{index:02}"),
            "Capacity peer",
            true,
        )
        .await;
    }
    let before = definitions(f.db.pool()).await;
    let resumed = f.observe(&toggle(701, true, "faq")).await;
    let overflow = service::put(
        f.db.pool(),
        true,
        "2222",
        "3333",
        &PutCommandInput {
            name: "overflow".to_owned(),
            description: "Overflow".to_owned(),
            template: "No room".to_owned(),
            text_trigger: None,
        },
        "overflow",
        AT,
    )
    .await;
    let after = definitions(f.db.pool()).await;
    f.close().await;
    changed_reply(&resumed, true);
    assert_eq!(row(&after)["enabled"], true);
    assert_eq!(immutable_fields(&before), immutable_fields(&after));
    assert!(
        matches!(
            overflow,
            Err(service::ServiceError::Validation(
                CommandError::OverCapacity(33)
            ))
        ),
        "34th row overflows with independent 33 payload"
    );
    let sets = publications(&resumed.requests);
    assert_eq!(sets.len(), 1);
    let names = published_names(&sets[0]);
    for index in 2..capacity {
        assert_eq!(
            names
                .iter()
                .filter(|&&name| name == format!("c{index:02}"))
                .count(),
            1,
            "retain every peer at the legal storage bound"
        );
    }
    assert_full_set(&sets[0], true);
}

#[tokio::test]
async fn toggle_publication_failure_reports_saved_state_not_sync_and_does_not_repair_on_noop() {
    for enabled in [false, true] {
        let f = Fixture::new(!enabled).await;
        f.runtime
            .sync_registry()
            .await
            .expect("confirm initial registry");
        let prior_snapshot = f
            .runtime
            .published_commands()
            .expect("confirmed publication");
        let prior_names: Vec<_> = prior_snapshot.iter().map(|d| d.name.clone()).collect();
        let before = definitions(f.db.pool()).await;
        f.failures.publish.store(true, Ordering::SeqCst);
        let changed = f.observe(&toggle(801, enabled, "faq")).await;
        let saved = definitions(f.db.pool()).await;
        let snapshot_names: Vec<_> = f
            .runtime
            .published_commands()
            .expect("retain prior snapshot")
            .iter()
            .map(|d| d.name.clone())
            .collect();
        let repeated = f.observe(&toggle(802, enabled, "faq")).await;
        let after_noop = definitions(f.db.pool()).await;
        f.failures.publish.store(false, Ordering::SeqCst);
        let repair = f.runtime.sync_registry().await;
        let sets = publications(&f.mock.requests());
        f.close().await;
        changed_reply(&changed, enabled);
        assert_eq!(
            row(&saved)["enabled"],
            enabled,
            "Discord failure does not roll back a saved transition"
        );
        assert_eq!(immutable_fields(&before), immutable_fields(&saved));
        assert_eq!(
            prior_names, snapshot_names,
            "only confirmed PUT advances discovery"
        );
        let text = private_reply(&changed).to_ascii_lowercase();
        assert!(
            text.contains("not") && (text.contains("synchron") || text.contains("publish")),
            "honest partial outcome: {text}"
        );
        assert!(
            ["resync", "restart", "reconcil"]
                .iter()
                .any(|action| text.contains(action)),
            "safe next action for unsynchronized picker: {text}"
        );
        assert!(
            !text.contains("/command-remove"),
            "never recommend deleting the saved definition"
        );
        unchanged_reply(&repeated, enabled);
        assert_eq!(saved, after_noop);
        assert!(!private_reply(&repeated)
            .to_ascii_lowercase()
            .contains("now synchronized"));
        assert!(repair.is_ok());
        assert_eq!(
            sets.len(),
            3,
            "initial, failed transition, explicit synchronization only"
        );
        assert_full_set(&sets[2], enabled);
    }
}

#[tokio::test]
async fn toggle_audit_failure_rolls_back_state_and_never_publishes() {
    for enabled in [false, true] {
        let f = Fixture::new(!enabled).await;
        sqlx::raw_sql("CREATE FUNCTION refuse_toggle_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture audit refusal'; END; $$; CREATE TRIGGER refuse_toggle_audit BEFORE INSERT ON automation_audit_log FOR EACH ROW EXECUTE FUNCTION refuse_toggle_audit();")
            .execute(f.db.pool()).await.expect("test-owned audit-failure injection");
        let before = definitions(f.db.pool()).await;
        let observed = f.observe(&toggle(901, enabled, "faq")).await;
        let after = definitions(f.db.pool()).await;
        let audit = facts(f.db.pool()).await;
        f.close().await;
        assert!(
            matches!(observed.result, Err(CustomCommandError::Storage)),
            "required audit is atomic with the transition: {:?}",
            observed.result
        );
        assert_eq!(before, after);
        assert!(audit.is_empty());
        assert!(publications(&observed.requests).is_empty());
        assert_eq!(
            observed.requests.len(),
            2,
            "ack then one safe completion, not a stranded defer"
        );
        let text = body(&observed.requests[1])["content"]
            .as_str()
            .expect("failure reply")
            .to_owned();
        assert!(
            !text.contains("fixture audit refusal")
                && !text.contains("Removed")
                && !text.contains("Saved /")
        );
        no_mentions(&body(&observed.requests[1]));
    }
}

#[tokio::test]
async fn toggle_failed_acknowledgement_never_changes_or_reactivates_saved_state() {
    for enabled in [false, true] {
        let f = Fixture::new(!enabled).await;
        let before = definitions(f.db.pool()).await;
        f.failures.ack.store(true, Ordering::SeqCst);
        let observed = f.observe(&toggle(1001, enabled, "faq")).await;
        let after = definitions(f.db.pool()).await;
        let audit = facts(f.db.pool()).await;
        f.close().await;
        assert!(matches!(observed.result, Err(CustomCommandError::Delivery)));
        assert_eq!(before, after);
        assert!(audit.is_empty());
        assert_eq!(
            observed.requests.len(),
            1,
            "one refused defer, no retry/publication/completion"
        );
        assert!(observed.requests[0].path.ends_with("/callback"));
    }
}

#[tokio::test]
async fn toggle_simultaneous_same_state_requests_commit_one_transition_and_one_full_set() {
    let f = Fixture::new(true).await;
    let before = definitions(f.db.pool()).await;
    let one = toggle(1101, false, "faq");
    let two = toggle(1102, false, "faq");
    let clone = f.runtime.clone();
    let (first, second) = tokio::join!(
        f.runtime.handle_interaction(&one, Some("Test guild")),
        clone.handle_interaction(&two, Some("Test guild"))
    );
    let after = definitions(f.db.pool()).await;
    let requests = f.mock.requests();
    f.close().await;
    assert!(matches!(first, Ok(true)) && matches!(second, Ok(true)));
    assert_eq!(row(&after)["enabled"], false);
    assert_eq!(immutable_fields(&before), immutable_fields(&after));
    let sets = publications(&requests);
    assert_eq!(
        sets.len(),
        1,
        "clones share mutation/read/full-set serialization"
    );
    assert_full_set(&sets[0], false);
    let replies: Vec<_> = [1101, 1102]
        .into_iter()
        .map(|id| {
            let selected = requests
                .iter()
                .filter(|r| r.path.contains(&format!("toggle-fixture-{id}")))
                .cloned()
                .collect();
            private_reply(&Observed {
                result: Ok(true),
                requests: selected,
                id,
            })
            .to_ascii_lowercase()
        })
        .collect();
    assert_eq!(
        replies
            .iter()
            .filter(|text| text.contains("already")
                || text.contains("unchanged")
                || text.contains("no change"))
            .count(),
        1
    );
}

#[tokio::test]
async fn toggle_opposite_concurrent_calls_leave_last_published_set_equal_to_saved_state() {
    let f = Fixture::new(false).await;
    let before = definitions(f.db.pool()).await;
    let enable = toggle(1201, true, "faq");
    let disable = toggle(1202, false, "faq");
    let (first, second) = tokio::join!(
        f.runtime.handle_interaction(&enable, Some("Test guild")),
        f.runtime.handle_interaction(&disable, Some("Test guild"))
    );
    let after = definitions(f.db.pool()).await;
    let sets = publications(&f.mock.requests());
    f.close().await;
    assert!(matches!(first, Ok(true)) && matches!(second, Ok(true)));
    assert_eq!(immutable_fields(&before), immutable_fields(&after));
    assert!(!sets.is_empty() && sets.len() <= 2);
    assert_full_set(
        sets.last().expect("at least the enable publishes"),
        row(&after)["enabled"].as_bool().expect("enabled state"),
    );
}

#[tokio::test]
async fn toggle_committed_disable_blocks_stale_invocations_while_publication_is_in_flight() {
    let f = Fixture::new(true).await;
    // Gate the transition's PUT, not a guessed sleep. The prior registry really
    // contains faq; release it only after checking invocation against saved state.
    let initial = InteractionRouter::new(gates(true))
        .publish_set(&[
            StoredCommand {
                guild_id: "2222".to_owned(),
                name: "faq".to_owned(),
                description: "Saved description".to_owned(),
                template: TEMPLATE.to_owned(),
                text_trigger: Some("!faq".to_owned()),
                enabled: true,
            }
            .registry_entry(),
            StoredCommand {
                guild_id: "2222".to_owned(),
                name: "other".to_owned(),
                description: "Saved description".to_owned(),
                template: "Other saved response".to_owned(),
                text_trigger: Some("!other".to_owned()),
                enabled: true,
            }
            .registry_entry(),
        ])
        .expect("initial registry");
    let receipt = |count: usize| {
        ScriptedResponse::json(200, Value::Array((0..count).map(|i| json!({"id": (8000+i).to_string(), "application_id":"1111", "version":"1", "type":1, "name":format!("receipt-{i}"), "description":"fixture"})).collect()))
    };
    let (mock, gate) = MockRest::start_gated_at(
        vec![
            receipt(initial.len()),
            ScriptedResponse::status(204),
            ScriptedResponse::status(403),
        ],
        ScriptedResponse::json(200, json!({"id":"9000"})),
        2,
    )
    .await;
    let executor = ActionExecutor::with_proxy("toggle-fixture".to_owned(), Some(mock.origin()))
        .expect("loopback executor");
    let service = runtime(f.db.pool().clone(), executor, true);
    service
        .sync_registry()
        .await
        .expect("confirmed pre-disable picker");
    let task_runtime = service.clone();
    let task = tokio::spawn(async move {
        task_runtime
            .handle_interaction(&toggle(1301, false, "faq"), Some("Test guild"))
            .await
    });
    let reached = tokio::time::timeout(Duration::from_secs(3), gate.wait_for_request())
        .await
        .is_ok();
    let during = definitions(f.db.pool()).await;
    let before_invocation = facts(f.db.pool()).await;
    let start = mock.requests().len();
    let invocations = if reached {
        Some(
            tokio::time::timeout(Duration::from_secs(2), async {
                let slash = interaction(1302, "faq", 0, vec![]);
                let prefix = prefix(1303, 3334);
                tokio::join!(
                    service.handle_interaction(&slash, Some("Test guild")),
                    service.handle_message(
                        &prefix,
                        AutomationMessageAcceptance::Unmatched,
                        true,
                        Some("Test guild")
                    )
                )
            })
            .await,
        )
    } else {
        None
    };
    let invocation_requests = mock.requests()[start..].to_vec();
    let after_invocation = facts(f.db.pool()).await;
    gate.release();
    let result = task.await.expect("disable task completes");
    let requests = mock.requests();
    mock.shutdown().await;
    drop(service);
    f.close().await;
    assert!(
        reached,
        "disable must reach full-set publication after committing"
    );
    assert_eq!(row(&during)["enabled"], false);
    assert!(matches!(invocations, Some(Ok((Ok(false), Ok(TextCommandOutcome::Ignored))))), "both stale invocation surfaces settle without REST even while publication is held: {invocations:?}");
    assert!(
        invocation_requests.is_empty(),
        "stale picker cannot bypass persisted pause during PUT"
    );
    assert_eq!(
        before_invocation, after_invocation,
        "no stale run or replay claim"
    );
    assert!(matches!(result, Ok(true)));
    assert_full_set(&publications(&requests)[1], false);
}

#[tokio::test]
async fn toggle_waits_for_existing_guild_lock_and_cannot_resurrect_a_concurrently_removed_row() {
    let f = Fixture::new(false).await;
    let peer =
        f.db.independent_pool()
            .await
            .expect("independent transaction observer");
    let other_runtime = runtime(peer.clone(), f.executor.clone(), true);
    let mut tx =
        f.db.pool()
            .begin()
            .await
            .expect("hold lifecycle transaction");
    store::lock_command_capacity(&mut tx, "2222")
        .await
        .expect("existing guild lifecycle lock");
    store::delete_command(&mut *tx, "2222", "faq")
        .await
        .expect("uncommitted competing removal");
    let task = tokio::spawn(async move {
        other_runtime
            .handle_interaction(&toggle(1351, true, "faq"), Some("Test guild"))
            .await
    });
    let waited = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND NOT granted AND database = (SELECT oid FROM pg_database WHERE datname = current_database()))")
                .fetch_one(f.db.pool()).await.expect("observe this isolated database's lock wait");
            if blocked { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.is_ok();
    tx.commit()
        .await
        .expect("commit competing removal, release lock");
    let result = task.await.expect("waiting lifecycle request settles");
    let after = definitions(f.db.pool()).await;
    let audit = facts(f.db.pool()).await;
    let requests = f.mock.requests();
    peer.close().await;
    f.close().await;
    assert!(waited, "a separate runtime must use the existing transaction lock, not an earlier cached definition");
    assert!(matches!(result, Ok(true)));
    assert!(
        !after
            .iter()
            .any(|r| r["guild_id"] == "2222" && r["name"] == "faq"),
        "enable is not an upsert or resurrection"
    );
    assert!(
        audit.is_empty(),
        "a now-missing target is a zero-write refusal"
    );
    assert!(publications(&requests).is_empty());
    let text = private_reply(&Observed {
        result,
        requests,
        id: 1351,
    })
    .to_ascii_lowercase();
    assert!(text.contains("found") || text.contains("missing"));
    assert!(
        !text.contains("enabled /") && !text.contains("saved /") && !text.contains("removed /")
    );
}

// Unchanged controls are deliberately separate from missing-toggle assertions,
// so a RED report can distinguish broken fixture infrastructure from behavior.
#[tokio::test]
async fn toggle_control_existing_enabled_and_disabled_execution_and_discovery() {
    for enabled in [false, true] {
        let f = Fixture::new(enabled).await;
        let before = definitions(f.db.pool()).await;
        let text = f.text(1401, 3333).await;
        let slash = f.observe(&interaction(1402, "faq", 0, vec![])).await;
        let inspect = f
            .observe(&interaction(
                1403,
                "command-inspect",
                32,
                vec![option("name", "faq")],
            ))
            .await;
        let list = f
            .observe(&interaction(1404, "command-list", 32, vec![]))
            .await;
        let after = definitions(f.db.pool()).await;
        let audit = facts(f.db.pool()).await;
        f.close().await;
        assert_eq!(before, after);
        assert!(private_reply(&inspect).contains(if enabled {
            "Status: Enabled"
        } else {
            "Status: Disabled"
        }));
        assert!(private_reply(&list).contains(if enabled {
            "/faq (or !faq) — on"
        } else {
            "/faq (or !faq) — off"
        }));
        if enabled {
            assert!(
                matches!(text, Ok(TextCommandOutcome::Delivered))
                    && matches!(slash.result, Ok(true))
            );
            assert_eq!(
                audit
                    .iter()
                    .filter(|a| a["action"] == "command.run" && a["outcome"] == "ok")
                    .count(),
                2
            );
            assert_eq!(
                audit
                    .iter()
                    .filter(|a| a["action"] == "command.text_attempt")
                    .count(),
                1
            );
        } else {
            assert!(
                matches!(text, Ok(TextCommandOutcome::Ignored))
                    && matches!(slash.result, Ok(false))
            );
            assert!(slash.requests.is_empty() && audit.is_empty());
        }
    }
}

#[tokio::test]
async fn toggle_control_existing_replace_and_remove_are_not_repurposed() {
    let f = Fixture::new(false).await;
    let before = definitions(f.db.pool()).await;
    let replace = f
        .observe(&interaction(
            1501,
            "command",
            32,
            vec![
                option("name", "faq"),
                option("template", "Replacement"),
                option("text-trigger", "!newfaq"),
            ],
        ))
        .await;
    let replaced = definitions(f.db.pool()).await;
    let remove = f
        .observe(&interaction(
            1502,
            "command-remove",
            32,
            vec![option("name", "faq")],
        ))
        .await;
    let removed = definitions(f.db.pool()).await;
    let absent = f
        .observe(&interaction(
            1503,
            "command-remove",
            32,
            vec![option("name", "faq")],
        ))
        .await;
    f.close().await;
    assert_eq!(private_reply(&replace), "Saved /faq.");
    assert_eq!(row(&replaced)["enabled"], true);
    assert_eq!(row(&replaced)["template"], "Replacement");
    assert_eq!(row(&replaced)["text_trigger"], "!newfaq");
    assert_eq!(row(&replaced)["created_by"], row(&before)["created_by"]);
    assert_eq!(row(&replaced)["created_at"], row(&before)["created_at"]);
    assert_eq!(private_reply(&remove), "Removed /faq.");
    assert!(!removed
        .iter()
        .any(|r| r["guild_id"] == "2222" && r["name"] == "faq"));
    assert_eq!(private_reply(&absent), "No command /faq found.");
    assert!(publications(&absent.requests).is_empty());
}

// Independent capacity pins for findings 6 and 7: 67 published +33 custom
// =100. The reservation-only `templateassistant` holds no guild slot, so the
// budget is 100-67=33, not 100-68=32. Expected values below are literals,
// never derived from the production helper under test.
const EXPECTED_CUSTOM_CAPACITY: usize = 33;
const OVERFLOW_CUSTOM_COUNT: usize = 34;
const EXPECTED_BUILTINS_WITHOUT_VOICE: usize = 45;
const EXPECTED_BUILTINS_WITH_VOICE: usize = 67;
const EXPECTED_GUILD_LIMIT: usize = 100;

fn bulk_name(index: usize) -> String {
    format!("zzbulk{index:02}")
}

fn bulk_stored(index: usize) -> StoredCommand {
    let name = bulk_name(index);
    StoredCommand {
        guild_id: "2222".to_owned(),
        name: name.clone(),
        description: format!("Bulk {index}"),
        template: format!("Bulk reply {index} {{username}}"),
        text_trigger: Some(format!("!{name}")),
        enabled: true,
    }
}

fn bulk_custom_commands(count: usize) -> Vec<CustomCommand> {
    (0..count)
        .map(|index| CustomCommand {
            name: bulk_name(index),
            description: format!("Bulk {index}"),
            enabled: true,
        })
        .collect()
}

fn bulk_import_body(count: usize) -> Value {
    let commands: Vec<Value> = (0..count)
        .map(|index| {
            let name = bulk_name(index);
            json!({
                "name": name,
                "description": format!("Bulk {index}"),
                "template": format!("Bulk reply {index} {{username}}"),
                "text_trigger": format!("!{name}"),
            })
        })
        .collect();
    json!({"version": 1, "commands": commands})
}

fn bulk_update_body() -> Value {
    let commands: Vec<Value> = (0..EXPECTED_CUSTOM_CAPACITY)
        .map(|index| {
            let name = bulk_name(index);
            json!({
                "name": name,
                "description": format!("Bulk updated {index}"),
                "template": format!("Updated {index} {{username}} in {{server}}"),
                "text_trigger": format!("!{name}"),
            })
        })
        .collect();
    json!({
        "version": 1,
        "overwrite": true,
        "commands": commands,
    })
}

fn all_enabled_gates(voice: bool, assistant: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice,
        voice_assistant: assistant,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn automation_voice_gates(voice: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        automations: true,
        moderation: false,
        voice,
        voice_assistant: false,
        scorecard: false,
        announcements: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn runtime_with_gates(
    pool: PgPool,
    executor: ActionExecutor,
    gates: RouterGates,
) -> CustomCommandRuntime {
    let mut router = InteractionRouter::new(gates);
    CustomCommandRuntime::register(&mut router);
    CustomCommandRuntime::new(pool, Arc::new(router), executor, 1111)
}

fn prefix_with(id: u64, actor: u64, content: &str) -> Message {
    serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": "2222", "channel_id": "4444", "type": 0,
        "author": {"id": actor.to_string(), "username": "tester", "discriminator": "0000", "avatar": null},
        "content": content, "timestamp": "2026-10-24T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false,
        "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false
    }))
    .expect("human prefix fixture")
}

#[test]
fn custom_catalog_33_row_export_parses_and_diffs_without_growth() {
    let reserved = reserved_command_names();
    for index in 0..OVERFLOW_CUSTOM_COUNT {
        let name = bulk_name(index);
        assert!(
            !reserved.contains(&name),
            "bulk catalog must not collide: {name}"
        );
        assert!(!builtin_command_names().contains(&name));
    }
    let rows: Vec<StoredCommand> = (0..EXPECTED_CUSTOM_CAPACITY).map(bulk_stored).collect();
    let document = transfer::export_document(&rows);
    assert_eq!(document.version, 1);
    assert_eq!(document.commands.len(), 33);
    let names: Vec<&str> = document
        .commands
        .iter()
        .map(|command| command.name.as_str())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "export is stable sorted");
    let body = serde_json::to_value(&document).expect("export serializes");
    let parsed = transfer::parse_import_document(&body, 33).expect("33-row catalog parses");
    assert_eq!(parsed.entries.len(), 33);
    assert_eq!(parsed.invalid_entries, 0);
    assert!(!parsed.overwrite);
    let diff = transfer::diff_import(&[], &parsed, &reserved, false).expect("33-row diff plans");
    assert_eq!(diff.to_create.len(), 33);
    assert!(diff.to_update.is_empty());
    assert!(diff.rejected.is_empty());
    assert_eq!(diff.accepted_entry_indices.len(), 33);
    assert_eq!(
        transfer::max_import_entries(),
        33,
        "parser budget is 33, not 32; reservation-only templateassistant holds no slot"
    );
    let overflow_body = bulk_import_body(34);
    let parser_overflow = transfer::parse_import_document(&overflow_body, 33);
    assert!(
        matches!(
            parser_overflow,
            Err(transfer::ImportParseError::TooManyEntries { count: 34, max: 33 })
        ),
        "parser refuses 34 with independent 33 budget: {parser_overflow:?}"
    );
    let parsed_loose =
        transfer::parse_import_document(&overflow_body, 100).expect("loose parse reaches planner");
    assert_eq!(
        transfer::diff_import(&[], &parsed_loose, &reserved, true),
        Err(CommandError::OverCapacity(33)),
        "34-row union overflows with independent 33 payload"
    );
    let updates = transfer::parse_import_document(&bulk_update_body(), 33)
        .expect("no-growth bulk updates parse");
    let planned = transfer::diff_import(&rows, &updates, &reserved, true).expect("no-growth plans");
    assert_eq!(planned.to_update.len(), 33);
    assert!(planned.to_create.is_empty());
    assert!(planned.rejected.is_empty());
    let same = transfer::parse_import_document(&body, 33).expect("same content parses");
    let converged =
        transfer::diff_import(&rows, &same, &reserved, false).expect("same content converges");
    assert_eq!(converged.unchanged.len(), 33);
    assert_eq!(converged.accepted_entry_indices.len(), 33);
}

#[test]
fn all_enabled_publication_67_plus_33_equals_100_with_voice_off_and_on() {
    for assistant in [false, true] {
        let off = InteractionRouter::new(all_enabled_gates(false, assistant));
        let off_builtins = off.publish_set(&[]).expect("off assembles");
        assert_eq!(
            off_builtins.len(),
            EXPECTED_BUILTINS_WITHOUT_VOICE,
            "assistant={assistant}"
        );
        let off_full = off
            .publish_set(&bulk_custom_commands(33))
            .expect("off 33 fits");
        assert_eq!(off_full.len(), 78, "45+33=78 assistant={assistant}");
        let on = InteractionRouter::new(all_enabled_gates(true, assistant));
        let on_builtins = on.publish_set(&[]).expect("on assembles");
        assert_eq!(
            on_builtins.len(),
            EXPECTED_BUILTINS_WITH_VOICE,
            "assistant={assistant}"
        );
        assert!(
            !on_builtins
                .iter()
                .any(|definition| definition.name == "templateassistant"),
            "assistant stays unpublished assistant={assistant}"
        );
        let on_full = on
            .publish_set(&bulk_custom_commands(33))
            .expect("on 33 fits");
        assert_eq!(on_full.len(), EXPECTED_GUILD_LIMIT, "67+33=100");
        let overflow = on.publish_set(&bulk_custom_commands(34));
        assert!(
            matches!(overflow, Err(RegistryError::TotalLimit(101))),
            "67+34=101 overflows publication assistant={assistant}: {overflow:?}"
        );
    }
}

#[test]
fn myroom_router_precedence_off_runs_custom_and_on_yields_to_voice_sink() {
    for custom_row in [None, Some(true), Some(false)] {
        let off = InteractionRouter::new(automation_voice_gates(false));
        assert!(
            !off.voice_owns_command("myroom"),
            "off claims nothing for custom_row={custom_row:?}"
        );
        let context = SlashContext {
            name: "myroom",
            guild_id: Some(2222),
            actor_permissions: Some(0),
            custom_row,
        };
        match custom_row {
            Some(true) => assert_eq!(
                off.route_slash(&context),
                SlashOutcome::Handled {
                    handler: HandlerId::AutomationCustom
                },
                "off runs the grandfathered row"
            ),
            _ => assert_eq!(
                off.route_slash(&context),
                SlashOutcome::Unknown,
                "off without an enabled row is unchanged"
            ),
        }
        let on = InteractionRouter::new(automation_voice_gates(true));
        assert!(on.voice_owns_command("myroom"));
        for permissions in [None, Some(0), Some(u64::MAX)] {
            let context = SlashContext {
                name: "myroom",
                guild_id: Some(2222),
                actor_permissions: permissions,
                custom_row,
            };
            assert_eq!(
                on.route_slash(&context),
                SlashOutcome::Ignore,
                "on yields even with custom_row={custom_row:?} permissions={permissions:?}"
            );
        }
        let on_publish = on
            .publish_set(&[CustomCommand {
                name: "myroom".to_owned(),
                description: "Legacy rooms".to_owned(),
                enabled: true,
            }])
            .expect("on assembles");
        let myroom = on_publish
            .iter()
            .find(|definition| definition.name == "myroom")
            .expect("voice myroom publishes");
        assert_ne!(
            myroom.description, "Legacy rooms",
            "on withholds the custom definition"
        );
        let off_publish = off
            .publish_set(&[CustomCommand {
                name: "myroom".to_owned(),
                description: "Legacy rooms".to_owned(),
                enabled: true,
            }])
            .expect("off assembles");
        let custom = off_publish
            .iter()
            .find(|definition| definition.name == "myroom")
            .expect("off publishes the grandfathered row");
        assert_eq!(custom.description, "Legacy rooms");
    }
}

#[tokio::test]
async fn custom_catalog_service_import_33_succeeds_and_34_overflows_with_bulk_updates() {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("explicit disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("isolated migrated fixture");
    let parsed = transfer::parse_import_document(&bulk_import_body(33), 33).expect("33 parses");
    let outcome = service::import(
        db.pool(),
        "2222",
        "3333",
        &parsed,
        false,
        "bulk33-import",
        AT,
    )
    .await;
    let stored = store::list_commands(db.pool(), "2222")
        .await
        .expect("list imported rows");
    let updated = transfer::parse_import_document(&bulk_update_body(), 33).expect("updates parse");
    let applied = service::import(
        db.pool(),
        "2222",
        "3333",
        &updated,
        true,
        "bulk33-update",
        AT,
    )
    .await;
    let after_update = store::list_commands(db.pool(), "2222")
        .await
        .expect("list after updates");
    let before_overflow = definitions(db.pool()).await;
    let loose =
        transfer::parse_import_document(&bulk_import_body(34), 100).expect("loose 34 parses");
    let overflow = service::import(
        db.pool(),
        "2222",
        "3333",
        &loose,
        true,
        "bulk34-overflow",
        AT,
    )
    .await;
    let after_overflow = definitions(db.pool()).await;
    db.close()
        .await
        .expect("drop owned test database before the expected RED assertion");
    let outcome = outcome.expect("33-row import commits");
    let applied = applied.expect("no-growth bulk updates commit");
    assert_eq!(outcome.imported, 33);
    assert_eq!(outcome.skipped, 0);
    assert!(outcome.conflicts.is_empty());
    assert_eq!(stored.len(), 33);
    assert_eq!(applied.imported, 33);
    assert_eq!(applied.skipped, 0);
    assert_eq!(after_update.len(), 33);
    assert!(
        after_update
            .iter()
            .all(|row| row.template.starts_with("Updated ")),
        "bulk updates rewrite templates without growth"
    );
    assert!(
        matches!(overflow, Err(service::ImportServiceError::OverCapacity(33))),
        "34th row overflows with independent 33 payload: {overflow:?}"
    );
    assert_eq!(
        before_overflow, after_overflow,
        "overflow writes nothing, including timestamps"
    );
    let live: Vec<CustomCommand> = after_update
        .iter()
        .map(StoredCommand::registry_entry)
        .collect();
    assert_eq!(live.len(), 33);
    for (voice, expected) in [(false, 78), (true, 100)] {
        let router = InteractionRouter::new(all_enabled_gates(voice, false));
        let set = router.publish_set(&live).expect("bulk publishes");
        assert_eq!(set.len(), expected, "voice={voice}");
    }
}

#[tokio::test]
async fn grandfathered_myroom_publishes_dispatches_edits_and_toggles_with_voice_off() {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("explicit disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("isolated migrated fixture");
    seed(
        db.pool(),
        "2222",
        "myroom",
        "!myroom",
        "Legacy {username} in {server}",
        true,
    )
    .await;
    seed(
        db.pool(),
        "2222",
        "freshsafe",
        "!freshsafe",
        "Fresh {username}",
        true,
    )
    .await;
    let mock =
        MockRest::with_responder(|request| reply_to(request, &WireFailures::default())).await;
    let executor = ActionExecutor::with_proxy("myroom-off-fixture".to_owned(), Some(mock.origin()))
        .expect("loopback-only executor");
    let runtime = runtime_with_gates(
        db.pool().clone(),
        executor.clone(),
        automation_voice_gates(false),
    );
    let rows = store::list_commands(db.pool(), "2222")
        .await
        .expect("stored grandfathered rows");
    let slash = observe(&runtime, &mock, &interaction(2001, "myroom", 0, vec![])).await;
    let prefix = runtime
        .handle_message(
            &prefix_with(2002, 3333, "!myroom ordinary arguments"),
            AutomationMessageAcceptance::Unmatched,
            true,
            Some("Test guild"),
        )
        .await;
    let fresh_slash = observe(&runtime, &mock, &interaction(2003, "freshsafe", 0, vec![])).await;
    // The same-name maintenance exception must not make the newly reserved
    // name available for a fresh write, even while voice is off.
    let fresh_collision = service::put(
        db.pool(),
        true,
        "2233",
        "3333",
        &PutCommandInput {
            name: "myroom".to_owned(),
            description: "New collision".to_owned(),
            template: "Not grandfathered".to_owned(),
            text_trigger: None,
        },
        "myroom-fresh-off",
        AT,
    )
    .await;
    let fresh_collision_rows = store::list_commands(db.pool(), "2233").await;
    let updated = service::put(
        db.pool(),
        true,
        "2222",
        "3333",
        &PutCommandInput {
            name: "myroom".to_owned(),
            description: "Updated rooms".to_owned(),
            template: "Updated {username}".to_owned(),
            text_trigger: Some("!myroom".to_owned()),
        },
        "myroom-edit-off",
        AT,
    )
    .await;
    let fresh_edit = service::put(
        db.pool(),
        true,
        "2222",
        "3333",
        &PutCommandInput {
            name: "freshsafe".to_owned(),
            description: "Fresh updated".to_owned(),
            template: "Fresh updated {username}".to_owned(),
            text_trigger: Some("!freshsafe".to_owned()),
        },
        "freshsafe-edit-off",
        AT,
    )
    .await;
    let disabled = observe(&runtime, &mock, &toggle(2004, false, "myroom")).await;
    let enabled = observe(&runtime, &mock, &toggle(2005, true, "myroom")).await;
    let fresh_disabled = observe(&runtime, &mock, &toggle(2006, false, "freshsafe")).await;
    let fresh_enabled = observe(&runtime, &mock, &toggle(2007, true, "freshsafe")).await;
    drop(runtime);
    drop(executor);
    mock.shutdown().await;
    db.close().await.expect("drop only the owned test database");
    // Assert captured voice-off observations only after explicit teardown.
    assert_eq!(rows.len(), 2);
    let customs: Vec<CustomCommand> = rows.iter().map(StoredCommand::registry_entry).collect();
    let published = InteractionRouter::new(automation_voice_gates(false))
        .publish_set(&customs)
        .expect("off publishes");
    let myroom = published
        .iter()
        .find(|definition| definition.name == "myroom")
        .expect("off publishes the grandfathered row");
    assert_eq!(myroom.description, "Saved description");
    assert!(
        published
            .iter()
            .any(|definition| definition.name == "freshsafe"),
        "fresh names publish off"
    );
    assert!(
        matches!(slash.result, Ok(true)),
        "off runs myroom: {:?}",
        slash.result
    );
    let completion = slash
        .requests
        .iter()
        .find(|request| request.method == "PATCH")
        .expect("myroom slash completes");
    assert_eq!(body(completion)["content"], "Legacy tester in Test guild");
    no_mentions(&body(completion));
    assert!(matches!(prefix, Ok(TextCommandOutcome::Delivered)));
    assert!(matches!(fresh_slash.result, Ok(true)));
    assert!(
        matches!(
            fresh_collision,
            Err(service::ServiceError::Validation(
                CommandError::ReservedName(_)
            ))
        ),
        "fresh myroom stays reserved: {fresh_collision:?}"
    );
    assert!(fresh_collision_rows
        .expect("fresh-name collision rows")
        .is_empty());
    assert!(
        updated.is_ok(),
        "off allows same-name edits to the grandfathered row: {updated:?}"
    );
    assert!(fresh_edit.is_ok(), "fresh names edit off: {fresh_edit:?}");
    changed_named_reply(&disabled, false, "myroom");
    changed_named_reply(&enabled, true, "myroom");
    changed_named_reply(&fresh_disabled, false, "freshsafe");
    changed_named_reply(&fresh_enabled, true, "freshsafe");
}

#[tokio::test]
async fn voice_on_retains_myroom_precedence_and_collision_safety() {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("explicit disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("isolated migrated fixture");
    seed(
        db.pool(),
        "2222",
        "myroom",
        "!myroom",
        "Legacy {username} in {server}",
        true,
    )
    .await;
    seed(
        db.pool(),
        "2222",
        "freshsafe",
        "!freshsafe",
        "Fresh {username}",
        true,
    )
    .await;
    let mock =
        MockRest::with_responder(|request| reply_to(request, &WireFailures::default())).await;
    let executor = ActionExecutor::with_proxy("myroom-on-fixture".to_owned(), Some(mock.origin()))
        .expect("loopback-only executor");
    let runtime = runtime_with_gates(
        db.pool().clone(),
        executor.clone(),
        automation_voice_gates(true),
    );
    let rows = store::list_commands(db.pool(), "2222")
        .await
        .expect("stored grandfathered rows");
    let customs: Vec<CustomCommand> = rows.iter().map(StoredCommand::registry_entry).collect();
    // Observe everything first; assert only after awaited teardown so a red
    // path never leaves the owned database behind.
    let published = InteractionRouter::new(automation_voice_gates(true)).publish_set(&customs);
    let yielded = observe(&runtime, &mock, &interaction(2101, "myroom", 0, vec![])).await;
    let fresh = observe(&runtime, &mock, &interaction(2102, "freshsafe", 0, vec![])).await;
    let fresh_prefix = runtime
        .handle_message(
            &prefix_with(2103, 3333, "!freshsafe hello"),
            AutomationMessageAcceptance::Unmatched,
            true,
            Some("Test guild"),
        )
        .await;
    let myroom_prefix = runtime
        .handle_message(
            &prefix_with(2104, 3334, "!myroom hello"),
            AutomationMessageAcceptance::Unmatched,
            true,
            Some("Test guild"),
        )
        .await;
    let fresh_voice_name = service::put(
        db.pool(),
        true,
        "2222",
        "3333",
        &PutCommandInput {
            name: "setup".to_owned(),
            description: "Fresh collision".to_owned(),
            template: "No room".to_owned(),
            text_trigger: None,
        },
        "fresh-setup-on",
        AT,
    )
    .await;
    let disabled = observe(&runtime, &mock, &toggle(2105, false, "myroom")).await;
    drop(runtime);
    drop(executor);
    mock.shutdown().await;
    db.close().await.expect("drop only the owned test database");

    let published = published.expect("on publishes");
    let myroom = published
        .iter()
        .find(|definition| definition.name == "myroom")
        .expect("voice myroom publishes");
    assert_ne!(
        myroom.description, "Saved description",
        "on withholds the custom definition"
    );
    assert!(
        published
            .iter()
            .any(|definition| definition.name == "freshsafe"),
        "fresh names still publish on"
    );
    assert!(
        matches!(yielded.result, Ok(false)) && yielded.requests.is_empty(),
        "on yields myroom to the voice sink: {:?}",
        yielded.result
    );
    assert!(
        matches!(fresh.result, Ok(true)),
        "fresh still runs on: {:?}",
        fresh.result
    );
    assert!(matches!(fresh_prefix, Ok(TextCommandOutcome::Delivered)));
    assert!(
        matches!(myroom_prefix, Ok(TextCommandOutcome::Delivered)),
        "voice has no prefix counterpart, so !myroom still delivers on"
    );
    assert!(
        matches!(
            fresh_voice_name,
            Err(service::ServiceError::Validation(
                CommandError::ReservedName(_)
            ))
        ),
        "fresh voice names stay refused on: {fresh_voice_name:?}"
    );
    changed_named_reply(&disabled, false, "myroom");
}
