//! Explicit outbound policy, applied before Twilight validates message payloads.
//! Never trust caller-supplied `allowed_mentions`, including interaction updates.
//! The fixed ticket-controls template alone opts into a validated opener-id
//! notification AFTER this sanitizer; arbitrary messages never opt in.

use serde_json::{json, Value};
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseType};
use two_bot_core::message_safety as text;

use crate::executor::DiscordError;

pub(crate) fn sanitize_message(body: &mut Value) {
    body["allowed_mentions"] = json!({
        "parse": [], "roles": [], "users": [], "replied_user": false,
    });
    if let Some(content) = body.get_mut("content") {
        if let Some(value) = content.as_str() {
            *content = Value::String(text::content(value));
        }
    }
    if let Some(embeds) = body.get_mut("embeds").and_then(Value::as_array_mut) {
        embeds.truncate(text::EMBED_LIMIT);
        // Discord's 6000-character budget is shared across ALL embeds.
        let mut remaining = text::EMBED_TOTAL_LIMIT;
        for embed in embeds.iter_mut() {
            bound_text(embed, "title", text::EMBED_TITLE_LIMIT, &mut remaining);
            bound_text(
                embed,
                "description",
                text::EMBED_DESCRIPTION_LIMIT,
                &mut remaining,
            );
            if let Some(author) = embed.get_mut("author") {
                bound_text(author, "name", text::EMBED_AUTHOR_LIMIT, &mut remaining);
            }
            if let Some(footer) = embed.get_mut("footer") {
                bound_text(footer, "text", text::EMBED_FOOTER_LIMIT, &mut remaining);
            }
            if let Some(fields) = embed.get_mut("fields").and_then(Value::as_array_mut) {
                fields.truncate(text::EMBED_FIELD_LIMIT);
                for field in fields.iter_mut() {
                    bound_text(field, "name", text::EMBED_FIELD_NAME_LIMIT, &mut remaining);
                    bound_text(
                        field,
                        "value",
                        text::EMBED_FIELD_VALUE_LIMIT,
                        &mut remaining,
                    );
                }
                // Field names and values must be nonempty after budget allocation.
                fields.retain(|field| nonempty(field, "name") && nonempty(field, "value"));
            }
            for (key, child) in [("author", "name"), ("footer", "text")] {
                if embed.get(key).is_some_and(|value| !nonempty(value, child)) {
                    embed.as_object_mut().expect("embed object").remove(key);
                }
            }
            for key in ["title", "description"] {
                if embed.get(key).is_some() && !nonempty(embed, key) {
                    embed.as_object_mut().expect("embed object").remove(key);
                }
            }
        }
        embeds.retain(has_embed_payload);
    }
    // Poll question/answer text is message text too: an obfuscated `@everyone`
    // here must not ride a poll past the content guard. Length is left to
    // Discord's own poll ceilings (an overlong question/answer surfaces as a
    // wire rejection, never a mutation), so only mentions are neutralized.
    if let Some(poll) = body.get_mut("poll") {
        if let Some(question) = poll.get_mut("question") {
            sanitize_poll_media_text(question);
        }
        if let Some(answers) = poll.get_mut("answers").and_then(Value::as_array_mut) {
            for answer in answers.iter_mut() {
                if let Some(media) = answer.get_mut("poll_media") {
                    sanitize_poll_media_text(media);
                }
            }
        }
    }
}

fn sanitize_poll_media_text(media: &mut Value) {
    if let Some(text) = media.get_mut("text") {
        if let Some(raw) = text.as_str() {
            *text = Value::String(text::neutralize_mentions(raw));
        }
    }
}

/// Create-only guard: updates may clear content and deferred callbacks have no
/// message yet. Embeds have already been pruned by `sanitize_message`.
pub(crate) fn validate_create(body: &Value) -> Result<(), DiscordError> {
    let has_text = body
        .get("content")
        .and_then(Value::as_str)
        .is_some_and(text::has_message_text);
    let has_payload = ["embeds", "attachments", "components"].iter().any(|key| {
        body.get(key)
            .and_then(Value::as_array)
            .is_some_and(|values| !values.is_empty())
    }) || body.get("poll").is_some_and(Value::is_object);
    if has_text || has_payload {
        Ok(())
    } else {
        Err(DiscordError::Rejected(
            "message has no sendable payload after sanitization".to_owned(),
        ))
    }
}

fn nonempty(value: &Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}

fn has_text(value: &Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(text::has_message_text)
}

// Placeholder labels are structurally valid; sendability belongs to the whole
// embed, not each label/value. Wholly invisible embeds still supply no payload.
fn has_embed_payload(embed: &Value) -> bool {
    has_text(embed, "title")
        || has_text(embed, "description")
        || embed
            .get("author")
            .is_some_and(|author| has_text(author, "name"))
        || embed
            .get("footer")
            .is_some_and(|footer| has_text(footer, "text"))
        || embed
            .get("fields")
            .and_then(Value::as_array)
            .is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| has_text(field, "name") || has_text(field, "value"))
            })
        || ["image", "thumbnail"]
            .iter()
            .any(|key| embed.get(key).is_some_and(Value::is_object))
}

fn bound_text(value: &mut Value, key: &str, limit: usize, remaining: &mut usize) {
    if let Some(field) = value.get_mut(key) {
        if let Some(raw) = field.as_str() {
            let bounded = text::truncate(&text::neutralize_mentions(raw), limit.min(*remaining));
            *remaining -= text::text_len(&bounded);
            *field = Value::String(bounded);
        }
    }
}

pub(crate) fn interaction_response(
    response: &InteractionResponse,
) -> Result<InteractionResponse, DiscordError> {
    match response.kind {
        InteractionResponseType::ChannelMessageWithSource
        | InteractionResponseType::DeferredChannelMessageWithSource
        | InteractionResponseType::UpdateMessage => {
            let mut value = serde_json::to_value(response)
                .map_err(|e| DiscordError::Rejected(format!("encode response: {e}")))?;
            if !value["data"].is_object() {
                value["data"] = json!({});
            }
            sanitize_message(&mut value["data"]);
            if response.kind == InteractionResponseType::ChannelMessageWithSource {
                validate_create(&value["data"])?;
            }
            let mut safe: InteractionResponse = serde_json::from_value(value)
                .map_err(|e| DiscordError::Rejected(format!("decode safe response: {e}")))?;
            // Attachment.file is #[serde(skip)]: keep the actual upload bytes,
            // not just their JSON metadata, across the sanitized round trip.
            if let Some(data) = safe.data.as_mut() {
                data.attachments = response
                    .data
                    .as_ref()
                    .and_then(|data| data.attachments.clone());
            }
            Ok(safe)
        }
        _ => Ok(response.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_sanitization_preserves_upload_bytes_and_nonmessage_responses() {
        use twilight_model::http::{attachment::Attachment, interaction::InteractionResponseData};
        let response = InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(InteractionResponseData {
                content: Some("@everyone".to_owned()),
                attachments: Some(vec![Attachment::from_bytes(
                    "fixture.txt".to_owned(),
                    b"preserved upload".to_vec(),
                    0,
                )]),
                ..Default::default()
            }),
        };
        let safe = interaction_response(&response).unwrap();
        assert_eq!(
            safe.data.as_ref().unwrap().attachments,
            response.data.as_ref().unwrap().attachments
        );
        for kind in [
            InteractionResponseType::Pong,
            InteractionResponseType::DeferredUpdateMessage,
            InteractionResponseType::ApplicationCommandAutocompleteResult,
            InteractionResponseType::Modal,
        ] {
            let response = InteractionResponse { kind, data: None };
            assert_eq!(interaction_response(&response).unwrap(), response);
        }
        let response = InteractionResponse {
            kind: InteractionResponseType::DeferredChannelMessageWithSource,
            data: None,
        };
        assert!(interaction_response(&response)
            .unwrap()
            .data
            .unwrap()
            .allowed_mentions
            .is_some());
    }

    #[test]
    fn attachment_only_creates_keep_uploads_and_other_payloads_are_accepted() {
        use twilight_model::http::{attachment::Attachment, interaction::InteractionResponseData};
        let attachment =
            Attachment::from_bytes("fixture.txt".to_owned(), b"upload only".to_vec(), 0);
        let response = InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(InteractionResponseData {
                attachments: Some(vec![attachment.clone()]),
                ..Default::default()
            }),
        };
        let safe = interaction_response(&response).unwrap();
        assert_eq!(safe.data.unwrap().attachments, Some(vec![attachment]));
        for body in [
            json!({"components": [{"type": 1, "components": [{"type": 2, "style": 1, "label": "button", "custom_id": "fixture"}]}]}),
            json!({"poll": {"question": {"text": "fixture"}}}),
        ] {
            assert!(validate_create(&body).is_ok());
        }
        assert!(interaction_response(&InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: None,
        })
        .is_err());
    }

    #[test]
    fn poll_question_and_answer_text_cannot_carry_mass_mentions() {
        let mut body = json!({"poll": {
            "question": {"text": "@eve\u{2060}ryone"},
            "answers": [
                {"poll_media": {"text": "@he\u{200b}re"}},
                {"poll_media": {}},
                {"answer_id": 3},
            ],
        }});
        sanitize_message(&mut body);
        assert_eq!(body["poll"]["question"]["text"], "@\u{200b}everyone");
        assert_eq!(
            body["poll"]["answers"][0]["poll_media"]["text"],
            "@\u{200b}here"
        );
        assert!(body["poll"]["answers"][1]["poll_media"]
            .get("text")
            .is_none());
        assert!(body["poll"]["answers"][2].get("poll_media").is_none());
        let safe = body.clone();
        sanitize_message(&mut body);
        assert_eq!(body, safe);
        assert!(validate_create(&body).is_ok());
    }

    #[test]
    fn caller_cannot_opt_into_mass_role_user_or_reply_mentions() {
        let mut body = json!({
            "content": "@everyone @here <@&123> <@456>",
            "allowed_mentions": {"parse": ["everyone", "roles", "users"], "roles": ["123"], "users": ["456"], "replied_user": true},
        });
        sanitize_message(&mut body);
        assert_eq!(
            body["allowed_mentions"],
            json!({"parse": [], "roles": [], "users": [], "replied_user": false})
        );
        assert!(!body["content"].as_str().unwrap().contains("@everyone"));
    }

    #[test]
    fn embeds_bound_each_field_and_the_shared_total_with_multibyte_text() {
        let mut body = json!({"embeds": (0..11).map(|_| json!({
            "title": "😀".repeat(129),
            "description": "界".repeat(4097),
            "author": {"name": "界".repeat(257)},
            "footer": {"text": "界".repeat(2049)},
            "fields": (0..26).map(|_| json!({"name": "界".repeat(257), "value": "😀".repeat(513)})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>()});
        sanitize_message(&mut body);
        let embeds = body["embeds"].as_array().unwrap();
        assert!(embeds.len() <= text::EMBED_LIMIT);
        let mut total = 0;
        for embed in embeds {
            for (pointer, limit) in [
                ("/title", text::EMBED_TITLE_LIMIT),
                ("/description", text::EMBED_DESCRIPTION_LIMIT),
                ("/author/name", text::EMBED_AUTHOR_LIMIT),
                ("/footer/text", text::EMBED_FOOTER_LIMIT),
            ] {
                if let Some(value) = embed.pointer(pointer).and_then(Value::as_str) {
                    let len = text::text_len(value);
                    assert!(len <= limit);
                    total += len;
                }
            }
            if let Some(fields) = embed["fields"].as_array() {
                assert!(fields.len() <= text::EMBED_FIELD_LIMIT);
                for field in fields {
                    for (key, limit) in [
                        ("name", text::EMBED_FIELD_NAME_LIMIT),
                        ("value", text::EMBED_FIELD_VALUE_LIMIT),
                    ] {
                        let value = field[key].as_str().unwrap();
                        assert!(!value.is_empty());
                        assert!(text::text_len(value) <= limit);
                        total += text::text_len(value);
                    }
                }
            }
        }
        assert!(total <= text::EMBED_TOTAL_LIMIT);
        let safe = body.clone();
        sanitize_message(&mut body);
        assert_eq!(body, safe);
    }

    #[test]
    fn placeholder_field_names_consume_budget_without_losing_visible_values() {
        let mut body = json!({"embeds": [{
            "title": "界".repeat(256),
            "description": "界".repeat(4096),
            "footer": {"text": "界".repeat(1646)},
            "fields": [{"name": "\u{200b}", "value": "界".repeat(10)}],
        }]});
        sanitize_message(&mut body);
        let field = &body["embeds"][0]["fields"][0];
        assert_eq!(field["name"], "\u{200b}");
        assert_eq!(field["value"], "界");
        let total = 256
            + 4096
            + 1646
            + text::text_len(field["name"].as_str().unwrap())
            + text::text_len(field["value"].as_str().unwrap());
        assert_eq!(total, text::EMBED_TOTAL_LIMIT);
        assert!(validate_create(&body).is_ok());
        let safe = body.clone();
        sanitize_message(&mut body);
        assert_eq!(body, safe);
    }

    #[test]
    fn embed_count_fields_and_cross_embed_budget_are_not_per_embed() {
        let mut body = json!({"embeds": (0..11).map(|_| json!({
            "title": "界".repeat(256),
            "fields": (0..26).map(|_| json!({"name": "界", "value": "😀"})).collect::<Vec<_>>()
        })).collect::<Vec<_>>()});
        sanitize_message(&mut body);
        let embeds = body["embeds"].as_array().unwrap();
        assert_eq!(embeds.len(), 10);
        assert_eq!(embeds[0]["fields"].as_array().unwrap().len(), 25);

        let mut body = json!({"embeds": [{"description": "界".repeat(4000)}, {"description": "😀".repeat(1500)}]});
        sanitize_message(&mut body);
        assert_eq!(
            text::text_len(body["embeds"][1]["description"].as_str().unwrap()),
            2000
        );
    }
}
