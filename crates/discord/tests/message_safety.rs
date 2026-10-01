//! Outbound safety acceptance against the local REST double, never Discord.

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use twilight_model::channel::message::AllowedMentions;
use twilight_model::http::interaction::{
    InteractionResponse, InteractionResponseData, InteractionResponseType,
};
use two_bot_core::message_safety::{content, render_template, text_len, CONTENT_LIMIT};
use two_bot_discord::{ActionExecutor, ChannelCall, DiscordError};

const INJECTION: &str = "@everyone @here <@&123> <@456>";

fn assert_safe(body: &Value) {
    // Twilight omits empty roles/users and false replied_user when serializing.
    // Require the explicit parse policy and assert every effective field instead.
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    let mentions: AllowedMentions =
        serde_json::from_value(body["allowed_mentions"].clone()).unwrap();
    assert_eq!(mentions, AllowedMentions::default());
    if let Some(text) = body["content"].as_str() {
        assert!(text_len(text) <= CONTENT_LIMIT);
        assert!(!text.contains("@everyone"));
        assert!(!text.contains("@here"));
    }
}

// Exhaustive over the executor action enum: adding a new channel action forces
// its message-sending classification (and corresponding wire test) to be updated.
fn sends_message(call: &ChannelCall) -> bool {
    match call {
        ChannelCall::PostMessage { .. } => true,
        ChannelCall::Purge { .. }
        | ChannelCall::Slowmode { .. }
        | ChannelCall::PutOverwrite { .. }
        | ChannelCall::DeleteOverwrite { .. }
        | ChannelCall::DeleteMessage { .. } => false,
    }
}

#[tokio::test]
async fn every_message_sending_action_carries_explicit_mention_policy() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "99"}))).await;
    let exec = ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap();
    // Direct creation, numeric nonce, audit string nonce, and every sending
    // ChannelCall variant, interaction UpdateMessage, and original-response edit.
    exec.post_message("4444", INJECTION, None).await.unwrap();
    exec.post_message("4444", INJECTION, Some(42))
        .await
        .unwrap();
    for nonce in [None, Some("43".to_owned()), Some("oa_fixture".to_owned())] {
        let call = ChannelCall::PostMessage {
            channel_id: "4444".to_owned(),
            content: INJECTION.to_owned(),
            nonce,
        };
        assert!(sends_message(&call));
        exec.execute_channel(&call).await.unwrap();
    }
    for kind in [
        InteractionResponseType::ChannelMessageWithSource,
        InteractionResponseType::DeferredChannelMessageWithSource,
        InteractionResponseType::UpdateMessage,
    ] {
        let response = InteractionResponse {
            kind,
            data: Some(InteractionResponseData {
                content: Some(INJECTION.to_owned()),
                allowed_mentions: Some(
                    serde_json::from_value(json!({
                        "parse": ["everyone", "roles", "users"],
                        "roles": ["123"], "users": ["456"], "replied_user": true,
                    }))
                    .unwrap(),
                ),
                ..Default::default()
            }),
        };
        exec.answer_interaction(5555, "fixture-interaction-token", &response)
            .await
            .unwrap();
    }
    exec.edit_interaction_response(5555, "fixture-interaction-token", INJECTION)
        .await
        .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 9);
    for request in requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_safe(if body.get("type").is_some() {
            &body["data"]
        } else {
            &body
        });
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn template_sticky_and_schedule_bodies_cannot_inject_mentions() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "99"}))).await;
    let exec = ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap();
    for injection in ["@everyone", "@here", "<@&123>", "@eve\u{200b}ryone"] {
        let rendered = render_template(
            "{username}: {body}",
            &[("username", injection), ("body", injection)],
        );
        // Sticky validation remains strict; rendering belongs at send time.
        two_bot_core::validate_body(injection).unwrap();
        for body in [rendered, injection.to_owned(), injection.to_owned()] {
            exec.post_message("4444", &body, None).await.unwrap();
        }
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 12);
    for request in requests {
        assert_safe(&serde_json::from_slice::<Value>(&request.body).unwrap());
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn interaction_limits_are_applied_before_twilight_validation() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let exec = ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap();
    let response: InteractionResponse = serde_json::from_value(json!({
        "type": 7,
        "data": {
            "content": "😀".repeat(1001),
            "embeds": [{"type": "rich", "title": "界".repeat(257), "description": "😀".repeat(2049)}],
        },
    })).unwrap();
    exec.answer_interaction(5555, "fixture-interaction-token", &response)
        .await
        .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_safe(&body["data"]);
    assert_eq!(text_len(body["data"]["content"].as_str().unwrap()), 2000);
    assert_eq!(
        text_len(body["data"]["embeds"][0]["title"].as_str().unwrap()),
        256
    );
    assert_eq!(
        text_len(body["data"]["embeds"][0]["description"].as_str().unwrap()),
        4096
    );
    // Source data is not mutated, so callers can retain the original input.
    assert_eq!(
        text_len(response.data.unwrap().content.as_deref().unwrap()),
        2002
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn original_response_edits_bound_text_after_neutralization() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "99"}))).await;
    let exec = ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap();
    let input = "😀".repeat(995) + "@everyone😀";
    exec.edit_interaction_response(5555, "fixture-interaction-token", &input)
        .await
        .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "PATCH");
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_safe(&body);
    assert_eq!(body["content"], "😀".repeat(995) + "@\u{200b}everyone");
    assert_eq!(text_len(body["content"].as_str().unwrap()), CONTENT_LIMIT);
    mock.shutdown().await;
}

#[tokio::test]
async fn unsendable_creates_are_rejected_after_rendering_without_http() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "99"}))).await;
    let exec = ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap();
    for text in [
        "",
        "\u{200b}",
        "\u{200c}",
        "\u{feff}",
        " \u{200b}\u{200c}\u{feff}\n",
    ] {
        for body in [
            text.to_owned(),
            render_template("{body}", &[("body", text)]),
        ] {
            assert!(two_bot_core::validate_body(&body).is_err());
            assert!(matches!(
                exec.post_message("4444", &body, None).await,
                Err(DiscordError::Rejected(_))
            ));
            let call = ChannelCall::PostMessage {
                channel_id: "4444".to_owned(),
                content: body.clone(),
                nonce: Some("oa_fixture".to_owned()),
            };
            assert!(matches!(
                exec.execute_channel(&call).await,
                Err(DiscordError::Rejected(_))
            ));
            let response = InteractionResponse {
                kind: InteractionResponseType::ChannelMessageWithSource,
                data: Some(InteractionResponseData {
                    content: Some(body),
                    ..Default::default()
                }),
            };
            assert!(matches!(
                exec.answer_interaction(5555, "fixture-interaction-token", &response)
                    .await,
                Err(DiscordError::Rejected(_))
            ));
        }
    }
    // Validate the effective payload, not the visible suffix truncated away.
    let response: InteractionResponse = serde_json::from_value(json!({
        "type": 4, "data": {"content": "\u{200b}".repeat(1999) + "😀", "embeds": [{"type": "rich", "description": "\u{feff}"}]},
    })).unwrap();
    assert!(matches!(
        exec.answer_interaction(5555, "fixture-interaction-token", &response)
            .await,
        Err(DiscordError::Rejected(_))
    ));
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn embeds_deferred_callbacks_and_clearing_edits_remain_valid() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "99"}))).await;
    let exec = ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap();
    for value in [
        json!({"type": 4, "data": {"embeds": [{"type": "rich", "description": "embed only"}]}}),
        json!({"type": 5}),
        json!({"type": 7, "data": {"content": "", "embeds": []}}),
    ] {
        let response: InteractionResponse = serde_json::from_value(value).unwrap();
        exec.answer_interaction(5555, "fixture-interaction-token", &response)
            .await
            .unwrap();
    }
    exec.edit_interaction_response(5555, "fixture-interaction-token", "")
        .await
        .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_safe(if body.get("type").is_some() {
            &body["data"]
        } else {
            &body
        });
    }
    let cleared: Value = serde_json::from_slice(&requests[3].body).unwrap();
    assert_eq!(cleared["content"], "");
    mock.shutdown().await;
}

#[tokio::test]
async fn placeholder_embed_labels_preserve_visible_values_on_create_and_update() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let exec = ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap();
    for kind in [4, 7] {
        let response: InteractionResponse = serde_json::from_value(json!({
            "type": kind,
            "data": {"embeds": [{"type": "rich", "fields": [
                {"name": "\u{200b}", "value": "Important result", "inline": false},
                {"name": "@everyone", "value": "\u{200b}", "inline": false},
            ]}]},
        }))
        .unwrap();
        exec.answer_interaction(5555, "fixture-interaction-token", &response)
            .await
            .unwrap();
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    for (request, kind) in requests.iter().zip([4, 7]) {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["type"], kind);
        assert_safe(&body["data"]);
        let fields = body["data"]["embeds"][0]["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0]["name"], "\u{200b}");
        assert_eq!(fields[0]["value"], "Important result");
        assert_eq!(fields[1]["name"], "@\u{200b}everyone");
        assert_eq!(fields[1]["value"], "\u{200b}");
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn invisible_only_embed_fields_do_not_supply_a_create_payload() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let exec = ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap();
    let response: InteractionResponse = serde_json::from_value(json!({
        "type": 4,
        "data": {"embeds": [{"type": "rich", "fields": [
            {"name": "\u{200b}", "value": "\u{200b}", "inline": false},
        ]}]},
    }))
    .unwrap();
    assert!(matches!(
        exec.answer_interaction(5555, "fixture-interaction-token", &response)
            .await,
        Err(DiscordError::Rejected(_))
    ));
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn meaningful_unicode_is_preserved_at_create_and_edit_boundaries() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "99"}))).await;
    let exec = ActionExecutor::with_proxy("fixture-token".to_owned(), Some(mock.origin())).unwrap();
    let text = "می\u{200c}روم 👩\u{200d}💻 @eve\u{200c}ryone @he\u{feff}re";
    exec.post_message("4444", text, None).await.unwrap();
    exec.edit_interaction_response(5555, "fixture-interaction-token", text)
        .await
        .unwrap();
    let response: InteractionResponse = serde_json::from_value(json!({
        "type": 4, "data": {"content": text, "embeds": [{"type": "rich", "description": text}]},
    }))
    .unwrap();
    exec.answer_interaction(5555, "fixture-interaction-token", &response)
        .await
        .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    for request in requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let data = if body.get("type").is_some() {
            &body["data"]
        } else {
            &body
        };
        assert_safe(data);
        assert_eq!(
            data["content"],
            "می\u{200c}روم 👩\u{200d}💻 @\u{200b}everyone @\u{200b}here"
        );
    }
    mock.shutdown().await;
}

#[test]
fn template_replacements_are_sanitized_after_expansion_not_reinterpreted() {
    assert_eq!(
        render_template(
            "{username} {server}",
            &[("username", "{server}"), ("server", "TWO")]
        ),
        "{server} TWO"
    );
    assert_eq!(
        render_template("{unknown} {unfinished", &[]),
        "{unknown} {unfinished"
    );
    let long = "😀".repeat(1001);
    assert_eq!(
        render_template("{body}", &[("body", &long)]),
        content(&long)
    );
    assert_eq!(
        text_len(&render_template("{body}", &[("body", &long)])),
        CONTENT_LIMIT
    );
}
