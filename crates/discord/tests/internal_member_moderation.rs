//! Website member moderation uses only local mock REST and, for the
//! acceptance lane, the explicitly guarded disposable test database.
//! Ledger rows must match the slash-command path exactly; only the Discord
//! wire reason carries the MAC marker.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::LazyLock;
use two_bot_core::commands::{PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MODERATE_MEMBERS};
use two_bot_core::funnel::format_iso_millis;
use two_bot_core::internal_actions::ErrorCode;
use two_bot_core::mac::{moderation_audit_token, parse_moderation_audit_reason};
use two_bot_core::member_moderation::{
    DiscordCall, MemMemberStore, MemberExecution, MemberModerationService, MockMemberDiscord,
};
use two_bot_core::member_moderation_store::PgMemberModerationStore;
use two_bot_core::{ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget};
use two_bot_discord::internal_member_moderation::{
    InternalMemberConfig, InternalMemberExecutor, InternalMemberRequest,
};
use two_bot_discord::ActionExecutor;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "100000000000000001";
const ACTOR: &str = "111111111111111111";
const TARGET: &str = "333333333333333333";
const STAFF_ROLE: &str = "444444444444444444";
const OWEN_ID: &str = "123456789012345678";
// Fixed clock: 2023-11-14T22:13:20.000Z, so expiries assert exactly.
const NOW_MS: i64 = 1_700_000_000_000;
const NOW_ISO: &str = "2023-11-14T22:13:20.000Z";

// Signing and verification share an ephemeral key, never a stored credential.
static SECRET: LazyLock<String> = LazyLock::new(|| {
    let mut bytes = [0u8; 32];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut bytes)
        .expect("OS randomness for the test audit key");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
});

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: OWEN_ID.to_owned(),
        protected_role_ids: HashSet::from([STAFF_ROLE.to_owned()]),
        bot_user_id: None,
    }
}

fn actor() -> ModerationActor {
    ModerationActor {
        user_id: ACTOR.to_owned(),
        role_ids: Vec::new(),
        highest_role_position: 50,
        permissions: PERM_BAN_MEMBERS | PERM_KICK_MEMBERS | PERM_MODERATE_MEMBERS,
    }
}

fn target() -> ModerationTarget {
    ModerationTarget {
        user_id: TARGET.to_owned(),
        role_ids: Vec::new(),
        highest_role_position: 10,
        is_bot: false,
        is_guild_owner: false,
    }
}

fn request(action: &str, extra: Value) -> InternalMemberRequest {
    let mut body = json!({
        "actor_id": ACTOR,
        "discord_id": TARGET,
        "reason": "  spam  ",
    });
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    InternalMemberRequest::from_body(action, body.as_object().unwrap()).unwrap()
}

fn executor<S>(store: S, mock: &MockRest) -> InternalMemberExecutor<S>
where
    S: two_bot_core::member_moderation::MemberModerationStore + Clone,
{
    let _ = rustls::crypto::ring::default_provider().install_default();
    InternalMemberExecutor::new(
        store,
        ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap(),
        InternalMemberConfig {
            guild_id: GUILD.to_owned(),
            enabled: true,
            policy: policy(),
            audit_secret: Some(SECRET.clone()),
        },
    )
    .unwrap()
}

/// Scripted Discord success for one website action.
fn success_script(action: &str) -> Vec<ScriptedResponse> {
    match action {
        "moderation.timeout" => vec![ScriptedResponse::json(200, json!({"id": TARGET}))],
        _ => vec![ScriptedResponse::status(204)],
    }
}

fn expected_outcome(action: &str) -> &'static str {
    match action {
        "moderation.ban" => "banned",
        "moderation.tempban" => "temporarily_banned",
        "moderation.kick" => "kicked",
        "moderation.timeout" => "timed_out",
        "moderation.warn" => "warned",
        _ => unreachable!("member verb"),
    }
}

fn action_extra(action: &str) -> Value {
    match action {
        "moderation.tempban" | "moderation.timeout" => json!({"duration_seconds": 3600}),
        _ => json!({}),
    }
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

/// The one wire mutation carries the MAC marker binding guild, key, action
/// and actor; the stored ledger row keeps the plain human reason.
fn verify_wire_reason(mock: &MockRest, action: &str, key: &str) -> String {
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "exactly one Discord call");
    let reason = decode_reason(requests[0].header("x-audit-log-reason").unwrap());
    assert!(reason.encode_utf16().count() <= 512);
    let marker =
        parse_moderation_audit_reason(Some(SECRET.as_str()), GUILD, Some(&reason)).unwrap();
    assert_eq!(marker.actor_id, ACTOR);
    assert_eq!(marker.action, action);
    assert_eq!(marker.token, moderation_audit_token(GUILD, key));
    assert!(reason.ends_with("spam"));
    reason
}

#[tokio::test]
async fn all_five_verbs_execute_with_signed_wire_reason_and_plain_ledger() {
    for action in [
        "moderation.ban",
        "moderation.tempban",
        "moderation.kick",
        "moderation.timeout",
        "moderation.warn",
    ] {
        let mock = MockRest::start(success_script(action), ScriptedResponse::status(500)).await;
        let store = MemMemberStore::new();
        let exec = executor(store.clone(), &mock);
        let key = format!("key-{action}");
        let result = exec
            .execute(
                &request(action, action_extra(action)),
                &actor(),
                &target(),
                Some(100),
                &format!("req-{action}"),
                &key,
                NOW_MS,
            )
            .await
            .unwrap();
        assert_eq!(result.outcome, expected_outcome(action));
        assert!(!result.replayed);
        if action == "moderation.warn" {
            assert!(mock.requests().is_empty(), "warn never touches Discord");
            assert_eq!(store.warnings().len(), 1);
        } else {
            verify_wire_reason(&mock, action, &key);
        }
        let audits = store.audits();
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].reason, "spam");
        assert_eq!(audits[0].action, action);
        assert_eq!(audits[0].actor_id, ACTOR);
        assert_eq!(audits[0].outcome, expected_outcome(action));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn wire_paths_match_discord_routes() {
    for (action, method, path) in [
        (
            "moderation.ban",
            "PUT",
            format!("/api/v10/guilds/{GUILD}/bans/{TARGET}"),
        ),
        (
            "moderation.tempban",
            "PUT",
            format!("/api/v10/guilds/{GUILD}/bans/{TARGET}"),
        ),
        (
            "moderation.kick",
            "DELETE",
            format!("/api/v10/guilds/{GUILD}/members/{TARGET}"),
        ),
        (
            "moderation.timeout",
            "PATCH",
            format!("/api/v10/guilds/{GUILD}/members/{TARGET}"),
        ),
    ] {
        let mock = MockRest::start(success_script(action), ScriptedResponse::status(500)).await;
        let store = MemMemberStore::new();
        executor(store, &mock)
            .execute(
                &request(action, action_extra(action)),
                &actor(),
                &target(),
                Some(100),
                &format!("req-route-{action}"),
                &format!("key-route-{action}"),
                NOW_MS,
            )
            .await
            .unwrap();
        let calls = mock.requests();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, method);
        assert_eq!(calls[0].path, path);
        if action == "moderation.timeout" {
            let body: Value = serde_json::from_slice(&calls[0].body).unwrap();
            let until = body
                .get("communication_disabled_until")
                .and_then(Value::as_str)
                .expect("timeout PATCH carries communication_disabled_until");
            // The service emits `format_iso_millis` (`...20.000Z`); the
            // executor normalizes to the Twilight-parseable `+00:00` form
            // before the wire, so assert the date prefix and offset suffix.
            assert!(
                until.starts_with("2023-11-14T23:13:20"),
                "timeout expiry matches NOW_MS + 3600s, got {until}"
            );
            assert!(
                until.ends_with("+00:00"),
                "until uses the parser-accepted offset form, got {until}"
            );
        }
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn hierarchy_refusal_makes_no_claim_or_wire_call() {
    // Queue one ban success: the refusal makes no wire call, so the same-key
    // retry against the lower target consumes the single queued 204.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let store = MemMemberStore::new();
    let exec = executor(store.clone(), &mock);
    let mut high = target();
    high.highest_role_position = 60;
    let err = exec
        .execute(
            &request("moderation.ban", json!({})),
            &actor(),
            &high,
            Some(100),
            "req-hierarchy",
            "key-hierarchy",
            NOW_MS,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ActionNotAllowed);
    assert!(mock.requests().is_empty());
    assert!(store.audits().is_empty());
    // Nothing was claimed: the same key succeeds once the target is lower.
    let result = exec
        .execute(
            &request("moderation.ban", json!({})),
            &actor(),
            &target(),
            Some(100),
            "req-hierarchy",
            "key-hierarchy",
            NOW_MS,
        )
        .await
        .unwrap();
    assert_eq!(result.outcome, "banned");
    assert!(!result.replayed);
    mock.shutdown().await;
}

#[tokio::test]
async fn protected_role_refusal_makes_no_claim_or_wire_call() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let store = MemMemberStore::new();
    let exec = executor(store.clone(), &mock);
    let mut staff = target();
    staff.role_ids = vec![STAFF_ROLE.to_owned()];
    let err = exec
        .execute(
            &request("moderation.kick", json!({})),
            &actor(),
            &staff,
            Some(100),
            "req-protected",
            "key-protected",
            NOW_MS,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ActionNotAllowed);
    assert!(mock.requests().is_empty());
    assert!(store.audits().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn tempban_schedules_unban_and_sweep_dispatches_it() {
    let mock = MockRest::start(
        success_script("moderation.tempban"),
        ScriptedResponse::status(500),
    )
    .await;
    let store = MemMemberStore::new();
    executor(store.clone(), &mock)
        .execute(
            &request("moderation.tempban", json!({"duration_seconds": 3600})),
            &actor(),
            &target(),
            Some(100),
            "req-tempban",
            "key-tempban",
            NOW_MS,
        )
        .await
        .unwrap();
    assert_eq!(store.unban_state("req-tempban").as_deref(), Some("pending"));
    // The queued expiry is claimable by the sweep once due; dispatch needs no
    // REST double here because the sweep path is service-owned.
    let discord = MockMemberDiscord::new();
    let sweep = MemberModerationService::new(discord.clone(), store.clone(), policy(), move || {
        NOW_MS + 3_600_000 + 1_000
    });
    assert_eq!(sweep.run_due_unbans(GUILD).await.unwrap(), 1);
    assert!(matches!(
        discord.calls().as_slice(),
        [DiscordCall::Unban { .. }]
    ));
    assert_eq!(store.unban_state("req-tempban").as_deref(), Some("done"));
    mock.shutdown().await;
}

#[tokio::test]
async fn same_key_replays_stored_outcome_without_second_wire_call() {
    let mock = MockRest::start(
        success_script("moderation.ban"),
        ScriptedResponse::status(500),
    )
    .await;
    let store = MemMemberStore::new();
    let exec = executor(store.clone(), &mock);
    let first = exec
        .execute(
            &request("moderation.ban", json!({})),
            &actor(),
            &target(),
            Some(100),
            "req-replay",
            "key-replay",
            NOW_MS,
        )
        .await
        .unwrap();
    assert!(!first.replayed);
    let second = exec
        .execute(
            &request("moderation.ban", json!({})),
            &actor(),
            &target(),
            Some(100),
            "req-replay",
            "key-replay",
            NOW_MS,
        )
        .await
        .unwrap();
    assert_eq!(second.outcome, "banned");
    assert!(second.replayed);
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(store.audits().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn key_reuse_with_different_content_is_mismatch() {
    let mock = MockRest::start(
        success_script("moderation.ban"),
        ScriptedResponse::status(500),
    )
    .await;
    let store = MemMemberStore::new();
    let exec = executor(store.clone(), &mock);
    exec.execute(
        &request("moderation.ban", json!({})),
        &actor(),
        &target(),
        Some(100),
        "req-mismatch",
        "key-mismatch",
        NOW_MS,
    )
    .await
    .unwrap();
    let mut body = json!({
        "actor_id": ACTOR,
        "discord_id": TARGET,
        "reason": "harassment",
    });
    for (key, value) in action_extra("moderation.ban").as_object().unwrap() {
        body[key] = value.clone();
    }
    let err = exec
        .execute(
            &InternalMemberRequest::from_body("moderation.ban", body.as_object().unwrap()).unwrap(),
            &actor(),
            &target(),
            Some(100),
            "req-mismatch-other",
            "key-mismatch",
            NOW_MS,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Malformed);
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn ledger_rows_match_slash_path_exactly() {
    // Slash path: the service driven directly with a plain Discord double.
    let slash_store = MemMemberStore::new();
    let slash_discord = MockMemberDiscord::new();
    let slash =
        MemberModerationService::new(slash_discord.clone(), slash_store.clone(), policy(), || {
            NOW_MS
        });
    let execution = MemberExecution {
        action: ModerationAction::Ban,
        guild_id: GUILD.to_owned(),
        actor: actor(),
        target: Some(target()),
        bot_highest_role_position: Some(100),
        reason: "spam".to_owned(),
        duration_seconds: None,
        request_id: "req-parity".to_owned(),
        idempotency_key: "key-parity".to_owned(),
    };
    let slash_result = slash.execute(&execution).await.unwrap();

    // Website path: the same logical request through the internal executor.
    let mock = MockRest::start(
        success_script("moderation.ban"),
        ScriptedResponse::status(500),
    )
    .await;
    let web_store = MemMemberStore::new();
    let web_result = executor(web_store.clone(), &mock)
        .execute(
            &request("moderation.ban", json!({})),
            &actor(),
            &target(),
            Some(100),
            "req-parity",
            "key-parity",
            NOW_MS,
        )
        .await
        .unwrap();

    assert_eq!(slash_result.outcome.as_str(), web_result.outcome);
    assert_eq!(slash_store.audits(), web_store.audits());
    // The one intentional difference: the website wire reason carries the MAC
    // marker while the ledger keeps the plain human reason on both paths.
    let slash_calls = slash_discord.calls();
    let DiscordCall::Ban { reason, .. } = &slash_calls[0] else {
        panic!("slash ban call");
    };
    assert_eq!(reason, "spam");
    verify_wire_reason(&mock, "moderation.ban", "key-parity");
    mock.shutdown().await;
}

#[tokio::test]
async fn disabled_config_or_unresolved_identity_refuses_before_store() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let closed = InternalMemberExecutor::new(
        MemMemberStore::new(),
        ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap(),
        InternalMemberConfig {
            guild_id: GUILD.to_owned(),
            enabled: false,
            policy: policy(),
            audit_secret: Some(SECRET.clone()),
        },
    )
    .unwrap();
    let err = closed
        .execute(
            &request("moderation.ban", json!({})),
            &actor(),
            &target(),
            Some(100),
            "req-closed",
            "key-closed",
            NOW_MS,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ActionNotAllowed);

    let store = MemMemberStore::new();
    let exec = executor(store.clone(), &mock);
    let mut stranger = actor();
    stranger.user_id = "999999999999999999".to_owned();
    let err = exec
        .execute(
            &request("moderation.ban", json!({})),
            &stranger,
            &target(),
            Some(100),
            "req-stranger",
            "key-stranger",
            NOW_MS,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ActionNotAllowed);
    let err = exec
        .execute(
            &request("moderation.ban", json!({})),
            &actor(),
            &target(),
            Some(100),
            "req-bad-key",
            "short",
            NOW_MS,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Malformed);
    assert!(mock.requests().is_empty());
    assert!(store.audits().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn discord_rejection_releases_claim_for_retry() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403), ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let store = MemMemberStore::new();
    let exec = executor(store.clone(), &mock);
    let err = exec
        .execute(
            &request("moderation.ban", json!({})),
            &actor(),
            &target(),
            Some(100),
            "req-rejected",
            "key-rejected",
            NOW_MS,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::DiscordRejected);
    // A proven pre-mutation failure is a real second attempt, not a replay.
    let retry = exec
        .execute(
            &request("moderation.ban", json!({})),
            &actor(),
            &target(),
            Some(100),
            "req-rejected",
            "key-rejected",
            NOW_MS,
        )
        .await
        .unwrap();
    assert_eq!(retry.outcome, "banned");
    assert!(!retry.replayed);
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn uncertain_timeout_keeps_fence() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(500)],
        ScriptedResponse::status(500),
    )
    .await;
    let store = MemMemberStore::new();
    let exec = executor(store.clone(), &mock);
    let err = exec
        .execute(
            &request("moderation.timeout", json!({"duration_seconds": 3600})),
            &actor(),
            &target(),
            Some(100),
            "req-uncertain",
            "key-uncertain",
            NOW_MS,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::DiscordUnavailable);
    // The uncertain effect stays fenced: same-key retry refuses, never a
    // second PATCH, and no completion was recorded.
    let err = exec
        .execute(
            &request("moderation.timeout", json!({"duration_seconds": 3600})),
            &actor(),
            &target(),
            Some(100),
            "req-uncertain",
            "key-uncertain",
            NOW_MS,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InProgress);
    assert_eq!(mock.requests().len(), 1);
    assert!(store.audits().is_empty());
    mock.shutdown().await;
}

// --- disposable-DB acceptance -------------------------------------------------

struct TestDb {
    fixture: TestDatabase,
}

impl TestDb {
    async fn new() -> Self {
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test-container URL required");
        let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap();
        Self { fixture }
    }

    async fn cleanup(self) {
        self.fixture.close().await.unwrap();
    }
}

fn pg_executor(db: &TestDb, mock: &MockRest) -> InternalMemberExecutor<PgMemberModerationStore> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    InternalMemberExecutor::new(
        PgMemberModerationStore::new(db.fixture.pool().clone(), GUILD),
        ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap(),
        InternalMemberConfig {
            guild_id: GUILD.to_owned(),
            enabled: true,
            policy: policy(),
            audit_secret: Some(SECRET.clone()),
        },
    )
    .unwrap()
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn postgres_all_five_verbs_write_ledger_and_replay() {
    let db = TestDb::new().await;
    // One target per verb: bans fence later mutations on the same member.
    let targets = [
        "333333333333333331",
        "333333333333333332",
        "333333333333333333",
        "333333333333333334",
        "333333333333333335",
    ];
    let verbs = [
        "moderation.ban",
        "moderation.tempban",
        "moderation.kick",
        "moderation.timeout",
        "moderation.warn",
    ];
    for (verb, target_id) in verbs.iter().zip(targets.iter()) {
        let mock = MockRest::start(
            if *verb == "moderation.timeout" {
                vec![ScriptedResponse::json(200, json!({"id": target_id}))]
            } else {
                vec![ScriptedResponse::status(204)]
            },
            ScriptedResponse::status(500),
        )
        .await;
        let exec = pg_executor(&db, &mock);
        let mut body = json!({
            "actor_id": ACTOR,
            "discord_id": target_id,
            "reason": "  spam  ",
        });
        for (key, value) in action_extra(verb).as_object().unwrap() {
            body[key] = value.clone();
        }
        let parsed = InternalMemberRequest::from_body(verb, body.as_object().unwrap()).unwrap();
        let target_member = ModerationTarget {
            user_id: (*target_id).to_owned(),
            ..target()
        };
        let key = format!("pg-key-{verb}");
        let req = format!("pg-req-{verb}");
        let result = exec
            .execute(
                &parsed,
                &actor(),
                &target_member,
                Some(100),
                &req,
                &key,
                NOW_MS,
            )
            .await
            .unwrap();
        assert_eq!(result.outcome, expected_outcome(verb));
        assert!(!result.replayed);
        let row: (String, String, String) = sqlx::query_as(
            "SELECT action, outcome, reason FROM moderation_audit WHERE request_id = $1",
        )
        .bind(&req)
        .fetch_one(db.fixture.pool())
        .await
        .unwrap();
        assert_eq!(
            (row.0.as_str(), row.1.as_str(), row.2.as_str()),
            (*verb, expected_outcome(verb), "spam")
        );
        // Same-key replay returns the stored outcome without a second mutation.
        let replay = exec
            .execute(
                &parsed,
                &actor(),
                &target_member,
                Some(100),
                &req,
                &key,
                NOW_MS,
            )
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.outcome, expected_outcome(verb));
        if *verb == "moderation.warn" {
            assert!(mock.requests().is_empty());
        } else {
            assert_eq!(mock.requests().len(), 1);
        }
        mock.shutdown().await;
    }
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn postgres_tempban_schedules_pending_unban_with_exact_expiry() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = pg_executor(&db, &mock);
    exec.execute(
        &request("moderation.tempban", json!({"duration_seconds": 3600})),
        &actor(),
        &target(),
        Some(100),
        "pg-tempban",
        "pg-tempban-key",
        NOW_MS,
    )
    .await
    .unwrap();
    let row: (String, bool) = sqlx::query_as(
        "SELECT state, execute_at = $2::text::timestamptz FROM moderation_scheduled_unbans WHERE request_id = $1",
    )
    .bind("pg-tempban")
    .bind(format_iso_millis(NOW_MS + 3_600_000))
    .fetch_one(db.fixture.pool())
    .await
    .unwrap();
    assert_eq!(row, ("pending".to_owned(), true));
    assert_eq!(NOW_ISO, format_iso_millis(NOW_MS));
    // The sweep owns dispatch: once due, the queued expiry completes.
    let discord = MockMemberDiscord::new();
    let sweep = MemberModerationService::new(
        discord,
        PgMemberModerationStore::new(db.fixture.pool().clone(), GUILD),
        policy(),
        move || NOW_MS + 3_600_000 + 1_000,
    );
    assert_eq!(sweep.run_due_unbans(GUILD).await.unwrap(), 1);
    mock.shutdown().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn postgres_hierarchy_refusal_writes_nothing() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let exec = pg_executor(&db, &mock);
    let mut high = target();
    high.highest_role_position = 60;
    let err = exec
        .execute(
            &request("moderation.ban", json!({})),
            &actor(),
            &high,
            Some(100),
            "pg-hierarchy",
            "pg-hierarchy-key",
            NOW_MS,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ActionNotAllowed);
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_audit")
        .fetch_one(db.fixture.pool())
        .await
        .unwrap();
    let claims: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_idempotency")
        .fetch_one(db.fixture.pool())
        .await
        .unwrap();
    assert_eq!((audits, claims), (0, 0));
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
    db.cleanup().await;
}
