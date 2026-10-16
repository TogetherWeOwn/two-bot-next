//! M8.1 acceptance: mutation HTTP 429 proves no effect (member slice).
//!
//! Failing-first regressions for the `proves_no_effect` alignment follow-up
//! noted at `docs/channel-lane-reconciliation.md:97`. The website member
//! executor still classifies a mutation 429 as ambiguous (fenced) instead of
//! releasing the failed attempt like the slash-runtime path does.
//!
//! These tests run WITHOUT a database (`cargo test -p two-bot-discord`):
//! * 429 tests assert the DESIRED behavior and run RED on current HEAD.
//!   The builder fixes production code and MUST NOT edit this file.
//! * Fence tests (5xx, timeout, cancellation, unexpected status) assert the
//!   ambiguity contract that must survive the fix; they are GREEN guards.
//!
//! Cancellation is modelled distinctly from timeout: the caller drops the
//! in-flight attempt (aborted task) instead of waiting for Discord's 5 s
//! abort. Both must stay fenced.

use common::{MockRest, ScriptedResponse};
use serde_json::json;
use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;
use two_bot_core::commands::{PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MODERATE_MEMBERS};
use two_bot_core::internal_actions::ErrorCode;
use two_bot_core::member_moderation::MemMemberStore;
use two_bot_core::{ModerationActor, ModerationPolicy, ModerationTarget};
use two_bot_discord::internal_member_moderation::{
    InternalMemberConfig, InternalMemberExecutor, InternalMemberRequest,
};
use two_bot_discord::{ActionExecutor, MODERATION_TIMEOUT_MS};

#[allow(dead_code)]
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
    assert_eq!(store.audits().len(), 1);
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
    assert!(store.audits().is_empty());
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
    assert!(store.audits().is_empty());
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
    assert!(store.audits().is_empty());
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
        assert!(store.audits().is_empty(), "status {status}");
        mock.shutdown().await;
    }
}
