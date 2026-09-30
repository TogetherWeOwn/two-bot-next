//! Combined shared-router/REST and isolated agent-testdb acceptance. No live inputs.
#![allow(dead_code)]
#[path = "../../discord/tests/common/mod.rs"]
mod common;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{MockRest, RestRequest, ScriptedResponse};
use futures_util::FutureExt as _;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use twilight_model::application::interaction::Interaction;
use twilight_model::gateway::event::Event;
use two_bot_core::onboarding::*;
use two_bot_core::onboarding_store::has_onboarding_prompt;
use two_bot_cutover::settings::SettingsStore;
use two_bot_discord::ActionExecutor;

use crate::gateway::{build_pipeline, ensure_crypto_provider};
use crate::onboarding::{OnboardingJob, OnboardingRuntime};

const TEST_DATABASE: &str = "postgres://agent_test@agent-testdb:5432/postgres";
const NOW: i64 = 1_790_780_400_000;

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
        let schema = format!("tog10278_onboarding_{stamp}");
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
        ] {
            sqlx::raw_sql(migration).execute(&pool).await.unwrap();
        }
        Self { pool, schema }
    }

    async fn pool(schema: &str) -> PgPool {
        Self::pool_with_lock_timeout(schema, "0").await
    }

    async fn pool_with_lock_timeout(schema: &str, lock_timeout: &str) -> PgPool {
        assert!(schema
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'));
        let schema = schema.to_owned();
        let lock_timeout = lock_timeout.to_owned();
        PgPoolOptions::new()
            .max_connections(4)
            .after_connect(move |connection, _| {
                let schema = schema.clone();
                let lock_timeout = lock_timeout.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false), set_config('lock_timeout', $2, false)")
                        .bind(schema)
                        .bind(lock_timeout)
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
    reject_next_post: bool,
    reject_roles: bool,
    member_reads: usize,
    reject_member_read: Option<usize>,
    reject_next_reply: bool,
    reject_replies: bool,
    reject_callback: bool,
    reply_delay: Duration,
    close_pool_on_callback: Option<PgPool>,
}

async fn discord(state: Arc<Mutex<DiscordState>>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let mut state = state.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/guilds/22") => ScriptedResponse::json(200, json!({"id":"22","owner_id":"888"})),
            ("GET", "/guilds/22/roles") => {
                let mut roles = vec![json!({"id":"22","permissions":"3072"}), json!({"id":"77","permissions":"0"})];
                roles.extend(GAME_PICKS.iter().chain(PLATFORM_PICKS.iter()).map(|pick| json!({"id":pick.role_id,"permissions":"0"})));
                ScriptedResponse::json(200, Value::Array(roles))
            }
            ("GET", path) if path.starts_with("/guilds/22/members/") => {
                let id = path.strip_prefix("/guilds/22/members/").unwrap();
                if id == "44" {
                    state.member_reads += 1;
                    if state.reject_member_read == Some(state.member_reads) {
                        return ScriptedResponse::status(403);
                    }
                }
                ScriptedResponse::json(200, json!({"user":{"id":id},"roles":if id == "999" { vec![] } else { state.roles.iter().cloned().collect::<Vec<_>>() }}))
            }
            ("GET", path) if path.starts_with("/channels/") => {
                let id = path.strip_prefix("/channels/").unwrap();
                let overwrites = if id == GAME_PICKS[0].primary_channel_id.unwrap() {
                    json!([
                        {"id":"22","type":0,"allow":"0","deny":"1024"},
                        {"id":GAME_PICKS[0].role_id,"type":0,"allow":"1024","deny":"0"}
                    ])
                } else if id == "17" {
                    json!([{"id":"999","type":1,"allow":"0","deny":"2048"}])
                } else { json!([]) };
                ScriptedResponse::json(200, json!({"id":id,"guild_id":if id == "15" { "23" } else { "22" }, "type":if id == "16" { 1 } else if matches!(id, "11" | "14") { 2 } else { 0 },"permission_overwrites":overwrites}))
            }
            ("PUT" | "DELETE", path) if path.contains("/roles/") => {
                if state.reject_roles { return ScriptedResponse::status(403); }
                let role = path.rsplit('/').next().unwrap();
                if request.method == "PUT" { state.roles.insert(role.to_owned()); } else { state.roles.remove(role); }
                ScriptedResponse::status(204)
            }
            ("POST", path) if path.starts_with("/channels/") && path.ends_with("/messages") => {
                if state.reject_next_post { state.reject_next_post = false; return ScriptedResponse::status(403); }
                ScriptedResponse::json(201, json!({"id":"99"})).delayed(Duration::from_millis(25))
            }
            ("POST", path) if path.ends_with("/callback") => {
                if state.reject_callback { return ScriptedResponse::status(403); }
                if let Some(pool) = state.close_pool_on_callback.take() {
                    tokio::spawn(async move { pool.close().await });
                    return ScriptedResponse::status(204).delayed(Duration::from_millis(100));
                }
                ScriptedResponse::status(204)
            }
            ("PATCH", path) if path.ends_with("/messages/@original") => {
                if state.reject_replies || state.reject_next_reply {
                    state.reject_next_reply = false;
                    return ScriptedResponse::status(403);
                }
                ScriptedResponse::json(200, json!({"id":"98"})).delayed(state.reply_delay)
            }
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    }).await
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

fn component(custom_id: &str, values: &[&str]) -> OnboardingJob {
    let interaction: Interaction = serde_json::from_value(json!({
        "application_id":"111", "authorizing_integration_owners":{"0":"22"},
        "id":"333", "token":"mock-callback", "type":3,"version":1,"guild_id":"22",
        "member":{"user":{"id":"44","username":"member","discriminator":"0"},"roles":[],"deaf":false,"mute":false,"flags":0},
        "data":{"custom_id":custom_id,"component_type":3,"values":values}
    })).unwrap();
    OnboardingJob::Interaction(Box::new(interaction))
}

fn posts(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path.contains("/channels/"))
        .collect()
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_modes_dry_run_anchor_and_redelivery() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
    let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
    let mut expected_posts = 0;
    for (index, (mode, dry_run)) in [("legacy",false),("legacy",true),("session",false),("session",true),("anchor",false),("anchor",true)].into_iter().enumerate() {
        let runtime = runtime(&db.pool, &mock, &vars(mode, dry_run));
        let member_id = 44 + index as u64;
        runtime.handle(welcome(member_id), NOW).await.unwrap();
        let sent = !dry_run || mode == "session";
        expected_posts += usize::from(sent);
        assert_eq!(posts(&mock).len(), expected_posts);
        assert_eq!(has_onboarding_prompt(&db.pool,"22",&member_id.to_string()).await.unwrap(),sent);
        runtime.handle(welcome(member_id), NOW + 1).await.unwrap();
        assert_eq!(posts(&mock).len(), expected_posts, "successful prompt is durable");
        if sent {
            let request = posts(&mock).pop().unwrap();
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["allowed_mentions"],json!({"parse":[],"users":[member_id.to_string()],"roles":[],"replied_user":false}));
            if mode == "anchor" {
                assert_eq!(request.path,"/api/v10/channels/14/messages");
                assert!(body.get("components").is_none());
                assert!(body.get("embeds").is_none());
                assert!(body["content"].as_str().unwrap().contains("<#14>"));
                assert!(!body["content"].as_str().unwrap().contains(SUNDAY_SQUAD.channel_id));
            } else {
                assert_eq!(body["components"][0]["components"][0]["custom_id"],if mode == "legacy" { GAME_SELECT_ID } else { SESSION_SELECT_ID });
            }
        }
    }
    assert_eq!(db.count(EVENT_ONBOARDING_PROMPTED).await,4);
    assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await,1,"only the successful anchor welcome routes");
    assert_eq!(db.count("member_join").await,0,"S3, not onboarding, owns join facts");
    }).catch_unwind().await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_failed_send_retry_and_concurrent_process_guards() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let state = Arc::new(Mutex::new(DiscordState {
            reject_next_post: true,
            ..Default::default()
        }));
        let mock = discord(state).await;
        let first = Arc::new(runtime(&db.pool, &mock, &vars("anchor", false)));
        assert!(first.handle(welcome(44), NOW).await.is_err());
        assert!(!has_onboarding_prompt(&db.pool, "22", "44").await.unwrap());
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 0);
        let second_pool = TestSchema::pool(&db.schema).await;
        let second = Arc::new(runtime(&second_pool, &mock, &vars("anchor", false)));
        let mut jobs = vec![];
        for i in 0..12 {
            let runtime = Arc::clone(if i % 2 == 0 { &first } else { &second });
            jobs.push(tokio::spawn(async move {
                runtime.handle(welcome(44), NOW + 1).await
            }));
        }
        for job in jobs {
            job.await.unwrap().unwrap();
        }
        assert_eq!(
            posts(&mock).len(),
            2,
            "one rejected post, one accepted post across two pools"
        );
        assert_eq!(db.count(EVENT_ONBOARDING_PROMPTED).await, 1);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
        let transactions: Vec<String> =
            sqlx::query_scalar("SELECT xmin::text FROM events WHERE member_id = '44'")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(transactions.len(), 2);
        assert_eq!(
            transactions[0], transactions[1],
            "anchor rows commit together"
        );
        first.handle(welcome(44), NOW + 2).await.unwrap();
        assert_eq!(posts(&mock).len(), 2);
        second_pool.close().await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_game_role_match_post_grant_routing_clear_and_dry_run() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let state = Arc::new(Mutex::new(DiscordState {
            roles: HashSet::from([GAME_PICKS[1].role_id.to_owned(), "77".into()]),
            ..Default::default()
        }));
        let mock = discord(Arc::clone(&state)).await;
        let live = runtime(&db.pool, &mock, &vars("legacy", false));
        live.handle(component(GAME_SELECT_ID, &["shooters"]), NOW)
            .await
            .unwrap();
        assert_eq!(
            state.lock().unwrap().roles,
            HashSet::from([GAME_PICKS[0].role_id.to_owned(), "77".into()])
        );
        let requests = mock.requests();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["type"], 5);
        assert_eq!(body["data"]["flags"], 64, "ephemeral before role/DB work");
        let grant = requests
            .iter()
            .position(|request| request.method == "PUT")
            .unwrap();
        let remove = requests
            .iter()
            .position(|request| request.method == "DELETE")
            .unwrap();
        let visibility = requests
            .iter()
            .position(|request| {
                request.path
                    == format!(
                        "/api/v10/channels/{}",
                        GAME_PICKS[0].primary_channel_id.unwrap()
                    )
            })
            .unwrap();
        assert!(
            visibility > grant && visibility > remove,
            "fresh visibility follows all role effects"
        );
        let reply = requests.last().unwrap();
        assert_eq!(reply.method, "PATCH");
        assert!(reply.header("authorization").is_none());
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert!(body["content"]
            .as_str()
            .unwrap()
            .contains(GAME_PICKS[0].primary_channel_id.unwrap()));
        assert!(!body["content"]
            .as_str()
            .unwrap()
            .contains(GAME_HUB_CHANNEL_ID));
        assert_eq!(db.count(EVENT_GAME_ROLES_SELECTED).await, 1);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
        live.handle(component(GAME_SELECT_ID, &[]), NOW + 1)
            .await
            .unwrap();
        assert_eq!(state.lock().unwrap().roles, HashSet::from(["77".into()]));
        assert_eq!(
            db.count(EVENT_GAME_ROLES_SELECTED).await,
            1,
            "clear adds neither row"
        );
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
        let before = mock.requests().len();
        runtime(&db.pool, &mock, &vars("legacy", true))
            .handle(component(GAME_SELECT_ID, &["shooters"]), NOW + 2)
            .await
            .unwrap();
        let after = mock.requests();
        assert_eq!(after.len() - before, 2, "dry run only defers and edits");
        let reply: Value = serde_json::from_slice(&after.last().unwrap().body).unwrap();
        assert_eq!(reply["content"], PICKER_DRY_RUN_REPLY);
        assert!(posts(&mock).is_empty());
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_failed_role_writes_record_nothing() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = discord(Arc::new(Mutex::new(DiscordState {
            reject_roles: true,
            ..Default::default()
        })))
        .await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        runtime
            .handle(component(GAME_SELECT_ID, &["shooters"]), NOW)
            .await
            .unwrap();
        assert_eq!(db.count(EVENT_GAME_ROLES_SELECTED).await, 0);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 0);
        let requests = mock.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method == "PATCH")
                .count(),
            1,
            "role failure is handled once and is terminal after the error edit",
        );
        let reply: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        assert_eq!(reply["content"], PICKER_ROLE_FAILURE_REPLY);
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_roleless_session_reselection_stale_menu_and_goodbye_mentions() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime
            .handle(component(GAME_SELECT_ID, &["shooters"]), NOW)
            .await
            .unwrap();
        assert!(
            mock.requests().is_empty(),
            "mode-exclusive shared router gates"
        );
        runtime
            .handle(component(SESSION_SELECT_ID, &["find-players"]), NOW)
            .await
            .unwrap();
        runtime
            .handle(component(SESSION_SELECT_ID, &["find-players"]), NOW + 1)
            .await
            .unwrap();
        runtime
            .handle(
                component(SESSION_SELECT_ID, &["find-players", "unknown"]),
                NOW + 2,
            )
            .await
            .unwrap();
        assert_eq!(
            db.count(EVENT_CHANNEL_ROUTED).await,
            2,
            "unknown selection routes nowhere"
        );
        let sources: Vec<String> = sqlx::query_scalar("SELECT source FROM events")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert!(sources.iter().all(|source| source == "session-picker"));
        assert_eq!(db.count(EVENT_GAME_ROLES_SELECTED).await, 0);
        runtime
            .handle(
                OnboardingJob::Goodbye {
                    guild_id: 22,
                    username: "@everyone <@44> <@&77>".into(),
                    bot: false,
                    joined_at_ms: Some(NOW - 2 * 86_400_000),
                },
                NOW + 3,
            )
            .await
            .unwrap();
        let request = posts(&mock).pop().unwrap();
        assert_eq!(request.path, "/api/v10/channels/13/messages");
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
        assert_eq!(body["allowed_mentions"]["users"], json!([]));
        assert_eq!(body["allowed_mentions"]["roles"], json!([]));
        assert!(body["content"].as_str().unwrap().contains("2 days"));
        assert!(
            !mock
                .requests()
                .iter()
                .any(|request| matches!(request.method.as_str(), "PUT" | "DELETE")),
            "no game or leveling reward role writes in session mode"
        );
        assert_eq!(
            db.count("member_leave").await,
            0,
            "leave funnel remains single-owned"
        );
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_hot_settings_and_live_channel_fences() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
        let mut deployment = vars("legacy", false);
        deployment.insert("DISCORD_LANDING_CHANNEL_IDS".into(), "15,16,17,12".into());
        let runtime = runtime(&db.pool, &mock, &deployment);
        runtime.handle(welcome(44), NOW).await.unwrap();
        assert_eq!(
            posts(&mock)[0].path,
            "/api/v10/channels/12/messages",
            "foreign guild, DM, and bot send denial are rejected in order"
        );
        SettingsStore::new(&db.pool)
            .set(
                "22",
                "DISCORD_LANDING_CHANNEL_IDS",
                Some(json!(["13"])),
                "test",
            )
            .await
            .unwrap();
        runtime.handle(welcome(45), NOW + 1).await.unwrap();
        assert_eq!(
            posts(&mock)[1].path,
            "/api/v10/channels/13/messages",
            "store-first live override"
        );
        SettingsStore::new(&db.pool)
            .set("22", "DISCORD_LANDING_CHANNEL_IDS", None, "test")
            .await
            .unwrap();
        SettingsStore::new(&db.pool)
            .set("22", "TWO_ONBOARDING_DRY_RUN", Some(json!(true)), "test")
            .await
            .unwrap();
        runtime.handle(welcome(46), NOW + 2).await.unwrap();
        assert_eq!(
            posts(&mock).len(),
            2,
            "hot dry run suppresses legacy welcome"
        );
        SettingsStore::new(&db.pool)
            .set("22", "TWO_ONBOARDING_DRY_RUN", None, "test")
            .await
            .unwrap();
        runtime.handle(welcome(46), NOW + 3).await.unwrap();
        assert_eq!(
            posts(&mock)[2].path,
            "/api/v10/channels/12/messages",
            "delete restores deployment fallback"
        );
        runtime
            .handle(
                OnboardingJob::Welcome {
                    guild_id: 23,
                    member_id: 47,
                    bot: false,
                    pending: false,
                    trigger: MembershipTrigger::GateCleared,
                    roles: vec![],
                },
                NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            posts(&mock).len(),
            3,
            "configured guild check survives direct job calls"
        );
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

fn replies(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "PATCH")
        .collect()
}

async fn assert_picker_failure(db: &TestSchema, mock: &MockRest, reply_count: usize) {
    assert_eq!(db.count(EVENT_GAME_ROLES_SELECTED).await, 0);
    assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 0);
    let requests = mock.requests();
    let defer: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(defer["type"], 5);
    assert_eq!(defer["data"]["flags"], 64);
    let replies = replies(mock);
    assert_eq!(replies.len(), reply_count);
    let reply = replies.last().unwrap();
    assert_eq!(
        reply.path,
        "/api/v10/webhooks/111/mock-callback/messages/@original"
    );
    assert!(reply.header("authorization").is_none());
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert!(body["content"].as_str().unwrap().contains("couldn't"));
    assert!(!body["content"].as_str().unwrap().contains("Done."));
    assert_eq!(
        body["allowed_mentions"],
        json!({"parse":[],"users":[],"roles":[],"replied_user":false})
    );
    assert!(posts(mock).is_empty());
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_anchor_route_insert_failure_rolls_back_prompt() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        sqlx::query(
            "ALTER TABLE events ADD CONSTRAINT reject_route CHECK (event_type <> 'channel_routed')",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("anchor", false));
        assert!(runtime.handle(welcome(44), NOW).await.is_err());
        assert_eq!(posts(&mock).len(), 1, "Discord accepted the anchor welcome");
        assert_eq!(db.count(EVENT_ONBOARDING_PROMPTED).await, 0);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 0);
        let guard = two_bot_core::onboarding_store::begin_prompt(&db.pool, "22", "44")
            .await
            .unwrap()
            .expect("route failure did not consume the marker or lock");
        drop(guard);
        sqlx::query("ALTER TABLE events DROP CONSTRAINT reject_route")
            .execute(&db.pool)
            .await
            .unwrap();
        // Discord and Postgres are not one transaction: the unrecorded send is
        // eligible for retry, but a committed retry must suppress redelivery.
        runtime.handle(welcome(44), NOW + 1).await.unwrap();
        runtime.handle(welcome(44), NOW + 2).await.unwrap();
        assert_eq!(posts(&mock).len(), 2);
        assert_eq!(db.count(EVENT_ONBOARDING_PROMPTED).await, 1);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_member_read_failures_finish_the_defer_without_success_rows() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        for (mode, custom_id, key, failed_read) in [
            ("session", SESSION_SELECT_ID, "find-players", 1),
            ("legacy", GAME_SELECT_ID, "shooters", 1),
            ("legacy", GAME_SELECT_ID, "shooters", 2),
        ] {
            let state = Arc::new(Mutex::new(DiscordState {
                reject_member_read: Some(failed_read),
                ..Default::default()
            }));
            let mock = discord(Arc::clone(&state)).await;
            runtime(&db.pool, &mock, &vars(mode, false))
                .handle(component(custom_id, &[key]), NOW)
                .await
                .unwrap();
            assert_picker_failure(&db, &mock, 1).await;
            assert_eq!(state.lock().unwrap().member_reads, failed_read);
            let role_calls = mock
                .requests()
                .iter()
                .filter(|request| matches!(request.method.as_str(), "PUT" | "DELETE"))
                .count();
            assert_eq!(role_calls, usize::from(failed_read == 2));
            if failed_read == 2 {
                let body: Value = serde_json::from_slice(&replies(&mock)[0].body).unwrap();
                assert!(body["content"]
                    .as_str()
                    .unwrap()
                    .contains("may already have changed"));
            }
        }
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_picker_insert_and_commit_failures_are_atomic_and_terminal() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        for deferred in [false, true] {
            if deferred {
                sqlx::raw_sql("CREATE FUNCTION reject_route() RETURNS trigger LANGUAGE plpgsql AS $$
                    BEGIN IF NEW.event_type = 'channel_routed' THEN RAISE EXCEPTION 'test route failure'; END IF;
                    RETURN NEW; END $$;
                    CREATE CONSTRAINT TRIGGER reject_route AFTER INSERT ON events DEFERRABLE INITIALLY DEFERRED
                    FOR EACH ROW EXECUTE FUNCTION reject_route();")
                    .execute(&db.pool).await.unwrap();
            } else {
                sqlx::query("ALTER TABLE events ADD CONSTRAINT reject_route CHECK (event_type <> 'channel_routed')")
                    .execute(&db.pool).await.unwrap();
            }
            for (mode, custom_id, key) in [
                ("legacy", GAME_SELECT_ID, "shooters"),
                ("session", SESSION_SELECT_ID, "find-players"),
            ] {
                let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
                runtime(&db.pool, &mock, &vars(mode, false))
                    .handle(component(custom_id, &[key]), NOW).await.unwrap();
                assert_picker_failure(&db, &mock, if deferred { 2 } else { 1 }).await;
            }
            if deferred {
                sqlx::raw_sql("DROP TRIGGER reject_route ON events; DROP FUNCTION reject_route();")
                    .execute(&db.pool).await.unwrap();
            } else {
                sqlx::query("ALTER TABLE events DROP CONSTRAINT reject_route")
                    .execute(&db.pool).await.unwrap();
            }
        }
    }).catch_unwind().await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_picker_begin_and_lock_failures_finish_the_defer() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let runtime_pool = TestSchema::pool(&db.schema).await;
        let mock = discord(Arc::new(Mutex::new(DiscordState {
            close_pool_on_callback: Some(runtime_pool.clone()),
            ..Default::default()
        })))
        .await;
        runtime(&runtime_pool, &mock, &vars("legacy", false))
            .handle(component(GAME_SELECT_ID, &["shooters"]), NOW)
            .await
            .unwrap();
        assert!(
            runtime_pool.is_closed(),
            "DB begin fails only after the callback"
        );
        assert_picker_failure(&db, &mock, 1).await;

        let runtime_pool = TestSchema::pool_with_lock_timeout(&db.schema, "100ms").await;
        let mut held = db.pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind("22:44:game_picker")
            .execute(&mut *held)
            .await
            .unwrap();
        let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
        runtime(&runtime_pool, &mock, &vars("legacy", false))
            .handle(component(GAME_SELECT_ID, &["shooters"]), NOW)
            .await
            .unwrap();
        assert_picker_failure(&db, &mock, 1).await;
        assert!(
            !mock
                .requests()
                .iter()
                .any(|request| request.method == "GET"),
            "lock failure precedes all member/role REST reads"
        );
        held.rollback().await.unwrap();
        runtime_pool.close().await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_reply_failures_roll_back_success_and_attempt_one_error_edit() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        for (mode, custom_id, key) in [
            ("legacy", GAME_SELECT_ID, "shooters"),
            ("session", SESSION_SELECT_ID, "find-players"),
        ] {
            for reject_replies in [false, true] {
                let mock = discord(Arc::new(Mutex::new(DiscordState {
                    reject_next_reply: true,
                    reject_replies,
                    ..Default::default()
                })))
                .await;
                let result = runtime(&db.pool, &mock, &vars(mode, false))
                    .handle(component(custom_id, &[key]), NOW)
                    .await;
                assert_eq!(
                    result.is_err(),
                    reject_replies,
                    "only an undelivered error reply remains retryable"
                );
                assert_picker_failure(&db, &mock, 2).await;
            }
        }
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_uncertain_callback_edits_without_replaying_roles() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        for reject_replies in [false, true] {
            let mock = discord(Arc::new(Mutex::new(DiscordState {
                reject_callback: true,
                reject_replies,
                ..Default::default()
            })))
            .await;
            let result = runtime(&db.pool, &mock, &vars("legacy", false))
                .handle(component(GAME_SELECT_ID, &["shooters"]), NOW)
                .await;
            assert_eq!(result.is_err(), reject_replies);
            assert_picker_failure(&db, &mock, 1).await;
            assert_eq!(
                mock.requests().len(),
                2,
                "callback failure never replays role work"
            );
        }
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_runtime_deferred_error_reply_is_bounded() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = discord(Arc::new(Mutex::new(DiscordState {
            reject_member_read: Some(1),
            reply_delay: Duration::from_secs(8),
            ..Default::default()
        })))
        .await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        let result = tokio::time::timeout(
            Duration::from_secs(7),
            runtime.handle(component(SESSION_SELECT_ID, &["find-players"]), NOW),
        )
        .await
        .expect("error reply must not hold the worker indefinitely");
        assert!(result.is_err(), "undelivered failure reply is retryable");
        assert_picker_failure(&db, &mock, 1).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
async fn onboarding_capture_requires_real_pending_transition_and_preserves_leave_state() {
    ensure_crypto_provider();
    let pool = PgPoolOptions::new().connect_lazy(TEST_DATABASE).unwrap();
    let executor =
        ActionExecutor::with_proxy("mock-token".into(), Some("http://127.0.0.1:1".into())).unwrap();
    let runtime = OnboardingRuntime::new(pool, executor, &vars("session", false), 22, 999).unwrap();
    let pipeline = build_pipeline(vec![]);
    let add = || {
        Event::MemberAdd(Box::new(serde_json::from_value(json!({
        "guild_id":"22","user":{"id":"44","username":"member","discriminator":"0"},
        "roles":[],"deaf":false,"mute":false,"flags":0,"pending":true,"joined_at":"2026-09-28T00:00:00.000000+00:00"
    })).unwrap()))
    };
    let update = || {
        Event::MemberUpdate(Box::new(
            serde_json::from_value(json!({
                "guild_id":"22","user":{"id":"44","username":"member","discriminator":"0"},
                "roles":[],"pending":false,"joined_at":"2026-09-28T00:00:00.000000+00:00"
            }))
            .unwrap(),
        ))
    };
    assert!(
        runtime.capture(&update(), &pipeline).is_none(),
        "missing cache is not proof of a true->false transition"
    );
    pipeline.handle(&add());
    let captured = runtime.capture(&update(), &pipeline).unwrap();
    pipeline.handle(&update());
    assert!(matches!(
        captured,
        OnboardingJob::Welcome {
            trigger: MembershipTrigger::GateCleared,
            ..
        }
    ));
    assert!(
        runtime.capture(&update(), &pipeline).is_none(),
        "false->false is not another gate clear"
    );
    let remove = Event::MemberRemove(
        serde_json::from_value(
            json!({"guild_id":"22","user":{"id":"44","username":"member","discriminator":"0"}}),
        )
        .unwrap(),
    );
    let captured = runtime.capture(&remove, &pipeline).unwrap();
    pipeline.handle(&remove);
    assert!(matches!(
        captured,
        OnboardingJob::Goodbye {
            joined_at_ms: Some(_),
            ..
        }
    ));
    let batch = pipeline.handlers().store().take_batch();
    for kind in [
        two_bot_core::EventType::MemberJoin,
        two_bot_core::EventType::GateCleared,
        two_bot_core::EventType::MemberLeave,
    ] {
        assert_eq!(
            batch
                .events
                .iter()
                .filter(|event| event.event_type == kind)
                .count(),
            1,
            "S3 owns exactly one {kind:?} fact"
        );
    }
}
