//! Self-role input/orchestration owned by the shared command runtime.
//! Boot composes at most one service, shared by dispatch and recovery.

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use sqlx::PgPool;
use tracing::warn;
use twilight_model::{
    application::interaction::{Interaction, InteractionData, InteractionType},
    channel::message::{component::ComponentType, EmojiReactionType},
    gateway::GatewayReaction,
};
use two_bot_core::{
    backup::guild_config::TWO_STAGING_GUILD_ID,
    self_roles::{
        event_order_for_event_id, event_order_from_snowflake, is_snowflake,
        parse_self_role_custom_id, parse_self_role_panels, reaction_option_key, self_role_reply,
        PanelMode, SelfRoleGates, SelfRolePanel, SettledOutcome,
    },
    settings::{to_env_string, LiveSettings, SettingsCache},
};
use two_bot_cutover::self_role_store::SelfRoleStore;
use two_bot_discord::ActionExecutor;

use crate::{
    command_runtime::new_id,
    jobs::{self, ErrorClass, Job},
    self_role_runtime::{
        Admission, RuntimeError, Selection, SelfRoleRequest, SelfRoleRuntime, StoreErrorKind,
    },
};

pub(crate) const RECOVERY_JOB_NAME: &str = "self_role_recovery";
const BOOT_IDENTITY_TIMEOUT: Duration = Duration::from_secs(10);

/// The approved activation scope: only the pinned TWO Staging guild.
pub(crate) fn staging_allowlist() -> HashSet<String> {
    HashSet::from([TWO_STAGING_GUILD_ID.to_owned()])
}

/// The panel catalogue key served from the live settings snapshot. Service
/// existence stays boot-gated (an empty boot catalogue builds no service);
/// every dispatch and recovery sweep re-resolves the catalogue, so a stored
/// catalogue replaces the boot one within one poll while enabling from a
/// boot-empty deployment needs a restart.
pub(crate) const PANELS_KEY: &str = "TWO_SELF_ROLE_PANELS";

/// Effective panel catalogue: the live snapshot wins over the boot catalogue,
/// an absent or unparsable row falls back to boot (a bypassed write guard
/// must not blank live pickers). A stored empty array resolves to no panels.
/// Pure for tests via `live`.
pub(crate) fn live_panels_with(
    guild: &str,
    boot: &[SelfRolePanel],
    live: Option<&LiveSettings>,
) -> Vec<SelfRolePanel> {
    let snapshot = live.map(LiveSettings::snapshot);
    panels_from_snapshot(guild, boot, snapshot.as_deref())
}

fn panels_from_snapshot(
    guild: &str,
    boot: &[SelfRolePanel],
    snapshot: Option<&SettingsCache>,
) -> Vec<SelfRolePanel> {
    let Some(live) = snapshot else {
        return boot.to_vec();
    };
    let Some(value) = live.get(guild, PANELS_KEY) else {
        return boot.to_vec();
    };
    let Some(rendered) = to_env_string(value) else {
        return boot.to_vec();
    };
    parse_self_role_panels(&rendered).unwrap_or_else(|_| boot.to_vec())
}

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

    /// Process boot: the ONE service Arc that `main` shares between gateway
    /// dispatch and the supervised recovery job. Every gate fails closed, and
    /// an empty or invalid catalogue returns before building a REST executor.
    pub async fn from_env(pool: PgPool, token: &str, guild_id: u64) -> Option<Arc<Self>> {
        let gates = match SelfRoleGates::from_env() {
            Ok(gates) => gates,
            Err(err) => {
                // Parked, not ignored: old panels stay unserved until fixed.
                tracing::error!(
                    error = %err,
                    "TWO_SELF_ROLE_PANELS or TWO_SELF_ROLE_DRY_RUN invalid; self-roles disabled"
                );
                return None;
            }
        };
        if gates.panels.is_empty() {
            return None;
        }
        let proxy = std::env::var("DISCORD_API_BASE")
            .ok()
            .filter(|value| !value.is_empty());
        let Ok(executor) = ActionExecutor::with_proxy(token.to_owned(), proxy) else {
            warn!("REST executor failed to build; self-roles disabled");
            return None;
        };
        Self::boot(
            gates,
            SelfRoleStore::new(pool),
            executor,
            guild_id,
            &staging_allowlist(),
        )
        .await
    }

    /// Allowlist and catalogue gates run before the one bot-identity read.
    /// A failed or slow read parks the feature instead of guessing an identity.
    pub(crate) async fn boot(
        gates: SelfRoleGates,
        store: SelfRoleStore,
        executor: ActionExecutor,
        guild_id: u64,
        staging_allowlist: &HashSet<String>,
    ) -> Option<Arc<Self>> {
        let guild_id = guild_id.to_string();
        if gates.panels.is_empty() || !staging_allowlist.contains(&guild_id) {
            return None;
        }
        let bot_id =
            match tokio::time::timeout(BOOT_IDENTITY_TIMEOUT, executor.current_bot_user_id()).await
            {
                Ok(Ok(id)) => id.to_string(),
                _ => {
                    warn!("bot user lookup failed; self-roles disabled");
                    return None;
                }
            };
        let runtime = SelfRoleRuntime {
            store,
            executor,
            guild_id,
            bot_id,
        };
        Self::new(runtime, gates, staging_allowlist).map(Arc::new)
    }

    /// Catalogue for this operation: the live snapshot layered over the boot
    /// catalogue. Resolved once per entry point so a poll racing dispatch
    /// cannot pair a panel from one catalogue with options from another.
    fn live_panels(&self) -> Vec<SelfRolePanel> {
        live_panels_with(
            &self.runtime.guild_id,
            &self.panels,
            crate::settings_jobs::live().as_ref(),
        )
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
        let panels = self.live_panels();
        let panel = panels.iter().find(|p| p.id == parsed.panel_id)?;
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

    /// True only for a stale self-role reference: the id parses and the press
    /// is otherwise well-formed, but the panel, source message, or option it
    /// names is gone. The caller answers one expired-control reply and mutates
    /// nothing. Malformed ids/shapes, foreign guilds, and bots are not stale:
    /// silence. A message-id mismatch is stale in any channel (the panel moved
    /// on; the custom id already binds the press to it), while a live message
    /// id in the wrong channel is spoofed, not stale. Mirrors `component_input`,
    /// except the channel comes from the source message itself
    /// (`Interaction::channel_id` is deprecated upstream; the message is the
    /// authority for which source message the press belongs to).
    pub(crate) fn stale_component(&self, interaction: &Interaction) -> bool {
        if interaction.kind != InteractionType::MessageComponent
            || !interaction
                .guild_id
                .is_some_and(|id| id.get().to_string() == self.runtime.guild_id)
        {
            return false;
        }
        let Some(InteractionData::MessageComponent(data)) = &interaction.data else {
            return false;
        };
        let Some(parsed) = parse_self_role_custom_id(&data.custom_id) else {
            return false;
        };
        let member = interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref());
        let Some(member) = member else {
            return false;
        };
        if member.bot || member.id.get().to_string() == self.runtime.bot_id {
            return false;
        }
        let panels = self.live_panels();
        let Some(panel) = panels.iter().find(|p| p.id == parsed.panel_id) else {
            // Parses, but names no live panel: the panel was removed.
            return true;
        };
        let Some(message) = interaction.message.as_ref() else {
            return false;
        };
        if message
            .guild_id
            .is_some_and(|id| Some(id) != interaction.guild_id)
        {
            return false;
        }
        if message.id.get().to_string() != panel.message_id {
            // Same panel, new source message in any channel: the panel moved
            // on and this press is on its former source.
            return true;
        }
        if message.channel_id.get().to_string() != panel.channel_id {
            // Live message id in the wrong channel: spoofed, not stale.
            return false;
        }
        match (panel.mode, data.component_type, parsed.option_key) {
            (PanelMode::Button, ComponentType::Button, Some(option_key))
                if data.values.is_empty() =>
            {
                !panel.options.iter().any(|o| o.key == option_key)
            }
            (PanelMode::Select, ComponentType::TextSelectMenu, None) => {
                let unique: HashSet<_> = data.values.iter().collect();
                unique.len() == data.values.len()
                    && !(panel.exclusive && data.values.len() > 1)
                    && data
                        .values
                        .iter()
                        .any(|key| !panel.options.iter().any(|o| &o.key == key))
            }
            _ => false,
        }
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
        let panels = self.live_panels();
        let panel = panels.iter().find(|p| {
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
        // Pin the catalogue and revision together. The store rechecks this
        // revision under the settings writer lock before creating durable work.
        let snapshot = crate::settings_jobs::live().map(|live| live.snapshot());
        let panels =
            panels_from_snapshot(&self.runtime.guild_id, &self.panels, snapshot.as_deref());
        if !panels.contains(&input.panel) {
            return SelfRoleResult::Ignored;
        }
        let revision = snapshot.as_ref().map_or(0, |snapshot| snapshot.revision());
        self.handle_admission(
            self.runtime
                .prepare_at_revision(&input.request, &input.panel, revision)
                .await,
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
            cadence_source: None,
            startup_jitter: jobs::startup_jitter(cadence, rand::random()),
            timeout: Duration::from_secs(25),
            action: Arc::new(move || {
                let service = Arc::clone(&service);
                Box::pin(async move {
                    service
                        .recover_once()
                        .await
                        .map(|_| ())
                        .map_err(recovery_error_class)
                })
            }),
        }
    }

    /// At most eight panels, sixteen discovery queries, sixty-four hints and
    /// thirty-two considered audits. Each panel interleaves up to four owners;
    /// queue priority and panel start rotate before any await so cancellation
    /// cannot permanently favor processing over terminal repair. Dry-run never
    /// claims terminal work. Renewed expiry supplies durable restart backoff.
    /// The sweep resolves the live catalogue once, so a stored empty array
    /// sweeps nothing instead of dividing by the boot length.
    pub async fn recover_once(&self) -> Result<usize, RuntimeError> {
        self.recover_panels(&self.live_panels()).await
    }

    async fn recover_panels(&self, panels: &[SelfRolePanel]) -> Result<usize, RuntimeError> {
        if panels.is_empty() {
            return Ok(0);
        }
        let mut considered = 0;
        let sweep = self.recovery_cursor.fetch_add(1, Ordering::Relaxed);
        let start = sweep % panels.len();
        for offset in 0..panels.len().min(8) {
            let panel = &panels[(start + offset) % panels.len()];
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
                let terminal_first = (sweep / panels.len() + slot) % 2 == 1;
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

/// Scalar job-metric class for a recovery sweep failure. Every store kind
/// maps explicitly so a timeout never reports as a database failure and a
/// zero-row receipt completion never conflates with real database errors.
pub(crate) fn recovery_error_class(error: RuntimeError) -> ErrorClass {
    match error {
        RuntimeError::Store(StoreErrorKind::Timeout) => ErrorClass::Timeout,
        RuntimeError::Store(StoreErrorKind::Database) => ErrorClass::Database,
        RuntimeError::Store(StoreErrorKind::UnmatchedReceipt) => ErrorClass::Configuration,
        RuntimeError::Rest(_) => ErrorClass::Rest,
        _ => ErrorClass::Configuration,
    }
}

#[cfg(test)]
pub(crate) mod tests;
