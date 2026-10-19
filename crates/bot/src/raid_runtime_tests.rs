//! Raid runtime acceptance against the mock Discord double. The one database
//! test is ignored and runs only on agent-testdb / the CI service container.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use two_bot_core::RaidTuning;
use two_bot_cutover::settings::SettingsStore;
use two_bot_discord::{ActionExecutor, JoinObservation, JoinObserver};

use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::gateway::ensure_crypto_provider;
use crate::raid_runtime::{self, RaidSettings, SettingsSource, StoreSettings};

const GUILD: u64 = 22;
const CHANNEL: &str = "12";
const NOW: i64 = 1_790_780_400_000;
const TEST_DATABASE: &str = "postgres://agent_test@agent-testdb:5432/postgres";

#[derive(Clone, Copy)]
enum Fixture {
    /// The bot may View and Send in the staff channel.
    Open,
    /// The bot may View but not Send.
    ViewOnly,
    /// Permissions are fine but Discord rejects the post.
    PostRejected,
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
            ("POST", "/channels/12/messages") => {
                if matches!(fixture, Fixture::PostRejected) {
                    ScriptedResponse::status(403)
                } else {
                    ScriptedResponse::json(201, json!({"id":"99"}))
                }
            }
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    })
    .await
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ensure_crypto_provider();
    ActionExecutor::with_proxy("raid-test-only-token".into(), Some(mock.origin())).unwrap()
}

fn settings(threshold: f64, channel: Option<&str>) -> RaidSettings {
    RaidSettings {
        tuning: RaidTuning::new(60.0, threshold).unwrap(),
        staff_channel: channel.map(str::to_owned),
    }
}

/// Serves the given settings one observation at a time, repeating the last.
struct Scripted(Vec<RaidSettings>);

impl SettingsSource for Scripted {
    async fn current(&mut self) -> RaidSettings {
        if self.0.len() > 1 {
            self.0.remove(0)
        } else {
            self.0[0].clone()
        }
    }
}

/// Panics on its first call, then behaves.
struct PanicsOnce {
    panicked: bool,
    then: RaidSettings,
}

impl SettingsSource for PanicsOnce {
    async fn current(&mut self) -> RaidSettings {
        if !self.panicked {
            self.panicked = true;
            panic!("settings source failure (test)");
        }
        self.then.clone()
    }
}

fn join(guild_id: u64, member_id: u64, at_ms: i64) -> JoinObservation {
    JoinObservation {
        guild_id,
        member_id,
        source: "unknown".into(),
        joined_at_ms: at_ms,
    }
}

/// Feed `joins` through a started runtime, then drain it deterministically:
/// dropping the only sender lets the worker finish its queue and exit.
async fn run(
    source: impl SettingsSource,
    mock: &MockRest,
    joins: impl IntoIterator<Item = JoinObservation>,
) {
    let (runtime, worker) = raid_runtime::start(GUILD, source, executor(mock));
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

fn burst(count: u64, step_ms: i64) -> Vec<JoinObservation> {
    (0..count)
        .map(|n| join(GUILD, 1000 + n, NOW + n as i64 * step_ms))
        .collect()
}

#[test]
fn raid_runtime_settings_defaults_and_bad_values() {
    let vars = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    let defaults = RaidSettings::from_vars(&vars(&[]));
    assert_eq!(defaults.tuning, RaidTuning::default());
    assert_eq!(defaults.staff_channel, None);

    let set = RaidSettings::from_vars(&vars(&[
        ("TWO_RAID_JOIN_THRESHOLD", "8"),
        ("TWO_RAID_WINDOW_SECONDS", "30.5"),
        ("DISCORD_STAFF_ALERT_CHANNEL_ID", " 123456789012345678 "),
    ]));
    assert_eq!(set.tuning, RaidTuning::new(30.5, 8.0).unwrap());
    assert_eq!(set.staff_channel.as_deref(), Some("123456789012345678"));

    // Unusable values never disable the watch or guess a channel.
    for bad in ["0", "-3", "abc", "NaN", "inf"] {
        let parsed = RaidSettings::from_vars(&vars(&[
            ("TWO_RAID_JOIN_THRESHOLD", bad),
            ("TWO_RAID_WINDOW_SECONDS", bad),
        ]));
        assert_eq!(parsed.tuning, RaidTuning::default(), "{bad}");
    }
    for bad in ["12", "not-a-channel", "1234567890123456789012"] {
        let parsed = RaidSettings::from_vars(&vars(&[("DISCORD_STAFF_ALERT_CHANNEL_ID", bad)]));
        assert_eq!(parsed.staff_channel, None, "{bad}");
    }
}

#[tokio::test]
async fn raid_runtime_posts_one_alert_with_empty_mentions_then_honors_cooldown() {
    let mock = discord(Fixture::Open).await;
    // Seven joins, one second apart, threshold 3: the third alerts, the rest
    // sit inside the 900 s cooldown.
    run(
        Scripted(vec![settings(3.0, Some(CHANNEL))]),
        &mock,
        burst(7, 1000),
    )
    .await;
    let sent = posts(&mock);
    assert_eq!(sent.len(), 1, "one alert; cooldown suppresses the rest");
    assert_eq!(sent[0].path, "/api/v10/channels/12/messages");
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    let content = body["content"].as_str().unwrap();
    assert!(content.starts_with("**Join burst** - 3 accounts joined in 2s"));
    assert!(content.contains("`1000` `1001` `1002`"));
    assert!(content.contains("the bot has kicked, banned and messaged nobody"));
    assert_eq!(
        body["allowed_mentions"],
        json!({"parse": [], "roles": [], "users": [], "replied_user": false}),
        "staff alerts never ping anyone"
    );
    // Only the posting path ran: no member mutation of any kind.
    assert!(mock
        .requests()
        .iter()
        .all(|r| r.method == "GET" || r.path == "/api/v10/channels/12/messages"));
    mock.shutdown().await;
}

#[tokio::test]
async fn raid_runtime_without_a_channel_logs_only_and_calls_nothing() {
    let mock = discord(Fixture::Open).await;
    run(Scripted(vec![settings(3.0, None)]), &mock, burst(5, 1000)).await;
    assert!(mock.requests().is_empty(), "no channel: no Discord I/O");
    mock.shutdown().await;
}

#[tokio::test]
async fn raid_runtime_refuses_a_channel_the_bot_cannot_send_in() {
    let mock = discord(Fixture::ViewOnly).await;
    run(
        Scripted(vec![settings(3.0, Some(CHANNEL))]),
        &mock,
        burst(5, 1000),
    )
    .await;
    assert!(
        posts(&mock).is_empty(),
        "no Send permission: nothing posted"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn raid_runtime_never_retries_a_rejected_post() {
    let mock = discord(Fixture::PostRejected).await;
    run(
        Scripted(vec![settings(3.0, Some(CHANNEL))]),
        &mock,
        burst(8, 1000),
    )
    .await;
    // One attempt: the cooldown was consumed when the alert was proposed, so
    // neither a retry nor a later join produces a second post.
    assert_eq!(posts(&mock).len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn raid_runtime_applies_updated_tuning_to_the_next_join() {
    let mock = discord(Fixture::Open).await;
    // Threshold 10 for the first two joins, then 3 live: the third join sees
    // the new tuning against the two joins already in the window.
    run(
        Scripted(vec![
            settings(10.0, Some(CHANNEL)),
            settings(10.0, Some(CHANNEL)),
            settings(3.0, Some(CHANNEL)),
        ]),
        &mock,
        burst(3, 1000),
    )
    .await;
    assert_eq!(posts(&mock).len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn raid_runtime_ignores_other_guilds_and_rejoining_members() {
    let mock = discord(Fixture::Open).await;
    let mut joins = vec![
        join(23, 1, NOW),
        join(23, 2, NOW + 1000),
        join(23, 3, NOW + 2000),
    ];
    // The same member rejoining never advances the window or the count.
    joins.extend([join(GUILD, 7, NOW), join(GUILD, 7, NOW + 1000)]);
    joins.push(join(GUILD, 8, NOW + 2000));
    run(Scripted(vec![settings(3.0, Some(CHANNEL))]), &mock, joins).await;
    assert!(posts(&mock).is_empty(), "two distinct local members < 3");
    mock.shutdown().await;
}

#[tokio::test]
async fn raid_runtime_survives_an_internal_panic() {
    let mock = discord(Fixture::Open).await;
    // The first observation panics in its settings read and is lost; the
    // worker keeps serving, so the next three joins still alert.
    run(
        PanicsOnce {
            panicked: false,
            then: settings(3.0, Some(CHANNEL)),
        },
        &mock,
        burst(4, 1000),
    )
    .await;
    assert_eq!(posts(&mock).len(), 1);
    mock.shutdown().await;
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
        let schema = format!("tog10430_raid_{stamp}");
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
        for migration in [
            include_str!("../../cutover/migrations/0330_guild_settings.sql"),
            include_str!("../../cutover/migrations/0331_guild_settings_versions.sql"),
            include_str!("../../cutover/migrations/0332_guild_settings_allocator.sql"),
            include_str!("../../cutover/migrations/0333_guild_settings_revision.sql"),
            include_str!("../../cutover/migrations/0334_guild_settings_cas.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&pool).await.unwrap();
        }
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
async fn raid_runtime_store_tuning_and_channel_are_live() {
    let db = TestSchema::new().await;
    let guild = GUILD.to_string();
    let deployment: HashMap<String, String> = HashMap::from([
        ("TWO_RAID_JOIN_THRESHOLD".to_owned(), "7".to_owned()),
        (
            "DISCORD_STAFF_ALERT_CHANNEL_ID".to_owned(),
            "123456789012345678".to_owned(),
        ),
    ]);
    let mut source =
        StoreSettings::new(db.pool.clone(), GUILD, deployment).with_max_age(Duration::ZERO);

    let first = source.current().await;
    assert_eq!(
        first.tuning.threshold(),
        7.0,
        "deployment value, no row yet"
    );
    assert_eq!(first.staff_channel.as_deref(), Some("123456789012345678"));

    let store = SettingsStore::new(&db.pool);
    store
        .set(
            &guild,
            "TWO_RAID_JOIN_THRESHOLD",
            Some(json!(3)),
            "raid-test",
        )
        .await
        .unwrap();
    store
        .set(
            &guild,
            "TWO_RAID_WINDOW_SECONDS",
            Some(json!(30)),
            "raid-test",
        )
        .await
        .unwrap();
    store
        .set(
            &guild,
            "DISCORD_STAFF_ALERT_CHANNEL_ID",
            Some(json!("999999999999999999")),
            "raid-test",
        )
        .await
        .unwrap();
    let live = source.current().await;
    assert_eq!(live.tuning, RaidTuning::new(30.0, 3.0).unwrap());
    assert_eq!(
        live.staff_channel.as_deref(),
        Some("999999999999999999"),
        "the staff channel is hot-wired; a stored row moves it"
    );

    // Deleting the overrides hands the keys back to the deployment values.
    store
        .set(&guild, "TWO_RAID_JOIN_THRESHOLD", None, "raid-test")
        .await
        .unwrap();
    store
        .set(&guild, "DISCORD_STAFF_ALERT_CHANNEL_ID", None, "raid-test")
        .await
        .unwrap();
    let back = source.current().await;
    assert_eq!(back.tuning, RaidTuning::new(30.0, 7.0).unwrap());
    assert_eq!(back.staff_channel.as_deref(), Some("123456789012345678"));

    // A failed refresh keeps the last good values.
    db.pool.close().await;
    let kept = source.current().await;
    assert_eq!(kept, back);
    db.close().await;
}
