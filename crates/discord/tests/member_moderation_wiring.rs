//! Member moderation wiring: `MemberDiscord` on the shared REST executor
//! against the scripted REST double.
//!
//! Every member verb through the trait seam: wire route, accepted statuses
//! (kick and unban treat 404 as complete), refusal/uncertain mapping with
//! exactly one wire call, the bounded audit-log reason, and the held-mock
//! 5 s abort.
//!
//! Dev-only: never ships in the release binary.

// `common` houses both the gateway double and the REST double; each test
// target uses one half (matches the `interaction_routing` precedent).
#[allow(dead_code)]
mod common;

use std::time::{Duration, Instant};

use common::{MockRest, ScriptedResponse};
use two_bot_core::member_moderation::{DiscordError as MemberError, MemberDiscord};
use two_bot_discord::{timeout_until_iso, ActionExecutor};

const GUILD: &str = "2222";
const USER: &str = "3333";
const REASON: &str = "member wiring";

fn executor_for(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("member-wiring-token".to_owned(), Some(mock.origin()))
        .expect("executor builds against the mock")
}

fn ban_path() -> String {
    format!("/api/v10/guilds/{GUILD}/bans/{USER}")
}

fn member_path() -> String {
    format!("/api/v10/guilds/{GUILD}/members/{USER}")
}

#[tokio::test]
async fn ban_puts_guild_ban_with_audit_reason() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    MemberDiscord::ban(&exec, GUILD, USER, REASON)
        .await
        .expect("ban 204 completes");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1, "exactly one wire call");
    assert_eq!(reqs[0].method, "PUT");
    assert_eq!(reqs[0].path, ban_path());
    assert!(
        reqs[0].header("x-audit-log-reason").is_some(),
        "ban carries the audit-log reason header"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn unban_completes_gone_member() {
    // Unbanning a non-banned user still completes the job (legacy accepts
    // 200/204/404), with no second attempt.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(404)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    MemberDiscord::unban(&exec, GUILD, USER, REASON)
        .await
        .expect("unban 404 completes");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1, "exactly one wire call");
    assert_eq!(reqs[0].method, "DELETE");
    assert_eq!(reqs[0].path, ban_path());
    mock.shutdown().await;
}

#[tokio::test]
async fn kick_completes_gone_member() {
    // Kicking a departed member still completes the job (legacy accepts
    // 200/204/404), with no second attempt.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(404)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    MemberDiscord::kick(&exec, GUILD, USER, REASON)
        .await
        .expect("kick 404 completes");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1, "exactly one wire call");
    assert_eq!(reqs[0].method, "DELETE");
    assert_eq!(reqs[0].path, member_path());
    mock.shutdown().await;
}

#[tokio::test]
async fn timeout_patches_communication_disabled_until() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(200)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let until = timeout_until_iso(3600);
    MemberDiscord::timeout(&exec, GUILD, USER, &until, REASON)
        .await
        .expect("timeout 200 completes");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1, "exactly one wire call");
    assert_eq!(reqs[0].method, "PATCH");
    assert_eq!(reqs[0].path, member_path());
    let body: serde_json::Value =
        serde_json::from_slice(&reqs[0].body).expect("timeout sends JSON");
    assert!(
        body["communication_disabled_until"].is_string(),
        "timeout carries the expiry timestamp"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn refusal_maps_to_rejected_without_retry() {
    // A definite Discord refusal proves no mutation happened: the claim is
    // safe to release, and the moderation lane never retries it.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let err = MemberDiscord::ban(&exec, GUILD, USER, REASON)
        .await
        .expect_err("ban 403 must refuse");
    assert!(
        matches!(err, MemberError::Rejected(_)),
        "403 maps to Rejected, got {err:?}"
    );
    assert_eq!(mock.requests().len(), 1, "refusal is never retried");
    mock.shutdown().await;
}

#[tokio::test]
async fn uncertain_failures_stay_uncertain_without_retry() {
    // Transport/5xx failures and rate limits leave the outcome unknowable:
    // the claim stays fenced, and the lane makes no second attempt.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(500), ScriptedResponse::status(429)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let err = MemberDiscord::ban(&exec, GUILD, USER, REASON)
        .await
        .expect_err("ban 500 must stay uncertain");
    assert!(
        matches!(err, MemberError::Unavailable(_)),
        "500 maps to Unavailable, got {err:?}"
    );
    let err = MemberDiscord::kick(&exec, GUILD, USER, REASON)
        .await
        .expect_err("kick 429 must stay uncertain");
    assert!(
        matches!(err, MemberError::RateLimited),
        "429 maps to RateLimited, got {err:?}"
    );
    assert_eq!(
        mock.requests().len(),
        2,
        "uncertain failures are never retried in-lane"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn held_mock_aborts_at_five_seconds() {
    // The mock answers in 8 s; the legacy 5 s abort must fire first through
    // the trait seam, with exactly one wire call and an uncertain outcome.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204).delayed(Duration::from_secs(8))],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let start = Instant::now();
    let err = MemberDiscord::ban(&exec, GUILD, USER, REASON)
        .await
        .expect_err("slow Discord must abort");
    let elapsed_ms = start.elapsed().as_millis() as u64;
    assert!(
        matches!(err, MemberError::Timeout),
        "slow answer maps to Timeout, got {err:?}"
    );
    assert!(
        (4500..=7500).contains(&elapsed_ms),
        "abort fires at the legacy 5 s mark, took {elapsed_ms} ms"
    );
    assert_eq!(mock.requests().len(), 1, "moderation never auto-retries");
    mock.shutdown().await;
}

#[tokio::test]
async fn oversize_reason_rejects_before_wire() {
    // The 512-char audit-log bound is enforced before any I/O: a refusal the
    // claim can safely release, with nothing on the wire.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let err = MemberDiscord::ban(&exec, GUILD, USER, &"y".repeat(600))
        .await
        .expect_err("oversize reason must refuse");
    assert!(
        matches!(err, MemberError::Rejected(_)),
        "oversize reason maps to Rejected, got {err:?}"
    );
    assert_eq!(
        mock.requests().len(),
        0,
        "bound refusal never reaches the wire"
    );
    mock.shutdown().await;
}
