// Member-notify DM delivery over a scripted REST double: template wire JSON,
// refusal mapping, and receipt handling. No gateway, no database.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Map, Value};
use two_bot_core::internal_actions::{ErrorCode, PlaceOfferSendRequest, ReminderSendRequest};
use two_bot_discord::executor::notify::NotifyOutcome;
use two_bot_discord::ActionExecutor;

const USER: &str = "100000000000000002";
const CHANNEL: &str = "100000000000000010";
const MESSAGE: &str = "100000000000000011";
const EVENT_KEY: &str = "launch:2026";
const EVENT_NAME: &str = "Launch Night";

fn reminder_body() -> Map<String, Value> {
    json!({
        "discord_id": USER,
        "event_key": EVENT_KEY,
        "kind": "starting_soon",
        "starts_at": "2026-11-04T18:00:00Z",
        "event_name": EVENT_NAME,
    })
    .as_object()
    .unwrap()
    .clone()
}

fn offer_body() -> Map<String, Value> {
    json!({
        "discord_id": USER,
        "event_key": EVENT_KEY,
        "event_name": EVENT_NAME,
        "expires_at": "2026-11-04T18:00:00+01:00",
    })
    .as_object()
    .unwrap()
    .clone()
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("fixture-bot-token".into(), Some(mock.origin())).unwrap()
}

fn opened_channel() -> ScriptedResponse {
    ScriptedResponse::json(201, json!({"id": CHANNEL}))
}

fn posted_message() -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"id": MESSAGE}))
}

#[tokio::test]
async fn reminder_post_carries_template_text_with_no_pings() {
    let mock = MockRest::start(
        vec![opened_channel(), posted_message()],
        ScriptedResponse::status(500),
    )
    .await;
    let reminder = ReminderSendRequest::validate(&reminder_body()).unwrap();
    let actual = executor(&mock).send_reminder_dm(&reminder).await.unwrap();
    assert_eq!(
        actual,
        NotifyOutcome::Sent {
            message_id: MESSAGE.into()
        }
    );
    let calls = mock.requests();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].method, "POST");
    assert_eq!(calls[0].path, "/api/v10/users/@me/channels");
    assert_eq!(
        serde_json::from_slice::<Value>(&calls[0].body).unwrap(),
        json!({"recipient_id": USER})
    );
    assert_eq!(
        calls[0].header("authorization"),
        Some("Bot fixture-bot-token")
    );
    assert_eq!(calls[1].method, "POST");
    assert_eq!(
        calls[1].path,
        format!("/api/v10/channels/{CHANNEL}/messages")
    );
    let posted = serde_json::from_slice::<Value>(&calls[1].body).unwrap();
    assert_eq!(
        posted["content"],
        json!("Reminder: Launch Night starts at 2026-11-04T18:00:00.000Z. Details: /e/launch:2026")
    );
    assert_eq!(posted["allowed_mentions"]["parse"], json!([]));
    assert_eq!(
        calls[1].header("authorization"),
        Some("Bot fixture-bot-token")
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn offer_post_carries_template_text_with_no_pings() {
    let mock = MockRest::start(
        vec![opened_channel(), posted_message()],
        ScriptedResponse::status(500),
    )
    .await;
    let offer = PlaceOfferSendRequest::validate(&offer_body()).unwrap();
    let actual = executor(&mock).send_place_offer_dm(&offer).await.unwrap();
    assert_eq!(
        actual,
        NotifyOutcome::Sent {
            message_id: MESSAGE.into()
        }
    );
    let calls = mock.requests();
    assert_eq!(calls.len(), 2);
    let posted = serde_json::from_slice::<Value>(&calls[1].body).unwrap();
    assert_eq!(
        posted["content"],
        json!("A place opened for Launch Night. Claim it before 2026-11-04T17:00:00.000Z: /e/launch:2026")
    );
    assert_eq!(posted["allowed_mentions"]["parse"], json!([]));
    mock.shutdown().await;
}

#[tokio::test]
async fn channel_open_refusal_is_dm_unavailable_without_a_second_call() {
    // Closed DMs, a blocked bot, or no shared guild surface here as a
    // definitive refusal: a 200 outcome, never a second wire attempt.
    for status in [400, 401, 403, 404, 405, 413, 415, 422] {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(status, json!({"message": "no"}))],
            posted_message(),
        )
        .await;
        let reminder = ReminderSendRequest::validate(&reminder_body()).unwrap();
        let actual = executor(&mock).send_reminder_dm(&reminder).await.unwrap();
        assert_eq!(actual, NotifyOutcome::DmUnavailable, "{status}");
        assert_eq!(mock.requests().len(), 1, "{status}");
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn message_post_refusal_is_dm_unavailable() {
    let mock = MockRest::start(
        vec![
            opened_channel(),
            ScriptedResponse::json(403, json!({"message": "no"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let offer = PlaceOfferSendRequest::validate(&offer_body()).unwrap();
    let actual = executor(&mock).send_place_offer_dm(&offer).await.unwrap();
    assert_eq!(actual, NotifyOutcome::DmUnavailable);
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn rate_limit_is_retryable_with_retry_after_and_no_burst() {
    let mock = MockRest::start(
        vec![ScriptedResponse::rate_limited(0.5, "7")],
        posted_message(),
    )
    .await;
    let reminder = ReminderSendRequest::validate(&reminder_body()).unwrap();
    let error = executor(&mock)
        .send_reminder_dm(&reminder)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::RateLimited);
    assert_eq!(error.retry_after_secs, Some(7));
    assert!(error.code.retryable());
    assert!(!format!("{error:?}").contains(EVENT_NAME));
    // A single attempt: no bursting past the 429.
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn server_error_is_unknown_never_dm_unavailable() {
    let mock = MockRest::start(
        vec![opened_channel(), ScriptedResponse::status(503)],
        ScriptedResponse::status(500),
    )
    .await;
    let offer = PlaceOfferSendRequest::validate(&offer_body()).unwrap();
    let error = executor(&mock)
        .send_place_offer_dm(&offer)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::DiscordUnavailable);
    assert!(error.code.retryable());
    assert!(!format!("{error:?}").contains(EVENT_NAME));
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn redirect_is_unknown_without_forwarding_credentials() {
    let target = MockRest::start(vec![], ScriptedResponse::status(201)).await;
    let mut redirect = ScriptedResponse::json(307, json!({"message": "elsewhere"}));
    redirect.headers.push(("location".into(), target.origin()));
    let mock = MockRest::start(vec![redirect], ScriptedResponse::status(201)).await;
    let reminder = ReminderSendRequest::validate(&reminder_body()).unwrap();
    let error = executor(&mock)
        .send_reminder_dm(&reminder)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::DiscordUnavailable);
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(target.requests().len(), 0);
    mock.shutdown().await;
    target.shutdown().await;
}

#[tokio::test]
async fn unreadable_receipt_is_unknown_not_a_phantom_send() {
    // Discord answered 200 but the receipt cannot name a message: claiming
    // `sent` would invent a message id, claiming `dm_unavailable` would deny
    // a delivery that may have happened. Unknown, so the caller reconciles.
    for receipt in [json!({"nope": true}), json!({"id": "not-an-id"})] {
        let mock = MockRest::start(
            vec![opened_channel(), ScriptedResponse::json(200, receipt)],
            ScriptedResponse::status(500),
        )
        .await;
        let reminder = ReminderSendRequest::validate(&reminder_body()).unwrap();
        let error = executor(&mock)
            .send_reminder_dm(&reminder)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::DiscordUnavailable);
        assert_eq!(error.log_reason, "dm_receipt_unreadable");
        assert_eq!(mock.requests().len(), 2);
        mock.shutdown().await;
    }
}
