//! M8.1 acceptance: mutation HTTP 429 proves no effect (member slice).
//!
//! These regressions verify that the website member executor releases a
//! rate-limited mutation attempt for retry, while uncertain outcomes (5xx,
//! timeout, cancellation, and unexpected statuses) remain fenced.
//!
//! Cancellation is modelled distinctly from timeout: the caller drops the
//! in-flight attempt (aborted task) instead of waiting for Discord's 5 s
//! abort. Both must stay fenced.
#![cfg(feature = "db")]

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;
use two_bot_core::commands::{PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MODERATE_MEMBERS};
use two_bot_core::internal_actions::ErrorCode;
use two_bot_core::member_moderation::{AuditRow, MemMemberStore};
use two_bot_core::{ModerationActor, ModerationPolicy, ModerationTarget};
use two_bot_discord::internal_member_moderation::{
    InternalMemberConfig, InternalMemberExecutor, InternalMemberRequest,
};
use two_bot_discord::{ActionExecutor, MODERATION_TIMEOUT_MS};

#[path = "mutation_429_member_common.rs"]
mod common;

const GUILD: &str = "100000000000000001";
const ACTOR: &str = "111111111111111111";
const TARGET_A: &str = "333333333333333333";
const TARGET_B: &str = "333333333333333334";
const STAFF_ROLE: &str = "444444444444444444";
const OWEN_ID: &str = "123456789012345678";
// Fixed clock: 2023-11-14T22:13:20.000Z, so expiries assert exactly.
const NOW_MS: i64 = 1_700_000_000_000;

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

fn target(user_id: &str) -> ModerationTarget {
    ModerationTarget {
        user_id: user_id.to_owned(),
        role_ids: Vec::new(),
        highest_role_position: 10,
        is_bot: false,
        is_guild_owner: false,
    }
}

fn ban_request() -> InternalMemberRequest {
    InternalMemberRequest::from_body(
        "moderation.ban",
        &json!({"actor_id": ACTOR, "discord_id": TARGET_A, "reason": "spam"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap()
}

fn kick_request(target_id: &str) -> InternalMemberRequest {
    InternalMemberRequest::from_body(
        "moderation.kick",
        &json!({"actor_id": ACTOR, "discord_id": target_id, "reason": "spam"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap()
}

fn executor(store: MemMemberStore, mock: &MockRest) -> InternalMemberExecutor<MemMemberStore> {
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

fn assert_failed_discord_audit(row: &AuditRow, request_id: &str, key: &str) {
    assert_eq!(row.request_id, format!("{request_id}:failed"));
    assert_eq!(row.outcome, "failed");
    assert_eq!(row.action, "moderation.ban");
    assert_eq!(row.guild_id, GUILD);
    assert_eq!(row.actor_id, ACTOR);
    assert_eq!(row.target_id.as_deref(), Some(TARGET_A));
    assert_eq!(row.idempotency_key, key);
    assert_eq!(row.reason, "Member moderation did not complete");
    let metadata: Value = serde_json::from_str(&row.metadata_json).unwrap();
    assert_eq!(
        metadata,
        json!({"stage": "discord", "request_id": request_id})
    );
}

fn assert_in_progress_discord_audit(row: &AuditRow, request_id: &str, key: &str) {
    assert_eq!(row.request_id, format!("{request_id}:in_progress"));
    assert_eq!(row.outcome, "in_progress");
    assert_eq!(row.action, "moderation.ban");
    assert_eq!(row.guild_id, GUILD);
    assert_eq!(row.actor_id, ACTOR);
    assert_eq!(row.target_id.as_deref(), Some(TARGET_A));
    assert_eq!(row.idempotency_key, key);
    assert_eq!(row.reason, "Member moderation did not complete");
    let metadata: Value = serde_json::from_str(&row.metadata_json).unwrap();
    assert_eq!(
        metadata,
        json!({"stage": "discord", "request_id": request_id})
    );
}

fn assert_in_progress_claim_audit(row: &AuditRow, request_id: &str, key: &str) {
    assert_eq!(row.request_id, format!("{request_id}:in_progress"));
    assert_eq!(row.outcome, "in_progress");
    assert_eq!(row.action, "moderation.ban");
    assert_eq!(row.guild_id, GUILD);
    assert_eq!(row.actor_id, ACTOR);
    assert_eq!(row.target_id.as_deref(), Some(TARGET_A));
    assert_eq!(row.idempotency_key, key);
    assert_eq!(row.reason, "Member moderation did not complete");
    let metadata: Value = serde_json::from_str(&row.metadata_json).unwrap();
    assert_eq!(
        metadata,
        json!({"stage": "claim", "request_id": request_id})
    );
}

fn assert_ban_success_audit(row: &AuditRow, request_id: &str, key: &str, generation: i64) {
    assert_eq!(row.request_id, request_id);
    assert_eq!(row.outcome, "banned");
    assert_eq!(row.action, "moderation.ban");
    assert_eq!(row.guild_id, GUILD);
    assert_eq!(row.actor_id, ACTOR);
    assert_eq!(row.target_id.as_deref(), Some(TARGET_A));
    assert_eq!(row.idempotency_key, key);
    assert_eq!(row.reason, "spam");
    let metadata: Value = serde_json::from_str(&row.metadata_json).unwrap();
    assert_eq!(
        metadata,
        json!({"duration_seconds": null, "ban_attempt_generation": generation})
    );
}

#[tokio::test]
async fn mutation_429_releases_member_claim_for_same_key_retry() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(429), ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let store = MemMemberStore::new();
    let exec = executor(store.clone(), &mock);
    let req = ban_request();
    let err = exec
        .execute(
            &req,
            &actor(),
            &target(TARGET_A),
            100,
            "req-429",
            "key-429-valid",
            NOW_MS,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RateLimited);
    // The released 429 attempt leaves one bound failed/discord row under the
    // suffixed id, so it can never mask the retry's success row.
    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    assert_failed_discord_audit(&audits[0], "req-429", "key-429-valid");
    // A rate-limited mutation proves no effect: the same key must be a real
    // second attempt, not a replay and not a fenced `InProgress`.
    let retry = exec
        .execute(
            &req,
            &actor(),
            &target(TARGET_A),
            100,
            "req-429",
            "key-429-valid",
            NOW_MS,
        )
        .await
        .unwrap();
    assert_eq!(retry.outcome, "banned");
    assert!(!retry.replayed);
    assert_eq!(mock.requests().len(), 2);
    // The failed row survives untouched and the accepted retry adds its plain
    // success row with a fresh ban generation (the 429 attempt was rejected).
    let audits = store.audits();
    assert_eq!(audits.len(), 2);
    assert_failed_discord_audit(&audits[0], "req-429", "key-429-valid");
    assert_ban_success_audit(&audits[1], "req-429", "key-429-valid", 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn mutation_429_releases_only_the_failed_attempt() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::status(429),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let store = MemMemberStore::new();
    let exec = executor(store.clone(), &mock);
    // An unrelated member is never fenced by another attempt's outcome.
    let kick_b = exec
        .execute(
            &kick_request(TARGET_B),
            &actor(),
            &target(TARGET_B),
            100,
            "req-other",
            "key-other",
            NOW_MS,
        )
        .await
        .unwrap();
    assert_eq!(kick_b.outcome, "kicked");
    // The 429 attempt itself reports rate-limited...
    let req = ban_request();
    assert_eq!(
        exec.execute(
            &req,
            &actor(),
            &target(TARGET_A),
            100,
            "req-429",
            "key-429-valid",
            NOW_MS
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::RateLimited
    );
    // ...and only its own key is released for a real retry.
    let retry = exec
        .execute(
            &req,
            &actor(),
            &target(TARGET_A),
            100,
            "req-429",
            "key-429-valid",
            NOW_MS,
        )
        .await
        .unwrap();
    assert_eq!(retry.outcome, "banned");
    assert!(!retry.replayed);
    assert_eq!(mock.requests().len(), 3);
    mock.shutdown().await;
}

#[tokio::test]
async fn mutation_5xx_stays_fenced() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(503)],
        ScriptedResponse::status(500),
    )
    .await;
    let store = MemMemberStore::new();
    let exec = executor(store.clone(), &mock);
    let req = ban_request();
    assert_eq!(
        exec.execute(
            &req,
            &actor(),
            &target(TARGET_A),
            100,
            "req-5xx",
            "key-5xx-valid",
            NOW_MS
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::DiscordUnavailable
    );
    assert_eq!(mock.requests().len(), 1);
    // The uncertain 503 leaves one bound in_progress/discord row under the
    // suffixed id.
    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    assert_in_progress_discord_audit(&audits[0], "req-5xx", "key-5xx-valid");
    // Uncertain effect: same-key retry refuses, never a second PUT, and no
    // completion was recorded.
    assert_eq!(
        exec.execute(
            &req,
            &actor(),
            &target(TARGET_A),
            100,
            "req-5xx",
            "key-5xx-valid",
            NOW_MS
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InProgress
    );
    assert_eq!(mock.requests().len(), 1);
    // The retry's in-flight claim audits under the same suffixed id and is
    // deduplicated, so the original discord observation survives untouched.
    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    assert_in_progress_discord_audit(&audits[0], "req-5xx", "key-5xx-valid");
    assert!(
        store
            .audits()
            .iter()
            .all(|row| row.outcome == "in_progress"),
        "no success or completion row"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn mutation_timeout_stays_fenced() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)
            .delayed(Duration::from_millis(MODERATION_TIMEOUT_MS + 250))],
        ScriptedResponse::status(500),
    )
    .await;
    let store = MemMemberStore::new();
    let exec = executor(store.clone(), &mock);
    let req = ban_request();
    assert_eq!(
        exec.execute(
            &req,
            &actor(),
            &target(TARGET_A),
            100,
            "req-timeout",
            "key-timeout",
            NOW_MS
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::UpstreamTimeout
    );
    assert_eq!(mock.requests().len(), 1);
    // The uncertain timeout leaves one bound in_progress/discord row under the
    // suffixed id.
    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    assert_in_progress_discord_audit(&audits[0], "req-timeout", "key-timeout");
    assert_eq!(
        exec.execute(
            &req,
            &actor(),
            &target(TARGET_A),
            100,
            "req-timeout",
            "key-timeout",
            NOW_MS
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InProgress
    );
    assert_eq!(mock.requests().len(), 1);
    // The retry's in-flight claim audits under the same suffixed id and is
    // deduplicated, so the original discord observation survives untouched.
    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    assert_in_progress_discord_audit(&audits[0], "req-timeout", "key-timeout");
    assert!(
        store
            .audits()
            .iter()
            .all(|row| row.outcome == "in_progress"),
        "no success or completion row"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn cancelled_mutation_stays_fenced() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204).delayed(Duration::from_secs(2))],
        ScriptedResponse::status(500),
    )
    .await;
    let store = MemMemberStore::new();
    let exec = executor(store.clone(), &mock);
    // Cancel the caller while the Discord PUT is in flight: dropping the
    // future must not release the claim, because the mutation may still land.
    let attempt = tokio::spawn({
        let exec = exec.clone();
        let req = ban_request();
        let actor = actor();
        let target = target(TARGET_A);
        async move {
            exec.execute(
                &req,
                &actor,
                &target,
                100,
                "req-cancel",
                "key-cancel",
                NOW_MS,
            )
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
    // Dropping the in-flight PUT writes no discord observation: the claim
    // stays in flight with an empty ledger until the retry's refusal audits.
    assert!(store.audits().is_empty());
    let req = ban_request();
    assert_eq!(
        exec.execute(
            &req,
            &actor(),
            &target(TARGET_A),
            100,
            "req-cancel",
            "key-cancel",
            NOW_MS
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InProgress
    );
    assert_eq!(mock.requests().len(), 1);
    // The aborted attempt never reached its discord audit, so the in-flight
    // refusal is the single bound in_progress/claim row under the suffixed id.
    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    assert_in_progress_claim_audit(&audits[0], "req-cancel", "key-cancel");
    assert!(
        store
            .audits()
            .iter()
            .all(|row| row.outcome == "in_progress"),
        "no success or completion row"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn unexpected_status_stays_fenced() {
    for status in [202, 302, 408, 409, 425] {
        let mock = MockRest::start(
            vec![ScriptedResponse::status(status)],
            ScriptedResponse::status(500),
        )
        .await;
        let store = MemMemberStore::new();
        let exec = executor(store.clone(), &mock);
        let req = ban_request();
        assert_eq!(
            exec.execute(
                &req,
                &actor(),
                &target(TARGET_A),
                100,
                "req-ambiguous",
                "key-ambiguous",
                NOW_MS
            )
            .await
            .unwrap_err()
            .code,
            ErrorCode::DiscordUnavailable,
            "status {status}"
        );
        assert_eq!(mock.requests().len(), 1, "status {status}");
        // Each ambiguous status leaves one bound in_progress/discord row under
        // the suffixed id.
        let audits = store.audits();
        assert_eq!(audits.len(), 1, "status {status}");
        assert_in_progress_discord_audit(&audits[0], "req-ambiguous", "key-ambiguous");
        assert_eq!(
            exec.execute(
                &req,
                &actor(),
                &target(TARGET_A),
                100,
                "req-ambiguous",
                "key-ambiguous",
                NOW_MS
            )
            .await
            .unwrap_err()
            .code,
            ErrorCode::InProgress,
            "status {status}"
        );
        assert_eq!(mock.requests().len(), 1, "status {status}");
        // The retry's in-flight claim audits under the same suffixed id and is
        // deduplicated, so the original discord observation survives untouched.
        let audits = store.audits();
        assert_eq!(audits.len(), 1, "status {status}");
        assert_in_progress_discord_audit(&audits[0], "req-ambiguous", "key-ambiguous");
        assert!(
            store
                .audits()
                .iter()
                .all(|row| row.outcome == "in_progress"),
            "status {status}: no success or completion row"
        );
        mock.shutdown().await;
    }
}
