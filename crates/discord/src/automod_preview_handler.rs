//! Discord invocation mapping for `/automod-preview`. The core decoder owns
//! validation; this module does not authorize, evaluate, or send replies.

use twilight_model::application::interaction::{
    application_command::{CommandData, CommandOptionValue},
    Interaction,
};
use two_bot_core::automod_preview_input::{
    decode_preview_input, DecodedPreviewInput, PreviewInputRefusal, PreviewOptionValue,
};

/// The invoker's context only. A DM has no guild or roles and must still pass
/// through the core scope gate, which refuses evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRequest {
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub actor_id: String,
    pub role_ids: Vec<String>,
    pub interaction_id: u64,
}

impl PreviewRequest {
    /// Extract invocation context, or return `None` if Discord omitted a
    /// required field. A guild invocation never falls back to a DM user.
    #[must_use]
    pub fn from_interaction(interaction: &Interaction) -> Option<Self> {
        let channel_id = interaction.channel.as_ref()?.id.to_string();
        let (user, role_ids) = if interaction.guild_id.is_some() {
            let member = interaction.member.as_ref()?;
            (
                member.user.as_ref()?,
                member.roles.iter().map(ToString::to_string).collect(),
            )
        } else {
            (interaction.user.as_ref()?, Vec::new())
        };
        Some(Self {
            guild_id: interaction.guild_id.map(|id| id.to_string()),
            channel_id,
            actor_id: user.id.to_string(),
            role_ids,
            interaction_id: interaction.id.get(),
        })
    }
}

/// Preserve every option so the core decoder can reject unknown names,
/// duplicates and unsupported types rather than silently ignoring them.
fn map_preview_options(data: &CommandData) -> Vec<(&str, PreviewOptionValue<'_>)> {
    data.options
        .iter()
        .map(|option| {
            let value = match &option.value {
                CommandOptionValue::String(text) => PreviewOptionValue::Text(text),
                CommandOptionValue::Integer(count) => PreviewOptionValue::Integer(*count),
                _ => PreviewOptionValue::Other,
            };
            (option.name.as_str(), value)
        })
        .collect()
}

/// Strictly decode an invocation's options without changing the inputs.
pub fn decode_interaction_options(
    data: &CommandData,
) -> Result<DecodedPreviewInput, PreviewInputRefusal> {
    decode_preview_input(&map_preview_options(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use two_bot_core::automod_runtime::STAGING_GUILD_ID;

    fn preview_interaction() -> Interaction {
        serde_json::from_value(json!({
            "id": "4242", "application_id": "1111", "type": 2,
            "attachment_size_limit": 10_485_760,
            "guild_id": STAGING_GUILD_ID,
            "channel": {"id": "777", "type": 0},
            "data": {"id": "1", "type": 1, "name": "automod-preview"},
            "member": {"user": {"id": "4242", "username": "previewer", "discriminator": "0"},
                "roles": ["9"], "flags": 0, "deaf": false, "mute": false},
            "token": "synthetic-test-token", "version": 1,
            "authorizing_integration_owners": {"0": STAGING_GUILD_ID},
            "entitlements": []
        }))
        .expect("synthetic preview interaction")
    }

    fn command_data(options: serde_json::Value) -> CommandData {
        serde_json::from_value(
            json!({"id": "1", "type": 1, "name": "automod-preview", "options": options}),
        )
        .expect("synthetic command data")
    }

    #[test]
    fn request_takes_invocation_ids_and_member_roles() {
        let request = PreviewRequest::from_interaction(&preview_interaction())
            .expect("complete invocation forms a request");
        assert_eq!(request.guild_id.as_deref(), Some(STAGING_GUILD_ID));
        assert_eq!(request.channel_id, "777");
        assert_eq!(request.actor_id, "4242");
        assert_eq!(request.role_ids, vec!["9".to_owned()]);
        assert_eq!(request.interaction_id, 4242);
    }

    #[test]
    fn request_maps_dm_to_no_guild_and_refuses_contextless_shapes() {
        let mut dm = preview_interaction();
        dm.guild_id = None;
        dm.user = dm.member.take().and_then(|member| member.user);
        let request = PreviewRequest::from_interaction(&dm).expect("DM still forms a request");
        assert_eq!(request.guild_id, None);
        assert_eq!(request.actor_id, "4242");
        assert!(request.role_ids.is_empty());
        dm.user = None;
        assert_eq!(PreviewRequest::from_interaction(&dm), None);

        for missing in ["member", "member_user", "channel"] {
            let mut interaction = preview_interaction();
            interaction.user = interaction
                .member
                .as_ref()
                .and_then(|member| member.user.clone());
            match missing {
                "member" => interaction.member = None,
                "member_user" => {
                    interaction.member.as_mut().expect("fixture member").user = None;
                }
                _ => interaction.channel = None,
            }
            assert_eq!(
                PreviewRequest::from_interaction(&interaction),
                None,
                "missing {missing} cannot form a request"
            );
        }
    }

    #[test]
    fn options_map_string_and_integer_and_refuse_the_rest_downstream() {
        let data = command_data(json!([
            {"name": "text", "type": 3, "value": "hello"},
            {"name": "mentions", "type": 4, "value": 3},
            {"name": "attachments", "type": 5, "value": true}
        ]));
        let mapped = map_preview_options(&data);
        assert_eq!(mapped.len(), 3);
        assert!(matches!(mapped[0].1, PreviewOptionValue::Text("hello")));
        assert!(matches!(mapped[1].1, PreviewOptionValue::Integer(3)));
        assert!(matches!(mapped[2].1, PreviewOptionValue::Other));
        assert_eq!(
            decode_interaction_options(&data),
            Err(PreviewInputRefusal::WrongType)
        );

        let valid = command_data(json!([
            {"name": "text", "type": 3, "value": "hello"},
            {"name": "mentions", "type": 4, "value": 3}
        ]));
        let decoded = decode_interaction_options(&valid).expect("valid options decode");
        assert_eq!(decoded.text, "hello");
        assert_eq!(decoded.mention_count, 3);
    }

    #[test]
    fn decoder_keeps_duplicate_options_and_attachment_delimiters_strict() {
        let duplicate = command_data(json!([
            {"name": "text", "type": 3, "value": "hello"},
            {"name": "text", "type": 3, "value": "again"}
        ]));
        assert_eq!(
            decode_interaction_options(&duplicate),
            Err(PreviewInputRefusal::DuplicateOption)
        );

        for (names, valid) in [("safe.exe,txt", false), ("safe.exe, txt", true)] {
            let data = command_data(json!([
                {"name": "text", "type": 3, "value": "hello"},
                {"name": "attachments", "type": 3, "value": names}
            ]));
            let decoded = decode_interaction_options(&data);
            if valid {
                assert_eq!(
                    decoded.expect("canonical separator").attachment_names,
                    ["safe.exe", "txt"]
                );
            } else {
                assert_eq!(decoded, Err(PreviewInputRefusal::BadFilename));
            }
        }
    }
}
