#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::json;
use sqlx::PgPool;
use std::collections::HashMap;
use two_bot_core::internal_action_store::{
    InternalActionStore, ReconciliationEvidence, RequestIdentity, TerminalFailure, TerminalResponse,
};
use two_bot_core::internal_actions::ErrorCode;
use two_bot_discord::executor::member::{store::MemberActionConfig, MemberOutcome};
use two_bot_discord::ActionExecutor;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "100000000000000001";
const USER: &str = "100000000000000002";
const BOT: &str = "100000000000000003";
const ROLE: &str = "100000000000000004";
const BOT_ROLE: &str = "100000000000000005";
const TOKEN: &str = "fixture-only-oauth-DO-NOT-PERSIST";

struct TestDb {
    fixture: TestDatabase,
    pool: PgPool,
}
impl TestDb {
    async fn new() -> Self {
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test-container URL required");
        let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap();
        let pool = fixture.pool().clone();
        Self { fixture, pool }
    }
    async fn cleanup(self) {
        self.fixture.close().await.unwrap();
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
async fn uncertain_role_intent_retains_resolved_role_after_configuration_remapping() {
    let db = TestDb::new().await;
    let store = InternalActionStore::new(db.pool.clone());
    let mut keys = HashMap::from([("member".into(), ROLE.into())]);
    let payload = json!({"action":"role.assign","discord_id":USER,"role_key":"member"}).to_string();
    let mock = MockRest::start(
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
            ScriptedResponse::status(503),
        ],
        ScriptedResponse::status(204),
    )
    .await;
    let exec = executor(&mock);
    assert_eq!(
        exec.execute_stored_member(
            &store,
            "website",
            "resolved-role-key",
            payload.as_bytes(),
            &config(&keys)
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::DiscordUnavailable
    );
    let calls = mock.requests();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[3].method, "PUT");
    assert_eq!(
        calls[3].path,
        format!("/api/v10/guilds/{GUILD}/members/{USER}/roles/{ROLE}")
    );
    let intent: (String, String, String, String) = sqlx::query_as(
        "SELECT state, guild_id, target_id, resolved_role_id FROM internal_idempotency",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        intent,
        ("unknown".into(), GUILD.into(), USER.into(), ROLE.into())
    );
    let audits: Vec<(String, String)> =
        sqlx::query_as("SELECT phase, resolved_role_id FROM internal_action_log ORDER BY audit_id")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        audits,
        vec![
            ("intent".into(), ROLE.into()),
            ("unknown".into(), ROLE.into())
        ]
    );

    keys.insert("member".into(), "100000000000000006".into());
    for _ in 0..2 {
        assert_eq!(
            executor(&mock)
                .execute_stored_member(
                    &store,
                    "website",
                    "resolved-role-key",
                    payload.as_bytes(),
                    &config(&keys)
                )
                .await
                .unwrap_err()
                .code,
            ErrorCode::InProgress
        );
        assert_eq!(mock.requests().len(), 4);
    }
    let identity = RequestIdentity::new(
        "website",
        "resolved-role-key",
        "role.assign",
        payload.as_bytes(),
    )
    .unwrap();
    store
        .reconcile(
            &identity,
            &TerminalResponse::Success {
                resource_id: None,
                affected: 1,
            },
            ReconciliationEvidence::DiscordConfirmedEffect,
        )
        .await
        .unwrap();
    let replay = executor(&mock)
        .execute_stored_member(
            &store,
            "website",
            "resolved-role-key",
            payload.as_bytes(),
            &config(&keys),
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.outcome, MemberOutcome::Assigned);
    assert_eq!(mock.requests().len(), 4);
    let roles: Vec<String> = sqlx::query_scalar(
        "SELECT resolved_role_id FROM internal_idempotency \
         UNION ALL SELECT resolved_role_id FROM internal_action_log",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(roles, vec![ROLE.to_owned(); 4]);
    mock.shutdown().await;
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

#[tokio::test]
#[ignore = "requires agent-testdb; CI explicitly runs this suite"]
async fn repeated_json_key_refuses_before_claim_or_rest() {
    let db = TestDb::new().await;
    let store = InternalActionStore::new(db.pool.clone());
    let keys = HashMap::from([("member".into(), ROLE.into())]);
    let config = config(&keys);
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!({"roles":[ROLE]}))],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(&mock);
    // serde_json alone keeps the last `discord_id`; the store must not act on
    // a body whose readers can disagree about the target.
    let repeated = format!(
        r#"{{"action":"role.assign","discord_id":"{USER}","discord_id":"{BOT}","role_key":"member"}}"#
    );
    let err = exec
        .execute_stored_member(&store, "website", "dup-key", repeated.as_bytes(), &config)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Malformed);
    assert_eq!(err.log_reason, "duplicate_json_key");
    assert!(mock.requests().is_empty());
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_idempotency")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    // No claim was taken, so the same key still runs a well-formed body fresh.
    let payload = json!({"action":"role.assign","discord_id":USER,"role_key":"member"}).to_string();
    let first = exec
        .execute_stored_member(&store, "website", "dup-key", payload.as_bytes(), &config)
        .await
        .unwrap();
    assert_eq!(first.outcome, MemberOutcome::AlreadyHeld);
    assert!(!first.replayed);
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
    db.cleanup().await;
}
