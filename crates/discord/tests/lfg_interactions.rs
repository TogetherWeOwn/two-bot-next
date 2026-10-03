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
    invocation(
        id,
        3333,
        permissions,
        json!({"id": "1", "name": "lfg", "type": 1, "options": [
            {"type": 3, "name": "title", "value": "Friday raid @everyone"},
            {"type": 3, "name": "starts-at", "value": "2099-09-11T20:00:00Z"},
            {"type": 3, "name": "roles", "value": "tank:Tank:1,dps:DPS:1"}
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

struct TestDb {
    pool: sqlx::PgPool,
    admin: sqlx::PgPool,
    schema: String,
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
        let search = schema.clone();
        let pool = sqlx::postgres::PgPoolOptions::new()
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
            .expect("schema pool");
        sqlx::migrate!("../cutover/migrations")
            .run(&pool)
            .await
            .expect("migrations");
        Self {
            pool,
            admin,
            schema,
        }
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
    rt.handle(&select(7003, post, 3333, "tank")).await.unwrap();
    assert_eq!(
        store::list_lfg_signups(&db.pool, post).await.unwrap()[0].joined_at,
        joined_at
    );
    rt.handle(&select(7004, post, 3334, "tank")).await.unwrap();
    assert_eq!(last_reply(&mock), "LFG full.");
    rt.handle(&select(7005, post, 3333, "dps")).await.unwrap();
    assert_eq!(last_reply(&mock), "LFG moved.");
    rt.handle(&select(7006, post, 3334, "tank")).await.unwrap();
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
            assert!(last_reply(&mock).contains("acceptance is uncertain"));
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
