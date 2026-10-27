//! Read-only welcome rendering. Never call the delivery or picker executor.

use super::*;
use twilight_model::application::interaction::{InteractionData, InteractionType};
use two_bot_core::command_permissions::command_permission;
use two_bot_core::message_safety::{text_len, CONTENT_LIMIT};
use two_bot_core::RouterRefusal;

// A synthetic, roleless identity, never resolved via Discord or the member store.
pub(super) const FIXTURE_MEMBER_ID: u64 = 765_432_109_876_543_210;
const BANNER_PREFIX: &str = "Preview — not a delivery";

/// The onboarding receive-relative ACK owns this slash, including refusals.
/// Shared command admission must yield before it can issue a busy callback.
pub(crate) fn owns_welcome_preview(interaction: &Interaction) -> bool {
    matches!(
        (&interaction.kind, interaction.data.as_ref()),
        (InteractionType::ApplicationCommand, Some(InteractionData::ApplicationCommand(data)))
            if data.name == "welcome-preview"
    )
}

pub(crate) fn is_preview_control(interaction: &Interaction) -> bool {
    interaction.kind == InteractionType::MessageComponent
        && interaction
            .message
            .as_ref()
            .is_some_and(|message| message.content.starts_with(BANNER_PREFIX))
}

impl OnboardingRuntime {
    pub(super) fn preview_banner(&self) -> String {
        // Mode is boot-only. Use a fixed label even on configuration failure;
        // no raw deployment/stored values may reach the guidance response.
        let mode = match self
            .deployment
            .get("TWO_ONBOARDING_MODE")
            .map(String::as_str)
        {
            Some("session") => "session",
            Some("anchor") => "anchor",
            _ => "legacy",
        };
        format!("{BANNER_PREFIX} · {mode} · fixture {FIXTURE_MEMBER_ID}")
    }

    pub(super) async fn preview_guidance(
        &self,
        interaction: &Interaction,
        guidance: &str,
    ) -> Result<(), RuntimeError> {
        self.reply(
            interaction,
            &format!("{}\n{guidance}", self.preview_banner()),
        )
        .await
    }

    pub(super) async fn welcome_preview(
        &self,
        interaction: &Interaction,
        now_ms: i64,
    ) -> Result<(), RuntimeError> {
        // Recheck the capture fence at the executor boundary. Refuse before SQL
        // and never resolve either the fixture's or the invoker's member state.
        if self.resolve_interaction(interaction).is_none() {
            return Ok(());
        }
        let permissions = interaction
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .map(|permissions| permissions.bits());
        if !command_permission("welcome-preview").is_some_and(|row| row.allows(permissions)) {
            return self
                .reply(interaction, &RouterRefusal::ManageServerRequired.message())
                .await;
        }
        let config = match self.config().await {
            Ok(config) => config,
            Err(_) => {
                return self.preview_guidance(interaction, "[configuration unavailable] No delivery was attempted. Ask a server admin to check the welcome settings, then try again.").await;
            }
        };
        // This is configured render input, not proof of live channel access.
        // Resolving bot/member/channel permissions belongs to preflight/delivery.
        let landing = config.landing_channel_ids.first().map(ToString::to_string);
        let anchor = config
            .anchor_channel_id
            .map(|id| id.to_string())
            .unwrap_or_default();
        let effect = adjudicate_welcome(
            config.gates.mode,
            FIXTURE_MEMBER_ID,
            landing.as_deref(),
            &anchor,
            now_ms / 1000,
            SUNDAY_SQUAD,
            config.gates.dry_run,
        );
        let WelcomeEffect::Post {
            channel_id,
            mut content,
            picker,
            ..
        } = effect
        else {
            let guidance = if config.gates.dry_run && config.gates.mode != OnboardingMode::Session {
                "[dry run] Welcome delivery is disabled in this mode. No delivery was attempted."
            } else {
                "[missing landing channel] No welcome destination is configured. Ask a server admin to configure a landing channel, then try again. No delivery was attempted."
            };
            return self.preview_guidance(interaction, guidance).await;
        };
        // Match the live welcome's configured anchor substitution exactly.
        if config.gates.mode == OnboardingMode::Anchor {
            content = content.replace(SUNDAY_SQUAD.channel_id, &channel_id);
        }
        let content = format!(
            "{}\nConfigured destination: <#{channel_id}>\n{content}",
            self.preview_banner()
        );
        if text_len(&content) > CONTENT_LIMIT {
            return self.preview_guidance(interaction, "[welcome exceeds preview limit] Ask a server admin to check the welcome settings, then try again. No delivery was attempted.").await;
        }
        let components = picker_components(picker, &[], &config.session_picks);
        // Components remain byte-identical to the live renderer. Their original
        // preview message banner fences submissions before config or mutations.
        if components.is_empty() {
            // The live channel post omits the components key when the picker
            // is empty (anchor mode); the preview must serialize identically
            // rather than sending an explicit empty array.
            return self
                .executor
                .edit_interaction_response_with_blocked_retry(
                    interaction.application_id.get(),
                    &interaction.token,
                    &content,
                )
                .await
                .map_err(|_| RuntimeError::Discord);
        }
        self.executor
            .edit_interaction_response_with_components_and_blocked_retry(
                interaction.application_id.get(),
                &interaction.token,
                &content,
                &components,
            )
            .await
            .map_err(|_| RuntimeError::Discord)
    }
}
