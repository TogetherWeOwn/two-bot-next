//! Payloads for the live staging `/help` embed smoke (offline half).
//!
//! Discord creates interactions only for real users, so no bot token can
//! invoke `/help` and capture its reply. The smoke therefore posts the exact
//! embeds the two help builders produce — [`help_response`] for `/help` and
//! [`help_detail_response`] for `/help <topic>` — as ordinary channel
//! messages and reads them back (scripts/staging_help_embed_smoke.py). This
//! module emits those embeds after the same executor sanitizer the live
//! reply path uses, together with the brand expectations the read-back must
//! match, so the Python half never re-implements builder or brand logic.
//! See docs/staging-help-embed-smoke.md for what this proves and does not.

use serde_json::{json, Value};
use two_bot_core::commands::{
    merge_commands, CommandChoice, CommandDefinition, CommandOption, CommandOptionType,
    CustomCommand, GUILD_COMMAND_LIMIT,
};

use crate::brand::{BRAND_EMBED_COLOR, BRAND_FOOTER_TEXT, BRAND_ICON_URL};
use crate::interactions::{help_detail_response, help_response};

/// Versioned payload schema the Python smoke refuses anything else under.
pub const HELP_EMBED_SMOKE_SCHEMA: &str = "help_embed_smoke.v1";

/// Build the maximal published registry: only a full registry pushes the
/// discovery text past Discord's content limit into the embed path, exactly
/// as the maximal-registry builder test exercises it.
fn maximal_registry() -> Result<Vec<CommandDefinition>, &'static str> {
    let additional = vec![
        two_bot_core::feature_commands::feature_commands(),
        two_bot_core::moderation::moderation_commands(),
    ];
    let builtins = merge_commands(&additional, &[]).map_err(|_| "registry_merge_failed")?;
    if builtins.len() > GUILD_COMMAND_LIMIT {
        return Err("registry_over_limit");
    }
    let custom: Vec<_> = (0..GUILD_COMMAND_LIMIT - builtins.len())
        .map(|i| CustomCommand {
            name: format!("custom_{i:025}"),
            description: "Custom command".to_owned(),
            enabled: true,
        })
        .collect();
    let defs = merge_commands(&additional, &custom).map_err(|_| "registry_merge_failed")?;
    if defs.len() != GUILD_COMMAND_LIMIT {
        return Err("registry_not_maximal");
    }
    Ok(defs)
}

/// Render one wide topic detail through the real detail renderer: the
/// 25-option/25-choice maximum definition is the widest shape the renderer
/// accepts and lands past the content limit in the embed path, as the
/// builder test proves. Real single-command details are short today, so this
/// synthetic definition is how the `/help <topic>` embed path is exercised.
fn wide_detail_text() -> String {
    let options = (0..25)
        .map(|n| {
            CommandOption::new(
                &format!("option{n:02}"),
                &format!("Argument {n:02}"),
                CommandOptionType::String,
            )
            .choices(
                (0..25)
                    .map(|c| CommandChoice {
                        name: format!("Label {c:02} {}", "😀".repeat(40)),
                        value: format!("value_{c:02}"),
                    })
                    .collect(),
            )
        })
        .collect();
    CommandDefinition::new("maximum", "Valid wide definition")
        .options(options)
        .help_detail_text()
}

/// Sanitize one builder response exactly as the executor does and return its
/// single embed, refusing (with a fixed code) any non-embed shape so a
/// builder change that drops the brand fails payload generation in CI
/// instead of silently posting plain content.
fn single_branded_embed(
    response: &twilight_model::http::interaction::InteractionResponse,
    code: &'static str,
) -> Result<Value, &'static str> {
    let safe = crate::message_safety::interaction_response(response).map_err(|_| code)?;
    let wire = serde_json::to_value(safe).map_err(|_| code)?;
    let embeds = wire["data"]["embeds"].as_array().ok_or(code)?;
    let [embed] = embeds.as_slice() else {
        return Err(code);
    };
    if embed["color"].as_u64() != Some(u64::from(BRAND_EMBED_COLOR))
        || embed["footer"]["text"] != BRAND_FOOTER_TEXT
        || embed["footer"]["icon_url"] != BRAND_ICON_URL
        || embed["description"].as_str().is_none_or(str::is_empty)
    {
        return Err(code);
    }
    Ok(embed.clone())
}

/// The two help embeds and the brand triple the live read-back must carry.
///
/// # Errors
/// Returns a fixed machine-readable code when the maximal registry cannot be
/// built or either builder stops producing a single branded embed.
pub fn help_embed_smoke_payloads() -> Result<Value, &'static str> {
    let defs = maximal_registry()?;
    let help = single_branded_embed(&help_response(&defs), "help_embed_missing")?;
    let help_topic = single_branded_embed(
        &help_detail_response(wide_detail_text()),
        "help_topic_embed_missing",
    )?;
    Ok(json!({
        "schema": HELP_EMBED_SMOKE_SCHEMA,
        "expected": {
            "color": BRAND_EMBED_COLOR,
            "footer_text": BRAND_FOOTER_TEXT,
            "icon_url": BRAND_ICON_URL,
        },
        "embeds": {
            "help": help,
            "help_topic": help_topic,
        },
    }))
}

#[cfg(test)]
mod help_embed_smoke_tests {
    use super::*;
    use two_bot_core::message_safety::{text_len, CONTENT_LIMIT, EMBED_DESCRIPTION_LIMIT};

    #[test]
    fn payloads_carry_both_builders_branded_embeds() {
        let payloads = help_embed_smoke_payloads().expect("payloads build");
        assert_eq!(payloads["schema"], HELP_EMBED_SMOKE_SCHEMA);
        assert_eq!(
            payloads["expected"]["color"].as_u64(),
            Some(u64::from(BRAND_EMBED_COLOR))
        );
        assert_eq!(payloads["expected"]["color"], 16_715_346);
        assert_eq!(payloads["expected"]["footer_text"], BRAND_FOOTER_TEXT);
        assert_eq!(payloads["expected"]["icon_url"], BRAND_ICON_URL);
        for key in ["help", "help_topic"] {
            let embed = &payloads["embeds"][key];
            assert_eq!(
                embed["color"].as_u64(),
                Some(u64::from(BRAND_EMBED_COLOR)),
                "{key} embed carries the brand color"
            );
            assert_eq!(embed["footer"]["text"], BRAND_FOOTER_TEXT, "{key} footer");
            assert_eq!(
                embed["footer"]["icon_url"], BRAND_ICON_URL,
                "{key} footer icon"
            );
            let description = embed["description"].as_str().expect("description");
            assert!(
                text_len(description) > CONTENT_LIMIT,
                "{key} exercises the embed path, not plain content"
            );
            assert!(
                text_len(description) <= EMBED_DESCRIPTION_LIMIT,
                "{key} description fits Discord's embed limit"
            );
        }
    }

    #[test]
    fn payload_embeds_match_the_sanitized_builder_output() {
        let defs = maximal_registry().expect("maximal registry");
        let safe = crate::message_safety::interaction_response(&help_response(&defs))
            .expect("sanitized help reply");
        let wire = serde_json::to_value(safe).expect("serializes");
        let payloads = help_embed_smoke_payloads().expect("payloads build");
        assert_eq!(payloads["embeds"]["help"], wire["data"]["embeds"][0]);
    }
}
