//! Mock-runtime acceptance: a nonempty game-picker submission whose keys
//! are all unrecognized must issue zero role PUT/DELETE calls, keep every
//! member role, and write no successful selection/routing facts. Explicit
//! empty (clear), mixed known/stale, known-only and dry-run submissions are
//! the passing controls. Isolated agent-testdb only; no live Discord.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use futures_util::FutureExt as _;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use twilight_model::application::interaction::Interaction;
use two_bot_core::onboarding::*;
use two_bot_discord::ActionExecutor;

use crate::gateway::ensure_crypto_provider;
use crate::onboarding::{OnboardingJob, OnboardingRuntime};

const TEST_DATABASE: &str = "postgres://agent_test@agent-testdb:5432/postgres";
const NOW: i64 = 1_790_781_200_000;

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
        let schema = format!("unknown_only_onboarding_{stamp}");
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
struct DiscordState {
    roles: HashSet<String>,
}

async fn discord(state: Arc<Mutex<DiscordState>>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let mut state = state.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/guilds/22") => {
                ScriptedResponse::json(200, json!({"id":"22","owner_id":"888"}))
            }
            ("GET", "/guilds/22/roles") => {
                let mut roles = vec![
                    json!({"id":"22","permissions":"3072"}),
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
                    json!({"user":{"id":id},"roles":if id == "999" { vec![] } else { state.roles.iter().cloned().collect::<Vec<_>>() }}),
                )
            }
            ("GET", path) if path.starts_with("/channels/") => {
                let id = path.strip_prefix("/channels/").unwrap();
                let overwrites = if id == GAME_PICKS[0].primary_channel_id.unwrap() {
                    json!([
                        {"id":"22","type":0,"allow":"0","deny":"1024"},
                        {"id":GAME_PICKS[0].role_id,"type":0,"allow":"1024","deny":"0"}
                    ])
                } else {
                    json!([])
                };
                ScriptedResponse::json(
                    200,
                    json!({"id":id,"guild_id":"22","type":if id == GAME_HUB_CHANNEL_ID { 15 } else { 0 },"permission_overwrites":overwrites}),
                )
            }
            ("PUT" | "DELETE", path) if path.contains("/roles/") => {
                let role = path.rsplit('/').next().unwrap();
                if request.method == "PUT" {
                    state.roles.insert(role.to_owned());
                } else {
                    state.roles.remove(role);
                }
                ScriptedResponse::status(204)
            }
            ("POST", path) if path.starts_with("/channels/") && path.ends_with("/messages") => {
                ScriptedResponse::json(201, json!({"id":"99"}))
            }
            ("POST", path) if path.ends_with("/callback") => ScriptedResponse::status(204),
            ("PATCH", path) if path.ends_with("/messages/@original") => {
                ScriptedResponse::json(200, json!({"id":"98"}))
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

fn component(values: &[&str]) -> OnboardingJob {
    let interaction: Interaction = serde_json::from_value(json!({
        "application_id":"111", "authorizing_integration_owners":{"0":"22"},
        "id":"333", "token":"mock-callback", "type":3,"version":1,"guild_id":"22",
        "member":{"user":{"id":"44","username":"member","discriminator":"0"},"roles":[],"deaf":false,"mute":false,"flags":0},
        "data":{"custom_id":GAME_SELECT_ID,"component_type":3,"values":values}
    }))
    .unwrap();
    OnboardingJob::Interaction(Box::new(interaction))
}

fn role_calls(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| {
            matches!(request.method.as_str(), "PUT" | "DELETE") && request.path.contains("/roles/")
        })
        .collect()
}

fn last_reply(mock: &MockRest) -> String {
    let replies: Vec<RestRequest> = mock
        .requests()
        .into_iter()
        .filter(|request| request.method == "PATCH")
        .collect();
    let body: Value = serde_json::from_slice(&replies.last().expect("a reply edit").body).unwrap();
    body["content"].as_str().expect("reply content").to_owned()
}

fn roles(state: &Arc<Mutex<DiscordState>>) -> HashSet<String> {
    state.lock().unwrap().roles.clone()
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_unknown_only_picker_preserves_roles_and_controls_still_apply() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let state = Arc::new(Mutex::new(DiscordState {
            roles: HashSet::from([GAME_PICKS[1].role_id.to_owned(), "77".into()]),
        }));
        let mock = discord(Arc::clone(&state)).await;
        let live = runtime(&db.pool, &mock, &vars("legacy", false));

        // Unknown-only: zero role calls, roles untouched, no funnel facts.
        live.handle(component(&["retired-game-key"]), NOW)
            .await
            .unwrap();
        assert_eq!(
            roles(&state),
            HashSet::from([GAME_PICKS[1].role_id.to_owned(), "77".into()]),
            "every member role survives an unknown-only submission"
        );
        assert_eq!(
            role_calls(&mock).len(),
            0,
            "unknown-only input must issue zero role PUT/DELETE calls"
        );
        assert_eq!(db.count(EVENT_GAME_ROLES_SELECTED).await, 0);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 0);
        let requests = mock.requests();
        let defer: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(defer["type"], 5);
        assert_eq!(defer["data"]["flags"], 64, "the answer stays ephemeral");
        let reply = last_reply(&mock);
        assert_ne!(
            reply, PICKER_CLEARED_REPLY,
            "unknown-only input must not be reported as a clear"
        );
        assert!(
            reply.to_lowercase().contains("again"),
            "reply must invite a retry from a fresh menu, got {reply:?}"
        );

        // Mixed control: the valid pick still applies, unrelated role kept.
        live.handle(component(&["retired-game-key", GAME_PICKS[0].key]), NOW + 1)
            .await
            .unwrap();
        assert_eq!(
            roles(&state),
            HashSet::from([GAME_PICKS[0].role_id.to_owned(), "77".into()]),
            "mixed input is not a blanket refusal"
        );
        assert_eq!(db.count(EVENT_GAME_ROLES_SELECTED).await, 1);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
        let calls_after_mixed = role_calls(&mock).len();
        assert!(calls_after_mixed > 0, "the mixed control writes roles");

        // Unknown-only again, now holding a freshly granted role.
        live.handle(component(&["retired-game-key"]), NOW + 2)
            .await
            .unwrap();
        assert_eq!(
            roles(&state),
            HashSet::from([GAME_PICKS[0].role_id.to_owned(), "77".into()])
        );
        assert_eq!(
            role_calls(&mock).len(),
            calls_after_mixed,
            "no further role calls for unknown-only input"
        );
        assert_eq!(db.count(EVENT_GAME_ROLES_SELECTED).await, 1);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);

        // Explicit-empty control: the clear branch is unchanged.
        live.handle(component(&[]), NOW + 3).await.unwrap();
        assert_eq!(
            roles(&state),
            HashSet::from(["77".into()]),
            "a raw empty selection still clears catalog roles only"
        );
        assert_eq!(
            db.count(EVENT_GAME_ROLES_SELECTED).await,
            1,
            "clear adds no selection row"
        );
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
        assert_eq!(last_reply(&mock), PICKER_CLEARED_REPLY);

        // Known-only control: the proven grant+route path still applies.
        live.handle(component(&[GAME_PICKS[0].key]), NOW + 4)
            .await
            .unwrap();
        assert_eq!(
            roles(&state),
            HashSet::from([GAME_PICKS[0].role_id.to_owned(), "77".into()])
        );
        assert_eq!(db.count(EVENT_GAME_ROLES_SELECTED).await, 2);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 2);

        // Dry-run control: unchanged, even for unknown-only input.
        let calls_before_dry = role_calls(&mock).len();
        runtime(&db.pool, &mock, &vars("legacy", true))
            .handle(component(&["retired-game-key"]), NOW + 5)
            .await
            .unwrap();
        assert_eq!(role_calls(&mock).len(), calls_before_dry);
        assert_eq!(last_reply(&mock), PICKER_DRY_RUN_REPLY);
        assert_eq!(
            roles(&state),
            HashSet::from([GAME_PICKS[0].role_id.to_owned(), "77".into()])
        );
        assert_eq!(db.count(EVENT_GAME_ROLES_SELECTED).await, 2);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 2);
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
