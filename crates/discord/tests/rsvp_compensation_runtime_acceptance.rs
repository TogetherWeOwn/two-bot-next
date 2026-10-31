//! Acceptance: the live `/rsvp` path compensates atomically through the
//! post-write revalidation seam.
//!
//! When the fence refuses a committed write (event or membership gone), the
//! handler compensates and keeps only the refusal. Store-level tests pin the
//! fencing and atomicity contract directly; these two pin it through the real
//! runtime seam with a forced audit-delete failure: the attempt's response
//! row AND its audit row must both survive the failed cleanup, the user must
//! still see only the existing refusal text, and exactly one bounded
//! `compensation_failed` diagnostic must name the write. The fault is a
//! disposable-DB `BEFORE DELETE` trigger on `announcements_audit_log` —
//! compensation is the only path that deletes there mid-attempt.
//!
//! Protected acceptance file: the paired builder consumes this head without
//! editing it.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

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

fn event(status: u64) -> ScriptedResponse {
    ScriptedResponse::json(
        200,
        json!({"id": EVENT, "guild_id": GUILD, "status": status}),
    )
}

fn member(user: &str) -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"user": {"id": user}, "roles": []}))
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

fn assert_reply(mock: &MockRest, content: &str) {
    let requests = mock.requests();
    let last = requests.last().unwrap();
    let body: Value = serde_json::from_slice(&last.body).unwrap();
    assert_eq!(last.method, "PATCH");
    assert_eq!(
        last.path,
        "/api/v10/webhooks/1111/mock-rsvp-token/messages/@original"
    );
    assert_eq!(body["content"], content);
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
}

async fn counts(pool: &Pool<Postgres>) -> (i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM event_rsvps), (SELECT count(*) FROM announcements_audit_log), (SELECT count(*) FROM community_facts)")
        .fetch_one(pool).await.unwrap()
}

/// Process-wide JSON log sink (one subscriber per test binary); readers only
/// consume bytes written after their own mark.
#[derive(Clone, Default)]
struct LogSink(Arc<Mutex<Vec<u8>>>);

impl Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn log_sink() -> LogSink {
    static SINK: OnceLock<LogSink> = OnceLock::new();
    SINK.get_or_init(|| {
        let sink = LogSink::default();
        let writer = sink.clone();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .json()
                .with_writer(move || writer.clone())
                .finish(),
        )
        .expect("install the process-wide log sink once");
        sink
    })
    .clone()
}

/// Serializes the diagnostic-window tests so an injected failure in one can
/// never appear inside another's window.
static DIAGNOSTIC_WINDOW: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn log_mark() -> usize {
    log_sink().0.lock().unwrap().len()
}

fn compensation_failures_since(mark: usize) -> Vec<Value> {
    let bytes = log_sink().0.lock().unwrap()[mark..].to_vec();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["fields"]["msg"] == "compensation_failed")
        .collect()
}

/// Exactly one compensation-failure diagnostic: the fixed field vocabulary
/// and none of the attempt's member or event IDs, the interaction
/// credential, or the injected database error text.
fn assert_single_failure(failures: Vec<Value>, write: &str) {
    let [diagnostic] = failures.as_slice() else {
        panic!("expected one compensation_failed diagnostic, found {failures:?}");
    };
    assert_eq!(diagnostic["level"], "ERROR", "{diagnostic}");
    assert_eq!(diagnostic["fields"]["write"], write, "{diagnostic}");
    assert_eq!(
        diagnostic["fields"].as_object().map(|fields| fields.len()),
        Some(3),
        "{diagnostic}"
    );
    let shown = diagnostic.to_string();
    for (kind, value) in [
        ("member id", USER),
        ("event id", EVENT),
        ("interaction token", "mock-rsvp-token"),
        ("database error text", "injected audit-delete fault"),
    ] {
        assert!(!shown.contains(value), "diagnostic leaked a {kind}");
    }
}

async fn install_audit_delete_fault(pool: &Pool<Postgres>) {
    sqlx::query(
        "CREATE FUNCTION injected_audit_delete_fault() RETURNS trigger LANGUAGE plpgsql AS
         $$ BEGIN RAISE EXCEPTION 'injected audit-delete fault'; END $$",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER fault_audit_delete BEFORE DELETE ON announcements_audit_log
         FOR EACH ROW EXECUTE FUNCTION injected_audit_delete_fault()",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// First write refused at the fence while the audit delete is faulted: the
/// user sees only the existing event-gone refusal, BOTH the attempt's
/// response row and its audit row survive, and exactly one bounded
/// `compensation_failed` diagnostic names the write. RED while the response
/// delete commits separately from the audit delete.
#[tokio::test]
async fn faulted_audit_delete_keeps_first_write_attempt_and_diagnostic() {
    let _window = DIAGNOSTIC_WINDOW.lock().await;
    let Some(fixture) = pool().await else {
        return;
    };
    install_audit_delete_fault(fixture.pool()).await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            member(USER),
            event(1),
            // Post-write fence: membership still current, event now gone.
            member(USER),
            ScriptedResponse::status(404),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let mark = log_mark();
    run(fixture.pool(), &mock, &rsvp(830, "going")).await;
    assert_reply(
        &mock,
        "No scheduled event with that id exists in this server.",
    );
    assert_eq!(mock.requests().len(), 6);
    assert_eq!(
        counts(fixture.pool()).await,
        (1, 1, 0),
        "the failed cleanup keeps both the response and its audit"
    );
    let audits: Vec<String> = sqlx::query_scalar("SELECT id FROM announcements_audit_log")
        .fetch_all(fixture.pool())
        .await
        .unwrap();
    assert_eq!(audits, ["rsvp:830".to_owned()]);
    assert_single_failure(compensation_failures_since(mark), "rsvp");
    mock.shutdown().await;
    fixture.close().await;
}

/// Refused rewrite while the audit delete is faulted: the first `going`
/// write commits, the `interested` rewrite hits a fence refusal, and the
/// failed cleanup leaves the ATTEMPTED rewrite plus both audits in place —
/// never a half-restored prior row. The user sees only the existing
/// membership refusal, with one bounded diagnostic. RED while the restore
/// commits separately from the audit delete.
#[tokio::test]
async fn faulted_audit_delete_keeps_refused_rewrite_attempt_and_history() {
    let _window = DIAGNOSTIC_WINDOW.lock().await;
    let Some(fixture) = pool().await else {
        return;
    };
    // Establish the preceding response and audit through the live path.
    let saved = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            member(USER),
            event(1),
            member(USER),
            event(1),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(fixture.pool(), &saved, &rsvp(840, "going")).await;
    assert_reply(&saved, "RSVP saved: going.");
    saved.shutdown().await;
    let before: (String, String) = sqlx::query_as(
        "SELECT status, responded_at::text FROM event_rsvps WHERE guild_id=$1 AND event_id=$2 AND user_id=$3",
    )
    .bind(GUILD)
    .bind(EVENT)
    .bind(USER)
    .fetch_one(fixture.pool())
    .await
    .unwrap();
    assert_eq!(before.0, "going");

    install_audit_delete_fault(fixture.pool()).await;
    let refused = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            member(USER),
            event(1),
            // Post-write fence: transient membership lookup failure. The
            // fence short-circuits, so no second event re-read runs.
            ScriptedResponse::status(403),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let mark = log_mark();
    run(fixture.pool(), &refused, &rsvp(841, "interested")).await;
    assert_reply(&refused, "Unable to verify server membership.");
    assert_eq!(refused.requests().len(), 5);
    refused.shutdown().await;

    // The failed cleanup changed nothing: the attempted rewrite (not the
    // restored prior row) and both audits remain.
    let after: (String, String) = sqlx::query_as(
        "SELECT status, responded_at::text FROM event_rsvps WHERE guild_id=$1 AND event_id=$2 AND user_id=$3",
    )
    .bind(GUILD)
    .bind(EVENT)
    .bind(USER)
    .fetch_one(fixture.pool())
    .await
    .unwrap();
    assert_eq!(after.0, "interested", "the attempted rewrite is untouched");
    assert_ne!(
        after.1, before.1,
        "the row carries the rewrite timestamp, not the restored prior one"
    );
    assert_eq!(counts(fixture.pool()).await, (1, 2, 0));
    let audits: Vec<String> =
        sqlx::query_scalar("SELECT id FROM announcements_audit_log ORDER BY id")
            .fetch_all(fixture.pool())
            .await
            .unwrap();
    assert_eq!(audits, ["rsvp:840".to_owned(), "rsvp:841".to_owned()]);
    assert_single_failure(compensation_failures_since(mark), "rsvp");
    fixture.close().await;
}
