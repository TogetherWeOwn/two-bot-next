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
