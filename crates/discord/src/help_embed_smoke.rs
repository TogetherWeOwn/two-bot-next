//! Renderer for the staging branded embed smoke (offline half).
//!
//! The runner posts embeds from the Help, announcement/Echo, RSVP and Voice
//! builders as ordinary test-channel messages. This does not invoke any
//! commands, publish a real event or test ephemeral interaction delivery.
//! Help replies go through the executor sanitizer before their embeds are
//! extracted. Expectations select the kit kind and footer independently of
//! the rendered payload.
//! See docs/staging-help-embed-smoke.md for what this proves and does not.

use serde_json::{json, Value};
use two_bot_core::commands::{
    merge_commands, CommandChoice, CommandDefinition, CommandOption, CommandOptionType,
    CustomCommand, GUILD_COMMAND_LIMIT,
};

use crate::brand::{ephemeral_reply_embed, EmbedKind, BRAND_FOOTER_TEXT, BRAND_ICON_URL};
use crate::interactions::{help_detail_response, help_response};
use crate::internal_actions::announcement_embed;
use crate::rsvp::rsvp_reply_embed;

/// Versioned payload schema the Python smoke refuses anything else under.
pub const HELP_EMBED_SMOKE_SCHEMA: &str = "help_embed_smoke.v3";

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
    if embed["color"].as_u64() != Some(u64::from(EmbedKind::Default.color()))
        || embed["footer"]["text"] != BRAND_FOOTER_TEXT
        || embed["footer"]["icon_url"] != BRAND_ICON_URL
        || embed["description"].as_str().is_none_or(str::is_empty)
    {
        return Err(code);
    }
    Ok(embed.clone())
}

fn expected_brand(kind: EmbedKind, surface: &str, icon_url: Option<&str>) -> Value {
    // Select the expected kit kind and footer independently of the built embed.
    let footer_text = if surface.is_empty() {
        BRAND_FOOTER_TEXT.to_owned()
    } else {
        format!("{BRAND_FOOTER_TEXT} · {surface}")
    };
    json!({"color": kind.color(), "footer_text": footer_text, "icon_url": icon_url})
}

/// The rendered embeds and per-builder expectations for the live read-back.
///
/// # Errors
/// Returns a fixed machine-readable code when the registry or help replies
/// cannot be rendered.
pub fn help_embed_smoke_payloads() -> Result<Value, &'static str> {
    use EmbedKind::{Danger, Default, Echo, Ok as Saved, Warn};

    let voice = |kind, text: &str| ephemeral_reply_embed(kind, "Voice", text.to_owned());
    let defs = maximal_registry()?;
    let help = single_branded_embed(&help_response(&defs), "help_embed_missing")?;
    let help_topic = single_branded_embed(
        &help_detail_response(wide_detail_text()),
        "help_topic_embed_missing",
    )?;
    let announcement = announcement_embed("Smoke announcement\nAn offline example.");
    let echo = announcement_embed("Echo · Smoke event\nAn offline example.");
    let rsvp_ok = rsvp_reply_embed(Ok("RSVP saved for the example event.".to_owned()));
    let rsvp_error = rsvp_reply_embed(Err("The example event is closed.".to_owned()));
    let icon = Some(BRAND_ICON_URL);
    Ok(json!({
        "schema": HELP_EMBED_SMOKE_SCHEMA,
        "expected": {
            "help": expected_brand(Default, "", icon),
            "help_topic": expected_brand(Default, "", icon),
            "announcement": expected_brand(Default, "Announcements", icon),
            "echo": expected_brand(Echo, "Echo", icon),
            "rsvp_ok": expected_brand(Saved, "Events", None),
            "rsvp_error": expected_brand(Danger, "Events", None),
            "voice_ok": expected_brand(Saved, "Voice", None),
            "voice_warn": expected_brand(Warn, "Voice", None),
            "voice_error": expected_brand(Danger, "Voice", None),
        },
        "embeds": {
            "help": help,
            "help_topic": help_topic,
            "announcement": announcement,
            "echo": echo,
            "rsvp_ok": rsvp_ok,
            "rsvp_error": rsvp_error,
            "voice_ok": voice(Saved, "The example room limit was updated."),
            "voice_warn": voice(Warn, "The example room is still updating."),
            "voice_error": voice(Danger, "The example room could not be updated."),
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
        for key in ["help", "help_topic"] {
            let expected = &payloads["expected"][key];
            assert_eq!(
                expected["color"].as_u64(),
                Some(u64::from(EmbedKind::Default.color()))
            );
            assert_eq!(
                expected["color"],
                crate::brand::brand_tokens::BRAND_DISCORD_EMBED_DEFAULT
            );
            assert_eq!(expected["footer_text"], BRAND_FOOTER_TEXT);
            assert_eq!(expected["icon_url"], BRAND_ICON_URL);
            let embed = &payloads["embeds"][key];
            assert_eq!(
                embed["color"].as_u64(),
                Some(u64::from(EmbedKind::Default.color())),
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
    fn additional_rows_match_their_kit_contracts() {
        use crate::brand::brand_tokens::{
            BRAND_DISCORD_EMBED_DANGER, BRAND_DISCORD_EMBED_DEFAULT, BRAND_DISCORD_EMBED_ECHO,
            BRAND_DISCORD_EMBED_OK, BRAND_DISCORD_EMBED_WARN,
        };

        let payloads = help_embed_smoke_payloads().expect("payloads build");
        for (key, color, surface, icon) in [
            (
                "announcement",
                BRAND_DISCORD_EMBED_DEFAULT,
                "Announcements",
                Some(BRAND_ICON_URL),
            ),
            (
                "echo",
                BRAND_DISCORD_EMBED_ECHO,
                "Echo",
                Some(BRAND_ICON_URL),
            ),
            ("rsvp_ok", BRAND_DISCORD_EMBED_OK, "Events", None),
            ("rsvp_error", BRAND_DISCORD_EMBED_DANGER, "Events", None),
            ("voice_ok", BRAND_DISCORD_EMBED_OK, "Voice", None),
            ("voice_warn", BRAND_DISCORD_EMBED_WARN, "Voice", None),
            ("voice_error", BRAND_DISCORD_EMBED_DANGER, "Voice", None),
        ] {
            let footer = format!("Together We Own · {surface}");
            let expected = &payloads["expected"][key];
            let embed = &payloads["embeds"][key];
            assert_eq!(expected["color"], color, "{key} expected color");
            assert_eq!(embed["color"], color, "{key} rendered color");
            assert_eq!(expected["footer_text"], footer, "{key} expected footer");
            assert_eq!(embed["footer"]["text"], footer, "{key} rendered footer");
            assert_eq!(expected["icon_url"], json!(icon), "{key} expected icon");
            assert_eq!(
                embed["footer"]["icon_url"].as_str(),
                icon,
                "{key} rendered icon"
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
