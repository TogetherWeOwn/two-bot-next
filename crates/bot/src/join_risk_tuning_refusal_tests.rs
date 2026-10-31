#![cfg(test)]

//! Acceptance tests: raid (join-risk) scoring refuses present-but-unusable
//! tuning instead of silently running on shipped defaults.
//!
//! Locked contract for the Builder (this file is immutable once handed off):
//!
//! - `JoinRiskSettings::try_from_vars(&vars) -> Result<JoinRiskSettings,
//!   JoinRiskTuningRefusal>` in `crate::join_risk_runtime` is the fallible
//!   reader the live settings path uses.
//! - A key that is **absent** keeps its existing default (tuning 60s / 5.0,
//!   no bulk window, log-only staff channel).
//! - A key that is **present but unusable** (unparsable, non-finite,
//!   non-positive, or a rejected tuning pair) makes the whole read refuse:
//!   no default is substituted for that key and no settings value is
//!   produced at all.
//! - The refusal is named per key family: `JoinRiskTuningRefusal::key()`
//!   returns the exact env/settings key that refused, and its `Display`
//!   output contains that key, so the log/metric signal names the bad key
//!   without echoing its value. When both tuning numbers are unusable the
//!   window key is named first (the existing evaluation order).
//! - On the arming/scoring path a refusal drops the observation with zero
//!   scoring side effects: no `join_risk_flags` row is claimed and no staff
//!   message is posted. Scoring never proceeds on defaults the operator
//!   did not ask for.
//!
//! Deliberately unpinned: present-but-empty (`""` / whitespace-only) values.
//! The Builder keeps or changes the current treat-as-absent behavior for
//! those; every non-empty unusable value above must refuse.
//!
//! Until the Builder lands `try_from_vars`, this file is intentionally RED
//! at compile time (the fallible reader does not exist yet).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use two_bot_core::join_risk_store::JoinRiskStore;
use two_bot_core::RaidTuning;
use two_bot_cutover::parse::date_to_snowflake;
use two_bot_discord::{ActionExecutor, JoinObservation, JoinObserver};

use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::gateway::ensure_crypto_provider;
use crate::join_risk_runtime::{self, JoinRiskSettings, StoreSettings};

const GUILD: u64 = 22;
const NOW: i64 = 1_790_780_400_000;
const TEST_DATABASE: &str = "postgres://agent_test@agent-testdb:5432/postgres";

const WINDOW_KEY: &str = "TWO_JOIN_RISK_WINDOW_SECONDS";
const THRESHOLD_KEY: &str = "TWO_JOIN_RISK_THRESHOLD";
const BULK_UNTIL_KEY: &str = "TWO_BULK_JOIN_WINDOW_UNTIL";
const CHANNEL_KEY: &str = "DISCORD_STAFF_ALERT_CHANNEL_ID";

const VALID_CHANNEL: &str = "123456789012345678";

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn refused_key(pairs: &[(&str, &str)]) -> String {
    let err = JoinRiskSettings::try_from_vars(&vars(pairs))
        .expect_err("present-but-unusable tuning must refuse, not default");
    assert!(
        format!("{err}").contains(err.key()),
        "the refusal's Display must name the refusing key: {err}"
    );
    err.key().to_owned()
}

#[test]
fn absent_keys_keep_the_shipped_defaults() {
    let settings = JoinRiskSettings::try_from_vars(&vars(&[]))
        .expect("absent keys are not a refusal; defaults apply");
    assert_eq!(settings.tuning, RaidTuning::new(60.0, 5.0).unwrap());
    assert_eq!(settings.bulk_join_window_until_ms, None);
    assert_eq!(settings.staff_channel, None);

    // One absent tuning number still defaults while its present partner is
    // honored: absence is per key, not all-or-nothing.
    let settings = JoinRiskSettings::try_from_vars(&vars(&[(THRESHOLD_KEY, "8")]))
        .expect("a lone valid threshold reads fine");
    assert_eq!(settings.tuning, RaidTuning::new(60.0, 8.0).unwrap());
    let settings = JoinRiskSettings::try_from_vars(&vars(&[(WINDOW_KEY, "30.5")]))
        .expect("a lone valid window reads fine");
    assert_eq!(settings.tuning, RaidTuning::new(30.5, 5.0).unwrap());
}

#[test]
fn valid_present_values_are_honored_verbatim() {
    let settings = JoinRiskSettings::try_from_vars(&vars(&[
        (WINDOW_KEY, "30.5"),
        (THRESHOLD_KEY, "8"),
        (BULK_UNTIL_KEY, "1790780400000"),
        (CHANNEL_KEY, VALID_CHANNEL),
    ]))
    .expect("every key present and usable reads fine");
    assert_eq!(settings.tuning, RaidTuning::new(30.5, 8.0).unwrap());
    assert_eq!(settings.bulk_join_window_until_ms, Some(1_790_780_400_000));
    assert_eq!(settings.staff_channel.as_deref(), Some(VALID_CHANNEL));
}

#[test]
fn unusable_window_refuses_and_names_the_window_key() {
    for bad in ["abc", "0", "-3", "NaN", "inf", "-inf", "1e999"] {
        assert_eq!(
            refused_key(&[(WINDOW_KEY, bad)]),
            WINDOW_KEY,
            "window {bad:?} must refuse as the window family, never default to 60s"
        );
    }
}

#[test]
fn unusable_threshold_refuses_and_names_the_threshold_key() {
    for bad in ["abc", "0", "-3", "NaN", "inf", "-inf", "1e999"] {
        assert_eq!(
            refused_key(&[(THRESHOLD_KEY, bad)]),
            THRESHOLD_KEY,
            "threshold {bad:?} must refuse as the threshold family, never default to 5.0"
        );
    }
}

#[test]
fn unusable_bulk_window_refuses_and_names_the_bulk_key() {
    for bad in ["abc", "0", "-3", "1.5", "NaN"] {
        assert_eq!(
            refused_key(&[(BULK_UNTIL_KEY, bad)]),
            BULK_UNTIL_KEY,
            "bulk-until {bad:?} must refuse as the bulk family, never read as no bulk window"
        );
    }
}

#[test]
fn unusable_staff_channel_refuses_and_names_the_channel_key() {
    for bad in [
        "not-a-channel",
        "123",
        "1234567890123456789012",
        "12345678901234567a",
    ] {
        assert_eq!(
            refused_key(&[(CHANNEL_KEY, bad)]),
            CHANNEL_KEY,
            "channel {bad:?} must refuse as the channel family, never read as log-only"
        );
    }
}

#[test]
fn rejected_tuning_pair_refuses_with_the_window_named_first() {
    // Both tuning numbers unusable: the pair is refused as a whole and the
    // window (evaluated first, as in `RaidTuning::new`) is the named key.
    // Neither number may be replaced by its shipped default.
    assert_eq!(
        refused_key(&[(WINDOW_KEY, "0"), (THRESHOLD_KEY, "-1")]),
        WINDOW_KEY
    );
    assert_eq!(
        refused_key(&[(WINDOW_KEY, "abc"), (THRESHOLD_KEY, "NaN")]),
        WINDOW_KEY
    );
}

#[test]
fn one_unusable_key_refuses_the_whole_read_even_when_the_rest_are_valid() {
    // No partial application: a single bad key must not produce settings
    // that mix operator values with silently substituted defaults.
    assert_eq!(
        refused_key(&[
            (WINDOW_KEY, "30.5"),
            (THRESHOLD_KEY, "8"),
            (BULK_UNTIL_KEY, "abc"),
            (CHANNEL_KEY, VALID_CHANNEL),
        ]),
        BULK_UNTIL_KEY
    );
    assert_eq!(
        refused_key(&[
            (WINDOW_KEY, "30.5"),
            (THRESHOLD_KEY, "0"),
            (BULK_UNTIL_KEY, "1790780400000"),
            (CHANNEL_KEY, VALID_CHANNEL),
        ]),
        THRESHOLD_KEY
    );
    assert_eq!(
        refused_key(&[
            (WINDOW_KEY, "30.5"),
            (THRESHOLD_KEY, "8"),
            (CHANNEL_KEY, "not-a-channel"),
        ]),
        CHANNEL_KEY
    );
    assert_eq!(
        refused_key(&[
            (WINDOW_KEY, "-30"),
            (THRESHOLD_KEY, "8"),
            (CHANNEL_KEY, VALID_CHANNEL),
        ]),
        WINDOW_KEY
    );
}

async fn discord() -> MockRest {
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
                ScriptedResponse::json(200, json!([{"id":"22","permissions":"3072"}]))
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

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

fn schema_name_at(nanos: u128) -> String {
    format!(
        "joinrisk_refusal_{}_{}_{}",
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
        // Same PID + counter + nanos scheme as join_risk_runtime_tests:
        // nanos can repeat across parallel tests sharing one test binary,
        // so the counter keeps same-tick names distinct and nanos keeps a
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

/// Arming path: the live settings source carries an unparsable threshold in
/// its deployment values. The observation that would normally flag and post
/// must instead be refused with zero scoring side effects — no
/// `join_risk_flags` row claimed, no staff post attempted. (The schema has
/// no settings tables, so the store refresh keeps the deployment values,
/// exactly the boot-with-a-typo case.)
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn join_risk_runtime_refused_tuning_scores_nothing_and_posts_nothing() {
    let db = TestSchema::new().await;
    let store = JoinRiskStore::from_pool(db.pool.clone());
    let mock = discord().await;
    ensure_crypto_provider();
    let executor =
        ActionExecutor::with_proxy("risk-test-only-token".into(), Some(mock.origin())).unwrap();

    let deployment = vars(&[(THRESHOLD_KEY, "abc")]);
    let source = StoreSettings::new(db.pool.clone(), GUILD, deployment);
    let (runtime, worker) = join_risk_runtime::start(GUILD, source, store, executor);

    // One-hour-old account: under the shipped defaults this join flags on
    // first sight, so any row or post proves scoring ran on defaults the
    // operator never asked for.
    let young: u64 = date_to_snowflake((NOW - 3_600_000) as u64)
        .parse()
        .expect("snowflakes are numeric");
    runtime.observe_join(JoinObservation {
        guild_id: GUILD,
        member_id: young,
        source: "unknown".into(),
        joined_at_ms: NOW,
    });
    drop(runtime);
    worker.await.expect("worker ends cleanly");

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM join_risk_flags")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "refused tuning must claim no join-risk row");
    assert!(
        mock.requests()
            .into_iter()
            .all(|request| request.method != "POST"),
        "refused tuning must post no staff message"
    );
    mock.shutdown().await;
    db.close().await;
}
