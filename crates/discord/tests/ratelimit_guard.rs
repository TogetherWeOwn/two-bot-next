//! Global guard acceptance against the existing local Discord REST double.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseType};
use two_bot_discord::{
    ratelimit_guard::{GuardConfig, GuardError, RateLimitGuard},
    ActionExecutor, DiscordError,
};

fn executor(mock: &MockRest, guard: &Arc<RateLimitGuard>) -> ActionExecutor {
    ActionExecutor::with_proxy_and_guard(
        "guard-fixture-token".to_owned(),
        Some(mock.origin()),
        guard.clone(),
    )
    .unwrap()
}

#[tokio::test]
async fn audit_global_admission_precedes_the_late_authorization() {
    use two_bot_core::audit_mirror::AuditMirror;

    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, serde_json::json!({"id": "640"})),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
    let exec = executor(&mock, &guard);
    // Anchor before observing the pause: the guard sets its deadline at
    // observation time plus 0.4 s and the 250 ms pad, and admission only
    // returns at or after that deadline, so authorization (which runs after
    // admission) lands at least 650 ms after this anchor. Anchoring after
    // `observe_global` instead subtracted any scheduling gap between the two
    // lines from the measured wait, so a gap over 50 ms made the 600 ms floor
    // unsatisfiable even though admission honored the full pause. Delays
    // after this anchor can only push observation and authorization later.
    let started = Instant::now();
    guard.observe_global(Some(0.4));
    let result = exec
        .post_mirror_checked("4444", "audit test", "oa_fixture", async {
            assert!(started.elapsed() >= Duration::from_millis(600));
            assert_eq!(guard.snapshot().global_pause_remaining, Duration::ZERO);
            Ok::<(), ()>(())
        })
        .await;
    assert_eq!(result, Ok(Ok("640".to_owned())));
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn audit_guard_closure_during_authorization_refuses_without_waiting_or_dispatch() {
    use two_bot_core::audit_mirror::{AuditMirror, MirrorError};

    for restriction in ["global", "breaker", "token"] {
        let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
        let guard = Arc::new(
            RateLimitGuard::new(GuardConfig {
                invalid_request_threshold: 1,
                ..Default::default()
            })
            .unwrap(),
        );
        let exec = executor(&mock, &guard);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            exec.post_mirror_checked("4444", "audit test", "oa_fixture", async {
                match restriction {
                    "global" => guard.observe_global(Some(600.0)),
                    "breaker" => guard.observe_status(403, true),
                    "token" => guard.observe_status(401, true),
                    _ => unreachable!(),
                }
                Ok::<(), ()>(())
            }),
        )
        .await
        .expect("no admission sleep is allowed after authorization")
        .unwrap();
        assert!(matches!(result, Err(MirrorError::Rejected(_))));
        assert_eq!(mock.requests().len(), 0);
        assert_eq!(exec.requests(), 0);
        assert_eq!(guard.snapshot().rejected_requests_total, 1);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn audit_guard_refused_reads_hold_evidence_and_recover_after_cooldown() {
    use two_bot_core::audit_mirror::{AuditMirror, MirrorError};

    for restriction in ["breaker", "token", "global"] {
        let mock = MockRest::start(
            vec![
                ScriptedResponse::json(200, serde_json::json!({"guild_id": "2222"})),
                ScriptedResponse::json(200, serde_json::json!([])),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        let guard = Arc::new(
            RateLimitGuard::new(GuardConfig {
                invalid_request_threshold: 1,
                window: Duration::from_millis(300),
            })
            .unwrap(),
        );
        let exec = executor(&mock, &guard);
        match restriction {
            "breaker" => guard.observe_status(403, true),
            "token" => guard.observe_status(401, true),
            "global" => guard.observe_global(Some(600.0)),
            _ => unreachable!(),
        }
        let (document, history) = tokio::join!(
            exec.channel_document("4444"),
            exec.channel_history("4444", None, 100),
        );
        assert!(matches!(document, Err(MirrorError::Uncertain(_))));
        assert!(matches!(history, Err(MirrorError::Uncertain(_))));
        assert_eq!(exec.requests(), 0);
        assert!(mock.requests().is_empty());
        if restriction == "breaker" {
            tokio::time::sleep(Duration::from_millis(320)).await;
            assert_eq!(
                exec.channel_document("4444").await.unwrap().guild_id,
                "2222"
            );
            assert!(exec
                .channel_history("4444", None, 100)
                .await
                .unwrap()
                .is_empty());
            assert_eq!(mock.requests().len(), 2);
            assert!(mock
                .requests()
                .iter()
                .all(|request| request.method == "GET"));
        }
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn interaction_webhook_401_counts_invalid_without_latching_the_bot_token() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(401)],
        ScriptedResponse::status(204),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
    let exec = executor(&mock, &guard);
    assert!(matches!(
        exec.edit_interaction_response(5555, "callback-fixture-token", "test")
            .await,
        Err(DiscordError::Rejected(_))
    ));
    assert_eq!(guard.snapshot().invalid_requests_total, 1);
    assert!(!guard.snapshot().token_invalid);
    ban(&exec).await.unwrap();
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn internal_member_guard_refusals_preserve_the_pre_dispatch_boundary() {
    use two_bot_core::internal_actions::{ErrorCode, GuildAddMemberRequest};

    for restriction in ["token", "global"] {
        let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
        let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
        let exec = executor(&mock, &guard);
        if restriction == "token" {
            guard.observe_status(401, true);
        } else {
            guard.observe_global(Some(600.0));
        }
        let body = serde_json::json!({
            "action": "guild.add_member",
            "discord_id": "100000000000000002",
            "access_token": "fixture-only-oauth"
        });
        let request = GuildAddMemberRequest::validate(body.as_object().unwrap()).unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            exec.add_internal_member("2222", &request, "fixture-only-oauth"),
        )
        .await
        .expect("member admission retains its 1500ms total deadline")
        .unwrap_err();
        assert_eq!(
            error.code,
            if restriction == "token" {
                ErrorCode::DiscordUnavailable
            } else {
                ErrorCode::RateLimited
            }
        );
        assert!(error.code.retryable());
        assert_eq!(error.log_reason, "discord_guard_refused");
        assert_eq!(exec.requests(), 0);
        assert_eq!(mock.requests().len(), 0);
        mock.shutdown().await;
    }
}

async fn ban(executor: &ActionExecutor) -> Result<(), DiscordError> {
    executor.ban("2222", "3333", "guard test").await
}

#[tokio::test]
async fn single_attempt_roster_get_preserves_wire_budget_and_shared_global_pause() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(429, serde_json::json!({"retry_after": 0.2, "global": true})),
            ScriptedResponse::json(200, serde_json::json!([])),
            ScriptedResponse::status(500),
        ],
        ScriptedResponse::status(200),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
    let first = executor(&mock, &guard);
    let second = executor(&mock, &guard);
    let path = "/guilds/2222/members?limit=1000";

    assert_eq!(first.get_json_once(path).await.unwrap(), None);
    assert_eq!(first.requests(), 1);
    assert_eq!(mock.requests().len(), 1, "a 429 must not retry the page");
    assert_eq!(
        second.get_json_once(path).await.unwrap(),
        Some(serde_json::json!([]))
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .received_at
            .duration_since(requests[0].received_at)
            >= Duration::from_millis(425),
        "the next page must honor the process-wide pause"
    );
    assert_eq!(second.get_json_once(path).await.unwrap(), None);
    assert_eq!(second.requests(), 2);
    assert_eq!(mock.requests().len(), 3, "a 5xx must not retry the page");
    assert_eq!(guard.snapshot().global_pauses_total, 1);
    assert_eq!(guard.snapshot().invalid_requests_total, 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn single_attempt_roster_get_refuses_before_wire_and_recovers_after_cooldown() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403)],
        ScriptedResponse::json(200, serde_json::json!([])),
    )
    .await;
    let guard = Arc::new(
        RateLimitGuard::new(GuardConfig {
            invalid_request_threshold: 1,
            window: Duration::from_millis(300),
        })
        .unwrap(),
    );
    let exec = executor(&mock, &guard);
    let path = "/guilds/2222/members?limit=1000";
    assert_eq!(exec.get_json_once(path).await.unwrap(), None);
    assert!(guard.snapshot().breaker_open);
    assert_eq!(
        exec.get_json_once(path).await.unwrap_err(),
        GuardError::CircuitOpen.to_string()
    );
    assert_eq!(exec.requests(), 1);
    assert_eq!(mock.requests().len(), 1);
    tokio::time::sleep(Duration::from_millis(320)).await;
    assert_eq!(
        exec.get_json_once(path).await.unwrap(),
        Some(serde_json::json!([]))
    );
    assert_eq!(exec.requests(), 2);
    assert_eq!(mock.requests().len(), 2);
    assert!(!guard.snapshot().breaker_open);
    mock.shutdown().await;
}

#[tokio::test]
async fn global_429_pauses_independent_executors_concurrently_once() {
    for signal in ["body", "global-header", "scope-header", "scope-body"] {
        let mut response = ScriptedResponse::json(
            429,
            serde_json::json!({"retry_after": 0.6, "global": signal == "body"}),
        );
        match signal {
            "global-header" => response
                .headers
                .push(("X-RateLimit-Global".into(), "true".into())),
            "scope-header" => response
                .headers
                .push(("X-RateLimit-Scope".into(), "global".into())),
            "scope-body" => response.body = br#"{"retry_after":0.6,"scope":"global"}"#.to_vec(),
            _ => {}
        }
        let mock = MockRest::start(vec![response], ScriptedResponse::status(204)).await;
        let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
        let first = executor(&mock, &guard);
        let second = executor(&mock, &guard);
        assert_eq!(ban(&first).await, Err(DiscordError::RateLimited));
        let start = Instant::now();
        let (a, b, c) = tokio::join!(ban(&first), ban(&second), ban(&second));
        a.unwrap();
        b.unwrap();
        c.unwrap();
        let requests = mock.requests();
        assert_eq!(requests.len(), 4);
        for request in &requests[1..] {
            assert!(
                request.received_at.duration_since(requests[0].received_at)
                    >= Duration::from_millis(800),
                "{signal}: caller skipped global pause"
            );
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{signal}: serialized/additive cooldowns"
        );
        assert_eq!(guard.snapshot().global_pauses_total, 1);
        assert_eq!(guard.snapshot().invalid_requests_total, 1);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn threshold_refuses_without_wire_and_rolling_window_reopens() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403), ScriptedResponse::status(403)],
        ScriptedResponse::status(204),
    )
    .await;
    let guard = Arc::new(
        RateLimitGuard::new(GuardConfig {
            invalid_request_threshold: 2,
            window: Duration::from_millis(300),
        })
        .unwrap(),
    );
    let first = executor(&mock, &guard);
    let second = executor(&mock, &guard);
    assert!(matches!(ban(&first).await, Err(DiscordError::Rejected(_))));
    assert!(matches!(ban(&second).await, Err(DiscordError::Rejected(_))));
    let err = ban(&first).await.unwrap_err();
    assert_eq!(err, DiscordError::Guard(GuardError::CircuitOpen));
    assert!(err.is_safe_pre_mutation());
    assert_eq!(mock.requests().len(), 2);
    assert_eq!(first.requests() + second.requests(), 2);
    assert_eq!(guard.snapshot().breaker_opens_total, 1);
    tokio::time::sleep(Duration::from_millis(320)).await;
    ban(&second).await.unwrap();
    let snapshot = guard.snapshot();
    assert!(!snapshot.breaker_open);
    assert_eq!(snapshot.invalid_requests_in_window, 0);
    assert_eq!(snapshot.invalid_requests_total, 2);
    assert_eq!(snapshot.breaker_closes_total, 1);
    assert_eq!(snapshot.rejected_requests_total, 1);
    assert_eq!(mock.requests().len(), 3);
    mock.shutdown().await;
}

#[tokio::test]
async fn bot_401_latches_fatal_even_after_window_rollover_and_for_essential_calls() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(401)],
        ScriptedResponse::status(204),
    )
    .await;
    let guard = Arc::new(
        RateLimitGuard::new(GuardConfig {
            invalid_request_threshold: 2,
            window: Duration::from_millis(30),
        })
        .unwrap(),
    );
    let first = executor(&mock, &guard);
    let second = executor(&mock, &guard);
    assert!(matches!(ban(&first).await, Err(DiscordError::Rejected(_))));
    assert!(guard.snapshot().token_invalid);
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        ban(&second).await,
        Err(DiscordError::Guard(GuardError::TokenInvalid))
    );
    assert_eq!(
        second
            .answer_interaction(1234, "callback-fixture-token", &pong())
            .await,
        Err(DiscordError::Guard(GuardError::TokenInvalid))
    );
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn global_headers_block_callers_before_delayed_body_finishes() {
    for (header_timing, retry_after, body_delay) in [
        (Some("0.8"), 0.8, Duration::from_millis(400)),
        (None, 0.8, Duration::from_millis(400)),
        (Some("0.1"), 1.0, Duration::from_millis(600)),
    ] {
        let mut response =
            ScriptedResponse::json(429, serde_json::json!({"retry_after": retry_after}));
        response
            .headers
            .push(("X-RateLimit-Global".into(), "true".into()));
        if let Some(timing) = header_timing {
            response.headers.push(("Retry-After".into(), timing.into()));
        }
        let mock = MockRest::start_with_body_delay(
            vec![response],
            ScriptedResponse::status(204),
            body_delay,
        )
        .await;
        let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
        let first = executor(&mock, &guard);
        let second = executor(&mock, &guard);
        let initial = tokio::spawn(async move { ban(&first).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while guard.snapshot().invalid_requests_total == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        // Header-observation time: the guard anchors every pause at `observed_at`
        // plus `retry_after + 250ms`, so the settled deadline minus this instant
        // must stay near the body timing plus the pad — not shift by the
        // body-finish delay. Sampling here (1ms poll) keeps measurement error
        // far below the 400-600ms restart signal.
        let header_observed = Instant::now();
        let callers = tokio::spawn(async move { tokio::join!(ban(&second), ban(&second)) });
        tokio::time::sleep(body_delay - Duration::from_millis(100)).await;
        assert!(!initial.is_finished(), "body must still be pending");
        assert_eq!(
            mock.requests().len(),
            1,
            "known header restriction bypassed"
        );
        assert_eq!(initial.await.unwrap(), Err(DiscordError::RateLimited));
        // No-restart proof via the guard deadline, not the wire clock: a true
        // restart-at-body would add the full `body_delay` (400-600ms) to the
        // deadline, while post-deadline scheduling stalls (which flaked the old
        // `retry_after + 0.55` wire bound on loaded runners) leave the deadline
        // itself untouched.
        //
        // Only the upper bound is asserted: both sampling lag (the 1ms poll
        // lands after the guard records the headers) and an expired pause
        // (zero remaining) shrink `anchored`, so a lower deadline bound would
        // reintroduce the flake class this removes. Short deadlines stay
        // covered by the wire lower bound below, and the exact pad/anchor
        // arithmetic by the paused-clock unit tests. A retained pre-body
        // deadline cannot serve instead: the provisional pause legitimately
        // differs from the settled one (60s fallback without header timing;
        // body timing wins over header timing).
        let settled_at = Instant::now();
        let remaining = guard.snapshot().global_pause_remaining;
        let anchored = settled_at
            .checked_add(remaining)
            .unwrap_or(settled_at)
            .saturating_duration_since(header_observed);
        assert!(
            anchored < Duration::from_secs_f64(retry_after + 0.35),
            "cooldown restarted at body: {anchored:?}"
        );
        let (a, b) = callers.await.unwrap();
        a.unwrap();
        b.unwrap();
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        for request in &requests[1..] {
            let elapsed = request.received_at.duration_since(requests[0].received_at);
            assert!(elapsed >= Duration::from_secs_f64(retry_after + 0.20));
        }
        assert_eq!(guard.snapshot().global_pauses_total, 1);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn global_retries_reuse_shared_deadline_across_all_retrying_lanes() {
    for lane in ["get", "kick", "publish"] {
        for signal in ["global-header", "scope-header", "body"] {
            let mut response = ScriptedResponse::json(
                429,
                serde_json::json!({"retry_after": 1.0, "global": signal == "body"}),
            );
            match signal {
                "global-header" => response
                    .headers
                    .push(("X-RateLimit-Global".into(), "true".into())),
                "scope-header" => response
                    .headers
                    .push(("X-RateLimit-Scope".into(), "global".into())),
                _ => {}
            }
            // The default after the 429 must satisfy the publish lane's
            // receipt contract (200 plus the echoed registry); get and kick
            // accept the same 200 body.
            let mock = MockRest::start_with_body_delay(
                vec![response],
                ScriptedResponse::json(200, serde_json::json!([])),
                Duration::from_millis(650),
            )
            .await;
            let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
            let executor = executor(&mock, &guard);
            tokio::time::timeout(Duration::from_secs(4), async {
                match lane {
                    "get" => {
                        executor.get_json("/guilds/2222").await.unwrap();
                    }
                    "kick" => {
                        let result = executor.kick_paced("2222", "3333", "guard test").await;
                        assert_eq!(result.outcome, two_bot_core::KickOutcome::Kicked);
                        assert_eq!(result.attempts, 2);
                    }
                    _ => executor
                        .publish_guild_commands(1111, 2222, &[])
                        .await
                        .unwrap(),
                }
            })
            .await
            .unwrap();
            let requests = mock.requests();
            assert_eq!(requests.len(), 2, "{lane}/{signal}");
            let gap = requests[1]
                .received_at
                .duration_since(requests[0].received_at);
            assert!(
                gap >= Duration::from_millis(1200),
                "{lane}/{signal}: skipped shared deadline"
            );
            assert!(
                gap < Duration::from_millis(1700),
                "{lane}/{signal}: restarted global retry after body: {gap:?}"
            );
            assert_eq!(guard.snapshot().global_pauses_total, 1);
            mock.shutdown().await;
        }
    }
}

#[tokio::test]
async fn guard_rejection_releases_paced_lanes_during_global_cooldown() {
    for status in [401, 403] {
        let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
        let guard = Arc::new(
            RateLimitGuard::new(GuardConfig {
                invalid_request_threshold: 1,
                ..Default::default()
            })
            .unwrap(),
        );
        let executor = executor(&mock, &guard);
        guard.observe_global(None);
        let get = tokio::spawn({
            let executor = executor.clone();
            async move { executor.get_json("/guilds/2222").await }
        });
        let kick = tokio::spawn({
            let executor = executor.clone();
            async move { executor.kick_paced("2222", "3333", "guard test").await }
        });
        let publish = tokio::spawn({
            let executor = executor.clone();
            async move { executor.publish_guild_commands(1111, 2222, &[]).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!get.is_finished());
        assert!(!kick.is_finished());
        assert!(!publish.is_finished());
        guard.observe_status(status, true);
        let (get, kick, publish) = tokio::time::timeout(Duration::from_millis(200), async {
            tokio::join!(get, kick, publish)
        })
        .await
        .expect("guard transition must interrupt a 600-second admission wait");
        let error = if status == 401 {
            GuardError::TokenInvalid
        } else {
            GuardError::CircuitOpen
        };
        assert_eq!(get.unwrap(), Err(error.to_string()));
        let kick = kick.unwrap();
        assert_eq!(kick.detail, error.to_string());
        assert_eq!(kick.attempts, 0);
        assert_eq!(publish.unwrap(), Err(DiscordError::Guard(error)));
        assert_eq!(executor.requests(), 0);
        assert!(mock.requests().is_empty());
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn cooldown_release_preserves_get_and_kick_wire_spacing() {
    for kick_lane in [false, true] {
        let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
        let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
        let executor = executor(&mock, &guard);
        guard.observe_global(Some(0.5));
        let start = Instant::now();
        let call = || async {
            if kick_lane {
                assert_eq!(
                    executor
                        .kick_paced("2222", "3333", "guard test")
                        .await
                        .attempts,
                    1
                );
            } else {
                executor.get_json("/guilds/2222").await.unwrap();
            }
        };
        tokio::join!(call(), call(), call());
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].received_at.duration_since(start) >= Duration::from_millis(700));
        let floor = Duration::from_millis(if kick_lane { 330 } else { 100 });
        for pair in requests.windows(2) {
            assert!(
                pair[1].received_at.duration_since(pair[0].received_at) >= floor,
                "cooldown released queued calls in a burst (kick={kick_lane})"
            );
        }
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn admission_timeout_is_safe_and_never_counts_a_wire_attempt() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
    let executor = executor(&mock, &guard);
    guard.observe_global(Some(6.0));
    let response = pong();
    let (ban_result, callback_result) = tokio::join!(
        ban(&executor),
        executor.answer_interaction(1234, "callback-fixture-token", &response),
    );
    for result in [ban_result, callback_result] {
        let error = result.unwrap_err();
        assert_eq!(error, DiscordError::Guard(GuardError::AdmissionTimeout));
        assert!(error.is_safe_pre_mutation());
    }
    assert_eq!(executor.requests(), 0);
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn stalled_global_bodies_time_out_clean_up_and_recover_without_abort() {
    for lane in ["get", "kick"] {
        for header_timing in [Some("6"), None] {
            let mut response = ScriptedResponse::json(429, serde_json::json!({"retry_after": 60}));
            response
                .headers
                .push(("X-RateLimit-Global".into(), "true".into()));
            if let Some(timing) = header_timing {
                response.headers.push(("Retry-After".into(), timing.into()));
            }
            let mock =
                MockRest::start_with_stalled_bodies(vec![response], ScriptedResponse::status(204))
                    .await;
            let guard = Arc::new(
                RateLimitGuard::new(GuardConfig {
                    window: Duration::from_secs(6),
                    ..Default::default()
                })
                .unwrap(),
            );
            let first = executor(&mock, &guard);
            let second = executor(&mock, &guard);
            let initial = tokio::spawn(async move {
                if lane == "get" {
                    first.get_json("/guilds/2222").await.unwrap();
                } else {
                    let result = first.kick_paced("2222", "3333", "guard test").await;
                    assert_eq!(result.outcome, two_bot_core::KickOutcome::Kicked);
                    assert_eq!(result.attempts, 2);
                }
            });
            tokio::time::timeout(Duration::from_secs(1), async {
                while guard.snapshot().pending_global_responses == 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            let start = Instant::now();
            tokio::time::timeout(Duration::from_millis(5700), async {
                while guard.snapshot().pending_global_responses != 0 {
                    assert_eq!(
                        mock.requests().len(),
                        1,
                        "unresolved body admitted a caller"
                    );
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("body deadline must automatically drop accounting");
            assert!(start.elapsed() >= Duration::from_millis(4800));
            assert!(!guard.snapshot().global_pause_remaining.is_zero());
            assert_eq!(
                mock.requests().len(),
                1,
                "timeout must retain conservative timing"
            );
            let (initial, recovered) = tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(initial, ban(&second))
            })
            .await
            .expect("all REST lanes must recover without abort/restart");
            initial.unwrap();
            recovered.unwrap();
            let requests = mock.requests();
            assert_eq!(requests.len(), 3);
            let floor = Duration::from_millis(if header_timing.is_some() { 6200 } else { 5950 });
            for request in &requests[1..] {
                assert!(request.received_at.duration_since(requests[0].received_at) >= floor);
            }
            assert_eq!(guard.snapshot().pending_global_responses, 0);
            assert_eq!(guard.snapshot().invalid_requests_total, 1);
            mock.shutdown().await;
        }
    }
}

#[tokio::test]
async fn publish_waits_out_long_global_retries_but_keeps_its_wire_deadline() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            429,
            serde_json::json!({"retry_after": 10, "global": true}),
        )],
        ScriptedResponse::json(200, serde_json::json!([])),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
    let first = executor(&mock, &guard);
    tokio::time::timeout(
        Duration::from_secs(14),
        first.publish_guild_commands(1111, 2222, &[]),
    )
    .await
    .unwrap()
    .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let gap = requests[1]
        .received_at
        .duration_since(requests[0].received_at);
    assert!(gap >= Duration::from_millis(10200));
    assert!(gap < Duration::from_secs(13));
    assert_eq!(first.requests(), 2);
    mock.shutdown().await;

    let mock = MockRest::start_with_stalled_bodies(
        vec![],
        ScriptedResponse::json(200, serde_json::json!([])),
    )
    .await;
    let first = executor(&mock, &guard);
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(6),
            first.publish_guild_commands(1111, 2222, &[])
        )
        .await
        .unwrap(),
        Err(DiscordError::Timeout),
    );
    assert_eq!(first.requests(), 1);
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn kick_guard_refusals_count_only_dispatched_http_attempts() {
    for status in [401, 403] {
        let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
        let guard = Arc::new(
            RateLimitGuard::new(GuardConfig {
                invalid_request_threshold: 1,
                ..Default::default()
            })
            .unwrap(),
        );
        guard.observe_status(status, true);
        let first = executor(&mock, &guard);
        let result = first.kick_paced("2222", "3333", "guard test").await;
        assert_eq!(result.outcome, two_bot_core::KickOutcome::Failed);
        assert_eq!(result.attempts, 0);
        assert_eq!(first.requests(), 0);
        assert!(mock.requests().is_empty());
        mock.shutdown().await;
    }
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            429,
            serde_json::json!({"retry_after": 0, "global": true}),
        )],
        ScriptedResponse::status(204),
    )
    .await;
    let guard = Arc::new(
        RateLimitGuard::new(GuardConfig {
            invalid_request_threshold: 1,
            ..Default::default()
        })
        .unwrap(),
    );
    let first = executor(&mock, &guard);
    let result = first.kick_paced("2222", "3333", "guard test").await;
    assert_eq!(result.outcome, two_bot_core::KickOutcome::Failed);
    assert_eq!(result.detail, GuardError::CircuitOpen.to_string());
    assert_eq!(result.attempts, 1);
    assert_eq!(first.requests(), 1);
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

fn pong() -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::Pong,
        data: None,
    }
}

#[tokio::test]
async fn callback_401_is_not_bot_token_failure_and_ack_is_essential() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(401)],
        ScriptedResponse::status(204),
    )
    .await;
    let guard = Arc::new(
        RateLimitGuard::new(GuardConfig {
            invalid_request_threshold: 1,
            ..Default::default()
        })
        .unwrap(),
    );
    let first = executor(&mock, &guard);
    assert!(matches!(
        first
            .answer_interaction(1234, "callback-fixture-token", &pong())
            .await,
        Err(DiscordError::Rejected(_))
    ));
    assert!(!guard.snapshot().token_invalid);
    assert!(guard.snapshot().breaker_open);
    assert_eq!(
        ban(&first).await,
        Err(DiscordError::Guard(GuardError::CircuitOpen))
    );
    first
        .answer_interaction(1234, "callback-fixture-token", &pong())
        .await
        .unwrap();
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn followup_401_is_an_expired_interaction_token_not_a_bot_token_failure() {
    use two_bot_core::router::replies::{InteractionReply, ReplyOperation};

    let mock = MockRest::start(
        vec![ScriptedResponse::status(401), ScriptedResponse::status(401)],
        ScriptedResponse::status(204),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
    let exec = executor(&mock, &guard);
    assert!(exec
        .execute_reply_operation(
            1111,
            7,
            "expired-fixture-token",
            ReplyOperation::Followup(InteractionReply::new("late", true)),
        )
        .await
        .is_err());
    assert!(exec
        .execute_reply_operation(
            1111,
            7,
            "expired-fixture-token",
            ReplyOperation::EditFollowup {
                message_id: 424_242,
                content: "late".to_owned(),
            },
        )
        .await
        .is_err());
    assert!(!guard.snapshot().token_invalid);
    ban(&exec).await.unwrap();
    assert_eq!(mock.requests().len(), 3);
    mock.shutdown().await;
}

// Evidence-read 429 budget: every 429 spends one of the MAX_HTTP_TRIES
// attempts in the two shared GET helpers, so persistent rate limiting
// terminates instead of parking the read until a separate guard refuses it.
// Local 429s sleep retry-after; global 429s ride the shared pause at next
// admission — both still advance the attempt budget.
fn local_429() -> ScriptedResponse {
    ScriptedResponse::json(429, serde_json::json!({"retry_after": 0, "global": false}))
}

fn global_429() -> ScriptedResponse {
    ScriptedResponse::json(429, serde_json::json!({"retry_after": 0, "global": true}))
}

fn read_budget() -> u64 {
    u64::from(two_bot_core::MAX_HTTP_TRIES)
}

#[tokio::test]
async fn observed_persistent_local_429_terminates_absent_within_budget() {
    let mock = MockRest::start(vec![], local_429()).await;
    let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
    let exec = executor(&mock, &guard);
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        exec.get_json_observed("/guilds/2222"),
    )
    .await
    .expect("persistent local 429 must terminate");
    assert_eq!(result.unwrap(), None);
    assert_eq!(mock.requests().len() as u64, read_budget());
    assert_eq!(exec.requests(), read_budget());
    mock.shutdown().await;
}

#[tokio::test]
async fn observed_persistent_global_429_terminates_absent_within_budget() {
    let mock = MockRest::start(vec![], global_429()).await;
    let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
    let exec = executor(&mock, &guard);
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        exec.get_json_observed("/guilds/2222"),
    )
    .await
    .expect("persistent global 429 must terminate");
    assert_eq!(result.unwrap(), None);
    assert_eq!(mock.requests().len() as u64, read_budget());
    assert_eq!(exec.requests(), read_budget());
    mock.shutdown().await;
}

#[tokio::test]
async fn checked_persistent_local_429_reports_evidence_unavailable_within_budget() {
    let mock = MockRest::start(vec![], local_429()).await;
    let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
    let exec = executor(&mock, &guard);
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        exec.get_json_checked("/guilds/2222"),
    )
    .await
    .expect("persistent local 429 must terminate");
    assert_eq!(result, Err("Discord evidence unavailable".to_owned()));
    assert_eq!(mock.requests().len() as u64, read_budget());
    mock.shutdown().await;
}

#[tokio::test]
async fn checked_persistent_global_429_reports_evidence_unavailable_within_budget() {
    let mock = MockRest::start(vec![], global_429()).await;
    let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
    let exec = executor(&mock, &guard);
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        exec.get_json_checked("/guilds/2222"),
    )
    .await
    .expect("persistent global 429 must terminate");
    assert_eq!(result, Err("Discord evidence unavailable".to_owned()));
    assert_eq!(mock.requests().len() as u64, read_budget());
    mock.shutdown().await;
}

#[tokio::test]
async fn observed_mixed_429_and_5xx_exhausts_within_one_budget() {
    let mock = MockRest::start(
        vec![
            local_429(),
            ScriptedResponse::status(500),
            local_429(),
            ScriptedResponse::status(500),
            local_429(),
        ],
        ScriptedResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
    let exec = executor(&mock, &guard);
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        exec.get_json_observed("/guilds/2222"),
    )
    .await
    .expect("mixed 429/5xx must terminate");
    assert_eq!(result.unwrap(), None);
    assert_eq!(
        mock.requests().len() as u64,
        read_budget(),
        "every 429 and 5xx shares one attempt budget"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn checked_mixed_429_and_5xx_exhausts_within_one_budget() {
    let mock = MockRest::start(
        vec![
            global_429(),
            ScriptedResponse::status(503),
            local_429(),
            ScriptedResponse::status(500),
            global_429(),
        ],
        ScriptedResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
    let exec = executor(&mock, &guard);
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        exec.get_json_checked("/guilds/2222"),
    )
    .await
    .expect("mixed 429/5xx must terminate");
    assert_eq!(result, Err("Discord evidence unavailable".to_owned()));
    assert_eq!(
        mock.requests().len() as u64,
        read_budget(),
        "every 429 and 5xx shares one attempt budget"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn evidence_reads_recover_after_a_single_429() {
    for make_429 in [local_429, global_429] {
        let mock = MockRest::start(
            vec![make_429()],
            ScriptedResponse::json(200, serde_json::json!({"ok": true})),
        )
        .await;
        let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
        let exec = executor(&mock, &guard);
        let (data, _) = tokio::time::timeout(
            Duration::from_secs(8),
            exec.get_json_observed("/guilds/2222"),
        )
        .await
        .expect("one 429 then success must terminate")
        .unwrap()
        .expect("recovery carries evidence");
        assert_eq!(data, serde_json::json!({"ok": true}));
        assert_eq!(mock.requests().len(), 2);
        mock.shutdown().await;

        let mock = MockRest::start(
            vec![make_429()],
            ScriptedResponse::json(200, serde_json::json!({"ok": true})),
        )
        .await;
        let guard = Arc::new(RateLimitGuard::new(Default::default()).unwrap());
        let exec = executor(&mock, &guard);
        let data = tokio::time::timeout(
            Duration::from_secs(8),
            exec.get_json_checked("/guilds/2222"),
        )
        .await
        .expect("one 429 then success must terminate")
        .unwrap();
        assert_eq!(data, Some(serde_json::json!({"ok": true})));
        assert_eq!(mock.requests().len(), 2);
        mock.shutdown().await;
    }
}

// Global-pause cap acceptance: a global pause learned from a 429 with no
// usable timing stays fail-closed but is bounded at `min(window, 60 s)`
// instead of a full budget window, and an HTTP-date `Retry-After` is parsed
// as usable timing before that fallback applies. Well-formed global 429s
// keep their exact waits (timing + the 250 ms pad), however long they are.

/// The bound for a global pause with no usable timing.
const CAP: Duration = Duration::from_secs(60);
/// The 429 path's long-standing safety pad over learned timing.
const PAD: Duration = Duration::from_millis(250);

/// Format an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`), the HTTP-date
/// form `Retry-After` may carry instead of delta-seconds.
fn http_date(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Civil calendar from days since the epoch (Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year_at_march = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 {
        year_at_march + 1
    } else {
        year_at_march
    };
    let weekday =
        ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][(days + 4).rem_euclid(7) as usize];
    let month_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(month - 1) as usize];
    format!(
        "{weekday}, {day:02} {month_name} {year} {:02}:{:02}:{:02} GMT",
        tod / 3_600,
        (tod % 3_600) / 60,
        tod % 60
    )
}

#[tokio::test(start_paused = true)]
async fn missing_global_timing_fails_closed_but_is_capped_at_sixty_seconds() {
    // The default window is 600 s; every unusable timing shape must land
    // on the 60 s cap, not on the window, and never on zero (fail-closed).
    for timing in [
        None,
        Some(-1.0),
        Some(f64::NAN),
        Some(f64::INFINITY),
        Some(f64::NEG_INFINITY),
        Some(1e100),
    ] {
        let guard = RateLimitGuard::new(GuardConfig::default()).unwrap();
        guard.observe_global(timing);
        assert_eq!(
            guard.snapshot().global_pause_remaining,
            CAP,
            "timing {timing:?} must fail closed at the cap"
        );
        assert_eq!(guard.snapshot().global_pauses_total, 1);
    }
}

#[tokio::test(start_paused = true)]
async fn unusable_timing_cap_is_the_smaller_of_window_and_sixty_seconds() {
    for (window, expected) in [
        (Duration::from_secs(30), Duration::from_secs(30)),
        (Duration::from_secs(60), Duration::from_secs(60)),
        (Duration::from_secs(90), Duration::from_secs(60)),
        (Duration::from_secs(600), Duration::from_secs(60)),
    ] {
        let guard = RateLimitGuard::new(GuardConfig {
            window,
            ..GuardConfig::default()
        })
        .unwrap();
        guard.observe_global(None);
        assert_eq!(
            guard.snapshot().global_pause_remaining,
            expected,
            "window {window:?} must cap at min(window, 60 s)"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn capped_pause_still_blocks_admission_until_the_cap_expires() {
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).unwrap());
    guard.observe_global(None);
    let waiter = tokio::spawn({
        let guard = guard.clone();
        async move { guard.admit(false).await }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(59)).await;
    assert!(
        !waiter.is_finished(),
        "the capped pause must still fail closed before 60 s"
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    waiter.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn well_formed_global_timing_keeps_its_exact_wait_even_above_the_cap() {
    // The cap bounds only the no-usable-timing fallback: a usable wait is
    // honored exactly (plus the pad), never clamped down to 60 s.
    for (timing, expected) in [
        (120.0, Duration::from_secs(120) + PAD),
        (61.0, Duration::from_secs(61) + PAD),
        (2.0, Duration::from_secs(2) + PAD),
        (0.0, PAD),
    ] {
        let guard = RateLimitGuard::new(GuardConfig::default()).unwrap();
        guard.observe_global(Some(timing));
        assert_eq!(
            guard.snapshot().global_pause_remaining,
            expected,
            "usable timing {timing} must keep its exact wait"
        );
    }
}

#[tokio::test]
async fn loopback_global_429_without_usable_timing_is_capped() {
    // (a) Global scope learned from the body, no timing anywhere.
    // (b) Global scope from the header, a Retry-After that is neither
    // delta-seconds nor an HTTP-date, and no body timing.
    let mut malformed = ScriptedResponse::json(429, serde_json::json!({}));
    malformed
        .headers
        .push(("X-RateLimit-Global".into(), "true".into()));
    malformed
        .headers
        .push(("Retry-After".into(), "not-a-number-or-date".into()));
    for response in [
        ScriptedResponse::json(429, serde_json::json!({"global": true})),
        malformed,
    ] {
        let mock = MockRest::start(vec![response], ScriptedResponse::status(204)).await;
        let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).unwrap());
        let exec = executor(&mock, &guard);
        assert_eq!(ban(&exec).await, Err(DiscordError::RateLimited));
        let remaining = guard.snapshot().global_pause_remaining;
        assert!(
            remaining <= CAP,
            "unusable-timing pause must be capped, got {remaining:?}"
        );
        assert!(
            remaining >= CAP - Duration::from_secs(15),
            "unusable-timing pause must still fail closed near the cap, got {remaining:?}"
        );
        assert_eq!(guard.snapshot().global_pauses_total, 1);
        assert_eq!(guard.snapshot().invalid_requests_total, 1);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn loopback_http_date_retry_after_is_parsed_before_the_fallback() {
    assert_eq!(http_date(UNIX_EPOCH), "Thu, 01 Jan 1970 00:00:00 GMT");
    let target = SystemTime::now() + Duration::from_secs(4);
    let mut response = ScriptedResponse::json(429, serde_json::json!({}));
    response
        .headers
        .push(("X-RateLimit-Global".into(), "true".into()));
    response
        .headers
        .push(("Retry-After".into(), http_date(target)));
    let mock = MockRest::start(vec![response], ScriptedResponse::status(204)).await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).unwrap());
    let exec = executor(&mock, &guard);
    assert_eq!(ban(&exec).await, Err(DiscordError::RateLimited));
    // Pause ~= (date - now) + pad: a few seconds, far below both the 60 s
    // capped fallback and the 600 s window.
    let remaining = guard.snapshot().global_pause_remaining;
    assert!(
        remaining >= Duration::from_secs(1),
        "HTTP-date pause must reflect the date, got {remaining:?}"
    );
    assert!(
        remaining <= Duration::from_secs(5),
        "HTTP-date pause must not fall back to the cap or window, got {remaining:?}"
    );
    assert_eq!(guard.snapshot().global_pauses_total, 1);
    // Admission recovers on the date's schedule, not the fallback's.
    tokio::time::timeout(Duration::from_secs(8), ban(&exec))
        .await
        .expect("the HTTP-date pause must expire in seconds")
        .unwrap();
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn loopback_provisional_pause_without_timing_is_capped_while_body_is_pending() {
    let mut response = ScriptedResponse::json(429, serde_json::json!({"retry_after": 0.8}));
    response
        .headers
        .push(("X-RateLimit-Global".into(), "true".into()));
    let mock = MockRest::start_with_body_delay(
        vec![response],
        ScriptedResponse::status(204),
        Duration::from_millis(600),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).unwrap());
    let exec = executor(&mock, &guard);
    let initial = tokio::spawn(async move { ban(&exec).await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while guard.snapshot().pending_global_responses == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    // Headers alone (no usable timing) hold a provisional pause: capped,
    // not a full window, while the body is still in flight.
    let provisional = guard.snapshot().global_pause_remaining;
    assert!(
        provisional <= CAP,
        "provisional pause must be capped, got {provisional:?}"
    );
    assert!(
        provisional >= CAP - Duration::from_secs(15),
        "provisional pause must fail closed near the cap, got {provisional:?}"
    );
    assert_eq!(initial.await.unwrap(), Err(DiscordError::RateLimited));
    // The body's usable timing then replaces the provisional fallback.
    let settled = guard.snapshot().global_pause_remaining;
    assert!(
        settled <= Duration::from_millis(1_050),
        "body timing must replace the provisional cap, got {settled:?}"
    );
    assert_eq!(guard.snapshot().pending_global_responses, 0);
    assert_eq!(guard.snapshot().global_pauses_total, 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn loopback_dropped_global_body_commits_the_capped_fallback() {
    let mut response = ScriptedResponse::json(429, serde_json::json!({"retry_after": 60}));
    response
        .headers
        .push(("X-RateLimit-Global".into(), "true".into()));
    let mock =
        MockRest::start_with_stalled_bodies(vec![response], ScriptedResponse::status(204)).await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).unwrap());
    let exec = executor(&mock, &guard);
    let initial = tokio::spawn(async move { exec.get_json("/guilds/2222").await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while guard.snapshot().pending_global_responses == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    // The stalled body times out and its accounting is dropped; the pause
    // that commits carries no usable timing, so it is the capped fallback.
    tokio::time::timeout(Duration::from_secs(8), async {
        while guard.snapshot().pending_global_responses != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("body deadline must drop the pending accounting");
    let remaining = guard.snapshot().global_pause_remaining;
    assert!(
        remaining <= CAP,
        "dropped-body pause must be capped, got {remaining:?}"
    );
    assert!(
        remaining >= CAP - Duration::from_secs(15),
        "dropped-body pause must fail closed near the cap, got {remaining:?}"
    );
    initial.abort();
    mock.shutdown().await;
}
