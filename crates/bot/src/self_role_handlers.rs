//! Self-role input/orchestration owned by the shared command runtime.
//! Boot injection remains parked until unresolved-work acceptance is complete.

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use tracing::warn;
use twilight_model::{
    application::interaction::{Interaction, InteractionData, InteractionType},
    channel::message::{component::ComponentType, EmojiReactionType},
    gateway::GatewayReaction,
};
use two_bot_core::self_roles::{
    event_order_for_event_id, event_order_from_snowflake, is_snowflake, parse_self_role_custom_id,
    reaction_option_key, self_role_reply, PanelMode, SelfRoleGates, SelfRolePanel, SettledOutcome,
};

use crate::{
    command_runtime::new_id,
    jobs::{self, ErrorClass, Job},
    self_role_runtime::{Admission, RuntimeError, Selection, SelfRoleRequest, SelfRoleRuntime},
};

pub(crate) const RECOVERY_JOB_NAME: &str = "self_role_recovery";

pub(crate) struct SelfRoleInput {
    pub panel: SelfRolePanel,
    pub request: SelfRoleRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelfRoleResult {
    Settled(SettledOutcome),
    DryRun,
    Duplicate,
    Ignored,
    Rejected,
    Pending,
}

impl SelfRoleResult {
    pub fn reply(self, color: bool) -> &'static str {
        match self {
            Self::Settled(outcome) => self_role_reply(outcome, color),
            Self::DryRun => "Dry run recorded; no roles were changed.",
            Self::Duplicate => "This selection is already being handled or was handled earlier.",
            Self::Ignored | Self::Rejected => "This role selection could not be applied.",
            Self::Pending => {
                "This role selection is unresolved; no successful change has been confirmed."
            }
        }
    }
}

pub(crate) struct SelfRoleService {
    runtime: SelfRoleRuntime,
    panels: Vec<SelfRolePanel>,
    dry_run: bool,
    recovery_cursor: AtomicUsize,
}

impl SelfRoleService {
    /// The caller supplies its approved STAGING allowlist, never event input.
    /// Empty/denied catalogues do not create a service or register a surface.
    pub fn new(
        runtime: SelfRoleRuntime,
        gates: SelfRoleGates,
        staging_allowlist: &HashSet<String>,
    ) -> Option<Self> {
        if gates.panels.is_empty()
            || !staging_allowlist.contains(&runtime.guild_id)
            || !is_snowflake(&runtime.guild_id)
            || !is_snowflake(&runtime.bot_id)
        {
            return None;
        }
        Some(Self {
            runtime,
            panels: gates.panels,
            dry_run: gates.dry_run,
            recovery_cursor: AtomicUsize::new(0),
        })
    }

    /// Only actual button/text-select input on the configured source message.
    /// The shared router has already selected ComponentHandler::SelfRole.
    #[allow(deprecated)]
    pub fn component_input(&self, interaction: &Interaction) -> Option<SelfRoleInput> {
        if interaction.kind != InteractionType::MessageComponent
            || interaction.guild_id?.get().to_string() != self.runtime.guild_id
        {
            return None;
        }
        let Some(InteractionData::MessageComponent(data)) = &interaction.data else {
            return None;
        };
        let parsed = parse_self_role_custom_id(&data.custom_id)?;
        let panel = self.panels.iter().find(|p| p.id == parsed.panel_id)?;
        let channel = interaction
            .channel
            .as_ref()
            .map(|c| c.id)
            .or(interaction.channel_id)?;
        if interaction.channel_id.is_some_and(|id| id != channel) {
            return None;
        }
        let message = interaction.message.as_ref()?;
        let member = interaction.member.as_ref()?.user.as_ref()?;
        if channel.get().to_string() != panel.channel_id
            || message.channel_id != channel
            || message.id.get().to_string() != panel.message_id
            || message
                .guild_id
                .is_some_and(|id| Some(id) != interaction.guild_id)
            || member.bot
            || member.id.get().to_string() == self.runtime.bot_id
        {
            return None;
        }
        let selection = match (panel.mode, data.component_type, parsed.option_key) {
            (PanelMode::Button, ComponentType::Button, Some(option_key))
                if data.values.is_empty() =>
            {
                if !panel.options.iter().any(|o| o.key == option_key) {
                    return None;
                }
                Selection::Button { option_key }
            }
            (PanelMode::Select, ComponentType::TextSelectMenu, None) => {
                let unique: HashSet<_> = data.values.iter().collect();
                if unique.len() != data.values.len()
                    || (panel.exclusive && data.values.len() > 1)
                    || data
                        .values
                        .iter()
                        .any(|key| !panel.options.iter().any(|o| &o.key == key))
                {
                    return None;
                }
                Selection::Select {
                    option_keys: data.values.clone(),
                }
            }
            _ => return None,
        };
        let event_id = interaction.id.get().to_string();
        let request = SelfRoleRequest {
            event_order: event_order_from_snowflake(&event_id)?,
            event_id,
            guild_id: self.runtime.guild_id.clone(),
            member_id: member.id.get().to_string(),
            channel_id: panel.channel_id.clone(),
            message_id: panel.message_id.clone(),
            selection,
        };
        Some(SelfRoleInput {
            panel: panel.clone(),
            request,
        })
    }

    /// Mint one delivery identity/order BEFORE detached gateway work. Reaction
    /// remove has no member; neither path trusts cached roles for authorization.
    pub fn reaction_input(
        &self,
        reaction: &GatewayReaction,
        remove: bool,
    ) -> Option<SelfRoleInput> {
        if reaction.guild_id?.get().to_string() != self.runtime.guild_id
            || reaction.user_id.get().to_string() == self.runtime.bot_id
        {
            return None;
        }
        let panel = self.panels.iter().find(|p| {
            p.mode == PanelMode::Reaction
                && p.channel_id == reaction.channel_id.get().to_string()
                && p.message_id == reaction.message_id.get().to_string()
        })?;
        let emoji_id = match &reaction.emoji {
            EmojiReactionType::Custom { id, .. } => Some(id.get().to_string()),
            EmojiReactionType::Unicode { .. } => None,
        };
        let emoji_name = match &reaction.emoji {
            EmojiReactionType::Custom { name, .. } => name.as_deref(),
            EmojiReactionType::Unicode { name } => Some(name.as_str()),
        };
        let option_key = reaction_option_key(panel, emoji_id.as_deref(), emoji_name)?;
        let event_id = format!("reaction:{}", new_id());
        let now = two_bot_core::funnel::now_millis_for_test().max(0) as u64;
        let request = SelfRoleRequest {
            event_order: event_order_for_event_id(&event_id, now),
            event_id,
            guild_id: self.runtime.guild_id.clone(),
            member_id: reaction.user_id.get().to_string(),
            channel_id: panel.channel_id.clone(),
            message_id: panel.message_id.clone(),
            selection: Selection::Reaction { option_key, remove },
        };
        Some(SelfRoleInput {
            panel: panel.clone(),
            request,
        })
    }

    pub async fn handle(&self, input: &SelfRoleInput) -> SelfRoleResult {
        if !self.panels.contains(&input.panel) {
            return SelfRoleResult::Ignored;
        }
        self.handle_admission(
            self.runtime.prepare(&input.request, &input.panel).await,
            &input.panel,
            &input.request.event_id,
        )
        .await
    }

    /// Await the sweep inside the existing supervisor's attempt, not a detached
    /// recovery task. Timeout/shutdown drops its owners and their lease keepers;
    /// durable evidence and pending remote exchanges remain for future recovery.
    /// A successful tick means the sweep completed, not that every audit settled.
    pub fn recovery_job(self: &Arc<Self>) -> Job {
        let service = Arc::clone(self);
        let cadence = Duration::from_secs(30);
        Job {
            name: RECOVERY_JOB_NAME,
            cadence,
            startup_jitter: jobs::startup_jitter(cadence, rand::random()),
            timeout: Duration::from_secs(25),
            action: Arc::new(move || {
                let service = Arc::clone(&service);
                Box::pin(async move {
                    service
                        .recover_once()
                        .await
                        .map(|_| ())
                        .map_err(|error| match error {
                            RuntimeError::Store => ErrorClass::Database,
                            RuntimeError::Rest(_) => ErrorClass::Rest,
                            _ => ErrorClass::Configuration,
                        })
                })
            }),
        }
    }

    /// At most eight panels, sixteen discovery queries, sixty-four hints and
    /// thirty-two considered audits. Each panel interleaves up to four owners;
    /// queue priority and panel start rotate before any await so cancellation
    /// cannot permanently favor processing over terminal repair. Dry-run never
    /// claims terminal work. Renewed expiry supplies durable restart backoff.
    pub async fn recover_once(&self) -> Result<usize, RuntimeError> {
        let mut considered = 0;
        let sweep = self.recovery_cursor.fetch_add(1, Ordering::Relaxed);
        let start = sweep % self.panels.len();
        for offset in 0..self.panels.len().min(8) {
            let panel = &self.panels[(start + offset) % self.panels.len()];
            let budget = (32 - considered).min(4);
            let mut processing = self
                .runtime
                .recovery_candidates(panel, budget)
                .await?
                .into_iter();
            let mut terminal = if self.dry_run {
                vec![]
            } else {
                self.runtime.terminal_candidates(panel, budget).await?
            }
            .into_iter();
            for slot in 0..budget {
                // Alternate each complete start-panel rotation, not sweep
                // parity (which would pin a panel's priority for even counts).
                let terminal_first = (sweep / self.panels.len() + slot) % 2 == 1;
                let next = if terminal_first {
                    terminal
                        .next()
                        .map(|c| (c, true))
                        .or_else(|| processing.next().map(|c| (c, false)))
                } else {
                    processing
                        .next()
                        .map(|c| (c, false))
                        .or_else(|| terminal.next().map(|c| (c, true)))
                };
                let Some((candidate, is_terminal)) = next else {
                    break;
                };
                considered += 1;
                if is_terminal {
                    if let Err(error) = self.runtime.recover_terminal(&candidate, panel).await {
                        warn!(?error, event_id = %candidate.event_id, "self-role terminal repair unresolved");
                    }
                } else {
                    let admission = self.runtime.recover(&candidate, panel).await;
                    self.handle_admission(admission, panel, &candidate.event_id)
                        .await;
                }
            }
            if considered == 32 {
                break;
            }
        }
        Ok(considered)
    }

    async fn handle_admission(
        &self,
        admission: Result<Admission, RuntimeError>,
        panel: &SelfRolePanel,
        event_id: &str,
    ) -> SelfRoleResult {
        let mut prepared = match admission {
            Ok(Admission::Ready(prepared)) => prepared,
            Ok(Admission::Duplicate) => return SelfRoleResult::Duplicate,
            Ok(Admission::Ignored) => return SelfRoleResult::Ignored,
            Ok(Admission::Rejected(_)) => return SelfRoleResult::Rejected,
            Err(error) => {
                warn!(?error, event_id = %event_id, "self-role admission unresolved");
                return SelfRoleResult::Pending;
            }
        };
        let result = if self.dry_run {
            self.runtime
                .settle_dry_run(&mut prepared, panel)
                .await
                .map(|()| SelfRoleResult::DryRun)
        } else {
            match self.runtime.execute(&mut prepared, panel).await {
                Ok(execution) => self
                    .runtime
                    .settle(&mut prepared, panel, execution)
                    .await
                    .map(SelfRoleResult::Settled),
                Err(error) => Err(error),
            }
        };
        match result {
            Ok(result) => result,
            Err(error) => {
                // Repairs need fresh typed evidence ownership and a new lane;
                // never enter in dry-run or report success for the old event.
                if !self.dry_run
                    && matches!(&error, RuntimeError::Stale)
                    && panel.exclusive
                    && (!prepared.audit.effects.attempted_added_role_ids.is_empty()
                        || !prepared.audit.effects.attempted_removed_role_ids.is_empty())
                {
                    if let Err(repair) = self.runtime.reconcile_stale(&mut prepared, panel).await {
                        warn!(?repair, event_id = %event_id, "self-role stale repair unresolved");
                    }
                }
                warn!(?error, event_id = %event_id, "self-role execution unresolved");
                self.runtime.park(&mut prepared).await;
                SelfRoleResult::Pending
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
