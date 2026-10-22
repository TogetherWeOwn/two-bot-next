#![cfg(test)]

use super::*;

#[tokio::test]
async fn ballot_message_capture_gets_original_without_bot_auth_then_edits_by_snowflake() {
    let mock = Mock::start(vec![
        response(200, json!({"id": "901", "channel_id": "900"})),
        response(200, json!({})),
    ])
    .await;
    let message = mock
        .api
        .interaction_message(Id::new(1), "synthetic-ballot-token")
        .await
        .unwrap();
    assert_eq!(
        message,
        MessageRef {
            channel_id: 900,
            message_id: 901
        }
    );
    let mut components = join_buttons();
    let twilight_model::channel::message::Component::ActionRow(row) = &mut components[0] else {
        panic!("button row")
    };
    for button in &mut row.components {
        let twilight_model::channel::message::Component::Button(button) = button else {
            panic!("button")
        };
        button.disabled = true;
    }
    mock.api
        .edit_component_message(message, "Vote expired.", &components)
        .await
        .unwrap();
    let requests = mock.state.recorded.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, Method::GET);
    assert_eq!(
        requests[0].path,
        "/api/v10/webhooks/1/synthetic-ballot-token/messages/@original"
    );
    assert!(!requests[0].bot_authenticated);
    assert_eq!(requests[1].method, Method::PATCH);
    assert_eq!(requests[1].path, "/api/v10/channels/900/messages/901");
    assert!(requests[1].bot_authenticated);
    assert_eq!(requests[1].body["content"], "Vote expired.");
    assert_eq!(requests[1].body["allowed_mentions"]["parse"], json!([]));
    assert!(requests[1].body["components"][0]["components"]
        .as_array()
        .unwrap()
        .iter()
        .all(|button| button["disabled"] == true));
}

#[tokio::test]
async fn ballot_message_capture_refuses_invalid_ids_and_never_retries_a_failed_read() {
    for (body, error) in [
        (json!({}), RoomHttpError::InvalidRequest),
        (
            json!({"id":"901", "channel_id":"0"}),
            RoomHttpError::InvalidRequest,
        ),
        (
            json!({"id":"0", "channel_id":"900"}),
            RoomHttpError::UnknownOutcome,
        ),
        (
            json!({"id":"bad", "channel_id":"900"}),
            RoomHttpError::UnknownOutcome,
        ),
    ] {
        let mock = Mock::start(vec![response(200, body)]).await;
        assert_eq!(
            mock.api
                .interaction_message(Id::new(1), "synthetic-ballot-token")
                .await,
            Err(error)
        );
        assert_eq!(mock.state.recorded.lock().unwrap().len(), 1);
    }
    for (status, error) in [
        (404, RoomHttpError::NotFound),
        (503, RoomHttpError::UnknownOutcome),
    ] {
        let mock = Mock::start(vec![response(status, json!({}))]).await;
        assert_eq!(
            mock.api
                .interaction_message(Id::new(1), "synthetic-ballot-token")
                .await,
            Err(error)
        );
        assert_eq!(mock.state.recorded.lock().unwrap().len(), 1);
        assert!(!mock.api.unauthorized.load(Ordering::Relaxed));
    }
}
