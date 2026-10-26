#![cfg(feature = "db")]

//! Protected acceptance for the existing replacement/removal writers, not PATCH.
//! Real Twilight interaction -> management runtime -> store/executor dispatch.
//! Pool hooks observe every acquisition/reconnect; snapshots use a separate pool.
//! Only Discord HTTP is doubled. Missing DB configuration is not DB evidence.

#[allow(dead_code)]
mod common;

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};
use twilight_model::{application::interaction::Interaction, id::Id};
use two_bot_core::{
    custom_command_store as store, custom_commands::StoredCommand, InteractionRouter, RouterGates,
};
use two_bot_discord::{
    custom_commands::{CustomCommandError, CustomCommandRuntime},
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

const TOKEN: &str = "writer-shapes-fixture";
const BODY: &str = "BODY_SENTINEL @everyone <@&9876> https://untrusted.invalid {user}";
const HOSTILE: &str = "UNTRUSTED_SENTINEL @everyone <@9876> https://untrusted.invalid";
const AT: &str = "2026-10-01T00:00:00.000000+00:00";

fn router(enabled: bool) -> InteractionRouter {
    let mut router = InteractionRouter::new(RouterGates {
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
    });
    CustomCommandRuntime::register(&mut router);
    router
}

fn runtime(pool: PgPool, mock: &MockRest, enabled: bool) -> CustomCommandRuntime {
    CustomCommandRuntime::new(
        pool,
        Arc::new(router(enabled)),
        ActionExecutor::with_proxy(TOKEN.to_owned(), Some(mock.origin()))
            .expect("loopback-only executor"),
        1111,
    )
}

fn option(name: &str, value: &str) -> Value {
    json!({"name": name, "type": 3, "value": value})
}

fn interaction(id: u64, command: &str, options: Vec<Value>) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": TOKEN, "version": 1, "guild_id": "2222",
        "channel": {"id": "4444", "type": 0, "name": "commands"},
        "authorizing_integration_owners": {"0": "2222"}, "entitlements": [],
        "member": {
            "permissions": "32", "roles": [], "joined_at": null,
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null}
        },
        "data": {"id": "5555", "name": command, "type": 1, "options": options}
    }))
    .expect("forged but deserializable Twilight interaction")
}

fn valid_options(command: &str) -> Vec<Value> {
    if command == "command" {
        vec![
            option("name", "FAQ"),
            option("template", BODY),
            option("description", "New description"),
            option("text-trigger", "!NewFaq"),
        ]
    } else {
        vec![option("name", "FAQ")]
    }
}

#[test]
fn published_writer_options_remain_replacement_not_patch_and_guild_only() {
    let defs = router(true).publish_set(&[]).expect("compiled registry");
    let wire = serde_json::to_value(two_bot_discord::interactions::publish_commands(&defs))
        .expect("Discord registry payload");
    for (name, expected) in [
        (
            "command",
            vec![
                ("name", true, 32),
                ("template", true, 2000),
                ("description", false, 100),
                ("text-trigger", false, 33),
            ],
        ),
        ("command-remove", vec![("name", true, 32)]),
    ] {
        let def = defs
            .iter()
            .find(|def| def.name == name)
            .expect("writer registered");
        assert!(!def.dm_permission);
        let command = wire
            .as_array()
            .expect("commands")
            .iter()
            .find(|command| command["name"] == name)
            .expect("published writer");
        assert_eq!(command["default_member_permissions"], "32");
        let options = command["options"].as_array().expect("writer options");
        assert_eq!(
            options.len(),
            expected.len(),
            "no PATCH flags or extra surface"
        );
        for (field, required, bound) in expected {
            let option = options
                .iter()
                .find(|option| option["name"] == field)
                .expect("option");
            assert_eq!(option["type"], 3);
            assert_eq!(option["required"].as_bool().unwrap_or(false), required);
            assert_eq!(option["max_length"], bound);
        }
        assert!(!router(false)
            .publish_set(&[])
            .expect("gated registry")
            .iter()
            .any(|def| def.name == name));
    }
}

struct Case {
    label: String,
    option: &'static str,
    options: Vec<Value>,
}

fn malformed(command: &str) -> Vec<Case> {
    let fields: &[&str] = if command == "command" {
        &["name", "template", "description", "text-trigger"]
    } else {
        &["name"]
    };
    let mut cases = Vec::new();
    for &field in fields {
        let valid = valid_options(command);
        let original = valid
            .iter()
            .find(|value| value["name"] == field)
            .expect("field present")
            .clone();
        let without: Vec<_> = valid
            .into_iter()
            .filter(|value| value["name"] != field)
            .collect();
        let integer = json!({"name": field, "type": 4, "value": 17});
        let boolean = json!({"name": field, "type": 5, "value": false});
        let nested = json!({"name": field, "type": 1, "options": [option("name", HOSTILE)]});
        for (label, replacement) in [
            (
                "duplicate-identical",
                vec![original.clone(), original.clone()],
            ),
            (
                "duplicate-conflicting",
                vec![original.clone(), option(field, HOSTILE)],
            ),
            ("integer", vec![integer.clone()]),
            ("boolean", vec![boolean.clone()]),
            ("nested", vec![nested.clone()]),
            (
                "string-then-integer",
                vec![original.clone(), integer.clone()],
            ),
            ("integer-then-string", vec![integer, original.clone()]),
            (
                "string-then-boolean",
                vec![original.clone(), boolean.clone()],
            ),
            ("boolean-then-string", vec![boolean, original.clone()]),
            ("string-then-nested", vec![original.clone(), nested.clone()]),
            ("nested-then-string", vec![nested, original]),
        ] {
            let mut options = without.clone();
            options.extend(replacement);
            cases.push(Case {
                label: format!("{field}/{label}"),
                option: field,
                options,
            });
        }
        if field == "name" || field == "template" {
            cases.push(Case {
                label: format!("{field}/missing"),
                option: field,
                options: without,
            });
        }
    }
    for extra in [
        option("unexpected", HOSTILE),
        option(HOSTILE, HOSTILE),
        json!({"name": "unexpected", "type": 2, "options": [
            {"name": "nested", "type": 1, "options": [option("template", BODY)]}
        ]}),
    ] {
        for first in [false, true] {
            let mut options = valid_options(command);
            options.insert(if first { 0 } else { options.len() }, extra.clone());
            cases.push(Case {
                label: format!("unknown/{}/{first}", cases.len()),
                option: "option",
                options,
            });
        }
    }
    if command == "command-remove" {
        for extra in ["template", "description", "text-trigger"] {
            let mut options = valid_options(command);
            options.push(option(extra, HOSTILE));
            cases.push(Case {
                label: format!("define-only/{extra}"),
                option: "option",
                options,
            });
        }
    }
    cases
}

async fn discord(publish_fails: Arc<AtomicBool>) -> MockRest {
    MockRest::with_responder(move |request| match request.method.as_str() {
        "POST" if request.path.ends_with("/callback") => ScriptedResponse::status(204),
        "PATCH" if request.path.ends_with("/messages/@original") => {
            ScriptedResponse::json(200, json!({"id": "9000"}))
        }
        "PUT" if request.path == "/api/v10/applications/1111/guilds/2222/commands" => {
            if publish_fails.load(Ordering::SeqCst) {
                return ScriptedResponse::status(403);
            }
            let mut receipt: Value = serde_json::from_slice(&request.body).expect("registry JSON");
            for (index, row) in receipt
                .as_array_mut()
                .expect("full-set registry")
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
    })
    .await
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

fn private_reply(observed: &Observed, allow_publication: bool) -> String {
    assert!(
        matches!(observed.result, Ok(true)),
        "management dispatch: {:?}",
        observed.result
    );
    let callback = format!("/interactions/{}/{TOKEN}/callback", observed.id);
    let edit = format!("/webhooks/1111/{TOKEN}/messages/@original");
    let replies: Vec<_> = observed
        .requests
        .iter()
        .filter(|request| {
            !(allow_publication
                && request.method == "PUT"
                && request.path == "/api/v10/applications/1111/guilds/2222/commands")
        })
        .collect();
    assert!(
        replies.iter().all(|request| {
            (request.method == "POST" && request.path.ends_with(&callback))
                || (request.method == "PATCH" && request.path.ends_with(&edit))
        }),
        "no follow-up, channel message, registry read or unexpected mutation"
    );
    let callbacks: Vec<_> = replies
        .iter()
        .filter(|request| request.method == "POST")
        .collect();
    assert_eq!(callbacks.len(), 1, "exactly one private acknowledgement");
    let ack: Value = serde_json::from_slice(&callbacks[0].body).expect("ack JSON");
    assert_eq!(ack["data"]["flags"].as_u64().unwrap_or(0) & 64, 64);
    let body = if ack["type"] == 5 {
        assert_eq!(replies.len(), 2, "one defer plus one completion");
        let edit = replies
            .iter()
            .find(|request| request.method == "PATCH")
            .expect("completion");
        serde_json::from_slice::<Value>(&edit.body).expect("completion JSON")
    } else {
        assert_eq!(ack["type"], 4);
        assert_eq!(replies.len(), 1);
        ack["data"].clone()
    };
    for payload in [&ack["data"], &body] {
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
        for key in ["users", "roles"] {
            assert!(
                payload["allowed_mentions"][key].is_null()
                    || payload["allowed_mentions"][key] == json!([])
            );
        }
        assert_ne!(payload["allowed_mentions"]["replied_user"], true);
    }
    assert!(body["embeds"].is_null() || body["embeds"] == json!([]));
    assert!(body["attachments"].is_null() || body["attachments"] == json!([]));
    let content = body["content"].as_str().expect("private text reply");
    assert!(!content.is_empty() && content.encode_utf16().count() <= 2000);
    content.to_owned()
}

fn correction(observed: &Observed, field: &str) {
    let content = private_reply(observed, false);
    let lower = content.to_lowercase();
    assert!(
        lower.contains(field),
        "correction must name {field}: {content}"
    );
    for forbidden in [
        "BODY_SENTINEL",
        "UNTRUSTED_SENTINEL",
        "@everyone",
        "<@",
        "https://untrusted.invalid",
        "sqlx",
        "automation_commands",
        "database",
        "storage",
        "unavailable",
        "Saved /",
        "Removed /",
    ] {
        assert!(
            !content.contains(forbidden),
            "shape refusal must not echo input or storage internals: {content}"
        );
    }
}

async fn closed_pool() -> PgPool {
    // No connection is opened; any accidental store path gets PoolClosed.
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .expect("synthetic lazy pool");
    pool.close().await;
    pool
}

async fn closed_shape_matrix(command: &str) {
    let mock = discord(Arc::new(AtomicBool::new(false))).await;
    let runtime = runtime(closed_pool().await, &mock, true);
    let mut failures = Vec::new();
    for (index, case) in malformed(command).into_iter().enumerate() {
        let observed = observe(
            &runtime,
            &mock,
            &interaction(1000 + index as u64, command, case.options),
        )
        .await;
        let check = std::panic::catch_unwind(|| correction(&observed, case.option));
        if check.is_err() {
            failures.push(case.label);
        }
    }
    mock.shutdown().await;
    assert!(
        failures.is_empty(),
        "/{command} failed shape refusals: {failures:?}"
    );
}

#[tokio::test]
async fn define_malformed_shapes_refuse_privately_before_store_access() {
    closed_shape_matrix("command").await;
}

#[tokio::test]
async fn remove_malformed_shapes_refuse_privately_before_store_access() {
    closed_shape_matrix("command-remove").await;
}

struct Fixture {
    db: TestDatabase,
    pool: PgPool,
    accesses: Arc<AtomicUsize>,
    mock: MockRest,
    runtime: CustomCommandRuntime,
    publish_fails: Arc<AtomicBool>,
}

impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit disposable test bootstrap required");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("isolated migrated test database");
        for (guild, name, enabled) in [
            ("2222", "faq", false),
            ("2222", "other", true),
            ("9999", "faq", true),
        ] {
            store::put_command(
                db.pool(),
                &StoredCommand {
                    guild_id: guild.to_owned(),
                    name: name.to_owned(),
                    description: "Saved description".to_owned(),
                    template: "Old template".to_owned(),
                    text_trigger: Some(format!("!{name}")),
                    enabled,
                },
                "66",
                "67",
                AT,
                AT,
            )
            .await
            .expect("seed real store");
        }
        let accesses = Arc::new(AtomicUsize::new(0));
        let acquired = Arc::clone(&accesses);
        let connected = Arc::clone(&accesses);
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .min_connections(0)
            .idle_timeout(None)
            .max_lifetime(None)
            .acquire_timeout(Duration::from_secs(10))
            .before_acquire(move |_, _| {
                acquired.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(true) })
            })
            .after_connect(move |_, _| {
                connected.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            })
            .connect_with(db.pool().connect_options().as_ref().clone())
            .await
            .expect("observed pool in the same disposable database");
        let publish_fails = Arc::new(AtomicBool::new(false));
        let mock = discord(Arc::clone(&publish_fails)).await;
        let runtime = runtime(pool.clone(), &mock, true);
        Self {
            db,
            pool,
            accesses,
            mock,
            runtime,
            publish_fails,
        }
    }

    async fn observe(&self, input: &Interaction) -> (Observed, usize) {
        self.accesses.store(0, Ordering::SeqCst);
        let observed = observe(&self.runtime, &self.mock, input).await;
        (observed, self.accesses.load(Ordering::SeqCst))
    }

    async fn close(self) {
        drop(self.runtime);
        self.pool.close().await;
        self.mock.shutdown().await;
        self.db
            .close()
            .await
            .expect("verified owned-database teardown");
    }
}

async fn snapshot(pool: &PgPool) -> (Vec<Value>, Vec<Value>) {
    let definitions =
        sqlx::query_scalar("SELECT to_jsonb(c) FROM automation_commands c ORDER BY guild_id, name")
            .fetch_all(pool)
            .await
            .expect("complete definition snapshot");
    let audits = sqlx::query_scalar("SELECT to_jsonb(a) FROM automation_audit_log a ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("complete audit snapshot");
    (definitions, audits)
}

fn faq(rows: &[Value]) -> &Value {
    rows.iter()
        .find(|row| row["guild_id"] == "2222" && row["name"] == "faq")
        .expect("saved faq")
}

fn publications(observed: &Observed) -> Vec<Value> {
    observed
        .requests
        .iter()
        .filter(|request| request.method == "PUT")
        .map(|request| serde_json::from_slice(&request.body).expect("registry JSON"))
        .collect()
}

#[tokio::test]
async fn malformed_writers_make_zero_pool_accesses_and_preserve_all_rows_and_audits() {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("DB acceptance not exercised: TWO_TEST_DATABASE_URL unset");
        return;
    }
    let f = Fixture::new().await;
    let mut failures = Vec::new();
    for (command_index, command) in ["command", "command-remove"].into_iter().enumerate() {
        for (index, case) in malformed(command).into_iter().enumerate() {
            let before = snapshot(f.db.pool()).await;
            let (observed, accesses) = f
                .observe(&interaction(
                    2000 + command_index as u64 * 100 + index as u64,
                    command,
                    case.options,
                ))
                .await;
            let after = snapshot(f.db.pool()).await;
            if accesses != 0 || before != after || !publications(&observed).is_empty() {
                failures.push(format!(
                    "/{command} {}: accesses={accesses}, rows_unchanged={}, publications={}",
                    case.label,
                    before == after,
                    publications(&observed).len()
                ));
            }
            if std::panic::catch_unwind(|| correction(&observed, case.option)).is_err() {
                failures.push(format!("/{command} {}: private correction", case.label));
            }
        }
    }
    f.close().await;
    assert!(
        failures.is_empty(),
        "zero-effect writer contract: {failures:#?}"
    );
}

#[tokio::test]
async fn healthy_default_and_full_replacements_keep_existing_semantics_and_remove_fences() {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("DB acceptance not exercised: TWO_TEST_DATABASE_URL unset");
        return;
    }
    let f = Fixture::new().await;
    let before = snapshot(f.db.pool()).await;
    let (full, full_accesses) = f
        .observe(&interaction(3000, "command", valid_options("command")))
        .await;
    let replaced = snapshot(f.db.pool()).await;
    let (defaulted, _) = f
        .observe(&interaction(
            3001,
            "command",
            vec![
                option("name", "FAQ"),
                option("template", "Default replacement"),
            ],
        ))
        .await;
    let default_rows = snapshot(f.db.pool()).await;
    let (created, _) = f
        .observe(&interaction(
            3002,
            "command",
            vec![
                option("name", "NewOne"),
                option("template", "Created response"),
            ],
        ))
        .await;
    let created_rows = snapshot(f.db.pool()).await;
    let (removed, remove_accesses) = f
        .observe(&interaction(
            3003,
            "command-remove",
            valid_options("command-remove"),
        ))
        .await;
    let removed_rows = snapshot(f.db.pool()).await;
    let (absent, _) = f
        .observe(&interaction(
            3004,
            "command-remove",
            valid_options("command-remove"),
        ))
        .await;
    let absent_rows = snapshot(f.db.pool()).await;
    f.close().await;

    assert_eq!(private_reply(&full, true), "Saved /faq.");
    assert!(
        full_accesses > 0 && remove_accesses > 0,
        "observer detects healthy store paths"
    );
    assert_eq!(faq(&replaced.0)["template"], BODY);
    assert_eq!(faq(&replaced.0)["description"], "New description");
    assert_eq!(faq(&replaced.0)["text_trigger"], "!newfaq");
    assert_eq!(
        faq(&replaced.0)["enabled"],
        true,
        "replacement still enables a paused row"
    );
    for rows in [&replaced.0, &default_rows.0] {
        assert_eq!(faq(rows)["created_by"], faq(&before.0)["created_by"]);
        assert_eq!(faq(rows)["created_at"], faq(&before.0)["created_at"]);
        assert_eq!(faq(rows)["updated_by"], "3333");
    }
    assert_eq!(private_reply(&defaulted, true), "Saved /faq.");
    assert_eq!(faq(&default_rows.0)["description"], "Custom command");
    assert_eq!(faq(&default_rows.0)["template"], "Default replacement");
    assert!(
        faq(&default_rows.0)["text_trigger"].is_null(),
        "omission clears trigger, not PATCH"
    );
    assert_eq!(private_reply(&created, true), "Saved /newone.");
    assert!(created_rows
        .0
        .iter()
        .any(|row| row["guild_id"] == "2222" && row["name"] == "newone" && row["enabled"] == true));
    assert_eq!(private_reply(&removed, true), "Removed /faq.");
    assert!(!removed_rows
        .0
        .iter()
        .any(|row| row["guild_id"] == "2222" && row["name"] == "faq"));
    for preserved in before
        .0
        .iter()
        .filter(|row| row["guild_id"] != "2222" || row["name"] != "faq")
    {
        assert!(
            removed_rows.0.contains(preserved),
            "other names and guilds unchanged"
        );
    }
    assert_eq!(private_reply(&absent, true), "No command /faq found.");
    assert_eq!(absent_rows.0, removed_rows.0);
    assert!(publications(&absent).is_empty());
    for observed in [&full, &defaulted, &created, &removed] {
        let sets = publications(observed);
        assert_eq!(sets.len(), 1, "one full-set publication after a mutation");
        assert!(sets[0]
            .as_array()
            .expect("full set")
            .iter()
            .any(|row| row["name"] == "help"));
        assert!(sets[0]
            .as_array()
            .expect("full set")
            .iter()
            .any(|row| row["name"] == "other"));
    }
    assert_eq!(replaced.1.len(), before.1.len() + 1);
    assert_eq!(default_rows.1.len(), replaced.1.len() + 1);
    assert_eq!(created_rows.1.len(), default_rows.1.len() + 1);
    assert_eq!(removed_rows.1.len(), created_rows.1.len() + 1);
}

#[tokio::test]
async fn healthy_writer_publication_failure_reports_saved_not_synchronized() {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("DB acceptance not exercised: TWO_TEST_DATABASE_URL unset");
        return;
    }
    let f = Fixture::new().await;
    f.publish_fails.store(true, Ordering::SeqCst);
    let (saved, _) = f
        .observe(&interaction(4000, "command", valid_options("command")))
        .await;
    let after_save = snapshot(f.db.pool()).await;
    let (removed, _) = f
        .observe(&interaction(
            4001,
            "command-remove",
            valid_options("command-remove"),
        ))
        .await;
    let after_remove = snapshot(f.db.pool()).await;
    f.close().await;
    for (observed, prefix) in [(&saved, "Saved /faq."), (&removed, "Removed /faq.")] {
        let reply = private_reply(observed, true);
        assert!(reply.starts_with(prefix));
        assert!(reply.contains("not yet synchronized"));
        assert_eq!(publications(observed).len(), 1);
    }
    assert_eq!(faq(&after_save.0)["template"], BODY);
    assert!(!after_remove
        .0
        .iter()
        .any(|row| row["guild_id"] == "2222" && row["name"] == "faq"));
}

#[tokio::test]
async fn authority_and_scope_controls_still_precede_writer_shape_validation() {
    let mock = discord(Arc::new(AtomicBool::new(false))).await;
    let pool = closed_pool().await;
    let enabled = runtime(pool.clone(), &mock, true);
    let disabled = runtime(pool, &mock, false);
    for (index, command) in ["command", "command-remove"].into_iter().enumerate() {
        let mut forbidden = interaction(
            5000 + index as u64 * 10,
            command,
            vec![option("name", HOSTILE)],
        );
        forbidden.member.as_mut().expect("member").permissions =
            Some(twilight_model::guild::Permissions::empty());
        let denied = observe(&enabled, &mock, &forbidden).await;
        let reply = private_reply(&denied, false);
        assert!(!reply.contains(HOSTILE));
        assert!(
            reply.to_lowercase().contains("permission") || reply.to_lowercase().contains("manage")
        );
        let off = observe(
            &disabled,
            &mock,
            &interaction(5001 + index as u64 * 10, command, vec![]),
        )
        .await;
        assert_eq!(
            private_reply(&off, false),
            two_bot_core::custom_commands::AUTOMATIONS_DISABLED_REPLY
        );
        for (scope_index, scope) in ["application", "guild", "dm"].into_iter().enumerate() {
            let mut foreign = interaction(
                5002 + index as u64 * 10 + scope_index as u64,
                command,
                vec![],
            );
            match scope {
                "application" => foreign.application_id = Id::new(9999),
                "guild" => foreign.guild_id = Some(Id::new(9999)),
                "dm" => foreign.guild_id = None,
                _ => unreachable!(),
            }
            let ignored = observe(&enabled, &mock, &foreign).await;
            assert!(matches!(ignored.result, Ok(false)), "{scope} ignored");
            assert!(ignored.requests.is_empty());
        }
    }
    mock.shutdown().await;
}
