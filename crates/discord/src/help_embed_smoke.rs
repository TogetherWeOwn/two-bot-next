//! Payloads for the live staging `/help` embed smoke (offline half).
//!
//! Discord creates interactions only for real users. The smoke therefore
//! posts the sanitized discovery and detail embeds as ordinary messages
//! without callback flags or buttons, then reads them back. See
//! docs/staging-help-embed-smoke.md for what this proves and does not.

use serde_json::{json, Value};
use twilight_model::http::interaction::InteractionResponse;
use two_bot_core::commands::{
    merge_commands, CommandChoice, CommandDefinition, CommandOption, CommandOptionType,
    CustomCommand, GUILD_COMMAND_LIMIT,
};

use crate::brand::{brand_footer_for, EmbedKind, BRAND_ICON_URL};
use crate::help_navigation::{DiscoveryPlan, HELP_SURFACE, HELP_TITLE};
use crate::interactions::help_detail_response;

/// Versioned payload schema the Python smoke refuses anything else under.
pub const HELP_EMBED_SMOKE_SCHEMA: &str = "help_embed_smoke.v2";

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

/// The widest supported option and choice counts exercise detail overflow.
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

/// Refuse a builder output that has lost its title, branding or description.
fn single_branded_embed(
    response: &InteractionResponse,
    footer_text: &str,
    code: &'static str,
) -> Result<Value, &'static str> {
    let safe = crate::message_safety::interaction_response(response).map_err(|_| code)?;
    let wire = serde_json::to_value(safe).map_err(|_| code)?;
    let embeds = wire["data"]["embeds"].as_array().ok_or(code)?;
    let [embed] = embeds.as_slice() else {
        return Err(code);
    };
    if embed["title"] != HELP_TITLE
        || embed["color"].as_u64() != Some(u64::from(EmbedKind::Default.color()))
        || embed["footer"]["text"] != footer_text
        || embed["footer"]["icon_url"] != BRAND_ICON_URL
        || embed["description"].as_str().is_none_or(str::is_empty)
    {
        return Err(code);
    }
    Ok(embed.clone())
}

/// The help embeds and per-surface branding the live read-back must carry.
///
/// # Errors
/// Returns a fixed code if the maximal registry cannot be rendered across
/// multiple pages or either reply stops producing a single branded embed.
pub fn help_embed_smoke_payloads() -> Result<Value, &'static str> {
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
        &help_detail_response(wide_detail_text()),
        &topic_footer,
        "help_topic_embed_missing",
    )?;
    let expected = |footer_text| {
        json!({
            "color": EmbedKind::Default.color(),
            "footer_text": footer_text,
            "icon_url": BRAND_ICON_URL,
        })
    };
    Ok(json!({
        "schema": HELP_EMBED_SMOKE_SCHEMA,
        "expected": {"help": expected(help_footer), "help_topic": expected(topic_footer)},
        "embeds": {"help": help, "help_topic": help_topic},
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
            assert_eq!(embed["color"], expected["color"]);
            assert_eq!(embed["footer"]["text"], expected["footer_text"]);
            assert_eq!(embed["footer"]["icon_url"], expected["icon_url"]);
            let description = embed["description"].as_str().expect("description");
            assert!(text_len(description) <= EMBED_DESCRIPTION_LIMIT);
        }
        let help = &payloads["embeds"]["help"];
        assert!(help["footer"]["text"]
            .as_str()
            .expect("discovery footer")
            .starts_with("Together We Own · Help · Page 1 of "));
        assert!(!help["fields"].as_array().expect("group fields").is_empty());
        let detail = &payloads["embeds"]["help_topic"];
        assert_eq!(detail["footer"]["text"], "Together We Own · Help");
        assert!(text_len(detail["description"].as_str().expect("description")) > CONTENT_LIMIT);
    }

    #[test]
    fn payload_embeds_match_the_sanitized_builder_output() {
        let defs = maximal_registry().expect("maximal registry");
        let first = DiscoveryPlan::new(&defs, 1, 1)
            .expect("plan")
            .first(1, 600)
            .expect("first page");
        let detail = help_detail_response(wide_detail_text());
        let payloads = help_embed_smoke_payloads().expect("payloads build");
        for (key, reply) in [("help", first), ("help_topic", detail)] {
            let safe =
                crate::message_safety::interaction_response(&reply).expect("sanitized reply");
            let wire = serde_json::to_value(safe).expect("serializes");
            assert_eq!(payloads["embeds"][key], wire["data"]["embeds"][0]);
        }
    }
}
