//! Containment runtime acceptance against the mock Discord double. The worker
//! tests are ignored and run only on agent-testdb / the CI service container.

#[path = "containment_tuning_refusal_acceptance_tests.rs"]
mod tuning_refusal_acceptance;

// Protected fail-open acceptance lives in its own file: a dropped
// quarantine outcome must surface as outcome-unknown with a metric. The
// implementation change must not edit the protected file.
#[path = "fail_open_containment_acceptance_tests.rs"]
mod fail_open_containment_acceptance;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use twilight_model::gateway::event::Event;
use twilight_model::gateway::payload::incoming::GuildAuditLogEntryCreate;
use twilight_model::guild::audit_log::{AuditLogEntry, AuditLogEventType};
use twilight_model::id::Id;
use two_bot_core::backup::guild_config::STAGING_BOT_APPLICATION_ID;
use two_bot_core::containment::{ContainmentPolicy, DestructiveAction};
use two_bot_core::containment_store::ContainmentStore;
use two_bot_cutover::parse::date_to_snowflake;
use two_bot_discord::{ActionExecutor, AuditEntryObserver, AuditLogObservation, MemPipeline};

use crate::containment_runtime::{self, id_list, map_action, ContainmentSettings, SettingsSource};
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::gateway::ensure_crypto_provider;

const GUILD: u64 = 22;
const CHANNEL: &str = "12";
const EXECUTOR: u64 = 5001;
const TEST_DATABASE: &str = "postgres://agent_test@agent-testdb:5432/postgres";

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

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

#[test]
fn containment_maps_only_destructive_audit_actions() {
    let cases = [
        (
            AuditLogEventType::MemberKick,
            Some(DestructiveAction::MemberKick),
        ),
        (
            AuditLogEventType::MemberBanAdd,
            Some(DestructiveAction::MemberBan),
        ),
        (
            AuditLogEventType::ChannelDelete,
            Some(DestructiveAction::ChannelDelete),
        ),
        (
            AuditLogEventType::RoleDelete,
            Some(DestructiveAction::RoleDelete),
        ),
        (
            AuditLogEventType::WebhookCreate,
            Some(DestructiveAction::WebhookCreate),
        ),
        (
            AuditLogEventType::WebhookUpdate,
            Some(DestructiveAction::WebhookUpdate),
        ),
        (
            AuditLogEventType::WebhookDelete,
            Some(DestructiveAction::WebhookDelete),
        ),
        (AuditLogEventType::MemberUpdate, None),
        (AuditLogEventType::ChannelCreate, None),
        (AuditLogEventType::RoleCreate, None),
    ];
    for (action, expected) in cases {
        assert_eq!(map_action(&action), expected);
    }
}

#[test]
fn containment_settings_defaults_and_bad_values() {
    let defaults = ContainmentSettings::from_vars(&vars(&[]));
    assert_eq!(defaults.window_ms, 60_000);
    assert_eq!(defaults.max_age_ms, 120_000);
    assert_eq!(defaults.heat_threshold, 5);
    assert_eq!(defaults.staff_channel, None);

    let set = ContainmentSettings::from_vars(&vars(&[
        ("TWO_ANTI_NUKE_WINDOW_SECONDS", "30.5"),
        ("TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS", "90"),
        ("TWO_ANTI_NUKE_HEAT_THRESHOLD", "8"),
        ("DISCORD_STAFF_ALERT_CHANNEL_ID", "123456789012345678"),
    ]));
    assert_eq!(set.window_ms, 30_500);
    assert_eq!(set.max_age_ms, 90_000);
    assert_eq!(set.heat_threshold, 8);
    assert_eq!(set.staff_channel.as_deref(), Some("123456789012345678"));

    // Unusable values never disable containment.
    for bad in ["0", "-3", "abc", "NaN", "inf"] {
        let parsed = ContainmentSettings::from_vars(&vars(&[
            ("TWO_ANTI_NUKE_WINDOW_SECONDS", bad),
            ("TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS", bad),
            ("TWO_ANTI_NUKE_HEAT_THRESHOLD", bad),
        ]));
        assert_eq!(parsed.window_ms, 60_000, "{bad}");
        assert_eq!(parsed.max_age_ms, 120_000, "{bad}");
        assert_eq!(parsed.heat_threshold, 5, "{bad}");
    }
    for bad in ["12", "not-a-channel", "1234567890123456789012"] {
        let parsed =
            ContainmentSettings::from_vars(&vars(&[("DISCORD_STAFF_ALERT_CHANNEL_ID", bad)]));
        assert_eq!(parsed.staff_channel, None, "{bad}");
    }
}

#[test]
fn containment_seconds_refuse_values_that_are_not_whole_positive_milliseconds() {
    let defaults = ContainmentSettings::from_vars(&vars(&[]));
    let cases: [(&str, Option<i64>); 13] = [
        ("0.0004", None),
        ("1e-9", None),
        ("0.0009999", None),
        ("0.001", Some(1)),
        ("0.0015", Some(1)),
        ("1.0005", Some(1_000)),
        ("9223372036854774", Some(((1_u64 << 63) - 2_048) as i64)),
        ("9223372036854775.808", None),
        ("9200000000000000", Some(9_200 * 10_i64.pow(15))),
        ("9300000000000000", None),
        ("1e300", None),
        ("1e306", None),
        ("1e308", None),
    ];
    for (raw, expected) in cases {
        let parsed = ContainmentSettings::from_vars(&vars(&[
            ("TWO_ANTI_NUKE_WINDOW_SECONDS", raw),
            ("TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS", raw),
        ]));
        assert_eq!(
            parsed.window_ms,
            expected.unwrap_or(defaults.window_ms),
            "window {raw}"
        );
        assert_eq!(
            parsed.max_age_ms,
            expected.unwrap_or(defaults.max_age_ms),
            "max age {raw}"
        );
        assert!(
            ContainmentPolicy::new(parsed.window_ms, parsed.max_age_ms, parsed.heat_threshold)
                .is_ok(),
            "{raw}"
        );
    }
}

#[test]
fn containment_id_lists_split_commas_and_drop_empties() {
    assert!(id_list(&vars(&[]), "TWO_ANTI_NUKE_TRUSTED_USER_IDS")
        .expect("empty list")
        .is_empty());
    let ids = ["1".repeat(18), "2".repeat(18), "3".repeat(18)];
    let raw = format!(" {},{},, {} ", ids[0], ids[1], ids[2]);
    let list = id_list(
        &vars(&[("TWO_ANTI_NUKE_TRUSTED_USER_IDS", &raw)]),
        "TWO_ANTI_NUKE_TRUSTED_USER_IDS",
    )
    .expect("valid snowflakes");
    assert_eq!(list, ids.into_iter().collect());
}

#[derive(Clone, Copy)]
enum Fixture {
    /// The bot may View and Send in the staff channel.
    Open,
    /// The bot may View but not Send.
    ViewOnly,
}

async fn discord(fixture: Fixture) -> MockRest {
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
            ("GET", "/guilds/22/roles") => {
                let bits = if matches!(fixture, Fixture::ViewOnly) {
                    "1024"
                } else {
                    "3072"
                };
                ScriptedResponse::json(
                    200,
                    json!([
                        {"id":"22","permissions":bits,"position":0,"managed":false},
                        {"id":"77","permissions":"8","position":1,"managed":false},
                        {"id":"88","permissions":"0","position":5,"managed":false},
                    ]),
                )
            }
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

fn settings(channel: Option<&str>) -> ContainmentSettings {
    ContainmentSettings {
        window_ms: 60_000,
        max_age_ms: 3_600_000,
        heat_threshold: 5,
        staff_channel: channel.map(str::to_owned),
    }
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

/// Feed `entries` through a started runtime, then drain it deterministically:
/// dropping the only sender lets the worker finish its queue and exit.
async fn run(
    source: impl SettingsSource,
    store: ContainmentStore,
    mock: &MockRest,
    entries: impl IntoIterator<Item = AuditLogObservation>,
    armed: bool,
) {
    let (runtime, worker) = containment_runtime::start(
        GUILD,
        source,
        store,
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

fn posts(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST")
        .collect()
}

fn posted_contents(mock: &MockRest) -> Vec<String> {
    posts(mock)
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

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

fn schema_name_at(nanos: u128) -> String {
    format!(
        "tog10430_contain_{}_{}_{}",
        std::process::id(),
        NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

#[test]
fn schema_names_are_distinct_when_the_clock_repeats() {
    assert_ne!(schema_name_at(42), schema_name_at(42));
}

struct TestSchema {
    pool: PgPool,
    schema: String,
}

impl TestSchema {
    async fn new() -> Self {
        // PID + counter + nanos: wall-clock nanos can repeat across parallel
        // tests on a coarse clock (run 39175965289 attempt 1: duplicate
        // schema `tog10430_*` -> 23505). The counter keeps same-tick
        // names distinct, the PID separates test binaries, and nanos keeps a
        // leaked schema plus PID reuse from ever repeating a name.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let schema = schema_name_at(nanos);
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

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_worker_posts_dry_run_once_then_suppresses_cooldown() {
    let db = TestSchema::new().await;
    let store = ContainmentStore::from_pool(db.pool.clone());
    let mock = discord(Fixture::Open).await;

    // Two channel deletes (3 + 3) reach heat 6 over threshold 5: the second
    // claim opens a dry-run incident and posts once with empty mentions. The
    // third entry lands inside the incident cooldown: suppressed, silent.
    run(
        Scripted(settings(Some(CHANNEL))),
        store,
        &mock,
        [
            entry(fresh_entry_id(0)),
            entry(fresh_entry_id(1_000)),
            entry(fresh_entry_id(2_000)),
        ],
        false,
    )
    .await;
    let bodies = posted_contents(&mock);
    assert_eq!(bodies.len(), 1, "one staff post, then cooldown silence");
    assert!(bodies[0].contains("Anti-nuke"), "{bodies:?}");
    assert!(bodies[0].contains("dry_run"), "{bodies:?}");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_worker_without_send_permission_logs_only() {
    let db = TestSchema::new().await;
    let store = ContainmentStore::from_pool(db.pool.clone());
    let mock = discord(Fixture::ViewOnly).await;
    run(
        Scripted(settings(Some(CHANNEL))),
        store,
        &mock,
        [entry(fresh_entry_id(0)), entry(fresh_entry_id(1_000))],
        false,
    )
    .await;
    assert!(posts(&mock).is_empty(), "denied channel posts nothing");
    mock.shutdown().await;
    db.close().await;
}

fn deletes(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "DELETE")
        .collect()
}

fn other_guild_entry(entry_id: u64) -> AuditLogObservation {
    AuditLogObservation {
        guild_id: GUILD + 77,
        entry_id,
        action: AuditLogEventType::ChannelDelete,
        executor_id: Some(EXECUTOR),
        target_id: Some(6001),
    }
}

/// Only the two verified-identity reads; anything else panics so a test
/// proves no claim, snapshot, removal or post ran past the fence.
async fn discord_identity_only(wrong_app: bool) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/users/@me") => {
                ScriptedResponse::json(200, json!({"id":"999","bot":true,"username":"bot"}))
            }
            ("GET", "/applications/@me") => {
                let id = if wrong_app {
                    "1"
                } else {
                    STAGING_BOT_APPLICATION_ID
                };
                ScriptedResponse::json(200, json!({"id": id}))
            }
            _ => panic!(
                "no REST beyond the identity reads: {} {}",
                request.method, path
            ),
        }
    })
    .await
}

/// Three removable roles in snapshot order; the second DELETE fails with a
/// 500 (uncertain) and the third must never run.
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
                ScriptedResponse::json(500, json!({"message": "upstream"}))
            }
            ("DELETE", path) if path.starts_with("/guilds/22/members/5001/roles/") => {
                panic!("no third removal after the failure: {path}")
            }
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    })
    .await
}

/// The executor's only dangerous role is managed: the whole plan is refused
/// and no DELETE may run.
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
async fn containment_worker_armed_removes_in_snapshot_order() {
    let db = TestSchema::new().await;
    let store = ContainmentStore::from_pool(db.pool.clone());
    let mock = discord(Fixture::Open).await;

    // Two channel deletes reach heat 6 over threshold 5: the armed worker
    // removes role 77 once, in snapshot order, and reports contained.
    run(
        Scripted(settings(Some(CHANNEL))),
        store,
        &mock,
        [entry(fresh_entry_id(0)), entry(fresh_entry_id(1_000))],
        true,
    )
    .await;
    let removed = deletes(&mock);
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
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_worker_armed_partial_failure_is_uncertain_and_blocks() {
    let db = TestSchema::new().await;
    let store = ContainmentStore::from_pool(db.pool.clone());
    let mock = discord_partial().await;

    // The second DELETE fails with a 500: the first role stays removed, no
    // third call runs, the outcome is uncertain, and the next entry is
    // blocked (uncertain never releases), so only one staff post exists.
    run(
        Scripted(settings(Some(CHANNEL))),
        store,
        &mock,
        [
            entry(fresh_entry_id(0)),
            entry(fresh_entry_id(1_000)),
            entry(fresh_entry_id(2_000)),
        ],
        true,
    )
    .await;
    let removed = deletes(&mock);
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
    assert_eq!(
        bodies.len(),
        1,
        "uncertain alert once, then cooldown silence"
    );
    assert!(bodies[0].contains("uncertain"), "{bodies:?}");
    assert!(bodies[0].contains("`77`"), "{bodies:?}");
    mock.shutdown().await;
    db.close().await;
}

/// The first guild-roles reads (one whole read budget) never answer; later
/// ones are prompt.
async fn discord_first_roles_read_stalls() -> MockRest {
    let roles_reads = std::sync::atomic::AtomicUsize::new(0);
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
            ("GET", "/guilds/22/roles") => {
                let roles = ScriptedResponse::json(
                    200,
                    json!([
                        {"id":"22","permissions":"3072","position":0,"managed":false},
                        {"id":"77","permissions":"8","position":1,"managed":false},
                        {"id":"88","permissions":"0","position":5,"managed":false},
                    ]),
                );
                if roles_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    < two_bot_core::MAX_HTTP_TRIES as usize
                {
                    roles.delayed(Duration::from_secs(600))
                } else {
                    roles
                }
            }
            ("GET", "/guilds/22/members/999") => {
                ScriptedResponse::json(200, json!({"user":{"id":"999"},"roles":["88"]}))
            }
            ("GET", "/guilds/22/members/5001") => {
                ScriptedResponse::json(200, json!({"user":{"id":"5001"},"roles":["77"]}))
            }
            ("GET", "/guilds/22/members/5002") => {
                ScriptedResponse::json(200, json!({"user":{"id":"5002"},"roles":["77"]}))
            }
            ("GET", "/channels/12") => ScriptedResponse::json(
                200,
                json!({"id":"12","guild_id":"22","type":0,"permission_overwrites":[]}),
            ),
            ("POST", "/channels/12/messages") => ScriptedResponse::json(201, json!({"id":"99"})),
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_worker_survives_a_stalled_roles_read_and_handles_the_next_entry() {
    let db = TestSchema::new().await;
    let store = ContainmentStore::from_pool(db.pool.clone());
    let mock = discord_first_roles_read_stalls().await;
    let other_executor = |entry_id| AuditLogObservation {
        executor_id: Some(5002),
        ..entry(entry_id)
    };

    // Executor 5001 opens an incident whose roles read never answers. The
    // worker must give up once the five-try read budget is spent (real time,
    // about 32.5 s: five 5 s bounds plus 0.5+1+2+4 s backoff), then plan
    // executor 5002's incident from the same queue.
    tokio::time::timeout(
        Duration::from_secs(60),
        run(
            Scripted(settings(Some(CHANNEL))),
            store,
            &mock,
            [
                entry(fresh_entry_id(0)),
                entry(fresh_entry_id(1_000)),
                other_executor(fresh_entry_id(2_000)),
                other_executor(fresh_entry_id(3_000)),
            ],
            false,
        ),
    )
    .await
    .expect("a stalled roles read must not park the containment worker");
    let bodies = posted_contents(&mock);
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(bodies[0].contains("failed"), "stalled incident: {bodies:?}");
    assert!(
        bodies[1].contains("dry_run"),
        "next entry planned: {bodies:?}"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_worker_armed_managed_role_refuses_without_delete() {
    let db = TestSchema::new().await;
    let store = ContainmentStore::from_pool(db.pool.clone());
    let mock = discord_managed().await;

    run(
        Scripted(settings(Some(CHANNEL))),
        store,
        &mock,
        [entry(fresh_entry_id(0)), entry(fresh_entry_id(1_000))],
        true,
    )
    .await;
    assert!(deletes(&mock).is_empty(), "refused plans never DELETE");
    let bodies = posted_contents(&mock);
    assert_eq!(bodies.len(), 1, "one refused staff post");
    assert!(bodies[0].contains("refused"), "{bodies:?}");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_worker_wrong_application_id_claims_nothing() {
    let db = TestSchema::new().await;
    let store = ContainmentStore::from_pool(db.pool.clone());
    let mock = discord_identity_only(true).await;

    run(
        Scripted(settings(Some(CHANNEL))),
        store,
        &mock,
        [entry(fresh_entry_id(0)), entry(fresh_entry_id(1_000))],
        true,
    )
    .await;
    assert!(posts(&mock).is_empty(), "refused identity posts nothing");
    assert!(deletes(&mock).is_empty(), "refused identity never DELETEs");
    for request in mock.requests() {
        assert_eq!(request.method, "GET", "no REST beyond the identity reads");
        assert!(
            request.path == "/api/v10/users/@me" || request.path == "/api/v10/applications/@me",
            "identity read only: {}",
            request.path
        );
    }
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn containment_worker_ignores_other_guild_entries() {
    let db = TestSchema::new().await;
    let store = ContainmentStore::from_pool(db.pool.clone());
    let mock = discord_identity_only(false).await;

    run(
        Scripted(settings(Some(CHANNEL))),
        store,
        &mock,
        [
            other_guild_entry(fresh_entry_id(0)),
            other_guild_entry(fresh_entry_id(1_000)),
        ],
        true,
    )
    .await;
    assert!(posts(&mock).is_empty(), "other guilds post nothing");
    assert!(deletes(&mock).is_empty(), "other guilds never DELETE");
    mock.shutdown().await;
    db.close().await;
}

struct RecordedEntries(Mutex<Vec<AuditLogObservation>>);

impl AuditEntryObserver for RecordedEntries {
    fn observe_audit_entry(&self, entry: AuditLogObservation) {
        self.0.lock().unwrap().push(entry);
    }
}

fn audit_event(guild_id: u64, entry_id: u64, executor: u64, target: u64) -> Event {
    Event::GuildAuditLogEntryCreate(Box::new(GuildAuditLogEntryCreate(AuditLogEntry {
        action_type: AuditLogEventType::ChannelDelete,
        changes: Vec::new(),
        guild_id: Some(Id::new(guild_id)),
        id: Id::new(entry_id),
        options: None,
        reason: None,
        target_id: Some(Id::new(target)),
        user_id: Some(Id::new(executor)),
    })))
}

/// Pipeline-level observer seam: an audit-log entry reaches the observer once
/// with executor and target mapped, registration is first-wins, and audit
/// entries write no funnel row.
#[test]
fn pipeline_audit_entry_observer_sees_entry_once_and_first_wins() {
    let pipeline = MemPipeline::for_replay();
    pipeline.handle(&audit_event(GUILD, 111, EXECUTOR, 6001));
    assert!(
        pipeline.handlers().store().rows().is_empty(),
        "audit entries write no funnel row"
    );

    let first = Arc::new(RecordedEntries(Mutex::new(Vec::new())));
    let second = Arc::new(RecordedEntries(Mutex::new(Vec::new())));
    pipeline.set_audit_entry_observer(first.clone());
    pipeline.set_audit_entry_observer(second.clone());
    let entry_id = 222;
    pipeline.handle(&audit_event(GUILD, entry_id, EXECUTOR, 6001));

    let seen = first.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "the observer sees the entry once");
    assert_eq!(seen[0].guild_id, GUILD);
    assert_eq!(seen[0].entry_id, entry_id);
    assert_eq!(seen[0].executor_id, Some(EXECUTOR));
    assert_eq!(seen[0].target_id, Some(6001));
    assert!(matches!(&seen[0].action, AuditLogEventType::ChannelDelete));
    assert!(
        second.0.lock().unwrap().is_empty(),
        "registration is first-wins"
    );
    assert!(
        pipeline.handlers().store().rows().is_empty(),
        "audit entries write no funnel row"
    );
}
