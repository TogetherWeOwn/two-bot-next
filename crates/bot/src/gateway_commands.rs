//! Gateway composition of the shared custom-command router/runtime.
//! Bootstrap reads finish before constructing the shard, including cold RESUME.
//! Event work is detached at reception by [`crate::command_runtime::CommandRuntime::dispatch`].

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

/// The ordinary message pipeline currently captures facts but has no automod
/// inspector. Only an explicit disabled configuration is known acceptance.
/// Missing, enabled, and malformed values must not turn capture into permission.
fn acceptance_without_inspector(automod: Option<&str>) -> AutomationMessageAcceptance {
    if automod == Some("0") {
        AutomationMessageAcceptance::AutomodDisabled
    } else {
        AutomationMessageAcceptance::Unavailable
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

    /// Detached prefix-trigger dispatch with the configured acceptance, which
    /// stays fail-closed until an automod verdict producer exists. The
    /// immutable pre-send attempt claim fences reordered or replayed events.
    pub async fn handle_message(&self, message: &Message) {
        let guild_name = self.guild_name();
        if let Err(error) = self
            .runtime
            .handle_message(
                message,
                self.acceptance,
                self.text_commands,
                Some(&guild_name),
            )
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
