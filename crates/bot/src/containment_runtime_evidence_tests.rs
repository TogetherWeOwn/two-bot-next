//! Containment runtime acceptance: incomplete or contradictory guild-role
//! evidence must fail closed before planning.
//!
//! Each malformed-evidence case drives the armed worker against the mock
//! Discord double with a scripted guild-roles snapshot and held-role sets.
//! The contract is observable end to end: zero role DELETEs, and the one
//! completed incident is never stored or reported as `contained`. Whether
//! the refusal surfaces as a snapshot failure or a plan refusal is an
//! implementation choice; claiming contained-success is not.
//!
//! Worker tests are ignored and run only on agent-testdb / the CI service
//! container, like the rest of the containment runtime suite.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use twilight_model::guild::audit_log::AuditLogEventType;
use two_bot_core::backup::guild_config::STAGING_BOT_APPLICATION_ID;
use two_bot_core::containment_store::ContainmentStore;
use two_bot_cutover::parse::date_to_snowflake;
use two_bot_discord::{ActionExecutor, AuditEntryObserver, AuditLogObservation};

use crate::containment_runtime::{self, ContainmentSettings, SettingsSource};
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::gateway::ensure_crypto_provider;

const GUILD: u64 = 22;
const CHANNEL: &str = "12";
const EXECUTOR: u64 = 5001;
const TEST_DATABASE: &str = "postgres://agent_test@agent-testdb:5432/postgres";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("wall clock")
        .as_millis() as i64
}

fn fresh_entry_id(offset_ms: i64) -> u64 {
    date_to_snowflake((now_ms() + offset_ms) as u64)
        .parse()
        .expect("snowflakes are numeric")
}

fn entry(entry_id: u64) -> AuditLogObservation {
    AuditLogObservation {
        guild_id: GUILD,
        entry_id,
        action: AuditLogEventType::ChannelDelete,
        executor_id: Some(EXECUTOR),
        target_id: Some(6001),
    }
}

struct Scripted(ContainmentSettings);

impl SettingsSource for Scripted {
    async fn current(&mut self) -> ContainmentSettings {
        self.0.clone()
    }
}

fn settings() -> ContainmentSettings {
    ContainmentSettings {
        window_ms: 60_000,
        max_age_ms: 3_600_000,
        heat_threshold: 5,
        staff_channel: Some(CHANNEL.to_owned()),
    }
}

/// A mock Discord whose guild-roles snapshot and held-role sets are scripted
/// per case. DELETE answers 204 and is recorded: a passing run must record
/// none, and the assertion — not a mock panic — reports the failure.
async fn discord_evidence(roles: Value, executor_roles: Value, bot_roles: Value) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/users/@me") => {
                ScriptedResponse::json(200, json!({"id":"999","bot":true,"username":"bot"}))
            }
            ("GET", "/applications/@me") => {
                ScriptedResponse::json(200, json!({"id": STAGING_BOT_APPLICATION_ID}))
            }
            ("GET", "/guilds/22") => {
                ScriptedResponse::json(200, json!({"id":"22","owner_id":"888"}))
            }
            ("GET", "/guilds/22/roles") => ScriptedResponse::json(200, roles.clone()),
            ("GET", "/guilds/22/members/999") => {
                ScriptedResponse::json(200, json!({"user":{"id":"999"},"roles": bot_roles.clone()}))
            }
            ("GET", "/guilds/22/members/5001") => ScriptedResponse::json(
                200,
                json!({"user":{"id":"5001"},"roles": executor_roles.clone()}),
            ),
            ("GET", "/channels/12") => ScriptedResponse::json(
                200,
                json!({"id":"12","guild_id":"22","type":0,"permission_overwrites":[]}),
            ),
            ("POST", "/channels/12/messages") => ScriptedResponse::json(201, json!({"id":"99"})),
            ("DELETE", path) if path.starts_with("/guilds/22/members/5001/roles/") => {
                ScriptedResponse::status(204)
            }
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    })
    .await
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ensure_crypto_provider();
    ActionExecutor::with_proxy("containment-test-only-token".into(), Some(mock.origin())).unwrap()
}

/// Feed two channel deletes (heat 6 over threshold 5) through an armed
/// worker, then drain it deterministically.
async fn run_armed(store: ContainmentStore, mock: &MockRest) {
    let (runtime, worker) = containment_runtime::start(
        GUILD,
        Scripted(settings()),
        store,
        executor(mock),
        HashSet::new(),
        HashSet::new(),
        true,
    );
    runtime.observe_audit_entry(entry(fresh_entry_id(0)));
    runtime.observe_audit_entry(entry(fresh_entry_id(1_000)));
    drop(runtime);
    worker.await.expect("worker ends cleanly");
}

fn deletes(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "DELETE")
        .collect()
}

fn posted_contents(mock: &MockRest) -> Vec<String> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST")
        .map(|request| {
            let body: Value =
                serde_json::from_slice(&request.body).expect("staff posts carry JSON");
            body.get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

struct TestSchema {
    pool: PgPool,
    schema: String,
}

impl TestSchema {
    async fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let schema = format!(
            "contain_evidence_{}_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed),
            nanos
        );
        let mut admin = PgConnection::connect(TEST_DATABASE)
            .await
            .expect("only agent-testdb, agent_test, empty password");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&mut admin)
            .await
            .unwrap();
        let path = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .after_connect(move |connection, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(path)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(TEST_DATABASE)
            .await
            .expect("only agent-testdb");
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0370_containment_claims.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        Self { pool, schema }
    }

    async fn incident_states(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT state FROM containment_incidents ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .expect("incident states readable")
    }

    async fn close(self) {
        self.pool.close().await;
        let mut admin = PgConnection::connect(TEST_DATABASE).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&mut admin)
        .await
        .unwrap();
    }
}

/// The shared fail-closed assertion: no DELETE ran, exactly one incident
/// completed, and nothing — stored state or staff post — claims contained.
async fn assert_refused_evidence(
    case: &str,
    roles: Value,
    executor_roles: Value,
    bot_roles: Value,
) {
    let db = TestSchema::new().await;
    let mock = discord_evidence(roles, executor_roles, bot_roles).await;
    run_armed(ContainmentStore::from_pool(db.pool.clone()), &mock).await;

    assert!(
        deletes(&mock).is_empty(),
        "{case}: incomplete evidence must never DELETE a role, saw {:?}",
        deletes(&mock)
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>()
    );
    let states = db.incident_states().await;
    assert_eq!(
        states.len(),
        1,
        "{case}: exactly one completed incident, got {states:?}"
    );
    assert_ne!(
        states[0], "contained",
        "{case}: incomplete evidence must not complete as contained"
    );
    assert_ne!(
        states[0], "containing",
        "{case}: the incident must still be completed durably"
    );
    for body in posted_contents(&mock) {
        assert!(
            !body.contains("**Anti-nuke contained**"),
            "{case}: staff post must not claim contained: {body}"
        );
    }
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_evidence_empty_snapshot_with_held_roles_refuses() {
    assert_refused_evidence("empty snapshot", json!([]), json!(["77"]), json!(["88"])).await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_evidence_missing_executor_role_refuses() {
    assert_refused_evidence(
        "executor role absent from snapshot",
        json!([
            {"id":"22","permissions":"3072","position":0,"managed":false},
            {"id":"88","permissions":"0","position":5,"managed":false},
        ]),
        json!(["77"]),
        json!(["88"]),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_evidence_missing_bot_role_refuses() {
    assert_refused_evidence(
        "bot role absent from snapshot",
        json!([
            {"id":"22","permissions":"3072","position":0,"managed":false},
            {"id":"76","permissions":"0","position":1,"managed":false},
        ]),
        json!(["76"]),
        json!(["88"]),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_evidence_duplicate_conflicting_role_refuses() {
    assert_refused_evidence(
        "duplicate role id, safe then dangerous",
        json!([
            {"id":"22","permissions":"3072","position":0,"managed":false},
            {"id":"77","permissions":"0","position":1,"managed":false},
            {"id":"77","permissions":"8","position":1,"managed":false},
            {"id":"88","permissions":"0","position":5,"managed":false},
        ]),
        json!(["77"]),
        json!(["88"]),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_evidence_duplicate_identical_role_refuses() {
    assert_refused_evidence(
        "duplicate role id, identical rows",
        json!([
            {"id":"22","permissions":"3072","position":0,"managed":false},
            {"id":"77","permissions":"8","position":1,"managed":false},
            {"id":"77","permissions":"8","position":1,"managed":false},
            {"id":"88","permissions":"0","position":5,"managed":false},
        ]),
        json!(["77"]),
        json!(["88"]),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_evidence_zero_role_identity_refuses() {
    assert_refused_evidence(
        "snapshot carries role id 0",
        json!([
            {"id":"22","permissions":"3072","position":0,"managed":false},
            {"id":"0","permissions":"8","position":1,"managed":false},
            {"id":"77","permissions":"8","position":2,"managed":false},
            {"id":"88","permissions":"0","position":5,"managed":false},
        ]),
        json!(["77"]),
        json!(["88"]),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_evidence_negative_position_refuses() {
    assert_refused_evidence(
        "dangerous role with negative position",
        json!([
            {"id":"22","permissions":"3072","position":0,"managed":false},
            {"id":"77","permissions":"8","position":-1,"managed":false},
            {"id":"88","permissions":"0","position":5,"managed":false},
        ]),
        json!(["77"]),
        json!(["88"]),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Controls: complete evidence keeps today's outcomes.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_evidence_complete_safe_only_snapshot_is_contained() {
    let db = TestSchema::new().await;
    let mock = discord_evidence(
        json!([
            {"id":"22","permissions":"3072","position":0,"managed":false},
            {"id":"76","permissions":"0","position":1,"managed":false},
            {"id":"88","permissions":"0","position":5,"managed":false},
        ]),
        json!(["76"]),
        json!(["88"]),
    )
    .await;
    run_armed(ContainmentStore::from_pool(db.pool.clone()), &mock).await;

    assert!(
        deletes(&mock).is_empty(),
        "a genuine empty plan removes nothing"
    );
    assert_eq!(
        db.incident_states().await,
        vec!["contained".to_owned()],
        "complete safe-only evidence keeps the genuine empty-plan success"
    );
    let bodies = posted_contents(&mock);
    assert_eq!(bodies.len(), 1, "one contained staff post: {bodies:?}");
    assert!(bodies[0].contains("contained"), "{bodies:?}");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_evidence_complete_managed_role_still_refuses() {
    let db = TestSchema::new().await;
    let mock = discord_evidence(
        json!([
            {"id":"22","permissions":"3072","position":0,"managed":false},
            {"id":"77","permissions":"8","position":1,"managed":true},
            {"id":"88","permissions":"0","position":5,"managed":false},
        ]),
        json!(["77"]),
        json!(["88"]),
    )
    .await;
    run_armed(ContainmentStore::from_pool(db.pool.clone()), &mock).await;

    assert!(
        deletes(&mock).is_empty(),
        "managed-role refusal never DELETEs"
    );
    assert_eq!(
        db.incident_states().await,
        vec!["refused".to_owned()],
        "the existing managed-role refusal is unchanged on complete evidence"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_evidence_complete_hierarchy_block_still_refuses() {
    let db = TestSchema::new().await;
    let mock = discord_evidence(
        json!([
            {"id":"22","permissions":"3072","position":0,"managed":false},
            {"id":"77","permissions":"8","position":9,"managed":false},
            {"id":"88","permissions":"0","position":5,"managed":false},
        ]),
        json!(["77"]),
        json!(["88"]),
    )
    .await;
    run_armed(ContainmentStore::from_pool(db.pool.clone()), &mock).await;

    assert!(deletes(&mock).is_empty(), "hierarchy refusal never DELETEs");
    assert_eq!(
        db.incident_states().await,
        vec!["refused".to_owned()],
        "the existing hierarchy refusal is unchanged on complete evidence"
    );
    mock.shutdown().await;
    db.close().await;
}
