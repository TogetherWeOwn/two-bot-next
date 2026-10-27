#![cfg(feature = "db")]

//! Protected acceptance: correct a response without replacing omitted metadata.
//!
//! `/command-edit name:<existing-name>` is additive, not a reinterpretation of
//! `/command`. Optional `template`, `description` and `text-trigger` are patches;
//! `clear-trigger:true` explicitly removes the trigger. A supplied trigger and
//! true clear flag contradict each other. False clear is not a clear operation.
//!
//! Real router/runtime, real disposable SQL store, loopback-only Discord HTTP.
//! Every rejected edit must own its interaction and reply: a missing handler
//! cannot pass this matrix by doing nothing. No Builder implementation consulted.

#[allow(dead_code)]
mod common;

use std::{any::Any, future::Future, panic::AssertUnwindSafe, pin::Pin, sync::Arc, time::Duration};

use common::{MockRest, RestRequest, ScriptedResponse};
use futures_util::FutureExt;
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use twilight_model::{application::interaction::Interaction, channel::Message, id::Id};
use two_bot_core::{
    command_permissions::{command_permission, CommandSurface},
    custom_command_store as store,
    custom_commands::{
        builtin_command_names, reserved_command_names, AutomationMessageAcceptance, StoredCommand,
    },
    help::help_text,
    InteractionRouter, RouterGates,
};
use two_bot_discord::{
    custom_commands::{CustomCommandError, CustomCommandRuntime, TextCommandOutcome},
    interactions::publish_commands,
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

const EDIT: &str = "command-edit";
const ORIGINAL: &str = "ORIGINAL_CONTENT_SENTINEL {username}";
const CHANGED: &str = "EDITED_CONTENT_SENTINEL {username}";
const DESCRIPTION: &str = "A discoverable saved response";

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
        ActionExecutor::with_proxy("edit-acceptance-fixture".to_owned(), Some(mock.origin()))
            .expect("loopback executor"),
        1111,
    )
}

fn option(name: &str, value: &str) -> Value {
    json!({"name": name, "type": 3, "value": value})
}

fn clear(value: bool) -> Value {
    json!({"name": "clear-trigger", "type": 5, "value": value})
}

fn interaction(id: u64, command: &str, permissions: u64, options: Vec<Value>) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": format!("edit-fixture-{id}"), "version": 1,
        "guild_id": "2222", "channel": {"id": "4444", "type": 0, "name": "commands"},
        "authorizing_integration_owners": {"0": "2222"}, "entitlements": [],
        "member": {
            "permissions": permissions.to_string(), "roles": [], "joined_at": null,
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null}
        },
        "data": {"id": "5555", "name": command, "type": 1, "options": options}
    }))
    .expect("well-formed Twilight interaction, including forged option types")
}

fn edit(id: u64, mut options: Vec<Value>) -> Interaction {
    options.insert(0, option("name", "faq"));
    interaction(id, EDIT, 32, options)
}

fn message(id: u64, trigger: &str) -> Message {
    serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": "2222", "channel_id": "4444", "type": 0,
        "author": {"id": (id + 10000).to_string(), "username": "tester", "discriminator": "0000", "avatar": null},
        "content": format!("{trigger} ignored arguments"), "timestamp": "2026-10-24T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false,
        "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false
    }))
    .expect("human, non-webhook, accepted prefix fixture")
}

async fn discord(publication_fails: bool) -> MockRest {
    MockRest::with_responder(move |request| match request.method.as_str() {
        "POST" if request.path.ends_with("/callback") => ScriptedResponse::status(204),
        "PATCH" if request.path.ends_with("/messages/@original") => {
            ScriptedResponse::json(200, json!({"id": "9000"}))
        }
        "POST" if request.path.ends_with("/channels/4444/messages") => {
            ScriptedResponse::json(200, json!({"id": "9001"}))
        }
        "PUT"
            if request
                .path
                .ends_with("/applications/1111/guilds/2222/commands") =>
        {
            if publication_fails {
                ScriptedResponse::status(403)
            } else {
                let sent: Vec<Value> =
                    serde_json::from_slice(&request.body).expect("registry JSON");
                // A bulk-overwrite receipt must have one valid command per item.
                // Registry assertions below independently inspect the sent definitions.
                ScriptedResponse::json(
                    200,
                    Value::Array(
                        sent.iter()
                            .enumerate()
                            .map(|(i, command)| {
                                let mut receipt = command.clone();
                                receipt["id"] = json!((10000 + i).to_string());
                                receipt["application_id"] = json!("1111");
                                receipt["version"] = json!("1");
                                receipt
                            })
                            .collect(),
                    ),
                )
            }
        }
        _ => ScriptedResponse::status(403),
    })
    .await
}

async fn seed(pool: &sqlx::PgPool, guild: &str, name: &str, enabled: bool) {
    store::put_command(
        pool,
        &StoredCommand {
            guild_id: guild.to_owned(),
            name: name.to_owned(),
            description: DESCRIPTION.to_owned(),
            template: ORIGINAL.to_owned(),
            text_trigger: Some(format!("!{name}")),
            enabled,
        },
        "111",
        "222",
        "2026-10-01T00:00:00Z",
        "2026-10-02T00:00:00Z",
    )
    .await
    .expect("seed complete definition with distinct creator/editor provenance");
}

async fn definitions(pool: &sqlx::PgPool) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(c) FROM automation_commands c ORDER BY guild_id, name")
        .fetch_all(pool)
        .await
        .expect("snapshot every definition field")
}

async fn facts(pool: &sqlx::PgPool) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(a) FROM automation_audit_log a ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("snapshot content-free audit")
}

struct Harness {
    db: TestDatabase,
    peer: sqlx::PgPool,
    mock: MockRest,
    runtime: CustomCommandRuntime,
}

impl Harness {
    async fn new(publication_fails: bool) -> Self {
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create isolated migrated test DB");
        let peer = db
            .independent_pool()
            .await
            .expect("independent worker connection pool");
        let mock = discord(publication_fails).await;
        let runtime = runtime(db.pool().clone(), &mock, true);
        seed(db.pool(), "2222", "faq", true).await;
        seed(db.pool(), "2222", "sibling", true).await;
        seed(db.pool(), "7777", "faq", true).await;
        Self {
            db,
            peer,
            mock,
            runtime,
        }
    }

    async fn observe(&self, input: &Interaction) -> Observed {
        let start = self.mock.requests().len();
        let result = self
            .runtime
            .handle_interaction(input, Some("Test guild"))
            .await;
        Observed {
            result,
            requests: self.mock.requests()[start..].to_vec(),
            id: input.id.get(),
        }
    }

    async fn close(self, outcome: Result<(), Box<dyn Any + Send>>) {
        drop(self.runtime);
        self.peer.close().await;
        self.mock.shutdown().await;
        self.db
            .close()
            .await
            .expect("verify test database teardown, even on RED");
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }
}

async fn with_fixture<F>(publication_fails: bool, test: F)
where
    F: for<'a> FnOnce(&'a Harness) -> Pin<Box<dyn Future<Output = ()> + 'a>>,
{
    let h = Harness::new(publication_fails).await;
    let outcome = AssertUnwindSafe(test(&h)).catch_unwind().await;
    h.close(outcome).await;
}

struct Observed {
    result: Result<bool, CustomCommandError>,
    requests: Vec<RestRequest>,
    id: u64,
}

fn publications(requests: &[RestRequest]) -> Vec<Vec<Value>> {
    requests
        .iter()
        .filter(|request| request.method == "PUT")
        .map(|request| {
            assert!(request
                .path
                .ends_with("/applications/1111/guilds/2222/commands"));
            serde_json::from_slice(&request.body).expect("complete guild registry")
        })
        .collect()
}

fn no_mentions(body: &Value) {
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    for field in ["users", "roles"] {
        assert!(
            body["allowed_mentions"][field].is_null()
                || body["allowed_mentions"][field] == json!([])
        );
    }
    assert_ne!(body["allowed_mentions"]["replied_user"], true);
}

fn private_reply(requests: &[RestRequest], id: u64) -> String {
    let callback = format!("/interactions/{id}/edit-fixture-{id}/callback");
    let completion = format!("/webhooks/1111/edit-fixture-{id}/messages/@original");
    assert!(
        requests.iter().all(|request| {
            (request.method == "POST" && request.path.ends_with(&callback))
                || (request.method == "PATCH" && request.path.ends_with(&completion))
                || (request.method == "PUT"
                    && request
                        .path
                        .ends_with("/applications/1111/guilds/2222/commands"))
        }),
        "no channel send, followup, foreign-guild publication or unrelated REST: {requests:?}"
    );
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "POST")
        .collect();
    assert_eq!(callbacks.len(), 1, "exactly one private acknowledgement");
    let callback: Value = serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    assert_eq!(callback["data"]["flags"].as_u64().unwrap_or(0) & 64, 64);
    no_mentions(&callback["data"]);
    let completions: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "PATCH")
        .collect();
    let body = if callback["type"] == 5 {
        assert_eq!(completions.len(), 1, "defer completes once");
        serde_json::from_slice::<Value>(&completions[0].body).expect("completion JSON")
    } else {
        assert_eq!(callback["type"], 4);
        assert!(completions.is_empty());
        callback["data"].clone()
    };
    no_mentions(&body);
    assert!(body["embeds"].is_null() || body["embeds"] == json!([]));
    assert!(body["attachments"].is_null() || body["attachments"] == json!([]));
    let content = body["content"].as_str().expect("bounded text confirmation");
    assert!(!content.is_empty() && content.encode_utf16().count() <= 2000);
    content.to_owned()
}

fn owned_reply(observed: &Observed) -> String {
    assert!(
        matches!(observed.result, Ok(true)),
        "edit must own this interaction: {:?}",
        observed.result
    );
    private_reply(&observed.requests, observed.id)
}

fn row(rows: &[Value], guild: &str, name: &str) -> Value {
    rows.iter()
        .find(|row| row["guild_id"] == guild && row["name"] == name)
        .expect("saved command identity")
        .clone()
}

fn assert_patch(before: &[Value], after: &[Value], changes: &[(&str, Value)]) {
    assert_eq!(
        before.len(),
        after.len(),
        "editing never creates or renames a command"
    );
    let mut expected = before.to_vec();
    let target = expected
        .iter_mut()
        .find(|row| row["guild_id"] == "2222" && row["name"] == "faq")
        .expect("original identity");
    let actual = row(after, "2222", "faq");
    for (key, value) in changes {
        target[*key] = value.clone();
    }
    assert_eq!(actual["updated_by"], "3333", "editor, not creator, changes");
    assert_ne!(
        actual["updated_at"], target["updated_at"],
        "a real edit advances update provenance"
    );
    target["updated_by"] = actual["updated_by"].clone();
    target["updated_at"] = actual["updated_at"].clone();
    assert_eq!(
        expected, after,
        "all omitted fields, identity, sibling and foreign rows survive"
    );
}

fn assert_safe_facts(rows: &[Value]) {
    let encoded = serde_json::to_string(rows).expect("audit JSON");
    for sentinel in [
        "ORIGINAL_CONTENT_SENTINEL",
        "EDITED_CONTENT_SENTINEL",
        "@everyone",
        "<@3333>",
    ] {
        assert!(
            !encoded.contains(sentinel),
            "audit must not contain template/rendered content"
        );
    }
}

fn assert_saved(observed: &Observed) {
    let reply = owned_reply(observed).to_lowercase();
    assert!(
        reply.contains("saved") || reply.contains("updated") || reply.contains("edited"),
        "truthful saved result: {reply}"
    );
    assert_eq!(
        publications(&observed.requests).len(),
        1,
        "one full-set resynchronization"
    );
}

#[test]
fn edit_is_discoverable_typed_reserved_guild_management_not_replacement() {
    let router = InteractionRouter::new(gates(true));
    let definitions = router.publish_set(&[]).expect("complete registry");
    let wire = serde_json::to_value(publish_commands(&definitions)).expect("wire registry");
    let edit = wire
        .as_array()
        .expect("commands")
        .iter()
        .find(|command| command["name"] == EDIT)
        .expect("publish explicit PATCH editing rather than silently changing /command");
    assert_eq!(edit["default_member_permissions"], "32");
    assert!(
        !definitions
            .iter()
            .find(|definition| definition.name == EDIT)
            .expect("guild-only edit")
            .dm_permission
    );
    let options = edit["options"].as_array().expect("typed options");
    assert_eq!(
        options.len(),
        5,
        "no rename, enable or import policy in this slice"
    );
    for (name, kind, required, maximum) in [
        ("name", 3, true, Some(32)),
        ("template", 3, false, Some(2000)),
        ("description", 3, false, Some(100)),
        ("text-trigger", 3, false, Some(33)),
        ("clear-trigger", 5, false, None),
    ] {
        let option = options
            .iter()
            .find(|option| option["name"] == name)
            .expect("advertised patch field");
        assert_eq!(option["type"], kind, "{name}");
        assert_eq!(option["required"] == true, required, "{name}");
        if let Some(maximum) = maximum {
            assert_eq!(option["max_length"], maximum, "{name}");
        }
    }
    assert!(wire
        .as_array()
        .expect("commands")
        .iter()
        .any(|command| command["name"] == "command"));
    assert!(
        help_text(&definitions).contains("/command-edit"),
        "help advertises the usable workflow"
    );
    let permission = command_permission(EDIT).expect("management permission row");
    assert_eq!(permission.required_permissions, 32);
    assert_eq!(permission.surface, CommandSurface::BuiltinSlash);
    assert!(builtin_command_names().contains(EDIT));
    assert!(reserved_command_names().contains(EDIT));
    assert!(!InteractionRouter::new(gates(false))
        .publish_set(&[])
        .expect("gated registry")
        .iter()
        .any(|definition| definition.name == EDIT));
}

#[test]
fn control_existing_replacement_still_requires_a_template() {
    let definitions = InteractionRouter::new(gates(true))
        .publish_set(&[])
        .expect("registry");
    let wire = serde_json::to_value(publish_commands(&definitions)).expect("wire registry");
    let command = wire
        .as_array()
        .expect("commands")
        .iter()
        .find(|command| command["name"] == "command")
        .expect("replacement remains published");
    let options = command["options"].as_array().expect("replacement options");
    assert!(options
        .iter()
        .any(|option| option["name"] == "template" && option["required"] == true));
    assert!(!options
        .iter()
        .any(|option| option["name"] == "clear-trigger"));
}

#[tokio::test]
async fn template_only_edit_preserves_metadata_and_updates_slash_and_prefix_execution() {
    with_fixture(false, |h| {
        Box::pin(async move {
            let before = definitions(h.db.pool()).await;
            let observed = h
                .observe(&edit(100, vec![option("template", CHANGED)]))
                .await;
            assert_saved(&observed);
            assert_patch(
                &before,
                &definitions(h.db.pool()).await,
                &[("template", json!(CHANGED))],
            );
            let published = publications(&observed.requests).pop().expect("registry");
            let faq = published
                .iter()
                .find(|command| command["name"] == "faq")
                .expect("saved slash remains discoverable");
            assert_eq!(faq["description"], DESCRIPTION);
            for name in ["command", EDIT, "rank", "sibling"] {
                assert!(
                    published.iter().any(|command| command["name"] == name),
                    "full-set publication retains {name}"
                );
            }
            assert!(!published
                .iter()
                .any(|command| command["name"] == "foreign-only"));
            let edit_facts = facts(h.db.pool()).await;
            assert_eq!(edit_facts.len(), 1, "one committed management fact");
            assert_eq!(edit_facts[0]["guild_id"], "2222");
            assert_eq!(edit_facts[0]["actor_id"], "3333");
            assert_eq!(edit_facts[0]["target_key"], "faq");
            assert_eq!(edit_facts[0]["outcome"], "ok");
            assert_safe_facts(&edit_facts);
            let slash = h.observe(&interaction(101, "faq", 0, vec![])).await;
            assert_eq!(owned_reply(&slash), "EDITED_CONTENT_SENTINEL tester");
            assert!(publications(&slash.requests).is_empty());
            let start = h.mock.requests().len();
            assert_eq!(
                h.runtime
                    .handle_message(
                        &message(102, "!faq"),
                        AutomationMessageAcceptance::Unmatched,
                        true,
                        Some("Test guild")
                    )
                    .await
                    .expect("prefix execution"),
                TextCommandOutcome::Delivered
            );
            let requests = h.mock.requests();
            let sent = &requests[start..];
            assert_eq!(sent.len(), 1, "one prefix send, no republish");
            assert!(sent[0].path.ends_with("/channels/4444/messages"));
            let body: Value = serde_json::from_slice(&sent[0].body).expect("message JSON");
            assert_eq!(body["content"], "EDITED_CONTENT_SENTINEL tester");
            no_mentions(&body);
            assert_safe_facts(&facts(h.db.pool()).await);
        })
    })
    .await;
}

#[tokio::test]
async fn description_only_edit_preserves_template_trigger_and_creator() {
    with_fixture(false, |h| {
        Box::pin(async move {
            let before = definitions(h.db.pool()).await;
            let observed = h
                .observe(&edit(
                    110,
                    vec![option("description", "A corrected discovery description")],
                ))
                .await;
            assert_saved(&observed);
            assert_patch(
                &before,
                &definitions(h.db.pool()).await,
                &[("description", json!("A corrected discovery description"))],
            );
            let published = publications(&observed.requests).pop().expect("registry");
            assert_eq!(
                published
                    .iter()
                    .find(|command| command["name"] == "faq")
                    .expect("FAQ")["description"],
                "A corrected discovery description"
            );
        })
    })
    .await;
}

#[tokio::test]
async fn trigger_only_edit_replaces_prefix_lookup_without_replacing_other_fields() {
    with_fixture(false, |h| {
        Box::pin(async move {
            let before = definitions(h.db.pool()).await;
            let observed = h
                .observe(&edit(120, vec![option("text-trigger", "!answer")]))
                .await;
            assert_saved(&observed);
            assert_patch(
                &before,
                &definitions(h.db.pool()).await,
                &[("text_trigger", json!("!answer"))],
            );
            assert!(store::find_text_trigger(h.db.pool(), "2222", "!faq")
                .await
                .expect("old lookup")
                .is_none());
            assert_eq!(
                store::find_text_trigger(h.db.pool(), "2222", "!answer")
                    .await
                    .expect("new lookup")
                    .expect("new trigger")
                    .name,
                "faq"
            );
            let start = h.mock.requests().len();
            assert_eq!(
                h.runtime
                    .handle_message(
                        &message(121, "!faq"),
                        AutomationMessageAcceptance::Unmatched,
                        true,
                        Some("Test guild")
                    )
                    .await
                    .expect("old prefix"),
                TextCommandOutcome::Ignored
            );
            assert_eq!(
                h.runtime
                    .handle_message(
                        &message(122, "!answer"),
                        AutomationMessageAcceptance::Unmatched,
                        true,
                        Some("Test guild")
                    )
                    .await
                    .expect("new prefix"),
                TextCommandOutcome::Delivered
            );
            assert_eq!(
                h.mock.requests().len() - start,
                1,
                "only the new trigger sends"
            );
        })
    })
    .await;
}

#[tokio::test]
async fn explicit_clear_removes_only_trigger_and_keeps_slash_response() {
    with_fixture(false, |h| {
        Box::pin(async move {
            let before = definitions(h.db.pool()).await;
            let observed = h.observe(&edit(130, vec![clear(true)])).await;
            assert_saved(&observed);
            assert_patch(
                &before,
                &definitions(h.db.pool()).await,
                &[("text_trigger", Value::Null)],
            );
            let start = h.mock.requests().len();
            assert_eq!(
                h.runtime
                    .handle_message(
                        &message(131, "!faq"),
                        AutomationMessageAcceptance::Unmatched,
                        true,
                        Some("Test guild")
                    )
                    .await
                    .expect("cleared prefix"),
                TextCommandOutcome::Ignored
            );
            assert_eq!(h.mock.requests().len(), start);
            let slash = h.observe(&interaction(132, "faq", 0, vec![])).await;
            assert_eq!(owned_reply(&slash), "ORIGINAL_CONTENT_SENTINEL tester");
            let cleared = definitions(h.db.pool()).await;
            let repeated = h.observe(&edit(133, vec![clear(true)])).await;
            let reply = owned_reply(&repeated).to_lowercase();
            assert!(
                ["unchanged", "no change", "no-op", "already"]
                    .iter()
                    .any(|word| reply.contains(word)),
                "clearing an absent trigger is a no-op: {reply}"
            );
            assert!(publications(&repeated.requests).is_empty());
            assert_eq!(cleared, definitions(h.db.pool()).await);
        })
    })
    .await;
}

#[tokio::test]
async fn hostile_saved_text_never_pings_via_confirmation_or_leaks_into_audit() {
    with_fixture(false, |h| {
        Box::pin(async move {
            let hostile = "EDITED_CONTENT_SENTINEL @everyone <@3333> {username}";
            let before = definitions(h.db.pool()).await;
            let observed = h
                .observe(&edit(
                    135,
                    vec![
                        option("template", hostile),
                        option("description", "@everyone saved response"),
                    ],
                ))
                .await;
            assert_saved(&observed);
            assert_patch(
                &before,
                &definitions(h.db.pool()).await,
                &[
                    ("template", json!(hostile)),
                    ("description", json!("@everyone saved response")),
                ],
            );
            assert_safe_facts(&facts(h.db.pool()).await);
        })
    })
    .await;
}

#[tokio::test]
async fn supplied_fields_replace_together_and_false_clear_does_not_clear() {
    with_fixture(false, |h| {
        Box::pin(async move {
            let before = definitions(h.db.pool()).await;
            let observed = h
                .observe(&edit(
                    140,
                    vec![
                        option("template", CHANGED),
                        option("description", "Replacement description"),
                        option("text-trigger", "!answer"),
                        clear(false),
                    ],
                ))
                .await;
            assert_saved(&observed);
            assert_patch(
                &before,
                &definitions(h.db.pool()).await,
                &[
                    ("template", json!(CHANGED)),
                    ("description", json!("Replacement description")),
                    ("text_trigger", json!("!answer")),
                ],
            );
        })
    })
    .await;
}

#[tokio::test]
async fn disabled_row_stays_disabled_and_is_not_republished_as_an_active_slash() {
    with_fixture(false, |h| Box::pin(async move {
        sqlx::query("UPDATE automation_commands SET enabled = false WHERE guild_id = '2222' AND name = 'faq'")
            .execute(h.db.pool()).await.expect("fixture disable");
        let before = definitions(h.db.pool()).await;
        let observed = h.observe(&edit(150, vec![option("template", CHANGED)])).await;
        assert_saved(&observed);
        assert_patch(&before, &definitions(h.db.pool()).await, &[("template", json!(CHANGED))]);
        assert!(!publications(&observed.requests)[0].iter().any(|command| command["name"] == "faq"));
        assert!(store::find_text_trigger(h.db.pool(), "2222", "!faq").await.expect("disabled lookup").is_none());
    })).await;
}

#[tokio::test]
async fn same_values_empty_patch_and_false_clear_are_truthful_noops() {
    with_fixture(false, |h| {
        Box::pin(async move {
            let before = definitions(h.db.pool()).await;
            for (i, options) in [
                vec![option("template", ORIGINAL)],
                vec![
                    option("description", DESCRIPTION),
                    option("text-trigger", "!faq"),
                ],
                vec![clear(false)],
                vec![],
            ]
            .into_iter()
            .enumerate()
            {
                let observed = h.observe(&edit(160 + i as u64, options)).await;
                let reply = owned_reply(&observed).to_lowercase();
                assert!(
                    ["unchanged", "no change", "no-op", "already"]
                        .iter()
                        .any(|word| reply.contains(word)),
                    "report a no-op, not a fabricated update: {reply}"
                );
                assert!(publications(&observed.requests).is_empty());
                assert_eq!(
                    before,
                    definitions(h.db.pool()).await,
                    "no-op must not churn update provenance"
                );
            }
            assert_safe_facts(&facts(h.db.pool()).await);
        })
    })
    .await;
}

#[tokio::test]
async fn malformed_edits_are_owned_refusals_with_no_definition_mutation_or_publication() {
    with_fixture(false, |h| {
        Box::pin(async move {
            let cases = vec![
                (
                    "set and clear",
                    vec![
                        option("name", "faq"),
                        option("text-trigger", "!answer"),
                        clear(true),
                    ],
                ),
                (
                    "duplicate name",
                    vec![
                        option("name", "faq"),
                        option("name", "sibling"),
                        option("template", CHANGED),
                    ],
                ),
                (
                    "duplicate template",
                    vec![
                        option("name", "faq"),
                        option("template", CHANGED),
                        option("template", "other"),
                    ],
                ),
                (
                    "duplicate description",
                    vec![
                        option("name", "faq"),
                        option("description", "one"),
                        option("description", "two"),
                    ],
                ),
                (
                    "duplicate trigger",
                    vec![
                        option("name", "faq"),
                        option("text-trigger", "!one"),
                        option("text-trigger", "!two"),
                    ],
                ),
                (
                    "duplicate clear",
                    vec![option("name", "faq"), clear(false), clear(true)],
                ),
                (
                    "wrong name type",
                    vec![
                        json!({"name":"name","type":5,"value":true}),
                        option("template", CHANGED),
                    ],
                ),
                (
                    "wrong template type",
                    vec![
                        option("name", "faq"),
                        json!({"name":"template","type":4,"value":7}),
                    ],
                ),
                (
                    "wrong description type",
                    vec![
                        option("name", "faq"),
                        json!({"name":"description","type":5,"value":true}),
                    ],
                ),
                (
                    "wrong trigger type",
                    vec![
                        option("name", "faq"),
                        json!({"name":"text-trigger","type":4,"value":7}),
                    ],
                ),
                (
                    "wrong clear type",
                    vec![option("name", "faq"), option("clear-trigger", "true")],
                ),
                (
                    "unknown option",
                    vec![
                        option("name", "faq"),
                        option("enabled", "true"),
                        option("template", CHANGED),
                    ],
                ),
                ("missing name", vec![option("template", CHANGED)]),
                (
                    "empty name",
                    vec![option("name", ""), option("template", CHANGED)],
                ),
                (
                    "invalid name",
                    vec![option("name", "bad.name"), option("template", CHANGED)],
                ),
                (
                    "overlong name",
                    vec![option("name", &"a".repeat(33)), option("template", CHANGED)],
                ),
                (
                    "reserved name",
                    vec![option("name", "rank"), option("template", CHANGED)],
                ),
                (
                    "own reserved name",
                    vec![option("name", EDIT), option("template", CHANGED)],
                ),
                (
                    "empty description",
                    vec![option("name", "faq"), option("description", "")],
                ),
                (
                    "overlong description",
                    vec![
                        option("name", "faq"),
                        option("description", &"a".repeat(101)),
                    ],
                ),
                (
                    "empty template",
                    vec![option("name", "faq"), option("template", "")],
                ),
                (
                    "unknown template variable",
                    vec![
                        option("name", "faq"),
                        option("template", "EDITED_CONTENT_SENTINEL {args}"),
                    ],
                ),
                (
                    "overlong template",
                    vec![option("name", "faq"), option("template", &"x".repeat(2001))],
                ),
                (
                    "UTF16 template limit",
                    vec![
                        option("name", "faq"),
                        option("template", &"😀".repeat(1001)),
                    ],
                ),
                (
                    "empty trigger is not clear",
                    vec![option("name", "faq"), option("text-trigger", "")],
                ),
                (
                    "invalid trigger",
                    vec![option("name", "faq"), option("text-trigger", "!bad.name")],
                ),
                (
                    "builtin trigger",
                    vec![option("name", "faq"), option("text-trigger", "!rank")],
                ),
                (
                    "overlong trigger",
                    vec![
                        option("name", "faq"),
                        option("text-trigger", &format!("!{}", "a".repeat(33))),
                    ],
                ),
                (
                    "trigger conflict",
                    vec![option("name", "faq"), option("text-trigger", "!sibling")],
                ),
                (
                    "absent command",
                    vec![option("name", "missing"), option("template", CHANGED)],
                ),
                (
                    "foreign-only command",
                    vec![option("name", "foreign-only"), option("template", CHANGED)],
                ),
            ];
            seed(h.db.pool(), "7777", "foreign-only", true).await;
            let before = definitions(h.db.pool()).await;
            for (i, (label, options)) in cases.into_iter().enumerate() {
                let observed = h
                    .observe(&interaction(200 + i as u64, EDIT, 32, options))
                    .await;
                let reply = owned_reply(&observed).to_lowercase();
                assert!(
                    !reply.contains("temporarily unavailable"),
                    "{label}: validation/miss is not an outage"
                );
                assert!(
                    !reply.starts_with("saved") && !reply.starts_with("updated"),
                    "{label}: must not announce a save"
                );
                assert!(
                    publications(&observed.requests).is_empty(),
                    "{label}: no registry mutation"
                );
                assert_eq!(
                    before,
                    definitions(h.db.pool()).await,
                    "{label}: no partial or cross-guild definition mutation"
                );
            }
            assert_safe_facts(&facts(h.db.pool()).await);
        })
    })
    .await;
}

#[tokio::test]
async fn a_trigger_held_by_a_disabled_sibling_is_still_a_conflict() {
    with_fixture(false, |h| Box::pin(async move {
        sqlx::query("UPDATE automation_commands SET enabled = false WHERE guild_id = '2222' AND name = 'sibling'")
            .execute(h.db.pool()).await.expect("disabled sibling fixture");
        let before = definitions(h.db.pool()).await;
        let observed = h.observe(&edit(250, vec![option("template", CHANGED), option("text-trigger", "!sibling")])).await;
        owned_reply(&observed);
        assert_eq!(before, definitions(h.db.pool()).await, "conflict cannot partially change template");
        assert!(publications(&observed.requests).is_empty());
    })).await;
}

#[tokio::test]
async fn final_complete_definition_is_validated_even_when_invalid_template_was_omitted() {
    with_fixture(false, |h| Box::pin(async move {
        for (i, invalid) in ["ORIGINAL_CONTENT_SENTINEL {args}".to_owned(), "😀".repeat(1001)].into_iter().enumerate() {
            sqlx::query("UPDATE automation_commands SET template = $1 WHERE guild_id = '2222' AND name = 'faq'")
                .bind(invalid).execute(h.db.pool()).await.expect("imported shape-valid but domain-invalid template");
            let before = definitions(h.db.pool()).await;
            let observed = h.observe(&edit(260 + i as u64, vec![option("description", "Must not save")])).await;
            owned_reply(&observed);
            assert_eq!(before, definitions(h.db.pool()).await, "validate merged definition, not just supplied fields");
            assert!(publications(&observed.requests).is_empty());
        }
    })).await;
}

#[tokio::test]
async fn management_authority_and_automation_gates_precede_even_malformed_patch_lookup() {
    let pool = PgPoolOptions::new().connect_lazy_with(
        PgConnectOptions::new()
            .host("agent-testdb")
            .username("agent_test")
            .password(""),
    );
    pool.close().await;
    for (enabled, permissions, expected) in [
        (true, 0, "Manage Server"),
        (true, 8, "Manage Server"),
        (false, 32, "Automations are disabled"),
    ] {
        let mock = discord(false).await;
        let service = runtime(pool.clone(), &mock, enabled);
        let input = interaction(
            300,
            EDIT,
            permissions,
            vec![
                option("name", "faq"),
                option("template", CHANGED),
                clear(true),
                option("text-trigger", "!answer"),
            ],
        );
        let result = service.handle_interaction(&input, Some("Test guild")).await;
        let observed = Observed {
            result,
            requests: mock.requests(),
            id: 300,
        };
        mock.shutdown().await;
        assert!(
            owned_reply(&observed).contains(expected),
            "authority/gate before parsing or closed store"
        );
        assert!(publications(&observed.requests).is_empty());
    }
}

#[tokio::test]
async fn control_wrong_guild_dm_and_wrong_application_are_silent_without_store_access() {
    let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new().host("agent-testdb"));
    pool.close().await;
    let mock = discord(false).await;
    let service = runtime(pool, &mock, true);
    let mut outcomes = Vec::new();
    for scope in ["guild", "dm", "application"] {
        let mut input = edit(310, vec![option("template", CHANGED)]);
        match scope {
            "guild" => input.guild_id = Some(Id::new(7777)),
            "dm" => input.guild_id = None,
            "application" => input.application_id = Id::new(9999),
            _ => unreachable!(),
        }
        outcomes.push(service.handle_interaction(&input, Some("Test guild")).await);
    }
    let requests = mock.requests();
    mock.shutdown().await;
    assert!(outcomes.iter().all(|result| matches!(result, Ok(false))));
    assert!(
        requests.is_empty(),
        "unowned scopes cannot defer, publish or send"
    );
}

#[tokio::test]
async fn lookup_outage_is_private_unavailability_not_a_miss_or_success() {
    let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new().host("agent-testdb"));
    pool.close().await;
    let mock = discord(false).await;
    let service = runtime(pool, &mock, true);
    let result = service
        .handle_interaction(
            &edit(320, vec![option("template", CHANGED)]),
            Some("Test guild"),
        )
        .await;
    let requests = mock.requests();
    mock.shutdown().await;
    assert!(
        !matches!(result, Ok(false)),
        "recognized editing outage is not an unknown interaction"
    );
    let content = private_reply(&requests, 320).to_lowercase();
    assert!(
        content.contains("unavailable") || content.contains("failed"),
        "honest outage: {content}"
    );
    assert!(
        !content.contains("not found")
            && !content.contains("does not exist")
            && !content.contains("saved")
    );
    assert!(publications(&requests).is_empty());
    assert!(
        !content.contains("pool") && !content.contains("sql") && !content.contains("postgres"),
        "no raw storage errors"
    );
}

#[tokio::test]
async fn store_failure_rolls_back_definition_and_success_audit_before_publication() {
    with_fixture(false, |h| Box::pin(async move {
        sqlx::query("ALTER TABLE automation_commands ADD CONSTRAINT acceptance_store_refusal CHECK (template <> 'EDITED_CONTENT_SENTINEL {username}')")
            .execute(h.db.pool()).await.expect("fixture write failure");
        let before = definitions(h.db.pool()).await;
        let observed = h.observe(&edit(330, vec![option("template", CHANGED), option("description", "Must roll back")])).await;
        assert!(!matches!(observed.result, Ok(false)), "owns storage failure");
        let reply = private_reply(&observed.requests, observed.id).to_lowercase();
        assert!(!reply.starts_with("saved") && !reply.starts_with("updated"));
        assert_eq!(before, definitions(h.db.pool()).await);
        assert!(publications(&observed.requests).is_empty());
        let audit = facts(h.db.pool()).await;
        assert!(!audit.iter().any(|row| row["outcome"] == "ok"), "no committed success for a failed write");
        assert_safe_facts(&audit);
    })).await;
}

#[tokio::test]
async fn audit_failure_rolls_back_the_entire_edit_and_does_not_publish() {
    with_fixture(false, |h| Box::pin(async move {
        sqlx::query("ALTER TABLE automation_audit_log ADD CONSTRAINT acceptance_audit_refusal CHECK (outcome <> 'ok')")
            .execute(h.db.pool()).await.expect("fixture failure of successful management audit");
        let before = definitions(h.db.pool()).await;
        let observed = h.observe(&edit(340, vec![option("template", CHANGED), clear(true)])).await;
        assert!(!matches!(observed.result, Ok(false)), "owns audit failure");
        let reply = private_reply(&observed.requests, observed.id).to_lowercase();
        assert!(!reply.starts_with("saved") && !reply.starts_with("updated"));
        assert_eq!(before, definitions(h.db.pool()).await, "definition and audit commit atomically");
        assert!(publications(&observed.requests).is_empty());
        assert_safe_facts(&facts(h.db.pool()).await);
    })).await;
}

#[tokio::test]
async fn publication_failure_leaves_saved_patch_but_never_claims_synchronized_success() {
    with_fixture(true, |h| {
        Box::pin(async move {
            let before = definitions(h.db.pool()).await;
            let observed = h
                .observe(&edit(350, vec![option("template", CHANGED)]))
                .await;
            let reply = owned_reply(&observed).to_lowercase();
            assert_patch(
                &before,
                &definitions(h.db.pool()).await,
                &[("template", json!(CHANGED))],
            );
            assert!(
                reply.contains("saved") || reply.contains("updated"),
                "definition was committed: {reply}"
            );
            assert!(
                reply.contains("failed")
                    && (reply.contains("publication") || reply.contains("sync")),
                "Discord state is not confirmed: {reply}"
            );
            assert_eq!(
                publications(&observed.requests).len(),
                1,
                "no blind publication retries"
            );
            assert_safe_facts(&facts(h.db.pool()).await);
        })
    })
    .await;
}

#[tokio::test]
async fn independent_workers_preserve_both_disjoint_edits() {
    with_fixture(false, |h| {
        Box::pin(async move {
            let before = definitions(h.db.pool()).await;
            let peer = runtime(h.peer.clone(), &h.mock, true);
            let first = edit(360, vec![option("template", CHANGED)]);
            let second = edit(361, vec![option("description", "Concurrent description")]);
            let (one, two) = tokio::join!(
                h.runtime.handle_interaction(&first, Some("Test guild")),
                peer.handle_interaction(&second, Some("Test guild")),
            );
            assert!(
                matches!(one, Ok(true)) && matches!(two, Ok(true)),
                "both real workers own edits: {one:?}, {two:?}"
            );
            assert_patch(
                &before,
                &definitions(h.db.pool()).await,
                &[
                    ("template", json!(CHANGED)),
                    ("description", json!("Concurrent description")),
                ],
            );
            let requests = h.mock.requests();
            assert_eq!(publications(&requests).len(), 2);
            for id in [360, 361] {
                let token = format!("edit-fixture-{id}");
                let replies: Vec<_> = requests
                    .iter()
                    .filter(|request| request.path.contains(&token))
                    .cloned()
                    .collect();
                private_reply(&replies, id);
            }
            let audit = facts(h.db.pool()).await;
            assert_eq!(audit.iter().filter(|row| row["outcome"] == "ok").count(), 2);
            assert_safe_facts(&audit);
        })
    })
    .await;
}

#[tokio::test]
async fn contended_guild_lock_rereads_metadata_and_never_reactivates_a_concurrent_disable() {
    with_fixture(false, |h| Box::pin(async move {
        let mut blocker = h.peer.begin().await.expect("independent transaction");
        store::lock_command_capacity(&mut blocker, "2222").await.expect("existing guild mutation lock");
        let input = edit(370, vec![option("template", CHANGED)]);
        let pending = h.runtime.handle_interaction(&input, Some("Test guild"));
        tokio::pin!(pending);
        let waiting = async {
            loop {
                let blocked: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND NOT granted AND database = (SELECT oid FROM pg_database WHERE datname = current_database()))")
                    .fetch_one(&h.peer).await.expect("observe contended guild lock");
                if blocked { break; }
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            result = &mut pending => panic!("edit completed without acquiring the held guild lock: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(2), waiting) => result.expect("edit must wait on the existing guild lock"),
        }
        sqlx::query("UPDATE automation_commands SET description = 'Concurrent committed description', enabled = false, updated_by = '444', updated_at = '2026-10-24T00:00:00Z' WHERE guild_id = '2222' AND name = 'faq'")
            .execute(&mut *blocker).await.expect("concurrent lock holder changes unrelated fields");
        blocker.commit().await.expect("release guild lock with new metadata");
        let before_edit = definitions(&h.peer).await;
        let result = pending.await;
        assert!(matches!(result, Ok(true)), "waiting edit must succeed without replacement: {result:?}");
        private_reply(&h.mock.requests(), 370);
        assert_patch(&before_edit, &definitions(h.db.pool()).await, &[("template", json!(CHANGED))]);
        assert!(!publications(&h.mock.requests())[0].iter().any(|command| command["name"] == "faq"));
    })).await;
}

#[tokio::test]
async fn control_existing_command_keeps_full_replacement_defaults_not_patch_semantics() {
    with_fixture(false, |h| {
        Box::pin(async move {
            let before = definitions(h.db.pool()).await;
            let observed = h
                .observe(&interaction(
                    380,
                    "command",
                    32,
                    vec![option("name", "faq"), option("template", CHANGED)],
                ))
                .await;
            assert_saved(&observed);
            assert_patch(
                &before,
                &definitions(h.db.pool()).await,
                &[
                    ("template", json!(CHANGED)),
                    ("description", json!("Custom command")),
                    ("text_trigger", Value::Null),
                ],
            );
            assert_safe_facts(&facts(h.db.pool()).await);
        })
    })
    .await;
}
