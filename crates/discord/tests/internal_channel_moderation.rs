//! Website channel moderation uses only local mock REST and the explicitly
//! guarded disposable test database. Every test owns and drops its own schema.
#![cfg(feature = "db")]
#![allow(dead_code)]

mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Row};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::Duration;
use two_bot_core::channel_moderation_store::ChannelModerationStore;
use two_bot_core::internal_actions::ErrorCode;
use two_bot_core::{ModerationActor, ModerationPolicy};
use two_bot_discord::internal_channel_moderation::{
    InternalChannelConfig, InternalChannelExecutor, InternalChannelRequest,
};
use two_bot_discord::ActionExecutor;

const ACTOR: &str = "333333333333333333";
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
async fn delayed_unlock_cannot_overwrite_a_later_lockdown_cycle() {
    let db = TestDb::new().await;
    let pool_b = db.independent_pool().await;
    let mock = MockRest::start(
        vec![
            channel(Some("3072"), "8192"),
            ScriptedResponse::status(204),
            channel(Some("1024"), "10240"),
            ScriptedResponse::status(204).delayed(Duration::from_millis(250)),
            channel(Some("3072"), "8192"),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec_a = executor(db.pool.clone(), &mock);
    let exec_b = executor(pool_b.clone(), &mock);
    let lock = request("moderation.lockdown", json!({}));
    exec_a
        .execute(&lock, &actor(), "cycle-one", "cycle-one-key", TIME)
        .await
        .unwrap();
    let original = db.store().get_lockdown(CHANNEL).await.unwrap().unwrap();
    let unlock_task = tokio::spawn(async move {
        exec_a
            .execute(
                &request("moderation.unlock", json!({})),
                &actor(),
                "unlock-one",
                "unlock-one-key",
                TIME,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if mock.requests().len() == 4 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        exec_b
            .execute(&lock, &actor(), "busy-cycle", "cycle-two-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::InProgress
    );
    assert_eq!(mock.requests().len(), 4);
    unlock_task.await.unwrap().unwrap();
    exec_b
        .execute(&lock, &actor(), "cycle-two", "cycle-two-key", TIME)
        .await
        .unwrap();
    let next = db.store().get_lockdown(CHANNEL).await.unwrap().unwrap();
    assert_ne!(original.recovery_generation, next.recovery_generation);
    assert_eq!(next.prior_allow, "3072");
    assert_eq!(next.prior_deny, "8192");
    assert!(!db
        .store()
        .clear_lockdown(CHANNEL, &original.recovery_generation)
        .await
        .unwrap());
    assert_eq!(db.store().get_lockdown(CHANNEL).await.unwrap(), Some(next));
    pool_b.close().await;
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn post_mutation_audit_failure_rolls_back_completion_and_retains_recovery() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            channel(Some("3072"), "8192"),
            ScriptedResponse::status(204),
            channel(Some("1024"), "10240"),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    exec.execute(
        &request("moderation.lockdown", json!({})),
        &actor(),
        "atomic-lock",
        "atomic-lock-key",
        TIME,
    )
    .await
    .unwrap();
    let original = db.store().get_lockdown(CHANNEL).await.unwrap().unwrap();
    // Test-owned schema only: force the *terminal audit* to fail after Discord
    // accepted restoration. All three ledger changes must roll back together.
    sqlx::query(
        "ALTER TABLE moderation_audit ADD CONSTRAINT reject_unlock CHECK (outcome <> 'unlocked')",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let unlock = request("moderation.unlock", json!({}));
    assert_eq!(
        exec.execute(
            &unlock,
            &actor(),
            "atomic-unlock",
            "atomic-unlock-key",
            TIME
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::Internal
    );
    assert_eq!(
        db.store().get_lockdown(CHANNEL).await.unwrap(),
        Some(original)
    );
    assert_eq!(
        exec.execute(&unlock, &actor(), "atomic-retry", "atomic-unlock-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::InProgress
    );
    assert_eq!(
        exec.execute(
            &request("moderation.lockdown", json!({})),
            &actor(),
            "unsafe-relock",
            "unsafe-relock-key",
            TIME
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InProgress
    );
    assert_eq!(mock.requests().len(), 4);
    let guards: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_channel_executions")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(guards, 1);
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn rejected_repeated_lockdown_preserves_original_seed_and_unlock_failure_keeps_it() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            channel(Some("3072"), "8192"),
            ScriptedResponse::status(204),
            channel(Some("1024"), "10240"),
            ScriptedResponse::status(403),
            channel(Some("1024"), "10240"),
            ScriptedResponse::status(503),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let lock = request("moderation.lockdown", json!({}));
    exec.execute(&lock, &actor(), "first-lock", "first-lock-key", TIME)
        .await
        .unwrap();
    let first = db.store().get_lockdown(CHANNEL).await.unwrap().unwrap();
    assert_eq!(
        exec.execute(
            &lock,
            &actor(),
            "rejected-repeat",
            "repeated-lock-key",
            TIME
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::DiscordRejected
    );
    assert_eq!(
        db.store().get_lockdown(CHANNEL).await.unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        exec.execute(
            &request("moderation.unlock", json!({})),
            &actor(),
            "failed-unlock",
            "failed-unlock-key",
            TIME
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::DiscordUnavailable
    );
    assert_eq!(db.store().get_lockdown(CHANNEL).await.unwrap(), Some(first));
    assert_eq!(
        exec.execute(&lock, &actor(), "unsafe-lock", "unsafe-lock-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::InProgress
    );
    assert_eq!(mock.requests().len(), 6);
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn stale_channel_ticket_and_recovery_generation_cannot_finish_current_owner() {
    use two_bot_core::channel_moderation_store::{ChannelAuditRow, ChannelClaim};
    let db = TestDb::new().await;
    let store = db.store();
    let key = "store-fence-key";
    let ChannelClaim::Claimed { ticket: first } = store
        .claim(GUILD, key, "moderation.unlock", "hash", TIME)
        .await
        .unwrap()
    else {
        panic!("claim")
    };
    assert!(store.release(&first).await.unwrap());
    let ChannelClaim::Claimed { ticket: second } = store
        .claim(GUILD, key, "moderation.unlock", "hash", TIME)
        .await
        .unwrap()
    else {
        panic!("claim")
    };
    assert!(!store.reserve_channel(&first, CHANNEL).await.unwrap());
    assert!(store.reserve_channel(&second, CHANNEL).await.unwrap());
    let record = store
        .record_lockdown(
            CHANNEL,
            GUILD,
            &two_bot_core::LockdownSeed {
                prior_allow: "3072".to_owned(),
                prior_deny: "8192".to_owned(),
                prior_exists: true,
            },
            "cleanup",
            TIME,
        )
        .await
        .unwrap();
    let audit = ChannelAuditRow {
        request_id: "fence-request".to_owned(),
        guild_id: GUILD.to_owned(),
        actor_id: ACTOR.to_owned(),
        action: "moderation.unlock".to_owned(),
        channel_id: Some(CHANNEL.to_owned()),
        reason: "cleanup".to_owned(),
        outcome: "unlocked".to_owned(),
        idempotency_key: key.to_owned(),
        metadata_json: "{}".to_owned(),
        created_at: TIME.to_owned(),
    };
    assert!(!store
        .finish_channel(&first, &audit, "{}", Some(&record.recovery_generation))
        .await
        .unwrap());
    assert!(!store
        .finish_channel(&second, &audit, "{}", Some("stale-generation"))
        .await
        .unwrap());
    assert_eq!(
        store.get_lockdown(CHANNEL).await.unwrap(),
        Some(record.clone())
    );
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_audit")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(audits, 0);
    assert!(store
        .finish_channel(&second, &audit, "{}", Some(&record.recovery_generation))
        .await
        .unwrap());
    assert!(!store
        .finish_channel(&second, &audit, "{}", None)
        .await
        .unwrap());
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn ambiguous_permission_responses_retain_request_channel_and_recovery_fences() {
    for (action, prior_allow) in [
        ("moderation.lockdown", Some("3072")),
        ("moderation.unlock", Some("3072")),
        ("moderation.unlock", None),
    ] {
        for status in [202, 302, 408, 409, 425, 429, 503] {
            let db = TestDb::new().await;
            let original = if action == "moderation.unlock" {
                Some(seed_lockdown(&db, prior_allow).await)
            } else {
                None
            };
            let mock = MockRest::start(
                vec![
                    channel(prior_allow, "8192"),
                    ScriptedResponse::status(status),
                ],
                ScriptedResponse::status(500),
            )
            .await;
            let exec = executor(db.pool.clone(), &mock);
            let req = request(action, json!({}));
            let failure = exec
                .execute(
                    &req,
                    &actor(),
                    "ambiguous-write",
                    "ambiguous-write-key",
                    TIME,
                )
                .await
                .unwrap_err();
            assert_eq!(
                failure.code,
                if status == 429 {
                    ErrorCode::RateLimited
                } else {
                    ErrorCode::DiscordUnavailable
                },
                "{action}: {status}"
            );
            let recovery = db.store().get_lockdown(CHANNEL).await.unwrap().unwrap();
            if let Some(original) = original {
                assert_eq!(recovery, original);
            } else {
                assert_eq!(recovery.prior_allow, "3072");
                assert_eq!(recovery.prior_deny, "8192");
            }
            assert_eq!(
                exec.execute(&req, &actor(), "same-key", "ambiguous-write-key", TIME)
                    .await
                    .unwrap_err()
                    .code,
                ErrorCode::InProgress
            );
            assert_eq!(
                exec.execute(
                    &request("moderation.lockdown", json!({})),
                    &actor(),
                    "distinct-key",
                    "distinct-write-key",
                    TIME,
                )
                .await
                .unwrap_err()
                .code,
                ErrorCode::InProgress
            );
            let wire = mock.requests();
            assert_eq!(wire.len(), 2);
            assert_eq!(wire[0].method, "GET");
            assert_eq!(
                wire[1].method,
                if action == "moderation.unlock" && prior_allow.is_none() {
                    "DELETE"
                } else {
                    "PUT"
                }
            );
            assert_reservations(&db, 1).await;
            mock.shutdown().await;
            db.cleanup().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn recovery_read_failures_release_both_fences_and_preserve_original_seed() {
    for (action, has_seed) in [
        ("moderation.lockdown", false),
        ("moderation.lockdown", true),
        ("moderation.unlock", true),
    ] {
        let db = TestDb::new().await;
        let original = if has_seed {
            Some(seed_lockdown(&db, Some("3072")).await)
        } else {
            None
        };
        // Only this test's unique schema is altered. The lookup fails while
        // claim/audit/abort remain writable; restoring the column ends the fault.
        sqlx::query("ALTER TABLE moderation_lockdowns RENAME COLUMN prior_allow TO hidden_mask")
            .execute(&db.pool)
            .await
            .unwrap();
        let mock = MockRest::start(
            vec![
                channel(Some("3072"), "8192"),
                channel(Some("3072"), "8192"),
                ScriptedResponse::status(204),
                channel(Some("3072"), "8192"),
                ScriptedResponse::json(200, json!({})),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        let exec = executor(db.pool.clone(), &mock);
        let req = request(action, json!({}));
        assert_eq!(
            exec.execute(&req, &actor(), "read-failure", "recovery-read-key", TIME)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Internal
        );
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(mock.requests()[0].method, "GET");
        assert_reservations(&db, 0).await;
        assert_audit(&db, action, "refused", "cleanup", None).await;
        sqlx::query("ALTER TABLE moderation_lockdowns RENAME COLUMN hidden_mask TO prior_allow")
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(db.store().get_lockdown(CHANNEL).await.unwrap(), original);
        assert!(
            !exec
                .execute(&req, &actor(), "read-retry", "recovery-read-key", TIME)
                .await
                .unwrap()
                .replayed
        );
        // The distinct key must acquire the channel too, not just replay a result.
        exec.execute(
            &request("moderation.slowmode", json!({"seconds":0})),
            &actor(),
            "after-read-retry",
            "after-read-retry-key",
            TIME,
        )
        .await
        .unwrap();
        assert_eq!(mock.requests().len(), 5);
        mock.shutdown().await;
        db.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn purge_history_failures_are_retryable_without_any_deletion() {
    for (failure, code) in [
        (ScriptedResponse::status(503), ErrorCode::DiscordUnavailable),
        (ScriptedResponse::status(429), ErrorCode::RateLimited),
        (ScriptedResponse::status(408), ErrorCode::DiscordUnavailable),
        (
            ScriptedResponse::json(200, json!([])).delayed(Duration::from_millis(
                two_bot_discord::MODERATION_TIMEOUT_MS + 250,
            )),
            ErrorCode::UpstreamTimeout,
        ),
    ] {
        let db = TestDb::new().await;
        let mock = MockRest::start(
            vec![
                channel(None, "0"),
                failure,
                channel(None, "0"),
                ScriptedResponse::json(
                    200,
                    json!([{"id":"555555555555555555"},{"id":"666666666666666666"}]),
                ),
                ScriptedResponse::status(204),
                channel(None, "0"),
                ScriptedResponse::json(200, json!({})),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        let exec = executor(db.pool.clone(), &mock);
        let req = request("moderation.purge", json!({"count":2}));
        assert_eq!(
            exec.execute(
                &req,
                &actor(),
                "history-failure",
                "history-failure-key",
                TIME
            )
            .await
            .unwrap_err()
            .code,
            code
        );
        assert_eq!(mock.requests().len(), 2);
        assert!(mock.requests().iter().all(|r| r.method == "GET"));
        assert_reservations(&db, 0).await;
        assert_audit(&db, "moderation.purge", "refused", "cleanup", None).await;
        let result = exec
            .execute(&req, &actor(), "history-retry", "history-failure-key", TIME)
            .await
            .unwrap();
        assert_eq!(result.affected, Some(2));
        assert!(!result.replayed);
        exec.execute(
            &request("moderation.slowmode", json!({"seconds":0})),
            &actor(),
            "after-purge",
            "after-purge-key",
            TIME,
        )
        .await
        .unwrap();
        assert_eq!(mock.requests().len(), 7);
        assert_eq!(mock.requests()[4].method, "POST");
        mock.shutdown().await;
        db.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn uncertain_purge_deletions_keep_same_key_and_distinct_key_fenced() {
    for count in [1, 2] {
        for status in [202, 302, 408, 429, 503] {
            let db = TestDb::new().await;
            let messages: Vec<Value> = ["555555555555555555", "666666666666666666"]
                .iter()
                .take(count)
                .map(|id| json!({"id":id}))
                .collect();
            let mock = MockRest::start(
                vec![
                    channel(None, "0"),
                    ScriptedResponse::json(200, json!(messages)),
                    ScriptedResponse::status(status),
                ],
                ScriptedResponse::status(500),
            )
            .await;
            let exec = executor(db.pool.clone(), &mock);
            let req = request("moderation.purge", json!({"count":count}));
            assert_eq!(
                exec.execute(
                    &req,
                    &actor(),
                    "deletion-failure",
                    "deletion-failure-key",
                    TIME
                )
                .await
                .unwrap_err()
                .code,
                if status == 429 {
                    ErrorCode::RateLimited
                } else {
                    ErrorCode::DiscordUnavailable
                }
            );
            assert_eq!(
                exec.execute(
                    &req,
                    &actor(),
                    "deletion-retry",
                    "deletion-failure-key",
                    TIME
                )
                .await
                .unwrap_err()
                .code,
                ErrorCode::InProgress
            );
            assert_eq!(
                exec.execute(
                    &request("moderation.slowmode", json!({"seconds":0})),
                    &actor(),
                    "unsafe-after-deletion",
                    "unsafe-after-deletion-key",
                    TIME,
                )
                .await
                .unwrap_err()
                .code,
                ErrorCode::InProgress
            );
            assert_eq!(mock.requests().len(), 3);
            assert_eq!(
                mock.requests()[2].method,
                if count == 1 { "DELETE" } else { "POST" }
            );
            assert_reservations(&db, 1).await;
            mock.shutdown().await;
            db.cleanup().await;
        }
    }
}

async fn seed_lockdown(db: &TestDb, prior_allow: Option<&str>) -> two_bot_core::LockdownRecord {
    db.store()
        .record_lockdown(
            CHANNEL,
            GUILD,
            &two_bot_core::LockdownSeed {
                prior_allow: prior_allow.unwrap_or("0").to_owned(),
                prior_deny: "8192".to_owned(),
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
fn decode_reason(header: &str) -> String {
    let mut bytes = Vec::new();
    let input = header.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            bytes.push(u8::from_str_radix(&header[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            bytes.push(input[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).unwrap()
}
fn verify_wire_reason(mock: &MockRest, action: &str, key: &str) -> String {
    let requests = mock.requests();
    let write = requests
        .iter()
        .find(|r| r.method != "GET")
        .expect("wire mutation");
    let reason = decode_reason(write.header("x-audit-log-reason").unwrap());
    assert!(reason.encode_utf16().count() <= 512);
    let marker = two_bot_core::mac::parse_moderation_audit_reason(
        Some(SECRET.as_str()),
        GUILD,
        Some(&reason),
    )
    .unwrap();
    assert_eq!(marker.actor_id, ACTOR);
    assert_eq!(marker.action, action);
    assert_eq!(
        marker.token,
        two_bot_core::mac::moderation_audit_token(GUILD, key)
    );
    reason
}

#[test]
fn request_validation_rejects_before_claim_or_wire() {
    for (action, extra) in [
        ("moderation.purge", json!({})),
        ("moderation.purge", json!({"count":0})),
        ("moderation.purge", json!({"count":101})),
        ("moderation.purge", json!({"count":"5"})),
        ("moderation.slowmode", json!({"seconds":21601})),
        ("moderation.lockdown", json!({"seconds":1.5})),
        ("moderation.unlock", json!({"duration_seconds":null})),
        (
            "moderation.lockdown",
            json!({"actor_id":"00000000000000000"}),
        ),
        (
            "moderation.lockdown",
            json!({"channel_id":"99999999999999999999"}),
        ),
        ("moderation.unlock", json!({"reason":"🦀".repeat(257)})),
        ("moderation.ban", json!({})),
    ] {
        let mut body = json!({"actor_id":ACTOR,"channel_id":CHANNEL,"reason":"cleanup"});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        assert!(
            InternalChannelRequest::from_body(action, body.as_object().unwrap()).is_err(),
            "{action}: {extra}"
        );
    }
    request("moderation.slowmode", json!({"seconds":0}));
    request("moderation.purge", json!({"count":100}));
}

const GUILD: &str = "111111111111111111";
const CHANNEL: &str = "222222222222222222";
const TIME: &str = "2026-09-30T00:00:00.000Z";

fn allowed_test_url(url: &str, ci: bool) -> bool {
    url == "postgres://agent_test@agent-testdb:5432/agent_test"
        || (ci && url == "postgres://agent_test@127.0.0.1:5432/agent_test")
}

#[test]
fn database_guard_refuses_credentials_and_non_test_targets() {
    assert!(allowed_test_url(
        "postgres://agent_test@agent-testdb:5432/agent_test",
        false
    ));
    assert!(allowed_test_url(
        "postgres://agent_test@127.0.0.1:5432/agent_test",
        true
    ));
    for url in [
        "postgres://agent_test@127.0.0.1:5432/agent_test",
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
    options: PgConnectOptions,
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
            "icm_{}_{}_{}",
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
            options,
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

    async fn independent_pool(&self) -> PgPool {
        Self::connect(&self.options, &self.schema).await
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

async fn assert_audit(
    db: &TestDb,
    action: &str,
    outcome: &str,
    reason: &str,
    affected: Option<u64>,
) {
    let row = sqlx::query("SELECT actor_id, target_id, channel_id, reason, outcome, metadata_json FROM moderation_audit WHERE action = $1")
        .bind(action).fetch_one(&db.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("actor_id"), ACTOR);
    assert_eq!(row.get::<Option<String>, _>("target_id"), None);
    assert_eq!(
        row.get::<Option<String>, _>("channel_id"),
        Some(CHANNEL.to_owned())
    );
    assert_eq!(row.get::<String, _>("reason"), reason);
    assert_eq!(row.get::<String, _>("outcome"), outcome);
    let meta: Value = serde_json::from_str(&row.get::<String, _>("metadata_json")).unwrap();
    assert_eq!(meta["affected"], json!(affected));
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn purge_counts_bulk_delete_and_replays_without_rest_or_duplicate_audit() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            channel(None, "0"),
            ScriptedResponse::json(
                200,
                json!([{"id":"555555555555555555"},{"id":"666666666666666666"}]),
            ),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let req = request("moderation.purge", json!({"count":2}));
    let key = "purge-test-key";
    let first = exec
        .execute(&req, &actor(), "purge-request", key, TIME)
        .await
        .unwrap();
    assert_eq!(
        first.response(),
        json!({"result":{"outcome":"purged","affected":2},"outcome":"purged"})
    );
    let second = exec
        .execute(&req, &actor(), "purge-retry", key, TIME)
        .await
        .unwrap();
    assert!(second.replayed);
    assert_eq!(second.affected, Some(2));
    let wire = mock.requests();
    assert_eq!(wire.len(), 3);
    assert_eq!(wire[2].method, "POST");
    assert!(wire[2].path.ends_with("/messages/bulk-delete"));
    verify_wire_reason(&mock, "moderation.purge", key);
    assert_audit(&db, "moderation.purge", "purged", "cleanup", Some(2)).await;
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_audit")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(audits, 1);
    let mismatch = request("moderation.purge", json!({"count":3}));
    assert_eq!(
        exec.execute(&mismatch, &actor(), "mismatch", key, TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Malformed
    );
    assert_eq!(mock.requests().len(), 3);
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn slowmode_zero_and_signed_maximum_unicode_reason_survive_replay() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            channel(Some("1024"), "8192"),
            ScriptedResponse::json(200, json!({})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let reason = "🦀".repeat(256);
    let key = "slowmode-test-key";
    let req = request("moderation.slowmode", json!({"seconds":0,"reason":reason}));
    let first = exec
        .execute(&req, &actor(), "slowmode-request", key, TIME)
        .await
        .unwrap();
    assert_eq!(first.outcome, "slowmode_updated");
    assert_eq!(first.affected, None);
    assert!(
        exec.execute(&req, &actor(), "slowmode-retry", key, TIME)
            .await
            .unwrap()
            .replayed
    );
    let wire = mock.requests();
    assert_eq!(wire.len(), 2);
    assert_eq!(wire[1].method, "PATCH");
    assert_eq!(
        serde_json::from_slice::<Value>(&wire[1].body).unwrap()["rate_limit_per_user"],
        0
    );
    let wire_reason = verify_wire_reason(&mock, "moderation.slowmode", key);
    assert!(wire_reason.ends_with('🦀'));
    assert_audit(
        &db,
        "moderation.slowmode",
        "slowmode_updated",
        &reason,
        None,
    )
    .await;
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn lockdown_and_unlock_restore_full_masks_and_replay_independently() {
    let db = TestDb::new().await;
    let allow = ((1u64 << 48) | 2048 | 1024).to_string();
    let deny = "8192";
    let mock = MockRest::start(
        vec![
            channel(Some(&allow), deny),
            ScriptedResponse::status(204),
            channel(Some(&((1u64 << 48) | 1024).to_string()), "10240"),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let lock = request("moderation.lockdown", json!({}));
    let unlock = request("moderation.unlock", json!({}));
    let lock_key = "lockdown-test-key";
    let unlock_key = "unlock-test-key";
    let locked = exec
        .execute(&lock, &actor(), "lock-request", lock_key, TIME)
        .await
        .unwrap();
    assert_eq!(locked.outcome, "locked_down");
    let record = db.store().get_lockdown(CHANNEL).await.unwrap().unwrap();
    assert_eq!(record.prior_allow, allow);
    assert_eq!(record.prior_deny, deny);
    assert!(
        exec.execute(&lock, &actor(), "lock-retry", lock_key, TIME)
            .await
            .unwrap()
            .replayed
    );
    verify_wire_reason(&mock, "moderation.lockdown", lock_key);
    let unlocked = exec
        .execute(&unlock, &actor(), "unlock-request", unlock_key, TIME)
        .await
        .unwrap();
    assert_eq!(unlocked.outcome, "unlocked");
    assert!(
        exec.execute(&unlock, &actor(), "unlock-retry", unlock_key, TIME)
            .await
            .unwrap()
            .replayed
    );
    assert!(db.store().get_lockdown(CHANNEL).await.unwrap().is_none());
    let wire = mock.requests();
    assert_eq!(wire.len(), 4);
    let written: Value = serde_json::from_slice(&wire[1].body).unwrap();
    assert_eq!(written["allow"], ((1u64 << 48) | 1024).to_string());
    assert_eq!(written["deny"], "10240");
    let restored: Value = serde_json::from_slice(&wire[3].body).unwrap();
    assert_eq!(restored["allow"], allow);
    assert_eq!(restored["deny"], deny);
    let reason = decode_reason(wire[3].header("x-audit-log-reason").unwrap());
    assert_eq!(
        two_bot_core::mac::parse_moderation_audit_reason(
            Some(SECRET.as_str()),
            GUILD,
            Some(&reason)
        )
        .unwrap()
        .action,
        "moderation.unlock"
    );
    assert_audit(&db, "moderation.lockdown", "locked_down", "cleanup", None).await;
    assert_audit(&db, "moderation.unlock", "unlocked", "cleanup", None).await;
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn missing_overwrite_is_deleted_on_unlock_and_untracked_unlock_refuses() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            channel(None, "0"),
            ScriptedResponse::status(204),
            channel(Some("0"), "2048"),
            ScriptedResponse::status(204),
            channel(None, "0"),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    exec.execute(
        &request("moderation.lockdown", json!({})),
        &actor(),
        "lock",
        "lock-absent-key",
        TIME,
    )
    .await
    .unwrap();
    assert!(
        !db.store()
            .get_lockdown(CHANNEL)
            .await
            .unwrap()
            .unwrap()
            .prior_exists
    );
    let unlock = request("moderation.unlock", json!({}));
    exec.execute(&unlock, &actor(), "unlock", "unlock-absent-key", TIME)
        .await
        .unwrap();
    let wire = mock.requests();
    assert_eq!(wire[3].method, "DELETE");
    assert!(wire[3].path.contains("/permissions/"));
    assert_eq!(
        exec.execute(&unlock, &actor(), "untracked", "untracked-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::ActionNotAllowed
    );
    assert_eq!(mock.requests().len(), 5);
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn concurrent_lock_and_unlock_are_ordered_across_independent_pools() {
    let db = TestDb::new().await;
    let pool_b = db.independent_pool().await;
    let mock = MockRest::start(
        vec![
            channel(Some("3072"), "8192"),
            ScriptedResponse::status(204).delayed(Duration::from_millis(250)),
            channel(Some("1024"), "10240"),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec_a = executor(db.pool.clone(), &mock);
    let exec_b = executor(pool_b.clone(), &mock);
    let lock = request("moderation.lockdown", json!({}));
    let unlock = request("moderation.unlock", json!({}));
    let lock_task = tokio::spawn(async move {
        exec_a
            .execute(&lock, &actor(), "ordered-lock", "ordered-lock-key", TIME)
            .await
    });
    // Bounded local mock gate, not CI polling: start unlock only after the PUT
    // is observed and held in the double, proving the overlap deterministically.
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
    assert_eq!(
        exec_b
            .execute(&unlock, &actor(), "busy-unlock", "ordered-unlock-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::InProgress
    );
    assert_eq!(mock.requests().len(), 2);
    lock_task.await.unwrap().unwrap();
    exec_b
        .execute(
            &unlock,
            &actor(),
            "ordered-unlock",
            "ordered-unlock-key",
            TIME,
        )
        .await
        .unwrap();
    assert_eq!(mock.requests().len(), 4);
    assert!(db.store().get_lockdown(CHANNEL).await.unwrap().is_none());
    let guards: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_channel_executions")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(guards, 0);
    pool_b.close().await;
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn uncertain_lockdown_keeps_seed_and_channel_fence_against_distinct_key_unlock() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![channel(Some("3072"), "8192"), ScriptedResponse::status(503)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let lock = request("moderation.lockdown", json!({}));
    assert_eq!(
        exec.execute(
            &lock,
            &actor(),
            "uncertain-lock",
            "uncertain-lock-key",
            TIME
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::DiscordUnavailable
    );
    assert!(db.store().get_lockdown(CHANNEL).await.unwrap().is_some());
    assert_eq!(
        exec.execute(&lock, &actor(), "retry", "uncertain-lock-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::InProgress
    );
    assert_eq!(
        exec.execute(
            &request("moderation.unlock", json!({})),
            &actor(),
            "unsafe-unlock",
            "distinct-unlock-key",
            TIME
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InProgress
    );
    assert_eq!(mock.requests().len(), 2);
    let guards: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_channel_executions")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(guards, 1);
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn rejected_lockdown_removes_only_new_seed_and_allows_same_key_retry() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            channel(Some("3072"), "8192"),
            ScriptedResponse::status(403),
            channel(Some("3072"), "8192"),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(db.pool.clone(), &mock);
    let req = request("moderation.lockdown", json!({}));
    let key = "rejected-lock-key";
    assert_eq!(
        exec.execute(&req, &actor(), "rejected", key, TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::DiscordRejected
    );
    assert!(db.store().get_lockdown(CHANNEL).await.unwrap().is_none());
    assert_eq!(
        exec.execute(&req, &actor(), "retry", key, TIME)
            .await
            .unwrap()
            .outcome,
        "locked_down"
    );
    assert_eq!(mock.requests().len(), 4);
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn permission_actor_and_guild_refusals_never_mutate() {
    let db = TestDb::new().await;
    let mock=MockRest::start(vec![ScriptedResponse::json(200,json!({"id":CHANNEL,"guild_id":"999999999999999999","type":0,"permission_overwrites":[]}))],ScriptedResponse::status(500)).await;
    let exec = executor(db.pool.clone(), &mock);
    let req = request("moderation.slowmode", json!({"seconds":30}));
    let mut denied = actor();
    denied.permissions = 0;
    assert_eq!(
        exec.execute(&req, &denied, "denied", "permission-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::ActionNotAllowed
    );
    denied = actor();
    denied.user_id = "777777777777777777".to_owned();
    assert_eq!(
        exec.execute(&req, &denied, "spoof", "spoof-actor-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::ActionNotAllowed
    );
    assert!(mock.requests().is_empty());
    assert_eq!(
        exec.execute(&req, &actor(), "foreign", "foreign-guild-key", TIME)
            .await
            .unwrap_err()
            .code,
        ErrorCode::DiscordRejected
    );
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(mock.requests()[0].method, "GET");
    let claims: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_idempotency")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(claims, 0);
    mock.shutdown().await;
    db.cleanup().await;
}
