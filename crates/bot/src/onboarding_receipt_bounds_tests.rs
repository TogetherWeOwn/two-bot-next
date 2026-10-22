//! Builder-authored pins for the welcome receipt arms: a channel REST error
//! must fail loudly instead of skipping, a transient record rollback must
//! recover through the immediate record-only retry, and 32 unsettled receipts
//! must refuse new sends. Isolated agent-testdb only. No live inputs.
#![cfg(test)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use futures_util::FutureExt as _;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use two_bot_core::onboarding::*;
use two_bot_discord::ActionExecutor;

use crate::gateway::ensure_crypto_provider;
use crate::onboarding::{OnboardingJob, OnboardingRuntime, RuntimeError};

const TEST_DATABASE: &str = "postgres://agent_test@agent-testdb:5432/postgres";
const NOW: i64 = 1_790_780_400_000;

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

fn schema_name_at(nanos: u128) -> String {
    format!(
        "tog36967_receipt_bounds_{}_{}_{}",
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
        let pool = Self::pool(&schema).await;
        for migration in [
            include_str!("../../cutover/migrations/0001_funnel.sql"),
            include_str!("../../cutover/migrations/0190_onboarding.sql"),
            include_str!("../../cutover/migrations/0330_guild_settings.sql"),
            include_str!("../../cutover/migrations/0331_guild_settings_versions.sql"),
            include_str!("../../cutover/migrations/0332_guild_settings_allocator.sql"),
            include_str!("../../cutover/migrations/0333_guild_settings_revision.sql"),
            include_str!("../../cutover/migrations/0334_guild_settings_cas.sql"),
            // The onboarding writers consult the opt-out suppression table.
            include_str!("../../cutover/migrations/0433_privacy_opt_out.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&pool).await.unwrap();
        }
        Self { pool, schema }
    }

    async fn pool(schema: &str) -> PgPool {
        assert!(schema
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'));
        let schema = schema.to_owned();
        PgPoolOptions::new()
            .max_connections(4)
            .after_connect(move |connection, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(schema)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(TEST_DATABASE)
            .await
            .expect("only agent-testdb")
    }

    async fn count(&self, kind: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM events WHERE event_type = $1")
            .bind(kind)
            .fetch_one(&self.pool)
            .await
            .unwrap()
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
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_namespace WHERE nspname = $1")
                .bind(&self.schema)
                .fetch_one(&mut admin)
                .await
                .unwrap();
        assert_eq!(remaining, 0, "only this test's schema removed");
    }
}

#[derive(Default)]
struct BoundsState {
    roles: HashSet<String>,
    fail_channels: bool,
}

async fn discord(state: Arc<Mutex<BoundsState>>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let state = state.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/guilds/22") => {
                ScriptedResponse::json(200, json!({"id":"22","owner_id":"888"}))
            }
            ("GET", "/guilds/22/roles") => {
                let mut roles = vec![
                    json!({"id":"22","permissions":"1051648"}),
                    json!({"id":"77","permissions":"0"}),
                ];
                roles.extend(
                    GAME_PICKS
                        .iter()
                        .chain(PLATFORM_PICKS.iter())
                        .map(|pick| json!({"id":pick.role_id,"permissions":"0"})),
                );
                ScriptedResponse::json(200, Value::Array(roles))
            }
            ("GET", path) if path.starts_with("/guilds/22/members/") => {
                let id = path.strip_prefix("/guilds/22/members/").unwrap();
                ScriptedResponse::json(
                    200,
                    json!({"user":{"id":id},"roles":state.roles.iter().cloned().collect::<Vec<_>>()}),
                )
            }
            ("GET", path) if path.starts_with("/channels/") => {
                if state.fail_channels {
                    return ScriptedResponse::status(503);
                }
                let id = path.strip_prefix("/channels/").unwrap();
                ScriptedResponse::json(
                    200,
                    json!({"id":id,"guild_id":"22","type":0,"permission_overwrites":[]}),
                )
            }
            ("POST", path) if path.starts_with("/channels/") && path.ends_with("/messages") => {
                ScriptedResponse::json(201, json!({"id":"99"})).delayed(Duration::from_millis(25))
            }
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    })
    .await
}

fn vars(mode: &str, dry_run: bool) -> HashMap<String, String> {
    HashMap::from([
        ("DISCORD_GUILD_ID".into(), "22".into()),
        ("TWO_ONBOARDING_MODE".into(), mode.into()),
        (
            "TWO_ONBOARDING_DRY_RUN".into(),
            if dry_run { "1" } else { "0" }.into(),
        ),
        ("DISCORD_LANDING_CHANNEL_IDS".into(), "12".into()),
        ("DISCORD_GOODBYE_CHANNEL_IDS".into(), "13".into()),
        ("DISCORD_ANCHOR_WELCOME_CHANNEL_ID".into(), "14".into()),
        (
            "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID".into(),
            "10".into(),
        ),
        ("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID".into(), "11".into()),
    ])
}

fn runtime(pool: &PgPool, mock: &MockRest, vars: &HashMap<String, String>) -> OnboardingRuntime {
    ensure_crypto_provider();
    let executor =
        ActionExecutor::with_proxy("onboarding-test-only-token".into(), Some(mock.origin()))
            .unwrap();
    OnboardingRuntime::new(pool.clone(), executor, vars, 22, 999).unwrap()
}

fn welcome(member_id: u64) -> OnboardingJob {
    OnboardingJob::Welcome {
        guild_id: 22,
        member_id,
        bot: false,
        pending: false,
        trigger: MembershipTrigger::Joined { pending: false },
        roles: vec![],
    }
}

fn posts(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path.contains("/channels/"))
        .collect()
}

/// A channel REST error must fail the welcome loudly, never become a silent
/// skip, and a later attempt must succeed once the read heals.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_receipt_channel_rest_error_fails_loudly_then_recovers() {
    let state = Arc::new(Mutex::new(BoundsState {
        fail_channels: true,
        ..Default::default()
    }));
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = discord(state.clone()).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        let error = runtime
            .handle(welcome(44), NOW)
            .await
            .expect_err("channel REST error must fail, not skip");
        assert!(
            matches!(error, RuntimeError::Discord),
            "unexpected error: {error:?}"
        );
        assert!(posts(&mock).is_empty(), "failed channel read posts nothing");
        assert_eq!(db.count(EVENT_ONBOARDING_PROMPTED).await, 0);
        state.lock().unwrap().fail_channels = false;
        runtime
            .handle(welcome(44), NOW + 1)
            .await
            .expect("welcome posts once the channel read heals");
        assert_eq!(posts(&mock).len(), 1);
        assert_eq!(db.count(EVENT_ONBOARDING_PROMPTED).await, 1);
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

/// A transient record rollback must recover inside the first welcome through
/// the immediate record-only retry: one post, one marker, two record attempts.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_receipt_transient_record_rollback_recovers_in_one_welcome() {
    let state = Arc::new(Mutex::new(BoundsState::default()));
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        // Fail exactly the first prompted insert. The sequence is
        // non-transactional, so the flag survives the rolled-back record.
        sqlx::query("CREATE SEQUENCE record_fail_seq")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE OR REPLACE FUNCTION fail_first_prompted_insert() RETURNS trigger
             LANGUAGE plpgsql AS $failfn$
             BEGIN
               IF NEW.event_type = '{EVENT_ONBOARDING_PROMPTED}' THEN
                 IF nextval('record_fail_seq') = 1 THEN
                   RAISE EXCEPTION 'injected transient record failure';
                 END IF;
               END IF;
               RETURN NEW;
             END $failfn$"
        )))
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_first_prompted BEFORE INSERT ON events
             FOR EACH ROW EXECUTE FUNCTION fail_first_prompted_insert()",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let mock = discord(state.clone()).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime
            .handle(welcome(44), NOW)
            .await
            .expect("transient rollback recovers via the immediate record-only retry");
        assert_eq!(posts(&mock).len(), 1, "one accepted post");
        assert_eq!(db.count(EVENT_ONBOARDING_PROMPTED).await, 1);
        let attempts: i64 = sqlx::query_scalar("SELECT last_value FROM record_fail_seq")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            attempts, 2,
            "first record rolled back, immediate retry committed"
        );
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

/// 32 unsettled receipts must refuse the 33rd send before any post, and an
/// existing receipt must still reconcile record-only once recording heals.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_receipt_capacity_refuses_new_sends_at_32_unsettled() {
    let state = Arc::new(Mutex::new(BoundsState::default()));
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE events ADD CONSTRAINT refuse_prompted
             CHECK (event_type <> '{EVENT_ONBOARDING_PROMPTED}')"
        )))
        .execute(&db.pool)
        .await
        .unwrap();
        let mock = discord(state.clone()).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        for member in 5001..=5032 {
            assert!(
                runtime
                    .handle(welcome(member), NOW + member as i64)
                    .await
                    .is_err(),
                "persistent record failure keeps member {member} unsettled"
            );
        }
        assert_eq!(posts(&mock).len(), 32, "32 unsettled posts retained");
        assert!(
            runtime.handle(welcome(5033), NOW + 5033).await.is_err(),
            "33rd welcome refused at receipt capacity"
        );
        assert_eq!(posts(&mock).len(), 32, "capacity refusal posts nothing");
        sqlx::query("ALTER TABLE events DROP CONSTRAINT refuse_prompted")
            .execute(&db.pool)
            .await
            .unwrap();
        runtime
            .handle(welcome(5001), NOW + 6000)
            .await
            .expect("existing receipt reconciles record-only");
        assert_eq!(posts(&mock).len(), 32, "reconciliation never reposts");
        assert_eq!(db.count(EVENT_ONBOARDING_PROMPTED).await, 1);
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
