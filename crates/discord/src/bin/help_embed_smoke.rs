//! Render the staged `/help` and `/help <topic>` embeds with the real builders.
//!
//! The staging help-embed smoke (`docs/help-embed-smoke.md`) posts exactly
//! these payloads to the staging test channel through the Discord REST API
//! and reads them back, so the brand assertion (colour, footer text, footer
//! icon URL) runs against Discord-rendered messages instead of unit-test
//! values alone. Nothing here contacts Discord: rendering is pure, and the
//! Python smoke owns every network call.
//!
//! Provenance is explicit in the output. `builder_content` and
//! `builder_embeds` hold what `help_response` / `help_detail_response`
//! produced for the full builtin registry; `posted_embeds` holds what the
//! smoke posts (the builder embeds when the registry is large enough to take
//! the embed branch, otherwise the same text wrapped with `brand_embed` —
//! the exact function the embed branch uses). `meta.builder_embedded`
//! records which path each case took, and the smoke evidence carries it, so
//! a content-only registry never reads as an embed-branch proof.

use std::io::Write;
use std::process::ExitCode;

use twilight_model::channel::message::embed::Embed;
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseData};
use two_bot_core::{CommandDefinition, InteractionRouter, RouterGates};
use two_bot_discord::brand::{brand_embed, BRAND_EMBED_COLOR, BRAND_FOOTER_TEXT, BRAND_ICON_URL};
use two_bot_discord::{help_detail_response, help_response};

/// Staging guild id, so the full builtin set (every gate on) matches what
/// staging would publish before custom rows. The value never leaves this
/// binary: it only selects definitions, and custom rows are out of scope —
/// the smoke proves the brand layer, not registry membership.
const SMOKE_GUILD_ID: u64 = 1545644954272137297;

/// One rendered case: what the builder produced and what the smoke posts.
struct RenderedCase {
    topic: String,
    builder_content: Option<String>,
    builder_embeds: Vec<Embed>,
    posted_embeds: Vec<Embed>,
    builder_embedded: bool,
}

fn full_builtin_gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(SMOKE_GUILD_ID),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: true,
        voice_assistant: false,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    }
}

/// The smoke topic: the builtin with the longest detail text (deterministic;
/// ties break on name). The longest real detail sits closest to the embed
/// branch, so the smoke covers the most representative rendering.
fn smoke_topic(defs: &[CommandDefinition]) -> &CommandDefinition {
    let mut best = &defs[0];
    for def in &defs[1..] {
        let candidate = def.help_detail_text();
        let current = best.help_detail_text();
        if candidate.len() > current.len()
            || (candidate.len() == current.len() && def.name < best.name)
        {
            best = def;
        }
    }
    best
}

/// Split one builder response into its content and embeds.
fn response_parts(response: &InteractionResponse) -> (Option<String>, Vec<Embed>) {
    match response.data.as_ref() {
        None => (None, Vec::new()),
        Some(InteractionResponseData {
            content, embeds, ..
        }) => (content.clone(), embeds.clone().unwrap_or_default()),
    }
}

/// The smoke payload for one builder response: the builder embeds when the
/// response took the embed branch, otherwise the same text wrapped with
/// `brand_embed` (the exact function the embed branch uses).
fn posted_for(content: Option<String>, embeds: Vec<Embed>) -> (Vec<Embed>, bool) {
    if embeds.is_empty() {
        match content {
            None => (Vec::new(), false),
            Some(text) => (
                vec![brand_embed(Embed {
                    author: None,
                    color: None,
                    description: Some(text),
                    fields: Vec::new(),
                    footer: None,
                    image: None,
                    kind: "rich".to_owned(),
                    provider: None,
                    thumbnail: None,
                    timestamp: None,
                    title: None,
                    url: None,
                    video: None,
                })],
                false,
            ),
        }
    } else {
        (embeds, true)
    }
}

fn render_case(topic: String, response: &InteractionResponse) -> RenderedCase {
    let (content, embeds) = response_parts(response);
    let (posted_embeds, builder_embedded) = posted_for(content.clone(), embeds.clone());
    RenderedCase {
        topic,
        builder_content: content,
        builder_embeds: embeds,
        posted_embeds,
        builder_embedded,
    }
}

fn case_json(case: &RenderedCase) -> serde_json::Value {
    serde_json::json!({
        "topic": case.topic,
        "builder_content": case.builder_content,
        "builder_embeds": case.builder_embeds,
        "posted_embeds": case.posted_embeds,
    })
}

fn render_payload(defs: &[CommandDefinition]) -> Result<serde_json::Value, String> {
    if defs.is_empty() {
        return Err("publish_set returned no definitions".to_owned());
    }
    let help = render_case("help".to_owned(), &help_response(defs));
    let topic = smoke_topic(defs);
    let detail = render_case(
        topic.name.clone(),
        &help_detail_response(topic.help_detail_text()),
    );
    if help.posted_embeds.is_empty() || detail.posted_embeds.is_empty() {
        return Err("a smoke case rendered no postable embed".to_owned());
    }
    Ok(serde_json::json!({
        "help": case_json(&help),
        "help_detail": case_json(&detail),
        "brand": {
            "color": BRAND_EMBED_COLOR,
            "footer_text": BRAND_FOOTER_TEXT,
            "footer_icon_url": BRAND_ICON_URL,
        },
        "meta": {
            "help_builder_embedded": help.builder_embedded,
            "help_detail_builder_embedded": detail.builder_embedded,
            "definition_count": defs.len(),
        },
    }))
}

fn run() -> Result<serde_json::Value, String> {
    let router = InteractionRouter::new(full_builtin_gates());
    let defs = router
        .publish_set(&[])
        .map_err(|error| format!("publish_set failed: {error}"))?;
    render_payload(&defs)
}

fn main() -> ExitCode {
    match run() {
        Ok(payload) => {
            let mut stdout = std::io::stdout().lock();
            let mut body = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned());
            body.push('\n');
            if stdout.write_all(body.as_bytes()).is_err() {
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "help-embed-smoke: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_with(content: Option<&str>, embeds: Vec<Embed>) -> InteractionResponse {
        InteractionResponse {
            kind:
                twilight_model::http::interaction::InteractionResponseType::ChannelMessageWithSource,
            data: Some(InteractionResponseData {
                content: content.map(str::to_owned),
                embeds: Some(embeds),
                ..Default::default()
            }),
        }
    }

    fn bare_embed(description: &str) -> Embed {
        Embed {
            author: None,
            color: None,
            description: Some(description.to_owned()),
            fields: Vec::new(),
            footer: None,
            image: None,
            kind: "rich".to_owned(),
            provider: None,
            thumbnail: None,
            timestamp: None,
            title: None,
            url: None,
            video: None,
        }
    }

    #[test]
    fn posted_embeds_pass_builder_embeds_through_untouched() {
        let built = brand_embed(bare_embed("long registry"));
        let (posted, embedded) = posted_for(None, vec![built.clone()]);
        assert!(embedded, "builder embeds must read as the embed branch");
        assert_eq!(posted, vec![built], "builder embeds post verbatim");
    }

    #[test]
    fn posted_embeds_wrap_content_with_the_brand_function() {
        let (posted, embedded) = posted_for(Some("short registry".to_owned()), Vec::new());
        assert!(
            !embedded,
            "content-only responses must not claim the embed branch"
        );
        assert_eq!(posted, vec![brand_embed(bare_embed("short registry"))]);
        let footer = posted[0]
            .footer
            .as_ref()
            .expect("wrapped embed has a footer");
        assert_eq!(posted[0].color, Some(BRAND_EMBED_COLOR));
        assert_eq!(footer.text, BRAND_FOOTER_TEXT);
        assert_eq!(footer.icon_url.as_deref(), Some(BRAND_ICON_URL));
    }

    #[test]
    fn posted_embeds_stay_empty_without_content_or_embeds() {
        let (posted, embedded) = posted_for(None, Vec::new());
        assert!(!embedded);
        assert!(posted.is_empty());
    }

    #[test]
    fn response_parts_split_content_and_embeds() {
        let built = brand_embed(bare_embed("body"));
        let (content, embeds) = response_parts(&response_with(Some("body"), vec![built.clone()]));
        assert_eq!(content.as_deref(), Some("body"));
        assert_eq!(embeds, vec![built]);
    }

    #[test]
    fn response_without_data_splits_empty() {
        let response = InteractionResponse {
            kind:
                twilight_model::http::interaction::InteractionResponseType::ChannelMessageWithSource,
            data: None,
        };
        let (content, embeds) = response_parts(&response);
        assert_eq!(content, None);
        assert!(embeds.is_empty());
    }

    #[test]
    fn smoke_topic_picks_the_longest_detail_deterministically() {
        let mut first = CommandDefinition::new("bbb", "same length");
        first.examples = vec!["example one".to_owned()];
        let mut second = CommandDefinition::new("aaa", "same length");
        second.examples = vec!["example one".to_owned()];
        let defs = vec![first, second];
        assert_eq!(
            defs[0].help_detail_text().len(),
            defs[1].help_detail_text().len(),
            "the tie-break fixture needs equal-length detail"
        );
        assert_eq!(smoke_topic(&defs).name, "aaa", "ties break on name");
    }

    #[test]
    fn empty_registry_is_a_clean_error() {
        let error = render_payload(&[]).expect_err("no definitions cannot render");
        assert_eq!(error, "publish_set returned no definitions");
    }

    #[test]
    fn full_registry_renders_two_postable_cases() {
        let payload = run().expect("the full builtin registry renders");
        for key in ["help", "help_detail"] {
            assert!(
                !payload[key]["posted_embeds"]
                    .as_array()
                    .expect("posted_embeds serializes as an array")
                    .is_empty(),
                "{key} posts at least one embed"
            );
        }
        assert_eq!(payload["brand"]["color"], BRAND_EMBED_COLOR);
        assert_eq!(payload["brand"]["footer_text"], BRAND_FOOTER_TEXT);
        assert_eq!(payload["brand"]["footer_icon_url"], BRAND_ICON_URL);
        assert!(
            payload["meta"]["definition_count"].as_u64().unwrap_or(0) > 0,
            "the registry is never empty"
        );
    }

    #[test]
    fn wrap_matches_the_help_embed_branch_for_long_registries() {
        let router = InteractionRouter::new(full_builtin_gates());
        let mut defs = router.publish_set(&[]).expect("builtins publish");
        // `help_text` renders names, not descriptions, so capacity comes
        // from distinct ungrouped names (the Custom section) until the real
        // builder flips to the embed branch.
        for i in 0..400 {
            defs.push(CommandDefinition::new(
                &format!("synthetic-capacity-filler-{i:03}"),
                "capacity filler",
            ));
            if !response_parts(&help_response(&defs)).1.is_empty() {
                break;
            }
        }
        let response = help_response(&defs);
        let (content, embeds) = response_parts(&response);
        assert!(
            !embeds.is_empty(),
            "the padded registry takes the embed branch"
        );
        let (posted, embedded) = posted_for(content, embeds.clone());
        assert!(embedded);
        assert_eq!(posted, embeds, "the embed branch posts verbatim");
    }
}
