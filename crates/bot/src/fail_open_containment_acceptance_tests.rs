#![cfg(test)]
//! Protected fail-open acceptance: a dropped quarantine outcome surfaces.
//!
//! Paired build card implements the fix; the Builder must not edit this file.
//! `finish` currently logs a `complete_incident` failure and posts the staff
//! alert anyway with the unrecorded outcome, so the alert and the durable
//! incident record silently disagree. The contract pinned here:
//!
//! - When `complete_incident` fails, the staff alert carries the existing
//!   `uncertain` outcome vocabulary (never the unrecorded outcome as fact)
//!   and the `two_bot_containment_store_errors_total{op="complete_incident"}`
//!   series increments. The Builder retries first if desired; with the
//!   failure injected below every attempt fails, so the verdict is always
//!   outcome-unknown. The series name and `op` label value are pinned: the
//!   Builder adds exactly them.
//! - With a healthy store the alert posts the decided outcome with no
//!   unknown marker (healthy path unchanged).
//!
//! Injection is a `BEFORE UPDATE` trigger on `containment_incidents` in the
//! test's own disposable schema: `claim_event` and `begin_incident` only
//! `INSERT` into that table (the event flip `UPDATE`s `containment_events`),
//! so the trigger breaks exactly the completion write, deterministically,
//! with no timing race. Ignored by default like the other worker-path tests:
//! the disposable database is the hard-coded
//! `postgres://agent_test@agent-testdb:5432/postgres` in a fresh per-test
//! schema. `DATABASE_URL` is never read.

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
/// Pinned series for completion-write failures; the `op` label value below
/// is part of the contract.
const STORE_ERROR_SERIES: &str = "two_bot_containment_store_errors_total";
const COMPLETE_OP: &str = r#"op="complete_incident""#;

async fn discord() -> MockRest {
    MockRest::with_responder(|request: &RestRequest| {
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
            ("GET", "/guilds/22/roles") => ScriptedResponse::json(
                200,
                json!([
                    {"id":"22","permissions":"3072","position":0,"managed":false},
                    {"id":"77","permissions":"8","position":1,"managed":false},
                    {"id":"88","permissions":"0","position":5,"managed":false},
                ]),
            ),
            ("GET", "/guilds/22/members/999") => {
                ScriptedResponse::json(200, json!({"user":{"id":"999"},"roles":["88"]}))
            }
            ("GET", "/guilds/22/members/5001") => {
                ScriptedResponse::json(200, json!({"user":{"id":"5001"},"roles":["77"]}))
            }
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
        // One channel delete (heat 3) opens the incident on the first entry.
        heat_threshold: 1,
        staff_channel: Some(CHANNEL.to_owned()),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("wall clock")
        .as_millis() as i64
}

fn fresh_entry() -> AuditLogObservation {
    let entry_id: u64 = date_to_snowflake(now_ms() as u64)
        .parse()
        .expect("snowflakes are numeric");
    AuditLogObservation {
        guild_id: GUILD,
        entry_id,
        action: AuditLogEventType::ChannelDelete,
        executor_id: Some(EXECUTOR),
        target_id: Some(6001),
    }
}

/// Feed one entry through a started runtime, then drain it deterministically:
// dropping the only sender lets the worker finish its queue and exit.
async fn run(store: ContainmentStore, mock: &MockRest) {
    let (runtime, worker) = containment_runtime::start(
        GUILD,
        Scripted(settings()),
        store,
        executor(mock),
        HashSet::new(),
        HashSet::new(),
        false,
    );
    runtime.observe_audit_entry(fresh_entry());
    drop(runtime);
    worker.await.expect("worker ends cleanly");
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

fn store_error_count(rendered: &str) -> f64 {
    rendered
        .lines()
        .filter(|line| line.starts_with(STORE_ERROR_SERIES))
        .filter(|line| line.contains(COMPLETE_OP))
        .filter_map(|line| line.split_whitespace().last()?.parse::<f64>().ok())
        .sum()
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
            "failopen_contain_{}_{}_{}",
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

    /// Break exactly the completion write: every `UPDATE` of an incident row
    /// raises, while the `INSERT`s behind claim and begin keep working.
    async fn break_completions(&self) {
        sqlx::raw_sql(
            "CREATE OR REPLACE FUNCTION fail_open_refuse_incident_update() \
             RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
             RAISE EXCEPTION 'injected complete_incident failure'; END $$; \
             CREATE TRIGGER fail_open_refuse_incident_update BEFORE UPDATE \
             ON containment_incidents FOR EACH ROW \
             EXECUTE FUNCTION fail_open_refuse_incident_update()",
        )
        .execute(&self.pool)
        .await
        .unwrap();
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

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn failed_completion_marks_outcome_unknown_and_meters_it() {
    let db = TestSchema::new().await;
    db.break_completions().await;
    let store = ContainmentStore::from_pool(db.pool.clone());
    let mock = discord().await;

    let before = store_error_count(&two_bot_core::metrics::global().render(None));
    run(store, &mock).await;
    let after = store_error_count(&two_bot_core::metrics::global().render(None));

    let bodies = posted_contents(&mock);
    mock.shutdown().await;
    assert_eq!(bodies.len(), 1, "one staff alert is still posted");
    assert!(
        bodies[0].contains("uncertain"),
        "a dropped outcome surfaces as outcome-unknown, got: {}",
        bodies[0]
    );
    assert!(
        after > before,
        "the completion failure increments {STORE_ERROR_SERIES}{{{COMPLETE_OP}}}"
    );
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn healthy_completion_posts_the_decided_outcome() {
    // Control: with a working store the alert carries the decided outcome
    // and no unknown marker, exactly as today.
    let db = TestSchema::new().await;
    let store = ContainmentStore::from_pool(db.pool.clone());
    let mock = discord().await;

    run(store, &mock).await;

    let bodies = posted_contents(&mock);
    mock.shutdown().await;
    assert_eq!(bodies.len(), 1, "one staff alert is posted");
    assert!(
        bodies[0].contains("dry_run"),
        "healthy completion posts the decided outcome, got: {}",
        bodies[0]
    );
    assert!(
        !bodies[0].contains("uncertain"),
        "healthy completion carries no unknown marker, got: {}",
        bodies[0]
    );
    db.close().await;
}
