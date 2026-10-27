//! Onboarding wire rendering; all I/O belongs to the shared ActionExecutor.

use twilight_model::channel::message::{
    component::{ActionRow, SelectMenu, SelectMenuOption, SelectMenuType},
    AllowedMentions, Component, EmojiReactionType, MessageFlags,
};
use twilight_model::http::interaction::{
    InteractionResponse, InteractionResponseData, InteractionResponseType,
};
use two_bot_core::onboarding::{
    preselected_game_keys, MentionPolicy, PickerKind, SessionPick, GAME_PICKS,
    GAME_PREVIEW_SELECT_ID, GAME_SELECT_ID, SESSION_SELECT_ID,
};

pub fn allowed_mentions(policy: MentionPolicy) -> AllowedMentions {
    AllowedMentions {
        parse: vec![],
        replied_user: false,
        roles: vec![],
        users: match policy {
            MentionPolicy::None => vec![],
            MentionPolicy::Member(id) => vec![twilight_model::id::Id::new(id)],
        },
    }
}

pub fn defer_ephemeral() -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::DeferredChannelMessageWithSource,
        data: Some(InteractionResponseData {
            flags: Some(MessageFlags::EPHEMERAL),
            allowed_mentions: Some(allowed_mentions(MentionPolicy::None)),
            ..Default::default()
        }),
    }
}

fn option(
    label: &str,
    key: &str,
    description: &str,
    emoji: &str,
    default: bool,
) -> SelectMenuOption {
    SelectMenuOption {
        default,
        description: Some(description.to_owned()),
        emoji: Some(EmojiReactionType::Unicode {
            name: emoji.to_owned(),
        }),
        label: label.to_owned(),
        value: key.to_owned(),
    }
}

fn select_row(
    custom_id: &str,
    placeholder: &str,
    min_values: u8,
    max_values: u8,
    options: Vec<SelectMenuOption>,
) -> Component {
    Component::ActionRow(ActionRow {
        id: None,
        components: vec![Component::SelectMenu(SelectMenu {
            id: None,
            channel_types: None,
            custom_id: custom_id.to_owned(),
            default_values: None,
            disabled: false,
            kind: SelectMenuType::Text,
            max_values: Some(max_values),
            min_values: Some(min_values),
            options: Some(options),
            placeholder: Some(placeholder.to_owned()),
            required: None,
        })],
    })
}

/// Source: <https://docs.rs/twilight-model/0.17.1/twilight_model/channel/message/component/struct.SelectMenu.html>
pub fn picker_components(
    picker: Option<PickerKind>,
    member_roles: &[&str],
    session_picks: &[SessionPick],
) -> Vec<Component> {
    match picker {
        None => vec![],
        Some(PickerKind::Games) => {
            let selected = preselected_game_keys(member_roles);
            let apply = select_row(
                GAME_SELECT_ID,
                "What do you play?",
                0,
                10,
                GAME_PICKS
                    .iter()
                    .map(|p| {
                        option(
                            p.label,
                            p.key,
                            p.description,
                            p.emoji,
                            selected.iter().any(|k| k == p.key),
                        )
                    })
                    .collect(),
            );
            // Self-only no-effect preview alongside the apply route. No
            // defaults: the member proposes an answer, they do not confirm
            // current roles. Min 0 keeps a deliberately empty proposal
            // distinct from unknown-only input; max covers the full catalog.
            let preview = select_row(
                GAME_PREVIEW_SELECT_ID,
                "Preview proposed changes…",
                0,
                10,
                GAME_PICKS
                    .iter()
                    .map(|p| option(p.label, p.key, p.description, p.emoji, false))
                    .collect(),
            );
            vec![apply, preview]
        }
        Some(PickerKind::Session) => vec![select_row(
            SESSION_SELECT_ID,
            "What do you want to do right now?",
            1,
            2,
            session_picks
                .iter()
                .map(|p| option(p.label, p.key, p.description, p.emoji, false))
                .collect(),
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use two_bot_core::onboarding::build_session_picks;

    #[test]
    fn menu_bounds_defaults_and_anchor_absence() {
        let games = picker_components(Some(PickerKind::Games), &[GAME_PICKS[0].role_id], &[]);
        let wire = serde_json::to_value(games).unwrap();
        let menu = &wire[0]["components"][0];
        assert_eq!(menu["min_values"], 0);
        assert_eq!(menu["max_values"], 10);
        assert_eq!(menu["options"].as_array().unwrap().len(), 10);
        assert_eq!(menu["options"][0]["default"], true);
        // Self-only preview alongside the apply route: distinct ID, human
        // preview label, empty-answer support and full-catalog multi-choice.
        assert_eq!(wire.as_array().unwrap().len(), 2);
        let preview = &wire[1]["components"][0];
        assert_eq!(preview["custom_id"], GAME_PREVIEW_SELECT_ID);
        assert!(preview["placeholder"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("preview"));
        assert_eq!(preview["min_values"], 0);
        assert_eq!(preview["max_values"], 10);
        assert_eq!(preview["options"].as_array().unwrap().len(), 10);
        assert!(preview["options"]
            .as_array()
            .unwrap()
            .iter()
            .all(|option| option["default"] == false));
        let session = picker_components(
            Some(PickerKind::Session),
            &[],
            &build_session_picks("10", "11"),
        );
        let wire = serde_json::to_value(session).unwrap();
        assert_eq!(wire[0]["components"][0]["min_values"], 1);
        assert_eq!(wire[0]["components"][0]["max_values"], 2);
        assert!(picker_components(None, &[], &[]).is_empty());
    }

    #[test]
    fn deferred_reply_is_private() {
        let wire = serde_json::to_value(defer_ephemeral()).unwrap();
        assert_eq!(wire["type"], 5);
        assert_eq!(wire["data"]["flags"], 64);
    }
}
