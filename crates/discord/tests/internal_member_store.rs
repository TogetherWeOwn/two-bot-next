#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;
#[path = "../../core/tests/common/internal_testdb.rs"]
mod internal_testdb;

use common::{MockRest, ScriptedResponse};
use serde_json::json;
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::collections::HashMap;
use two_bot_core::internal_action_store::{
    InternalActionStore, ReconciliationEvidence, RequestIdentity, TerminalFailure, TerminalResponse,
};
use two_bot_core::internal_actions::ErrorCode;
use two_bot_discord::executor::member::{store::MemberActionConfig, MemberOutcome};
use two_bot_discord::ActionExecutor;

const GUILD: &str = "100000000000000001";
const USER: &str = "100000000000000002";
const BOT: &str = "100000000000000003";
const ROLE: &str = "100000000000000004";
const BOT_ROLE: &str = "100000000000000005";
const TOKEN: &str = "fixture-only-oauth-DO-NOT-PERSIST";

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    schema: String,
}
impl TestDb {
    async fn new() -> Self {
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test-container URL required");
        let options =
            internal_testdb::test_options(&url).expect("refusing non-test-container target");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .unwrap();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("ia10861_{}_{nanos}", std::process::id());
        assert!(
            schema.len() <= 63
                && schema
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.options([("search_path", &schema)]))
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0350_internal_actions.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        Self {
            admin,
            pool,
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
        .unwrap();
        self.admin.close().await;
    }
}
fn config(keys: &HashMap<String, String>) -> MemberActionConfig<'_> {
    MemberActionConfig {
        guild_id: GUILD,
        bot_user_id: BOT,
        role_keys: keys,
        allow_add_member: true,
    }
}
fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("fixture-bot-token".into(), Some(mock.origin())).unwrap()
}

#[tokio::test]
#[ignore = "requires agent-testdb; CI explicitly runs this suite"]
async fn replay_returns_recorded_member_outcome_without_another_rest_call() {
    let db = TestDb::new().await;
    let store = InternalActionStore::new(db.pool.clone());
    let keys = HashMap::from([("member".into(), ROLE.into())]);
    let config = config(&keys);
    for (index, status, expected) in [
        (0, 201, MemberOutcome::Added),
        (1, 204, MemberOutcome::AlreadyMember),
    ] {
        let mock = MockRest::start(
            vec![ScriptedResponse::status(status)],
            ScriptedResponse::status(500),
        )
        .await;
        let exec = executor(&mock);
        let payload =
            json!({"action":"guild.add_member","discord_id":USER,"access_token":TOKEN}).to_string();
        let key = format!("member-key-{index}");
        let first = exec
            .execute_stored_member(&store, "website", &key, payload.as_bytes(), &config)
            .await
            .unwrap();
        let replay = executor(&mock)
            .execute_stored_member(&store, "website", &key, payload.as_bytes(), &config)
            .await
            .unwrap();
        assert_eq!(first.outcome, expected);
        assert!(!first.replayed);
        assert_eq!(replay.outcome, first.outcome);
        assert!(replay.replayed);
        assert_eq!(
            first.outcome.success_body("req"),
            replay.outcome.success_body("req")
        );
        assert_eq!(mock.requests().len(), 1);
        let changed = payload.replace(USER, BOT);
        assert_eq!(
            exec.execute_stored_member(&store, "website", &key, changed.as_bytes(), &config)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Malformed
        );
        assert_eq!(mock.requests().len(), 1);
        mock.shutdown().await;
    }
    for (index, held, expected) in [
        (0, false, MemberOutcome::Assigned),
        (1, true, MemberOutcome::AlreadyHeld),
    ] {
        let mut script = vec![ScriptedResponse::json(
            200,
            json!({"roles": if held {vec![ROLE]} else {vec![]}}),
        )];
        if !held {
            script.extend([
                ScriptedResponse::json(200, json!({"roles":[BOT_ROLE]})),
                ScriptedResponse::json(
                    200,
                    json!([
                        {"id":GUILD,"position":0,"managed":false},
                        {"id":ROLE,"position":1,"managed":false},
                        {"id":BOT_ROLE,"position":10,"managed":true}
                    ]),
                ),
                ScriptedResponse::status(204),
            ]);
        }
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let exec = executor(&mock);
        let payload =
            json!({"action":"role.assign","discord_id":USER,"role_key":"member"}).to_string();
        let key = format!("role-key-{index}");
        let first = exec
            .execute_stored_member(&store, "website", &key, payload.as_bytes(), &config)
            .await
            .unwrap();
        let count = mock.requests().len();
        let replay = exec
            .execute_stored_member(&store, "website", &key, payload.as_bytes(), &config)
            .await
            .unwrap();
        assert_eq!(first.outcome, expected);
        assert!(!first.replayed);
        assert_eq!(replay.outcome, first.outcome);
        assert!(replay.replayed);
        assert_eq!(mock.requests().len(), count);
        mock.shutdown().await;
    }
    let persisted: Vec<String> = sqlx::query_scalar("SELECT row_to_json(i)::text FROM internal_idempotency i UNION ALL SELECT row_to_json(a)::text FROM internal_action_log a").fetch_all(&db.pool).await.unwrap();
    assert_eq!(persisted.len(), 12); // four intents, two audit rows each
    assert!(persisted.iter().all(|row| !row.contains(TOKEN)));
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb; CI explicitly runs this suite"]
async fn reconciled_terminal_failures_replay_without_another_rest_call() {
    let db = TestDb::new().await;
    let store = InternalActionStore::new(db.pool.clone());
    let keys = HashMap::from([("member".into(), ROLE.into())]);
    let config = config(&keys);
    for action in ["guild.add_member", "role.assign"] {
        let payload = if action == "guild.add_member" {
            json!({"action":action,"discord_id":USER,"access_token":TOKEN})
        } else {
            json!({"action":action,"discord_id":USER,"role_key":"member"})
        }
        .to_string();
        for (failure, code, retryable, reason) in [
            (
                TerminalFailure::Malformed,
                ErrorCode::Malformed,
                false,
                "malformed",
            ),
            (
                TerminalFailure::ActionNotAllowed,
                ErrorCode::ActionNotAllowed,
                false,
                "action_not_allowed",
            ),
            (
                TerminalFailure::DiscordRejected,
                ErrorCode::DiscordRejected,
                false,
                "discord_rejected",
            ),
            (
                TerminalFailure::NoEffect,
                ErrorCode::DiscordUnavailable,
                true,
                "no_effect",
            ),
        ] {
            let response = TerminalResponse::Failure(failure);
            let key = format!("{action}-{}", response.code());
            let mock = MockRest::start(vec![], ScriptedResponse::status(503)).await;
            let exec = executor(&mock);
            assert_eq!(
                exec.execute_stored_member(&store, "website", &key, payload.as_bytes(), &config)
                    .await
                    .unwrap_err()
                    .code,
                ErrorCode::DiscordUnavailable
            );
            let count = mock.requests().len();
            assert!(count > 0);
            let identity =
                RequestIdentity::new("website", &key, action, payload.as_bytes()).unwrap();
            store
                .reconcile(&identity, &response, ReconciliationEvidence::ProvenNotSent)
                .await
                .unwrap();
            for _ in 0..2 {
                let error = executor(&mock)
                    .execute_stored_member(&store, "website", &key, payload.as_bytes(), &config)
                    .await
                    .unwrap_err();
                assert_eq!(error.code, code);
                assert_eq!(error.status(), response.status());
                assert_eq!(error.code.retryable(), retryable);
                assert_eq!(error.log_reason, reason);
                assert_eq!(error.retry_after_secs, None);
                assert!(!format!("{error:?}").contains(TOKEN));
                assert_eq!(mock.requests().len(), count);
            }
            mock.shutdown().await;
        }
    }
    let completed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM internal_idempotency WHERE state = 'completed'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(completed, 8);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb; CI explicitly runs this suite"]
async fn redirects_never_persist_success_or_forward_credentials() {
    let db = TestDb::new().await;
    let store = InternalActionStore::new(db.pool.clone());
    let keys = HashMap::from([("member".into(), ROLE.into())]);
    let config = config(&keys);
    let target = MockRest::start(vec![], ScriptedResponse::status(201)).await;
    for action in ["guild.add_member", "role.assign"] {
        let payload = if action == "guild.add_member" {
            json!({"action":action,"discord_id":USER,"access_token":TOKEN})
        } else {
            json!({"action":action,"discord_id":USER,"role_key":"member"})
        }
        .to_string();
        for status in [301, 302, 307, 308] {
            let mut script = if action == "role.assign" {
                vec![
                    ScriptedResponse::json(200, json!({"roles":[]})),
                    ScriptedResponse::json(200, json!({"roles":[BOT_ROLE]})),
                    ScriptedResponse::json(
                        200,
                        json!([
                            {"id":GUILD,"position":0,"managed":false},
                            {"id":ROLE,"position":1,"managed":false},
                            {"id":BOT_ROLE,"position":10,"managed":true}
                        ]),
                    ),
                ]
            } else {
                vec![]
            };
            let mut redirect = ScriptedResponse::status(status);
            redirect.headers.push(("location".into(), target.origin()));
            script.push(redirect);
            let mock = MockRest::start(script, ScriptedResponse::status(201)).await;
            let key = format!("{action}-redirect-{status}");
            let error = executor(&mock)
                .execute_stored_member(&store, "website", &key, payload.as_bytes(), &config)
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::DiscordUnavailable);
            assert_eq!(error.log_reason, "discord_unexpected_status");
            assert_eq!(
                executor(&mock)
                    .execute_stored_member(&store, "website", &key, payload.as_bytes(), &config)
                    .await
                    .unwrap_err()
                    .code,
                ErrorCode::InProgress
            );
            assert_eq!(
                mock.requests().len(),
                if action == "role.assign" { 4 } else { 1 }
            );
            assert!(target.requests().is_empty());
            mock.shutdown().await;
        }
    }
    let states: Vec<(String, Option<String>, Option<i32>)> =
        sqlx::query_as("SELECT state, response_code, http_status FROM internal_idempotency")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(states.len(), 8);
    assert!(states
        .iter()
        .all(|(state, code, status)| state == "unknown" && code.is_none() && status.is_none()));
    let terminal_audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM internal_action_log WHERE phase = 'terminal'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(terminal_audits, 0);
    target.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb; CI explicitly runs this suite"]
async fn uncertain_response_retains_fence_and_disabled_action_never_sends() {
    let db = TestDb::new().await;
    let store = InternalActionStore::new(db.pool.clone());
    let keys = HashMap::new();
    let mut config = config(&keys);
    let payload =
        json!({"action":"guild.add_member","discord_id":USER,"access_token":TOKEN}).to_string();
    let mock = MockRest::start(
        vec![ScriptedResponse::rate_limited(0.5, "1")],
        ScriptedResponse::status(201),
    )
    .await;
    let exec = executor(&mock);
    config.allow_add_member = false;
    assert_eq!(
        exec.execute_stored_member(
            &store,
            "website",
            "disabled-key",
            payload.as_bytes(),
            &config
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::ActionNotAllowed
    );
    assert!(mock.requests().is_empty());
    config.allow_add_member = true;
    assert_eq!(
        exec.execute_stored_member(
            &store,
            "website",
            "unknown-key",
            payload.as_bytes(),
            &config
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::RateLimited
    );
    assert_eq!(
        exec.execute_stored_member(
            &store,
            "website",
            "unknown-key",
            payload.as_bytes(),
            &config
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InProgress
    );
    assert_eq!(mock.requests().len(), 1);
    let state: String = sqlx::query_scalar("SELECT state FROM internal_idempotency")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(state, "unknown");
    mock.shutdown().await;
    db.cleanup().await;
}
