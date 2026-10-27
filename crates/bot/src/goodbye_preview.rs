//! Read-only goodbye rendering. Never call the delivery executor, and never
//! touch the member store or the prompt guard: a real departure releases it.

use super::welcome_preview::FIXTURE_MEMBER_ID;
use super::*;
use twilight_model::application::interaction::{InteractionData, InteractionType};
use two_bot_core::command_permissions::command_permission;
use two_bot_core::message_safety::{text_len, CONTENT_LIMIT};
use two_bot_core::RouterRefusal;

/// The synthetic stay in days.
const FIXTURE_DAYS: i64 = 7;

/// The synthetic display name. It is deliberately hostile (markdown, a mass
/// mention and a user mention across a newline) so every preview proves the
/// live `sanitize_goodbye_name` boundary instead of rendering a tame sample.
fn fixture_name() -> String {
    format!("Preview *member* @everyone\n<@{FIXTURE_MEMBER_ID}>")
}

/// The onboarding receive-relative ACK owns this slash, including refusals.
/// Shared command admission must yield before it can issue a busy callback.
pub(crate) fn owns_goodbye_preview(interaction: &Interaction) -> bool {
    matches!(
        (&interaction.kind, interaction.data.as_ref()),
        (InteractionType::ApplicationCommand, Some(InteractionData::ApplicationCommand(data)))
            if data.name == "goodbye-preview"
    )
}

impl OnboardingRuntime {
    pub(super) async fn goodbye_preview(
        &self,
        interaction: &Interaction,
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
        if !command_permission("goodbye-preview").is_some_and(|row| row.allows(permissions)) {
            return self
                .reply(interaction, &RouterRefusal::ManageServerRequired.message())
                .await;
        }
        let config = match self.config().await {
            Ok(config) => config,
            Err(_) => {
                return self.preview_guidance(interaction, "[configuration unavailable] No delivery was attempted. Ask a server admin to check the onboarding settings, then try again.").await;
            }
        };
        // Configured render input, not proof of live channel access: the live
        // path's postable-channel scan needs member and permission reads, and
        // those belong to delivery and preflight, never to a preview.
        let channel = config.goodbye_channel_ids.first().map(ToString::to_string);
        let effect = adjudicate_goodbye(
            config.gates.mode,
            &fixture_name(),
            Some(FIXTURE_DAYS),
            channel.as_deref(),
            config.gates.dry_run,
        );
        let (channel_id, content) = match effect {
            Some(GoodbyeEffect::Post {
                channel_id,
                content,
                ..
            }) => (channel_id, content),
            None => {
                return self.preview_guidance(interaction, "[session mode required] Goodbye posts only run in session mode. No delivery was attempted. Ask a server admin to switch onboarding to session mode, then try again.").await;
            }
            Some(GoodbyeEffect::Skip { reason: "dry_run" }) => {
                return self.preview_guidance(interaction, "[dry run] Goodbye delivery is disabled while onboarding dry run is on. No delivery was attempted. Ask a server admin to turn off dry run, then try again.").await;
            }
            Some(GoodbyeEffect::Skip { .. }) => {
                return self.preview_guidance(interaction, "[missing goodbye channel] No goodbye destination is configured. Ask a server admin to set a goodbye channel, then try again. No delivery was attempted.").await;
            }
        };
        let content = format!(
            "{}\nConfigured destination: <#{channel_id}>\n{content}",
            self.preview_banner()
        );
        if text_len(&content) > CONTENT_LIMIT {
            return self.preview_guidance(interaction, "[goodbye exceeds preview limit] Ask a server admin to check the onboarding settings, then try again. No delivery was attempted.").await;
        }
        self.reply(interaction, &content).await
    }
}
