//! Global guard acceptance against the existing local Discord REST double.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
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
    guard.observe_global(Some(0.4));
    let started = Instant::now();
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

async fn ban(executor: &ActionExecutor) -> Result<(), DiscordError> {
    executor.ban("2222", "3333", "guard test").await
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
        let callers = tokio::spawn(async move { tokio::join!(ban(&second), ban(&second)) });
        tokio::time::sleep(body_delay - Duration::from_millis(100)).await;
        assert!(!initial.is_finished(), "body must still be pending");
        assert_eq!(
            mock.requests().len(),
            1,
            "known header restriction bypassed"
        );
        assert_eq!(initial.await.unwrap(), Err(DiscordError::RateLimited));
        let (a, b) = callers.await.unwrap();
        a.unwrap();
        b.unwrap();
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        for request in &requests[1..] {
            let elapsed = request.received_at.duration_since(requests[0].received_at);
            assert!(elapsed >= Duration::from_secs_f64(retry_after + 0.20));
            assert!(
                elapsed < Duration::from_secs_f64(retry_after + 0.55),
                "cooldown restarted at body"
            );
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
            let mock = MockRest::start_with_body_delay(
                vec![response],
                ScriptedResponse::status(204),
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
        ScriptedResponse::status(204),
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
