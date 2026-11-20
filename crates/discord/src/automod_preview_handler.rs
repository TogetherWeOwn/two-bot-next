//! Map Discord invocations and drive preview attempts. The router owns
//! authorization and the caller owns ephemeral delivery and metric emission.

use twilight_model::application::interaction::{
    application_command::{CommandData, CommandOptionValue},
    Interaction,
};
use two_bot_core::automod_preview_input::{
    decode_preview_input, DecodedPreviewInput, PreviewInputRefusal, PreviewOptionValue,
};

#[cfg(feature = "db")]
use {
    crate::automod_activation::{AutomodActivation, AutomodFacts},
    std::sync::{Mutex, PoisonError},
    tokio::time::Instant,
    twilight_model::application::interaction::{InteractionData, InteractionType},
    two_bot_core::{
        automod_preview_attempt::{attempt_preview, AttemptContext, AttemptResult},
        automod_preview_flow::{plan_pre_claim, PreClaimPlan},
        automod_preview_input::build_preview_message,
        automod_preview_limiter::PreviewRateLimiter,
        automod_preview_report::{metric_unavailable, PREVIEW_UNAVAILABLE_REPLY},
        automod_preview_store::SqlPreviewStore,
        automod_runtime::AutomodClaimLedger,
        funnel::format_iso_millis,
        AutomodConfig,
    },
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
        #[cfg(feature = "db")]
        let channel_id = crate::channel_moderation::invoking_channel_id(interaction)?;
        #[cfg(not(feature = "db"))]
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

/// Drive an authorized invocation with its UTC receipt. Resolve the preview
/// policy after decode and scope, without changing the gateway activation.
/// The caller owns ephemeral delivery and reserves time for it after `deadline`.
#[cfg(feature = "db")]
pub async fn handle_automod_preview<L: AutomodClaimLedger, F: AutomodFacts>(
    activation: Option<&AutomodActivation<L, F>>,
    pool: &sqlx::PgPool,
    limiter: &Mutex<PreviewRateLimiter>,
    interaction: &Interaction,
    receipt_ms: u64,
    deadline: Instant,
    resolve_live: impl FnOnce(&str) -> Option<AutomodConfig>,
) -> AttemptResult {
    if Instant::now() >= deadline {
        return unavailable(None);
    }
    let (InteractionType::ApplicationCommand, Some(InteractionData::ApplicationCommand(data))) =
        (interaction.kind, interaction.data.as_ref())
    else {
        return unavailable(None);
    };
    let Some(request) = PreviewRequest::from_interaction(interaction) else {
        return unavailable(None);
    };
    let decoded = decode_interaction_options(data);
    let scoped = decoded.as_ref().ok().and_then(|input| {
        let (active, guild) = activation.zip(request.guild_id.as_deref())?;
        if !active.preview(|runtime| runtime.admits(Some(guild))) {
            return None;
        }
        let live = resolve_live(guild);
        let snapshot =
            active.preview(|runtime| runtime.preview_snapshot(guild, &request.actor_id, live))?;
        Some((guild, input, snapshot))
    });
    let plan = plan_pre_claim(
        decoded.as_ref().map_err(|refusal| *refusal),
        scoped.is_some(),
        scoped.is_some(),
        &request.interaction_id.to_string(),
    );
    if let PreClaimPlan::AnswerNow { reply, metric } = plan {
        return AttemptResult { reply, metric };
    }
    let Some((guild_id, input, (dry_run, evaluate))) = scoped else {
        return unavailable(None);
    };
    let Ok(message) = build_preview_message(
        input,
        guild_id,
        &request.channel_id,
        &request.actor_id,
        &request.role_ids,
        request.interaction_id,
        receipt_ms,
    ) else {
        return unavailable(Some(dry_run));
    };
    let receipt_iso = format_iso_millis(i64::try_from(receipt_ms).unwrap_or(i64::MAX));
    let (actor_id, id) = (&request.actor_id, request.interaction_id);
    let context = AttemptContext {
        guild_id,
        actor_id,
        channel_id: &request.channel_id,
        receipt_iso: &receipt_iso,
        dry_run,
    };
    let store = SqlPreviewStore::new(pool, deadline);
    let admit = || {
        limiter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .admit_before(actor_id, guild_id, id, deadline)
    };
    let evaluate = || evaluate(&message, input.mention_count);
    attempt_preview(plan, context, store, admit, evaluate).await
}

#[cfg(feature = "db")]
fn unavailable(dry_run: Option<bool>) -> AttemptResult {
    AttemptResult {
        reply: PREVIEW_UNAVAILABLE_REPLY.to_owned(),
        metric: Some(metric_unavailable(dry_run)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use two_bot_core::automod_runtime::STAGING_GUILD_ID;

    fn preview_interaction(legacy_channel_id: Option<&str>) -> Interaction {
        serde_json::from_value(json!({
            "id": "4242", "application_id": "1111", "type": 2,
            "attachment_size_limit": 10_485_760,
            "guild_id": STAGING_GUILD_ID,
            "channel": {"id": "777", "type": 0}, "channel_id": legacy_channel_id,
            "data": {"id": "1", "type": 1, "name": "automod-preview"},
            "member": {"user": {"id": "2222", "username": "previewer", "discriminator": "0"},
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
        let mut interaction = preview_interaction(Some("888"));
        let request = PreviewRequest::from_interaction(&interaction)
            .expect("complete invocation forms a request");
        assert_eq!(request.guild_id.as_deref(), Some(STAGING_GUILD_ID));
        assert_eq!(request.channel_id, "777");
        assert_eq!(request.actor_id, "2222");
        assert_eq!(request.role_ids, vec!["9".to_owned()]);
        assert_eq!(request.interaction_id, 4242);

        interaction.channel = None;
        let legacy = PreviewRequest::from_interaction(&interaction);
        #[cfg(feature = "db")]
        assert_eq!(legacy.expect("legacy invocation channel").channel_id, "888");
        #[cfg(not(feature = "db"))]
        assert_eq!(legacy, None);
    }

    #[test]
    fn request_maps_dm_to_no_guild_and_refuses_contextless_shapes() {
        let mut dm = preview_interaction(None);
        dm.guild_id = None;
        dm.user = dm.member.take().and_then(|member| member.user);
        let request = PreviewRequest::from_interaction(&dm).expect("DM still forms a request");
        assert_eq!(request.guild_id, None);
        assert_eq!(request.actor_id, "2222");
        assert!(request.role_ids.is_empty());
        dm.user = None;
        assert_eq!(PreviewRequest::from_interaction(&dm), None);

        for missing in ["member", "member_user", "channel"] {
            let mut interaction = preview_interaction(None);
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
    fn decoder_rejects_unknown_and_noncanonical_option_names() {
        let extra = command_data(json!([
            {"name": "text", "type": 3, "value": "hello"},
            {"name": "extra", "type": 3, "value": "unexpected"}
        ]));
        assert_eq!(
            decode_interaction_options(&extra),
            Err(PreviewInputRefusal::UnknownOption)
        );

        let uppercase = command_data(json!([{"name": "TEXT", "type": 3, "value": "hello"}]));
        assert_eq!(
            decode_interaction_options(&uppercase),
            Err(PreviewInputRefusal::UnknownOption)
        );
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
