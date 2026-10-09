//! Gateway composition of the shared custom-command router/runtime.
//! Bootstrap reads finish before constructing the shard, including cold RESUME.
//! Event work is detached by [`crate::command_runtime::CommandRuntime::dispatch`] or its verdict-carrying form.

use std::{
    collections::HashMap,
    sync::{Arc, PoisonError, RwLock},
};

use sqlx::PgPool;
use twilight_gateway::Event;
use twilight_model::{
    application::interaction::Interaction,
    channel::Message,
    gateway::payload::incoming::GuildCreate,
    id::{marker::GuildMarker, Id},
};
use two_bot_core::{
    automod_runtime::FunnelDisposition,
    custom_commands::AutomationMessageAcceptance,
    feature_commands::FeatureGates,
    moderation::ModerationGates,
    router::{RouterGates, SurfaceFlags},
    InteractionRouter,
};
use two_bot_discord::{
    custom_commands::{CustomCommandError, CustomCommandRuntime},
    ActionExecutor,
};

pub struct GatewayCommandConfig {
    gates: RouterGates,
    text_commands: bool,
    acceptance: AutomationMessageAcceptance,
}

impl GatewayCommandConfig {
    pub fn from_map(guild_id: u64, vars: &HashMap<String, String>) -> Result<Self, sqlx::Error> {
        let features = FeatureGates::from_map(vars).map_err(|_| config_error())?;
        let moderation = ModerationGates::from_map(vars).map_err(|_| config_error())?;
        if guild_id == 0 {
            return Err(config_error());
        }
        let gates = RouterGates::from_slices(
            Some(guild_id),
            &features,
            &moderation,
            SurfaceFlags {
                scorecard: vars
                    .get("TWO_COMMUNITY_SCORECARD")
                    .is_some_and(|v| v == "1"),
                // Component handlers are not activated by this feature slice.
                ..SurfaceFlags::default()
            },
        );
        Ok(Self {
            gates,
            text_commands: features.text_commands,
            acceptance: acceptance_without_inspector(vars.get("TWO_AUTOMOD").map(String::as_str)),
        })
    }
}

fn config_error() -> sqlx::Error {
    sqlx::Error::InvalidArgument("gateway command configuration invalid".into())
}

/// Only an explicit disabled configuration is acceptance without an automod verdict.
fn acceptance_without_inspector(automod: Option<&str>) -> AutomationMessageAcceptance {
    if automod == Some("0") {
        AutomationMessageAcceptance::AutomodDisabled
    } else {
        AutomationMessageAcceptance::Unavailable
    }
}

/// Automod verdict for one MessageCreate, as decided in gateway order by the
/// serial dispatch worker. An accepted create behaves like an unmatched
/// inspection; every other verdict fails closed. A missing verdict is never
/// acceptance: the worker's bool precheck may let an unscreened create through,
/// and this mapping is the second fence.
pub fn acceptance_for_verdict(verdict: Option<FunnelDisposition>) -> AutomationMessageAcceptance {
    match verdict {
        Some(FunnelDisposition::Accept) => AutomationMessageAcceptance::Unmatched,
        Some(FunnelDisposition::CaptureOnly) => AutomationMessageAcceptance::CaptureOnly,
        // `None` dispositions belong to edits, never creates: a create carrying
        // one is unexpected, so refuse it like a match.
        Some(FunnelDisposition::None) => AutomationMessageAcceptance::Matched,
        None => AutomationMessageAcceptance::Unavailable,
    }
}

fn effective_acceptance(
    configured: AutomationMessageAcceptance,
    verdict: Option<FunnelDisposition>,
) -> AutomationMessageAcceptance {
    // `TWO_AUTOMOD=0` never waits for a verdict: the disabled fast path stays
    // exactly as configured. Every other configuration needs the verdict.
    if configured == AutomationMessageAcceptance::AutomodDisabled {
        configured
    } else {
        acceptance_for_verdict(verdict)
    }
}

pub struct GatewayCommands {
    runtime: CustomCommandRuntime,
    application_id: u64,
    guild_id: Id<GuildMarker>,
    /// Seeded by bootstrap REST, refreshed from GUILD_CREATE/GUILD_UPDATE.
    guild_name: RwLock<String>,
    text_commands: bool,
    acceptance: AutomationMessageAcceptance,
}

impl GatewayCommands {
    #[cfg(test)]
    pub async fn bootstrap(
        pool: PgPool,
        executor: ActionExecutor,
        config: GatewayCommandConfig,
    ) -> Result<Arc<crate::command_runtime::CommandRuntime>, sqlx::Error> {
        let runtime = crate::command_runtime::CommandRuntime::new(
            pool,
            executor,
            crate::command_runtime::router_with_commands(config.gates),
            config.gates.configured_guild.ok_or_else(config_error)?,
            config.gates.automations,
        );
        runtime.initialize_custom_commands(config).await?;
        Ok(runtime)
    }

    pub(crate) async fn bootstrap_with_router(
        pool: PgPool,
        executor: ActionExecutor,
        config: GatewayCommandConfig,
        router: Arc<InteractionRouter>,
    ) -> Result<Self, sqlx::Error> {
        let guild_id = config
            .gates
            .configured_guild
            .and_then(Id::new_checked)
            .ok_or_else(config_error)?;
        // The cause is a fixed token (never the error text), so a failed boot
        // names the read and why without exposing provider or transport detail.
        let application_id = executor.current_application_id().await.map_err(|error| {
            tracing::warn!(
                step = "application_lookup",
                cause = error.cause(),
                "gateway bootstrap read failed"
            );
            sqlx::Error::InvalidArgument("gateway application context unavailable".into())
        })?;
        let bootstrap_guild_name = executor.guild_name(guild_id.get()).await.map_err(|error| {
            tracing::warn!(
                step = "guild_lookup",
                cause = error.cause(),
                "gateway bootstrap read failed"
            );
            sqlx::Error::InvalidArgument("gateway guild context unavailable".into())
        })?;
        let runtime = CustomCommandRuntime::new(pool, router, executor, application_id);
        Ok(Self {
            runtime,
            application_id,
            guild_id,
            guild_name: RwLock::new(bootstrap_guild_name),
            text_commands: config.text_commands,
            acceptance: config.acceptance,
        })
    }

    /// Reception-order observation, before later events spawn their work:
    /// keeps the template `{server}` name current without a cache lookup.
    pub fn observe(&self, event: &Event) {
        let name = match event {
            Event::GuildCreate(guild) => match guild.as_ref() {
                GuildCreate::Available(guild) if guild.id == self.guild_id => &guild.name,
                _ => return,
            },
            Event::GuildUpdate(update) if update.0.id == self.guild_id => &update.0.name,
            _ => return,
        };
        name.clone_into(
            &mut self
                .guild_name
                .write()
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    fn guild_name(&self) -> String {
        self.guild_name
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn published_commands(&self) -> Option<Arc<[two_bot_core::CommandDefinition]>> {
        self.runtime.published_commands()
    }

    /// One complete registry for READY and for a cold RESUME where READY is
    /// absent. The session must name the bootstrapped application; a mismatch
    /// publishes nothing.
    pub async fn sync_registry(&self, application_id: u64) -> Result<(), CustomCommandError> {
        if application_id != self.application_id {
            return Err(CustomCommandError::Context);
        }
        self.runtime.sync_registry().await
    }

    /// Detached interaction dispatch. True means this runtime owns the
    /// interaction, including a failed or uncertain attempt: acknowledged
    /// operations never fall through to another handler or replay.
    pub async fn handle_interaction(&self, interaction: &Interaction) -> bool {
        let guild_name = self.guild_name();
        match self
            .runtime
            .handle_interaction(interaction, Some(&guild_name))
            .await
        {
            Ok(handled) => handled,
            Err(error) => {
                // Errors contain fixed codes only.
                tracing::warn!(error = %error, "custom-command interaction finished without confirmed success");
                true
            }
        }
    }

    /// Detached prefix-trigger dispatch with the automod verdict for this
    /// create. A disabled configuration keeps its fast-path acceptance;
    /// every other configuration resolves the verdict (absent fails closed).
    /// The immutable pre-send attempt claim fences reordered or replayed events.
    pub async fn handle_message(&self, message: &Message, verdict: Option<FunnelDisposition>) {
        let acceptance = effective_acceptance(self.acceptance, verdict);
        let guild_name = self.guild_name();
        if let Err(error) = self
            .runtime
            .handle_message(message, acceptance, self.text_commands, Some(&guild_name))
            .await
        {
            tracing::warn!(error = %error, "custom-command trigger finished without confirmed success");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_inspector_accepts_only_explicitly_disabled_automod() {
        assert_eq!(
            acceptance_without_inspector(Some("0")),
            AutomationMessageAcceptance::AutomodDisabled
        );
        for value in [
            None,
            Some("1"),
            Some(""),
            Some("false"),
            Some(" 0"),
            Some("00"),
        ] {
            assert_eq!(
                acceptance_without_inspector(value),
                AutomationMessageAcceptance::Unavailable
            );
        }
    }

    #[test]
    fn automod_verdict_maps_to_trigger_acceptance() {
        // Allow: an accepted create behaves like an unmatched inspection.
        assert_eq!(
            acceptance_for_verdict(Some(FunnelDisposition::Accept)),
            AutomationMessageAcceptance::Unmatched
        );
        assert!(acceptance_for_verdict(Some(FunnelDisposition::Accept)).permits_automations());
        // Deny: containment and unexpected edit-like verdicts never run triggers.
        assert_eq!(
            acceptance_for_verdict(Some(FunnelDisposition::CaptureOnly)),
            AutomationMessageAcceptance::CaptureOnly
        );
        assert_eq!(
            acceptance_for_verdict(Some(FunnelDisposition::None)),
            AutomationMessageAcceptance::Matched
        );
        for verdict in [
            Some(FunnelDisposition::CaptureOnly),
            Some(FunnelDisposition::None),
        ] {
            assert!(!acceptance_for_verdict(verdict).permits_automations());
        }
        // Absent: a missing verdict fails closed.
        assert_eq!(
            acceptance_for_verdict(None),
            AutomationMessageAcceptance::Unavailable
        );
        assert!(!acceptance_for_verdict(None).permits_automations());
    }

    #[test]
    fn disabled_automod_keeps_fast_path_without_a_verdict() {
        assert_eq!(
            effective_acceptance(
                AutomationMessageAcceptance::AutomodDisabled,
                Some(FunnelDisposition::Accept)
            ),
            AutomationMessageAcceptance::AutomodDisabled
        );
        assert_eq!(
            effective_acceptance(AutomationMessageAcceptance::AutomodDisabled, None),
            AutomationMessageAcceptance::AutomodDisabled
        );
        assert!(
            effective_acceptance(AutomationMessageAcceptance::AutomodDisabled, None)
                .permits_automations()
        );
        // Any other configuration resolves the verdict, and absent stays closed.
        assert_eq!(
            effective_acceptance(
                AutomationMessageAcceptance::Unavailable,
                Some(FunnelDisposition::Accept)
            ),
            AutomationMessageAcceptance::Unmatched
        );
        assert_eq!(
            effective_acceptance(AutomationMessageAcceptance::Unavailable, None),
            AutomationMessageAcceptance::Unavailable
        );
        assert!(
            !effective_acceptance(AutomationMessageAcceptance::Unavailable, None)
                .permits_automations()
        );
    }

    #[test]
    fn command_gates_use_shared_parsers_and_fail_closed() {
        let mut vars = HashMap::new();
        assert!(GatewayCommandConfig::from_map(0, &vars).is_err());
        let config = GatewayCommandConfig::from_map(1, &vars).unwrap();
        assert!(!config.gates.automations);
        assert!(!config.text_commands);
        assert!(!config.acceptance.permits_automations());
        vars.insert("TWO_AUTOMATIONS".into(), "1".into());
        vars.insert("TWO_TEXT_COMMANDS".into(), "1".into());
        vars.insert("TWO_AUTOMOD".into(), "0".into());
        let config = GatewayCommandConfig::from_map(1, &vars).unwrap();
        assert!(config.gates.automations && config.text_commands);
        assert!(config.acceptance.permits_automations());
        vars.insert("TWO_MODERATION".into(), "1".into());
        assert!(GatewayCommandConfig::from_map(1, &vars).is_err());
    }
}
