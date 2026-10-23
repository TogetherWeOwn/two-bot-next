//! Actual shared router + REST executor + isolated test-container store.
#![cfg(feature = "db")]
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    commands::PERM_MANAGE_EVENTS, lfg, lfg_store as store, RouterGates,
    ANNOUNCEMENTS_DISABLED_REPLY, MANAGE_EVENTS_REQUIRED,
};
use two_bot_discord::{
    interactions::InteractionRuntime,
    lfg_interactions::{LfgInteractions, LfgRequest},
    ActionExecutor,
};

const GUILD: &str = "2222";
const CHANNEL: &str = "4444";
const BOT: u64 = 1111;

fn gates(enabled: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        announcements: enabled,
        scorecard: false,
        automations: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn invocation(id: u64, user: u64, permissions: u64, data: Value, kind: u8) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": BOT.to_string(), "type": kind,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"user": {"id": user.to_string(), "username": "member", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": null, "deaf": false, "mute": false, "flags": 0, "permissions": permissions.to_string()},
        "token": "lfg-mock-token", "version": 1, "entitlements": [],
        "authorizing_integration_owners": {}, "data": data
    })).expect("wire interaction")
}

fn create(id: u64, permissions: u64) -> Interaction {
    create_with_roles(id, permissions, "tank:Tank:1,dps:DPS:1")
}

fn create_with_roles(id: u64, permissions: u64, roles: &str) -> Interaction {
    invocation(
        id,
        3333,
        permissions,
        json!({"id": "1", "name": "lfg", "type": 1, "options": [
            {"type": 3, "name": "title", "value": "Friday raid @everyone"},
            {"type": 3, "name": "starts-at", "value": "2099-09-11T20:00:00Z"},
            {"type": 3, "name": "roles", "value": roles}
        ]}),
        2,
    )
}

fn select(id: u64, post: &str, user: u64, role: &str) -> Interaction {
    invocation(
        id,
        user,
        0,
        json!({"custom_id": lfg::lfg_custom_id(post), "component_type": 3, "values": [role]}),
        3,
    )
}

fn close(id: u64, post: &str, permissions: u64) -> Interaction {
    invocation(
        id,
        3333,
        permissions,
        json!({"id": "1", "name": "lfg-close", "type": 1,
        "options": [{"type": 3, "name": "id", "value": post}]}),
        2,
    )
}

fn runtime(pool: sqlx::PgPool, mock: &MockRest, enabled: bool) -> InteractionRuntime {
    InteractionRuntime::new(
        gates(enabled),
        pool,
        ActionExecutor::with_proxy("lfg-test-token".into(), Some(mock.origin())).expect("executor"),
        BOT,
        two_bot_core::ClassifierConfig::default(),
    )
}

#[tokio::test]
async fn unknown_bot_identity_never_proves_acceptance() {
    // A resumed boot without tickets may not know the bot user yet: recovery
    // must stay uncertain rather than read history against a guessed author.
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!([]))).await;
    let executor =
        ActionExecutor::with_proxy("lfg-test-token".into(), Some(mock.origin())).expect("executor");
    assert!(executor
        .recover_message_by_nonce(CHANNEL, &lfg::lfg_nonce("lfg-1"), 0)
        .await
        .is_err());
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn foreign_application_is_ignored_before_callback_or_store() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    let runtime = runtime(pool, &mock, true);
    runtime.set_application_id(BOT + 1);
    assert!(!runtime
        .handle(&create(901, PERM_MANAGE_EVENTS))
        .await
        .unwrap());
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

async fn connect_schema_pool(url: &str, schema: &str) -> sqlx::PgPool {
    let search = schema.to_owned();
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .after_connect(move |conn, _| {
            let search = search.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(search)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(url)
        .await
        .expect("schema pool")
}

struct TestDb {
    pool: sqlx::PgPool,
    admin: sqlx::PgPool,
    schema: String,
    url: String,
}
impl TestDb {
    async fn new() -> Self {
        // Fixed agent test endpoint or this job's disposable CI service only.
        // Never read application URLs, secret env vars, or inherited credentials.
        let url = if std::env::var("TWO_LFG_TESTDB_CI").as_deref() == Ok("1") {
            "postgres://agent_test@127.0.0.1:5432/agent_test"
        } else {
            "postgres://agent_test@agent-testdb:5432/two_bot_test_tog10084"
        };
        let admin = sqlx::PgPool::connect(url).await.expect("testdb reachable");
        let schema = format!(
            "lfg_runtime_{}",
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        );
        // Audited identifier: constant prefix + timestamp digits; no external input.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("test schema");
        let pool = connect_schema_pool(url, &schema).await;
        sqlx::migrate!("../cutover/migrations")
            .run(&pool)
            .await
            .expect("migrations");
        Self {
            pool,
            admin,
            schema,
            url: url.to_owned(),
        }
    }
    /// An independent pool on the same test schema: reads through it prove the
    /// state under test is durable, not held by the first pool or service.
    async fn fresh_pool(&self) -> sqlx::PgPool {
        connect_schema_pool(&self.url, &self.schema).await
    }
    async fn cleanup(self) {
        self.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .expect("cleanup test schema");
        self.admin.close().await;
    }
}

async fn expire_actor_window() {
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    tokio::time::resume();
}

fn body(request: &common::RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("JSON body")
}
fn last_reply(mock: &MockRest) -> String {
    let requests = mock.requests();
    body(requests.last().expect("reply"))["content"]
        .as_str()
        .expect("content")
        .into()
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn router_runs_create_signup_full_switch_leave_close_with_audit() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "5000"})),
        ],
        ScriptedResponse::json(200, json!({"id": "5900"})),
    )
    .await;
    let rt = runtime(db.pool.clone(), &mock, true);
    let post = "lfg-7001";
    assert!(rt
        .handle(&create(7001, PERM_MANAGE_EVENTS))
        .await
        .expect("create"));
    assert_eq!(last_reply(&mock), "LFG posted: `lfg-7001`.");
    let stored = store::get_lfg(&db.pool, GUILD, post)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.message_id.as_deref(), Some("5000"));
    assert_eq!(
        store::list_lfg_roles(&db.pool, post).await.unwrap().len(),
        2
    );

    rt.handle(&select(7002, post, 3333, "tank")).await.unwrap();
    assert_eq!(last_reply(&mock), "LFG joined.");
    let joined_at = store::list_lfg_signups(&db.pool, post).await.unwrap()[0]
        .joined_at
        .clone();
    expire_actor_window().await;
    rt.handle(&select(7003, post, 3333, "tank")).await.unwrap();
    assert_eq!(
        store::list_lfg_signups(&db.pool, post).await.unwrap()[0].joined_at,
        joined_at
    );
    rt.handle(&select(7004, post, 3334, "tank")).await.unwrap();
    assert_eq!(last_reply(&mock), "LFG full.");
    expire_actor_window().await;
    rt.handle(&select(7005, post, 3333, "dps")).await.unwrap();
    assert_eq!(last_reply(&mock), "LFG moved.");
    rt.handle(&select(7006, post, 3334, "tank")).await.unwrap();
    expire_actor_window().await;
    rt.handle(&select(7007, post, 3334, "dps")).await.unwrap();
    assert_eq!(last_reply(&mock), "LFG full.");
    assert_eq!(
        store::list_lfg_signups(&db.pool, post).await.unwrap()[1].role_key,
        "tank"
    );
    rt.handle(&select(7008, post, 3333, "__leave__"))
        .await
        .unwrap();
    assert_eq!(last_reply(&mock), "LFG left.");
    rt.handle(&close(7009, post, PERM_MANAGE_EVENTS))
        .await
        .unwrap();
    assert_eq!(last_reply(&mock), "LFG closed.");
    rt.handle(&select(7010, post, 3335, "tank")).await.unwrap();
    assert_eq!(last_reply(&mock), "LFG closed.");
    rt.handle(&select(7011, post, 3334, "__leave__"))
        .await
        .unwrap();
    assert_eq!(last_reply(&mock), "LFG left.");
    rt.handle(&close(7012, post, PERM_MANAGE_EVENTS))
        .await
        .unwrap();
    assert_eq!(last_reply(&mock), "LFG was already closed or missing.");
    assert!(store::list_lfg_signups(&db.pool, post)
        .await
        .unwrap()
        .is_empty());

    let requests = mock.requests();
    let message_requests: Vec<_> = requests
        .iter()
        .filter(|r| r.path.starts_with("/api/v10/channels/"))
        .collect();
    assert_eq!(
        message_requests
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        1
    );
    assert_eq!(body(message_requests[0])["nonce"], lfg::lfg_nonce(post));
    assert_eq!(body(message_requests[0])["enforce_nonce"], true);
    assert_eq!(
        body(message_requests[0])["components"][0]["components"][0]["type"],
        3
    );
    for request in &message_requests {
        assert_eq!(body(request)["allowed_mentions"]["parse"], json!([]));
    }
    assert_eq!(
        body(message_requests.last().unwrap())["components"],
        json!([])
    );
    for request in requests.iter().filter(|r| r.path.ends_with("/callback")) {
        assert_eq!(body(request)["type"], 5);
        assert_eq!(body(request)["data"]["flags"], 64);
    }
    let audits: Vec<(String, String)> = sqlx::query_as(
        "SELECT action, outcome FROM announcements_audit_log ORDER BY created_at, id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(audits.len(), 12);
    assert!(audits.contains(&("lfg.signup".into(), "full".into())));
    assert!(audits.contains(&("lfg.close".into(), "closed".into())));

    let bad_roles = format!("{}:Tank:1", "@everyone".repeat(200));
    assert!(rt
        .handle(&create_with_roles(7013, PERM_MANAGE_EVENTS, &bad_roles))
        .await
        .unwrap());
    let requests = mock.requests();
    let reply = body(requests.last().expect("deferred error reply"));
    assert_eq!(requests.last().unwrap().method, "PATCH");
    let content = reply["content"].as_str().expect("reply content");
    assert!(content.encode_utf16().count() <= lfg::MAX_MESSAGE_CHARS);
    assert!(!content.contains("@everyone"));
    assert_eq!(reply["allowed_mentions"]["parse"], json!([]));

    mock.shutdown().await;
    drop(rt);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn router_refuses_lfg_capacity_before_posting_and_names_the_close_command() {
    let db = TestDb::new().await;
    for n in 0..20 {
        let post = lfg::LfgPost {
            id: format!("quota-{n}"),
            guild_id: GUILD.into(),
            channel_id: CHANNEL.into(),
            message_id: None,
            title: "Stored title".into(),
            starts_at: "2099-09-11T20:00:00Z".into(),
            status: lfg::LfgStatus::Open,
            created_by: "3333".into(),
            created_at: "2026-09-30T00:00:00.000Z".into(),
            closed_at: None,
        };
        store::put_lfg(&db.pool, &post, &[], true).await.unwrap();
    }
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::json(200, json!({"id": "5900"})),
    )
    .await;
    let rt = runtime(db.pool.clone(), &mock, true);
    rt.handle(&create(7199, PERM_MANAGE_EVENTS)).await.unwrap();
    assert_eq!(
        last_reply(&mock),
        two_bot_core::automation_quota::AutomationQuota::OpenLfgPosts.to_string()
    );
    assert!(store::get_lfg(&db.pool, GUILD, "lfg-7199")
        .await
        .unwrap()
        .is_none());
    assert!(store::list_lfg_roles(&db.pool, "lfg-7199")
        .await
        .unwrap()
        .is_empty());
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM announcements_audit_log")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(audits, 0);
    assert!(!mock
        .requests()
        .iter()
        .any(|request| request.path.starts_with("/api/v10/channels/")));
    mock.shutdown().await;
    drop(rt);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn router_persists_post_and_roles_before_discord_accepts() {
    let db = TestDb::new().await;
    let (mock, acceptance) = MockRest::start_gated(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "5001"})),
        ],
        ScriptedResponse::json(200, json!({"id": "5900"})),
    )
    .await;
    let rt = std::sync::Arc::new(runtime(db.pool.clone(), &mock, true));
    let worker = {
        let rt = rt.clone();
        tokio::spawn(async move { rt.handle(&create(7201, PERM_MANAGE_EVENTS)).await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        acceptance.wait_for_request(),
    )
    .await
    .expect("POST reached mock");
    assert!(mock
        .requests()
        .iter()
        .any(|r| r.method == "POST" && r.path.starts_with("/api/v10/channels/")));
    let post = store::get_lfg(&db.pool, GUILD, "lfg-7201")
        .await
        .unwrap()
        .expect("durable before acceptance");
    assert!(post.message_id.is_none());
    assert_eq!(
        store::list_lfg_roles(&db.pool, &post.id)
            .await
            .unwrap()
            .len(),
        2
    );
    acceptance.release();
    assert!(worker.await.unwrap().unwrap());
    assert_eq!(
        store::get_lfg(&db.pool, GUILD, &post.id)
            .await
            .unwrap()
            .unwrap()
            .message_id
            .as_deref(),
        Some("5001")
    );
    mock.shutdown().await;
    drop(rt);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn router_recovers_ambiguous_post_by_own_nonce_without_duplicate_send() {
    let db = TestDb::new().await;
    let post = "lfg-7301";
    let nonce = lfg::lfg_nonce(post);
    let mock = MockRest::start(vec![ScriptedResponse::status(204), ScriptedResponse::status(500),
        ScriptedResponse::json(200, json!([
            {"id": "5999", "nonce": nonce, "channel_id": CHANNEL, "author": {"id": "9999"}},
            {"id": "5002", "nonce": nonce, "channel_id": CHANNEL, "author": {"id": BOT.to_string()}}
        ]))], ScriptedResponse::json(200, json!({"id": "5900"}))).await;
    let rt = runtime(db.pool.clone(), &mock, true);
    rt.handle(&create(7301, PERM_MANAGE_EVENTS)).await.unwrap();
    assert_eq!(last_reply(&mock), "LFG posted: `lfg-7301`.");
    assert_eq!(
        store::get_lfg(&db.pool, GUILD, post)
            .await
            .unwrap()
            .unwrap()
            .message_id
            .as_deref(),
        Some("5002")
    );
    assert_eq!(
        store::list_lfg_roles(&db.pool, post).await.unwrap().len(),
        2
    );
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.method == "POST" && r.path.starts_with("/api/v10/channels/"))
            .count(),
        1
    );
    assert!(mock
        .requests()
        .iter()
        .any(|r| r.method == "GET" && r.path.ends_with("/messages?limit=100")));
    mock.shutdown().await;
    drop(rt);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn router_cleans_failed_posts_but_preserves_unreadable_acceptance() {
    let db = TestDb::new().await;
    // A definite rejection and an ambiguous failure with readable empty history
    // clean up. A denied or malformed history response is never proof of absence.
    let cases = [
        (7401, ScriptedResponse::status(400), None, false),
        (
            7402,
            ScriptedResponse::status(500),
            Some(ScriptedResponse::json(200, json!([]))),
            false,
        ),
        (
            7403,
            ScriptedResponse::status(500),
            Some(ScriptedResponse::status(403)),
            true,
        ),
        (
            7404,
            ScriptedResponse::json(200, json!({})),
            Some(ScriptedResponse::json(200, json!({"not": "history"}))),
            true,
        ),
    ];
    for (id, sent, history, retained) in cases {
        let mut script = vec![ScriptedResponse::status(204), sent];
        if let Some(history) = history {
            script.push(history);
        }
        let mock =
            MockRest::start(script, ScriptedResponse::json(200, json!({"id": "5900"}))).await;
        let rt = runtime(db.pool.clone(), &mock, true);
        rt.handle(&create(id, PERM_MANAGE_EVENTS)).await.unwrap();
        let post = format!("lfg-{id}");
        let stored = store::get_lfg(&db.pool, GUILD, &post).await.unwrap();
        assert_eq!(stored.is_some(), retained);
        assert_eq!(
            store::list_lfg_roles(&db.pool, &post).await.unwrap().len(),
            if retained { 2 } else { 0 }
        );
        if retained {
            assert!(stored.unwrap().message_id.is_none());
            assert_eq!(
                last_reply(&mock),
                "LFG post acceptance is uncertain; saved state retained for nonce recovery."
            );
        } else {
            assert!(last_reply(&mock).contains("operation failed"));
            let audit: (String,) =
                sqlx::query_as("SELECT outcome FROM announcements_audit_log WHERE target_key = $1")
                    .bind(&post)
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            assert_eq!(audit.0, "failed");
        }
        assert_eq!(
            mock.requests()
                .iter()
                .filter(|r| r.method == "POST" && r.path.starts_with("/api/v10/channels/"))
                .count(),
            1
        );
        mock.shutdown().await;
        drop(rt);
    }
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn router_recovers_persisted_post_without_replacing_closed_roles_or_signups() {
    let db = TestDb::new().await;
    let post = lfg::LfgPost {
        id: "lfg-7501".into(),
        guild_id: GUILD.into(),
        channel_id: CHANNEL.into(),
        message_id: None,
        title: "Stored title".into(),
        starts_at: "2099-09-11T20:00:00Z".into(),
        status: lfg::LfgStatus::Open,
        created_by: "3333".into(),
        created_at: "2026-09-30T00:00:00.000Z".into(),
        closed_at: None,
    };
    let roles = lfg::spec_roles(&post.id, &lfg::parse_role_spec("healer:Healer:2").unwrap());
    store::put_lfg(&db.pool, &post, &roles, true).await.unwrap();
    store::signup_lfg(
        &db.pool,
        GUILD,
        &post.id,
        "healer",
        "3334",
        "2026-09-30T00:01:00.000Z",
    )
    .await
    .unwrap();
    store::close_lfg(&db.pool, GUILD, &post.id, "2026-09-30T00:02:00.000Z")
        .await
        .unwrap();
    let mock = MockRest::start(vec![ScriptedResponse::status(204),
        ScriptedResponse::json(200, json!([{"id": "5003", "nonce": lfg::lfg_nonce(&post.id), "channel_id": CHANNEL, "author": {"id": BOT.to_string()}}]))],
        ScriptedResponse::json(200, json!({"id": "5900"}))).await;
    let rt = runtime(db.pool.clone(), &mock, true);
    rt.handle(&create(7501, PERM_MANAGE_EVENTS)).await.unwrap();
    let stored = store::get_lfg(&db.pool, GUILD, &post.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.message_id.as_deref(), Some("5003"));
    assert_eq!(stored.status, lfg::LfgStatus::Closed);
    assert_eq!(
        stored.closed_at.as_deref(),
        Some("2026-09-30T00:02:00.000Z")
    );
    assert_eq!(stored.title, "Stored title");
    assert_eq!(
        store::list_lfg_roles(&db.pool, &post.id).await.unwrap(),
        roles
    );
    assert_eq!(
        store::list_lfg_signups(&db.pool, &post.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(!mock
        .requests()
        .iter()
        .any(|r| r.method == "POST" && r.path.starts_with("/api/v10/channels/")));
    let requests = mock.requests();
    let refresh = requests
        .iter()
        .find(|r| r.method == "PATCH" && r.path.starts_with("/api/v10/channels/"))
        .unwrap();
    assert_eq!(body(refresh)["components"], json!([]));
    assert_eq!(body(refresh)["allowed_mentions"]["parse"], json!([]));
    mock.shutdown().await;
    drop(rt);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn router_refuses_cross_guild_post_targeting_including_id_only_leave() {
    let db = TestDb::new().await;
    let post = lfg::LfgPost {
        id: "foreign-post".into(),
        guild_id: "9999".into(),
        channel_id: CHANNEL.into(),
        message_id: Some("5004".into()),
        title: "Foreign".into(),
        starts_at: "2099-09-11T20:00:00.000Z".into(),
        status: lfg::LfgStatus::Open,
        created_by: "3333".into(),
        created_at: "2026-09-30T00:00:00.000Z".into(),
        closed_at: None,
    };
    let roles = lfg::spec_roles(&post.id, &lfg::parse_role_spec("tank:Tank:1").unwrap());
    store::put_lfg(&db.pool, &post, &roles, true).await.unwrap();
    store::signup_lfg(
        &db.pool,
        "9999",
        &post.id,
        "tank",
        "3334",
        "2026-09-30T00:01:00.000Z",
    )
    .await
    .unwrap();
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5900"}))).await;
    let rt = runtime(db.pool.clone(), &mock, true);
    rt.handle(&select(7601, &post.id, 3334, "__leave__"))
        .await
        .unwrap();
    assert_eq!(last_reply(&mock), "LFG were not signed up.");
    rt.handle(&select(7602, &post.id, 3335, "tank"))
        .await
        .unwrap();
    assert_eq!(last_reply(&mock), "LFG missing.");
    rt.handle(&close(7603, &post.id, PERM_MANAGE_EVENTS))
        .await
        .unwrap();
    assert_eq!(last_reply(&mock), "LFG was already closed or missing.");
    assert_eq!(
        store::get_lfg(&db.pool, "9999", &post.id)
            .await
            .unwrap()
            .unwrap(),
        post
    );
    assert_eq!(
        store::list_lfg_signups(&db.pool, &post.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(!mock
        .requests()
        .iter()
        .any(|r| r.path.starts_with("/api/v10/channels/")));
    mock.shutdown().await;
    drop(rt);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn router_fences_guild_permissions_and_disabled_announcements() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let rt = runtime(db.pool.clone(), &mock, true);
    let mut foreign = create(7101, PERM_MANAGE_EVENTS);
    foreign.guild_id = Some(twilight_model::id::Id::new(9999));
    assert!(!rt.handle(&foreign).await.unwrap());
    assert!(mock.requests().is_empty());
    rt.handle(&create(7102, 0)).await.unwrap();
    assert_eq!(
        body(mock.requests().last().unwrap())["data"]["content"],
        MANAGE_EVENTS_REQUIRED
    );
    rt.handle(&close(7103, "foreign", 0)).await.unwrap();
    assert_eq!(mock.requests().len(), 2);
    let off = runtime(db.pool.clone(), &mock, false);
    off.handle(&create(7104, PERM_MANAGE_EVENTS)).await.unwrap();
    assert_eq!(
        body(mock.requests().last().unwrap())["data"]["content"],
        ANNOUNCEMENTS_DISABLED_REPLY
    );
    assert!(!off
        .handle(&select(7105, "foreign", 3333, "tank"))
        .await
        .unwrap());
    let count: (i64,) = sqlx::query_as("SELECT count(*) FROM lfg_posts")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count.0, 0);
    assert!(mock
        .requests()
        .iter()
        .all(|r| r.path.ends_with("/callback")));
    mock.shutdown().await;
    drop(rt);
    drop(off);
    db.cleanup().await;
}

fn close_without_id(id: u64, permissions: u64) -> Interaction {
    invocation(
        id,
        3333,
        permissions,
        json!({"id": "1", "name": "lfg-close", "type": 1, "options": []}),
        2,
    )
}

async fn store_counts(pool: &sqlx::PgPool) -> (i64, i64, i64, i64) {
    let posts: i64 = sqlx::query_scalar("SELECT count(*) FROM lfg_posts")
        .fetch_one(pool)
        .await
        .unwrap();
    let roles: i64 = sqlx::query_scalar("SELECT count(*) FROM lfg_roles")
        .fetch_one(pool)
        .await
        .unwrap();
    let signups: i64 = sqlx::query_scalar("SELECT count(*) FROM lfg_signups")
        .fetch_one(pool)
        .await
        .unwrap();
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM announcements_audit_log")
        .fetch_one(pool)
        .await
        .unwrap();
    (posts, roles, signups, audits)
}

fn channel_requests_any(mock: &MockRest) -> usize {
    mock.requests()
        .iter()
        .filter(|r| r.path.starts_with("/api/v10/channels/"))
        .count()
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn router_refuses_invalid_close_ids_without_store_or_channel_mutation() {
    // S3: the /lfg-close interaction-edge guard (lfg_interactions.rs:153-164)
    // refuses empty and over-128-UTF-16-unit ids before admission, SQL, audit
    // or channel work. This sends the matrix through the actual router
    // (InteractionRuntime::handle), not the pure resolver, so deleting the
    // handler bound lets the stored 129-char post close and the test fails.
    const REFUSAL: &str = "LFG id must be between 1 and 128 characters.";
    const MISSING_ROW: &str = "LFG was already closed or missing.";
    const MISSING_OPTION: &str = "Missing id option.";
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5900"}))).await;
    let rt = runtime(db.pool.clone(), &mock, true);

    // One ordinary post plus exact-boundary posts that must stay closable, plus
    // a stored 129-char post that the guard must leave open. Seeded directly
    // so the refusal matrix has durable rows to preserve.
    let main_post = lfg::LfgPost {
        id: "lfg-7801".into(),
        guild_id: GUILD.into(),
        channel_id: CHANNEL.into(),
        message_id: None,
        title: "Friday raid".into(),
        starts_at: "2099-09-11T20:00:00Z".into(),
        status: lfg::LfgStatus::Open,
        created_by: "3333".into(),
        created_at: "2026-09-30T00:00:00.000Z".into(),
        closed_at: None,
    };
    let ascii128 = "a".repeat(128);
    let emoji64 = "😀".repeat(64); // 64 chars, 128 UTF-16 units (surrogate pairs count 2).
    let stored129 = "z".repeat(129);
    for id in [&main_post.id, &ascii128, &emoji64, &stored129] {
        let post = lfg::LfgPost {
            id: id.clone(),
            guild_id: GUILD.into(),
            channel_id: CHANNEL.into(),
            message_id: None,
            title: "Stored title".into(),
            starts_at: "2099-09-11T20:00:00Z".into(),
            status: lfg::LfgStatus::Open,
            created_by: "3333".into(),
            created_at: "2026-09-30T00:00:00.000Z".into(),
            closed_at: None,
        };
        let roles = lfg::spec_roles(&post.id, &lfg::parse_role_spec("tank:Tank:1").unwrap());
        store::put_lfg(&db.pool, &post, &roles, true).await.unwrap();
        store::signup_lfg(
            &db.pool,
            GUILD,
            &post.id,
            "tank",
            "3334",
            "2026-09-30T00:01:00.000Z",
        )
        .await
        .unwrap();
    }
    assert_eq!(ascii128.encode_utf16().count(), 128);
    assert_eq!(emoji64.encode_utf16().count(), 128);
    assert_eq!(stored129.encode_utf16().count(), 129);
    let baseline = store_counts(&db.pool).await;
    assert_eq!(baseline, (4, 4, 4, 0));
    let main_signups = store::list_lfg_signups(&db.pool, &main_post.id)
        .await
        .unwrap();
    assert_eq!(main_signups.len(), 1);

    // Refusal matrix: empty, ASCII overlong, multibyte overlong (UTF-16, not
    // chars or bytes), hostile mention tail, and the stored 129-char row.
    // The 65-emoji case is 65 chars but 130 UTF-16 units, so a char-counting
    // bound would wrongly admit it.
    let emoji65 = "😀".repeat(65);
    assert_eq!(emoji65.encode_utf16().count(), 130);
    let long_129 = "é".repeat(129);
    assert_eq!(long_129.encode_utf16().count(), 129);
    let hostile = "@everyone".repeat(20);
    assert!(hostile.encode_utf16().count() > 128);
    let refusals = [
        String::new(),
        "x".repeat(129),
        "y".repeat(300),
        emoji65,
        long_129,
        hostile,
        stored129.clone(),
    ];
    let mut next_id: u64 = 7801;
    for post_id in &refusals {
        let requests_before = mock.requests().len();
        let channels_before = channel_requests_any(&mock);
        assert!(rt
            .handle(&close(next_id, post_id, PERM_MANAGE_EVENTS))
            .await
            .unwrap());
        next_id += 1;
        assert_eq!(last_reply(&mock), REFUSAL);
        // Fixed literal: no echo of the hostile tail, mention-safe, bounded.
        let requests = mock.requests();
        let reply = body(requests.last().expect("refusal edit"));
        assert_eq!(reply["content"].as_str().unwrap(), REFUSAL);
        assert!(!reply["content"].as_str().unwrap().contains("@everyone"));
        assert_eq!(reply["allowed_mentions"]["parse"], json!([]));
        assert!(REFUSAL.encode_utf16().count() <= lfg::MAX_MESSAGE_CHARS);
        // Exactly the deferred ack plus the ephemeral edit; no channel work.
        assert_eq!(requests.len() - requests_before, 2);
        assert_eq!(channel_requests_any(&mock) - channels_before, 0);
        // Sessions, signups, roles and audit rows all unchanged.
        assert_eq!(store_counts(&db.pool).await, baseline);
        assert_eq!(
            store::list_lfg_signups(&db.pool, &main_post.id)
                .await
                .unwrap()
                .len(),
            1
        );
    }
    // The stored overlong row proves the handler bound: without it the router
    // would have closed this post instead of refusing.
    assert_eq!(
        store::get_lfg(&db.pool, GUILD, &stored129)
            .await
            .unwrap()
            .unwrap()
            .status,
        lfg::LfgStatus::Open
    );

    // Missing-option control: no id option at all. Same no-mutation shape with
    // its own missing-option text.
    let requests_before = mock.requests().len();
    assert!(rt
        .handle(&close_without_id(next_id, PERM_MANAGE_EVENTS))
        .await
        .unwrap());
    next_id += 1;
    assert_eq!(last_reply(&mock), MISSING_OPTION);
    assert_eq!(mock.requests().len() - requests_before, 2);
    assert_eq!(store_counts(&db.pool).await, baseline);

    // Guard-pass controls: 100 e-acutes are 100 UTF-16 units but 200 bytes, so
    // a byte-counting bound would wrongly refuse them. They reach the store and
    // report the existing missing-row outcome with one audit row, like any
    // unknown id. Whitespace-only keeps that same missing-row behavior.
    let byte_control = "é".repeat(100);
    assert_eq!(byte_control.encode_utf16().count(), 100);
    assert_eq!(byte_control.len(), 200);
    for (post_id, expected) in [
        (byte_control, MISSING_ROW),
        ("no-such-lfg-7800".to_owned(), MISSING_ROW),
        ("   ".to_owned(), MISSING_ROW),
    ] {
        let (posts, roles, signups, audits) = store_counts(&db.pool).await;
        let requests_before = mock.requests().len();
        let channels_before = channel_requests_any(&mock);
        assert!(rt
            .handle(&close(next_id, &post_id, PERM_MANAGE_EVENTS))
            .await
            .unwrap());
        next_id += 1;
        assert_eq!(last_reply(&mock), expected);
        assert_eq!(mock.requests().len() - requests_before, 2);
        assert_eq!(channel_requests_any(&mock) - channels_before, 0);
        // No session/signup/role change; exactly one missing-row audit row.
        let after = store_counts(&db.pool).await;
        assert_eq!((after.0, after.1, after.2), (posts, roles, signups));
        assert_eq!(after.3, audits + 1);
    }

    // Exact-boundary valid controls: 128 ASCII units and 64 emoji (128 units)
    // close successfully, proving the bound is > 128, not >= 128, in UTF-16.
    for post_id in [&ascii128, &emoji64] {
        let (_, _, _, audits) = store_counts(&db.pool).await;
        assert!(rt
            .handle(&close(next_id, post_id, PERM_MANAGE_EVENTS))
            .await
            .unwrap());
        next_id += 1;
        assert_eq!(last_reply(&mock), "LFG closed.");
        assert_eq!(
            store::get_lfg(&db.pool, GUILD, post_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            lfg::LfgStatus::Closed
        );
        // Closing flips status without dropping signups; one close audit row.
        assert_eq!(
            store::list_lfg_signups(&db.pool, post_id)
                .await
                .unwrap()
                .len(),
            1
        );
        let (_, _, _, after_audits) = store_counts(&db.pool).await;
        assert_eq!(after_audits, audits + 1);
    }

    // Valid close lifecycle stays green: the ordinary post closes, then a
    // second close reports the missing-row outcome.
    assert!(rt
        .handle(&close(next_id, &main_post.id, PERM_MANAGE_EVENTS))
        .await
        .unwrap());
    next_id += 1;
    assert_eq!(last_reply(&mock), "LFG closed.");
    assert_eq!(
        store::get_lfg(&db.pool, GUILD, &main_post.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        lfg::LfgStatus::Closed
    );
    assert!(rt
        .handle(&close(next_id, &main_post.id, PERM_MANAGE_EVENTS))
        .await
        .unwrap());
    assert_eq!(last_reply(&mock), MISSING_ROW);
    assert!(
        channel_requests_any(&mock) == 0,
        "close with no message id edits nothing"
    );

    mock.shutdown().await;
    drop(rt);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn queued_lfg_work_leaves_pool_connections_for_the_gateway() {
    // TOG-12174: every lock waiter used to hold a pool connection, starving the
    // lock holder and the gateway checkpoint writer of the 5-connection pool.
    let db = TestDb::new().await;
    let post = lfg::LfgPost {
        id: "lfg-7601".into(),
        guild_id: GUILD.into(),
        channel_id: CHANNEL.into(),
        message_id: Some("5006".into()),
        title: "Crowded raid".into(),
        starts_at: "2099-09-11T20:00:00.000Z".into(),
        status: lfg::LfgStatus::Open,
        created_by: "3333".into(),
        created_at: "2026-10-02T00:00:00.000Z".into(),
        closed_at: None,
    };
    let roles = lfg::spec_roles(&post.id, &lfg::parse_role_spec("dps:DPS:10").unwrap());
    store::put_lfg(&db.pool, &post, &roles, true).await.unwrap();
    // The first message edit stays unanswered: its execution holds the lock.
    let (mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::json(200, json!({"id": "5900"}))],
        ScriptedResponse::json(200, json!({"id": "5900"})),
    )
    .await;
    let executor = std::sync::Arc::new(
        ActionExecutor::with_proxy("lfg-test-token".into(), Some(mock.origin())).expect("executor"),
    );
    let service = std::sync::Arc::new(LfgInteractions::new(db.pool.clone()));
    let tasks: Vec<_> = (0..6u64)
        .map(|n| {
            let (executor, service, post_id) = (executor.clone(), service.clone(), post.id.clone());
            tokio::spawn(async move {
                let request = LfgRequest::Select(lfg::LfgSelectAction::Signup {
                    post_id,
                    role_key: "dps".into(),
                });
                let actor = (4000 + n).to_string();
                service
                    .execute(&executor, request, GUILD, &actor, 7601 + n, BOT)
                    .await
            })
        })
        .collect();
    tokio::time::timeout(std::time::Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("the lock holder reaches Discord");
    // Let the other five queue; uncapped, each would now hold a connection.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // Stand-in for the checkpoint writer, which panics after a 5 s deadline.
    let spare = tokio::time::timeout(std::time::Duration::from_secs(2), db.pool.acquire())
        .await
        .expect("a pool connection stays free while LFG work queues")
        .expect("acquire");
    drop(spare);
    gate.release();
    for task in tasks {
        task.await.expect("join").expect("signup succeeds");
    }
    assert_eq!(
        store::list_lfg_signups(&db.pool, &post.id)
            .await
            .unwrap()
            .len(),
        6
    );
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.method == "PATCH")
            .count(),
        6
    );
    mock.shutdown().await;
    db.cleanup().await;
}

// ── Saved-state outcomes when audit or message refresh fails ────────────────
// Standalone acceptance for the documented non-atomic boundary: store
// mutations commit through the pool before the separate announcement audit,
// and the audit is not atomic with the mutation. A rejected audit reports
// the applied outcome and audit loss with no refresh; a failed refresh keeps
// the saved mutation and says so. Direct-service replay of an audited signup
// reuses the saved outcome without duplicating signup/audit rows or
// resurrecting a member who left. (Routed gateway redelivery is not claimed
// here: it still requires a valid initial acknowledgement, which Discord
// rejects with 40060 once answered.) Tests-only: no production behavior,
// copy, or recovery policy changes.

/// One open post with a posted message id, so executions take the refresh path.
async fn open_post_with_message(pool: &sqlx::PgPool, post_id: &str, message_id: &str) {
    let post = lfg::LfgPost {
        id: post_id.into(),
        guild_id: GUILD.into(),
        channel_id: CHANNEL.into(),
        message_id: Some(message_id.into()),
        title: "Saved-state raid".into(),
        starts_at: "2099-09-11T20:00:00Z".into(),
        status: lfg::LfgStatus::Open,
        created_by: "3333".into(),
        created_at: "2026-09-30T00:00:00.000Z".into(),
        closed_at: None,
    };
    let roles = lfg::spec_roles(&post.id, &lfg::parse_role_spec("tank:Tank:1").unwrap());
    store::put_lfg(pool, &post, &roles, true).await.unwrap();
}

/// Fixture fault scoped to the required audit INSERT after mutation: the
/// domain rows stay writable while `lfg.signup`/`lfg.close` audit rows are
/// rejected. Dropped explicitly at the end of each fault test.
async fn reject_lfg_audits(pool: &sqlx::PgPool) {
    sqlx::query(
        "ALTER TABLE announcements_audit_log
         ADD CONSTRAINT lfg_saved_state_no_lfg_audit
         CHECK (action NOT IN ('lfg.signup', 'lfg.close'))",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn allow_lfg_audits(pool: &sqlx::PgPool) {
    sqlx::query(
        "ALTER TABLE announcements_audit_log
         DROP CONSTRAINT lfg_saved_state_no_lfg_audit",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn channel_requests(mock: &MockRest, method: &str) -> usize {
    mock.requests()
        .iter()
        .filter(|r| r.method == method && r.path.starts_with("/api/v10/channels/"))
        .count()
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn rejected_signup_audit_reports_saved_join_and_audit_loss_without_refresh() {
    let db = TestDb::new().await;
    let post = "lfg-8101";
    open_post_with_message(&db.pool, post, "5000").await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::json(200, json!({"id": "5900"})),
    )
    .await;
    let rt = runtime(db.pool.clone(), &mock, true);
    reject_lfg_audits(&db.pool).await;
    assert!(rt.handle(&select(8101, post, 3333, "tank")).await.unwrap());
    allow_lfg_audits(&db.pool).await;
    // Report the applied join without pretending the separate audit succeeded.
    assert_eq!(
        last_reply(&mock),
        "LFG joined. Audit could not be recorded; check the saved state before retrying."
    );
    assert_eq!(
        body(mock.requests().last().unwrap())["allowed_mentions"]["parse"],
        json!([])
    );
    // An independent pool sees the saved signup and the missing audit.
    let fresh = db.fresh_pool().await;
    let signups = store::list_lfg_signups(&fresh, post).await.unwrap();
    assert_eq!(signups.len(), 1);
    assert_eq!(signups[0].user_id, "3333");
    assert_eq!(signups[0].role_key, "tank");
    let audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM announcements_audit_log WHERE target_key = $1")
            .bind(post)
            .fetch_one(&fresh)
            .await
            .unwrap();
    assert_eq!(audits, 0);
    fresh.close().await;
    // No refresh follows the failed audit: nothing touches the channel.
    assert_eq!(channel_requests(&mock, "PATCH"), 0);
    assert_eq!(channel_requests(&mock, "POST"), 0);
    mock.shutdown().await;
    drop(rt);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn rejected_close_audit_reports_saved_closure_and_audit_loss_without_refresh() {
    let db = TestDb::new().await;
    let post = "lfg-8201";
    open_post_with_message(&db.pool, post, "5000").await;
    store::signup_lfg(
        &db.pool,
        GUILD,
        post,
        "tank",
        "3334",
        "2026-09-30T00:01:00.000Z",
    )
    .await
    .unwrap();
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::json(200, json!({"id": "5900"})),
    )
    .await;
    let rt = runtime(db.pool.clone(), &mock, true);
    reject_lfg_audits(&db.pool).await;
    assert!(rt
        .handle(&close(8201, post, PERM_MANAGE_EVENTS))
        .await
        .unwrap());
    allow_lfg_audits(&db.pool).await;
    assert_eq!(
        last_reply(&mock),
        "LFG closed. Audit could not be recorded; check the saved state before retrying."
    );
    assert_eq!(
        body(mock.requests().last().unwrap())["allowed_mentions"]["parse"],
        json!([])
    );
    // The durable closure remains, verified through an independent pool.
    let fresh = db.fresh_pool().await;
    let stored = store::get_lfg(&fresh, GUILD, post).await.unwrap().unwrap();
    assert_eq!(stored.status, lfg::LfgStatus::Closed);
    assert!(stored.closed_at.is_some());
    let audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM announcements_audit_log WHERE target_key = $1")
            .bind(post)
            .fetch_one(&fresh)
            .await
            .unwrap();
    assert_eq!(audits, 0);
    fresh.close().await;
    assert_eq!(channel_requests(&mock, "PATCH"), 0);
    assert_eq!(channel_requests(&mock, "POST"), 0);
    mock.shutdown().await;
    drop(rt);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn failed_refresh_patch_keeps_saved_signup_and_audit_with_warning_and_no_post() {
    let db = TestDb::new().await;
    let post = "lfg-8301";
    open_post_with_message(&db.pool, post, "5000").await;
    // Deferred callback succeeds, the refresh PATCH fails, the final webhook
    // edit (default) succeeds.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ScriptedResponse::status(500)],
        ScriptedResponse::json(200, json!({"id": "5900"})),
    )
    .await;
    let rt = runtime(db.pool.clone(), &mock, true);
    assert!(rt.handle(&select(8301, post, 3333, "tank")).await.unwrap());
    // The existing saved-state warning is returned unchanged.
    assert_eq!(
        last_reply(&mock),
        "LFG joined. Message refresh failed; the saved result above still applies."
    );
    // Saved signup plus exactly one audit, visible from an independent pool.
    let fresh = db.fresh_pool().await;
    let signups = store::list_lfg_signups(&fresh, post).await.unwrap();
    assert_eq!(signups.len(), 1);
    assert_eq!(signups[0].user_id, "3333");
    let audits: Vec<(String, String)> =
        sqlx::query_as("SELECT action, outcome FROM announcements_audit_log WHERE target_key = $1")
            .bind(post)
            .fetch_all(&fresh)
            .await
            .unwrap();
    assert_eq!(audits, vec![("lfg.signup".to_owned(), "joined".to_owned())]);
    fresh.close().await;
    // Exactly the one failed refresh PATCH; no replacement message POST.
    assert_eq!(channel_requests(&mock, "PATCH"), 1);
    assert_eq!(channel_requests(&mock, "POST"), 0);
    mock.shutdown().await;
    drop(rt);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "needs agent-testdb or the CI service container"]
async fn direct_replay_retries_refresh_without_duplicating_signup_audit_or_resurrecting_leaver() {
    let db = TestDb::new().await;
    let post = "lfg-8401";
    open_post_with_message(&db.pool, post, "5000").await;
    // Callback defers accept 204 or a 200 JSON receipt; the refresh PATCH
    // needs only a 200 status; the follow-up webhook edit needs a 200 JSON
    // receipt with an id. One scripted 204 plus the JSON default covers all
    // six routed calls (callback, refresh, webhook edit per handle).
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::json(200, json!({"id": "5900"})),
    )
    .await;
    let rt = runtime(db.pool.clone(), &mock, true);
    assert!(rt.handle(&select(8401, post, 3333, "tank")).await.unwrap());
    assert_eq!(last_reply(&mock), "LFG joined.");
    assert!(rt
        .handle(&select(8402, post, 3333, "__leave__"))
        .await
        .unwrap());
    assert_eq!(last_reply(&mock), "LFG left.");
    assert!(store::list_lfg_signups(&db.pool, post)
        .await
        .unwrap()
        .is_empty());
    mock.shutdown().await;
    drop(rt);
    // Recreated pool, executor double and service: the replay is a
    // direct-service retry of the same audited interaction, not gateway
    // redelivery (which still needs a valid initial acknowledgement).
    let fresh = db.fresh_pool().await;
    let replay_mock = MockRest::start(
        vec![ScriptedResponse::status(200)],
        ScriptedResponse::status(200),
    )
    .await;
    let executor = ActionExecutor::with_proxy("lfg-test-token".into(), Some(replay_mock.origin()))
        .expect("executor");
    let service = LfgInteractions::new(fresh.clone());
    let replayed = service
        .execute(
            &executor,
            LfgRequest::Select(lfg::LfgSelectAction::Signup {
                post_id: post.into(),
                role_key: "tank".into(),
            }),
            GUILD,
            "3333",
            8401,
            BOT,
        )
        .await
        .unwrap();
    assert_eq!(replayed, "LFG joined.");
    // No duplicated signup or audit, and the member who left stays gone.
    assert!(store::list_lfg_signups(&fresh, post)
        .await
        .unwrap()
        .is_empty());
    let audits: Vec<(String,)> =
        sqlx::query_as("SELECT outcome FROM announcements_audit_log WHERE id = $1")
            .bind("lfg:signup:8401")
            .fetch_all(&fresh)
            .await
            .unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].0, "joined");
    // The replay retried exactly the refresh, posting nothing new.
    assert_eq!(channel_requests(&replay_mock, "PATCH"), 1);
    assert_eq!(channel_requests(&replay_mock, "POST"), 0);
    replay_mock.shutdown().await;
    fresh.close().await;
    db.cleanup().await;
}

// Real PostgreSQL counterparts to the offline acceptance suite. New cases use
// the guarded disposable-database fixture and run in the normal db test lane.
async fn durable_select_db() -> two_bot_testsupport::TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("test bootstrap required");
    two_bot_testsupport::TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("disposable LFG test database")
}

async fn replay_post(pool: &sqlx::PgPool, id: &str, guild: &str) {
    let post = lfg::LfgPost {
        id: id.into(),
        guild_id: guild.into(),
        channel_id: CHANNEL.into(),
        message_id: Some("5000".into()),
        title: "Replay raid".into(),
        starts_at: "2099-09-11T20:00:00Z".into(),
        status: lfg::LfgStatus::Open,
        created_by: "3333".into(),
        created_at: "2026-09-30T00:00:00.000Z".into(),
        closed_at: None,
    };
    let roles = lfg::spec_roles(
        &post.id,
        &lfg::parse_role_spec("tank:Tank:2,healer:Healer:2").unwrap(),
    );
    store::put_lfg(pool, &post, &roles, true).await.unwrap();
}

fn signup_action(post: &str, role: &str) -> lfg::LfgSelectAction {
    lfg::LfgSelectAction::Signup {
        post_id: post.into(),
        role_key: role.into(),
    }
}

fn leave_action(post: &str) -> lfg::LfgSelectAction {
    lfg::LfgSelectAction::Leave {
        post_id: post.into(),
    }
}

// Compare complete rows, not just counts: a replay must not switch a role,
// rewrite joined_at, close a post, or overwrite the original audit identity.
async fn select_snapshot(pool: &sqlx::PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'posts', (SELECT coalesce(jsonb_agg(to_jsonb(p) ORDER BY to_jsonb(p)::text), '[]'::jsonb) FROM lfg_posts p),
            'roles', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY to_jsonb(r)::text), '[]'::jsonb) FROM lfg_roles r),
            'signups', (SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY to_jsonb(s)::text), '[]'::jsonb) FROM lfg_signups s),
            'audits', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY to_jsonb(a)::text), '[]'::jsonb) FROM announcements_audit_log a))",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn refuse_divergent_select(
    original: lfg::LfgSelectAction,
    divergent: lfg::LfgSelectAction,
    guild: &str,
    actor: &str,
) {
    let db = durable_select_db().await;
    let posts = [("replay-a", GUILD), ("replay-b", GUILD)];
    let leaving = matches!(&original, lfg::LfgSelectAction::Leave { .. });
    for (post, guild) in posts {
        replay_post(db.pool(), post, guild).await;
        if leaving {
            store::signup_lfg(
                db.pool(),
                guild,
                post,
                "tank",
                "3333",
                "2026-09-30T00:01:00Z",
            )
            .await
            .unwrap();
        }
    }
    if leaving {
        // A different actor/target would have a real membership to remove.
        store::signup_lfg(
            db.pool(),
            GUILD,
            "replay-a",
            "healer",
            "3334",
            "2026-09-30T00:01:00Z",
        )
        .await
        .unwrap();
    }
    let mock = MockRest::start(vec![], ScriptedResponse::status(200)).await;
    let executor =
        ActionExecutor::with_proxy("lfg-test-token".into(), Some(mock.origin())).unwrap();
    let service = LfgInteractions::new(db.pool().clone());
    assert_eq!(
        service
            .execute(
                &executor,
                LfgRequest::Select(original),
                GUILD,
                "3333",
                8501,
                BOT
            )
            .await
            .unwrap(),
        if leaving { "LFG left." } else { "LFG joined." }
    );
    assert_eq!(channel_requests(&mock, "PATCH"), 1);
    drop(service);
    mock.shutdown().await;

    // A new service AND independent pool exclude cooldown/process-local state.
    let fresh = db.independent_pool().await.unwrap();
    let before = select_snapshot(&fresh).await;
    assert_eq!(before["audits"].as_array().unwrap().len(), 1);
    let replay_mock = MockRest::start(vec![], ScriptedResponse::status(200)).await;
    let executor =
        ActionExecutor::with_proxy("lfg-test-token".into(), Some(replay_mock.origin())).unwrap();
    let restarted = LfgInteractions::new(fresh.clone());
    let result = restarted
        .execute(
            &executor,
            LfgRequest::Select(divergent),
            guild,
            actor,
            8501,
            BOT,
        )
        .await;
    let after = select_snapshot(&fresh).await;
    let requests = replay_mock.requests();
    replay_mock.shutdown().await;
    drop(restarted);
    fresh.close().await;
    db.close().await.unwrap();
    assert!(
        result.is_err(),
        "divergent Select reused an interaction: {result:?}"
    );
    assert_eq!(
        after, before,
        "divergent Select changed durable state or audit"
    );
    assert!(
        requests.is_empty(),
        "refused replay must not refresh or post"
    );
}

macro_rules! durable_select_divergence {
    ($name:ident, $original:expr, $divergent:expr, $guild:expr, $actor:expr) => {
        #[tokio::test]
        #[ignore = "needs TWO_TEST_DATABASE_URL disposable database"]
        async fn $name() {
            refuse_divergent_select($original, $divergent, $guild, $actor).await;
        }
    };
}

durable_select_divergence!(
    durable_select_signup_binds_guild,
    signup_action("replay-a", "tank"),
    signup_action("replay-a", "tank"),
    "9999",
    "3333"
);
durable_select_divergence!(
    durable_select_signup_binds_actor,
    signup_action("replay-a", "tank"),
    signup_action("replay-a", "tank"),
    GUILD,
    "3334"
);
durable_select_divergence!(
    durable_select_signup_binds_target,
    signup_action("replay-a", "tank"),
    signup_action("replay-b", "tank"),
    GUILD,
    "3333"
);
durable_select_divergence!(
    durable_select_signup_binds_role,
    signup_action("replay-a", "tank"),
    signup_action("replay-a", "healer"),
    GUILD,
    "3333"
);
durable_select_divergence!(
    durable_select_leave_cannot_reuse_signup_identity,
    signup_action("replay-a", "tank"),
    leave_action("replay-a"),
    GUILD,
    "3333"
);
durable_select_divergence!(
    durable_select_leave_binds_guild,
    leave_action("replay-a"),
    leave_action("replay-a"),
    "9999",
    "3333"
);
durable_select_divergence!(
    durable_select_leave_binds_actor,
    leave_action("replay-a"),
    leave_action("replay-a"),
    GUILD,
    "3334"
);
durable_select_divergence!(
    durable_select_leave_binds_target,
    leave_action("replay-a"),
    leave_action("replay-b"),
    GUILD,
    "3333"
);
durable_select_divergence!(
    durable_select_signup_cannot_reuse_leave_identity,
    leave_action("replay-a"),
    signup_action("replay-a", "tank"),
    GUILD,
    "3333"
);

#[tokio::test]
#[ignore = "needs TWO_TEST_DATABASE_URL disposable database"]
async fn durable_select_identical_leave_replay_preserves_later_signup() {
    let db = durable_select_db().await;
    replay_post(db.pool(), "replay-a", GUILD).await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(200)).await;
    let executor =
        ActionExecutor::with_proxy("lfg-test-token".into(), Some(mock.origin())).unwrap();
    let service = LfgInteractions::new(db.pool().clone());
    for (id, action, reply) in [
        (8601, signup_action("replay-a", "tank"), "LFG joined."),
        (8602, leave_action("replay-a"), "LFG left."),
        (8603, signup_action("replay-a", "healer"), "LFG joined."),
    ] {
        if id == 8603 {
            // Leave is cooldown-exempt, but the same actor's next signup is not.
            // This DB test uses an unpaused clock, so wait out the signup cooldown.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        assert_eq!(
            service
                .execute(
                    &executor,
                    LfgRequest::Select(action),
                    GUILD,
                    "3333",
                    id,
                    BOT
                )
                .await
                .unwrap(),
            reply
        );
    }
    drop(service);
    mock.shutdown().await;
    let fresh = db.independent_pool().await.unwrap();
    let saved = store::list_lfg_signups(&fresh, "replay-a").await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].user_id, "3333");
    assert_eq!(saved[0].role_key, "healer");
    let before = select_snapshot(&fresh).await;
    assert_eq!(before["audits"].as_array().unwrap().len(), 3);
    let replay_mock = MockRest::start(vec![], ScriptedResponse::status(200)).await;
    let executor =
        ActionExecutor::with_proxy("lfg-test-token".into(), Some(replay_mock.origin())).unwrap();
    let restarted = LfgInteractions::new(fresh.clone());
    let result = restarted
        .execute(
            &executor,
            LfgRequest::Select(leave_action("replay-a")),
            GUILD,
            "3333",
            8602,
            BOT,
        )
        .await;
    let after = select_snapshot(&fresh).await;
    let requests = replay_mock.requests();
    replay_mock.shutdown().await;
    drop(restarted);
    fresh.close().await;
    db.close().await.unwrap();
    assert_eq!(result.unwrap(), "LFG left.");
    assert_eq!(
        after, before,
        "an identical leave must not remove a later signup or append an audit"
    );
    assert_eq!(
        requests.len(),
        1,
        "replay only refreshes the existing message"
    );
    assert_eq!(requests[0].method, "PATCH");
    assert_eq!(requests[0].path, "/api/v10/channels/4444/messages/5000");
    assert_eq!(body(&requests[0])["allowed_mentions"]["parse"], json!([]));
    assert!(body(&requests[0])["content"]
        .as_str()
        .unwrap()
        .contains("<@3333>"));
}

#[tokio::test]
#[ignore = "needs TWO_TEST_DATABASE_URL disposable database"]
async fn durable_select_legacy_null_reason_cannot_prove_request_identity() {
    let db = durable_select_db().await;
    replay_post(db.pool(), "replay-a", GUILD).await;
    // Historical rows lack the role/action binding; matching the other fields
    // and a prior joined outcome is not proof this request is identical.
    sqlx::query("INSERT INTO announcements_audit_log
        (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
        VALUES ('lfg:signup:8701', $1, '3333', 'lfg.signup', 'replay-a', 'joined', NULL, '2026-09-30T00:01:00Z')")
        .bind(GUILD)
        .execute(db.pool())
        .await
        .unwrap();
    let fresh = db.independent_pool().await.unwrap();
    let before = select_snapshot(&fresh).await;
    assert_eq!(before["audits"].as_array().unwrap().len(), 1);
    assert!(before["audits"][0]["reason"].is_null());
    assert_eq!(before["signups"], json!([]));
    let mock = MockRest::start(vec![], ScriptedResponse::status(200)).await;
    let executor =
        ActionExecutor::with_proxy("lfg-test-token".into(), Some(mock.origin())).unwrap();
    let service = LfgInteractions::new(fresh.clone());
    let result = service
        .execute(
            &executor,
            LfgRequest::Select(signup_action("replay-a", "tank")),
            GUILD,
            "3333",
            8701,
            BOT,
        )
        .await;
    let after = select_snapshot(&fresh).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(service);
    fresh.close().await;
    db.close().await.unwrap();
    assert!(
        result.is_err(),
        "legacy audit does not prove identical delivery: {result:?}"
    );
    assert_eq!(after, before);
    assert!(
        requests.is_empty(),
        "legacy uncertainty must not refresh or post"
    );
}

async fn wait_for_select_lock(pool: &sqlx::PgPool, lock_type: &str) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
                 WHERE a.datname = current_database() AND NOT l.granted AND l.locktype = $1
                 AND cardinality(pg_blocking_pids(l.pid)) > 0
                 AND ($1 = 'advisory' OR (l.relation = 'announcements_audit_log'::regclass AND l.mode = 'RowExclusiveLock')))",
            )
            .bind(lock_type)
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
#[ignore = "needs TWO_TEST_DATABASE_URL disposable database"]
async fn durable_select_concurrent_different_posts_serialize_before_the_first_audit_exists() {
    let db = durable_select_db().await;
    replay_post(db.pool(), "replay-a", GUILD).await;
    replay_post(db.pool(), "replay-b", GUILD).await;
    let observer = db.independent_pool().await.unwrap();
    let second_pool = db.independent_pool().await.unwrap();
    // SHARE allows the replay's audit SELECT but blocks INSERT. The first
    // mutation commits while its audit is absent: without an interaction lock,
    // a differently-targeted request can mutate too, then queue for this table.
    let mut audit_gate = observer.begin().await.unwrap();
    sqlx::query("LOCK TABLE announcements_audit_log IN SHARE MODE")
        .execute(&mut *audit_gate)
        .await
        .unwrap();
    let mock = MockRest::start(vec![], ScriptedResponse::status(200)).await;
    let first_service = LfgInteractions::new(db.pool().clone());
    let first_executor =
        ActionExecutor::with_proxy("lfg-test-token".into(), Some(mock.origin())).unwrap();
    let first = tokio::spawn(async move {
        first_service
            .execute(
                &first_executor,
                LfgRequest::Select(signup_action("replay-a", "tank")),
                GUILD,
                "3333",
                8801,
                BOT,
            )
            .await
    });
    let first_waiting = wait_for_select_lock(&observer, "relation").await;
    let before = select_snapshot(&observer).await;
    let second_service = LfgInteractions::new(second_pool.clone());
    let second_executor =
        ActionExecutor::with_proxy("lfg-test-token".into(), Some(mock.origin())).unwrap();
    let second = tokio::spawn(async move {
        second_service
            .execute(
                &second_executor,
                LfgRequest::Select(signup_action("replay-b", "tank")),
                GUILD,
                "3333",
                8801,
                BOT,
            )
            .await
    });
    // Observe a real PostgreSQL waiter, not a wall-clock guess or a local
    // semaphore: services, pools and REST executors are independent.
    let second_waiting = wait_for_select_lock(&observer, "advisory").await;
    let while_waiting = select_snapshot(&observer).await;
    let premature_requests = mock.requests();
    // Always release the test-owned lock before assertions or joining workers.
    audit_gate.commit().await.unwrap();
    let first_result = first.await.unwrap();
    let second_result = second.await.unwrap();
    let after = select_snapshot(&observer).await;
    let requests = mock.requests();
    mock.shutdown().await;
    second_pool.close().await;
    observer.close().await;
    db.close().await.unwrap();
    assert!(
        first_waiting,
        "first service must reach the blocked audit INSERT"
    );
    assert_eq!(before["signups"].as_array().unwrap().len(), 1);
    assert_eq!(
        before["audits"],
        json!([]),
        "race window has no replay receipt yet"
    );
    assert!(
        second_waiting,
        "same interaction on another post must wait on an advisory lock"
    );
    assert_eq!(
        while_waiting, before,
        "contender must not mutate before the first audit commits"
    );
    assert!(
        premature_requests.is_empty(),
        "no channel work before the audit"
    );
    assert_eq!(first_result.unwrap(), "LFG joined.");
    assert!(
        second_result.is_err(),
        "contender must refuse the now-durable divergent receipt: {second_result:?}"
    );
    for key in ["posts", "roles", "signups"] {
        assert_eq!(after[key], before[key], "contender changed {key}");
    }
    assert_eq!(after["audits"].as_array().unwrap().len(), 1);
    assert_eq!(after["audits"][0]["target_key"], "replay-a");
    assert_eq!(after["audits"][0]["outcome"], "joined");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "PATCH");
    assert_eq!(requests[0].path, "/api/v10/channels/4444/messages/5000");
    assert_eq!(body(&requests[0])["allowed_mentions"]["parse"], json!([]));
}
