#![cfg(feature = "db")]

//! Builder-owned regression pin for writer-shape validation ordering.
//!
//! The sequential malformed-writer matrices cannot distinguish decoding
//! before the registry mutex from decoding after it: the mutex is always
//! uncontended when each case is awaited in turn. This test holds a valid
//! writer's full-registry publication at the mock response gate, so the
//! holder task genuinely owns the shared registry mutex mid-PUT, and then
//! requires malformed `/command` and `/command-remove` interactions on a
//! clone of the same runtime to settle with their private corrections and
//! zero store effects BEFORE the gate is released. Moving writer decoding
//! behind the registry lock makes the malformed calls wait for the held
//! publication instead, and this test times out and fails.

#[allow(dead_code)]
mod common;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    custom_command_store as store, custom_commands::StoredCommand, InteractionRouter, RouterGates,
};
use two_bot_discord::{
    custom_commands::{CustomCommandError, CustomCommandRuntime},
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

const HOLDER_TEMPLATE: &str = "Holder replacement";
const HOSTILE: &str = "UNTRUSTED_SENTINEL @everyone <@9876>";
const AT: &str = "2026-10-01T00:00:00.000000+00:00";

fn runtime(pool: PgPool, mock: &MockRest) -> CustomCommandRuntime {
    let mut router = InteractionRouter::new(RouterGates {
        configured_guild: Some(2222),
        automations: true,
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
    CustomCommandRuntime::new(
        pool,
        Arc::new(router),
        ActionExecutor::with_proxy("writer-lock-fixture".to_owned(), Some(mock.origin()))
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
        "token": format!("writer-lock-{id}"), "version": 1, "guild_id": "2222",
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

struct Observed {
    result: Result<bool, CustomCommandError>,
    requests: Vec<RestRequest>,
    id: u64,
}

/// The management path defers privately, then completes once. Anything else
/// (a registry PUT, a channel send, a second owner) fails here.
fn correction(observed: &Observed, field: &str) {
    assert!(
        matches!(observed.result, Ok(true)),
        "malformed writer must settle while publication is held: {:?}",
        observed.result
    );
    let callback = format!(
        "/interactions/{}/writer-lock-{}/callback",
        observed.id, observed.id
    );
    let edit = format!(
        "/webhooks/1111/writer-lock-{}/messages/@original",
        observed.id
    );
    assert_eq!(
        observed.requests.len(),
        2,
        "one private defer plus one completion, no publication"
    );
    let ack_request = &observed.requests[0];
    assert_eq!(ack_request.method, "POST");
    assert!(ack_request.path.ends_with(&callback));
    let ack: Value = serde_json::from_slice(&ack_request.body).expect("ack JSON");
    assert_eq!(ack["type"], 5);
    assert_eq!(ack["data"]["flags"].as_u64().unwrap_or(0) & 64, 64);
    let edit_request = &observed.requests[1];
    assert_eq!(edit_request.method, "PATCH");
    assert!(edit_request.path.ends_with(&edit));
    let completion: Value = serde_json::from_slice(&edit_request.body).expect("completion JSON");
    for payload in [&ack["data"], &completion] {
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
    }
    let content = completion["content"].as_str().expect("private text reply");
    assert!(!content.is_empty() && content.encode_utf16().count() <= 2000);
    assert!(
        content.to_lowercase().contains(field),
        "correction must name {field}: {content}"
    );
    for forbidden in ["UNTRUSTED_SENTINEL", "@everyone", "Saved /", "Removed /"] {
        assert!(
            !content.contains(forbidden),
            "shape refusal must not echo hostile input: {content}"
        );
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

#[tokio::test]
async fn malformed_writers_reject_before_a_contended_registry_lock() {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("DB acceptance not exercised: TWO_TEST_DATABASE_URL unset");
        return;
    }
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
    // Count every acquisition on the runtime's own pool; snapshots run on
    // the separate fixture pool and never inflate the count.
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
    // Script order is deterministic: malformed writers run sequentially
    // while the holder's PUT is gated, so requests never interleave.
    let completion = || ScriptedResponse::json(200, json!({"id": "9000"}));
    let (mock, gate) = MockRest::start_gated_at(
        vec![
            ScriptedResponse::status(204), // holder defer
            ScriptedResponse::status(403), // holder publication PUT (gated)
            ScriptedResponse::status(204), // malformed define defer
            completion(),                  // malformed define completion
            ScriptedResponse::status(204), // malformed remove defer
            completion(),                  // malformed remove completion
            ScriptedResponse::status(204), // unknown-option define defer
            completion(),                  // unknown-option define completion
        ],
        completion(),
        1,
    )
    .await;
    let runtime = runtime(pool.clone(), &mock);

    // Holder: a valid replacement commits, then blocks republishing the
    // whole registry while still owning the shared registry mutex.
    let holder_runtime = runtime.clone();
    let task = tokio::spawn(async move {
        let holder = interaction(
            7001,
            "command",
            vec![
                option("name", "FAQ"),
                option("template", HOLDER_TEMPLATE),
                option("description", "New description"),
                option("text-trigger", "!NewFaq"),
            ],
        );
        holder_runtime
            .handle_interaction(&holder, Some("Test guild"))
            .await
    });
    let reached = tokio::time::timeout(Duration::from_secs(3), gate.wait_for_request())
        .await
        .is_ok();
    // The holder's write has committed by the time its PUT is in flight.
    let during = snapshot(db.pool()).await;
    accesses.store(0, Ordering::SeqCst);
    let malformed: Vec<(u64, &str, Vec<Value>, &str)> = vec![
        (
            7002,
            "command",
            vec![
                option("name", "FAQ"),
                option("name", "FAQ"),
                option("template", "Duplicate name must not win"),
            ],
            "name",
        ),
        (
            7003,
            "command-remove",
            vec![option("name", "FAQ"), option("name", "other")],
            "name",
        ),
        (
            7004,
            "command",
            vec![
                option("name", "FAQ"),
                option("template", "Unknown option must not win"),
                option("unexpected", HOSTILE),
            ],
            "option",
        ),
    ];
    let mut outcomes: Vec<Option<Observed>> = Vec::new();
    if reached {
        for (id, command, options, _field) in &malformed {
            let input = interaction(*id, command, options.clone());
            let start = mock.requests().len();
            let timed = tokio::time::timeout(
                Duration::from_secs(2),
                runtime.handle_interaction(&input, Some("Test guild")),
            )
            .await;
            match timed {
                Ok(result) => outcomes.push(Some(Observed {
                    result,
                    requests: mock.requests()[start..].to_vec(),
                    id: *id,
                })),
                Err(_) => {
                    outcomes.push(None);
                    break;
                }
            }
        }
    }
    let after = snapshot(db.pool()).await;
    let accesses_after = accesses.load(Ordering::SeqCst);
    let holder_finished_before_release = task.is_finished();
    gate.release();
    let holder_result = match tokio::time::timeout(Duration::from_secs(3), task).await {
        Ok(Ok(result)) => Some(result),
        _ => None,
    };
    let requests = mock.requests();
    drop(runtime);
    pool.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified owned-database teardown");

    assert!(
        reached,
        "valid writer must reach full-set publication after committing"
    );
    assert!(
        !holder_finished_before_release,
        "the gate must still hold the publication while malformed writers run"
    );
    let faq = during
        .0
        .iter()
        .find(|row| row["guild_id"] == "2222" && row["name"] == "faq")
        .expect("holder row is saved, not deleted");
    assert_eq!(
        faq["template"], HOLDER_TEMPLATE,
        "holder committed before its gated publication"
    );
    assert_eq!(
        outcomes.len(),
        malformed.len(),
        "every malformed writer settles before the gate is released"
    );
    for (outcome, (_id, _command, _options, field)) in outcomes.iter().zip(&malformed) {
        let observed = outcome
            .as_ref()
            .expect("malformed writer must not wait behind the held publication");
        correction(observed, field);
    }
    assert_eq!(
        accesses_after, 0,
        "malformed writers perform zero pool accesses under contention"
    );
    assert_eq!(
        during, after,
        "malformed writers change no definitions or audit rows under contention"
    );
    assert_eq!(
        requests.iter().filter(|r| r.method == "PUT").count(),
        1,
        "only the holder publishes"
    );
    assert!(
        matches!(holder_result, Some(Ok(true))),
        "holder completes once released: {holder_result:?}"
    );
}
