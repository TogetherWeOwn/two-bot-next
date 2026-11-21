//! Payloads for the staging branded embed smoke (offline half).
//!
//! Discord creates interactions only for real users, so no bot token can
//! invoke `/help` and capture its reply. The smoke therefore posts the exact
//! embeds from the Help, announcement, Echo, RSVP and Voice builders,
//! as ordinary channel messages and reads them back (scripts/staging_help_embed_smoke.py).
//! Help replies go through the executor sanitizer before embed extraction.
//! Each row supplies kit expectations independently of its rendered embed.
//! See docs/staging-help-embed-smoke.md for what this proves and does not.

use serde_json::{json, Value};
use two_bot_core::commands::{
    merge_commands, CommandChoice, CommandDefinition, CommandOption, CommandOptionType,
    CustomCommand, GUILD_COMMAND_LIMIT,
};

use crate::brand::{brand_footer_for, ephemeral_reply_embed, EmbedKind, BRAND_ICON_URL};
use crate::help_navigation::{DiscoveryPlan, HelpRecovery, HELP_SURFACE, HELP_TITLE};
use crate::interactions::branded_help_detail_response;
use crate::internal_actions::announcement_embed;
use crate::rsvp::rsvp_reply_embed;

/// Versioned payload schema the Python smoke refuses anything else under.
pub const HELP_EMBED_SMOKE_SCHEMA: &str = "help_embed_smoke.v3";

/// Build a maximal published registry to exercise the multi-page footer.
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
/// accepts and exercises the overflow path.
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
/// single embed, refusing any non-embed shape with a fixed code.
fn sanitized_embed(
    response: &twilight_model::http::interaction::InteractionResponse,
    code: &'static str,
) -> Result<Value, &'static str> {
    let safe = crate::message_safety::interaction_response(response).map_err(|_| code)?;
    let wire = serde_json::to_value(safe).map_err(|_| code)?;
    let embeds = wire["data"]["embeds"].as_array().ok_or(code)?;
    let [embed] = embeds.as_slice() else {
        return Err(code);
    };
    Ok(embed.clone())
}

fn single_branded_embed(
    response: &twilight_model::http::interaction::InteractionResponse,
    footer_text: &str,
    code: &'static str,
) -> Result<Value, &'static str> {
    let embed = sanitized_embed(response, code)?;
    if embed["color"].as_u64() != Some(u64::from(EmbedKind::Default.color()))
        || embed["title"] != HELP_TITLE
        || embed["footer"]["text"] != footer_text
        || embed["footer"]["icon_url"] != BRAND_ICON_URL
        || embed["description"].as_str().is_none_or(str::is_empty)
    {
        return Err(code);
    }
    Ok(embed)
}

fn expected_brand(kind: EmbedKind, footer_text: String, icon_url: Option<&str>) -> Value {
    json!({"color": kind.color(), "footer_text": footer_text, "icon_url": icon_url})
}

/// The rendered embeds and per-row branding the live read-back must carry.
///
/// # Errors
/// Returns a fixed code if the maximal registry cannot be rendered across
/// multiple pages or either Help reply stops producing a single branded embed.
pub fn help_embed_smoke_payloads() -> Result<Value, &'static str> {
    use EmbedKind::{Danger, Default, Echo, Ok as Saved, Warn};

    let defs = maximal_registry()?;
    let plan = DiscoveryPlan::new(&defs, 1, 1).map_err(|_| "help_plan_failed")?;
    let count = plan.page_count();
    if count < 2 {
        return Err("help_plan_not_paginated");
    }
    let first = plan.first(1, 600).ok_or("help_first_page_missing")?;
    let help_footer = brand_footer_for(&format!("{HELP_SURFACE} · Page 1 of {count}")).text;
    let topic_footer = brand_footer_for(HELP_SURFACE).text;
    let help = single_branded_embed(&first, &help_footer, "help_embed_missing")?;
    let help_topic = single_branded_embed(
        &branded_help_detail_response(wide_detail_text()),
        &topic_footer,
        "help_topic_embed_missing",
    )?;
    let recovery = |kind: HelpRecovery| sanitized_embed(&kind.response(), "help_recovery_missing");
    let icon = Some(BRAND_ICON_URL);
    let public = |kind, surface| expected_brand(kind, brand_footer_for(surface).text, icon);
    let private = |kind, surface| expected_brand(kind, brand_footer_for(surface).text, None);
    let voice = |kind, text: &str| ephemeral_reply_embed(kind, "Voice", text.to_owned());
    Ok(json!({
        "schema": HELP_EMBED_SMOKE_SCHEMA,
        "expected": {
            "help": expected_brand(Default, help_footer, icon),
            "help_topic": expected_brand(Default, topic_footer, icon),
            "help_warn": public(Warn, HELP_SURFACE),
            "help_error": public(Danger, HELP_SURFACE),
            "announcement": public(Default, "Announcements"),
            "echo": public(Echo, "Echo"),
            "rsvp_ok": private(Saved, "Events"),
            "rsvp_error": private(Danger, "Events"),
            "voice_ok": private(Saved, "Voice"),
            "voice_warn": private(Warn, "Voice"),
            "voice_error": private(Danger, "Voice"),
        },
        "embeds": {
            "help": help,
            "help_topic": help_topic,
            "help_warn": recovery(HelpRecovery::Expired)?,
            "help_error": recovery(HelpRecovery::CannotBuild)?,
            "announcement": announcement_embed("Smoke announcement\nAn offline example."),
            "echo": announcement_embed("Echo · Smoke event\nAn offline example."),
            "rsvp_ok": rsvp_reply_embed(Ok("RSVP saved for the example event.".to_owned())),
            "rsvp_error": rsvp_reply_embed(Err("The example event is closed.".to_owned())),
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
            let embed = &payloads["embeds"][key];
            let expected = &payloads["expected"][key];
            assert_eq!(
                expected["color"],
                crate::brand::brand_tokens::BRAND_DISCORD_EMBED_DEFAULT
            );
            assert_eq!(expected["icon_url"], BRAND_ICON_URL);
            assert_eq!(embed["title"], HELP_TITLE);
            assert_eq!(
                embed["color"].as_u64(),
                Some(u64::from(EmbedKind::Default.color())),
                "{key} embed carries the brand color"
            );
            assert_eq!(embed["footer"]["text"], expected["footer_text"]);
            assert_eq!(
                embed["footer"]["icon_url"], BRAND_ICON_URL,
                "{key} footer icon"
            );
            let description = embed["description"].as_str().expect("description");
            assert!(
                text_len(description) <= EMBED_DESCRIPTION_LIMIT,
                "{key} description fits Discord's embed limit"
            );
        }
        let help = &payloads["embeds"]["help"];
        assert!(help["footer"]["text"]
            .as_str()
            .expect("discovery footer")
            .starts_with("Together We Own · Help · Page 1 of "));
        assert!(!help["fields"].as_array().expect("group fields").is_empty());
        let detail = &payloads["embeds"]["help_topic"];
        assert_eq!(detail["footer"]["text"], "Together We Own · Help");
        let description = detail["description"].as_str().expect("description");
        assert!(text_len(description) > CONTENT_LIMIT);
        assert!(
            description.ends_with("\n…(detail truncated; /help lists every confirmed command.)\n")
        );
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
                "help_warn",
                BRAND_DISCORD_EMBED_WARN,
                "Help",
                Some(BRAND_ICON_URL),
            ),
            (
                "help_error",
                BRAND_DISCORD_EMBED_DANGER,
                "Help",
                Some(BRAND_ICON_URL),
            ),
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
        let first = DiscoveryPlan::new(&defs, 1, 1)
            .expect("plan")
            .first(1, 600)
            .expect("first page");
        let detail = branded_help_detail_response(wide_detail_text());
        let payloads = help_embed_smoke_payloads().expect("payloads build");
        for (key, reply) in [("help", first), ("help_topic", detail)] {
            let safe =
                crate::message_safety::interaction_response(&reply).expect("sanitized reply");
            let wire = serde_json::to_value(safe).expect("serializes");
            assert_eq!(payloads["embeds"][key], wire["data"]["embeds"][0]);
        }
    }
}
