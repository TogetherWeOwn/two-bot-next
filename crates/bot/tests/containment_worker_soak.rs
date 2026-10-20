//! Staging-soak acceptance for the containment runtime **worker path**:
//! one end-to-end journey per test through the real worker, the durable
//! store and the mock Discord double — never a live guild, never a
//! staging/production database.
//!
//! # Acceptance contract (pinned seam)
//!
//! The worker lives in the binary crate today, so this external suite
//! pins exactly one public seam for the builder to expose without any
//! behavior change:
//!
//! ```text
//! two_bot::containment_runtime::{start, ContainmentSettings, SettingsSource}
//! ```
//!
//! with the signatures, fields (`window_ms`, `max_age_ms`,
//! `heat_threshold`, `staff_channel`) and call shape exercised below —
//! the same shape the in-crate runtime tests already use. Until that
//! wiring lands this file fails to compile with an unresolved import
//! of `two_bot::containment_runtime`; that is the intended red state.
//! The builder must not edit this file to make it pass.
//!
//! Everything else comes from crates that are already public: the
//! durable store (`two_bot_core::containment_store`), the shared
//! executor (`two_bot_discord::ActionExecutor`) and a loopback-only REST
//! fixture containing just the surface these worker journeys use.
//!
//! # What is pinned
//!
//! - threshold cross -> dry-run alert, with the triggering audit ID
//!   claimed exactly once even when the same entry is redelivered and
//!   even when every entry is replayed into a **restarted** worker;
//! - evidence persistence read back from the disposable Postgres:
//!   per-entry dispositions and exactly one incident row per journey;
//! - whole-plan preflight refusal (managed dangerous role) persists
//!   `refused` and issues no role DELETE;
//! - ordered partial failure stops at the first failed removal, is
//!   persisted `uncertain`, blocks later entries, and is never retried
//!   or restored on replay;
//! - fixed-vocabulary outcomes only: a canary string in the mock's
//!   upstream error body must not appear in the staff post or in the
//!   persisted incident result, and staff posts carry no mentions.
//!
//! # Running
//!
//! Ignored by default. The `containment_runtime_` name prefix puts
//! these tests in the existing CI containment lane:
//!
//! ```text
//! cargo test -p two-bot --locked --test containment_worker_soak -- --ignored
//! ```
//!
//! The only database touched is the hard-coded disposable
//! `postgres://agent_test@agent-testdb:5432/postgres`, in a fresh
//! per-test schema (same fence as the in-crate runtime tests).
//! `DATABASE_URL` is never read.

#[path = "support/containment_rest.rs"]
mod common;

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use twilight_model::guild::audit_log::AuditLogEventType;
use two_bot::containment_runtime::{self, ContainmentSettings, SettingsSource};
use two_bot_core::backup::guild_config::STAGING_BOT_APPLICATION_ID;
use two_bot_core::containment_store::ContainmentStore;
use two_bot_cutover::parse::date_to_snowflake;
use two_bot_discord::{ActionExecutor, AuditEntryObserver, AuditLogObservation};

const GUILD: u64 = 22;
const CHANNEL: &str = "12";
const EXECUTOR: u64 = 5001;
const TEST_DATABASE: &str = "postgres://agent_test@agent-testdb:5432/postgres";
/// Placed in a mock upstream error body; fixed-vocabulary outcomes must
/// never echo it into a staff post or a persisted incident result.
const UPSTREAM_CANARY: &str = "soak-canary-upstream-detail";

/// Real processing time, so occurrence freshness never depends on a constant.
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

fn settings() -> ContainmentSettings {
    ContainmentSettings {
        window_ms: 60_000,
        max_age_ms: 3_600_000,
        heat_threshold: 5,
        staff_channel: Some(CHANNEL.to_owned()),
    }
}

struct Scripted(ContainmentSettings);

impl SettingsSource for Scripted {
    async fn current(&mut self) -> ContainmentSettings {
        self.0.clone()
    }
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("containment-test-only-token".into(), Some(mock.origin()))
        .expect("executor builds against the mock")
}

/// Feed `entries` through a started runtime, then drain it
/// deterministically: dropping the only sender lets the worker finish
/// its queue and exit. Each call stands up a fresh worker, so calling
/// it twice against one store models a restart/replay.
async fn run_worker(
    pool: &PgPool,
    mock: &MockRest,
    entries: impl IntoIterator<Item = AuditLogObservation>,
    armed: bool,
) {
    let (runtime, worker) = containment_runtime::start(
        GUILD,
        Scripted(settings()),
        ContainmentStore::from_pool(pool.clone()),
        executor(mock),
        HashSet::new(),
        HashSet::new(),
        armed,
    );
    for observation in entries {
        runtime.observe_audit_entry(observation);
    }
    drop(runtime);
    worker.await.expect("worker ends cleanly");
}

fn requests(mock: &MockRest, method: &str) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == method)
        .collect()
}

fn posted_contents(mock: &MockRest) -> Vec<String> {
    requests(mock, "POST")
        .into_iter()
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

/// Staff posts are fixed-vocabulary and mention-free: no user/role
/// mention markup, no mass mentions, no upstream body echo.
fn assert_post_is_fixed_vocabulary(content: &str) {
    assert!(!content.contains("<@"), "mention markup: {content:?}");
    assert!(!content.contains("@everyone"), "mass mention: {content:?}");
    assert!(!content.contains("@here"), "mass mention: {content:?}");
    assert!(
        !content.contains(UPSTREAM_CANARY),
        "upstream error body echoed: {content:?}"
    );
}

struct TestSchema {
    pool: PgPool,
    schema: String,
}

impl TestSchema {
    async fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("contain_soak_{stamp}");
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

/// `(audit_entry_id, state)` for every persisted event, in occurrence order.
async fn event_rows(pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT audit_entry_id, state FROM containment_events \
         ORDER BY occurred_at, audit_entry_id",
    )
    .fetch_all(pool)
    .await
    .expect("evidence rows readable")
}

/// `(trigger_audit_entry_id, state, heat, result_json, completed_at)`
/// for every persisted incident.
async fn incident_rows(
    pool: &PgPool,
) -> Vec<(String, String, i32, Option<String>, Option<String>)> {
    sqlx::query_as(
        "SELECT trigger_audit_entry_id, state, heat, result_json, completed_at \
         FROM containment_incidents ORDER BY started_at, id",
    )
    .fetch_all(pool)
    .await
    .expect("incident rows readable")
}

/// The bot may View and Send in the staff channel; the executor holds
/// one removable dangerous role (77).
async fn discord_open() -> MockRest {
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

/// Three removable roles in snapshot order; the second DELETE fails
/// with a 500 carrying the canary, and the third must never run.
async fn discord_partial() -> MockRest {
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
                    {"id":"79","permissions":"8","position":2,"managed":false},
                    {"id":"80","permissions":"8","position":3,"managed":false},
                    {"id":"88","permissions":"8","position":5,"managed":false},
                ]),
            ),
            ("GET", "/guilds/22/members/999") => {
                ScriptedResponse::json(200, json!({"user":{"id":"999"},"roles":["88"]}))
            }
            ("GET", "/guilds/22/members/5001") => {
                ScriptedResponse::json(200, json!({"user":{"id":"5001"},"roles":["77","79","80"]}))
            }
            ("GET", "/channels/12") => ScriptedResponse::json(
                200,
                json!({"id":"12","guild_id":"22","type":0,"permission_overwrites":[]}),
            ),
            ("POST", "/channels/12/messages") => ScriptedResponse::json(201, json!({"id":"99"})),
            ("DELETE", "/guilds/22/members/5001/roles/77") => ScriptedResponse::status(204),
            ("DELETE", "/guilds/22/members/5001/roles/79") => {
                ScriptedResponse::json(500, json!({"message": UPSTREAM_CANARY}))
            }
            ("DELETE", path) if path.starts_with("/guilds/22/members/5001/roles/") => {
                panic!("no third removal after the failure: {path}")
            }
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    })
    .await
}

/// The executor's only dangerous role is managed: whole-plan preflight
/// refuses and no DELETE may run.
async fn discord_managed() -> MockRest {
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
                    {"id":"77","permissions":"8","position":1,"managed":true},
                    {"id":"88","permissions":"8","position":5,"managed":false},
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
            ("DELETE", path) if path.starts_with("/guilds/22/members/") => {
                panic!("refused plans never DELETE: {path}")
            }
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_worker_soak_dry_run_claims_once_across_redelivery_and_restart() {
    let db = TestSchema::new().await;
    let mock = discord_open().await;
    let (first, trigger, during_cooldown) = (
        fresh_entry_id(0),
        fresh_entry_id(1_000),
        fresh_entry_id(2_000),
    );

    // Two channel deletes (3 + 3) cross heat 5 on the second entry.
    // The trigger is then redelivered with the same audit ID, and a
    // third entry lands inside the incident cooldown.
    run_worker(
        &db.pool,
        &mock,
        [
            entry(first),
            entry(trigger),
            entry(trigger),
            entry(during_cooldown),
        ],
        false,
    )
    .await;

    let bodies = posted_contents(&mock);
    assert_eq!(bodies.len(), 1, "one dry-run post, then cooldown silence");
    assert!(bodies[0].contains("Anti-nuke"), "{bodies:?}");
    assert!(bodies[0].contains("dry_run"), "{bodies:?}");
    assert_post_is_fixed_vocabulary(&bodies[0]);
    assert!(
        requests(&mock, "DELETE").is_empty(),
        "dry-run never removes a role"
    );

    // Evidence: three distinct audit IDs (the redelivery claimed
    // nothing new), first observed, trigger persisted as contain, and
    // exactly one completed dry-run incident at heat 6 whose trigger
    // is the contain row.
    let events = event_rows(&db.pool).await;
    assert_eq!(
        events.len(),
        3,
        "duplicate audit ID claims once: {events:?}"
    );
    assert_eq!(events[0].1, "observe", "first entry below threshold");
    assert_eq!(events[1].1, "contain", "trigger persisted as contain");
    let incidents = incident_rows(&db.pool).await;
    assert_eq!(incidents.len(), 1, "exactly one incident: {incidents:?}");
    assert_eq!(
        incidents[0].0, events[1].0,
        "incident trigger is the contain row"
    );
    assert_eq!(incidents[0].1, "dry_run");
    assert_eq!(incidents[0].2, 6, "two channel deletes weigh 3 + 3");
    assert!(incidents[0].4.is_some(), "dry-run incident completed");

    // Restart soak: a fresh worker over the same store replays every
    // entry. Nothing is re-claimed, re-posted or removed.
    run_worker(
        &db.pool,
        &mock,
        [entry(first), entry(trigger), entry(during_cooldown)],
        false,
    )
    .await;
    assert_eq!(
        posted_contents(&mock).len(),
        1,
        "restart replay posts nothing new"
    );
    assert!(requests(&mock, "DELETE").is_empty());
    assert_eq!(event_rows(&db.pool).await.len(), 3, "replay claims nothing");
    assert_eq!(
        incident_rows(&db.pool).await.len(),
        1,
        "replay opens no second incident"
    );

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_worker_soak_armed_happy_path_contains_and_persists() {
    let db = TestSchema::new().await;
    let mock = discord_open().await;

    run_worker(
        &db.pool,
        &mock,
        [entry(fresh_entry_id(0)), entry(fresh_entry_id(1_000))],
        true,
    )
    .await;

    let removed = requests(&mock, "DELETE");
    assert_eq!(removed.len(), 1, "one DELETE for the dangerous role");
    assert!(
        removed[0]
            .path
            .ends_with("/guilds/22/members/5001/roles/77"),
        "unexpected DELETE path: {}",
        removed[0].path
    );
    let bodies = posted_contents(&mock);
    assert_eq!(bodies.len(), 1, "one contained staff post");
    assert!(bodies[0].contains("contained"), "{bodies:?}");
    assert!(bodies[0].contains("`77`"), "{bodies:?}");
    assert_post_is_fixed_vocabulary(&bodies[0]);

    let incidents = incident_rows(&db.pool).await;
    assert_eq!(incidents.len(), 1, "exactly one incident: {incidents:?}");
    assert_eq!(incidents[0].1, "contained");
    assert!(incidents[0].4.is_some(), "contained incident completed");
    let events = event_rows(&db.pool).await;
    assert_eq!(events.len(), 2, "both entries persisted: {events:?}");
    assert_eq!(incidents[0].0, events[1].0, "trigger is the second entry");

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_worker_soak_preflight_refusal_persists_without_delete() {
    let db = TestSchema::new().await;
    let mock = discord_managed().await;

    run_worker(
        &db.pool,
        &mock,
        [entry(fresh_entry_id(0)), entry(fresh_entry_id(1_000))],
        true,
    )
    .await;

    assert!(
        requests(&mock, "DELETE").is_empty(),
        "a refused whole plan removes nothing, not even the safe prefix"
    );
    let bodies = posted_contents(&mock);
    assert_eq!(bodies.len(), 1, "one refused staff post");
    assert!(bodies[0].contains("refused"), "{bodies:?}");
    assert_post_is_fixed_vocabulary(&bodies[0]);

    let incidents = incident_rows(&db.pool).await;
    assert_eq!(incidents.len(), 1, "exactly one incident: {incidents:?}");
    assert_eq!(incidents[0].1, "refused", "preflight refusal persisted");

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_runtime_worker_soak_partial_failure_uncertain_never_retried() {
    let db = TestSchema::new().await;
    let mock = discord_partial().await;
    let (first, trigger, later) = (
        fresh_entry_id(0),
        fresh_entry_id(1_000),
        fresh_entry_id(2_000),
    );

    // The second DELETE fails upstream: the first role stays removed,
    // no third call runs, the outcome is uncertain, and the later
    // entry is blocked (uncertain never releases).
    run_worker(
        &db.pool,
        &mock,
        [entry(first), entry(trigger), entry(later)],
        true,
    )
    .await;

    let removed = requests(&mock, "DELETE");
    assert_eq!(removed.len(), 2, "stop after the first failure");
    assert!(
        removed[0].path.ends_with("/members/5001/roles/77"),
        "snapshot order first: {}",
        removed[0].path
    );
    assert!(
        removed[1].path.ends_with("/members/5001/roles/79"),
        "failing removal second: {}",
        removed[1].path
    );
    let bodies = posted_contents(&mock);
    assert_eq!(bodies.len(), 1, "uncertain alert once, then silence");
    assert!(bodies[0].contains("uncertain"), "{bodies:?}");
    assert!(bodies[0].contains("`77`"), "{bodies:?}");
    assert_post_is_fixed_vocabulary(&bodies[0]);

    let incidents = incident_rows(&db.pool).await;
    assert_eq!(incidents.len(), 1, "exactly one incident: {incidents:?}");
    assert_eq!(incidents[0].1, "uncertain", "partial failure persisted");
    assert!(
        !incidents[0]
            .3
            .as_deref()
            .unwrap_or_default()
            .contains(UPSTREAM_CANARY),
        "persisted result must be fixed-vocabulary: {incidents:?}"
    );

    // Restart soak: replaying every entry into a fresh worker retries
    // neither the failed removal nor the alert, and restores nothing.
    run_worker(
        &db.pool,
        &mock,
        [entry(first), entry(trigger), entry(later)],
        true,
    )
    .await;
    assert_eq!(
        requests(&mock, "DELETE").len(),
        2,
        "no automatic retry of the failed removal"
    );
    assert_eq!(posted_contents(&mock).len(), 1, "no second alert");
    assert!(
        requests(&mock, "PUT").is_empty() && requests(&mock, "PATCH").is_empty(),
        "no automatic restore of the removed role"
    );
    assert_eq!(
        incident_rows(&db.pool).await.len(),
        1,
        "uncertain still blocks after restart"
    );

    mock.shutdown().await;
    db.close().await;
}
