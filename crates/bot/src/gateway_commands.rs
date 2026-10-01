//! Gateway composition of the shared custom-command router/runtime.
//! Bootstrap reads finish before constructing the shard, including cold RESUME.

use std::{collections::HashMap, sync::Arc};

use sqlx::PgPool;
use twilight_cache_inmemory::InMemoryCache;
use twilight_gateway::Event;
use twilight_model::id::{marker::GuildMarker, Id};
use two_bot_core::{
    custom_commands::AutomationMessageAcceptance,
    feature_commands::FeatureGates,
    moderation::ModerationGates,
    router::{RouterGates, SurfaceFlags},
    InteractionRouter,
};
use two_bot_discord::{custom_commands::CustomCommandRuntime, ActionExecutor};

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
    bootstrap_guild_name: String,
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
            crate::command_runtime::CommandRuntime::build_router(config.gates),
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
        let application_id = executor.current_application_id().await.map_err(|_| {
            sqlx::Error::InvalidArgument("gateway application context unavailable".into())
        })?;
        let bootstrap_guild_name = executor.guild_name(guild_id.get()).await.map_err(|_| {
            sqlx::Error::InvalidArgument("gateway guild context unavailable".into())
        })?;
        let runtime = CustomCommandRuntime::new(pool, router, executor, application_id);
        Ok(Self {
            runtime,
            application_id,
            guild_id,
            bootstrap_guild_name,
            text_commands: config.text_commands,
            acceptance: config.acceptance,
        })
    }

    /// Called after the ordinary pipeline and awaited before its checkpoint.
    /// Per-command failures are terminal for this dispatch, not a reason to
    /// retry an acknowledged/uncertain external operation. Deadline cancellation
    /// stops the shard; immutable prefix attempts fence any subsequent replay.
    pub async fn handle_event(
        &self,
        event: &Event,
        cache: &InMemoryCache,
    ) -> Result<bool, sqlx::Error> {
        if let Event::Ready(ready) = event {
            if ready.application.id.get() != self.application_id {
                return Err(sqlx::Error::InvalidArgument(
                    "gateway application context mismatch".into(),
                ));
            }
        }
        if matches!(event, Event::Ready(_) | Event::Resumed) {
            // One complete registry, also after a cold RESUME where READY is
            // absent. Do not report readiness after a failed synchronization.
            self.runtime.sync_registry().await.map_err(|_| {
                sqlx::Error::InvalidArgument("gateway registry synchronization failed".into())
            })?;
            return Ok(true);
        }
        // Own the name before awaiting: never hold a cache shard lock over I/O.
        // https://docs.rs/twilight-cache-inmemory/0.17.1/twilight_cache_inmemory/struct.InMemoryCache.html#method.guild
        let guild_name = cache
            .guild(self.guild_id)
            .map(|guild| guild.name().to_owned());
        let guild_name = guild_name.as_deref().unwrap_or(&self.bootstrap_guild_name);
        let result = match event {
            Event::InteractionCreate(interaction) => {
                self.runtime
                    .handle_interaction(interaction, Some(guild_name))
                    .await
            }
            Event::MessageCreate(message) => self
                .runtime
                .handle_message(
                    message,
                    self.acceptance,
                    self.text_commands,
                    Some(guild_name),
                )
                .await
                .map(|_| true),
            _ => return Ok(false),
        };
        match result {
            Ok(handled) => Ok(handled),
            Err(error) => {
                // Acknowledged/uncertain operations must never fall through to
                // another handler or replay. Errors contain fixed codes only.
                tracing::warn!(error = %error, "custom-command dispatch finished without confirmed success");
                Ok(true)
            }
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
