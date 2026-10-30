//! Protected adapter regressions: pre-wire contention and the admitted
//! exchange share one deadline; a 200 without a valid receipt is uncertain.
//! Uses the real component editor and retry wrapper against loopback HTTP.
#![cfg(test)]

use super::{executor_for, MockRest, ScriptedResponse};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use two_bot_core::onboarding::PickerKind;
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};
use two_bot_discord::onboarding_messages::picker_components;
use two_bot_discord::{ActionExecutor, DiscordError};

async fn component_receipt(response: ScriptedResponse, retry: bool) -> Result<(), DiscordError> {
    let mock = MockRest::start(vec![response], ScriptedResponse::status(500)).await;
    let executor = executor_for(&mock);
    let components = picker_components(Some(PickerKind::Games), &[], &[]);
    let result = if retry {
        executor
            .edit_interaction_response_with_components_and_blocked_retry(
                1234,
                "synthetic-component-token",
                "Open your picker",
                &components,
            )
            .await
    } else {
        executor
            .edit_interaction_response_with_components(
                1234,
                "synthetic-component-token",
                "Open your picker",
                &components,
            )
            .await
    };
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "an accepted PATCH is never retried");
    assert_eq!(executor.requests(), 1, "one wire attempt on either adapter");
    assert_eq!(requests[0].method, "PATCH");
    assert_eq!(
        requests[0].path,
        "/api/v10/webhooks/1234/synthetic-component-token/messages/@original"
    );
    assert!(requests[0].header("authorization").is_none());
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["content"], "Open your picker");
    assert_eq!(
        body["components"],
        serde_json::to_value(&components).unwrap()
    );
    assert_eq!(
        body["allowed_mentions"],
        json!({"parse":[],"users":[],"roles":[],"replied_user":false})
    );
    mock.shutdown().await;
    result
}

async fn reject_receipt(body: Value, retry: bool) {
    assert_eq!(
        component_receipt(ScriptedResponse::json(200, body.clone()), retry).await,
        Err(DiscordError::Unavailable(
            "invalid mutation receipt id".into()
        )),
        "HTTP 200 is not a receipt: {body} (retry wrapper: {retry})"
    );
}

// Deleting the receipt check at the component adapter makes each negative
// case return Ok instead of the exact uncertain-delivery classification.
#[tokio::test]
async fn reopen_component_direct_rejects_missing_receipt_id() {
    reject_receipt(json!({}), false).await;
}

#[tokio::test]
async fn reopen_component_retry_rejects_missing_receipt_id() {
    reject_receipt(json!({}), true).await;
}

#[tokio::test]
async fn reopen_component_direct_rejects_zero_receipt_id() {
    reject_receipt(json!({"id":"0"}), false).await;
}

#[tokio::test]
async fn reopen_component_retry_rejects_zero_receipt_id() {
    reject_receipt(json!({"id":"0"}), true).await;
}

#[tokio::test]
async fn reopen_component_direct_rejects_malformed_string_receipt_ids() {
    for id in ["", "not-an-id", "-1", "1.5", "18446744073709551616"] {
        reject_receipt(json!({"id":id}), false).await;
    }
}

#[tokio::test]
async fn reopen_component_retry_rejects_malformed_string_receipt_ids() {
    for id in ["", "not-an-id", "-1", "1.5", "18446744073709551616"] {
        reject_receipt(json!({"id":id}), true).await;
    }
}

#[tokio::test]
async fn reopen_component_direct_rejects_non_string_receipt_ids() {
    for id in [json!(99), json!(true), Value::Null, json!({}), json!([])] {
        reject_receipt(json!({"id":id}), false).await;
    }
}

#[tokio::test]
async fn reopen_component_retry_rejects_non_string_receipt_ids() {
    for id in [json!(99), json!(true), Value::Null, json!({}), json!([])] {
        reject_receipt(json!({"id":id}), true).await;
    }
}

#[tokio::test]
async fn reopen_component_direct_accepts_valid_nonzero_receipt_control() {
    assert_eq!(
        component_receipt(ScriptedResponse::json(200, json!({"id":"99"})), false).await,
        Ok(())
    );
}

#[tokio::test]
async fn reopen_component_retry_accepts_valid_nonzero_receipt_control() {
    assert_eq!(
        component_receipt(ScriptedResponse::json(200, json!({"id":"99"})), true).await,
        Ok(())
    );
}

#[tokio::test]
async fn reopen_component_malformed_json_is_uncertain_and_not_retried() {
    for retry in [false, true] {
        let mut response = ScriptedResponse::status(200);
        response.body = b"{".to_vec();
        assert_eq!(
            component_receipt(response, retry).await,
            Err(DiscordError::Unavailable("invalid mutation receipt".into()))
        );
    }
}

const TOKEN: &str = "component-budget-test-token";

/// The external boundary is held until a controlled instant. Only Blocked
/// refusals can be retried; each accepted attempt is counted separately from
/// the refused attempts, and still passes through the real Hyper transport.
#[derive(Debug)]
struct HeldUntil {
    key: TokenKey,
    first_attempt: Mutex<Option<tokio::time::Instant>>,
    hold: Duration,
    blocked: AtomicUsize,
    admitted: AtomicUsize,
}

struct Release;

impl SendCompletion for Release {
    fn complete(
        self: Box<Self>,
        _cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async { Ok(()) })
    }
}

impl SendAdmission for HeldUntil {
    fn token_key(&self) -> &TokenKey {
        &self.key
    }

    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        let now = tokio::time::Instant::now();
        let first = *self.first_attempt.lock().unwrap().get_or_insert(now);
        let held = now.duration_since(first) < self.hold;
        if held {
            self.blocked.fetch_add(1, Ordering::SeqCst);
        } else {
            self.admitted.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async move {
            if held {
                Err(AdmissionError::Blocked)
            } else {
                Ok(AdmissionPermit::new(Box::new(Release)))
            }
        })
    }
}

fn governed_executor(mock: &MockRest, hold: Duration) -> (ActionExecutor, Arc<HeldUntil>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let lane = Arc::new(HeldUntil {
        key: TokenKey::for_bot_token(TOKEN).unwrap(),
        first_attempt: Mutex::new(None),
        hold,
        blocked: AtomicUsize::new(0),
        admitted: AtomicUsize::new(0),
    });
    let executor = ActionExecutor::with_admission(
        TOKEN.into(),
        Some(mock.origin()),
        Arc::clone(&lane) as Arc<dyn SendAdmission>,
    )
    .unwrap();
    (executor, lane)
}

// Ignoring retry_admission_blocked's remaining budget restarts a full wire
// timeout after 2200 ms of refusals and incorrectly accepts a 3200 ms edit.
#[tokio::test]
async fn reopen_component_retry_bounds_contention_and_slow_exchange_together() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!({"id":"99"})).delayed(Duration::from_millis(1000))],
        ScriptedResponse::status(500),
    )
    .await;
    let (executor, lane) = governed_executor(&mock, Duration::from_millis(2200));
    let components = picker_components(Some(PickerKind::Games), &[], &[]);
    let start = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(4),
        executor.edit_interaction_response_with_components_and_blocked_retry(
            1234,
            "synthetic-component-token",
            "Open your picker",
            &components,
        ),
    )
    .await
    .expect("the component adapter must finish within its absolute budget");
    let elapsed = start.elapsed();
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "one accepted send, no uncertain retry");
    let blocked = lane.blocked.load(Ordering::SeqCst);
    let admitted = lane.admitted.load(Ordering::SeqCst);
    assert_eq!(
        executor.requests(),
        (blocked + admitted) as u64,
        "the executor counts pre-wire refusals as well as admitted attempts"
    );
    assert!(blocked > 1, "pre-wire contention was exercised");
    assert_eq!(admitted, 1, "no second admitted attempt");
    assert!(
        elapsed < Duration::from_millis(2850),
        "2200 ms contention plus wire time must fit one 2500 ms budget (with scheduler margin), got {elapsed:?}"
    );
    assert_eq!(
        result,
        Err(DiscordError::Timeout),
        "the delayed accepted edit times out, never a late success"
    );
    assert_eq!(requests[0].method, "PATCH");
    assert!(requests[0].path.ends_with("/messages/@original"));
    mock.shutdown().await;
}

#[tokio::test]
async fn reopen_component_retry_waits_out_short_contention_then_succeeds_control() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!({"id":"99"}))],
        ScriptedResponse::status(500),
    )
    .await;
    let (executor, lane) = governed_executor(&mock, Duration::from_millis(80));
    let components = picker_components(Some(PickerKind::Games), &[], &[]);
    assert_eq!(
        executor
            .edit_interaction_response_with_components_and_blocked_retry(
                1234,
                "synthetic-component-token",
                "Open your picker",
                &components,
            )
            .await,
        Ok(())
    );
    let blocked = lane.blocked.load(Ordering::SeqCst);
    let admitted = lane.admitted.load(Ordering::SeqCst);
    assert!(blocked > 0);
    assert_eq!(admitted, 1);
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(
        executor.requests(),
        (blocked + admitted) as u64,
        "the executor counts pre-wire refusals as well as admitted attempts"
    );
    mock.shutdown().await;
}
