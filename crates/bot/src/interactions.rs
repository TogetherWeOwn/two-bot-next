//! Boot-effective gates and bounded shared interaction dispatch.

use std::{collections::HashMap, sync::Arc};

use tokio::task::JoinSet;
use twilight_gateway::Event;
use two_bot_core::{FeatureGates, ModerationGates, RouterGates, SurfaceFlags};
use two_bot_discord::ActionExecutor;

use crate::command_runtime::{router_with_commands, CommandRuntime};

const MAX_IN_FLIGHT: usize = 32;

fn unavailable(message: &'static str) -> sqlx::Error {
    sqlx::Error::InvalidArgument(message.into())
}

/// Announcement gates are cold: read guild overrides once, with env fallback.
/// Never treat a failed settings read as an empty snapshot.
pub async fn initialize(
    pool: sqlx::PgPool,
    guild_id: u64,
    token: String,
) -> Result<InteractionDispatch, sqlx::Error> {
    let snapshot = two_bot_cutover::settings::SettingsStore::new(&pool)
        .load_snapshot()
        .await?;
    let mut vars: HashMap<String, String> = std::env::vars().collect();
    vars.extend(
        two_bot_core::settings::SettingsCache::load(&snapshot)
            .env_snapshot(Some(&guild_id.to_string())),
    );
    let gates = boot_gates(guild_id, &vars)?;
    let proxy = std::env::var("DISCORD_API_BASE")
        .ok()
        .filter(|value| !value.is_empty());
    let executor = ActionExecutor::with_proxy(token, proxy)
        .map_err(|_| unavailable("interaction executor initialization failed"))?;
    // This also supplies identity on a RESUME boot, which receives no READY user.
    let (bot_user_id, application_id) = executor
        .current_identity()
        .await
        .map_err(|_| unavailable("interaction identity unavailable"))?;
    // TOG-10080 owns the custom-command store. None is unavailable, not zero rows.
    let runtime =
        CommandRuntime::build(pool, executor, router_with_commands(gates), guild_id, None);
    runtime.set_identity(bot_user_id, application_id);
    runtime
        .publish_registry_checked(Some(application_id))
        .await?;
    Ok(InteractionDispatch::new(
        runtime,
        bot_user_id,
        application_id,
    ))
}

pub(crate) fn publication_definitions(
    router: &two_bot_core::InteractionRouter,
    gates: RouterGates,
    custom: Option<&[two_bot_core::CustomCommand]>,
) -> Result<Option<Vec<two_bot_core::CommandDefinition>>, sqlx::Error> {
    if gates.automations && custom.is_none() {
        // Bulk replacement requires an authoritative custom row load. A missing
        // store must not silently delete existing commands from Discord.
        tracing::warn!("registry publication deferred: custom-command store unavailable");
        return Ok(None);
    }
    router
        .publish_set(custom.unwrap_or_default())
        .map(Some)
        .map_err(|_| unavailable("command registry invalid"))
}

fn boot_gates(guild_id: u64, vars: &HashMap<String, String>) -> Result<RouterGates, sqlx::Error> {
    let features = FeatureGates::from_map(vars)
        .map_err(|_| unavailable("invalid interaction feature gates"))?;
    let moderation = ModerationGates::from_map(vars)
        .map_err(|_| unavailable("invalid interaction moderation gates"))?;
    Ok(RouterGates::from_slices(
        Some(guild_id),
        &features,
        &moderation,
        SurfaceFlags {
            scorecard: vars
                .get("TWO_COMMUNITY_SCORECARD")
                .is_some_and(|v| v == "1"),
            ..Default::default()
        },
    ))
}

pub struct InteractionDispatch {
    runtime: Arc<CommandRuntime>,
    bot_user_id: u64,
    application_id: u64,
    tasks: JoinSet<()>,
}

impl InteractionDispatch {
    fn new(runtime: Arc<CommandRuntime>, bot_user_id: u64, application_id: u64) -> Self {
        Self {
            runtime,
            bot_user_id,
            application_id,
            tasks: JoinSet::new(),
        }
    }

    /// Never await paced REST/SQL on the shard's polling path. Admission is bounded;
    /// overload/panics fail before committing this dispatch rather than growing tasks.
    /// Dropping the JoinSet aborts tasks on shard failure (durable LFG state remains).
    /// https://docs.rs/tokio/1/tokio/task/struct.JoinSet.html
    pub fn handle(&mut self, event: &Event) -> Result<(), sqlx::Error> {
        while let Some(result) = self.tasks.try_join_next() {
            result.map_err(|_| unavailable("interaction task panicked"))?;
        }
        let handled = matches!(
            event,
            Event::Ready(_)
                | Event::Resumed
                | Event::InteractionCreate(_)
                | Event::MessageCreate(_)
        );
        if handled && self.tasks.len() >= MAX_IN_FLIGHT {
            return Err(unavailable("interaction dispatch capacity exceeded"));
        }
        match event {
            Event::Ready(ready) => {
                if ready.user.id.get() != self.bot_user_id
                    || ready.application.id.get() != self.application_id
                {
                    return Err(unavailable("gateway and REST identities differ"));
                }
                self.runtime
                    .set_identity(self.bot_user_id, self.application_id);
                let runtime = Arc::clone(&self.runtime);
                let application_id = self.application_id;
                self.tasks.spawn(async move {
                    runtime.publish_registry(Some(application_id)).await;
                });
            }
            Event::Resumed => {
                let runtime = Arc::clone(&self.runtime);
                self.tasks.spawn(async move {
                    runtime.publish_registry(None).await;
                });
            }
            Event::InteractionCreate(interaction) => {
                let runtime = Arc::clone(&self.runtime);
                let interaction = interaction.0.clone();
                self.tasks.spawn(async move {
                    runtime.on_interaction(&interaction).await;
                });
            }
            Event::MessageCreate(message) => {
                let runtime = Arc::clone(&self.runtime);
                let message = message.0.clone();
                self.tasks.spawn(async move {
                    runtime.on_message(&message).await;
                });
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, data: serde_json::Value) -> Event {
        Event::from(
            twilight_gateway::parse(
                serde_json::json!({"op": 0, "s": 1, "t": kind, "d": data}).to_string(),
                twilight_gateway::EventTypeFlags::all(),
            )
            .unwrap()
            .unwrap(),
        )
    }

    fn dispatch() -> InteractionDispatch {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
            .unwrap();
        let gates = boot_gates(22, &HashMap::new()).unwrap();
        let runtime = CommandRuntime::build(
            pool,
            ActionExecutor::with_proxy("mock-token".into(), Some("http://127.0.0.1:1".into()))
                .unwrap(),
            router_with_commands(gates),
            22,
            None,
        );
        runtime.set_identity(99, 11);
        InteractionDispatch::new(runtime, 99, 11)
    }

    #[test]
    fn publication_is_full_and_does_not_guess_custom_rows() {
        let mut vars = HashMap::from([("TWO_ANNOUNCEMENTS".into(), "1".into())]);
        let gates = boot_gates(22, &vars).unwrap();
        let router = two_bot_core::InteractionRouter::new(gates);
        let defs = publication_definitions(&router, gates, None)
            .unwrap()
            .unwrap();
        assert!(defs.iter().any(|d| d.name == "lfg"));
        assert!(defs.iter().any(|d| d.name == "lfg-close"));
        assert!(defs.iter().any(|d| d.name == "rank"));
        assert!(defs.len() > 2);
        vars.insert("TWO_AUTOMATIONS".into(), "1".into());
        let gates = boot_gates(22, &vars).unwrap();
        let router = two_bot_core::InteractionRouter::new(gates);
        assert!(publication_definitions(&router, gates, None)
            .unwrap()
            .is_none());
        assert!(publication_definitions(&router, gates, Some(&[]))
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn ready_checks_identity_and_resumed_retains_boot_identity() {
        crate::gateway::ensure_crypto_provider();
        let mut dispatch = dispatch();
        let ready = |id: &str| {
            event(
                "READY",
                serde_json::json!({
                    "v": 10, "user": {"id": id, "username": "mock", "discriminator": "0"},
                    "session_id": "mock", "resume_gateway_url": "ws://127.0.0.1:1", "guilds": [],
                    "application": {"id": "11", "flags": 0}
                }),
            )
        };
        dispatch.handle(&ready("99")).unwrap();
        dispatch.handle(&Event::Resumed).unwrap();
        assert!(dispatch.handle(&ready("88")).is_err());
    }

    #[tokio::test]
    async fn bounded_dispatch_does_not_wait_for_pending_work_and_aborts_on_drop() {
        crate::gateway::ensure_crypto_provider();
        let mut dispatch = dispatch();
        let mut aborts = Vec::new();
        for _ in 0..MAX_IN_FLIGHT {
            aborts.push(dispatch.tasks.spawn(std::future::pending::<()>()));
        }
        assert!(dispatch.handle(&Event::Resumed).is_err());
        let interaction = event(
            "INTERACTION_CREATE",
            serde_json::json!({
                "application_id": "11", "authorizing_integration_owners": {}, "id": "33",
                "token": "mock-token", "type": 2, "version": 1, "guild_id": "22",
                "data": {"id": "44", "name": "lfg", "type": 1}
            }),
        );
        assert!(dispatch.handle(&interaction).is_err());
        drop(dispatch);
        tokio::task::yield_now().await;
        assert!(aborts.iter().all(|task| task.is_finished()));
    }

    #[test]
    fn boot_gate_is_exact_and_default_off() {
        let mut vars = HashMap::new();
        assert!(!boot_gates(22, &vars).unwrap().announcements);
        for value in ["true", "0", "", " 1"] {
            vars.insert("TWO_ANNOUNCEMENTS".into(), value.into());
            assert!(!boot_gates(22, &vars).unwrap().announcements);
        }
        vars.insert("TWO_ANNOUNCEMENTS".into(), "1".into());
        let gates = boot_gates(22, &vars).unwrap();
        assert!(gates.announcements);
        assert_eq!(gates.configured_guild, Some(22));
    }
}
