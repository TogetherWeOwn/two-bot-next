//! Join-risk runtime acceptance against the mock Discord double. The worker
//! test is ignored and runs only on agent-testdb / the CI service container.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use two_bot_core::join_risk_store::JoinRiskStore;
use two_bot_core::onboarding::MentionPolicy;
use two_bot_core::raid::{JoinRiskInput, JoinRiskPolicy};
use two_bot_core::RaidTuning;
use two_bot_cutover::parse::date_to_snowflake;
use two_bot_cutover::STAGING_GUILD_ID;
use two_bot_discord::{ActionExecutor, JoinObservation, JoinObserver};

use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::gateway::ensure_crypto_provider;
use crate::join_risk_runtime::{self, AntiNukeFences, Fanout, JoinRiskSettings, SettingsSource};

const GUILD: u64 = 22;
const CHANNEL: &str = "12";
const NOW: i64 = 1_790_780_400_000;
const TEST_DATABASE: &str = "postgres://agent_test@agent-testdb:5432/postgres";

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn staging(enabled: bool, dry_run: &str, mode: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if enabled {
        map.insert("TWO_ANTI_NUKE".to_owned(), "1".to_owned());
    }
    map.insert("TWO_ANTI_NUKE_DRY_RUN".to_owned(), dry_run.to_owned());
    map.insert("TWO_ONBOARDING_MODE".to_owned(), mode.to_owned());
    map
}

#[test]
fn join_risk_fences_need_exact_flag_on_the_staging_guild() {
    let staging_id: u64 = STAGING_GUILD_ID.parse().unwrap();
    // Enabled: exact "1" on staging.
    let fences = AntiNukeFences::from_vars(&staging(true, "1", "legacy"), staging_id);
    assert!(fences.enabled);

    // Anything but exact "1" stays off, even on staging.
    for bad in ["", "0", "true", " 1", "1 "] {
        let mut map = staging(false, "1", "legacy");
        map.insert("TWO_ANTI_NUKE".to_owned(), bad.to_owned());
        assert!(
            !AntiNukeFences::from_vars(&map, staging_id).enabled,
            "{bad:?}"
        );
    }
    // Exact "1" anywhere else stays off: the staging-only fence holds.
    assert!(!AntiNukeFences::from_vars(&staging(true, "1", "legacy"), 22).enabled);
    assert!(!AntiNukeFences::from_vars(&staging(true, "1", "legacy"), 0).enabled);
}

#[test]
fn join_risk_armed_needs_explicit_dry_run_zero_outside_session_mode() {
    let staging_id: u64 = STAGING_GUILD_ID.parse().unwrap();
    // Armed: dry-run explicitly "0", legacy mode.
    assert!(AntiNukeFences::from_vars(&staging(true, "0", "legacy"), staging_id).armed);
    assert!(AntiNukeFences::from_vars(&staging(true, "0", "anchor"), staging_id).armed);

    // Dry-run default and session mode refuse arming; proposals still flow.
    assert!(!AntiNukeFences::from_vars(&staging(true, "1", "legacy"), staging_id).armed);
    assert!(!AntiNukeFences::from_vars(&staging(true, "0", "session"), staging_id).armed);
    let mut unset = HashMap::new();
    unset.insert("TWO_ANTI_NUKE".to_owned(), "1".to_owned());
    assert!(!AntiNukeFences::from_vars(&unset, staging_id).armed);

    // Disabled means disarmed too.
    assert!(!AntiNukeFences::from_vars(&staging(false, "0", "legacy"), staging_id).armed);
}

#[test]
fn join_risk_settings_defaults_and_bad_values() {
    let defaults = JoinRiskSettings::from_vars(&vars(&[]));
    assert_eq!(
        defaults.tuning,
        RaidTuning::new(60.0, 5.0).unwrap(),
        "shipped join-risk defaults"
    );
    assert_eq!(defaults.bulk_join_window_until_ms, None);
    assert_eq!(defaults.staff_channel, None);

    let set = JoinRiskSettings::from_vars(&vars(&[
        ("TWO_JOIN_RISK_THRESHOLD", "8"),
        ("TWO_JOIN_RISK_WINDOW_SECONDS", "30.5"),
        ("TWO_BULK_JOIN_WINDOW_UNTIL", "1790780400000"),
        ("DISCORD_STAFF_ALERT_CHANNEL_ID", "123456789012345678"),
    ]));
    assert_eq!(set.tuning, RaidTuning::new(30.5, 8.0).unwrap());
    assert_eq!(set.bulk_join_window_until_ms, Some(1_790_780_400_000));
    assert_eq!(set.staff_channel.as_deref(), Some("123456789012345678"));

    // Unusable numbers never disable scoring or invent a bulk window.
    for bad in ["0", "-3", "abc", "NaN", "inf"] {
        let parsed = JoinRiskSettings::from_vars(&vars(&[
            ("TWO_JOIN_RISK_THRESHOLD", bad),
            ("TWO_JOIN_RISK_WINDOW_SECONDS", bad),
            ("TWO_BULK_JOIN_WINDOW_UNTIL", bad),
        ]));
        assert_eq!(parsed.tuning, RaidTuning::new(60.0, 5.0).unwrap(), "{bad}");
        assert_eq!(parsed.bulk_join_window_until_ms, None, "{bad}");
    }
    for bad in ["not-a-channel", "1234567890123456789012"] {
        let parsed = JoinRiskSettings::from_vars(&vars(&[("DISCORD_STAFF_ALERT_CHANNEL_ID", bad)]));
        assert_eq!(parsed.staff_channel, None, "{bad}");
    }
}

#[test]
fn join_risk_staff_message_only_for_persisted_flags_with_empty_mentions() {
    let policy = JoinRiskPolicy::new("22".to_owned(), 60.0, 5.0, None).unwrap();
    // One-hour-old account: flagged on the first join.
    let young = date_to_snowflake((NOW - 3_600_000) as u64);
    let input = JoinRiskInput {
        guild_id: "22".to_owned(),
        member_id: young,
        member_is_bot: false,
        account_created_at_ms: NOW - 3_600_000,
        joined_at_ms: Some(NOW),
        source: "unknown".to_owned(),
    };
    let evidence = policy
        .prepare(&input, NOW)
        .expect("young non-bot join prepares")
        .score(0);
    assert!(evidence.flagged);
    let message = evidence.staff_message(true).expect("persisted flag alerts");
    assert_eq!(message.mentions, MentionPolicy::None);
    assert!(message.content.contains("Join risk flag"));

    // Duplicates and unflagged evidence never alert, even when persisted.
    assert_eq!(evidence.staff_message(false), None);
    let old = JoinRiskInput {
        member_id: "1001".to_owned(),
        account_created_at_ms: 1_000_000,
        ..input.clone()
    };
    let calm = policy
        .prepare(&old, NOW)
        .expect("old-account join prepares")
        .score(0);
    assert!(!calm.flagged);
    assert_eq!(calm.staff_message(true), None);
}

#[test]
fn join_risk_bulk_window_keeps_the_row_but_drops_the_flag() {
    let policy = JoinRiskPolicy::new("22".to_owned(), 60.0, 5.0, Some(NOW + 60_000)).unwrap();
    let input = JoinRiskInput {
        guild_id: "22".to_owned(),
        member_id: date_to_snowflake((NOW - 3_600_000) as u64),
        member_is_bot: false,
        account_created_at_ms: NOW - 3_600_000,
        joined_at_ms: Some(NOW),
        source: "unknown".to_owned(),
    };
    let evidence = policy
        .prepare(&input, NOW)
        .expect("bulk-window join prepares")
        .score(0);
    assert!(evidence.score >= 3, "bulk does not erase the score");
    assert!(!evidence.flagged, "bulk suppresses the flag");
    assert_eq!(evidence.staff_message(true), None);
}

struct Rec {
    seen: std::sync::Arc<Mutex<Vec<u64>>>,
}

impl JoinObserver for Rec {
    fn observe_join(&self, join: JoinObservation) {
        self.seen.lock().unwrap().push(join.member_id);
    }
}

#[test]
fn join_risk_fanout_feeds_both_runtimes() {
    // The doubles share their recorders out-of-band, so the test never
    // downcasts the trait objects the fan-out holds.
    let first_seen = std::sync::Arc::new(Mutex::new(Vec::new()));
    let second_seen = std::sync::Arc::new(Mutex::new(Vec::new()));
    let fanout = Fanout {
        first: std::sync::Arc::new(Rec {
            seen: first_seen.clone(),
        }),
        second: std::sync::Arc::new(Rec {
            seen: second_seen.clone(),
        }),
    };
    fanout.observe_join(JoinObservation {
        guild_id: GUILD,
        member_id: 7,
        source: "unknown".into(),
        joined_at_ms: NOW,
    });
    assert_eq!(*first_seen.lock().unwrap(), vec![7]);
    assert_eq!(*second_seen.lock().unwrap(), vec![7]);
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
            ("GET", "/guilds/22") => {
                ScriptedResponse::json(200, json!({"id":"22","owner_id":"888"}))
            }
            ("GET", "/guilds/22/roles") => {
                let bits = if matches!(fixture, Fixture::ViewOnly) {
                    "1024"
                } else {
                    "3072"
                };
                ScriptedResponse::json(200, json!([{"id":"22","permissions":bits}]))
            }
            ("GET", "/guilds/22/members/999") => {
                ScriptedResponse::json(200, json!({"user":{"id":"999"},"roles":[]}))
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
    ActionExecutor::with_proxy("risk-test-only-token".into(), Some(mock.origin())).unwrap()
}

struct Scripted(JoinRiskSettings);

impl SettingsSource for Scripted {
    async fn current(&mut self) -> JoinRiskSettings {
        self.0.clone()
    }
}

fn settings(threshold: f64, channel: Option<&str>) -> JoinRiskSettings {
    JoinRiskSettings {
        tuning: RaidTuning::new(60.0, threshold).unwrap(),
        bulk_join_window_until_ms: None,
        staff_channel: channel.map(str::to_owned),
    }
}

fn young_id(age_ms: i64) -> u64 {
    date_to_snowflake((NOW - age_ms) as u64)
        .parse()
        .expect("snowflakes are numeric")
}

fn join(member_id: u64, at_ms: i64) -> JoinObservation {
    JoinObservation {
        guild_id: GUILD,
        member_id,
        source: "unknown".into(),
        joined_at_ms: at_ms,
    }
}

/// Feed `joins` through a started runtime, then drain it deterministically:
/// dropping the only sender lets the worker finish its queue and exit.
async fn run(
    source: impl SettingsSource,
    store: JoinRiskStore,
    mock: &MockRest,
    joins: impl IntoIterator<Item = JoinObservation>,
) {
    let (runtime, worker) = join_risk_runtime::start(GUILD, source, store, executor(mock));
    for observation in joins {
        runtime.observe_join(observation);
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
        let schema = format!("tog10430_risk_{stamp}");
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
            "../../cutover/migrations/0360_join_risk_flags.sql"
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
async fn join_risk_worker_posts_persisted_flags_once_and_ignores_the_rest() {
    let db = TestSchema::new().await;
    let store = JoinRiskStore::from_pool(db.pool.clone());
    let mock = discord(Fixture::Open).await;

    // One-hour-old account, threshold 5: the first join flags and posts once
    // with empty mentions; the identical replay is a duplicate and stays
    // silent, so a failed send could never become a second alert.
    let flagged = join(young_id(3_600_000), NOW);
    run(
        Scripted(settings(5.0, Some(CHANNEL))),
        store.clone(),
        &mock,
        [flagged.clone(), flagged],
    )
    .await;
    let bodies = posted_contents(&mock);
    assert_eq!(bodies.len(), 1, "exactly one staff post for claim + replay");
    assert!(bodies[0].contains("Join risk flag"), "{bodies:?}");

    // Five old-account joins reach the burst bonus (score 2) but never the
    // flag line: burst alone does not page staff.
    run(
        Scripted(settings(5.0, Some(CHANNEL))),
        store,
        &mock,
        (0..5).map(|n| join(2000 + n, NOW + 10_000 + n as i64 * 1000)),
    )
    .await;
    assert_eq!(
        posted_contents(&mock).len(),
        1,
        "old-account burst stays silent"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn join_risk_worker_without_send_permission_logs_only_and_never_retries() {
    let db = TestSchema::new().await;
    let store = JoinRiskStore::from_pool(db.pool.clone());
    let mock = discord(Fixture::ViewOnly).await;
    run(
        Scripted(settings(5.0, Some(CHANNEL))),
        store,
        &mock,
        [join(young_id(3_600_000), NOW)],
    )
    .await;
    assert!(posts(&mock).is_empty(), "denied channel posts nothing");
    mock.shutdown().await;
    db.close().await;
}
