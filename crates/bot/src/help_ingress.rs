//! Gateway receipt owner for discovery help and its page buttons.

use std::future::Future;
use std::sync::{atomic::Ordering, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;
use tracing::warn;
use twilight_gateway::Event;
use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use twilight_model::http::interaction::InteractionResponse;
use two_bot_core::help::cursor::{Cursor, LIFETIME_SECONDS};
use two_bot_discord::executor::RECEIPT_ADMISSION_BUDGET_MS;
use two_bot_discord::help_navigation::{source_cursor, DiscoveryPlan, HelpRecovery};

use super::{CommandRuntime, DispatchTasks, RegistrySyncError, LANE_INTERACTIONS};
use crate::db_log::discord_class;

pub(crate) const LANE_HELP: usize = 0;
pub(crate) const LANE_HELP_REFUSAL: usize = 1;
pub(crate) const HELP_LIMIT: usize = 8;

#[derive(Default)]
pub(super) struct HelpTasks([Vec<HelpTask>; 2]);

struct HelpTask {
    actor: u64,
    id: u64,
    handle: tokio::task::AbortHandle,
}

impl HelpTasks {
    pub(super) fn abort(&mut self) {
        for task in self.0.iter_mut().flat_map(|lane| lane.drain(..)) {
            task.handle.abort();
        }
    }

    fn prune(&mut self) {
        for lane in &mut self.0 {
            lane.retain(|task| !task.handle.is_finished());
        }
    }
}

impl DispatchTasks {
    fn spawn_help<F: Future<Output = ()> + Send + 'static>(
        &mut self,
        actor: u64,
        id: u64,
        work: impl FnOnce(bool) -> F,
    ) -> Option<usize> {
        self.help.prune();
        let lanes = &mut self.help.0;
        if self.stopped || lanes.iter().flatten().any(|task| task.id == id) {
            return None;
        }
        let lane = if lanes[LANE_HELP].len() < HELP_LIMIT
            && !lanes.iter().flatten().any(|task| task.actor == actor)
        {
            LANE_HELP
        } else if lanes[LANE_HELP_REFUSAL].len() < HELP_LIMIT
            && !lanes[LANE_HELP_REFUSAL]
                .iter()
                .any(|task| task.actor == actor)
        {
            LANE_HELP_REFUSAL
        } else {
            self.record_drop(&[LANE_INTERACTIONS], "help capacity reached",
                "command dispatch saturated; Help event not admitted (sampled; see two_bot_dispatch_drops_total)");
            return None;
        };
        lanes[lane].push(HelpTask {
            actor,
            id,
            handle: tokio::spawn(work(lane == LANE_HELP_REFUSAL)).abort_handle(),
        });
        Some(lane)
    }
}

impl CommandRuntime {
    #[cfg(test)]
    pub(crate) fn hold_help_for_test(&self, actor: u64, id: u64) -> Option<usize> {
        let mut tasks = self.tasks.lock().expect("command task scope");
        tasks.spawn_help(actor, id, |_| std::future::pending())
    }

    #[cfg(test)]
    pub(crate) fn help_in_flight(&self, lane: usize) -> usize {
        let mut tasks = self.tasks.lock().expect("command task scope");
        tasks.help.prune();
        tasks.help.0[lane].len()
    }

    /// Run before ordered reception. Only token-authenticated REST may fill a missing pin.
    pub(crate) async fn bootstrap_application_id(&self) -> Result<u64, RegistrySyncError> {
        let id = self
            .executor
            .current_application_id()
            .await
            .map_err(|_| RegistrySyncError::ApplicationLookup)?;
        match self.application_id.load(Ordering::Relaxed) {
            0 => self.set_identity(id, id),
            pinned if pinned != id => return Err(RegistrySyncError::ApplicationLookup),
            _ => {}
        }
        Ok(id)
    }

    /// Claims only the reserved help routes, including silent guild/app
    /// refusals. The gateway must not feed a claimed event to another owner.
    pub(super) fn dispatch_help(self: &Arc<Self>, event: &Event, received: Instant) -> bool {
        let Event::InteractionCreate(event) = event else {
            return false;
        };
        let interaction = &event.0;
        let discovery = match interaction.data.as_ref() {
            Some(InteractionData::MessageComponent(data)) => {
                interaction.kind == InteractionType::MessageComponent
                    && Cursor::is_help_control(&data.custom_id)
            }
            Some(InteractionData::ApplicationCommand(data)) => {
                !data
                    .options
                    .iter()
                    .any(|option| matches!(option.name.as_str(), "command" | "subcommand"))
                    && self
                        .executor
                        .command_identities()
                        .slash_name(interaction)
                        .as_deref()
                        == Some("help")
            }
            _ => false,
        };
        if !discovery {
            return false;
        }
        // Both discovery routes are always exposed, but only in the configured guild.
        let guild = self.interactions.router.gates().configured_guild;
        if !interaction
            .guild_id
            .is_some_and(|id| Some(id.get()) == guild)
        {
            return true;
        }
        let application = self.application_id.load(Ordering::Relaxed);
        let Some(actor) = interaction.author_id() else {
            return true;
        };
        let deadline = received + Duration::from_millis(RECEIPT_ADMISSION_BUDGET_MS);
        if application != interaction.application_id.get() || Instant::now() >= deadline {
            return true;
        }
        let id = interaction.id.get();
        let mut tasks = self.tasks.lock().expect("command task scope");
        let _ = tasks.spawn_help(actor.get(), id, |refuse| {
            let runtime = Arc::clone(self);
            let interaction = interaction.clone();
            async move {
                if Instant::now() >= deadline {
                    return;
                }
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let response = runtime
                    .discovery_response(&interaction, refuse, application, actor.get(), now)
                    .unwrap_or_else(HelpRecovery::response);
                if let Err(error) = runtime
                    .executor
                    .answer_interaction_since_receipt(id, &interaction.token, &response, received)
                    .await
                {
                    warn!(
                        interaction_id = id,
                        reason = discord_class(&error),
                        "help callback failed"
                    );
                }
            }
        });
        true
    }

    fn discovery_response(
        &self,
        interaction: &Interaction,
        refuse: bool,
        application: u64,
        actor: u64,
        now: u64,
    ) -> Result<InteractionResponse, HelpRecovery> {
        let from_button = interaction.kind == InteractionType::MessageComponent;
        let cursor = if from_button {
            let (cursor, _) = source_cursor(
                interaction,
                application,
                self.interactions.known_bot_user_id(),
            )
            .ok_or(HelpRecovery::Invalid)?;
            Some(cursor)
        } else {
            None
        };
        if refuse {
            if cursor.is_some_and(|cursor| cursor.is_expired(now)) {
                return Err(HelpRecovery::Expired);
            }
            return Err(HelpRecovery::Busy { from_button });
        }
        let defs = self.published_commands().ok_or(HelpRecovery::CannotBuild)?;
        let plan = DiscoveryPlan::new(&defs, self.guild_id, application)
            .map_err(|_| HelpRecovery::CannotBuild)?;
        match cursor {
            Some(cursor) => plan
                .navigate(cursor, actor, now)
                .map_err(HelpRecovery::from),
            None => plan
                .first(actor, now + LIFETIME_SECONDS)
                .ok_or(HelpRecovery::CannotBuild),
        }
    }
}
