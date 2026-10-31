//! Acceptance: an RSVP response and its required audit row commit atomically.
//!
//! A failed required-audit insert during a first RSVP must leave neither a
//! new response nor a new audit row, and the same failure during a rewrite
//! must preserve the exact preceding response and audit history. The failure
//! seam is a `BEFORE INSERT` trigger on `announcements_audit_log` inside the
//! disposable per-test database only; no production seam or schema is added.
//! Dropping the trigger and retrying is the success control in each test.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{Pool, Postgres};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const EVENT: &str = "1546451670500642999";
const USER: &str = "1546451670500642888";

fn router() -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD.parse().unwrap()),
        scorecard: true,
        automations: false,
        announcements: true,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    })
}

fn rsvp(id: u64, status: &str) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": GUILD},
        "id": id.to_string(), "token": "mock-rsvp-token", "type": 2,
        "version": 1, "guild_id": GUILD,
        "member": {"permissions": "0", "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": USER, "username": "invoker", "discriminator": "0"}},
        "data": {"id": "4444", "name": "rsvp", "type": 1,
            "options": [
                {"name": "event-id", "type": 3, "value": EVENT},
                {"name": "status", "type": 3, "value": status}
            ],
            "resolved": {"users": {
                USER: {"id": USER, "username": "human", "discriminator": "0"}
            }}}
    }))
    .expect("wire interaction")
}

/// Route-aware Discord double: membership and event reads always succeed, so
/// the only failure an attempt can hit is the injected audit insert. This
/// also keeps the tests independent of the post-write fence's read order.
async fn live_mock() -> MockRest {
    MockRest::with_responder(|request| {
        if request.method == "POST" && request.path.ends_with("/callback") {
            ScriptedResponse::status(204)
        } else if request.method == "GET" && request.path.contains("/members/") {
            ScriptedResponse::json(200, json!({"user": {"id": USER}, "roles": []}))
        } else if request.method == "GET" && request.path.contains("/scheduled-events/") {
            ScriptedResponse::json(200, json!({"id": EVENT, "guild_id": GUILD, "status": 1}))
        } else if request.method == "PATCH" && request.path.contains("/webhooks/") {
            // Deferred edits require an ID-bearing 200 receipt.
            ScriptedResponse::json(200, json!({"id": "99"}))
        } else {
            ScriptedResponse::status(500)
        }
    })
    .await
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy(
        "mock-token-not-a-credential".to_owned(),
        Some(mock.origin()),
    )
    .unwrap()
}

struct Fixture {
    db: TestDatabase,
}

impl Fixture {
    fn pool(&self) -> &Pool<Postgres> {
        self.db.pool()
    }

    async fn close(self) {
        self.db.close().await.expect("drop test database");
    }
}

async fn pool() -> Option<Fixture> {
    let url = match std::env::var("TWO_RSVP_RUNTIME_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => {
            eprintln!("skipped database acceptance: TWO_RSVP_RUNTIME_TEST_DATABASE_URL not set");
            return None;
        }
        Err(_) => panic!("test URL must be Unicode"),
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create guarded disposable test database; no credential fallback");
    Some(Fixture { db })
}

async fn run(pool: &Pool<Postgres>, mock: &MockRest, interaction: &Interaction) {
    let runtime = InteractionRuntime::with_router(
        router(),
        pool.clone(),
        executor(mock),
        0,
        ClassifierConfig::default(),
    );
    assert!(runtime
        .handle(interaction)
        .await
        .expect("handler execution"));
}

/// The user-visible text of the most recent deferred edit.
fn last_edit_content(mock: &MockRest) -> String {
    let requests = mock.requests();
    let edit = requests
        .iter()
        .rev()
        .find(|request| request.method == "PATCH")
        .expect("a deferred edit reply");
    let body: Value = serde_json::from_slice(&edit.body).unwrap();
    body["content"]
        .as_str()
        .expect("edit content is text")
        .to_owned()
}

/// A failed audit insert must surface one of the existing safe refusals:
/// never the saved text and never the injected database error.
fn assert_safe_refusal(content: &str) {
    assert!(
        ["Unable to audit RSVP.", "Unable to save RSVP."].contains(&content),
        "expected an existing safe RSVP refusal, got: {content}"
    );
    assert!(!content.contains("injected audit fault"), "{content}");
    assert!(!content.starts_with("RSVP saved:"), "{content}");
}

async fn install_audit_fault(pool: &Pool<Postgres>) {
    sqlx::query("CREATE FUNCTION injected_audit_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected audit fault'; END $$")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fault_audit_insert BEFORE INSERT ON announcements_audit_log FOR EACH ROW EXECUTE FUNCTION injected_audit_fault()")
        .execute(pool)
        .await
        .unwrap();
}

async fn remove_audit_fault(pool: &Pool<Postgres>) {
    sqlx::query("DROP TRIGGER fault_audit_insert ON announcements_audit_log")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION injected_audit_fault()")
        .execute(pool)
        .await
        .unwrap();
}

async fn counts(pool: &Pool<Postgres>) -> (i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM event_rsvps), (SELECT count(*) FROM announcements_audit_log), (SELECT count(*) FROM community_facts)")
        .fetch_one(pool)
        .await
        .unwrap()
}

type ResponseRow = (String, String);
type AuditRow = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
);

async fn stored_response(pool: &Pool<Postgres>) -> Option<ResponseRow> {
    sqlx::query_as(
        "SELECT status, responded_at::text FROM event_rsvps WHERE guild_id=$1 AND event_id=$2 AND user_id=$3",
    )
    .bind(GUILD)
    .bind(EVENT)
    .bind(USER)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn stored_audits(pool: &Pool<Postgres>) -> Vec<AuditRow> {
    sqlx::query_as(
        "SELECT id, guild_id, actor_id, action, target_key, outcome, reason, created_at::text FROM announcements_audit_log ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// First write: an injected required-audit insert failure leaves neither a
/// new response nor a new audit row and returns the existing safe refusal
/// rather than success. Removing the fault makes the same write succeed.
#[tokio::test]
async fn audit_insert_failure_on_first_rsvp_leaves_no_response_or_audit() {
    let Some(fixture) = pool().await else {
        return;
    };
    install_audit_fault(fixture.pool()).await;
    let mock = live_mock().await;
    run(fixture.pool(), &mock, &rsvp(900, "going")).await;
    assert_safe_refusal(&last_edit_content(&mock));
    assert_eq!(stored_response(fixture.pool()).await, None);
    assert_eq!(stored_audits(fixture.pool()).await, Vec::<AuditRow>::new());
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;

    // Success control: with the fault removed, the same member's first write
    // produces the expected response and exactly one audit row.
    remove_audit_fault(fixture.pool()).await;
    let mock = live_mock().await;
    run(fixture.pool(), &mock, &rsvp(901, "going")).await;
    assert_eq!(last_edit_content(&mock), "RSVP saved: going.");
    let stored = stored_response(fixture.pool())
        .await
        .expect("the control write stored a response");
    assert_eq!(stored.0, "going");
    let audits = stored_audits(fixture.pool()).await;
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].0, "rsvp:901");
    assert_eq!(audits[0].1, GUILD);
    assert_eq!(audits[0].2.as_deref(), Some(USER));
    assert_eq!(audits[0].3, "event.rsvp");
    assert_eq!(audits[0].4.as_deref(), Some(EVENT));
    assert_eq!(audits[0].5, "going");
    assert_eq!(counts(fixture.pool()).await, (1, 1, 0));
    mock.shutdown().await;
    fixture.close().await;
}

/// Rewrite: an injected required-audit insert failure preserves the exact
/// preceding response status/timestamp and the full prior audit history,
/// with no partially committed replacement. Removing the fault makes the
/// same rewrite succeed and append its audit row.
#[tokio::test]
async fn audit_insert_failure_on_rsvp_rewrite_preserves_exact_prior_state() {
    let Some(fixture) = pool().await else {
        return;
    };
    // Establish the preceding response and audit through the live path.
    let mock = live_mock().await;
    run(fixture.pool(), &mock, &rsvp(910, "going")).await;
    assert_eq!(last_edit_content(&mock), "RSVP saved: going.");
    mock.shutdown().await;
    let before_response = stored_response(fixture.pool())
        .await
        .expect("the setup write stored a response");
    assert_eq!(before_response.0, "going");
    let before_audits = stored_audits(fixture.pool()).await;
    assert_eq!(before_audits.len(), 1);
    assert_eq!(before_audits[0].0, "rsvp:910");

    install_audit_fault(fixture.pool()).await;
    let mock = live_mock().await;
    run(fixture.pool(), &mock, &rsvp(911, "interested")).await;
    assert_safe_refusal(&last_edit_content(&mock));
    // The exact prior response (status and timestamp) and the exact prior
    // audit history survive: no replacement was partially committed.
    assert_eq!(stored_response(fixture.pool()).await, Some(before_response));
    assert_eq!(stored_audits(fixture.pool()).await, before_audits);
    assert_eq!(counts(fixture.pool()).await, (1, 1, 0));
    mock.shutdown().await;

    // Success control: with the fault removed, the going -> interested rewrite
    // lands with the changed receipt, and its audit row appends after the
    // preserved history.
    remove_audit_fault(fixture.pool()).await;
    let mock = live_mock().await;
    run(fixture.pool(), &mock, &rsvp(912, "interested")).await;
    assert_eq!(
        last_edit_content(&mock),
        "RSVP saved: interested (changed from going)."
    );
    let stored = stored_response(fixture.pool())
        .await
        .expect("the control rewrite stored a response");
    assert_eq!(stored.0, "interested");
    let audits = stored_audits(fixture.pool()).await;
    assert_eq!(audits.len(), 2);
    assert_eq!(audits[0].0, "rsvp:910");
    assert_eq!(audits[1].0, "rsvp:912");
    assert_eq!(audits[1].5, "interested");
    assert_eq!(counts(fixture.pool()).await, (1, 2, 0));
    mock.shutdown().await;
    fixture.close().await;
}
