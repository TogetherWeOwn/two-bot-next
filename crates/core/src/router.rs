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
//! Unknown slash names (no builtin, no custom row) and unknown `custom_id`s
//! are `Ignore`: some other application's command, not ours to answer —
//! exactly the legacy fall-through. Legacy has no modal submits; modals route
//! through the same component-id table so future slices have a place to land.
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

use std::collections::HashMap;

use super::commands::{
    merge_commands, CommandDefinition, CustomCommand, RegistryError, PERM_MANAGE_EVENTS,
    PERM_MANAGE_GUILD,
};
use super::feature_commands::{
    announcement_commands, automation_commands, scorecard_attendance_command, FeatureGates,
};
use super::moderation::{ModerationAction, ModerationGates};
use super::onboarding::{GAME_SELECT_ID, SESSION_SELECT_ID};

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

/// Legacy-exact (`src/automations/discord.ts` `AUTOMATIONS_DISABLED_REPLY`).
pub const AUTOMATIONS_DISABLED_REPLY: &str = "Automations are disabled on this server.";
/// Legacy-exact (automation + feed handlers).
pub const MANAGE_SERVER_REQUIRED: &str = "Manage Server permission is required.";
/// Legacy-exact (LFG handlers).
pub const MANAGE_EVENTS_REQUIRED: &str = "Manage Events permission is required.";
/// Legacy-exact (`src/moderation/commands.ts` guild fence).
pub const GUILD_RESTRICTED_REPLY: &str = "This command is restricted to the configured guild.";
/// Port shape for the unified router (legacy never registered the handler, so
/// it stayed silent; the router refuses explicitly instead).
pub const ANNOUNCEMENTS_DISABLED_REPLY: &str = "Announcements are disabled on this server.";
/// Port shape, same rationale as above.
pub const MODERATION_DISABLED_REPLY: &str = "Moderation is not enabled on this server.";
/// Port shape, same rationale as above.
pub const SCORECARD_DISABLED_REPLY: &str = "Attendance capture is not enabled on this server.";

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
    ScorecardAttendance,
    AutomationAdmin,
    AutomationCustom,
    Rsvp,
    RsvpAttendance,
    Lfg,
    LfgClose,
    FeedAdd,
    FeedRemove,
    FeedList,
    Moderation(ModerationAction),
}

/// Which feature slice owns a routed component / modal submit (parity §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentHandler {
    GamePicker,
    SessionPicker,
    SelfRole,
    Tickets,
    LfgSignup,
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
}

impl RouterRefusal {
    /// Legacy reply text for this refusal.
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
            // Legacy `src/moderation/policy.ts`: `Missing required permission
            // for ${request.action}` where the action is `moderation.ban`, ….
            Self::ModerationPermission(action) => {
                format!("Missing required permission for {}", action.action_name())
            }
        }
    }
}

/// Slash-command routing outcome: plain data, no Discord calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashOutcome {
    Handled { handler: HandlerId },
    Refuse { refusal: RouterRefusal },
    Ignore,
}

/// Component / modal-submit routing outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentOutcome {
    Handled { handler: ComponentHandler },
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

    fn guild_ok(&self, guild_id: Option<u64>) -> bool {
        guild_id.is_some_and(|g| Some(g) == self.gates.configured_guild)
    }

    fn has_perm(actor_permissions: Option<u64>, required: u64) -> bool {
        actor_permissions.is_some_and(|bits| bits & required == required)
    }

    /// Route one slash command through fence → gate → permission checks.
    #[must_use]
    pub fn route_slash(&self, ctx: &SlashContext<'_>) -> SlashOutcome {
        // Built-in table first (reserved names shadow custom rows, same as
        // the publish merge).
        if let Some(outcome) = self.route_builtin(ctx.name, ctx.guild_id, ctx.actor_permissions) {
            return outcome;
        }
        // Dynamic DB-backed custom commands (#22): everyone while automations
        // are on; explicit refusal while off; silence otherwise (legacy
        // `registerAutomationCommands`: unknown names are another app's).
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
        if let Some(action) = ModerationAction::ALL
            .iter()
            .find(|a| a.command_name() == name)
        {
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
            if !Self::has_perm(actor_permissions, action.required_permission()) {
                return Some(SlashOutcome::Refuse {
                    refusal: RouterRefusal::ModerationPermission(*action),
                });
            }
            return Some(SlashOutcome::Handled {
                handler: HandlerId::Moderation(*action),
            });
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
            perm: Option<(u64, RouterRefusal)>,
        }

        let row = match name {
            "rank" => Row {
                handler: HandlerId::Rank,
                gate: RowGate::Always,
                perm: None,
            },
            "leaderboard" => Row {
                handler: HandlerId::Leaderboard,
                gate: RowGate::Always,
                perm: None,
            },
            "attendance" => Row {
                handler: HandlerId::ScorecardAttendance,
                gate: RowGate::Scorecard,
                perm: None,
            },
            "command" | "command-remove" | "command-list" | "schedule" | "schedule-remove"
            | "schedule-list" | "sticky" | "sticky-remove" => Row {
                handler: HandlerId::AutomationAdmin,
                gate: RowGate::Automations,
                perm: Some((PERM_MANAGE_GUILD, RouterRefusal::ManageServerRequired)),
            },
            "rsvp" => Row {
                handler: HandlerId::Rsvp,
                gate: RowGate::Announcements,
                perm: None,
            },
            // Namespaced: legacy RSVP-totals `attendance` collides with the
            // scorecard `attendance` on `guild.commands.set` — see
            // `commands.rs`. The two names route to different handlers.
            "rsvp-attendance" => Row {
                handler: HandlerId::RsvpAttendance,
                gate: RowGate::Announcements,
                perm: None,
            },
            "lfg" => Row {
                handler: HandlerId::Lfg,
                gate: RowGate::Announcements,
                perm: Some((PERM_MANAGE_EVENTS, RouterRefusal::ManageEventsRequired)),
            },
            "lfg-close" => Row {
                handler: HandlerId::LfgClose,
                gate: RowGate::Announcements,
                perm: Some((PERM_MANAGE_EVENTS, RouterRefusal::ManageEventsRequired)),
            },
            "feed-add" => Row {
                handler: HandlerId::FeedAdd,
                gate: RowGate::Announcements,
                perm: Some((PERM_MANAGE_GUILD, RouterRefusal::ManageServerRequired)),
            },
            "feed-remove" => Row {
                handler: HandlerId::FeedRemove,
                gate: RowGate::Announcements,
                perm: Some((PERM_MANAGE_GUILD, RouterRefusal::ManageServerRequired)),
            },
            "feed-list" => Row {
                handler: HandlerId::FeedList,
                gate: RowGate::Announcements,
                perm: Some((PERM_MANAGE_GUILD, RouterRefusal::ManageServerRequired)),
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
        if let Some((bits, refusal)) = row.perm {
            if !Self::has_perm(actor_permissions, bits) {
                return Some(SlashOutcome::Refuse { refusal });
            }
        }
        Some(SlashOutcome::Handled {
            handler: row.handler,
        })
    }

    /// Route one message component by `custom_id` (exact ids, then prefixes).
    #[must_use]
    pub fn route_component(&self, custom_id: &str, guild_id: Option<u64>) -> ComponentOutcome {
        let handler = if custom_id == GAME_SELECT_ID {
            if !self.gates.onboarding_picker {
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
            if !self.gates.self_roles {
                return ComponentOutcome::Ignore;
            }
            ComponentHandler::SelfRole
        } else {
            return ComponentOutcome::Ignore;
        };
        ComponentOutcome::Handled { handler }
    }

    /// Route one modal submit by `custom_id` through the same table as
    /// components. Legacy has no modals; unknown ids are `Ignore`.
    #[must_use]
    pub fn route_modal(&self, custom_id: &str, guild_id: Option<u64>) -> ComponentOutcome {
        self.route_component(custom_id, guild_id)
    }

    /// Assemble the ONE complete guild command set for publish-on-ready
    /// (legacy `CommandRegistry::sync` order: community, automation,
    /// announcement, moderation — then DB custom commands). First-wins dedupe
    /// and the 100-command ceiling come from `merge_commands`.
    pub fn publish_set(
        &self,
        custom: &[CustomCommand],
    ) -> Result<Vec<CommandDefinition>, RegistryError> {
        let mut extra: Vec<Vec<CommandDefinition>> = Vec::with_capacity(4);
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
        merge_commands(&extra, custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::PERM_MANAGE_EVENTS;
    use std::collections::HashMap;

    const GUILD: u64 = 2222;

    fn all_on() -> RouterGates {
        RouterGates {
            configured_guild: Some(GUILD),
            scorecard: true,
            automations: true,
            announcements: true,
            moderation: true,
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
            ("ban", HandlerId::Moderation(ModerationAction::Ban)),
            ("tempban", HandlerId::Moderation(ModerationAction::TempBan)),
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
            ("attendance", HandlerId::ScorecardAttendance),
            ("command", HandlerId::AutomationAdmin),
            ("command-remove", HandlerId::AutomationAdmin),
            ("command-list", HandlerId::AutomationAdmin),
            ("schedule", HandlerId::AutomationAdmin),
            ("schedule-remove", HandlerId::AutomationAdmin),
            ("schedule-list", HandlerId::AutomationAdmin),
            ("sticky", HandlerId::AutomationAdmin),
            ("sticky-remove", HandlerId::AutomationAdmin),
            ("rsvp", HandlerId::Rsvp),
            ("rsvp-attendance", HandlerId::RsvpAttendance),
            ("lfg", HandlerId::Lfg),
            ("lfg-close", HandlerId::LfgClose),
            ("feed-add", HandlerId::FeedAdd),
            ("feed-remove", HandlerId::FeedRemove),
            ("feed-list", HandlerId::FeedList),
        ];
        assert_eq!(cases.len(), 27, "all 27 builtins covered");
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
            ("feed-list", RouterRefusal::AnnouncementsDisabled),
            ("ban", RouterRefusal::ModerationDisabled),
            ("purge", RouterRefusal::ModerationDisabled),
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
    fn refusal_texts_match_legacy() {
        assert_eq!(
            RouterRefusal::AutomationsDisabled.message(),
            "Automations are disabled on this server."
        );
        assert_eq!(
            RouterRefusal::ManageServerRequired.message(),
            "Manage Server permission is required."
        );
        assert_eq!(
            RouterRefusal::ManageEventsRequired.message(),
            "Manage Events permission is required."
        );
        assert_eq!(
            RouterRefusal::GuildRestricted.message(),
            "This command is restricted to the configured guild."
        );
        assert_eq!(
            RouterRefusal::ModerationPermission(ModerationAction::Ban).message(),
            "Missing required permission for moderation.ban"
        );
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
        // Disabled row: silence (legacy `if (!custom.enabled) return`).
        assert_eq!(r.route_slash(&custom(Some(false))), SlashOutcome::Ignore);
        // No row: another app's command, not ours.
        assert_eq!(r.route_slash(&custom(None)), SlashOutcome::Ignore);
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
    fn unknown_slash_names_are_ignored() {
        let r = router();
        assert_eq!(
            r.route_slash(&ctx("rota-acknowledge", Some(GUILD), Some(u64::MAX))),
            SlashOutcome::Ignore,
            "dropped rota command is not ours"
        );
        assert_eq!(
            r.route_slash(&ctx(
                "definitely-not-a-command",
                Some(GUILD),
                Some(u64::MAX)
            )),
            SlashOutcome::Ignore
        );
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
        // Unknown ids are not ours.
        assert_eq!(
            r.route_component("two:unknown:thing", Some(GUILD)),
            ComponentOutcome::Ignore
        );
        assert_eq!(
            r.route_component("other", Some(GUILD)),
            ComponentOutcome::Ignore
        );
        // Modal submits share the table.
        assert_eq!(
            r.route_modal("two:lfg:abc123", Some(GUILD)),
            ComponentOutcome::Handled {
                handler: ComponentHandler::LfgSignup
            }
        );
        assert_eq!(
            r.route_modal("two:unknown:thing", Some(GUILD)),
            ComponentOutcome::Ignore
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
        // 2 core + 1 scorecard + 8 automation + 7 announcement + 9 moderation
        // + 1 custom = 28, in legacy publish order, guild-only throughout.
        assert_eq!(set.len(), 28);
        assert_eq!(&names[..3], ["rank", "leaderboard", "attendance"]);
        assert!(names.contains(&"rsvp-attendance"));
        assert_eq!(names.iter().filter(|n| **n == "attendance").count(), 1);
        assert_eq!(
            &names[18..27],
            [
                "ban", "tempban", "kick", "timeout", "warn", "purge", "slowmode", "lockdown",
                "unlock",
            ]
        );
        assert_eq!(names[27], "faq");
        assert!(set.iter().all(|c| !c.dm_permission));
    }

    #[test]
    fn publish_set_respects_gates() {
        let off = InteractionRouter::new(RouterGates {
            scorecard: false,
            automations: false,
            announcements: false,
            moderation: false,
            ..all_on()
        });
        let set = off.publish_set(&[]).expect("core-only set");
        let names: Vec<_> = set.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["rank", "leaderboard"]);
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
    }
}
