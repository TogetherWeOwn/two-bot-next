//! Interaction router + full command registry publish (TOG-10075).
//!
//! Ports `docs/parity.md` §3 `InteractionCreate` (all §1–§2 dispatch) and the
//! `ClientReady` command publish (`guild.commands.set`) from legacy two-bot as
//! framework-free data plus pure functions built on the `commands`,
//! `feature_commands` and `moderation` registry modules. No twilight types here:
//! the discord adapter translates wire interactions into [`SlashContext`]s and
//! `custom_id` strings, so every route, refusal and publish assembly is
//! unit-testable without Discord.
//!
//! Source files (legacy `two-bot`, frozen `main`):
//! - slash dispatch: `src/leveling/discord.ts` (`registerLeveling`),
//!   `src/automations/discord.ts` (`registerAutomationCommands`),
//!   `src/announcements/discord.ts` (`registerAnnouncementCommands`),
//!   `src/moderation/commands.ts` (`registerModerationHandler`),
//!   `src/analytics/communityAttendance.ts` (`registerCommunityAttendance`).
//! - component dispatch: `src/discord/onboarding.ts` (`GAME_SELECT_ID`),
//!   `src/discord/sessionWelcome.ts` (`SESSION_SELECT_ID`),
//!   `src/discord/selfRoles.ts` (`two:self-role:` prefix),
//!   `src/discord/tickets.ts` (`two:tickets:open/claim/close`),
//!   `src/announcements/discord.ts` (`two:lfg:` prefix).
//! - publish: `src/discord/commandRegistry.ts` (`CommandRegistry::sync`,
//!   `mergedCommandData`) wired in `src/index.ts:493-502`.
//!
//! Routing contract (legacy order preserved per handler):
//! 1. Guild fence — the bot serves one configured guild. A mismatched (or
//!    missing) guild is `Ignore`, except moderation which answers with the
//!    legacy guild-restriction refusal.
//! 2. Env gate — a known command whose feature is off gets the legacy refusal
//!    reply (never silence: silence from a still-published command reads to
//!    Discord as "the application failed to respond").
//! 3. Permission bits — the handler-level check legacy performs even though
//!    `default_member_permissions` hides the command from non-admin pickers.
//!
//! Unknown slash names and `custom_id`s in the configured guild get a uniform
//! ephemeral reply (a deliberate improvement on legacy's silent fall-through).
//! Foreign/missing guilds remain fenced. Legacy has no modal submits; modals
//! route through the component-id table. [`replies`] owns async reply timing,
//! error redaction and panic isolation; the adapter supplies the transport.
//!
//! Publish: [`InteractionRouter::publish_set`] assembles the ONE complete
//! guild set (core + enabled features in legacy order + custom) via
//! `merge_commands`. The caller publishes it once on ready; there is no
//! partial view. Emitting it over HTTP (`set_guild_commands`) and executing
//! replies belongs to the S4 REST executor slice ([TOG-10076]); this module
//! only produces outcome enums — no private dispatcher, no HTTP client.
//!
//! Deliberately out of scope: `!<trigger>` prefix handling and sticky re-posts
//! (gateway `automationMessageAccepted`, not interactions), reaction-role
//! grant/revoke (`MessageReactionAdd/Remove`), and `/rota-acknowledge`
//! (dropped with the rota stack, matrix §9).

pub mod replies;

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use super::command_permissions::command_permission;
use super::commands::{
    core_commands, merge_commands, CommandDefinition, CustomCommand, RegistryError,
};
use super::feature_commands::{
    announcement_commands, automation_commands, scorecard_attendance_command, FeatureGates,
};
use super::moderation::{ModerationAction, ModerationGates};
use super::onboarding::{GAME_PREVIEW_SELECT_ID, GAME_SELECT_ID, SESSION_SELECT_ID};
use super::voice_rooms::voice_commands;

/// Names of the voice command set, computed once: the router consults them on
/// every slash dispatch while `TWO_VOICE=1`.
fn voice_command_names() -> &'static HashSet<String> {
    static NAMES: OnceLock<HashSet<String>> = OnceLock::new();
    NAMES.get_or_init(|| voice_commands().into_iter().map(|def| def.name).collect())
}

// --- component ids (legacy exact) --------------------------------------------

/// Ticket panel buttons (`src/discord/tickets.ts`).
pub const TICKET_OPEN_ID: &str = "two:tickets:open";
/// Ticket claim button.
pub const TICKET_CLAIM_ID: &str = "two:tickets:claim";
/// Ticket close button.
pub const TICKET_CLOSE_ID: &str = "two:tickets:close";
/// LFG signup select prefix (`src/announcements/discord.ts`).
pub const LFG_PREFIX: &str = "two:lfg:";
/// Self-role button/select prefix (`src/selfRoles/plan.ts`).
pub const SELF_ROLE_PREFIX: &str = "two:self-role:";

// --- refusal texts ------------------------------------------------------------
// Actionable denials: every refusal names the Discord permission, who to ask,
// or the admin-only enable path. Legacy one-liners live in git history; these
// are what Discord shows.

/// Automations gate: env-gated, not a Discord role — say who enables it.
pub const AUTOMATIONS_DISABLED_REPLY: &str = "Automations are disabled on this server. Ask a server admin to enable them in the bot configuration — this is a host setting, not a Discord role.";
/// Automation + feed handlers: Discord permission name plus who grants it.
pub const MANAGE_SERVER_REQUIRED: &str =
    "You need the Manage Server permission to use this command. Ask a server admin to grant it.";
/// LFG / attendance handlers: Discord permission name plus who grants it.
pub const MANAGE_EVENTS_REQUIRED: &str =
    "You need the Manage Events permission to use this command. Ask a server admin to grant it.";
/// Legacy-exact (`src/moderation/commands.ts` guild fence).
pub const GUILD_RESTRICTED_REPLY: &str = "This command is restricted to the configured guild.";
/// Announcement gate: env-gated, not a Discord role — say who enables it.
pub const ANNOUNCEMENTS_DISABLED_REPLY: &str = "Announcements are disabled on this server. Ask a server admin to enable them in the bot configuration — this is a host setting, not a Discord role.";
/// Moderation gate: env-gated, not a Discord role — say who enables it.
pub const MODERATION_DISABLED_REPLY: &str = "Moderation is not enabled on this server. Ask a server admin to enable it in the bot configuration — this is a host setting, not a Discord role.";
/// Scorecard gate: env-gated, not a Discord role — say who enables it.
pub const SCORECARD_DISABLED_REPLY: &str = "Attendance capture is not enabled on this server. Ask a server admin to enable it in the bot configuration — this is a host setting, not a Discord role.";

// --- handler identity ----------------------------------------------------------

/// Which feature slice owns a routed slash command (parity §1 row).
///
/// Sibling S4 slices implement [`InteractionHandler`] for their variants; the
/// router only names the owner — execution stays in the feature slice and
/// emits effects through the REST executor ([TOG-10076]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HandlerId {
    Rank,
    Leaderboard,
    Help,
    Privacy,
    Reopen,
    WelcomePreview,
    GameMix,
    ScorecardAttendance,
    AutomationAdmin,
    AutomationCustom,
    Giveaway,
    Rsvp,
    RsvpAttendance,
    Lfg,
    LfgClose,
    LfgEdit,
    LfgList,
    FeedAdd,
    FeedRemove,
    FeedPause,
    FeedResume,
    FeedList,
    FeedInspect,
    Moderation(ModerationAction),
    PurgePreview,
    ChannelInspect,
    LockdownPreview,
    TempbanList,
    ModerationRecent,
}

/// Which feature slice owns a routed component / modal submit (parity §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentHandler {
    GamePicker,
    SessionPicker,
    SelfRole,
    Tickets,
    LfgSignup,
    GiveawayEntry,
}

/// Contract feature slices implement so the router can hand a routed
/// interaction to its registered owner. Execution (stores, REST effects)
/// lives in the feature slice; this trait is the registration side only.
pub trait InteractionHandler: Send + Sync + std::fmt::Debug {
    /// The [`HandlerId`] this handler executes.
    fn id(&self) -> HandlerId;
}

// --- gates ---------------------------------------------------------------------

/// Everything the router needs to decide route vs refusal vs publish.
///
/// Bridges the existing gate modules: `FeatureGates` (slice 2),
/// `ModerationGates` (slice 3), plus the scorecard flag
/// (`TWO_COMMUNITY_SCORECARD`, legacy `cfg.communityScorecard`) and the
/// component-surface flags from legacy `src/index.ts` registration
/// conditions (tickets need all three Discord ids; self-roles need a
/// non-empty panel catalogue; pickers need landing channels / session mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterGates {
    /// Single configured guild (`DISCORD_GUILD_ID`); `None` fences everything.
    pub configured_guild: Option<u64>,
    /// `TWO_COMMUNITY_SCORECARD=1` — scorecard `attendance` (#12).
    pub scorecard: bool,
    /// `TWO_AUTOMATIONS=1` — automation admin + custom commands (#14–#22).
    pub automations: bool,
    /// `TWO_ANNOUNCEMENTS=1` — RSVP/LFG/feed commands (#24–#30).
    pub announcements: bool,
    /// `TWO_MODERATION=1` — moderation commands (#3–#11).
    pub moderation: bool,
    /// `TWO_VOICE=1` — the temporary-voice command set. Merged after
    /// moderation, so the voice `kick` loses first-wins to moderation
    /// `/kick`; the bot-crate voice sink answers every voice name, so the
    /// router yields silently for them.
    pub voice: bool,
    /// Assistant endpoint configured (`TWO_ASSISTANT_ENDPOINT`). Kept for
    /// the assistant slice; `/templateassistant` stays unpublished until
    /// that slice lands with a handler.
    pub voice_assistant: bool,
    /// Ticket env triple set (category + staff role + panel channel).
    pub tickets: bool,
    /// Non-empty self-role panel catalogue (`TWO_SELF_ROLE_PANELS`).
    pub self_roles: bool,
    /// Game picker live (legacy/anchor onboarding with landing channels).
    pub onboarding_picker: bool,
    /// Session picker live (`TWO_ONBOARDING_MODE=session`).
    pub session_picker: bool,
}

/// Component-surface flags for [`RouterGates::from_slices`]: the registration
/// conditions from legacy `src/index.ts` (tickets need the env triple,
/// self-roles a non-empty panel catalogue, pickers landing channels / session
/// mode). Bundled so the bridge stays under the argument limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SurfaceFlags {
    /// `TWO_COMMUNITY_SCORECARD=1` — scorecard `attendance` (#12).
    pub scorecard: bool,
    /// `TWO_VOICE=1` — the temporary-voice command set.
    pub voice: bool,
    /// Assistant endpoint configured (`TWO_ASSISTANT_ENDPOINT`).
    pub voice_assistant: bool,
    /// Ticket env triple set (category + staff role + panel channel).
    pub tickets: bool,
    /// Non-empty self-role panel catalogue (`TWO_SELF_ROLE_PANELS`).
    pub self_roles: bool,
    /// Game picker live (legacy/anchor onboarding with landing channels).
    pub onboarding_picker: bool,
    /// Session picker live (`TWO_ONBOARDING_MODE=session`).
    pub session_picker: bool,
}

impl RouterGates {
    /// Bridge the existing gate modules into one router input.
    #[must_use]
    pub fn from_slices(
        configured_guild: Option<u64>,
        features: &FeatureGates,
        moderation: &ModerationGates,
        surfaces: SurfaceFlags,
    ) -> Self {
        Self {
            configured_guild,
            scorecard: surfaces.scorecard,
            automations: features.automations,
            announcements: features.announcements,
            moderation: moderation.enabled,
            voice: surfaces.voice,
            voice_assistant: surfaces.voice_assistant,
            tickets: surfaces.tickets,
            self_roles: surfaces.self_roles,
            onboarding_picker: surfaces.onboarding_picker,
            session_picker: surfaces.session_picker,
        }
    }
}

// --- slash routing ---------------------------------------------------------------

/// Framework-free slash-command invocation: what the adapter extracted from
/// the twilight interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashContext<'a> {
    /// Invoked command name (lowercase, as Discord sends it).
    pub name: &'a str,
    /// Guild the interaction arrived in (`None` in DMs).
    pub guild_id: Option<u64>,
    /// Invoker's guild permission bits (`member.permissions`); `None` in DMs.
    pub actor_permissions: Option<u64>,
    /// DB-backed custom-command row: `Some(true)` enabled, `Some(false)`
    /// disabled row, `None` no row for this name.
    pub custom_row: Option<bool>,
}

/// Why a slash command was refused (legacy reply text via [`RouterRefusal::message`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterRefusal {
    AutomationsDisabled,
    AnnouncementsDisabled,
    ModerationDisabled,
    ScorecardDisabled,
    ManageServerRequired,
    ManageEventsRequired,
    GuildRestricted,
    ModerationPermission(ModerationAction),
    PurgePreviewPermission,
    ChannelInspectPermission,
    LockdownPreviewPermission,
    TempbanListPermission,
    ModerationRecentPermission,
}

impl RouterRefusal {
    /// User-facing denial text: Discord permission names and a next step, never
    /// an internal action id.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::AutomationsDisabled => AUTOMATIONS_DISABLED_REPLY.to_owned(),
            Self::AnnouncementsDisabled => ANNOUNCEMENTS_DISABLED_REPLY.to_owned(),
            Self::ModerationDisabled => MODERATION_DISABLED_REPLY.to_owned(),
            Self::ScorecardDisabled => SCORECARD_DISABLED_REPLY.to_owned(),
            Self::ManageServerRequired => MANAGE_SERVER_REQUIRED.to_owned(),
            Self::ManageEventsRequired => MANAGE_EVENTS_REQUIRED.to_owned(),
            Self::GuildRestricted => GUILD_RESTRICTED_REPLY.to_owned(),
            Self::PurgePreviewPermission => "You need the Manage Messages permission to use /purge-preview. Ask a server moderator or admin to grant it.".to_owned(),
            Self::ChannelInspectPermission => "You need the Manage Channels permission to use /channel-inspect. Ask a server moderator or admin to grant it.".to_owned(),
            Self::LockdownPreviewPermission => "You need the Manage Channels permission to use /lockdown-preview. Ask a server moderator or admin to grant it.".to_owned(),
            Self::TempbanListPermission => "You need the Ban Members permission to use /tempban-list. Ask a server moderator or admin to grant it.".to_owned(),
            Self::ModerationRecentPermission => "You need the Moderate Members permission to use /moderation-recent. Ask a server moderator or admin to grant it.".to_owned(),
            // Names the Discord permission (Ban Members, …) and the slash
            // command, not the internal `moderation.ban` action id.
            Self::ModerationPermission(action) => {
                format!(
                    "You need the {} permission to use /{}. Ask a server moderator or admin to grant it.",
                    action.discord_permission_name(),
                    action.command_name()
                )
            }
        }
    }
}

/// Slash-command routing outcome: plain data, no Discord calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashOutcome {
    Handled { handler: HandlerId },
    Refuse { refusal: RouterRefusal },
    Unknown,
    Ignore,
}

/// Component / modal-submit routing outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentOutcome {
    Handled { handler: ComponentHandler },
    Unknown,
    Ignore,
}

// --- router ------------------------------------------------------------------------

/// The single interaction router: slash commands by name, components and
/// modal submits by `custom_id` (exact ids, then `two:lfg:` / `two:self-role:`
/// prefixes — legacy match order, exact first).
pub struct InteractionRouter {
    gates: RouterGates,
    handlers: HashMap<HandlerId, Box<dyn InteractionHandler>>,
}

impl std::fmt::Debug for InteractionRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractionRouter")
            .field("gates", &self.gates)
            .field("handler_count", &self.handlers.len())
            .finish_non_exhaustive()
    }
}

impl InteractionRouter {
    /// Build the router over the given gates. Handlers register separately
    /// via [`InteractionRouter::register`]; routing works without them (the
    /// outcome names the owner either way).
    #[must_use]
    pub fn new(gates: RouterGates) -> Self {
        Self {
            gates,
            handlers: HashMap::new(),
        }
    }

    /// Register a feature slice's handler (test stubs here; real slices in
    /// their own cards).
    pub fn register(&mut self, handler: Box<dyn InteractionHandler>) {
        self.handlers.insert(handler.id(), handler);
    }

    /// The registered handler for a routed id, if one registered.
    #[must_use]
    pub fn handler_for(&self, id: &HandlerId) -> Option<&dyn InteractionHandler> {
        self.handlers.get(id).map(AsRef::as_ref)
    }

    /// Router gates (publish + replay assertions read these back).
    #[must_use]
    pub fn gates(&self) -> RouterGates {
        self.gates
    }

    /// Conformance seam: the full production registration set, readable
    /// without the binary crates. `router_with_commands` (bot) plus the
    /// member verbs (`CommandRuntime::build`, while the member consumer
    /// exists) plus the LFG pair (`InteractionRuntime::with_router`)
    /// register the same [`HandlerId`] set; the bot-crate
    /// `production_router_matches_composed_production_wiring` test fails on
    /// any drift between that composition and this seam. Registrations here
    /// are marker ids (execution lives in the slices); only
    /// [`InteractionRouter::handler_for`] presence is meaningful.
    #[must_use]
    pub fn production_router(gates: RouterGates, member_present: bool) -> InteractionRouter {
        #[derive(Debug)]
        struct ProductionMarker(HandlerId);
        impl InteractionHandler for ProductionMarker {
            fn id(&self) -> HandlerId {
                self.0
            }
        }
        let mut router = InteractionRouter::new(gates);
        let mut register = |id: HandlerId| router.register(Box::new(ProductionMarker(id)));
        if gates.moderation {
            // Channel slice: the non-member verbs (`register_channel_handlers`).
            for action in ModerationAction::ALL
                .into_iter()
                .filter(|a| !a.targets_member())
            {
                register(HandlerId::Moderation(action));
            }
            register(HandlerId::PurgePreview);
            register(HandlerId::ChannelInspect);
            register(HandlerId::LockdownPreview);
            register(HandlerId::ModerationRecent);
        }
        if gates.automations {
            // Sticky/admin plus custom rows (`CustomCommandRuntime::register`).
            register(HandlerId::AutomationAdmin);
            register(HandlerId::AutomationCustom);
            register(HandlerId::Giveaway);
        }
        if gates.announcements {
            for id in [
                HandlerId::FeedAdd,
                HandlerId::FeedRemove,
                HandlerId::FeedPause,
                HandlerId::FeedResume,
                HandlerId::FeedList,
                HandlerId::FeedInspect,
                HandlerId::Rsvp,
                HandlerId::RsvpAttendance,
            ] {
                register(id);
            }
        }
        if gates.scorecard {
            register(HandlerId::ScorecardAttendance);
        }
        for id in [
            HandlerId::Rank,
            HandlerId::Leaderboard,
            HandlerId::Help,
            HandlerId::Privacy,
            HandlerId::Reopen,
            HandlerId::WelcomePreview,
            HandlerId::GameMix,
        ] {
            register(id);
        }
        if member_present {
            register(HandlerId::TempbanList);
            // Member verbs (`CommandRuntime::build`, while the consumer exists).
            for action in [
                ModerationAction::Ban,
                ModerationAction::TempBan,
                ModerationAction::Kick,
                ModerationAction::Timeout,
                ModerationAction::Warn,
            ] {
                register(HandlerId::Moderation(action));
            }
        }
        // LFG management (`InteractionRuntime::with_router`, ungated).
        register(HandlerId::Lfg);
        register(HandlerId::LfgClose);
        register(HandlerId::LfgEdit);
        register(HandlerId::LfgList);
        router
    }

    /// The single place blessing routeable ids that intentionally stay
    /// unregistered: the five member verbs while no member consumer exists
    /// (their owner-map entries guard on the same condition, so the gap keeps
    /// the unavailable-reply shape instead of executing).
    #[must_use]
    pub fn intentional_registration_exceptions(member_present: bool) -> Vec<HandlerId> {
        if member_present {
            Vec::new()
        } else {
            vec![
                HandlerId::TempbanList,
                HandlerId::Moderation(ModerationAction::Ban),
                HandlerId::Moderation(ModerationAction::TempBan),
                HandlerId::Moderation(ModerationAction::Kick),
                HandlerId::Moderation(ModerationAction::Timeout),
                HandlerId::Moderation(ModerationAction::Warn),
            ]
        }
    }

    /// Whether an active voice sink owns this slash name before shared
    /// interaction admission. Builtins stay with the router (notably `/kick`).
    #[must_use]
    pub fn voice_owns_command(&self, name: &str) -> bool {
        self.gates.voice
            && voice_command_names().contains(name)
            && self.route_builtin(name, None, None).is_none()
    }

    fn guild_ok(&self, guild_id: Option<u64>) -> bool {
        guild_id.is_some_and(|g| Some(g) == self.gates.configured_guild)
    }

    fn permission_allowed(
        name: &str,
        guild_id: Option<u64>,
        actor_permissions: Option<u64>,
    ) -> bool {
        let row = command_permission(name).expect("builtin command has a permission row");
        if row.allows(actor_permissions) {
            return true;
        }
        // Metadata-only security audit: never include interaction tokens,
        // options, message bodies, or target/member display names.
        tracing::warn!(
            target: "two_bot_core::command_permissions",
            command = row.command,
            guild_id = guild_id.map(|id| id.to_string()),
            required_permissions = row.required_permissions,
            actor_permissions = actor_permissions.map(|id| id.to_string()),
            "command_permission_denied"
        );
        false
    }

    /// Route one slash command through fence → gate → permission checks.
    #[must_use]
    pub fn route_slash(&self, ctx: &SlashContext<'_>) -> SlashOutcome {
        // Built-in table first (reserved names shadow custom rows, same as
        // the publish merge).
        if let Some(outcome) = self.route_builtin(ctx.name, ctx.guild_id, ctx.actor_permissions) {
            return outcome;
        }
        // Voice names belong to the bot-crate voice sink, which answers every
        // one exactly once. Yield silently so the shared runtime never adds an
        // unknown-command reply (or runs a same-named custom row) on top. Only
        // while `TWO_VOICE=1`: with the gate off nothing is published and the
        // names stay free for custom commands.
        if self.gates.voice && voice_command_names().contains(ctx.name) {
            return SlashOutcome::Ignore;
        }
        // The assistant has no handler yet and stays unpublished until its
        // slice lands. Keep the explicit unknown-command reply in every gate
        // state, rather than executing a pre-existing custom row under the
        // squatted name (publish withholds it via `reserved_names`, and
        // custom-command validation rejects new rows).
        if ctx.name == "templateassistant" {
            return if self.guild_ok(ctx.guild_id) {
                SlashOutcome::Unknown
            } else {
                SlashOutcome::Ignore
            };
        }
        // Dynamic DB-backed custom commands (#22): everyone while automations
        // are on; explicit refusal while off. Missing/disabled rows in the
        // configured guild get the same reply as other stale interactions.
        match ctx.custom_row {
            Some(true) => {
                if !self.guild_ok(ctx.guild_id) {
                    return SlashOutcome::Ignore;
                }
                if !self.gates.automations {
                    return SlashOutcome::Refuse {
                        refusal: RouterRefusal::AutomationsDisabled,
                    };
                }
                SlashOutcome::Handled {
                    handler: HandlerId::AutomationCustom,
                }
            }
            Some(false) | None if self.guild_ok(ctx.guild_id) => SlashOutcome::Unknown,
            Some(false) | None => SlashOutcome::Ignore,
        }
    }

    /// Built-in §1 rows. `None` = not a builtin (caller falls through to the
    /// custom-command path).
    fn route_builtin(
        &self,
        name: &str,
        guild_id: Option<u64>,
        actor_permissions: Option<u64>,
    ) -> Option<SlashOutcome> {
        // Moderation carries its own guild fence (refusal, not silence).
        let moderation = match name {
            "tempban-list" => Some((HandlerId::TempbanList, RouterRefusal::TempbanListPermission)),
            "purge-preview" => Some((
                HandlerId::PurgePreview,
                RouterRefusal::PurgePreviewPermission,
            )),
            "channel-inspect" => Some((
                HandlerId::ChannelInspect,
                RouterRefusal::ChannelInspectPermission,
            )),
            // Dark until the publication slice: routed and adjudicated like
            // the other channel reads, but with no published definition.
            "lockdown-preview" => Some((
                HandlerId::LockdownPreview,
                RouterRefusal::LockdownPreviewPermission,
            )),
            "moderation-recent" => Some((
                HandlerId::ModerationRecent,
                RouterRefusal::ModerationRecentPermission,
            )),
            _ => ModerationAction::ALL
                .iter()
                .find(|a| a.command_name() == name)
                .map(|action| {
                    (
                        HandlerId::Moderation(*action),
                        RouterRefusal::ModerationPermission(*action),
                    )
                }),
        };
        if let Some((handler, permission_refusal)) = moderation {
            if !self.guild_ok(guild_id) {
                return Some(SlashOutcome::Refuse {
                    refusal: RouterRefusal::GuildRestricted,
                });
            }
            if !self.gates.moderation {
                return Some(SlashOutcome::Refuse {
                    refusal: RouterRefusal::ModerationDisabled,
                });
            }
            if !Self::permission_allowed(name, guild_id, actor_permissions) {
                return Some(SlashOutcome::Refuse {
                    refusal: permission_refusal,
                });
            }
            return Some(SlashOutcome::Handled { handler });
        }

        /// Which feature gate a builtin row needs.
        #[derive(Debug, Clone, Copy)]
        enum RowGate {
            Always,
            Automations,
            Announcements,
            Scorecard,
        }

        impl RowGate {
            fn refusal(self) -> Option<RouterRefusal> {
                match self {
                    Self::Always => None,
                    Self::Automations => Some(RouterRefusal::AutomationsDisabled),
                    Self::Announcements => Some(RouterRefusal::AnnouncementsDisabled),
                    Self::Scorecard => Some(RouterRefusal::ScorecardDisabled),
                }
            }

            fn enabled(self, gates: &RouterGates) -> bool {
                match self {
                    Self::Always => true,
                    Self::Automations => gates.automations,
                    Self::Announcements => gates.announcements,
                    Self::Scorecard => gates.scorecard,
                }
            }
        }

        struct Row {
            handler: HandlerId,
            gate: RowGate,
            permission_refusal: Option<RouterRefusal>,
        }

        let row = match name {
            "rank" => Row {
                handler: HandlerId::Rank,
                gate: RowGate::Always,
                permission_refusal: None,
            },
            "leaderboard" => Row {
                handler: HandlerId::Leaderboard,
                gate: RowGate::Always,
                permission_refusal: None,
            },
            // Next-only discovery surface: always on, open to
            // everyone, answered from the live publish set.
            "help" => Row {
                handler: HandlerId::Help,
                gate: RowGate::Always,
                permission_refusal: None,
            },
            // Member privacy self-serve: always on, open to everyone.
            // The subject is always the invoker (no target option), so no
            // permission gate applies; guild scoping is enforced by the
            // guild fence above plus the handler's own guild check.
            "privacy" => Row {
                handler: HandlerId::Privacy,
                gate: RowGate::Always,
                permission_refusal: None,
            },
            // Self-service picker recovery: always on, open to everyone.
            // The subject is always the invoker (a `kind` choice, no target
            // option), so no permission gate applies; an unavailable kind
            // gets private no-effect guidance from the handler after the
            // defer, where the kind is visible — the router never sees
            // options, so kind-level enablement cannot live here.
            "reopen" => Row {
                handler: HandlerId::Reopen,
                gate: RowGate::Always,
                permission_refusal: None,
            },
            "welcome-preview" => Row {
                handler: HandlerId::WelcomePreview,
                gate: RowGate::Always,
                permission_refusal: Some(RouterRefusal::ManageServerRequired),
            },
            "game-mix" => Row {
                handler: HandlerId::GameMix,
                gate: RowGate::Always,
                permission_refusal: Some(RouterRefusal::ManageServerRequired),
            },
            // Scorecard check-in gates `ManageEvents` both in the published
            // definition (`feature_commands.rs`) and at dispatch (`rsvp.rs`
            // `require_manage_events`): Discord picker hiding is not
            // authorization, so the router enforces it server-side too.
            "attendance" => Row {
                handler: HandlerId::ScorecardAttendance,
                gate: RowGate::Scorecard,
                permission_refusal: Some(RouterRefusal::ManageEventsRequired),
            },
            "command" | "command-remove" | "command-list" | "command-inspect"
            | "command-disable" | "command-enable" | "schedule" | "schedule-remove"
            | "schedule-list" | "sticky" | "sticky-remove" => Row {
                handler: HandlerId::AutomationAdmin,
                gate: RowGate::Automations,
                permission_refusal: Some(RouterRefusal::ManageServerRequired),
            },
            "giveaway" => Row {
                handler: HandlerId::Giveaway,
                gate: RowGate::Automations,
                permission_refusal: Some(RouterRefusal::ManageServerRequired),
            },
            "rsvp" => Row {
                handler: HandlerId::Rsvp,
                gate: RowGate::Announcements,
                permission_refusal: None,
            },
            // Namespaced: legacy RSVP-totals `attendance` collides with the
            // scorecard `attendance` on `guild.commands.set` — see
            // `commands.rs`. The two names route to different handlers.
            "rsvp-attendance" => Row {
                handler: HandlerId::RsvpAttendance,
                gate: RowGate::Announcements,
                permission_refusal: None,
            },
            "lfg" => Row {
                handler: HandlerId::Lfg,
                gate: RowGate::Announcements,
                permission_refusal: Some(RouterRefusal::ManageEventsRequired),
            },
            "lfg-close" => Row {
                handler: HandlerId::LfgClose,
                gate: RowGate::Announcements,
                permission_refusal: Some(RouterRefusal::ManageEventsRequired),
            },
            "lfg-edit" => Row {
                handler: HandlerId::LfgEdit,
                gate: RowGate::Announcements,
                permission_refusal: Some(RouterRefusal::ManageEventsRequired),
            },
            "lfg-list" => Row {
                handler: HandlerId::LfgList,
                gate: RowGate::Announcements,
                permission_refusal: Some(RouterRefusal::ManageEventsRequired),
            },
            "feed-add" => Row {
                handler: HandlerId::FeedAdd,
                gate: RowGate::Announcements,
                permission_refusal: Some(RouterRefusal::ManageServerRequired),
            },
            "feed-remove" => Row {
                handler: HandlerId::FeedRemove,
                gate: RowGate::Announcements,
                permission_refusal: Some(RouterRefusal::ManageServerRequired),
            },
            "feed-pause" => Row {
                handler: HandlerId::FeedPause,
                gate: RowGate::Announcements,
                permission_refusal: Some(RouterRefusal::ManageServerRequired),
            },
            "feed-resume" => Row {
                handler: HandlerId::FeedResume,
                gate: RowGate::Announcements,
                permission_refusal: Some(RouterRefusal::ManageServerRequired),
            },
            "feed-list" => Row {
                handler: HandlerId::FeedList,
                gate: RowGate::Announcements,
                permission_refusal: Some(RouterRefusal::ManageServerRequired),
            },
            "feed-inspect" => Row {
                handler: HandlerId::FeedInspect,
                gate: RowGate::Announcements,
                permission_refusal: Some(RouterRefusal::ManageServerRequired),
            },
            _ => return None,
        };

        if !self.guild_ok(guild_id) {
            return Some(SlashOutcome::Ignore);
        }
        if !row.gate.enabled(&self.gates) {
            return Some(SlashOutcome::Refuse {
                refusal: row.gate.refusal().expect("gated row has a refusal"),
            });
        }
        if !Self::permission_allowed(name, guild_id, actor_permissions) {
            return Some(SlashOutcome::Refuse {
                refusal: row
                    .permission_refusal
                    .expect("restricted command has a permission refusal"),
            });
        }
        Some(SlashOutcome::Handled {
            handler: row.handler,
        })
    }

    /// Route one message component by `custom_id` (exact ids, then prefixes).
    #[must_use]
    pub fn route_component(&self, custom_id: &str, guild_id: Option<u64>) -> ComponentOutcome {
        let handler = if custom_id == GAME_SELECT_ID || custom_id == GAME_PREVIEW_SELECT_ID {
            if !self.gates.onboarding_picker || !self.guild_ok(guild_id) {
                return ComponentOutcome::Ignore;
            }
            ComponentHandler::GamePicker
        } else if custom_id == SESSION_SELECT_ID {
            if !self.gates.session_picker || !self.guild_ok(guild_id) {
                return ComponentOutcome::Ignore;
            }
            ComponentHandler::SessionPicker
        } else if custom_id == TICKET_OPEN_ID
            || custom_id == TICKET_CLAIM_ID
            || custom_id == TICKET_CLOSE_ID
        {
            if !self.gates.tickets || !self.guild_ok(guild_id) {
                return ComponentOutcome::Ignore;
            }
            ComponentHandler::Tickets
        } else if let Some(_suffix) = custom_id.strip_prefix(LFG_PREFIX) {
            if !self.gates.announcements || !self.guild_ok(guild_id) {
                return ComponentOutcome::Ignore;
            }
            ComponentHandler::LfgSignup
        } else if custom_id.strip_prefix(SELF_ROLE_PREFIX).is_some() {
            if !self.gates.self_roles || !self.guild_ok(guild_id) {
                return ComponentOutcome::Ignore;
            }
            ComponentHandler::SelfRole
        } else if custom_id.starts_with("gw:") {
            if !self.gates.automations || !self.guild_ok(guild_id) {
                return ComponentOutcome::Ignore;
            }
            ComponentHandler::GiveawayEntry
        } else {
            return if self.guild_ok(guild_id) {
                ComponentOutcome::Unknown
            } else {
                ComponentOutcome::Ignore
            };
        };
        ComponentOutcome::Handled { handler }
    }

    /// Route one modal submit by `custom_id` through the same table as
    /// components. Unknown ids reply only inside the configured guild.
    #[must_use]
    pub fn route_modal(&self, custom_id: &str, guild_id: Option<u64>) -> ComponentOutcome {
        self.route_component(custom_id, guild_id)
    }

    /// Every built-in slash name the dispatch table recognizes, independent
    /// of gates. [`InteractionRouter::route_slash`] matches builtins before
    /// custom rows — and refuses the disabled ones rather than falling
    /// through — so a custom row under one of these names could never
    /// execute. Publish reserves the same set.
    fn all_builtin_names() -> HashSet<String> {
        core_commands()
            .iter()
            .chain(std::iter::once(&scorecard_attendance_command()))
            .chain(automation_commands().iter())
            .chain(announcement_commands().iter())
            .chain(super::moderation::moderation_commands().iter())
            .map(|def| def.name.clone())
            .collect()
    }

    /// Builtin names plus the voice names while `TWO_VOICE=1`: a published
    /// voice command shadows a same-named custom row exactly like a builtin
    /// (dispatch yields to the voice sink above), but with the gate off the
    /// names stay free for custom commands. `/templateassistant` is squatted
    /// unconditionally until its slice lands: dispatch shadows the name (see
    /// `route_slash`), so a custom row under it could publish yet never
    /// execute.
    fn reserved_names(&self) -> HashSet<String> {
        let mut reserved = Self::all_builtin_names();
        if self.gates.voice {
            reserved.extend(voice_command_names().iter().cloned());
        }
        reserved.insert("templateassistant".to_owned());
        reserved
    }

    /// Assemble the ONE complete guild command set for publish-on-ready
    /// (legacy `CommandRegistry::sync` order: community, automation,
    /// announcement, moderation, then the voice set — then DB custom
    /// commands). `/templateassistant` stays unpublished until its slice
    /// lands with a handler: no published command may lack one. First-wins
    /// dedupe and the 100-command ceiling come from `merge_commands`; voice
    /// `kick` loses to moderation `/kick` there.
    ///
    /// Two publish/routing agreements keep a published command executable:
    /// - custom rows publish only while automations are on. Every custom
    ///   invocation refuses as [`RouterRefusal::AutomationsDisabled`] while
    ///   off, so leaving them in the set only burns the ceiling.
    /// - every built-in name is reserved even when its feature is off.
    ///   Dispatch matches builtins first (moderation included) and refuses
    ///   the disabled row, so a same-named custom command would publish yet
    ///   never execute.
    pub fn publish_set(
        &self,
        custom: &[CustomCommand],
    ) -> Result<Vec<CommandDefinition>, RegistryError> {
        let mut extra: Vec<Vec<CommandDefinition>> = Vec::with_capacity(6);
        if self.gates.scorecard {
            extra.push(vec![scorecard_attendance_command()]);
        }
        if self.gates.automations {
            extra.push(automation_commands());
        }
        if self.gates.announcements {
            extra.push(announcement_commands());
        }
        if self.gates.moderation {
            extra.push(super::moderation::moderation_commands());
        }
        if self.gates.voice {
            extra.push(voice_commands());
            // No assistant push: the name stays reserved (see
            // `reserved_names`) and dispatch keeps shadowing it, so the
            // slice re-activates by publishing again.
        }
        // `merge_commands` reserves the active builtins; the router additionally
        // withholds disabled builtin names (same precedence as dispatch) and
        // all custom rows while automations are off. `merge_commands` still
        // applies its enabled/dedupe/ceiling rules on top.
        let reserved = self.reserved_names();
        let visible: Vec<CustomCommand> = if self.gates.automations {
            custom
                .iter()
                .filter(|cmd| !reserved.contains(&cmd.name))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        merge_commands(&extra, &visible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{PERM_MANAGE_EVENTS, PERM_MANAGE_GUILD};
    use std::collections::HashMap;

    const GUILD: u64 = 2222;

    fn all_on() -> RouterGates {
        RouterGates {
            configured_guild: Some(GUILD),
            scorecard: true,
            automations: true,
            announcements: true,
            moderation: true,
            voice: false,
            voice_assistant: false,
            tickets: true,
            self_roles: true,
            onboarding_picker: true,
            session_picker: true,
        }
    }

    fn router() -> InteractionRouter {
        InteractionRouter::new(all_on())
    }

    /// Context builder: name borrows, guild and permissions copy.
    fn ctx(name: &str, guild_id: Option<u64>, perms: Option<u64>) -> SlashContext<'_> {
        SlashContext {
            name,
            guild_id,
            actor_permissions: perms,
            custom_row: None,
        }
    }

    #[derive(Debug)]
    struct Stub(HandlerId);

    impl InteractionHandler for Stub {
        fn id(&self) -> HandlerId {
            self.0
        }
    }

    #[test]
    fn every_section1_row_routes_to_its_handler() {
        let r = router();
        let cases: &[(&str, HandlerId)] = &[
            ("rank", HandlerId::Rank),
            ("leaderboard", HandlerId::Leaderboard),
            ("help", HandlerId::Help),
            ("privacy", HandlerId::Privacy),
            ("reopen", HandlerId::Reopen),
            ("welcome-preview", HandlerId::WelcomePreview),
            ("game-mix", HandlerId::GameMix),
            ("ban", HandlerId::Moderation(ModerationAction::Ban)),
            ("tempban", HandlerId::Moderation(ModerationAction::TempBan)),
            ("tempban-list", HandlerId::TempbanList),
            ("kick", HandlerId::Moderation(ModerationAction::Kick)),
            ("timeout", HandlerId::Moderation(ModerationAction::Timeout)),
            ("warn", HandlerId::Moderation(ModerationAction::Warn)),
            ("purge", HandlerId::Moderation(ModerationAction::Purge)),
            (
                "slowmode",
                HandlerId::Moderation(ModerationAction::Slowmode),
            ),
            (
                "lockdown",
                HandlerId::Moderation(ModerationAction::Lockdown),
            ),
            ("unlock", HandlerId::Moderation(ModerationAction::Unlock)),
            ("purge-preview", HandlerId::PurgePreview),
            ("channel-inspect", HandlerId::ChannelInspect),
            ("lockdown-preview", HandlerId::LockdownPreview),
            ("moderation-recent", HandlerId::ModerationRecent),
            ("attendance", HandlerId::ScorecardAttendance),
            ("command", HandlerId::AutomationAdmin),
            ("command-remove", HandlerId::AutomationAdmin),
            ("command-list", HandlerId::AutomationAdmin),
            ("command-inspect", HandlerId::AutomationAdmin),
            ("command-disable", HandlerId::AutomationAdmin),
            ("command-enable", HandlerId::AutomationAdmin),
            ("schedule", HandlerId::AutomationAdmin),
            ("schedule-remove", HandlerId::AutomationAdmin),
            ("schedule-list", HandlerId::AutomationAdmin),
            ("sticky", HandlerId::AutomationAdmin),
            ("sticky-remove", HandlerId::AutomationAdmin),
            ("giveaway", HandlerId::Giveaway),
            ("rsvp", HandlerId::Rsvp),
            ("rsvp-attendance", HandlerId::RsvpAttendance),
            ("lfg", HandlerId::Lfg),
            ("lfg-close", HandlerId::LfgClose),
            ("lfg-edit", HandlerId::LfgEdit),
            ("lfg-list", HandlerId::LfgList),
            ("feed-add", HandlerId::FeedAdd),
            ("feed-remove", HandlerId::FeedRemove),
            ("feed-pause", HandlerId::FeedPause),
            ("feed-resume", HandlerId::FeedResume),
            ("feed-list", HandlerId::FeedList),
            ("feed-inspect", HandlerId::FeedInspect),
        ];
        assert_eq!(cases.len(), 45, "all 45 handler-owned builtins covered");
        for (name, handler) in cases {
            assert_eq!(
                r.route_slash(&ctx(name, Some(GUILD), Some(u64::MAX))),
                SlashOutcome::Handled { handler: *handler },
                "{name} routes to its handler",
            );
        }
    }

    #[test]
    fn attendance_namespacing_holds() {
        // Parity §1 #12 vs #25: scorecard keeps `attendance`, RSVP totals is
        // `rsvp-attendance` — different names, different handlers, both live.
        let r = router();
        assert_eq!(
            r.route_slash(&ctx("attendance", Some(GUILD), Some(u64::MAX))),
            SlashOutcome::Handled {
                handler: HandlerId::ScorecardAttendance
            }
        );
        assert_eq!(
            r.route_slash(&ctx("rsvp-attendance", Some(GUILD), Some(0))),
            SlashOutcome::Handled {
                handler: HandlerId::RsvpAttendance
            }
        );
    }

    #[test]
    fn registered_stubs_receive_their_routes() {
        let mut r = router();
        for id in [
            HandlerId::Rank,
            HandlerId::ScorecardAttendance,
            HandlerId::AutomationAdmin,
            HandlerId::AutomationCustom,
            HandlerId::RsvpAttendance,
            HandlerId::Lfg,
            HandlerId::LfgEdit,
            HandlerId::Moderation(ModerationAction::Ban),
            HandlerId::FeedList,
        ] {
            r.register(Box::new(Stub(id)));
        }
        for (name, id) in [
            ("rank", HandlerId::Rank),
            ("attendance", HandlerId::ScorecardAttendance),
            ("schedule", HandlerId::AutomationAdmin),
            ("rsvp-attendance", HandlerId::RsvpAttendance),
            ("lfg", HandlerId::Lfg),
            ("lfg-edit", HandlerId::LfgEdit),
            ("ban", HandlerId::Moderation(ModerationAction::Ban)),
            ("feed-list", HandlerId::FeedList),
        ] {
            let outcome = r.route_slash(&ctx(name, Some(GUILD), Some(u64::MAX)));
            let SlashOutcome::Handled { handler } = outcome else {
                panic!("{name} must route");
            };
            assert_eq!(handler, id);
            assert!(r.handler_for(&handler).is_some_and(|h| h.id() == id));
        }
        // Unregistered handler id: routes fine, no stub attached.
        let outcome = r.route_slash(&ctx("rsvp", Some(GUILD), Some(0)));
        assert_eq!(
            outcome,
            SlashOutcome::Handled {
                handler: HandlerId::Rsvp
            }
        );
        assert!(r.handler_for(&HandlerId::Rsvp).is_none());
    }

    #[test]
    fn disabled_features_refuse_not_silence() {
        let off = RouterGates {
            scorecard: false,
            automations: false,
            announcements: false,
            moderation: false,
            ..all_on()
        };
        let r = InteractionRouter::new(off);
        for (name, refusal) in [
            ("attendance", RouterRefusal::ScorecardDisabled),
            ("command", RouterRefusal::AutomationsDisabled),
            ("schedule-list", RouterRefusal::AutomationsDisabled),
            ("rsvp", RouterRefusal::AnnouncementsDisabled),
            ("rsvp-attendance", RouterRefusal::AnnouncementsDisabled),
            ("lfg", RouterRefusal::AnnouncementsDisabled),
            ("lfg-edit", RouterRefusal::AnnouncementsDisabled),
            ("feed-list", RouterRefusal::AnnouncementsDisabled),
            ("ban", RouterRefusal::ModerationDisabled),
            ("tempban-list", RouterRefusal::ModerationDisabled),
            ("purge", RouterRefusal::ModerationDisabled),
            ("purge-preview", RouterRefusal::ModerationDisabled),
            ("channel-inspect", RouterRefusal::ModerationDisabled),
            ("moderation-recent", RouterRefusal::ModerationDisabled),
        ] {
            assert_eq!(
                r.route_slash(&ctx(name, Some(GUILD), Some(u64::MAX))),
                SlashOutcome::Refuse { refusal },
                "{name} refuses while disabled",
            );
        }
        // Always-on core still routes.
        assert_eq!(
            r.route_slash(&ctx("rank", Some(GUILD), Some(0))),
            SlashOutcome::Handled {
                handler: HandlerId::Rank
            }
        );
    }

    #[test]
    fn refusal_texts_are_actionable() {
        // Disabled features name the admin-only enable path (host setting, not
        // a Discord role).
        for refusal in [
            RouterRefusal::AutomationsDisabled,
            RouterRefusal::AnnouncementsDisabled,
            RouterRefusal::ModerationDisabled,
            RouterRefusal::ScorecardDisabled,
        ] {
            let text = refusal.message();
            assert!(
                text.contains("server admin") && text.contains("host setting, not a Discord role"),
                "{refusal:?} names the enable path: {text}"
            );
        }
        // Permission denials name the Discord permission and who grants it —
        // never an internal action id.
        assert_eq!(
            RouterRefusal::ManageServerRequired.message(),
            "You need the Manage Server permission to use this command. Ask a server admin to grant it."
        );
        assert_eq!(
            RouterRefusal::ManageEventsRequired.message(),
            "You need the Manage Events permission to use this command. Ask a server admin to grant it."
        );
        assert_eq!(
            RouterRefusal::GuildRestricted.message(),
            "This command is restricted to the configured guild."
        );
        assert_eq!(
            RouterRefusal::ModerationPermission(ModerationAction::Ban).message(),
            "You need the Ban Members permission to use /ban. Ask a server moderator or admin to grant it."
        );
        assert_eq!(
            RouterRefusal::ModerationPermission(ModerationAction::Kick).message(),
            "You need the Kick Members permission to use /kick. Ask a server moderator or admin to grant it."
        );
        assert_eq!(
            RouterRefusal::ModerationPermission(ModerationAction::Timeout).message(),
            "You need the Moderate Members permission to use /timeout. Ask a server moderator or admin to grant it."
        );
        assert_eq!(
            RouterRefusal::ModerationPermission(ModerationAction::Purge).message(),
            "You need the Manage Messages permission to use /purge. Ask a server moderator or admin to grant it."
        );
        assert_eq!(
            RouterRefusal::ModerationPermission(ModerationAction::Slowmode).message(),
            "You need the Manage Channels permission to use /slowmode. Ask a server moderator or admin to grant it."
        );
        for action in ModerationAction::ALL {
            let text = RouterRefusal::ModerationPermission(action).message();
            assert!(
                !text.contains("moderation."),
                "{action:?} must not leak the internal id: {text}"
            );
        }
    }

    #[test]
    fn private_read_permissions_name_the_invoked_command_and_never_fall_through() {
        let r = router();
        for (name, required, handler, refusal, expected) in [
            ("tempban-list", 4, HandlerId::TempbanList, RouterRefusal::TempbanListPermission,
             "You need the Ban Members permission to use /tempban-list. Ask a server moderator or admin to grant it."),
            ("purge-preview", 8192, HandlerId::PurgePreview, RouterRefusal::PurgePreviewPermission,
             "You need the Manage Messages permission to use /purge-preview. Ask a server moderator or admin to grant it."),
            ("channel-inspect", 16, HandlerId::ChannelInspect, RouterRefusal::ChannelInspectPermission,
             "You need the Manage Channels permission to use /channel-inspect. Ask a server moderator or admin to grant it."),
            ("lockdown-preview", 16, HandlerId::LockdownPreview, RouterRefusal::LockdownPreviewPermission,
             "You need the Manage Channels permission to use /lockdown-preview. Ask a server moderator or admin to grant it."),
            ("moderation-recent", 1_099_511_627_776, HandlerId::ModerationRecent, RouterRefusal::ModerationRecentPermission,
             "You need the Moderate Members permission to use /moderation-recent. Ask a server moderator or admin to grant it."),
        ] {
            assert_eq!(refusal.message(), expected);
            for permissions in [None, Some(0), Some(8), Some(!required)] {
                assert_eq!(
                    r.route_slash(&SlashContext { custom_row: Some(true), ..ctx(name, Some(GUILD), permissions) }),
                    SlashOutcome::Refuse { refusal },
                    "/{name} cannot borrow custom-command or administrator-only authority"
                );
            }
            assert_eq!(
                r.route_slash(&ctx(name, Some(GUILD), Some(required))),
                SlashOutcome::Handled { handler }
            );
            for guild in [None, Some(9999)] {
                assert_eq!(
                    r.route_slash(&ctx(name, guild, Some(u64::MAX))),
                    SlashOutcome::Refuse { refusal: RouterRefusal::GuildRestricted }
                );
            }
            let unconfigured = InteractionRouter::new(RouterGates { configured_guild: None, ..all_on() });
            assert_eq!(
                unconfigured.route_slash(&ctx(name, Some(GUILD), Some(u64::MAX))),
                SlashOutcome::Refuse { refusal: RouterRefusal::GuildRestricted }
            );
            let off = InteractionRouter::new(RouterGates { moderation: false, ..all_on() });
            assert_eq!(
                off.route_slash(&SlashContext { custom_row: Some(true), ..ctx(name, Some(GUILD), Some(u64::MAX)) }),
                SlashOutcome::Refuse { refusal: RouterRefusal::ModerationDisabled }
            );
            let set = off.publish_set(&[CustomCommand { name: name.to_owned(), description: "shadow".to_owned(), enabled: true }]).expect("assembles");
            assert!(!set.iter().any(|definition| definition.name == name));
        }
    }

    #[test]
    fn permission_checks_match_legacy() {
        let r = router();
        // Automation admin without ManageGuild.
        assert_eq!(
            r.route_slash(&ctx("command", Some(GUILD), Some(0))),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::ManageServerRequired
            }
        );
        // LFG without ManageEvents.
        assert_eq!(
            r.route_slash(&ctx("lfg", Some(GUILD), Some(0))),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::ManageEventsRequired
            }
        );
        // LFG with exactly ManageEvents passes.
        assert_eq!(
            r.route_slash(&ctx("lfg", Some(GUILD), Some(PERM_MANAGE_EVENTS))),
            SlashOutcome::Handled {
                handler: HandlerId::Lfg
            }
        );
        for (name, handler) in [
            ("lfg-close", HandlerId::LfgClose),
            ("lfg-edit", HandlerId::LfgEdit),
        ] {
            for permissions in [None, Some(0), Some(8), Some(PERM_MANAGE_GUILD)] {
                assert_eq!(
                    r.route_slash(&ctx(name, Some(GUILD), permissions)),
                    SlashOutcome::Refuse {
                        refusal: RouterRefusal::ManageEventsRequired
                    },
                    "{name} needs resolved ManageEvents"
                );
            }
            assert_eq!(
                r.route_slash(&ctx(name, Some(GUILD), Some(PERM_MANAGE_EVENTS))),
                SlashOutcome::Handled { handler },
                "{name} accepts exactly ManageEvents"
            );
        }
        // Feed without ManageGuild.
        assert_eq!(
            r.route_slash(&ctx("feed-add", Some(GUILD), Some(PERM_MANAGE_EVENTS))),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::ManageServerRequired
            }
        );
        // Moderation without the verb's bit.
        assert_eq!(
            r.route_slash(&ctx("ban", Some(GUILD), Some(0))),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::ModerationPermission(ModerationAction::Ban)
            }
        );
        // DM (no permissions) refuses on gated commands, routes open ones
        // only when the guild fence passes — DMs never pass the fence.
        assert_eq!(
            r.route_slash(&ctx("rank", None, None)),
            SlashOutcome::Ignore
        );
    }

    #[test]
    fn giveaway_requires_manage_guild_for_its_dedicated_handler() {
        let r = router();
        for permissions in [None, Some(0), Some(PERM_MANAGE_EVENTS)] {
            assert_eq!(
                r.route_slash(&ctx("giveaway", Some(GUILD), permissions)),
                SlashOutcome::Refuse {
                    refusal: RouterRefusal::ManageServerRequired
                },
                "giveaway refuses without Manage Guild: {permissions:?}",
            );
        }
        assert_eq!(
            r.route_slash(&ctx("giveaway", Some(GUILD), Some(PERM_MANAGE_GUILD))),
            SlashOutcome::Handled {
                handler: HandlerId::Giveaway
            }
        );
    }

    #[test]
    fn giveaway_automation_gate_precedes_its_permission_check() {
        let off = InteractionRouter::new(RouterGates {
            automations: false,
            ..all_on()
        });
        for permissions in [None, Some(0), Some(PERM_MANAGE_GUILD), Some(u64::MAX)] {
            assert_eq!(
                off.route_slash(&ctx("giveaway", Some(GUILD), permissions)),
                SlashOutcome::Refuse {
                    refusal: RouterRefusal::AutomationsDisabled
                },
                "disabled giveaway refuses before checking permissions: {permissions:?}",
            );
        }
    }

    #[test]
    fn giveaway_guild_fence_precedes_its_feature_and_permission_gates() {
        for (configured_guild, guild_id) in [
            (Some(GUILD), None),
            (Some(GUILD), Some(9999)),
            (None, None),
            (None, Some(GUILD)),
            (None, Some(9999)),
        ] {
            for automations in [false, true] {
                let r = InteractionRouter::new(RouterGates {
                    configured_guild,
                    automations,
                    ..all_on()
                });
                for permissions in [None, Some(u64::MAX)] {
                    assert_eq!(
                        r.route_slash(&ctx("giveaway", guild_id, permissions)),
                        SlashOutcome::Ignore,
                        "giveaway stays fenced: configured={configured_guild:?}, guild={guild_id:?}, automations={automations}, permissions={permissions:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn attendance_requires_manage_events() {
        let r = router();
        // Denied and missing permission bits refuse, mirroring
        // `rsvp::require_manage_events`; Discord picker hiding is not
        // authorization.
        assert_eq!(
            r.route_slash(&ctx("attendance", Some(GUILD), Some(0))),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::ManageEventsRequired
            }
        );
        assert_eq!(
            r.route_slash(&ctx("attendance", Some(GUILD), None)),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::ManageEventsRequired
            }
        );
        assert_eq!(
            r.route_slash(&ctx("attendance", Some(GUILD), Some(PERM_MANAGE_EVENTS))),
            SlashOutcome::Handled {
                handler: HandlerId::ScorecardAttendance
            }
        );
    }

    #[test]
    fn picker_components_enforce_the_guild_fence() {
        let r = router();
        let unconfigured = InteractionRouter::new(RouterGates {
            configured_guild: None,
            ..all_on()
        });
        for id in [GAME_SELECT_ID, "two:self-role:games"] {
            // Foreign guild, missing guild, and unconfigured router: silence
            // on both the component and modal paths (modals share the table).
            for (router, guild, label) in [
                (&r, Some(9999), "foreign guild"),
                (&r, None, "missing guild"),
                (&unconfigured, Some(GUILD), "unconfigured router"),
                (&unconfigured, None, "unconfigured router, no guild"),
            ] {
                assert_eq!(
                    router.route_component(id, guild),
                    ComponentOutcome::Ignore,
                    "{id} ignores {label}"
                );
                assert_eq!(
                    router.route_modal(id, guild),
                    ComponentOutcome::Ignore,
                    "{id} modal ignores {label}"
                );
            }
        }
    }

    #[test]
    fn guild_fence_binds_everything() {
        let r = router();
        // Wrong guild: silence everywhere except moderation's refusal.
        for name in ["rank", "attendance", "command", "rsvp", "lfg", "feed-list"] {
            assert_eq!(
                r.route_slash(&ctx(name, Some(9999), Some(u64::MAX))),
                SlashOutcome::Ignore,
                "{name} ignores foreign guilds",
            );
        }
        assert_eq!(
            r.route_slash(&ctx("ban", Some(9999), Some(u64::MAX))),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::GuildRestricted
            }
        );
        // Unconfigured bot (no guild): moderation still refuses, rest ignore.
        let unconfigured = InteractionRouter::new(RouterGates {
            configured_guild: None,
            ..all_on()
        });
        assert_eq!(
            unconfigured.route_slash(&ctx("kick", Some(GUILD), Some(u64::MAX))),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::GuildRestricted
            }
        );
        assert_eq!(
            unconfigured.route_slash(&ctx("rank", Some(GUILD), Some(0))),
            SlashOutcome::Ignore
        );
    }

    #[test]
    fn custom_commands_follow_the_automation_gate() {
        let r = router();
        let custom = |row| SlashContext {
            name: "faq",
            guild_id: Some(GUILD),
            actor_permissions: Some(0),
            custom_row: row,
        };
        // Enabled row, everyone (no perm gate on dynamic commands).
        assert_eq!(
            r.route_slash(&custom(Some(true))),
            SlashOutcome::Handled {
                handler: HandlerId::AutomationCustom
            }
        );
        // Disabled/missing rows now get the uniform stale-interaction reply.
        assert_eq!(r.route_slash(&custom(Some(false))), SlashOutcome::Unknown);
        assert_eq!(r.route_slash(&custom(None)), SlashOutcome::Unknown);
        // Builtin names shadow custom rows (publish merge does the same).
        let shadow = SlashContext {
            name: "rank",
            custom_row: Some(true),
            ..custom(None)
        };
        assert_eq!(
            r.route_slash(&shadow),
            SlashOutcome::Handled {
                handler: HandlerId::Rank
            }
        );
        // Feature off: existing row gets the explicit refusal.
        let off = InteractionRouter::new(RouterGates {
            automations: false,
            ..all_on()
        });
        assert_eq!(
            off.route_slash(&custom(Some(true))),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::AutomationsDisabled
            }
        );
    }

    #[test]
    fn unknown_slash_names_reply_only_inside_the_guild_fence() {
        let r = router();
        for name in ["rota-acknowledge", "definitely-not-a-command"] {
            assert_eq!(
                r.route_slash(&ctx(name, Some(GUILD), Some(u64::MAX))),
                SlashOutcome::Unknown
            );
            assert_eq!(
                r.route_slash(&ctx(name, Some(9999), Some(u64::MAX))),
                SlashOutcome::Ignore
            );
            assert_eq!(r.route_slash(&ctx(name, None, None)), SlashOutcome::Ignore);
        }
    }

    #[test]
    fn component_prefix_routing() {
        let r = router();
        assert_eq!(
            r.route_component(GAME_SELECT_ID, Some(GUILD)),
            ComponentOutcome::Handled {
                handler: ComponentHandler::GamePicker
            }
        );
        assert_eq!(
            r.route_component(GAME_PREVIEW_SELECT_ID, Some(GUILD)),
            ComponentOutcome::Handled {
                handler: ComponentHandler::GamePicker
            }
        );
        assert_eq!(
            r.route_component(SESSION_SELECT_ID, Some(GUILD)),
            ComponentOutcome::Handled {
                handler: ComponentHandler::SessionPicker
            }
        );
        for id in [TICKET_OPEN_ID, TICKET_CLAIM_ID, TICKET_CLOSE_ID] {
            assert_eq!(
                r.route_component(id, Some(GUILD)),
                ComponentOutcome::Handled {
                    handler: ComponentHandler::Tickets
                },
                "{id} routes to tickets"
            );
        }
        assert_eq!(
            r.route_component("two:lfg:abc123", Some(GUILD)),
            ComponentOutcome::Handled {
                handler: ComponentHandler::LfgSignup
            }
        );
        assert_eq!(
            r.route_component("two:self-role:games:valorant", Some(GUILD)),
            ComponentOutcome::Handled {
                handler: ComponentHandler::SelfRole
            }
        );
        assert_eq!(
            r.route_component("two:self-role:games", Some(GUILD)),
            ComponentOutcome::Handled {
                handler: ComponentHandler::SelfRole
            }
        );
        // Unknown ids reply consistently without escaping the guild fence.
        for id in ["two:unknown:thing", "other"] {
            assert_eq!(
                r.route_component(id, Some(GUILD)),
                ComponentOutcome::Unknown
            );
            assert_eq!(r.route_component(id, Some(9999)), ComponentOutcome::Ignore);
            assert_eq!(r.route_component(id, None), ComponentOutcome::Ignore);
        }
        // Modal submits share the table.
        assert_eq!(
            r.route_modal("two:lfg:abc123", Some(GUILD)),
            ComponentOutcome::Handled {
                handler: ComponentHandler::LfgSignup
            }
        );
        assert_eq!(
            r.route_modal("two:unknown:thing", Some(GUILD)),
            ComponentOutcome::Unknown
        );
    }

    #[test]
    fn disabled_component_surfaces_ignore() {
        let off = InteractionRouter::new(RouterGates {
            tickets: false,
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
            announcements: false,
            ..all_on()
        });
        for id in [
            GAME_SELECT_ID,
            SESSION_SELECT_ID,
            TICKET_OPEN_ID,
            "two:lfg:x",
            "two:self-role:p",
        ] {
            assert_eq!(
                off.route_component(id, Some(GUILD)),
                ComponentOutcome::Ignore,
                "{id} ignored while disabled"
            );
        }
        // Guild fence on the fenced components.
        let r = router();
        assert_eq!(
            r.route_component(TICKET_OPEN_ID, Some(9999)),
            ComponentOutcome::Ignore
        );
        assert_eq!(
            r.route_component(SESSION_SELECT_ID, Some(9999)),
            ComponentOutcome::Ignore
        );
        assert_eq!(
            r.route_component("two:lfg:x", Some(9999)),
            ComponentOutcome::Ignore
        );
    }

    #[test]
    fn publish_set_is_the_complete_merged_view() {
        let r = router();
        let custom = vec![CustomCommand {
            name: "faq".to_owned(),
            description: "FAQ".to_owned(),
            enabled: true,
        }];
        let set = r.publish_set(&custom).expect("full set assembles");
        let names: Vec<_> = set.iter().map(|c| c.name.as_str()).collect();
        // 7 core + 1 scorecard + 12 automation + 12 announcements + 9 moderation
        // + 4 reads + 1 custom = 46, in publish order, guild-only throughout.
        assert_eq!(set.len(), 46);
        assert_eq!(
            &names[..8],
            [
                "rank",
                "leaderboard",
                "help",
                "privacy",
                "reopen",
                "welcome-preview",
                "game-mix",
                "attendance"
            ]
        );
        assert_eq!(names[11], "command-inspect");
        assert_eq!(&names[18..21], ["sticky-remove", "giveaway", "rsvp"]);
        assert!(names.contains(&"rsvp-attendance"));
        assert_eq!(names.iter().filter(|n| **n == "attendance").count(), 1);
        assert_eq!(names[24], "lfg-edit");
        assert_eq!(names[25], "lfg-list");
        assert_eq!(
            &names[26..32],
            [
                "feed-add",
                "feed-remove",
                "feed-pause",
                "feed-resume",
                "feed-list",
                "feed-inspect"
            ]
        );
        assert_eq!(
            &names[32..42],
            [
                "ban",
                "tempban",
                "tempban-list",
                "kick",
                "timeout",
                "warn",
                "purge",
                "slowmode",
                "lockdown",
                "unlock",
            ]
        );
        assert_eq!(
            &names[42..45],
            ["purge-preview", "channel-inspect", "moderation-recent"]
        );
        assert_eq!(names[45], "faq");
        assert!(set.iter().all(|c| !c.dm_permission));
    }

    #[test]
    fn metadata_edit_and_saved_response_lifecycle_survive_every_publish_gate_combination() {
        // Missing either additive feature, or deduping a distinct name, must
        // fail even when another feature happens to keep the total unchanged.
        let automation = [
            "command",
            "command-remove",
            "command-list",
            "command-inspect",
            "command-disable",
            "command-enable",
            "schedule",
            "schedule-remove",
            "schedule-list",
            "sticky",
            "sticky-remove",
            "giveaway",
        ];
        let announcements = [
            "rsvp",
            "rsvp-attendance",
            "lfg",
            "lfg-close",
            "lfg-edit",
            "lfg-list",
            "feed-add",
            "feed-remove",
            "feed-pause",
            "feed-resume",
            "feed-list",
            "feed-inspect",
        ];
        let moderation = [
            "ban",
            "tempban",
            "tempban-list",
            "kick",
            "timeout",
            "warn",
            "purge",
            "slowmode",
            "lockdown",
            "unlock",
            "purge-preview",
            "channel-inspect",
            "moderation-recent",
        ];
        for mask in 0_u8..64 {
            let gates = RouterGates {
                scorecard: mask & 1 != 0,
                automations: mask & 2 != 0,
                announcements: mask & 4 != 0,
                moderation: mask & 8 != 0,
                voice: mask & 16 != 0,
                voice_assistant: mask & 32 != 0,
                ..all_on()
            };
            for custom_enabled in [false, true] {
                let router = InteractionRouter::new(gates);
                let defs = router
                    .publish_set(&[CustomCommand {
                        name: "faq".to_owned(),
                        description: "FAQ".to_owned(),
                        enabled: custom_enabled,
                    }])
                    .expect("every feature combination assembles");
                let names: Vec<_> = defs.iter().map(|def| def.name.as_str()).collect();
                let mut expected = vec![
                    "rank",
                    "leaderboard",
                    "help",
                    "privacy",
                    "reopen",
                    "welcome-preview",
                    "game-mix",
                ];
                if gates.scorecard {
                    expected.push("attendance");
                }
                if gates.automations {
                    expected.extend(automation);
                }
                if gates.announcements {
                    expected.extend(announcements);
                }
                if gates.moderation {
                    expected.extend(moderation);
                }
                if gates.voice {
                    expected.extend(
                        VOICE_NAMES
                            .iter()
                            .copied()
                            .filter(|name| *name != "kick" || !gates.moderation),
                    );
                }
                if gates.automations && custom_enabled {
                    expected.push("faq");
                }
                assert_eq!(names, expected, "mask={mask} custom={custom_enabled}");
                assert_eq!(
                    names
                        .iter()
                        .copied()
                        .collect::<std::collections::HashSet<_>>()
                        .len(),
                    names.len(),
                    "distinct commands must never collide: mask={mask} custom={custom_enabled}",
                );
                assert!(defs.iter().all(|def| !def.dm_permission));
                assert!(!names.contains(&"templateassistant"));
                for (name, enabled, permission) in [
                    ("command-disable", gates.automations, "32"),
                    ("command-enable", gates.automations, "32"),
                    ("lfg-edit", gates.announcements, "8589934592"),
                ] {
                    let found = defs.iter().find(|def| def.name == name);
                    assert_eq!(found.is_some(), enabled, "{name} mask={mask}");
                    if let Some(def) = found {
                        assert_eq!(def.default_member_permissions.as_deref(), Some(permission));
                    }
                }
            }
        }
    }

    #[test]
    fn publish_set_respects_gates() {
        let off = InteractionRouter::new(RouterGates {
            scorecard: false,
            automations: false,
            announcements: false,
            moderation: false,
            voice: false,
            voice_assistant: false,
            tickets: false,
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
            ..all_on()
        });
        let set = off.publish_set(&[]).expect("core-only set");
        let names: Vec<_> = set.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "rank",
                "leaderboard",
                "help",
                "privacy",
                "reopen",
                "welcome-preview",
                "game-mix"
            ]
        );
    }

    #[test]
    fn publish_withholds_custom_rows_while_automations_off() {
        let off = InteractionRouter::new(RouterGates {
            scorecard: false,
            automations: false,
            announcements: false,
            moderation: false,
            ..all_on()
        });
        let row = || CustomCommand {
            name: "faq".to_owned(),
            description: "FAQ".to_owned(),
            enabled: true,
        };
        // A stored catalog is not published while gated off — every such
        // invocation refuses at dispatch, so publishing burns the ceiling.
        let set = off.publish_set(&[row()]).expect("core-only set");
        let names: Vec<_> = set.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "rank",
                "leaderboard",
                "help",
                "privacy",
                "reopen",
                "welcome-preview",
                "game-mix"
            ]
        );
        // Over-limit stored catalogs no longer fail the publish either.
        let crowded: Vec<_> = (0..99)
            .map(|i| CustomCommand {
                name: format!("c{i}"),
                description: "crowd".to_owned(),
                enabled: true,
            })
            .collect();
        let set = off.publish_set(&crowded).expect("core-only set");
        assert_eq!(set.len(), 7);
    }

    #[test]
    fn publish_reserves_disabled_builtin_names() {
        // `ban` routes (as a refusal) even while moderation is off, so a
        // same-named custom row could never execute — publish withholds it,
        // matching dispatch precedence.
        for (gate, builtin) in [
            ("moderation", "ban"),
            ("announcements", "lfg"),
            ("announcements", "lfg-edit"),
            ("scorecard", "attendance"),
        ] {
            let mut gates = all_on();
            match gate {
                "moderation" => gates.moderation = false,
                "announcements" => gates.announcements = false,
                _ => gates.scorecard = false,
            }
            let r = InteractionRouter::new(gates);
            let row = CustomCommand {
                name: builtin.to_owned(),
                description: "shadow".to_owned(),
                enabled: true,
            };
            let set = r.publish_set(&[row]).expect("assembles");
            assert!(
                !set.iter().any(|c| c.name == builtin),
                "{builtin} stays unpublished while {gate} is off"
            );
        }
    }

    #[test]
    fn gates_bridge_from_existing_modules() {
        let features = FeatureGates::from_map(&HashMap::from([(
            "TWO_ANNOUNCEMENTS".to_owned(),
            "1".to_owned(),
        )]))
        .expect("parses");
        let moderation = ModerationGates::from_map(&HashMap::new()).expect("defaults");
        let gates = RouterGates::from_slices(
            Some(GUILD),
            &features,
            &moderation,
            SurfaceFlags {
                scorecard: true,
                onboarding_picker: true,
                ..SurfaceFlags::default()
            },
        );
        assert!(!gates.automations && gates.announcements && !gates.moderation);
        assert!(gates.scorecard && !gates.tickets && gates.onboarding_picker);
        assert!(!gates.voice && !gates.voice_assistant);
    }

    #[test]
    fn gates_bridge_carries_the_voice_flags() {
        let features = FeatureGates::from_map(&HashMap::new()).expect("defaults");
        let moderation = ModerationGates::from_map(&HashMap::new()).expect("defaults");
        let gates = RouterGates::from_slices(
            Some(GUILD),
            &features,
            &moderation,
            SurfaceFlags {
                voice: true,
                voice_assistant: true,
                ..SurfaceFlags::default()
            },
        );
        assert!(gates.voice && gates.voice_assistant);
    }

    /// The 22 voice names in published order (pinned independently in
    /// `voice_rooms::tests::voice_command_shapes_and_gates`).
    const VOICE_NAMES: [&str; 22] = [
        "create",
        "setup",
        "ping",
        "invite",
        "textchannels",
        "access",
        "reclaim",
        "transfer",
        "logging",
        "export",
        "import",
        "position",
        "group",
        "inheritpermissions",
        "defaultlimit",
        "alwaysprivate",
        "kick",
        "name",
        "private",
        "public",
        "limit",
        "unlimit",
    ];

    fn voice_on() -> RouterGates {
        RouterGates {
            voice: true,
            ..all_on()
        }
    }

    fn published(gates: RouterGates) -> Vec<CommandDefinition> {
        InteractionRouter::new(gates)
            .publish_set(&[])
            .expect("set assembles")
    }

    #[test]
    fn voice_set_publishes_after_moderation_only_while_gated() {
        let off = published(all_on());
        let off_names: Vec<_> = off.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(off.len(), 45);
        for name in VOICE_NAMES.iter().filter(|name| **name != "kick") {
            assert!(
                !off_names.contains(name),
                "/{name} leaked with the gate off"
            );
        }

        let on = published(voice_on());
        let on_names: Vec<_> = on.iter().map(|c| c.name.as_str()).collect();
        // 45 builtins keep their order; the 21 voice names that survive
        // first-wins follow in voice order. `kick` is already moderation's.
        assert_eq!(on.len(), 66);
        assert_eq!(on_names[..45], off_names[..]);
        let expected: Vec<_> = VOICE_NAMES
            .iter()
            .copied()
            .filter(|name| *name != "kick")
            .collect();
        assert_eq!(on_names[45..], expected[..]);
        assert!(!on_names.contains(&"templateassistant"));
        assert!(on.iter().all(|c| !c.dm_permission));
    }

    #[test]
    fn voice_admission_yield_respects_the_gate_and_builtin_precedence() {
        let off = InteractionRouter::new(all_on());
        assert!(!off.voice_owns_command("ping"));

        let on = InteractionRouter::new(voice_on());
        assert!(on.voice_owns_command("ping"));
        assert!(!on.voice_owns_command("kick"));
        assert!(!on.voice_owns_command("sticky"));
    }

    #[test]
    fn voice_kick_loses_first_wins_to_moderation_kick() {
        use crate::commands::PERM_KICK_MEMBERS;
        let on = published(voice_on());
        let kicks: Vec<_> = on.iter().filter(|c| c.name == "kick").collect();
        assert_eq!(kicks.len(), 1, "one /kick in the registry");
        assert_eq!(
            kicks[0].default_member_permissions,
            Some(PERM_KICK_MEMBERS.to_string()),
            "the surviving /kick is the moderation shape"
        );
        assert!(kicks[0].options.iter().any(|o| o.name == "target"));
        assert!(!kicks[0].options.iter().any(|o| o.name == "member"));

        // Moderation off merges nothing ahead of it, so the voice vote-kick
        // shape publishes (runtime dispatch still routes a tracked-room target
        // to the voice sink before the moderation refusal).
        let no_moderation = published(RouterGates {
            moderation: false,
            ..voice_on()
        });
        let kick = no_moderation
            .iter()
            .find(|c| c.name == "kick")
            .expect("voice kick publishes without moderation");
        assert_eq!(kick.default_member_permissions, None);
        assert!(kick.options.iter().any(|o| o.name == "member"));
    }

    #[test]
    fn templateassistant_stays_unpublished_until_its_slice_lands() {
        // No handler yet, so no publish in any gate state; the slice
        // re-activates by publishing again (dispatch keeps shadowing the
        // squatted name, and the reservation keeps custom rows out).
        for (voice, assistant) in [(false, false), (false, true), (true, false), (true, true)] {
            let set = published(RouterGates {
                voice,
                voice_assistant: assistant,
                ..all_on()
            });
            assert!(
                !set.iter().any(|c| c.name == "templateassistant"),
                "voice={voice} assistant={assistant}"
            );
        }
        // Both gates on is the full voice set without the assistant: 45
        // builtins plus the 21 surviving voice names.
        let both = published(RouterGates {
            voice_assistant: true,
            ..voice_on()
        });
        assert_eq!(both.len(), 66);
    }

    #[test]
    fn voice_names_yield_to_the_voice_sink_only_while_gated() {
        let on = InteractionRouter::new(voice_on());
        let off = router();
        for name in VOICE_NAMES.iter().filter(|name| **name != "kick") {
            // The sink owns the whole interaction, so the router stays silent
            // in the configured guild, other guilds and DMs alike.
            for (guild, perms) in [(Some(GUILD), Some(u64::MAX)), (Some(1), None), (None, None)] {
                assert_eq!(
                    on.route_slash(&ctx(name, guild, perms)),
                    SlashOutcome::Ignore,
                    "/{name} must yield while TWO_VOICE is on"
                );
            }
            // Gate off: nothing is published, so the stale-interaction reply
            // is the unchanged behaviour.
            assert_eq!(
                off.route_slash(&ctx(name, Some(GUILD), Some(u64::MAX))),
                SlashOutcome::Unknown,
                "/{name} is unchanged with the gate off"
            );
        }
        // `/kick` stays moderation's; the claim check lives in the runtime.
        assert_eq!(
            on.route_slash(&ctx("kick", Some(GUILD), Some(u64::MAX))),
            SlashOutcome::Handled {
                handler: HandlerId::Moderation(ModerationAction::Kick)
            }
        );
        // `/templateassistant` has no handler yet and stays unpublished:
        // the unknown-command reply in every gate state, never silence
        // and never a stored custom row.
        for (voice, voice_assistant) in [(false, false), (false, true), (true, false), (true, true)]
        {
            let router = InteractionRouter::new(RouterGates {
                voice,
                voice_assistant,
                ..all_on()
            });
            assert_eq!(
                router.route_slash(&ctx("templateassistant", Some(GUILD), Some(u64::MAX))),
                SlashOutcome::Unknown,
                "voice={voice} voice_assistant={voice_assistant}"
            );
        }
    }

    #[test]
    fn voice_names_shadow_custom_rows_only_while_gated() {
        let row = CustomCommand {
            name: "ping".to_owned(),
            description: "shadow".to_owned(),
            enabled: true,
        };
        let custom_ctx = || SlashContext {
            custom_row: Some(true),
            ..ctx("ping", Some(GUILD), Some(0))
        };

        // Gate on: the voice `/ping` publishes, the custom row is withheld and
        // dispatch yields to the sink instead of running the custom handler.
        let on = InteractionRouter::new(voice_on());
        let set = on
            .publish_set(std::slice::from_ref(&row))
            .expect("assembles");
        let pings: Vec<_> = set.iter().filter(|c| c.name == "ping").collect();
        assert_eq!(pings.len(), 1);
        assert_ne!(pings[0].description, "shadow");
        assert_eq!(on.route_slash(&custom_ctx()), SlashOutcome::Ignore);

        // Gate off: the name is free, so the custom row publishes and runs.
        let off = router();
        let set = off
            .publish_set(std::slice::from_ref(&row))
            .expect("assembles");
        let ping = set.iter().find(|c| c.name == "ping").expect("custom ping");
        assert_eq!(ping.description, "shadow");
        assert_eq!(
            off.route_slash(&custom_ctx()),
            SlashOutcome::Handled {
                handler: HandlerId::AutomationCustom
            }
        );
    }

    #[test]
    fn unpublished_assistant_never_publishes_nor_runs_a_stored_custom_row() {
        for (voice, voice_assistant) in [(false, false), (false, true), (true, false), (true, true)]
        {
            let router = InteractionRouter::new(RouterGates {
                voice,
                voice_assistant,
                ..all_on()
            });
            let row = CustomCommand {
                name: "templateassistant".to_owned(),
                description: "Stored custom row".to_owned(),
                enabled: true,
            };
            // The squatted name never publishes, so the stored row stays out
            // in every gate state.
            let definitions = router.publish_set(&[row]).unwrap();
            assert!(
                !definitions
                    .iter()
                    .any(|def| def.name == "templateassistant"),
                "voice={voice} voice_assistant={voice_assistant}"
            );
            // ... and dispatch never runs it either.
            let context = SlashContext {
                custom_row: Some(true),
                ..ctx("templateassistant", Some(GUILD), Some(0))
            };
            assert_eq!(
                router.route_slash(&context),
                SlashOutcome::Unknown,
                "voice={voice} voice_assistant={voice_assistant}"
            );
        }
    }

    /// `command_permission_denied` must log Discord IDs as decimal strings
    /// (`docs/logging.md`), never `Some(…)` or JSON numbers.
    #[test]
    fn permission_denied_logs_decimal_string_ids() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);

        struct Guard<'a>(std::sync::MutexGuard<'a, Vec<u8>>);

        impl std::io::Write for Guard<'_> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> MakeWriter<'a> for SharedBuf {
            type Writer = Guard<'a>;

            fn make_writer(&'a self) -> Self::Writer {
                Guard(self.0.lock().unwrap_or_else(|e| e.into_inner()))
            }
        }

        let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(buf.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            assert!(!InteractionRouter::permission_allowed(
                "purge",
                Some(GUILD),
                Some(0)
            ));
        });
        let text = String::from_utf8(buf.0.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .expect("log output is utf-8");
        let denied = text
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|event| {
                event
                    .get("fields")
                    .and_then(|fields| fields.get("message"))
                    .and_then(serde_json::Value::as_str)
                    == Some("command_permission_denied")
            })
            .expect("permission denial is logged");
        let fields = denied.get("fields").expect("json fields envelope");
        assert_eq!(
            fields.get("guild_id").and_then(serde_json::Value::as_str),
            Some("2222"),
            "guild id must be a decimal string, not Some(…)"
        );
        assert_eq!(
            fields
                .get("actor_permissions")
                .and_then(serde_json::Value::as_str),
            Some("0"),
            "actor permissions must be a decimal string"
        );
    }
}
