//! Gateway receipt owner for discovery help and its page buttons.

use std::sync::{atomic::Ordering, Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;
use tracing::warn;
use twilight_gateway::Event;
use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use twilight_model::http::interaction::InteractionResponse;
use two_bot_core::help::cursor::{Cursor, LIFETIME_SECONDS};
use two_bot_core::{ComponentHandler, ComponentOutcome, HandlerId, SlashContext, SlashOutcome};
use two_bot_discord::executor::RECEIPT_ADMISSION_BUDGET_MS;
use two_bot_discord::help_navigation::{source_cursor, DiscoveryPlan, HelpRecovery};

use super::{CommandRuntime, DispatchTasks, DISPATCH_LIMITS, LANE_BUSY, LANE_HELP};
use crate::db_log::discord_class;

enum Request {
    Discovery,
    Button,
}

// Store only active scalar IDs, never tokens or a retained page/session. The
// separate refusal budget lets a refusal through even when all plan slots are
// occupied, without queuing work or consuming another plan slot.
const HELP_LIMIT: usize = DISPATCH_LIMITS[LANE_HELP];
const REFUSAL_LIMIT: usize = DISPATCH_LIMITS[LANE_BUSY];
static HELP_SLOTS: Slots = Slots(Mutex::new(Vec::new()));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    Render,
    Refuse,
}

struct Slot {
    actor: u64,
    interaction: u64,
    admission: Admission,
}

struct Slots(Mutex<Vec<Slot>>);

impl Slots {
    fn claim(&self, actor: u64, id: u64, tasks: &Mutex<DispatchTasks>) -> Option<Admission> {
        let mut slots = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if slots.iter().any(|slot| slot.interaction == id) {
            return None;
        }
        let plans = slots
            .iter()
            .filter(|slot| slot.admission == Admission::Render)
            .count();
        let actor_active = slots.iter().any(|slot| slot.actor == actor);
        let admission = if plans < HELP_LIMIT && !actor_active {
            Admission::Render
        } else if slots.len() - plans < REFUSAL_LIMIT
            && !slots
                .iter()
                .any(|slot| slot.actor == actor && slot.admission == Admission::Refuse)
        {
            Admission::Refuse
        } else {
            drop(slots);
            tasks.lock().expect("command task scope").record_drop(
                &[LANE_HELP],
                "help capacity reached",
                "command dispatch saturated; Help event not admitted (sampled; see two_bot_dispatch_drops_total)",
            );
            return None;
        };
        slots.push(Slot {
            actor,
            interaction: id,
            admission,
        });
        Some(admission)
    }

    fn release(&self, interaction: u64) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|slot| slot.interaction != interaction);
    }
}

struct Permit(u64);

impl Drop for Permit {
    fn drop(&mut self) {
        HELP_SLOTS.release(self.0);
    }
}

impl CommandRuntime {
    /// Claims only the reserved help routes, including silent guild/app
    /// refusals. The gateway must not feed a claimed event to another owner.
    pub(super) fn dispatch_help(self: &Arc<Self>, event: &Event, received: Instant) -> bool {
        let Event::InteractionCreate(event) = event else {
            return false;
        };
        let interaction = &event.0;
        let guild = interaction.guild_id.map(|id| id.get());
        let request = match (&interaction.kind, interaction.data.as_ref()) {
            (InteractionType::MessageComponent, Some(InteractionData::MessageComponent(data)))
                if Cursor::is_help_control(&data.custom_id) =>
            {
                let ComponentOutcome::Handled {
                    handler: ComponentHandler::HelpPage,
                } = self
                    .interactions
                    .router
                    .route_component_with_help(&data.custom_id, guild)
                else {
                    return true;
                };
                Request::Button
            }
            (
                InteractionType::ApplicationCommand,
                Some(InteractionData::ApplicationCommand(data)),
            ) if !data
                .options
                .iter()
                .any(|option| matches!(option.name.as_str(), "command" | "subcommand"))
                && self
                    .executor
                    .command_identities()
                    .slash_name(interaction)
                    .as_deref()
                    == Some("help") =>
            {
                let SlashOutcome::Handled {
                    handler: HandlerId::Help,
                } = self.interactions.router.route_slash(&SlashContext {
                    name: "help",
                    guild_id: guild,
                    actor_permissions: None,
                    custom_row: None,
                })
                else {
                    return true;
                };
                Request::Discovery
            }
            _ => return false,
        };
        let application = self.application_id.load(Ordering::Relaxed);
        let Some(actor) = interaction.author_id() else {
            return true;
        };
        let deadline = received + Duration::from_millis(RECEIPT_ADMISSION_BUDGET_MS);
        if application != interaction.application_id.get() || Instant::now() >= deadline {
            return true;
        }
        let id = interaction.id.get();
        let Some(admission) = HELP_SLOTS.claim(actor.get(), id, &self.tasks) else {
            return true;
        };
        let permit = Permit(id);
        let runtime = Arc::clone(self);
        let interaction = interaction.clone();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let lane = match admission {
            Admission::Render => LANE_HELP,
            Admission::Refuse => LANE_BUSY,
        };
        self.spawn(lane, async move {
            let _permit = permit;
            if Instant::now() >= deadline {
                return;
            }
            let response = match admission {
                Admission::Render => runtime
                    .discovery_response(&interaction, request, application, actor.get(), now)
                    .unwrap_or_else(HelpRecovery::response),
                Admission::Refuse => HelpRecovery::Busy {
                    from_button: matches!(request, Request::Button),
                }
                .response(),
            };
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
        });
        true
    }

    fn discovery_response(
        &self,
        interaction: &Interaction,
        request: Request,
        application: u64,
        actor: u64,
        now: u64,
    ) -> Result<InteractionResponse, HelpRecovery> {
        let cursor = match request {
            Request::Discovery => None,
            Request::Button => {
                let (cursor, _) = source_cursor(
                    interaction,
                    application,
                    self.interactions.known_bot_user_id(),
                )
                .ok_or(HelpRecovery::Invalid)?;
                Some(cursor)
            }
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_runtime::LANE_INTERACTIONS;

    #[test]
    fn admission_caps_plans_and_refusals_without_queueing_or_racing_duplicates() {
        let slots = Slots(Mutex::new(Vec::new()));
        let tasks = Mutex::new(DispatchTasks::default());
        let claim = |actor, id| slots.claim(actor, id, &tasks);
        let drops = || tasks.lock().unwrap().drops[LANE_INTERACTIONS];
        assert_eq!(claim(1, 101), Some(Admission::Render));
        assert_eq!(claim(1, 101), None);
        assert_eq!(claim(1, 201), Some(Admission::Refuse));
        assert_eq!(claim(1, 201), None);
        assert_eq!(drops(), 0, "in-flight duplicates are not drops");
        assert_eq!(claim(1, 202), None);
        assert_eq!(drops(), 1, "per-actor refusal cap");
        for actor in 2..=HELP_LIMIT as u64 {
            assert_eq!(claim(actor, 100 + actor), Some(Admission::Render));
        }
        for i in 1..REFUSAL_LIMIT as u64 {
            assert_eq!(claim(900 + i, 201 + i), Some(Admission::Refuse));
        }
        assert_eq!(claim(900, 300), None, "both budgets are full");
        assert_eq!(drops(), 2, "one drop per capacity rejection");
        slots.release(101);
        slots.release(201);
        assert_eq!(claim(1, 301), Some(Admission::Render));
        assert_eq!(claim(900, 300), Some(Admission::Refuse));
    }
}
