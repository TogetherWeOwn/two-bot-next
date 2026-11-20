use super::*;
use crate::{brand::BRAND_ICON_URL, message_safety::interaction_response};
use serde_json::{json, Value};
use two_bot_core::help::cursor::LIFETIME_SECONDS;
use two_bot_core::message_safety::neutralize_mentions;

const ACTOR: u64 = 42;
const NOW: u64 = 1000;
const EXPIRY: u64 = NOW + LIFETIME_SECONDS;

fn wire(response: &InteractionResponse) -> Value {
    let safe = interaction_response(response).expect("safe callback");
    let wire = serde_json::to_value(safe).expect("wire callback");
    assert_eq!(wire["type"], 4);
    assert_eq!(wire["data"]["flags"], MessageFlags::EPHEMERAL.bits());
    assert_eq!(wire["data"]["allowed_mentions"], json!({"parse": []}));
    assert_eq!(wire["data"]["embeds"].as_array().expect("embeds").len(), 1);
    assert!(wire["data"]["embeds"][0].get("flags").is_none());
    assert_eq!(
        wire["data"]["embeds"][0]["footer"]["icon_url"],
        BRAND_ICON_URL
    );
    wire
}

#[test]
fn rows_and_footers_match_every_position() {
    type Buttons = &'static [(&'static str, u8, usize)];
    let cases: &[(usize, usize, Buttons)] = &[
        (1, 1, &[]),
        (2, 1, &[("Go to page 2", 1, 2)]),
        (2, 2, &[("Back to page 1", 2, 1)]),
        (4, 1, &[("Go to page 2", 1, 2)]),
        (4, 2, &[("Back to page 1", 2, 1), ("Go to page 3", 1, 3)]),
        (
            4,
            3,
            &[
                ("Back to page 1", 2, 1),
                ("Back to page 2", 2, 2),
                ("Go to page 4", 1, 4),
            ],
        ),
        (4, 4, &[("Back to page 1", 2, 1), ("Back to page 3", 2, 3)]),
    ];
    for &(count, current, expected) in cases {
        let pages: Vec<_> = (1..=count)
            .map(|page| Page {
                description: format!("Fixture page {page}"),
                fields: vec![],
            })
            .collect();
        let revision = Revision::for_pages(1, 2, RENDER_VERSION, &pages);
        let plan = DiscoveryPlan { pages, revision };
        let response = plan.render(current - 1, ACTOR, EXPIRY).expect("page");
        let wire = wire(&response);
        let data = &wire["data"];
        let embed = &data["embeds"][0];
        let footer = if count == 1 {
            "Together We Own · Help".to_owned()
        } else {
            format!("Together We Own · Help · Page {current} of {count}")
        };
        assert_eq!(embed["footer"]["text"], footer);
        assert_eq!(embed["title"], HELP_TITLE);
        assert_eq!(embed["color"], EmbedKind::Default.color());
        assert!(embed["timestamp"].is_null());
        if expected.is_empty() {
            assert!(data.get("components").is_none());
            continue;
        }
        let rows = data["components"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["type"], 1);
        let buttons = rows[0]["components"].as_array().expect("buttons");
        assert_eq!(buttons.len(), expected.len());
        for (button, &(label, style, target)) in buttons.iter().zip(expected) {
            assert_eq!(button["label"], label);
            assert_eq!(button["style"], style);
            assert_ne!(button["disabled"], true);
            let id = button["custom_id"].as_str().expect("custom id");
            assert!(id.len() <= 100 && id.ends_with(&format!(":{target}")));
            let cursor = Cursor::parse(id).expect("cursor");
            assert_eq!(
                cursor.target(ACTOR, NOW, revision, count),
                Ok((target - 1, EXPIRY))
            );
        }
    }
}

#[test]
fn following_primary_reaches_every_safe_command_with_the_original_expiry() {
    // Max-width names make the aggregate exceed one embed after packing.
    let defs: Vec<_> = (0..100)
        .map(|index| {
            CommandDefinition::new(
                &format!("{index:02}@here{}", "😀".repeat(25)),
                "Synthetic custom command",
            )
        })
        .collect();
    let plan = DiscoveryPlan::new(&defs, 1, 2).expect("bounded plan");
    assert!(plan.pages.len() > 1);
    let mut response = plan.first(ACTOR, EXPIRY).expect("initial page");
    let mut actual = Vec::new();
    for (index, page) in plan.pages.iter().enumerate() {
        let wire = wire(&response);
        let data = &wire["data"];
        let embed = &data["embeds"][0];
        assert_eq!(embed["description"], "**Server commands** (100 live)\nThis is the last confirmed command list. Your picker hides gated commands you cannot use.");
        let fields = embed["fields"].as_array().expect("fields");
        assert!(fields.len() <= 6);
        assert_eq!(fields.len(), page.fields.len());
        for (field, (name, value)) in fields.iter().zip(&page.fields) {
            assert_eq!(field["inline"], false);
            assert_eq!(field["name"], *name);
            assert_eq!(field["value"], *value);
            actual.extend(value.split(" · ").map(str::to_owned));
        }
        let buttons = data["components"][0]["components"]
            .as_array()
            .expect("controls");
        let forward = buttons.iter().find(|button| button["style"] == 1);
        if index + 1 == plan.pages.len() {
            assert!(forward.is_none());
            break;
        }
        let id = forward.expect("reachable next page")["custom_id"]
            .as_str()
            .expect("id");
        let cursor = Cursor::parse(id).expect("parse");
        assert_eq!(
            cursor.target(ACTOR, NOW, plan.revision, plan.pages.len()),
            Ok((index + 1, EXPIRY))
        );
        response = plan
            .navigate(cursor, ACTOR, NOW + 1)
            .expect("private next page");
    }
    let expected: Vec<_> = defs
        .iter()
        .map(|def| format!("/{}", neutralize_mentions(&def.name)))
        .collect();
    assert_eq!(actual, expected);
}

fn click_fixture() -> Value {
    let pages: Vec<_> = (1..=2)
        .map(|index| Page {
            description: format!("Page {index}"),
            fields: vec![],
        })
        .collect();
    let revision = Revision::for_pages(1, 2, RENDER_VERSION, &pages);
    let plan = DiscoveryPlan { pages, revision };
    let response = wire(&plan.first(ACTOR, EXPIRY).expect("first page"));
    let rows = &response["data"]["components"];
    let actor = json!({"id": ACTOR.to_string(), "username": "actor", "discriminator": "0"});
    json!({
        "id": "100", "type": 3, "application_id": "2", "guild_id": "1",
        "authorizing_integration_owners": {"0": "1"}, "entitlements": [], "token": "fixture",
        "data": {"component_type": 2, "custom_id": rows[0]["components"][0]["custom_id"], "values": []},
        "member": {"deaf": false, "mute": false, "flags": 0, "roles": [], "user": actor},
        "message": {
            "id": "98", "channel_id": "3", "guild_id": "1", "application_id": "2", "type": 20,
            "author": {"id": "2", "username": "bot", "discriminator": "0", "bot": true},
            "flags": response["data"]["flags"], "components": rows, "embeds": response["data"]["embeds"],
            "interaction_metadata": {
                "id": "99", "type": 2, "user": actor, "authorizing_integration_owners": {"0": "1"},
                "original_response_message_id": null, "interacted_message_id": null
            },
            "attachments": [], "content": "", "mention_everyone": false, "mention_roles": [],
            "mentions": [], "pinned": false, "timestamp": "2026-11-20T04:00:00.000000+00:00", "tts": false
        }
    })
}

#[test]
fn initial_and_descendant_help_messages_belong_to_the_clicking_actor() {
    let mut payload = click_fixture();
    let expected =
        Cursor::parse(payload["data"]["custom_id"].as_str().expect("button id")).expect("cursor");
    for kind in [2, 3] {
        payload["message"]["interaction_metadata"]["type"] = json!(kind);
        if kind == 3 {
            payload["message"]["interaction_metadata"]["interacted_message_id"] = json!("97");
            // The attached message may omit scoping fields; the pinned bot
            // author and the interaction itself still establish scope.
            payload["message"]["application_id"] = Value::Null;
            payload["message"]["guild_id"] = Value::Null;
        }
        let interaction = serde_json::from_value(payload.clone()).expect("click");
        assert_eq!(source_cursor(&interaction, 2, 2), Some((expected, ACTOR)));
    }
}

#[test]
fn help_cursor_does_not_authorize_a_different_actor_or_source() {
    for (path, value) in [
        ("/application_id", json!("5")),
        ("/guild_id", Value::Null),
        ("/member/user", Value::Null),
        ("/message", Value::Null),
        ("/message/application_id", json!("5")),
        ("/message/guild_id", json!("5")),
        ("/message/author/bot", json!(false)),
        ("/message/author/id", json!("5")),
        ("/message/flags", json!(0)),
        ("/message/interaction_metadata", Value::Null),
        ("/message/interaction_metadata/user/id", json!("5")),
        ("/message/interaction_metadata/type", json!(5)),
        (
            "/message/interaction_metadata/original_response_message_id",
            json!("5"),
        ),
        ("/message/components", json!([])),
        ("/data/component_type", json!(3)),
        ("/data/values", json!(["unexpected"])),
        ("/data/custom_id", json!("h1:malformed")),
    ] {
        let mut payload = click_fixture();
        *payload.pointer_mut(path).expect("fixture path") = value;
        let interaction = serde_json::from_value(payload).expect("click");
        assert_eq!(source_cursor(&interaction, 2, 2), None, "{path}");
    }
    let mut payload = click_fixture();
    // Even matching Discord actor metadata cannot authorize a cursor for
    // someone else. Both the source and cursor must agree with the click.
    payload["member"]["user"]["id"] = json!("5");
    payload["message"]["interaction_metadata"]["user"]["id"] = json!("5");
    let interaction = serde_json::from_value(payload).expect("click");
    assert_eq!(source_cursor(&interaction, 2, 2), None);
}

#[test]
fn private_recovery_replies_have_exact_copy_and_no_controls() {
    for (recovery, kind, title, description) in [
        (HelpRecovery::from(CursorError::Stale), EmbedKind::Warn, "The command list changed", "This page is from an older list, so the bot didn't open it. Run `/help` to see the current one."),
        (HelpRecovery::from(CursorError::Expired), EmbedKind::Warn, "This help page has expired", "Help pages last 10 minutes. Run `/help` to start again."),
        (HelpRecovery::from(CursorError::Invalid), EmbedKind::Warn, "That help button doesn't work here", "Run `/help` to get your own list."),
        (HelpRecovery::Busy { from_button: true }, EmbedKind::Warn, "The bot is catching up", "It couldn't open that page just now. Press the same button again in a moment."),
        (HelpRecovery::Busy { from_button: false }, EmbedKind::Warn, "The bot is catching up", "It couldn't open help just now. Run `/help` again in a moment."),
        (HelpRecovery::CannotBuild, EmbedKind::Danger, "Help isn't available right now", "The bot couldn't build the command list. Try `/help` again in a few minutes."),
    ] {
        let wire = wire(&recovery.response());
        let data = &wire["data"];
        let embed = &data["embeds"][0];
        assert!(data.get("components").is_none());
        assert_eq!(embed["color"], kind.color());
        assert_eq!(embed["title"], title);
        assert_eq!(embed["description"], description);
        assert_eq!(embed["footer"]["text"], "Together We Own · Help");
    }
}
