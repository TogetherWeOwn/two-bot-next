#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    ChannelClaim, ChannelModerationStore, HandlerId, InteractionRouter, ModerationAction,
    RouterGates,
};
use two_bot_discord::{register_channel_handlers, ActionExecutor, ChannelModerationRuntime};

const GUILD: &str = "111111111111111111";
const CHANNEL: &str = "222222222222222222";
const ACTOR: &str = "333333333333333333";
const PERMISSIONS: u64 = (1 << 4) | (1 << 13);

struct Database {
    store: ChannelModerationStore,
    admin: PgPool,
    schema: String,
}
impl Database {
    async fn open() -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit isolated test service required");
        let ci = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
        let host = if url == "postgres://agent_test@agent-testdb:5432/agent_test" {
            "agent-testdb"
        } else if ci && url == "postgres://agent_test@127.0.0.1:5432/agent_test" {
            "127.0.0.1"
        } else {
            panic!("non-test database refused")
        };
        assert!(std::env::var_os("PGOPTIONS").is_none());
        // No inherited password or .pgpass; no ambient host, role or TLS mode.
        let options = PgConnectOptions::new_without_pgpass()
            .host(host)
            .port(5432)
            .username("agent_test")
            .password("")
            .database("agent_test")
            .ssl_mode(PgSslMode::Disable);
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .unwrap();
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!(
            "cm_runtime_{}_{}_{}",
            std::process::id(),
            nonce,
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.options([("search_path", schema.clone())]))
            .await
            .unwrap();
        let store = ChannelModerationStore::from_pool(pool);
        store.migrate().await.unwrap();
        Self {
            store,
            admin,
            schema,
        }
    }
    async fn count(&self, table: &str) -> i64 {
        assert!([
            "moderation_audit",
            "moderation_idempotency",
            "moderation_channel_execution",
            "moderation_lockdowns"
        ]
        .contains(&table));
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(self.store.pool())
            .await
            .unwrap()
    }
    async fn close(self) {
        self.store.pool().close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}

fn router(enabled: bool) -> InteractionRouter {
    let mut router = InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD.parse().unwrap()),
        moderation: enabled,
        scorecard: false,
        automations: false,
        announcements: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    register_channel_handlers(&mut router);
    router
}

fn interaction(id: u64, action: &str, options: Value, permissions: u64) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "444444444444444444", "type": 2,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0, "permissions": permissions.to_string()},
        "data": {"id": "555555555555555555", "type": 1, "name": action, "options": options},
        "member": {"user": {"id": ACTOR, "username": "test-member", "discriminator": "0", "avatar": null},
                   "roles": [], "flags": 0, "deaf": false, "mute": false, "permissions": permissions.to_string()},
        "token": "synthetic-interaction-token", "version": 1, "locale": "en-US", "guild_locale": "en-US",
        "authorizing_integration_owners": {"0": GUILD}, "entitlements": []
    })).expect("synthetic Twilight interaction")
}

fn options(numeric: Option<(&str, i64)>) -> Value {
    let mut options = vec![json!({"name":"reason", "type":3, "value":"mock audit reason"})];
    if let Some((name, number)) = numeric {
        options.push(json!({"name":name, "type":4, "value":number}));
    }
    json!(options)
}

fn runtime(db: &Database, mock: &MockRest) -> ChannelModerationRuntime {
    ChannelModerationRuntime::new(
        db.store.clone(),
        ActionExecutor::with_proxy("synthetic-test-token".to_owned(), Some(mock.origin())).unwrap(),
    )
}

fn overwrite(allow: &str, deny: &str) -> ScriptedResponse {
    ScriptedResponse::json(
        200,
        json!({"permission_overwrites":[{"id":GUILD,"type":0,"allow":allow,"deny":deny}]}),
    )
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service"]
async fn shared_router_gates_bounds_and_required_reason_refuse_without_http() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    let on = router(true);
    for action in [
        ModerationAction::Purge,
        ModerationAction::Slowmode,
        ModerationAction::Lockdown,
        ModerationAction::Unlock,
    ] {
        assert!(on.handler_for(&HandlerId::Moderation(action)).is_some());
    }
    let mut cases = vec![
        (
            router(false),
            interaction(1, "lockdown", options(None), PERMISSIONS),
        ),
        (
            router(true),
            interaction(2, "purge", options(Some(("count", 1))), 0),
        ),
        (
            router(true),
            interaction(3, "slowmode", options(Some(("seconds", 0))), 1 << 13),
        ),
        (
            router(true),
            interaction(4, "purge", options(Some(("count", 0))), PERMISSIONS),
        ),
        (
            router(true),
            interaction(5, "purge", options(Some(("count", 101))), PERMISSIONS),
        ),
        (
            router(true),
            interaction(
                6,
                "slowmode",
                options(Some(("seconds", 21601))),
                PERMISSIONS,
            ),
        ),
        (
            router(true),
            interaction(7, "slowmode", options(Some(("seconds", -1))), PERMISSIONS),
        ),
        (
            router(true),
            interaction(8, "lockdown", json!([]), PERMISSIONS),
        ),
        (
            router(true),
            interaction(
                9,
                "unlock",
                json!([{"name":"reason","type":3,"value":" "}]),
                PERMISSIONS,
            ),
        ),
    ];
    let mut wrong_guild = interaction(10, "lockdown", options(None), PERMISSIONS);
    wrong_guild.guild_id = Some(twilight_model::id::Id::new(999));
    cases.push((router(true), wrong_guild));
    for (router, request) in &cases {
        assert_eq!(
            runtime
                .execute(router, request)
                .await
                .unwrap()
                .unwrap()
                .outcome,
            "refused"
        );
    }
    assert!(mock.requests().is_empty());
    assert_eq!(db.count("moderation_audit").await, cases.len() as i64);
    assert_eq!(db.count("moderation_idempotency").await, 0);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service"]
async fn purge_and_slowmode_success_audit_and_replay_without_repeating_effects() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(
                200,
                json!([{"id":"600000000000000001"},{"id":"600000000000000002"}]),
            ),
            ScriptedResponse::status(204),
            ScriptedResponse::status(200),
            ScriptedResponse::status(200),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let router = router(true);
    let purge = interaction(20, "purge", options(Some(("count", 100))), PERMISSIONS);
    let result = runtime.execute(&router, &purge).await.unwrap().unwrap();
    assert_eq!(result.outcome, "purged");
    assert!(result.text.contains("(2)"));
    let replay = runtime.execute(&router, &purge).await.unwrap().unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.text, result.text);
    for (id, seconds) in [(21, 0), (22, 21600)] {
        assert_eq!(
            runtime
                .execute(
                    &router,
                    &interaction(
                        id,
                        "slowmode",
                        options(Some(("seconds", seconds))),
                        PERMISSIONS
                    )
                )
                .await
                .unwrap()
                .unwrap()
                .outcome,
            "slowmode_updated"
        );
    }
    let metadata: String =
        sqlx::query_scalar("SELECT metadata_json FROM moderation_audit WHERE request_id = '20'")
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&metadata).unwrap(),
        json!({"count":100,"affected":2})
    );
    assert_eq!(mock.requests().len(), 4);
    assert_eq!(db.count("moderation_audit").await, 3);
    assert_eq!(db.count("moderation_channel_execution").await, 0);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service"]
async fn repeated_lockdown_keeps_first_seed_and_unlock_restores_exact_masks() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            overwrite("3072", "8192"),
            ScriptedResponse::status(204),
            overwrite("1024", "10240"),
            ScriptedResponse::status(204),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let router = router(true);
    for id in [30, 31] {
        assert_eq!(
            runtime
                .execute(
                    &router,
                    &interaction(id, "lockdown", options(None), PERMISSIONS)
                )
                .await
                .unwrap()
                .unwrap()
                .outcome,
            "locked_down"
        );
        let rec = db.store.get_lockdown(CHANNEL).await.unwrap().unwrap();
        assert_eq!(
            (rec.prior_allow.as_str(), rec.prior_deny.as_str()),
            ("3072", "8192")
        );
    }
    assert_eq!(
        runtime
            .execute(
                &router,
                &interaction(32, "unlock", options(None), PERMISSIONS)
            )
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "unlocked"
    );
    assert!(db.store.get_lockdown(CHANNEL).await.unwrap().is_none());
    let requests = mock.requests();
    let first: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let restored: Value = serde_json::from_slice(&requests[4].body).unwrap();
    assert_eq!(
        (first["allow"].as_str(), first["deny"].as_str()),
        (Some("1024"), Some("10240"))
    );
    assert_eq!(
        (restored["allow"].as_str(), restored["deny"].as_str()),
        (Some("3072"), Some("8192"))
    );
    assert!(requests
        .iter()
        .filter(|r| r.method == "PUT")
        .all(|r| r.path.ends_with(&format!("/permissions/{GUILD}"))));
    assert_eq!(
        runtime
            .execute(
                &router,
                &interaction(33, "unlock", options(None), PERMISSIONS)
            )
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "refused"
    );
    assert_eq!(
        mock.requests().len(),
        5,
        "untracked manual deny must not be cleared"
    );
    assert_eq!(db.count("moderation_audit").await, 4);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service"]
async fn initially_absent_overwrite_is_deleted_only_after_recorded_lockdown() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"permission_overwrites":[]})),
            ScriptedResponse::status(204),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let router = router(true);
    runtime
        .execute(
            &router,
            &interaction(40, "lockdown", options(None), PERMISSIONS),
        )
        .await
        .unwrap();
    assert!(
        !db.store
            .get_lockdown(CHANNEL)
            .await
            .unwrap()
            .unwrap()
            .prior_exists
    );
    runtime
        .execute(
            &router,
            &interaction(41, "unlock", options(None), PERMISSIONS),
        )
        .await
        .unwrap();
    let requests = mock.requests();
    assert_eq!(requests[2].method, "DELETE");
    assert!(requests[2].path.ends_with(&format!("/permissions/{GUILD}")));
    assert_eq!(db.count("moderation_lockdowns").await, 0);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service"]
async fn ambiguous_unlock_retains_recovery_and_both_claims_across_all_keys() {
    for failure in [
        ScriptedResponse::status(503),
        ScriptedResponse::rate_limited(0.0, "0"),
        ScriptedResponse::status(204).delayed(std::time::Duration::from_millis(5200)),
    ] {
        let db = Database::open().await;
        let mock = MockRest::start(
            vec![
                overwrite("3072", "8192"),
                ScriptedResponse::status(204),
                failure,
            ],
            ScriptedResponse::status(500),
        )
        .await;
        let runtime = runtime(&db, &mock);
        let router = router(true);
        runtime
            .execute(
                &router,
                &interaction(50, "lockdown", options(None), PERMISSIONS),
            )
            .await
            .unwrap();
        let rec = db.store.get_lockdown(CHANNEL).await.unwrap().unwrap();
        let unlock = interaction(51, "unlock", options(None), PERMISSIONS);
        assert_eq!(
            runtime
                .execute(&router, &unlock)
                .await
                .unwrap()
                .unwrap()
                .outcome,
            "in_progress"
        );
        assert_eq!(
            runtime
                .execute(&router, &unlock)
                .await
                .unwrap()
                .unwrap()
                .outcome,
            "in_progress"
        );
        assert_eq!(
            runtime
                .execute(
                    &router,
                    &interaction(52, "lockdown", options(None), PERMISSIONS)
                )
                .await
                .unwrap()
                .unwrap()
                .outcome,
            "in_progress"
        );
        assert_eq!(
            mock.requests().len(),
            3,
            "no automatic retry or competing-key mutation"
        );
        assert_eq!(db.store.get_lockdown(CHANNEL).await.unwrap(), Some(rec));
        assert_eq!(db.count("moderation_channel_execution").await, 1);
        mock.shutdown().await;
        db.close().await;
    }
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service"]
async fn rejected_first_lock_retires_only_its_seed_and_finishes_refusal() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![overwrite("1024", "8192"), ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let router = router(true);
    let lock = interaction(60, "lockdown", options(None), PERMISSIONS);
    assert_eq!(
        runtime
            .execute(&router, &lock)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "refused"
    );
    assert!(
        runtime
            .execute(&router, &lock)
            .await
            .unwrap()
            .unwrap()
            .replayed
    );
    assert_eq!(db.count("moderation_lockdowns").await, 0);
    assert_eq!(db.count("moderation_channel_execution").await, 0);
    assert_eq!(db.count("moderation_audit").await, 1);
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service"]
async fn audit_failure_retries_only_finalization_not_a_successful_discord_effect() {
    let db = Database::open().await;
    sqlx::raw_sql("CREATE SEQUENCE audit_attempts;
        CREATE FUNCTION fail_first_audit() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF nextval('audit_attempts') = 1 THEN RAISE EXCEPTION 'injected audit failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER fail_first BEFORE INSERT ON moderation_audit FOR EACH ROW EXECUTE FUNCTION fail_first_audit();")
        .execute(db.store.pool()).await.unwrap();
    let mock = MockRest::start(
        vec![ScriptedResponse::status(200)],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let router = router(true);
    let request = interaction(70, "slowmode", options(Some(("seconds", 10))), PERMISSIONS);
    assert_eq!(
        runtime
            .execute(&router, &request)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "slowmode_updated"
    );
    assert!(
        runtime
            .execute(&router, &request)
            .await
            .unwrap()
            .unwrap()
            .replayed
    );
    assert_eq!(db.count("moderation_audit").await, 1);
    assert_eq!(db.count("moderation_channel_execution").await, 0);
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service"]
async fn first_seed_and_durable_lane_exist_before_lock_put_is_accepted() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            overwrite("3072", "8192"),
            ScriptedResponse::status(204).delayed(std::time::Duration::from_millis(400)),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let running = tokio::spawn(async move {
        runtime
            .execute(
                &router(true),
                &interaction(75, "lockdown", options(None), PERMISSIONS),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while mock.requests().len() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(!running.is_finished(), "PUT has not yet been accepted");
    let rec = db.store.get_lockdown(CHANNEL).await.unwrap().unwrap();
    assert_eq!(
        (rec.prior_allow.as_str(), rec.prior_deny.as_str()),
        ("3072", "8192")
    );
    assert_eq!(db.count("moderation_channel_execution").await, 1);
    assert_eq!(
        running.await.unwrap().unwrap().unwrap().outcome,
        "locked_down"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service"]
async fn exhausted_audit_failure_retains_lane_and_cannot_redo_successful_effect() {
    let db = Database::open().await;
    sqlx::query("ALTER TABLE moderation_audit ADD CONSTRAINT deny_audit CHECK (outcome <> 'slowmode_updated')")
        .execute(db.store.pool()).await.unwrap();
    let mock = MockRest::start(
        vec![ScriptedResponse::status(200)],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let router = router(true);
    let request = interaction(76, "slowmode", options(Some(("seconds", 10))), PERMISSIONS);
    assert!(runtime.execute(&router, &request).await.is_err());
    assert_eq!(
        runtime
            .execute(&router, &request)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "in_progress"
    );
    assert_eq!(
        runtime
            .execute(
                &router,
                &interaction(77, "slowmode", options(Some(("seconds", 0))), PERMISSIONS)
            )
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "in_progress"
    );
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(db.count("moderation_channel_execution").await, 1);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service"]
async fn stale_tickets_and_action_only_mismatch_never_admit_runtime_effects() {
    use sha2::{Digest, Sha256};
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    let router = router(true);
    let request = interaction(80, "slowmode", options(Some(("seconds", 10))), PERMISSIONS);
    let canonical = json!([
        "moderation.slowmode",
        GUILD,
        ACTOR,
        CHANNEL,
        options(Some(("seconds", 10)))
    ])
    .to_string();
    let hash = hex::encode(Sha256::digest(canonical.as_bytes()));
    let ChannelClaim::Claimed { ticket: old } = db
        .store
        .claim(GUILD, "80", "moderation.slowmode", &hash, "now")
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(db.store.release(&old).await.unwrap());
    let ChannelClaim::Claimed { ticket: new } = db
        .store
        .claim(GUILD, "80", "moderation.slowmode", &hash, "now")
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(!db.store.release(&old).await.unwrap());
    assert!(!db.store.complete(&old, "stale", "{}", "now").await.unwrap());
    assert_eq!(
        runtime
            .execute(&router, &request)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "in_progress"
    );
    assert!(db.store.release(&new).await.unwrap());
    // Exact same content hash, ONLY the action differs.
    db.store
        .claim(GUILD, "80", "moderation.unlock", &hash, "now")
        .await
        .unwrap();
    assert_eq!(
        runtime
            .execute(&router, &request)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "refused"
    );
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
    db.close().await;
}
