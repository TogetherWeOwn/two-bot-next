//! Explicit outbound policy, applied before Twilight validates message payloads.
//! No current sending action opts into notifications. Never trust caller-supplied
//! `allowed_mentions`, including for interaction message updates.

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
        for embed in embeds {
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
                if embed
                    .get(key)
                    .is_some_and(|value| value.as_str() == Some(""))
                {
                    embed.as_object_mut().expect("embed object").remove(key);
                }
            }
        }
        embeds.retain(|embed| {
            [
                "title",
                "description",
                "author",
                "footer",
                "image",
                "thumbnail",
                "fields",
            ]
            .iter()
            .any(|key| match embed.get(key) {
                Some(Value::Array(values)) => !values.is_empty(),
                Some(Value::Null) | None => false,
                Some(_) => true,
            })
        });
    }
}

fn nonempty(value: &Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty())
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
            serde_json::from_value(value)
                .map_err(|e| DiscordError::Rejected(format!("decode safe response: {e}")))
        }
        _ => Ok(response.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
