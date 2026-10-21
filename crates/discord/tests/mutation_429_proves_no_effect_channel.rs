//! M8.1 acceptance: mutation HTTP 429 proves no effect (website channel slice).
//!
//! These database-backed regressions verify that the website channel executor
//! releases a rate-limited mutation attempt for retry, while uncertain outcomes
//! (5xx, timeout, cancellation, and unexpected statuses) remain fenced.
//!
//! Run with the disposable test database (never staging or production):
//!
//! ```sh
//! TWO_TEST_DATABASE_URL=postgres://agent_test@agent-testdb:5432/agent_test \
//!   cargo test -p two-bot-discord --features db --locked \
//!   --test mutation_429_proves_no_effect_channel -- --ignored --test-threads=1
//! ```
//!
//! Cancellation is modelled distinctly from timeout: the caller drops the
//! in-flight attempt (aborted task) instead of waiting for Discord's 5 s
//! abort. Both must stay fenced.
#![cfg(feature = "db")]

#[path = "mutation_429_common.rs"]
mod common;

use common::{fresh_message_id, MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::Duration;
use two_bot_core::channel_moderation_store::ChannelModerationStore;
use two_bot_core::internal_actions::ErrorCode;
use two_bot_core::{ModerationActor, ModerationPolicy};
use two_bot_discord::internal_channel_moderation::{
    InternalChannelConfig, InternalChannelExecutor, InternalChannelRequest,
};
use two_bot_discord::{ActionExecutor, MODERATION_TIMEOUT_MS};

const GUILD: &str = "111111111111111111";
const CHANNEL: &str = "222222222222222222";
const ACTOR: &str = "333333333333333333";
const TIME: &str = "2026-09-30T00:00:00.000Z";

// Signing and verification share an ephemeral key, never a stored credential.
static SECRET: LazyLock<String> = LazyLock::new(|| {
    let mut bytes = [0u8; 32];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut bytes)
        .expect("OS randomness for the test audit key");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
});

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn single_delete_429_releases_attempt_for_same_key_retry() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            channel(None, "0"),
            ScriptedResponse::json(
                200,
                json!([{"id": fresh_message_id(1), "author": {"bot": false}}]),
            ),
            ScriptedResponse::status(429),
            channel(None, "0"),
            ScriptedResponse::json(
                200,
                json!([{"id": fresh_message_id(1), "author": {"bot": false}}]),
            ),
            ScriptedResponse::status(204),
            channel(None, "0"),
            ScriptedResponse::json(200, json!({"id": CHANNEL})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let req = request("moderation.purge", json!({"count": 1}));
    assert_eq!(
        exec.execute(&req, &actor(), "single-429", "single-429-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::RateLimited
    );
    // A rate-limited delete proves no effect: both fences release, so the
    // same key is a real second attempt and the lane is free for others.
    assert_reservations(&db, 0).await;
    let retry = exec
        .execute(&req, &actor(), "single-429", "single-429-key", TIME)
        .await
        .unwrap();
    assert_eq!(retry.affected, Some(1));
    assert!(!retry.replayed);
    exec.execute(
        &request("moderation.slowmode", json!({"seconds": 0})),
        &actor(),
        "after-single-429",
        "after-single-429-key",
        TIME,
    )
    .await
    .unwrap();
    let wire = mock.requests();
    assert_eq!(wire.len(), 8);
    assert_eq!(wire[2].method, "DELETE");
    assert_eq!(wire[5].method, "DELETE");
    assert_eq!(wire[7].method, "PATCH");
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn bulk_delete_429_releases_attempt_for_same_key_retry() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            channel(None, "0"),
            ScriptedResponse::json(
                200,
                json!([
                    {"id": fresh_message_id(1), "author": {"bot": false}},
                    {"id": fresh_message_id(2), "author": {"bot": false}}
                ]),
            ),
            ScriptedResponse::status(429),
            channel(None, "0"),
            ScriptedResponse::json(
                200,
                json!([
                    {"id": fresh_message_id(1), "author": {"bot": false}},
                    {"id": fresh_message_id(2), "author": {"bot": false}}
                ]),
            ),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let req = request("moderation.purge", json!({"count": 2}));
    assert_eq!(
        exec.execute(&req, &actor(), "bulk-429", "bulk-429-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::RateLimited
    );
    assert_reservations(&db, 0).await;
    let retry = exec
        .execute(&req, &actor(), "bulk-429", "bulk-429-key", TIME)
        .await
        .unwrap();
    assert_eq!(retry.affected, Some(2));
    assert!(!retry.replayed);
    let wire = mock.requests();
    assert_eq!(wire.len(), 6);
    assert_eq!(wire[2].method, "POST");
    assert!(wire[2].path.ends_with("/messages/bulk-delete"));
    assert_eq!(wire[5].method, "POST");
    assert!(wire[5].path.ends_with("/messages/bulk-delete"));
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn channel_overwrite_429_releases_attempt_and_retires_only_its_seed() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            channel(Some("3072"), "8192"),
            ScriptedResponse::status(429),
            channel(Some("3072"), "8192"),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let req = request("moderation.lockdown", json!({}));
    assert_eq!(
        exec.execute(&req, &actor(), "overwrite-429", "overwrite-429-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::RateLimited
    );
    // The refused first PUT never changed the overwrite, so its new seed is
    // retired exactly like a 403 refusal; the lane itself is released.
    assert!(db.store().get_lockdown(CHANNEL).await.unwrap().is_none());
    assert_reservations(&db, 0).await;
    assert_eq!(
        exec.execute(&req, &actor(), "overwrite-429", "overwrite-429-key", TIME)
            .await
            .unwrap()
            .outcome,
        "locked_down"
    );
    assert_eq!(mock.requests().len(), 4);
    assert_eq!(mock.requests()[1].method, "PUT");
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn slowmode_429_releases_attempt_for_same_and_distinct_keys() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            channel(Some("3072"), "8192"),
            ScriptedResponse::status(429),
            channel(Some("3072"), "8192"),
            ScriptedResponse::json(200, json!({"id": CHANNEL})),
            channel(Some("3072"), "8192"),
            ScriptedResponse::json(200, json!({"id": CHANNEL})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let req = request("moderation.slowmode", json!({"seconds": 30}));
    assert_eq!(
        exec.execute(&req, &actor(), "slowmode-429", "slowmode-429-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::RateLimited
    );
    assert_reservations(&db, 0).await;
    assert_eq!(
        exec.execute(&req, &actor(), "slowmode-429", "slowmode-429-key", TIME)
            .await
            .unwrap()
            .outcome,
        "slowmode_updated"
    );
    exec.execute(
        &request("moderation.slowmode", json!({"seconds": 0})),
        &actor(),
        "slowmode-distinct",
        "slowmode-distinct-key",
        TIME,
    )
    .await
    .unwrap();
    let wire = mock.requests();
    assert_eq!(wire.len(), 6);
    assert_eq!(wire.iter().filter(|r| r.method == "PATCH").count(), 3);
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn unlock_429_releases_lane_but_preserves_recovery_seed() {
    let db = TestDb::new().await;
    let original = seed_lockdown(&db, Some("3072")).await;
    let mock = MockRest::start(
        vec![
            channel(Some("1024"), "10240"),
            ScriptedResponse::status(429),
            channel(Some("1024"), "10240"),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let req = request("moderation.unlock", json!({}));
    assert_eq!(
        exec.execute(&req, &actor(), "unlock-429", "unlock-429-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::RateLimited
    );
    // The lane releases so /unlock stays reachable, but the recovery seed is
    // untouched: a later unlock still restores the exact original masks.
    assert_eq!(
        db.store().get_lockdown(CHANNEL).await.unwrap(),
        Some(original)
    );
    assert_reservations(&db, 0).await;
    assert_eq!(
        exec.execute(&req, &actor(), "unlock-429", "unlock-429-key", TIME)
            .await
            .unwrap()
            .outcome,
        "unlocked"
    );
    assert!(db.store().get_lockdown(CHANNEL).await.unwrap().is_none());
    assert_eq!(mock.requests().len(), 4);
    assert_eq!(mock.requests()[1].method, "PUT");
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn lockdown_429_preserves_prior_seed_while_releasing_the_lane() {
    let db = TestDb::new().await;
    let original = seed_lockdown(&db, Some("3072")).await;
    let mock = MockRest::start(
        vec![
            channel(Some("1024"), "10240"),
            ScriptedResponse::status(429),
            channel(Some("1024"), "10240"),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let req = request("moderation.lockdown", json!({}));
    assert_eq!(
        exec.execute(&req, &actor(), "relock-429", "relock-429-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::RateLimited
    );
    // The first seed survives the refused write; only the fences release.
    assert_eq!(
        db.store().get_lockdown(CHANNEL).await.unwrap(),
        Some(original.clone())
    );
    assert_reservations(&db, 0).await;
    assert_eq!(
        exec.execute(&req, &actor(), "relock-429", "relock-429-key", TIME)
            .await
            .unwrap()
            .outcome,
        "locked_down"
    );
    assert_eq!(
        db.store().get_lockdown(CHANNEL).await.unwrap(),
        Some(original)
    );
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn slowmode_5xx_and_unexpected_statuses_stay_fenced() {
    for status in [503, 202, 302, 408, 409, 425] {
        let db = TestDb::new().await;
        let mock = MockRest::start(
            vec![
                channel(Some("3072"), "8192"),
                ScriptedResponse::status(status),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        let exec = executor(db.pool.clone(), &mock);
        let req = request("moderation.slowmode", json!({"seconds": 30}));
        assert_eq!(
            exec.execute(&req, &actor(), "slowmode-fence", "slowmode-fence-key", TIME)
                .await
                .unwrap_err()
                .code,
            ErrorCode::DiscordUnavailable,
            "status {status}"
        );
        assert_eq!(
            exec.execute(&req, &actor(), "slowmode-fence", "slowmode-fence-key", TIME)
                .await
                .unwrap_err()
                .code,
            ErrorCode::InProgress,
            "status {status}"
        );
        assert_eq!(
            exec.execute(
                &request("moderation.lockdown", json!({})),
                &actor(),
                "distinct-fence",
                "distinct-fence-key",
                TIME,
            )
            .await
            .unwrap_err()
            .code,
            ErrorCode::InProgress,
            "status {status}"
        );
        let wire = mock.requests();
        assert_eq!(wire.len(), 2, "status {status}");
        assert_eq!(wire[1].method, "PATCH", "status {status}");
        assert_reservations(&db, 1).await;
        mock.shutdown().await;
        db.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn slowmode_timeout_stays_fenced() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            channel(Some("3072"), "8192"),
            ScriptedResponse::json(200, json!({"id": CHANNEL}))
                .delayed(Duration::from_millis(MODERATION_TIMEOUT_MS + 250)),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let req = request("moderation.slowmode", json!({"seconds": 30}));
    assert_eq!(
        exec.execute(
            &req,
            &actor(),
            "slowmode-timeout",
            "slowmode-timeout-key",
            TIME
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::UpstreamTimeout
    );
    assert_eq!(
        exec.execute(
            &req,
            &actor(),
            "slowmode-timeout",
            "slowmode-timeout-key",
            TIME
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InProgress
    );
    assert_eq!(mock.requests().len(), 2);
    assert_reservations(&db, 1).await;
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn cancelled_lockdown_stays_fenced_with_seed_retained() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            channel(Some("3072"), "8192"),
            ScriptedResponse::status(204).delayed(Duration::from_secs(2)),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    // Cancel the caller while the Discord PUT is in flight: dropping the
    // future must not release the lane or the new seed, because the mutation
    // may still land.
    let attempt = tokio::spawn({
        let exec = exec.clone();
        let req = request("moderation.lockdown", json!({}));
        let actor = actor();
        async move {
            exec.execute(&req, &actor, "cancel-lock", "cancel-lock-key", TIME)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if mock.requests().iter().any(|r| r.method == "PUT") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    attempt.abort();
    let _ = attempt.await;
    let req = request("moderation.lockdown", json!({}));
    assert_eq!(
        exec.execute(&req, &actor(), "cancel-lock", "cancel-lock-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::InProgress
    );
    assert_eq!(
        exec.execute(
            &request("moderation.unlock", json!({})),
            &actor(),
            "distinct-after-cancel",
            "distinct-after-cancel-key",
            TIME,
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InProgress
    );
    assert!(db.store().get_lockdown(CHANNEL).await.unwrap().is_some());
    assert_eq!(mock.requests().len(), 2);
    assert_reservations(&db, 1).await;
    mock.shutdown().await;
    db.cleanup().await;
}

async fn seed_lockdown(db: &TestDb, prior_allow: Option<&str>) -> two_bot_core::LockdownRecord {
    db.store()
        .record_lockdown(
            CHANNEL,
            GUILD,
            &two_bot_core::LockdownSeed {
                prior_allow: prior_allow.unwrap_or("0").to_owned(),
                prior_deny: if prior_allow.is_some() { "8192" } else { "0" }.to_owned(),
                prior_exists: prior_allow.is_some(),
            },
            "cleanup",
            TIME,
        )
        .await
        .unwrap()
}

async fn assert_reservations(db: &TestDb, expected: i64) {
    let claims: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_idempotency")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let channels: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_channel_executions")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(claims, expected);
    assert_eq!(channels, expected);
}

fn actor() -> ModerationActor {
    ModerationActor {
        user_id: ACTOR.to_owned(),
        role_ids: vec![],
        highest_role_position: 10,
        permissions: two_bot_core::commands::PERM_MANAGE_CHANNELS
            | two_bot_core::commands::PERM_MANAGE_MESSAGES,
    }
}

fn request(action: &str, extra: Value) -> InternalChannelRequest {
    let mut body = json!({"actor_id": ACTOR, "channel_id": CHANNEL, "reason": "  cleanup  "});
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    InternalChannelRequest::from_body(action, body.as_object().unwrap()).unwrap()
}

fn executor(pool: PgPool, mock: &MockRest) -> InternalChannelExecutor {
    let _ = rustls::crypto::ring::default_provider().install_default();
    InternalChannelExecutor::new(
        ChannelModerationStore::from_pool(pool),
        ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap(),
        InternalChannelConfig {
            guild_id: GUILD.to_owned(),
            enabled: true,
            policy: ModerationPolicy {
                owen_user_id: "444444444444444444".to_owned(),
                protected_role_ids: Default::default(),
                bot_user_id: None,
            },
            audit_secret: Some(SECRET.to_owned()),
        },
    )
    .unwrap()
}

fn channel(allow: Option<&str>, deny: &str) -> ScriptedResponse {
    let overwrites = allow
        .map(|allow| vec![json!({"id":GUILD,"type":0,"allow":allow,"deny":deny})])
        .unwrap_or_default();
    ScriptedResponse::json(
        200,
        json!({"id": CHANNEL,"guild_id": GUILD,"type":0,"permission_overwrites":overwrites}),
    )
}

fn allowed_test_url(url: &str, ci: bool) -> bool {
    // Also accept the workspace bootstrap URL used by the check.yml coverage
    // sweep; callers still connect to `agent_test` with an isolated schema,
    // so data never lands in the bootstrap database.
    url == "postgres://agent_test@agent-testdb:5432/agent_test"
        || url == "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci"
        || (ci && url == "postgres://agent_test@127.0.0.1:5432/agent_test")
        || (ci && url == "postgres://agent_test:@127.0.0.1:5432/two_bot_test_ci")
}

#[test]
fn database_guard_refuses_credentials_and_non_test_targets() {
    assert!(allowed_test_url(
        "postgres://agent_test@agent-testdb:5432/agent_test",
        false
    ));
    assert!(allowed_test_url(
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
        false
    ));
    assert!(allowed_test_url(
        "postgres://agent_test@127.0.0.1:5432/agent_test",
        true
    ));
    assert!(allowed_test_url(
        "postgres://agent_test:@127.0.0.1:5432/two_bot_test_ci",
        true
    ));
    for url in [
        "postgres://agent_test@127.0.0.1:5432/agent_test",
        "postgres://agent_test:@127.0.0.1:5432/two_bot_test_ci",
        "postgres://agent_test@production:5432/agent_test",
        "postgres://agent_test@staging:5432/agent_test",
        "postgres://admin@agent-testdb:5432/agent_test",
        "postgres://agent_test:password@agent-testdb:5432/agent_test",
        "postgres://agent_test@agent-testdb:5432/production",
        "postgres://agent_test@agent-testdb:5432/agent_test?host=production",
    ] {
        assert!(!allowed_test_url(url, false));
    }
}

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    schema: String,
}

impl TestDb {
    async fn new() -> Self {
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test database URL required");
        let ci = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
        assert!(allowed_test_url(&url, ci), "non-test database refused");
        assert!(std::env::var_os("PGOPTIONS").is_none(), "PGOPTIONS refused");
        // Never read .pgpass or inherit credentials, host, role, or TLS keys.
        let host = if url.contains("@127.0.0.1:") {
            "127.0.0.1"
        } else {
            "agent-testdb"
        };
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
            .expect("empty-password test container");
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!(
            "m429_{}_{}_{}",
            std::process::id(),
            nonce,
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = Self::connect(&options, &schema).await;
        let store = ChannelModerationStore::from_pool(pool.clone());
        store.migrate().await.unwrap();
        Self {
            admin,
            pool,
            schema,
        }
    }

    async fn connect(options: &PgConnectOptions, schema: &str) -> PgPool {
        PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.clone().options([("search_path", schema)]))
            .await
            .unwrap()
    }

    fn store(&self) -> ChannelModerationStore {
        ChannelModerationStore::from_pool(self.pool.clone())
    }

    async fn cleanup(self) {
        self.pool.close().await;
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
