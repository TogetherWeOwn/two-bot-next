//! Containment runtime acceptance against the mock Discord double. The worker
//! tests are ignored and run only on agent-testdb / the CI service container.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use twilight_model::guild::audit_log::AuditLogEventType;
use two_bot_core::backup::guild_config::STAGING_BOT_APPLICATION_ID;
use two_bot_core::containment::DestructiveAction;
use two_bot_core::containment_store::ContainmentStore;
use two_bot_cutover::parse::date_to_snowflake;
use two_bot_discord::{ActionExecutor, AuditEntryObserver, AuditLogObservation};

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
        ("DISCORD_STAFF_ALERT_CHANNEL_ID", "12"),
    ]));
    assert_eq!(set.window_ms, 30_500);
    assert_eq!(set.max_age_ms, 90_000);
    assert_eq!(set.heat_threshold, 8);
    assert_eq!(set.staff_channel.as_deref(), Some("12"));

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
    for bad in ["not-a-channel", "1234567890123456789012"] {
        let parsed =
            ContainmentSettings::from_vars(&vars(&[("DISCORD_STAFF_ALERT_CHANNEL_ID", bad)]));
        assert_eq!(parsed.staff_channel, None, "{bad}");
    }
}

#[test]
fn containment_id_lists_split_commas_and_drop_empties() {
    assert!(id_list(&vars(&[]), "TWO_ANTI_NUKE_TRUSTED_USER_IDS").is_empty());
    let list = id_list(
        &vars(&[("TWO_ANTI_NUKE_TRUSTED_USER_IDS", " 1,2,, 3 ")]),
        "TWO_ANTI_NUKE_TRUSTED_USER_IDS",
    );
    assert_eq!(
        list,
        ["1", "2", "3"].into_iter().map(str::to_owned).collect()
    );
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
            ("GET", "/oauth2/applications/@me") => {
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
                        {"id":"88","permissions":"8","position":5,"managed":false},
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
) {
    let (runtime, worker) = containment_runtime::start(
        GUILD,
        source,
        store,
        executor(mock),
        HashSet::new(),
        HashSet::new(),
        false,
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
        let schema = format!("tog10430_contain_{stamp}");
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
    )
    .await;
    assert!(posts(&mock).is_empty(), "denied channel posts nothing");
    mock.shutdown().await;
    db.close().await;
}
