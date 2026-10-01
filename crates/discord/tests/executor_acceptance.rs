//! S4 acceptance ([TOG-10076](/TOG/issues/TOG-10076)): the Discord REST action
//! executor against the scripted REST side of the mock Discord double.
//!
//! Every pacing/backoff number from docs/parity.md §6 is pinned on the wire
//! (mock-side arrival times, not client timers): 110 ms paced floor, 350 ms
//! kick floor, 429 → `retry-after + 250 ms` (body wins over header), 5xx →,
//! `500 * 2^attempt` backoff with ≤4 retries, moderation 5 s abort with no
//! auto-retry, and `allowed_mentions: { parse: [] }` on outbound messages.
//! The mock records every call for assertions.
//!
//! Dev-only: never ships in the release binary.

// `common` houses both the gateway double and the REST double; each test
// target uses one half (matches the `interaction_routing` precedent).
#[allow(dead_code)]
mod common;

use std::time::{Duration, Instant};

use common::{MockRest, RestRequest, ScriptedResponse};
use two_bot_core::{ActionOutcome, KickOutcome, ModerationAction, ModerationExecution};
use two_bot_discord::executor::KickAttemptState;
use two_bot_discord::{ActionExecutor, DiscordError};

const GUILD: &str = "2222";
const USER: &str = "3333";
const CHANNEL: &str = "4444";
const REASON: &str = "s4 acceptance";

fn executor_for(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("s4-acceptance-token".to_owned(), Some(mock.origin()))
        .expect("executor builds against the mock")
}

/// Mock-side arrival gap between two consecutive requests, ms.
fn gap_ms(reqs: &[RestRequest]) -> u64 {
    reqs[1]
        .received_at
        .duration_since(reqs[0].received_at)
        .as_millis() as u64
}

fn kick_path() -> String {
    format!("/api/v10/guilds/{GUILD}/members/{USER}")
}

#[tokio::test]
async fn kick_terminal_paths_hit_expected_route_with_audit_reason() {
    // (scripted status, expected outcome): success and already-gone both end
    // on the first attempt with zero retries.
    for (status, expected) in [
        (204u16, KickOutcome::Kicked),
        (404, KickOutcome::AlreadyGone),
    ] {
        let mock = MockRest::start(
            vec![ScriptedResponse::status(status)],
            ScriptedResponse::status(500),
        )
        .await;
        let exec = executor_for(&mock);
        let result = exec.kick_paced(GUILD, USER, REASON).await;
        assert_eq!(result.outcome, expected, "status {status}");
        assert_eq!(result.attempts, 1, "terminal status is never retried");
        let reqs = mock.requests();
        assert_eq!(reqs.len(), 1, "exactly one wire call");
        assert_eq!(reqs[0].method, "DELETE");
        assert_eq!(reqs[0].path, kick_path());
        assert!(
            reqs[0].header("x-audit-log-reason").is_some(),
            "kick carries the audit-log reason header"
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn kick_paces_removals_at_350ms_floor() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::status(204),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    for _ in 0..3 {
        let result = exec.kick_paced(GUILD, USER, REASON).await;
        assert_eq!(result.outcome, KickOutcome::Kicked);
    }
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3);
    // The third call catches timestamps captured before the second call's wait.
    for pair in reqs.windows(2) {
        let gap = gap_ms(pair);
        assert!(
            (300..=3000).contains(&gap),
            "kick lane holds the legacy 350 ms floor, got {gap} ms"
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn guarded_kick_clones_hold_350ms_wire_floor_despite_different_guard_delays() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let first = executor_for(&mock);
    let second = first.clone();
    let (guard_entered, entered) = tokio::sync::oneshot::channel();
    let first = tokio::spawn(async move {
        let mut guard_entered = Some(guard_entered);
        first
            .kick_paced_guarded(GUILD, USER, REASON, |state| {
                assert_eq!(state, KickAttemptState::default());
                let guard_entered = guard_entered.take().expect("one authorization");
                async move {
                    guard_entered.send(()).expect("guard entry observed");
                    tokio::time::sleep(Duration::from_millis(750)).await;
                    Ok::<(), ()>(())
                }
            })
            .await
    });
    entered.await.expect("first guard holds the reservation");
    let second = tokio::spawn(async move {
        second
            .kick_paced_guarded(GUILD, "3334", REASON, |state| async move {
                assert_eq!(state, KickAttemptState::default());
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<(), ()>(())
            })
            .await
    });
    let joined = async { tokio::join!(first, second) };
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), joined)
        .await
        .expect("both guarded kicks finish");
    assert_eq!(first.unwrap().unwrap().outcome, KickOutcome::Kicked);
    assert_eq!(second.unwrap().unwrap().outcome, KickOutcome::Kicked);
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    assert!(reqs.iter().all(|r| r.method == "DELETE"));
    assert_eq!(
        reqs[0].path,
        kick_path(),
        "the slow guard cannot be overtaken"
    );
    assert_eq!(reqs[1].path, "/api/v10/guilds/2222/members/3334");
    let gap = reqs[1].received_at.duration_since(reqs[0].received_at);
    assert!(
        gap >= Duration::from_millis(350),
        "clones must maintain 350 ms between actual DELETE arrivals, got {gap:?}"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn guarded_kick_definitive_429_and_5xx_do_not_mark_mutation_uncertain() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::rate_limited(0.0, "0"),
            ScriptedResponse::status(503),
        ],
        ScriptedResponse::status(204),
    )
    .await;
    let exec = executor_for(&mock);
    let mut states = Vec::new();
    let stopped = exec
        .kick_paced_guarded(GUILD, USER, REASON, |state| {
            states.push(state);
            async move {
                if state.attempts == 2 {
                    Err(state)
                } else {
                    Ok(())
                }
            }
        })
        .await
        .expect_err("fresh protection refuses the third DELETE");
    assert_eq!(
        stopped,
        KickAttemptState {
            attempts: 2,
            mutation_uncertain: false
        }
    );
    assert_eq!(
        states,
        (0..=2)
            .map(|attempts| KickAttemptState {
                attempts,
                mutation_uncertain: false
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(
        mock.requests().len(),
        2,
        "guard refusal sends no third DELETE"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn guarded_kick_timeout_uncertainty_survives_a_later_429() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204).delayed(Duration::from_secs(6)),
            ScriptedResponse::rate_limited(0.0, "0"),
        ],
        ScriptedResponse::status(204),
    )
    .await;
    let exec = executor_for(&mock);
    let mut states = Vec::new();
    let stopped = exec
        .kick_paced_guarded(GUILD, USER, REASON, |state| {
            states.push(state);
            async move {
                if state.attempts == 2 {
                    Err(state)
                } else {
                    Ok(())
                }
            }
        })
        .await
        .expect_err("fresh protection refuses a retry with a pending uncertain mutation");
    assert_eq!(
        stopped,
        KickAttemptState {
            attempts: 2,
            mutation_uncertain: true
        }
    );
    assert_eq!(
        states,
        vec![
            KickAttemptState::default(),
            KickAttemptState {
                attempts: 1,
                mutation_uncertain: true
            },
            KickAttemptState {
                attempts: 2,
                mutation_uncertain: true
            },
        ]
    );
    assert_eq!(
        mock.requests().len(),
        2,
        "guard refusal sends no third DELETE"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn kick_429_parks_for_body_retry_after_plus_250ms() {
    // Body `retry_after: 1.0` → 1000 + 250 ms wait; the `retry-after: 3`
    // header must lose (it would park ~3250 ms instead).
    let mock = MockRest::start(
        vec![
            ScriptedResponse::rate_limited(1.0, "3"),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let result = exec.kick_paced(GUILD, USER, REASON).await;
    assert_eq!(result.outcome, KickOutcome::Kicked);
    assert_eq!(
        result.attempts, 2,
        "429 parks on the same attempt, then retries"
    );
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    let gap = gap_ms(&reqs);
    assert!(
        (1000..=3000).contains(&gap),
        "429 wait is body retry-after + 250 ms (header loses), got {gap} ms"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn kick_5xx_backs_off_from_500ms_then_succeeds() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(503), ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let result = exec.kick_paced(GUILD, USER, REASON).await;
    assert_eq!(result.outcome, KickOutcome::Kicked);
    assert_eq!(result.attempts, 2);
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    let gap = gap_ms(&reqs);
    assert!(
        (400..=4000).contains(&gap),
        "first 5xx backoff is legacy 500 ms, got {gap} ms"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn kick_gives_up_after_five_attempts() {
    // ≤4 retries: 500 + 1000 + 2000 + 4000 ms of backoff, then a Failed
    // value — the executor reports, never throws, never retries a sixth time.
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let exec = executor_for(&mock);
    let result = exec.kick_paced(GUILD, USER, REASON).await;
    assert_eq!(result.outcome, KickOutcome::Failed);
    assert_eq!(result.attempts, 5, "one try plus legacy ≤4 retries");
    assert_eq!(result.status, Some(500));
    assert_eq!(mock.requests().len(), 5, "no sixth attempt");
    mock.shutdown().await;
}

#[tokio::test]
async fn moderation_lane_aborts_without_retry() {
    // The mock answers in 8 s; the legacy 5 s abort must fire first, with
    // exactly one wire call — no auto-retry on the moderation lane.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204).delayed(Duration::from_secs(8))],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let start = Instant::now();
    let err = exec
        .ban(GUILD, USER, REASON)
        .await
        .expect_err("slow Discord must abort");
    let elapsed_ms = start.elapsed().as_millis() as u64;
    assert!(
        matches!(err, DiscordError::Timeout),
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
async fn moderation_429_is_not_retried() {
    let mock = MockRest::start(
        vec![ScriptedResponse::rate_limited(0.0, "1")],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let err = exec
        .ban(GUILD, USER, REASON)
        .await
        .expect_err("429 on the moderation lane is terminal");
    assert!(
        matches!(err, DiscordError::RateLimited),
        "429 maps to RateLimited, got {err:?}"
    );
    assert_eq!(mock.requests().len(), 1, "moderation never auto-retries");
    mock.shutdown().await;
}

#[tokio::test]
async fn unban_404_completes_and_audit_header_is_sent() {
    // Legacy accepts 200/204/404 for unban; the audit reason rides along.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(404)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    exec.unban(GUILD, USER, REASON)
        .await
        .expect("unbanning a non-banned user still completes");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "DELETE");
    assert_eq!(reqs[0].path, format!("/api/v10/guilds/{GUILD}/bans/{USER}"));
    assert!(
        reqs[0].header("x-audit-log-reason").is_some(),
        "ban-lane verbs carry the audit-log reason header"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn post_message_suppresses_mentions() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(201, serde_json::json!({"id": "99"}))],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let id = exec
        .post_message(CHANNEL, "hello @everyone", None)
        .await
        .expect("post succeeds");
    assert_eq!(id, "99");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(
        reqs[0].path,
        format!("/api/v10/channels/{CHANNEL}/messages")
    );
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("post body is JSON");
    assert_eq!(body["content"], "hello @everyone");
    assert_eq!(
        body["allowed_mentions"]["parse"],
        serde_json::json!([]),
        "admin-authored text never pings @everyone"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn paced_gets_hold_110ms_floor() {
    let channel_body = serde_json::json!({"id": CHANNEL});
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, channel_body.clone()),
            ScriptedResponse::json(200, channel_body.clone()),
            ScriptedResponse::json(200, channel_body),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    for _ in 0..3 {
        let result = exec
            .get_json(&format!("/channels/{CHANNEL}"))
            .await
            .expect("read succeeds");
        assert!(result.is_some());
    }
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3);
    assert!(reqs.iter().all(|r| r.method == "GET"));
    // Check both gaps: two calls alone miss a stale post-wait timestamp.
    for pair in reqs.windows(2) {
        let gap = gap_ms(pair);
        assert!(
            (80..=2000).contains(&gap),
            "paced lane holds the legacy 110 ms floor, got {gap} ms"
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn warn_outcome_never_reaches_the_wire() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let exec = executor_for(&mock);
    let outcome = ActionOutcome::Warned {
        user_id: USER.to_owned(),
    };
    let exec_ctx = ModerationExecution {
        guild_id: GUILD.to_owned(),
        channel_id: None,
        action: ModerationAction::Warn,
        target_user_id: Some(USER.to_owned()),
        reason: REASON.to_owned(),
        duration_seconds: None,
        count: None,
        seconds: None,
    };
    let returned = exec
        .execute_outcome(&exec_ctx, &outcome)
        .await
        .expect("warn is store-only and always succeeds");
    assert_eq!(returned, outcome);
    assert!(mock.requests().is_empty(), "warn sends zero Discord calls");
    assert_eq!(exec.requests(), 0);
    mock.shutdown().await;
}
