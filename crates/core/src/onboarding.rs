//! Framework-free onboarding decisions (TOG-10086): game/session pickers,
//! legacy/session/anchor welcomes, gate-clear eligibility and session goodbye.
//!
//! Parity source: TogetherWeOwn/two-bot @ d5d11793, `src/onboarding/{flow,
//! catalog,session,mode,anchorEvent}.ts` and `src/discord/{onboarding,
//! sessionWelcome,anchorWelcome}.ts`; see `docs/parity.md` §§2, 3, 8.
//! Post-freeze selection/visibility fixes through bffccf3: b948b89, b3747a8,
//! a40d4a5 and d3d9afe.
//! Card-directed difference: anchor is selected by `TWO_ONBOARDING_MODE`, not
//! by the presence of `DISCORD_ANCHOR_WELCOME_CHANNEL_ID`.
//!
//! Outcomes describe guild-channel posts and ephemeral replies, never DMs.
//! Session picks have no role fields. Game routing uses the hub fallback only
//! when visible; the executor must re-resolve visibility after granting roles.
//! Goodbyes require empty allowed-mentions even for mention-like usernames.
//! The store's prompt guard serializes successful join/gate-clear sends.
//!
//! Runtime wiring belongs to the shared S4 router/REST executor. This slice
//! supplies outcomes, not a private dispatcher or HTTP client.

use std::collections::{HashMap, HashSet};

/// Game picker select-menu custom id. Static: the panel outlives every restart
/// (legacy `GAME_SELECT_ID`).
pub const GAME_SELECT_ID: &str = "two:onboarding:games";
/// Session picker select-menu custom id (legacy `SESSION_SELECT_ID`).
pub const SESSION_SELECT_ID: &str = "two:onboarding:session";

/// TWO production guild (legacy `GUILD_ID`). Deployment data, kept for parity;
/// the adapter uses configured guild ids at runtime.
pub const TWO_GUILD_ID: &str = "326474832151838730";
/// Game-hub forum channel: every verified member can see it, and the fallback
/// destination for every pick (legacy `GAME_HUB_CHANNEL_ID`).
pub const GAME_HUB_CHANNEL_ID: &str = "1092312335529541632";
/// Introduce-yourself channel linked from the legacy welcome
/// (legacy `INTRO_CHANNEL_ID`).
pub const INTRO_CHANNEL_ID: &str = "1087198966346690570";

/// Sunday Squad anchor spec (legacy `SUNDAY_SQUAD`, TWO-66 §5.4): Fall Guys, an
/// hour, every Sunday 20:00 America/New_York in the Lobby voice room.
pub const ANCHOR_CHANNEL_ID: &str = "1175127344072118405";
/// Staging fixture destinations (legacy `SESSION_PICKS` ids). Runtime handlers
/// receive per-guild ids from config; these pin the staging proof only.
pub const STAGING_LOOKING_TO_PLAY_CHANNEL_ID: &str = "1546211377847337020";
pub const STAGING_LOBBY_VOICE_CHANNEL_ID: &str = "1546211378430345286";

// --- modes ------------------------------------------------------------------

/// Onboarding flow selector (`TWO_ONBOARDING_MODE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OnboardingMode {
    /// Game-role picker + hub routing (legacy default).
    Legacy,
    /// Roleless two-pick routing + goodbye (live since TOG-2795).
    Session,
    /// One-message Sunday Squad welcome; the picker stays live but silent.
    Anchor,
}

impl OnboardingMode {
    /// Valid switch values, for error messages.
    pub const VALID: [&'static str; 3] = ["legacy", "session", "anchor"];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Session => "session",
            Self::Anchor => "anchor",
        }
    }
}

/// Leveling reward roles are a role write, so session mode suppresses them
/// (legacy `levelRoleWritesForOnboardingMode`). XP, level-ups and `/rank` are
/// unaffected — only `member.roles.add` is gated by the caller.
#[must_use]
pub fn level_role_writes_allowed(mode: OnboardingMode) -> bool {
    mode != OnboardingMode::Session
}

/// Game-picker role writes are allowed in every mode except session, which
/// guarantees zero role writes structurally (legacy `src/index.ts` registers
/// no role-writing handler in session mode; `role.assign` gating for internal
/// actions belongs to TOG-9880).
#[must_use]
pub fn game_picker_allowed(mode: OnboardingMode) -> bool {
    mode != OnboardingMode::Session
}

/// Onboarding env gates (legacy `loadConfig` onboarding fields +
/// `parseOnboardingMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnboardingGates {
    /// `TWO_ONBOARDING_MODE` (unset = legacy).
    pub mode: OnboardingMode,
    /// `TWO_ONBOARDING_DRY_RUN=1`: no role writes, no legacy/anchor welcomes
    /// and no session goodbyes. Roleless session welcomes/picker replies still
    /// run and record, matching legacy `sessionWelcome.ts`.
    pub dry_run: bool,
}

/// Invalid onboarding-gate environment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OnboardingGateError {
    #[error("TWO_ONBOARDING_MODE must be exactly \"legacy\", \"session\" or \"anchor\" when set, got {0:?}")]
    InvalidMode(String),
}

impl OnboardingGates {
    /// Read gates from the process environment.
    pub fn from_env() -> Result<Self, OnboardingGateError> {
        Self::from_map(&std::env::vars().collect())
    }

    /// Read gates from an explicit map (tests, staged config).
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, OnboardingGateError> {
        let mode = match vars.get("TWO_ONBOARDING_MODE").map(String::as_str) {
            None | Some("") | Some("legacy") => OnboardingMode::Legacy,
            Some("session") => OnboardingMode::Session,
            Some("anchor") => OnboardingMode::Anchor,
            Some(raw) => return Err(OnboardingGateError::InvalidMode(raw.to_owned())),
        };
        Ok(Self {
            mode,
            dry_run: vars.get("TWO_ONBOARDING_DRY_RUN").is_some_and(|v| v == "1"),
        })
    }
}

// --- prompt decision ----------------------------------------------------------

/// Why a member is (not) due a welcome (legacy `PromptDecision`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDecision {
    Prompt,
    Skip(PromptSkip),
}

/// Skip reasons, in legacy check order: bots, then the rules gate, then the
/// once-per-member guard (legacy `decidePrompt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSkip {
    Bot,
    StillPending,
    AlreadyPrompted,
}

/// Pure prompt gate: a member becomes promptable the moment they can see and
/// click things. `already_prompted` is the store's `onboarding_prompted`
/// answer; the store enforces once-per-member again at write time.
#[must_use]
pub fn decide_prompt(is_bot: bool, pending: bool, already_prompted: bool) -> PromptDecision {
    if is_bot {
        return PromptDecision::Skip(PromptSkip::Bot);
    }
    if pending {
        return PromptDecision::Skip(PromptSkip::StillPending);
    }
    if already_prompted {
        return PromptDecision::Skip(PromptSkip::AlreadyPrompted);
    }
    PromptDecision::Prompt
}

/// Gateway trigger for the welcome path. Members who accept the rules on the
/// invite screen arrive ungated (`Joined { pending: false }`); everyone else
/// prompts when `pending` flips true → false (`GateCleared`). A gated join
/// prompts nobody — `GuildMemberUpdate` picks them up later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MembershipTrigger {
    Joined { pending: bool },
    GateCleared,
}

/// Whether this gateway event may prompt (the router calls [`decide_prompt`]
/// next, so a stale `pending` still cannot slip through).
#[must_use]
pub fn welcome_trigger(trigger: MembershipTrigger) -> bool {
    match trigger {
        MembershipTrigger::Joined { pending } => !pending,
        MembershipTrigger::GateCleared => true,
    }
}

// --- game catalog ---------------------------------------------------------------

/// One game-menu entry (legacy `GamePick`). Ids are the live TWO ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GamePick {
    /// Stable key: select-menu value + event metadata.
    pub key: &'static str,
    pub label: &'static str,
    /// Select description (Discord truncates at 100 chars).
    pub description: &'static str,
    pub emoji: &'static str,
    /// Role granted on pick. Always below the bot's highest role.
    pub role_id: &'static str,
    pub role_name: &'static str,
    /// Intended room; `None` = interest has a role but no room, hub is honest.
    pub primary_channel_id: Option<&'static str>,
    /// Used when the primary is not visible to the member.
    pub fallback_channel_id: &'static str,
}

/// The ten game options in the picker menu (legacy `GAME_PICKS`).
pub const GAME_PICKS: [GamePick; 10] = [
    GamePick {
        key: "shooters",
        label: "Shooters",
        description: "CS, Siege, CoD, Battlefield, Valorant",
        emoji: "🎯",
        role_id: "1051272877871222915",
        role_name: "Shooter Games",
        primary_channel_id: Some("1179217198930202735"),
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "survival",
        label: "Survival",
        description: "Rust, DayZ, Ark, Valheim, Palworld",
        emoji: "🎮",
        role_id: "1179233034713702511",
        role_name: "Survival Games",
        primary_channel_id: Some("1178937094035492884"),
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "horror",
        label: "Horror",
        description: "Phasmophobia, Lethal Company, Dead by Daylight",
        emoji: "👻",
        role_id: "1119666971584237679",
        role_name: "Horror Games",
        primary_channel_id: Some("1118994447036850369"),
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "counterstrike",
        label: "Counter-Strike",
        description: "CS2 specifically",
        emoji: "💣",
        role_id: "1179233301295284385",
        role_name: "CounterStrike",
        primary_channel_id: None,
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "rocketleague",
        label: "Rocket League",
        description: "Car football",
        emoji: "🚀",
        role_id: "1065438504521322526",
        role_name: "rocketleague",
        primary_channel_id: None,
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "fallguys",
        label: "Fall Guys",
        description: "Beans",
        emoji: "🫘",
        role_id: "1065438396069191700",
        role_name: "fallguys",
        primary_channel_id: None,
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "warthunder",
        label: "War Thunder",
        description: "Tanks and planes",
        emoji: "🛩️",
        role_id: "1065438198316138507",
        role_name: "warthunder",
        primary_channel_id: None,
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "retro",
        label: "Retro",
        description: "Anything pre-2005",
        emoji: "🕹️",
        role_id: "1063255307410739241",
        role_name: "Retro Games",
        primary_channel_id: None,
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "tabletop",
        label: "Tabletop",
        description: "D&D, board games, TCGs",
        emoji: "🎲",
        role_id: "1063255343884406864",
        role_name: "Tabletop Games",
        primary_channel_id: None,
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "pokemon",
        label: "Pokemon",
        description: "Main series, TCG, GO",
        emoji: "⚡",
        role_id: "1063245872328081439",
        role_name: "Pokemon",
        primary_channel_id: None,
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
];

/// Platform question (legacy `PLATFORM_PICKS`). Not in the menu, but valid in
/// a submission lookup — same as legacy `pickByKey` over `ALL_PICKS`.
pub const PLATFORM_PICKS: [GamePick; 5] = [
    GamePick {
        key: "pc",
        label: "PC",
        description: "",
        emoji: "🖥️",
        role_id: "1092247753574330458",
        role_name: "PC",
        primary_channel_id: None,
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "xbox",
        label: "Xbox",
        description: "",
        emoji: "🟩",
        role_id: "1087930995875008522",
        role_name: "Xbox",
        primary_channel_id: None,
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "playstation",
        label: "PlayStation",
        description: "",
        emoji: "🔵",
        role_id: "1087931108945039470",
        role_name: "Playstation",
        primary_channel_id: None,
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "switch",
        label: "Switch",
        description: "",
        emoji: "🔴",
        role_id: "1092248449786855595",
        role_name: "Switch",
        primary_channel_id: None,
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
    GamePick {
        key: "mobile",
        label: "Mobile",
        description: "",
        emoji: "📱",
        role_id: "1092250849717264475",
        role_name: "Mobile",
        primary_channel_id: None,
        fallback_channel_id: GAME_HUB_CHANNEL_ID,
    },
];

/// Look a submission key up across games + platforms (legacy `pickByKey`).
#[must_use]
pub fn pick_by_key(key: &str) -> Option<&'static GamePick> {
    GAME_PICKS
        .iter()
        .chain(PLATFORM_PICKS.iter())
        .find(|p| p.key == key)
}

/// Where one pick sends a member (legacy `Destination`). `degraded` is
/// surfaced in the weekly numbers: non-zero means members are being sent to
/// the hub because the real room is still dark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub key: String,
    pub label: String,
    pub emoji: String,
    /// None when neither the primary nor the fallback is visible.
    pub channel_id: Option<String>,
    pub degraded: bool,
}

/// Resolve one pick to a linkable channel (legacy `resolveDestination`). Both
/// primary and fallback must be checked: an unavailable pick keeps its role
/// but has no route. A visible hub-by-design is not flagged degraded.
#[must_use]
pub fn resolve_destination(pick: &GamePick, visible: &dyn Fn(&str) -> bool) -> Destination {
    let (channel_id, degraded) = match pick.primary_channel_id {
        Some(primary) if visible(primary) => (Some(primary.to_owned()), false),
        _ if visible(pick.fallback_channel_id) => (
            Some(pick.fallback_channel_id.to_owned()),
            pick.primary_channel_id.is_some(),
        ),
        _ => (None, false),
    };
    Destination {
        key: pick.key.to_owned(),
        label: pick.label.to_owned(),
        emoji: pick.emoji.to_owned(),
        channel_id,
        degraded,
    }
}

/// Planned game selection (legacy `SelectionResult`). Owned data, no Discord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameSelection {
    /// Roles to hold afterwards — the menu returns the whole answer, not a delta.
    pub role_ids: Vec<String>,
    pub destinations: Vec<Destination>,
    /// Visible channels to link, in first-submission order, deduped.
    pub channel_ids: Vec<String>,
    pub unknown_keys: Vec<String>,
    pub degraded_count: usize,
}

/// Turn "member ticked these boxes" into "grant these roles, link these
/// channels" (legacy `planSelection`). Pure: touches neither Discord nor the
/// database. Unknown keys are reported, never routed.
#[must_use]
pub fn plan_game_selection(keys: &[&str], visible: &dyn Fn(&str) -> bool) -> GameSelection {
    let mut picks = Vec::new();
    let mut unknown_keys = Vec::new();
    let mut seen = HashSet::new();
    for key in keys {
        // First occurrence wins for known and unknown keys alike.
        if !seen.insert(*key) {
            continue;
        }
        match pick_by_key(key) {
            Some(pick) => picks.push(pick),
            None => unknown_keys.push((*key).to_owned()),
        }
    }
    let destinations: Vec<Destination> = picks
        .iter()
        .map(|p| resolve_destination(p, visible))
        .collect();
    let mut channel_ids = Vec::new();
    for d in &destinations {
        if let Some(channel_id) = &d.channel_id {
            if !channel_ids.contains(channel_id) {
                channel_ids.push(channel_id.clone());
            }
        }
    }
    GameSelection {
        role_ids: picks.iter().map(|p| p.role_id.to_owned()).collect(),
        degraded_count: destinations.iter().filter(|d| d.degraded).count(),
        destinations,
        channel_ids,
        unknown_keys,
    }
}

impl GameSelection {
    /// Picks that keep their role but have no visible destination (legacy
    /// `unavailable`): they never contribute a link or a successful route.
    #[must_use]
    pub fn unavailable_keys(&self) -> Vec<String> {
        self.destinations
            .iter()
            .filter(|destination| destination.channel_id.is_none())
            .map(|destination| destination.key.clone())
            .collect()
    }
}

/// Which game keys a member already holds, so the picker opens ticked with
/// their current answers (legacy `currentGameKeys`). Platform roles are menu
/// state, not game state — same as legacy, which scans `GAME_PICKS` only.
#[must_use]
pub fn current_game_keys(member_role_ids: &[&str]) -> Vec<String> {
    GAME_PICKS
        .iter()
        .filter(|p| member_role_ids.contains(&p.role_id))
        .map(|p| p.key.to_owned())
        .collect()
}

/// Legacy welcome copy (legacy `welcomeText`). Tone is the CEO's to rewrite;
/// the intro-channel link is the load-bearing part.
#[must_use]
pub fn legacy_welcome_text(member_id: u64) -> String {
    [
        format!("<@{member_id}> welcome to TWO."),
        String::new(),
        "Pick what you play below and I will open the right channels for you.".to_owned(),
        format!(
            "You can change this any time, and there is an intro thread in <#{INTRO_CHANNEL_ID}> if you want one."
        ),
    ]
    .join("\n")
}

/// Discord channel link (legacy `channelLink`).
#[must_use]
pub fn channel_link(guild_id: u64, channel_id: &str) -> String {
    format!("https://discord.com/channels/{guild_id}/{channel_id}")
}

/// Copy when role writes fail (almost always hierarchy — legacy
/// `handleGameSelect` fallback). Adapter-visible so the executor renders it.
pub const PICKER_ROLE_FAILURE_REPLY: &str =
    "I could not set those roles - my own permissions are wrong. Staff have been notified in the logs.";
/// Dry-run picker reply (legacy `handleGameSelect` dry-run branch).
pub const PICKER_DRY_RUN_REPLY: &str = "Dry run: no roles were changed.";
/// Cleared-selection reply (legacy empty-`roleIds` branch).
pub const PICKER_CLEARED_REPLY: &str =
    "Cleared your game roles. Open the menu again whenever you like.";

/// Game-picker outcome for the executor (from `handleGameSelect`):
/// add/remove roles to match the menu exactly, ephemeral reply, funnel writes
/// after success. `None` (no outcome) in session mode — the router must not
/// route the game select there at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GamePickerOutcome {
    pub add_role_ids: Vec<String>,
    pub remove_role_ids: Vec<String>,
    pub reply: String,
    /// Always true: only the clicker sees the result (keeps landing readable).
    pub ephemeral: bool,
    /// Record `game_roles_selected` — only when at least one game was granted
    /// (legacy skips the row on a pure clear: the clear branch replies and
    /// returns before either recorder call).
    pub record_selected: bool,
    /// Record `channel_routed` after success only with visible destinations;
    /// clearing roles or saving roles with no route records nothing routed.
    pub record_routed: bool,
    /// Provisional until [`Self::finalize_after_grant`] re-resolves visibility
    /// after successful role writes, updating the plan, reply and routed flag
    /// together. A just-added role can itself reveal the channel to link.
    ///
    /// If the executor's role writes fail it replies
    /// [`PICKER_ROLE_FAILURE_REPLY`] and records nothing (legacy catch path).
    pub routed: GameSelection,
}

impl GamePickerOutcome {
    /// Finalize a successful role selection using refreshed member visibility.
    /// The executor must call this after role writes and before replying or
    /// recording funnel events. Dry runs and clears retain their special reply
    /// and record no events. Role deltas and unknown-key diagnostics survive.
    #[must_use]
    pub fn finalize_after_grant(mut self, visible: &dyn Fn(&str) -> bool, guild_id: u64) -> Self {
        if !self.record_selected {
            return self;
        }
        let keys: Vec<&str> = self
            .routed
            .destinations
            .iter()
            .map(|destination| destination.key.as_str())
            .chain(self.routed.unknown_keys.iter().map(String::as_str))
            .collect();
        let plan = plan_game_selection(&keys, visible);
        self.reply = game_picker_reply(&plan, guild_id);
        self.record_routed = !plan.channel_ids.is_empty();
        self.routed = plan;
        self
    }
}

/// Build the successful ephemeral reply from a post-grant visibility plan.
#[must_use]
pub fn game_picker_reply(plan: &GameSelection, guild_id: u64) -> String {
    let heading = if plan.channel_ids.is_empty() {
        "Game roles saved."
    } else {
        "Done. Here is where to go:"
    };
    let mut lines = vec![heading.to_owned(), String::new()];
    let mut seen = HashSet::new();
    for destination in &plan.destinations {
        let target = match &destination.channel_id {
            Some(channel_id) => channel_link(guild_id, channel_id),
            None => "No channel is available to you right now.".to_owned(),
        };
        let line = format!(
            "{} **{}** → {}",
            destination.emoji, destination.label, target
        );
        if seen.insert(line.clone()) {
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// Adjudicate one game-picker submission. `member_role_ids` is the member's
/// current role set; unticked game roles they hold are removed so roles never
/// drift from the declared answer. Unknown keys are logged by the caller, not
/// shown to the member.
pub fn adjudicate_game_select(
    mode: OnboardingMode,
    selected_keys: &[&str],
    member_role_ids: &[&str],
    visible: &dyn Fn(&str) -> bool,
    guild_id: u64,
    dry_run: bool,
) -> Option<GamePickerOutcome> {
    if !game_picker_allowed(mode) {
        return None;
    }
    if dry_run {
        let routed = plan_game_selection(selected_keys, visible);
        return Some(GamePickerOutcome {
            add_role_ids: Vec::new(),
            remove_role_ids: Vec::new(),
            reply: PICKER_DRY_RUN_REPLY.to_owned(),
            ephemeral: true,
            record_selected: false,
            record_routed: false,
            routed,
        });
    }
    let plan = plan_game_selection(selected_keys, visible);
    if plan.role_ids.is_empty() {
        // The clear branch replies and returns before either recorder call —
        // roles are removed but no funnel rows are written.
        return Some(GamePickerOutcome {
            add_role_ids: Vec::new(),
            remove_role_ids: GAME_PICKS
                .iter()
                .map(|p| p.role_id.to_owned())
                .filter(|r| member_role_ids.contains(&r.as_str()))
                .collect(),
            reply: PICKER_CLEARED_REPLY.to_owned(),
            ephemeral: true,
            record_selected: false,
            record_routed: false,
            routed: plan,
        });
    }
    let selected: HashSet<&str> = plan.role_ids.iter().map(String::as_str).collect();
    let remove_role_ids = GAME_PICKS
        .iter()
        .map(|p| p.role_id)
        .filter(|r| !selected.contains(r) && member_role_ids.contains(r))
        .map(str::to_owned)
        .collect();
    // Provisional: finalize_after_grant refreshes the reply, plan and recording
    // flag together after successful role writes, using current permissions.
    Some(GamePickerOutcome {
        add_role_ids: plan.role_ids.clone(),
        remove_role_ids,
        reply: game_picker_reply(&plan, guild_id),
        ephemeral: true,
        record_selected: true,
        record_routed: !plan.channel_ids.is_empty(),
        routed: plan,
    })
}

// --- session picker ------------------------------------------------------------
// Roleless routing (TOG-1654/TOG-1644): "what do you want to do right now"
// hands a member a link to a room, never a role. The zero-role-delta
// guarantee is structural — no role field exists anywhere in this section.

/// One session-menu entry (legacy `SessionPick`). Keys are stable: they ride
/// in the select-menu value and in event metadata, and a posted panel
/// outlives every restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPick {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub emoji: &'static str,
    /// Destination room. Must be visible to a plain member.
    pub channel_id: String,
}

/// Build the two-option catalog from per-guild channel ids (legacy
/// `buildSessionPicks`). Ids are deployment data and must never be shared
/// across guilds, so the catalog is built per guild, not constant.
#[must_use]
pub fn build_session_picks(looking_to_play: &str, lobby_voice: &str) -> [SessionPick; 2] {
    [
        SessionPick {
            key: "find-players",
            label: "Find people to play with",
            description: "Post the game, your platform and a start time.",
            emoji: "🎲",
            channel_id: looking_to_play.to_owned(),
        },
        SessionPick {
            key: "join-voice",
            label: "Join voice now",
            description: "The Lobby is open - see who is around.",
            emoji: "🔊",
            channel_id: lobby_voice.to_owned(),
        },
    ]
}

/// Staging fixture catalog (legacy `SESSION_PICKS`). Pins the staging proof;
/// runtime handlers build per-guild picks from config instead.
#[must_use]
pub fn staging_session_picks() -> [SessionPick; 2] {
    build_session_picks(
        STAGING_LOOKING_TO_PLAY_CHANNEL_ID,
        STAGING_LOBBY_VOICE_CHANNEL_ID,
    )
}

/// Look a session key up in a catalog (legacy `pickByKey` overload).
#[must_use]
pub fn session_pick_by_key<'a>(key: &str, catalog: &'a [SessionPick]) -> Option<&'a SessionPick> {
    catalog.iter().find(|p| p.key == key)
}

/// Planned session selection (legacy `SessionPlan`). Owned data, no Discord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPlan {
    /// Known picks, catalog order, deduped.
    pub picks: Vec<String>,
    /// Destinations the member can actually open, deduped, catalog order.
    pub channel_ids: Vec<String>,
    /// Known picks withheld because the member cannot view them right now.
    pub unavailable: Vec<String>,
    pub unknown_keys: Vec<String>,
}

/// Turn "member submitted these keys" into "acknowledge this, link that"
/// (legacy `planSession`). Pure. Unknown keys are reported and skipped; valid
/// picks still route in catalog order. Only an empty or wholly unroutable
/// submission has no links, and stale choices get a retry note in the ack.
#[must_use]
pub fn plan_session(
    keys: &[&str],
    visible: &dyn Fn(&str) -> bool,
    catalog: &[SessionPick],
) -> SessionPlan {
    let mut unknown_keys = Vec::new();
    let mut wanted = HashSet::new();
    for key in keys {
        match session_pick_by_key(key, catalog) {
            None => unknown_keys.push((*key).to_owned()),
            Some(pick) => {
                wanted.insert(pick.key);
            }
        }
    }
    let picks: Vec<&SessionPick> = catalog
        .iter()
        .filter(|pick| wanted.contains(pick.key))
        .collect();
    let mut channel_ids = Vec::new();
    let mut unavailable = Vec::new();
    for pick in &picks {
        if visible(&pick.channel_id) {
            if !channel_ids.contains(&pick.channel_id) {
                channel_ids.push(pick.channel_id.clone());
            }
        } else {
            unavailable.push(pick.key.to_owned());
        }
    }
    SessionPlan {
        picks: picks.iter().map(|p| p.key.to_owned()).collect(),
        channel_ids,
        unavailable,
        unknown_keys,
    }
}

/// Ephemeral acknowledgement, one per submission, visible only to the clicker
/// (legacy `sessionAckText`). Re-selecting the same option yields
/// byte-identical text — that determinism is the idempotency the TOG-1654
/// acceptance asks for.
#[must_use]
pub fn session_ack_text(plan: &SessionPlan) -> String {
    if plan.channel_ids.is_empty() {
        if !plan.unknown_keys.is_empty() {
            return [
                "That option is gone or stale - the panel was probably replaced by a newer one.",
                "Nothing was changed. Open the picker again and choose afresh.",
            ]
            .join("\n");
        }
        return [
            "Those rooms are not open to you right now.",
            "Nothing was changed - try again in a moment, or say hello in the welcome channel and someone will grab you.",
        ].join("\n");
    }
    let links: Vec<String> = plan
        .channel_ids
        .iter()
        .map(|id| format!("<#{id}>"))
        .collect();
    let mut ack = format!("On it - head to {}.", links.join(" and "));
    if !plan.unknown_keys.is_empty() {
        ack.push_str(" One choice was gone or stale, so I skipped it - open the picker again if you want to re-pick that part.");
    }
    ack
}

/// Session welcome copy (legacy `sessionWelcomeText`). Picks a destination
/// for tonight, not a label forever.
#[must_use]
pub fn session_welcome_text(member_id: u64) -> String {
    [
        format!("<@{member_id}> you're in - that was the whole application."),
        String::new(),
        "What do you want to do right now? Pick below and I will point you at the right room. You can change your mind any time - this picks a destination for tonight, not a label forever.".to_owned(),
    ]
    .join("\n")
}

/// Session-picker outcome for the executor (from `handleSessionSelect`).
/// There is deliberately no dry-run branch — the legacy handler always acks;
/// dry run gates the welcome/goodbye posts, never the picker's reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPickerOutcome {
    pub reply: String,
    /// Always true: only the clicker sees the result.
    pub ephemeral: bool,
    /// Record `channel_routed` — only when the plan produced destinations
    /// (a repeatable funnel event: re-selecting records again but changes no
    /// member state, because there is none to change).
    pub record_routed: bool,
    pub routed: SessionPlan,
}

/// Adjudicate one session-picker submission.
#[must_use]
pub fn adjudicate_session_select(
    keys: &[&str],
    visible: &dyn Fn(&str) -> bool,
    catalog: &[SessionPick],
) -> SessionPickerOutcome {
    let routed = plan_session(keys, visible, catalog);
    SessionPickerOutcome {
        reply: session_ack_text(&routed),
        ephemeral: true,
        record_routed: !routed.channel_ids.is_empty(),
        routed,
    }
}

// --- anchor (Sunday Squad) ------------------------------------------------------
// Spec: TWO-66 §5.3–§5.4 via `src/onboarding/anchorEvent.ts`. The recurrence is
// 20:00 America/New_York — wall-clock, never a fixed second count, so the
// week US DST ends (169 hours, not 168) cannot drift.

/// Anchor spec (legacy `AnchorEventSpec`). Only Sunday is exercised; the
/// weekday field stays so the spec's shape survives the port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchorSpec {
    pub name: &'static str,
    /// Voice room the event runs in, and the channel the welcome posts into
    /// (discord.js v14 voice text-chat surface = the channel itself).
    pub channel_id: &'static str,
    /// 0 = Sunday, matching `Date.getUTCDay`.
    pub weekday: u32,
    /// Local wall-clock start.
    pub hour: u32,
    pub minute: u32,
    pub duration_minutes: u64,
    /// Run 1: Sunday 23 August 2026 20:00 America/New_York, as epoch seconds.
    /// Anchoring anywhere but run 1 shifts every later occurrence (the trap
    /// named in the spec: 30 August is run 2).
    pub series_start_epoch: i64,
}

/// The Sunday Squad (legacy `SUNDAY_SQUAD`). Fall Guys, an hour, every Sunday.
pub const SUNDAY_SQUAD: AnchorSpec = AnchorSpec {
    name: "Sunday Squad",
    channel_id: ANCHOR_CHANNEL_ID,
    weekday: 0,
    hour: 20,
    minute: 0,
    duration_minutes: 60,
    series_start_epoch: 1_787_529_600,
};

/// Near-event window: under two hours before an occurrence (TWO-66 §5.3).
pub const NEAR_EVENT_SECS: i64 = 2 * 60 * 60;

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as i64;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: `(y, m, d)` for a day count.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
}

/// Weekday of a day count, Sunday = 0 (1970-01-01 was a Thursday).
fn weekday_of_days(days: i64) -> u32 {
    (days.rem_euclid(7) + 4) as u32 % 7
}

/// Nth weekday of a month (1-based), as day-of-month. US DST: second Sunday
/// of March, first Sunday of November.
fn nth_weekday_of_month(y: i32, m: u32, weekday: u32, n: u32) -> u32 {
    let first = weekday_of_days(days_from_civil(y, m, 1));
    1 + (weekday + 7 - first) % 7 + (n - 1) * 7
}

/// New York offset in seconds east of UTC at `utc_secs` (real 2007+ rules:
/// EDT −04:00 from the second Sunday of March 02:00 local to the first
/// Sunday of November 02:00 local, EST −05:00 otherwise).
fn ny_offset_secs(utc_secs: i64) -> i64 {
    let days = utc_secs.div_euclid(86_400);
    let (mut y, m, _) = civil_from_days(days);
    // The March Sundays that fall in January/February belong to the DST year
    // starting that March; a November UTC date is past the fall-back.
    if m < 3 {
        y -= 1;
    }
    let spring_day = nth_weekday_of_month(y, 3, 0, 2);
    let fall_day = nth_weekday_of_month(y, 11, 0, 1);
    // 02:00 local = 07:00Z (EDT) in spring, 06:00Z (EST) in fall.
    let spring_utc = days_from_civil(y, 3, spring_day) * 86_400 + 7 * 3600;
    let fall_utc = days_from_civil(y, 11, fall_day) * 86_400 + 6 * 3600;
    if utc_secs >= spring_utc && utc_secs < fall_utc {
        -4 * 3600
    } else {
        -5 * 3600
    }
}

/// The instant at which the New York wall clock reads this wall time
/// (legacy `zonedEpochMs`). Two passes: the first guess uses the offset at
/// the naive instant — wrong side of the boundary on the two changeover days
/// — and re-reading the offset at that guess fixes it.
pub fn zoned_epoch_secs(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    let naive = days_from_civil(year, month, day) * 86_400
        + i64::from(hour) * 3600
        + i64::from(minute) * 60;
    let guess = naive - ny_offset_secs(naive);
    naive - ny_offset_secs(guess)
}

/// Next Sunday 20:00 NY strictly after `now_secs`, as epoch secs
/// (legacy `nextOccurrenceMs`). Walks local calendar dates, converting each
/// independently, so the series never drifts across a DST boundary.
pub fn next_anchor_occurrence(now_secs: i64, spec: AnchorSpec) -> i64 {
    let local_now = now_secs + ny_offset_secs(now_secs);
    let today = local_now.div_euclid(86_400);
    // Any 9-day window contains the weekday; 10 iterations bound the loop.
    // Today is checked first: a member joining Sunday 19:00 local is told
    // about *today's* 20:00, not next week's.
    for add in 0..10 {
        let days = today + add;
        if weekday_of_days(days) != spec.weekday {
            continue;
        }
        let (y, m, d) = civil_from_days(days);
        let start = zoned_epoch_secs(y, m, d, spec.hour, spec.minute);
        if start > now_secs {
            return start;
        }
    }
    // Defensive: unreachable for weekday 0–6, but a garbage spec must not
    // hang the caller.
    zoned_epoch_secs(2030, 1, 6, spec.hour, spec.minute)
}

/// Most recent Sunday 20:00 NY at or before `now_secs`.
fn previous_anchor_occurrence(now_secs: i64, spec: AnchorSpec) -> Option<i64> {
    let local_now = now_secs + ny_offset_secs(now_secs);
    let mut days = local_now.div_euclid(86_400);
    for _ in 0..10 {
        if weekday_of_days(days) == spec.weekday {
            let (y, m, d) = civil_from_days(days);
            let start = zoned_epoch_secs(y, m, d, spec.hour, spec.minute);
            if start <= now_secs {
                return Some(start);
            }
        }
        days -= 1;
    }
    None
}

/// Which occurrence to name, and in which voice (legacy `OccurrenceContext`).
/// An occurrence still in progress counts as near and is the one named —
/// otherwise a member joining at 20:30 would be told about next Sunday while
/// the event runs in the room they are reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OccurrenceContext {
    pub start_epoch: i64,
    pub near: bool,
    pub live: bool,
}

pub fn occurrence_context(now_secs: i64, spec: AnchorSpec) -> OccurrenceContext {
    if let Some(prev) = previous_anchor_occurrence(now_secs, spec) {
        if now_secs < prev + (spec.duration_minutes as i64) * 60 {
            return OccurrenceContext {
                start_epoch: prev,
                near: true,
                live: true,
            };
        }
    }
    let next = next_anchor_occurrence(now_secs, spec);
    OccurrenceContext {
        start_epoch: next,
        near: next - now_secs < NEAR_EVENT_SECS,
        live: false,
    }
}

/// Anchor welcome copy (legacy `anchorWelcomeText`, TWO-66 §5.3 verbatim).
/// Nothing may be appended — no picker, no buttons, no footer.
#[must_use]
pub fn anchor_welcome_text(member_id: u64, now_secs: i64, spec: AnchorSpec) -> String {
    let ctx = occurrence_context(now_secs, spec);
    let middle = if ctx.near {
        format!(
            "The thing to know: **{}** is happening right now in <#{}> — Fall Guys, for about another hour. Come say hi. You don't need it installed to join in.",
            spec.name, spec.channel_id
        )
    } else {
        format!(
            "The thing to know: **{}**, every Sunday at 8pm Eastern in <#{}>. We play Fall Guys for about an hour. Next one is <t:{}:R>.",
            spec.name, spec.channel_id, ctx.start_epoch
        )
    };
    [
        format!("Hey <@{member_id}> — glad you're here."),
        String::new(),
        middle,
        String::new(),
        "You don't need to sign up or say anything first — just join the voice room and I'll get you into the party. Haven't got Fall Guys? Come anyway, there's something we can play right there in the room. If you can't make Sunday, hop in whenever and see who's about.".to_owned(),
    ]
    .join("\n")
}

// --- welcome + goodbye effects ---------------------------------------------------
// One outcome enum per gateway event. The router matches; the REST executor
// renders. `Skip.reason` mirrors the legacy debug/log line names so staging
// logs stay greppable (`onboarding_skip`, `anchor_welcome_skip`,
// `session_welcome_no_channel`, `session_goodbye_posted`).

/// Welcome outcome for join / gate-clear (all three modes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WelcomeEffect {
    /// Caller could not post (no landing channel) or dry run decided against
    /// posting: decide + log, post nothing, record nothing.
    Skip { reason: &'static str },
    /// Post the welcome in `channel_id`. The executor resolves the channel
    /// and checks postability; the domain only names the preference order.
    Post {
        channel_id: String,
        content: String,
        /// Users to mention: exactly the new member. Roles/everyone never.
        mention_user_id: u64,
        /// Game picker attached (legacy + session). Anchor posts none.
        picker: Option<PickerKind>,
    },
}

/// Which picker rides on the welcome post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Games,
    Session,
}

/// Pre-selected menu values for an already-answered picker (re-open ticked,
/// so "change it any time" is one click). Games only; session is stateless.
#[must_use]
pub fn preselected_game_keys(member_role_ids: &[&str]) -> Vec<String> {
    current_game_keys(member_role_ids)
}

/// Adjudicate the welcome for one promptable member. The caller has already
/// run [`decide_prompt`] and picked a postable guild channel (no DMs). Session
/// welcomes still post in dry run because they never grant roles; legacy and
/// anchor do not. The executor acquires the store's prompt guard before sending.
#[must_use]
pub fn adjudicate_welcome(
    mode: OnboardingMode,
    member_id: u64,
    landing_channel_id: Option<&str>,
    anchor_channel_id: &str,
    now_secs: i64,
    anchor_spec: AnchorSpec,
    dry_run: bool,
) -> WelcomeEffect {
    if mode != OnboardingMode::Anchor && landing_channel_id.is_none() {
        return WelcomeEffect::Skip {
            reason: "no_landing_channel",
        };
    }
    if dry_run && mode != OnboardingMode::Session {
        return WelcomeEffect::Skip { reason: "dry_run" };
    }
    match mode {
        OnboardingMode::Legacy => match landing_channel_id {
            None => WelcomeEffect::Skip {
                reason: "no_landing_channel",
            },
            Some(channel) => WelcomeEffect::Post {
                channel_id: channel.to_owned(),
                content: legacy_welcome_text(member_id),
                mention_user_id: member_id,
                picker: Some(PickerKind::Games),
            },
        },
        OnboardingMode::Session => match landing_channel_id {
            None => WelcomeEffect::Skip {
                reason: "no_landing_channel",
            },
            Some(channel) => WelcomeEffect::Post {
                channel_id: channel.to_owned(),
                content: session_welcome_text(member_id),
                mention_user_id: member_id,
                picker: Some(PickerKind::Session),
            },
        },
        OnboardingMode::Anchor => WelcomeEffect::Post {
            channel_id: anchor_channel_id.to_owned(),
            content: anchor_welcome_text(member_id, now_secs, anchor_spec),
            mention_user_id: member_id,
            picker: None,
        },
    }
}

/// Explicit executor contract: disable mention parsing, allowing only the
/// named welcome recipient (when present). Never allow roles or everyone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionPolicy {
    None,
    Member(u64),
}

/// Goodbye outcome for member-remove (session mode only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoodbyeEffect {
    /// No goodbye channel, or dry run: log, post nothing.
    Skip { reason: &'static str },
    /// Post the goodbye with `MentionPolicy::None` (`parse: []`). The person
    /// who left is named in plain text, never pinged.
    Post {
        channel_id: String,
        content: String,
        mentions: MentionPolicy,
    },
}

/// Days between join and leave, floored; `None` when the join is unknown
/// (legacy `daysInGuild`).
#[must_use]
pub fn days_in_guild(joined_at_ms: Option<i64>, left_at_ms: Option<i64>) -> Option<i64> {
    match (joined_at_ms, left_at_ms) {
        (Some(joined), Some(left)) => Some((left - joined).div_euclid(86_400_000)),
        _ => None,
    }
}

/// Goodbye copy (legacy `goodbyeText`). Plain on purpose: no guilt, no
/// retention pitch. The notice (who, when, how long) is the product; the stay
/// detail already lands in the funnel as `member_leave`.
///
/// The username is legacy-trusted display text; the executor must render it
/// as plain text (no mention parsing) — `GoodbyeEffect::Post` explicitly
/// supplies `MentionPolicy::None`.
#[must_use]
pub fn goodbye_text(username: &str, days: Option<i64>) -> String {
    let stay = match days {
        None => String::new(),
        Some(d) if d <= 0 => " (was here less than a day)".to_owned(),
        Some(1) => " (was here 1 day)".to_owned(),
        Some(d) => format!(" (was here {d} days)"),
    };
    format!(
        "**{username}** left the server{stay}. Their messages and voice history stay on the books."
    )
}

/// Adjudicate the goodbye. Non-session modes have no goodbye surface at all
/// (legacy only registers `GuildMemberRemove` in `registerSessionWelcome`);
/// the router should not call this otherwise, but the `None` keeps that
/// structural.
#[must_use]
pub fn adjudicate_goodbye(
    mode: OnboardingMode,
    username: &str,
    days: Option<i64>,
    goodbye_channel_id: Option<&str>,
    dry_run: bool,
) -> Option<GoodbyeEffect> {
    if mode != OnboardingMode::Session {
        return None;
    }
    if dry_run {
        return Some(GoodbyeEffect::Skip { reason: "dry_run" });
    }
    match goodbye_channel_id {
        None => Some(GoodbyeEffect::Skip {
            reason: "no_goodbye_channel",
        }),
        Some(channel) => Some(GoodbyeEffect::Post {
            channel_id: channel.to_owned(),
            content: goodbye_text(username, days),
            mentions: MentionPolicy::None,
        }),
    }
}

// --- funnel rows ---------------------------------------------------------------
// Pure row builders for the funnel log (`OnboardingRecorder` /
// `SessionRecorder` writes). The store module inserts them; the idempotency
// keys are byte-identical to legacy `idempotencyKey()` so rows written by
// either implementation dedupe against each other.

/// Funnel event type strings written by onboarding (legacy `EVENT_TYPES`
/// spellings — also the `event_type` column values).
pub const EVENT_ONBOARDING_PROMPTED: &str = "onboarding_prompted";
pub const EVENT_GAME_ROLES_SELECTED: &str = "game_roles_selected";
pub const EVENT_CHANNEL_ROUTED: &str = "channel_routed";

/// Picker attribution sources (legacy `source` strings).
pub const SOURCE_PICKER: &str = "picker";
pub const SOURCE_SESSION_PICKER: &str = "session-picker";

/// One funnel row to insert. Snowflakes ride as text (the `events` columns
/// are `TEXT`); `occurred_at` is emitter-set ISO-8601 UTC, never DB-defaulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunnelRow {
    pub guild_id: String,
    pub member_id: String,
    pub event_type: &'static str,
    pub occurred_at: String,
    pub source: String,
    pub metadata: Option<String>,
}

/// Stable idempotency key, byte-identical to legacy `idempotencyKey()` for
/// these types: `onboarding_prompted` is once-per-member; the other two
/// repeat by design (re-picks) and key on member + time.
#[must_use]
pub fn funnel_idempotency_key(row: &FunnelRow) -> String {
    if row.event_type == EVENT_ONBOARDING_PROMPTED {
        format!("{}:{}:{}", row.guild_id, row.member_id, row.event_type)
    } else {
        format!(
            "{}:{}:{}:{}",
            row.guild_id, row.member_id, row.event_type, row.occurred_at
        )
    }
}

/// The welcome went out (legacy `OnboardingRecorder::prompted` /
/// `SessionRecorder::prompted`). No metadata; source names the channel.
#[must_use]
pub fn prompted_row(
    guild_id: &str,
    member_id: &str,
    channel_id: &str,
    occurred_at: &str,
) -> FunnelRow {
    FunnelRow {
        guild_id: guild_id.to_owned(),
        member_id: member_id.to_owned(),
        event_type: EVENT_ONBOARDING_PROMPTED,
        occurred_at: occurred_at.to_owned(),
        source: format!("channel:{channel_id}"),
        metadata: None,
    }
}

/// Game keys only — no usernames, no message text (legacy `selected`).
#[must_use]
pub fn game_selected_row(
    guild_id: &str,
    member_id: &str,
    keys: &[String],
    occurred_at: &str,
) -> FunnelRow {
    let metadata = serde_json::json!({ "picks": keys });
    FunnelRow {
        guild_id: guild_id.to_owned(),
        member_id: member_id.to_owned(),
        event_type: EVENT_GAME_ROLES_SELECTED,
        occurred_at: occurred_at.to_owned(),
        source: SOURCE_PICKER.to_owned(),
        metadata: Some(metadata.to_string()),
    }
}

/// Legacy game/anchor routed row (legacy `OnboardingRecorder::routed`,
/// source `picker`). `degraded` is surfaced in the weekly numbers: non-zero
/// means members are being sent to the hub because the real room is dark.
/// `unavailable` names picks that kept their role but had no visible
/// destination; the key is present only when at least one pick is unavailable,
/// so fully routed rows stay byte-identical to legacy.
#[must_use]
pub fn channel_routed_row(
    guild_id: &str,
    member_id: &str,
    channel_ids: &[String],
    degraded_count: usize,
    unavailable: &[String],
    occurred_at: &str,
) -> FunnelRow {
    let mut metadata = serde_json::json!({ "channels": channel_ids, "degraded": degraded_count });
    if !unavailable.is_empty() {
        metadata["unavailable"] = serde_json::json!(unavailable);
    }
    FunnelRow {
        guild_id: guild_id.to_owned(),
        member_id: member_id.to_owned(),
        event_type: EVENT_CHANNEL_ROUTED,
        occurred_at: occurred_at.to_owned(),
        source: SOURCE_PICKER.to_owned(),
        metadata: Some(metadata.to_string()),
    }
}

/// Session routed row (legacy `SessionRecorder::routed`, source
/// `session-picker`). Deliberately no `game_roles_selected` alongside it:
/// there are no roles, so there is no selection to record, and a row with an
/// empty picks list would read as a bug in the weekly numbers.
#[must_use]
pub fn session_routed_row(
    guild_id: &str,
    member_id: &str,
    plan: &SessionPlan,
    occurred_at: &str,
) -> FunnelRow {
    let metadata = serde_json::json!({
        "picks": plan.picks,
        "channels": plan.channel_ids,
        "unavailable": plan.unavailable,
    });
    FunnelRow {
        guild_id: guild_id.to_owned(),
        member_id: member_id.to_owned(),
        event_type: EVENT_CHANNEL_ROUTED,
        occurred_at: occurred_at.to_owned(),
        source: SOURCE_SESSION_PICKER.to_owned(),
        metadata: Some(metadata.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMBER: u64 = 900_000_000_000_007_777;
    /// Saturday 22 August 2026 12:00Z — standing just before run 1.
    const BEFORE_RUN_1: i64 = 1_787_400_000;
    const RUN_1: i64 = 1_787_529_600;

    fn see_everything() -> impl Fn(&str) -> bool {
        |_| true
    }

    fn see_nothing() -> impl Fn(&str) -> bool {
        |_| false
    }

    fn see_hub() -> impl Fn(&str) -> bool {
        |channel_id| channel_id == GAME_HUB_CHANNEL_ID
    }

    fn catalog() -> [SessionPick; 2] {
        staging_session_picks()
    }

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    // --- gates ------------------------------------------------------------------

    #[test]
    fn gates_default_to_legacy_without_dry_run() {
        let gates = OnboardingGates::from_map(&HashMap::new()).expect("defaults");
        assert_eq!(gates.mode, OnboardingMode::Legacy);
        assert!(!gates.dry_run);
    }

    #[test]
    fn gates_parse_all_three_modes_and_reject_garbage() {
        for (raw, mode) in [
            ("legacy", OnboardingMode::Legacy),
            ("session", OnboardingMode::Session),
            ("anchor", OnboardingMode::Anchor),
        ] {
            let gates =
                OnboardingGates::from_map(&vars(&[("TWO_ONBOARDING_MODE", raw)])).expect("parses");
            assert_eq!(gates.mode, mode);
            assert_eq!(gates.mode.as_str(), raw);
        }
        assert_eq!(
            OnboardingGates::from_map(&vars(&[("TWO_ONBOARDING_MODE", "fortnite")])),
            Err(OnboardingGateError::InvalidMode("fortnite".to_owned()))
        );
        let gates =
            OnboardingGates::from_map(&vars(&[("TWO_ONBOARDING_DRY_RUN", "1")])).expect("parses");
        assert!(gates.dry_run);
    }

    #[test]
    fn session_mode_suppresses_role_writes_everywhere_else_keeps_them() {
        assert!(!level_role_writes_allowed(OnboardingMode::Session));
        assert!(level_role_writes_allowed(OnboardingMode::Legacy));
        assert!(level_role_writes_allowed(OnboardingMode::Anchor));
        assert!(!game_picker_allowed(OnboardingMode::Session));
        assert!(game_picker_allowed(OnboardingMode::Legacy));
        assert!(game_picker_allowed(OnboardingMode::Anchor));
    }

    // --- prompt gate ----------------------------------------------------------------

    #[test]
    fn prompt_gate_refuses_bots_pending_and_repeats_in_that_order() {
        assert_eq!(
            decide_prompt(false, true, false),
            PromptDecision::Skip(PromptSkip::StillPending)
        );
        assert_eq!(
            decide_prompt(true, false, false),
            PromptDecision::Skip(PromptSkip::Bot)
        );
        assert_eq!(
            decide_prompt(false, false, true),
            PromptDecision::Skip(PromptSkip::AlreadyPrompted)
        );
        assert_eq!(decide_prompt(false, false, false), PromptDecision::Prompt);
        // Bots are refused before the gate is even consulted.
        assert_eq!(
            decide_prompt(true, true, false),
            PromptDecision::Skip(PromptSkip::Bot)
        );
    }

    #[test]
    fn gated_joins_wait_for_gate_clear() {
        assert!(!welcome_trigger(MembershipTrigger::Joined {
            pending: true
        }));
        assert!(welcome_trigger(MembershipTrigger::Joined {
            pending: false
        }));
        assert!(welcome_trigger(MembershipTrigger::GateCleared));
    }

    // --- game catalog -----------------------------------------------------------------

    #[test]
    fn catalog_ids_are_well_formed_unique_and_menu_sized() {
        let mut keys = HashSet::new();
        let mut roles = HashSet::new();
        for pick in GAME_PICKS.iter().chain(PLATFORM_PICKS.iter()) {
            assert!(is_snowflake_like(pick.role_id), "{} role", pick.key);
            assert!(
                is_snowflake_like(pick.fallback_channel_id),
                "{} hub",
                pick.key
            );
            if let Some(primary) = pick.primary_channel_id {
                assert!(is_snowflake_like(primary), "{} room", pick.key);
            }
            assert!(pick.label.len() <= 100, "{} label", pick.key);
            assert!(pick.description.len() <= 100, "{} description", pick.key);
            assert!(keys.insert(pick.key), "duplicate key {}", pick.key);
            assert!(roles.insert(pick.role_id), "duplicate role {}", pick.key);
        }
        assert!(GAME_PICKS.len() <= 25 && PLATFORM_PICKS.len() <= 25);
        assert_eq!(GAME_PICKS.len(), 10);
        assert!(pick_by_key("pc").is_some(), "platform keys resolve too");
        assert_eq!(pick_by_key("nope"), None);
    }

    fn is_snowflake_like(s: &str) -> bool {
        (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
    }

    #[test]
    fn visible_room_is_used_dark_room_falls_back_hubless_pick_is_not_degraded() {
        let shooters = pick_by_key("shooters").expect("catalog");
        let used = resolve_destination(shooters, &see_everything());
        assert_eq!(used.channel_id.as_deref(), shooters.primary_channel_id);
        assert!(!used.degraded);
        let dark = resolve_destination(shooters, &see_hub());
        assert_eq!(dark.channel_id.as_deref(), Some(GAME_HUB_CHANNEL_ID));
        assert!(dark.degraded, "fallback must show in the numbers");
        let hub = resolve_destination(pick_by_key("rocketleague").expect("catalog"), &see_hub());
        assert_eq!(hub.channel_id.as_deref(), Some(GAME_HUB_CHANNEL_ID));
        assert!(!hub.degraded, "hub-by-design is not a failure");
    }

    #[test]
    fn game_plan_dedupes_rooms_reports_unknown_and_handles_empty() {
        let plan = plan_game_selection(
            &["shooters", "rocketleague", "fallguys", "not-a-game"],
            &see_hub(),
        );
        assert_eq!(plan.unknown_keys, vec!["not-a-game".to_owned()]);
        assert_eq!(plan.role_ids.len(), 3);
        assert_eq!(plan.channel_ids, vec![GAME_HUB_CHANNEL_ID.to_owned()]);
        assert_eq!(plan.degraded_count, 1, "only the shooters fallback counts");
        let empty = plan_game_selection(&[], &see_everything());
        assert!(empty.role_ids.is_empty() && empty.channel_ids.is_empty());
        assert_eq!(empty.degraded_count, 0);
    }

    #[test]
    fn game_plan_dedupes_known_and_unknown_keys_in_first_submission_order() {
        let plan = plan_game_selection(
            &[
                "horror", "shooters", "horror", "gone", "gone", "shooters", "retired",
            ],
            &see_hub(),
        );
        assert_eq!(
            plan.role_ids,
            vec![
                pick_by_key("horror").unwrap().role_id,
                pick_by_key("shooters").unwrap().role_id,
            ]
        );
        assert_eq!(
            plan.destinations
                .iter()
                .map(|d| d.key.as_str())
                .collect::<Vec<_>>(),
            vec!["horror", "shooters"]
        );
        assert_eq!(plan.degraded_count, 2, "one fallback per distinct pick");
        assert_eq!(plan.channel_ids, vec![GAME_HUB_CHANNEL_ID]);
        assert_eq!(plan.unknown_keys, vec!["gone", "retired"]);
        let unknown = plan_game_selection(&["gone", "gone"], &see_everything());
        assert_eq!(unknown.unknown_keys, vec!["gone"]);
        assert!(unknown.role_ids.is_empty() && unknown.destinations.is_empty());
        assert_eq!(unknown.degraded_count, 0);
    }

    #[test]
    fn invisible_primary_and_fallback_retain_roles_without_a_route() {
        let plan = plan_game_selection(&["shooters", "rocketleague"], &see_nothing());
        assert_eq!(
            plan.role_ids,
            vec![
                pick_by_key("shooters").unwrap().role_id,
                pick_by_key("rocketleague").unwrap().role_id,
            ]
        );
        assert!(plan.channel_ids.is_empty());
        assert!(plan.destinations.iter().all(|d| d.channel_id.is_none()));
        assert_eq!(
            plan.degraded_count, 0,
            "unavailable is not a fallback route"
        );
        let reply = game_picker_reply(&plan, 1);
        assert!(reply.starts_with("Game roles saved."));
        assert!(reply.contains("No channel is available to you right now."));
        assert!(!reply.contains("discord.com/channels") && !reply.contains("<#"));
    }

    #[test]
    fn current_keys_reflect_game_roles_only() {
        let shooters = pick_by_key("shooters").expect("catalog").role_id;
        let horror = pick_by_key("horror").expect("catalog").role_id;
        let pc = pick_by_key("pc").expect("catalog").role_id;
        let keys = current_game_keys(&[shooters, horror, pc, "9999999999999999999"]);
        assert_eq!(keys, vec!["shooters".to_owned(), "horror".to_owned()]);
        assert_eq!(preselected_game_keys(&[pc]), Vec::<String>::new());
    }

    // --- game picker --------------------------------------------------------------------

    #[test]
    fn picker_adds_and_removes_roles_to_match_the_menu() {
        let shooters = pick_by_key("shooters").expect("catalog").role_id;
        let horror = pick_by_key("horror").expect("catalog").role_id;
        // Member holds shooters, now picks horror: horror added, shooters removed.
        let out = adjudicate_game_select(
            OnboardingMode::Legacy,
            &["horror"],
            &[shooters],
            &see_everything(),
            1,
            false,
        )
        .expect("legacy serves the picker");
        assert_eq!(out.add_role_ids, vec![horror.to_owned()]);
        assert_eq!(out.remove_role_ids, vec![shooters.to_owned()]);
        assert!(out.ephemeral, "only the clicker sees the result");
        assert!(out.record_selected && out.record_routed);
        assert!(out.reply.starts_with("Done. Here is where to go:\n\n"));
        assert!(out.reply.contains("https://discord.com/channels/1/"));
    }

    #[test]
    fn picker_clear_removes_roles_but_records_nothing() {
        let shooters = pick_by_key("shooters").expect("catalog").role_id;
        let out = adjudicate_game_select(
            OnboardingMode::Legacy,
            &[],
            &[shooters],
            &see_everything(),
            1,
            false,
        )
        .expect("legacy serves the picker");
        assert_eq!(out.remove_role_ids, vec![shooters.to_owned()]);
        assert!(out.add_role_ids.is_empty());
        assert_eq!(out.reply, PICKER_CLEARED_REPLY);
        // The clear branch returns before either recorder call.
        assert!(!out.record_selected && !out.record_routed);
    }

    #[test]
    fn picker_dry_run_changes_nothing_and_session_serves_nothing() {
        let out = adjudicate_game_select(
            OnboardingMode::Legacy,
            &["shooters"],
            &[],
            &see_everything(),
            1,
            true,
        )
        .expect("dry run still answers");
        assert_eq!(out.reply, PICKER_DRY_RUN_REPLY);
        assert!(out.add_role_ids.is_empty() && out.remove_role_ids.is_empty());
        assert!(!out.record_selected && !out.record_routed);
        assert_eq!(
            adjudicate_game_select(
                OnboardingMode::Session,
                &["shooters"],
                &[],
                &see_everything(),
                1,
                false
            ),
            None,
            "session mode must not route the game select at all"
        );
    }

    #[test]
    fn legacy_welcome_mentions_the_member_and_the_intro_channel() {
        let text = legacy_welcome_text(MEMBER);
        assert!(text.starts_with("<@900000000000007777> welcome to TWO."));
        assert!(text.contains(INTRO_CHANNEL_ID));
    }

    // --- session picker -------------------------------------------------------------------

    #[test]
    fn session_plan_routes_both_picks_deduped_and_ordered() {
        let picks = catalog();
        let plan = plan_session(&["find-players", "join-voice"], &see_everything(), &picks);
        assert_eq!(plan.picks, vec!["find-players", "join-voice"]);
        assert_eq!(
            plan.channel_ids,
            vec![
                STAGING_LOOKING_TO_PLAY_CHANNEL_ID.to_owned(),
                STAGING_LOBBY_VOICE_CHANNEL_ID.to_owned()
            ]
        );
        assert!(plan.unavailable.is_empty() && plan.unknown_keys.is_empty());
    }

    #[test]
    fn session_reselect_is_idempotent_and_dark_rooms_are_withheld() {
        let picks = catalog();
        let once = plan_session(&["find-players"], &see_everything(), &picks);
        let twice = plan_session(&["find-players", "find-players"], &see_everything(), &picks);
        assert_eq!(once, twice);
        assert_eq!(
            session_ack_text(&once),
            session_ack_text(&twice),
            "byte-identical acks"
        );
        let dark = plan_session(&["join-voice"], &see_nothing(), &picks);
        assert_eq!(dark.unavailable, vec!["join-voice".to_owned()]);
        assert!(dark.channel_ids.is_empty());
        assert!(!session_ack_text(&dark).contains(STAGING_LOBBY_VOICE_CHANNEL_ID));
    }

    #[test]
    fn session_valid_picks_survive_stale_keys_and_offer_a_partial_retry() {
        let picks = catalog();
        let plan = plan_session(&["survival", "find-players"], &see_everything(), &picks);
        assert_eq!(plan.unknown_keys, vec!["survival"]);
        assert_eq!(plan.picks, vec!["find-players"]);
        assert_eq!(plan.channel_ids, vec![STAGING_LOOKING_TO_PLAY_CHANNEL_ID]);
        let ack = session_ack_text(&plan);
        assert!(
            ack.contains("stale"),
            "stale choices still get a retry note"
        );
        assert!(ack.contains(&format!("<#{STAGING_LOOKING_TO_PLAY_CHANNEL_ID}>")));
        assert!(!ack.contains("Nothing was changed"));
    }

    #[test]
    fn session_entirely_unknown_or_empty_submissions_route_nothing() {
        let picks = catalog();
        let unknown = plan_session(&["nonsense"], &see_everything(), &picks);
        assert!(unknown.picks.is_empty() && unknown.channel_ids.is_empty());
        assert_eq!(unknown.unknown_keys, vec!["nonsense"]);
        let ack = session_ack_text(&unknown);
        assert!(ack.contains("stale") && ack.contains("Nothing was changed"));
        let empty = adjudicate_session_select(&[], &see_everything(), &picks);
        assert!(empty.routed.picks.is_empty() && empty.routed.channel_ids.is_empty());
        assert!(!empty.record_routed);
        assert!(!empty.reply.contains("<#"));
    }

    #[test]
    fn session_reordered_duplicate_submissions_follow_the_supplied_catalog() {
        let picks = catalog();
        let keys = ["join-voice", "find-players", "join-voice", "find-players"];
        let plan = plan_session(&keys, &see_everything(), &picks);
        assert_eq!(
            plan,
            plan_session(&["find-players", "join-voice"], &see_everything(), &picks)
        );
        let reversed = [picks[1].clone(), picks[0].clone()];
        let plan = plan_session(&keys, &see_everything(), &reversed);
        assert_eq!(plan.picks, vec!["join-voice", "find-players"]);
        assert_eq!(
            plan.channel_ids,
            vec![
                STAGING_LOBBY_VOICE_CHANNEL_ID,
                STAGING_LOOKING_TO_PLAY_CHANNEL_ID
            ]
        );
        let hidden = plan_session(&keys, &see_nothing(), &reversed);
        assert_eq!(hidden.unavailable, vec!["join-voice", "find-players"]);
        assert!(hidden.channel_ids.is_empty());
    }

    #[test]
    fn session_shared_channels_dedupe_without_losing_selected_picks() {
        let picks = build_session_picks("10", "10");
        let plan = plan_session(&["join-voice", "find-players"], &see_everything(), &picks);
        assert_eq!(plan.picks, vec!["find-players", "join-voice"]);
        assert_eq!(plan.channel_ids, vec!["10"]);
        assert_eq!(session_ack_text(&plan), "On it - head to <#10>.");
    }

    #[test]
    fn session_ack_links_destinations() {
        let picks = catalog();
        let ack = session_ack_text(&plan_session(&["find-players"], &see_everything(), &picks));
        assert!(ack.contains(STAGING_LOOKING_TO_PLAY_CHANNEL_ID));
        assert!(ack.starts_with("On it - head to "));
    }

    #[test]
    fn session_outcome_records_routed_only_with_destinations() {
        let picks = catalog();
        let hit = adjudicate_session_select(&["join-voice"], &see_everything(), &picks);
        assert!(hit.ephemeral);
        assert!(hit.record_routed);
        assert_eq!(hit.routed.picks, vec!["join-voice".to_owned()]);
        let miss = adjudicate_session_select(&["gone-option"], &see_everything(), &picks);
        assert!(!miss.record_routed, "nothing routed, nothing recorded");
        assert!(miss.reply.contains("stale"));
    }

    #[test]
    fn session_welcome_is_a_destination_not_a_label() {
        let text = session_welcome_text(MEMBER);
        assert!(text.contains("that was the whole application"));
        assert!(text.contains("not a label forever"));
    }

    // --- anchor recurrence ------------------------------------------------------------------

    #[test]
    fn civil_date_math_round_trips_on_independent_pins() {
        // Epoch-day counts and Sunday=0 weekdays from Python datetime.
        for (y, m, d, days, wd) in [
            (2026, 8, 23, 20_688, 0),
            (2026, 8, 24, 20_689, 1),
            (2026, 10, 25, 20_751, 0),
            (2026, 11, 1, 20_758, 0),
            (2027, 3, 14, 20_891, 0),
        ] {
            assert_eq!(days_from_civil(y, m, d), days);
            assert_eq!(civil_from_days(days), (y, m, d));
            assert_eq!(weekday_of_days(days), wd);
        }
        assert_eq!(nth_weekday_of_month(2026, 3, 0, 2), 8);
        assert_eq!(nth_weekday_of_month(2026, 11, 0, 1), 1);
        assert_eq!(nth_weekday_of_month(2027, 3, 0, 2), 14);
        assert_eq!(nth_weekday_of_month(2027, 11, 0, 1), 7);
    }

    #[test]
    fn first_six_occurrences_are_the_ones_the_spec_names() {
        let mut cursor = BEFORE_RUN_1;
        for expected in [
            1_787_529_600,
            1_788_134_400,
            1_788_739_200,
            1_789_344_000,
            1_789_948_800,
            1_790_553_600,
        ] {
            let next = next_anchor_occurrence(cursor, SUNDAY_SQUAD);
            assert_eq!(next, expected);
            cursor = next;
        }
    }

    #[test]
    fn dst_weeks_are_169_and_167_hours_not_168() {
        // Fall-back 2026: Oct 25 EDT → Nov 1 EST.
        assert_eq!(zoned_epoch_secs(2026, 10, 25, 20, 0), 1_792_972_800);
        assert_eq!(zoned_epoch_secs(2026, 11, 1, 20, 0), 1_793_581_200);
        assert_eq!(1_793_581_200 - 1_792_972_800, 608_400);
        // Spring-forward 2027: Mar 7 EST → Mar 14 EDT.
        assert_eq!(zoned_epoch_secs(2027, 3, 7, 20, 0), 1_804_467_600);
        assert_eq!(zoned_epoch_secs(2027, 3, 14, 20, 0), 1_805_068_800);
        assert_eq!(1_805_068_800 - 1_804_467_600, 601_200);
    }

    #[test]
    fn every_occurrence_is_2000_local_for_a_year_of_sundays() {
        let mut cursor = 1_759_824_000; // 2026-10-01T00:00Z.
        for _ in 0..30 {
            let next = next_anchor_occurrence(cursor, SUNDAY_SQUAD);
            // Sunday 20:00 NY lands Monday 00:00Z (EDT) or 01:00Z (EST).
            let tod = next.rem_euclid(86_400);
            assert!(tod == 0 || tod == 3600, "{next} is not 20:00 New York time");
            assert_eq!(
                weekday_of_days(next.div_euclid(86_400)),
                1,
                "Mondays in UTC"
            );
            cursor = next;
        }
    }

    #[test]
    fn sunday_evening_names_today_not_next_week() {
        // Sunday 23 Aug 2026 19:00 local = 23:00Z: tonight's 20:00 is next.
        assert_eq!(next_anchor_occurrence(1_787_525_200, SUNDAY_SQUAD), RUN_1);
        // Monday 24 Aug 00:30Z (Sunday 20:30 EDT, event live): next is Aug 30.
        assert_eq!(
            next_anchor_occurrence(RUN_1 + 1800, SUNDAY_SQUAD),
            1_788_134_400
        );
    }

    #[test]
    fn occurrence_context_names_the_live_event_not_next_week() {
        // Twenty minutes before run 1: near, not live.
        let ctx = occurrence_context(RUN_1 - 1200, SUNDAY_SQUAD);
        assert_eq!((ctx.start_epoch, ctx.near, ctx.live), (RUN_1, true, false));
        // Half an hour in: the running event is named.
        let ctx = occurrence_context(RUN_1 + 1800, SUNDAY_SQUAD);
        assert_eq!((ctx.start_epoch, ctx.near, ctx.live), (RUN_1, true, true));
        // Saturday noon: far voice with a relative timestamp.
        let ctx = occurrence_context(BEFORE_RUN_1, SUNDAY_SQUAD);
        assert_eq!((ctx.start_epoch, ctx.near, ctx.live), (RUN_1, false, false));
    }

    #[test]
    fn anchor_copy_has_two_voices_and_no_attachments() {
        let far = anchor_welcome_text(MEMBER, BEFORE_RUN_1, SUNDAY_SQUAD);
        assert!(far.starts_with("Hey <@900000000000007777> — glad you're here."));
        assert!(far.contains("every Sunday at 8pm Eastern"));
        assert!(far.contains("<t:1787529600:R>"));
        assert!(far.contains("just join the voice room"));
        let near = anchor_welcome_text(MEMBER, RUN_1 - 1200, SUNDAY_SQUAD);
        assert!(near.contains("happening right now"));
        assert!(!near.contains("<t:"), "near copy names no timestamp");
        let live = anchor_welcome_text(MEMBER, RUN_1 + 1800, SUNDAY_SQUAD);
        assert!(live.contains("happening right now"));
    }

    // --- welcome + goodbye ---------------------------------------------------------------------

    #[test]
    fn welcome_posts_picker_per_mode_and_skips_without_a_channel() {
        match adjudicate_welcome(
            OnboardingMode::Legacy,
            MEMBER,
            Some("111"),
            ANCHOR_CHANNEL_ID,
            BEFORE_RUN_1,
            SUNDAY_SQUAD,
            false,
        ) {
            WelcomeEffect::Post {
                channel_id,
                picker,
                mention_user_id,
                ..
            } => {
                assert_eq!(channel_id, "111");
                assert_eq!(picker, Some(PickerKind::Games));
                assert_eq!(mention_user_id, MEMBER);
            }
            WelcomeEffect::Skip { .. } => panic!("legacy with a channel posts"),
        }
        match adjudicate_welcome(
            OnboardingMode::Session,
            MEMBER,
            Some("222"),
            ANCHOR_CHANNEL_ID,
            BEFORE_RUN_1,
            SUNDAY_SQUAD,
            false,
        ) {
            WelcomeEffect::Post {
                picker, content, ..
            } => {
                assert_eq!(picker, Some(PickerKind::Session));
                assert!(content.contains("whole application"));
            }
            WelcomeEffect::Skip { .. } => panic!("session with a channel posts"),
        }
        // Anchor posts the one message with no picker, channel config aside.
        match adjudicate_welcome(
            OnboardingMode::Anchor,
            MEMBER,
            None,
            ANCHOR_CHANNEL_ID,
            BEFORE_RUN_1,
            SUNDAY_SQUAD,
            false,
        ) {
            WelcomeEffect::Post {
                channel_id,
                picker,
                content,
                ..
            } => {
                assert_eq!(channel_id, ANCHOR_CHANNEL_ID);
                assert_eq!(picker, None);
                assert!(content.contains("Sunday Squad"));
            }
            WelcomeEffect::Skip { .. } => panic!("anchor always posts"),
        }
        assert!(matches!(
            adjudicate_welcome(
                OnboardingMode::Legacy,
                MEMBER,
                None,
                ANCHOR_CHANNEL_ID,
                BEFORE_RUN_1,
                SUNDAY_SQUAD,
                false
            ),
            WelcomeEffect::Skip { .. }
        ));
        for mode in [OnboardingMode::Legacy, OnboardingMode::Anchor] {
            assert!(matches!(
                adjudicate_welcome(
                    mode,
                    MEMBER,
                    Some("111"),
                    ANCHOR_CHANNEL_ID,
                    BEFORE_RUN_1,
                    SUNDAY_SQUAD,
                    true
                ),
                WelcomeEffect::Skip { reason: "dry_run" }
            ));
        }
        // Session's roleless welcome still posts in dry run, like legacy.
        assert!(matches!(
            adjudicate_welcome(
                OnboardingMode::Session,
                MEMBER,
                Some("111"),
                ANCHOR_CHANNEL_ID,
                BEFORE_RUN_1,
                SUNDAY_SQUAD,
                true
            ),
            WelcomeEffect::Post {
                picker: Some(PickerKind::Session),
                ..
            }
        ));
        assert!(matches!(
            adjudicate_welcome(
                OnboardingMode::Session,
                MEMBER,
                None,
                ANCHOR_CHANNEL_ID,
                BEFORE_RUN_1,
                SUNDAY_SQUAD,
                true
            ),
            WelcomeEffect::Skip {
                reason: "no_landing_channel"
            }
        ));
    }

    #[test]
    fn goodbye_names_the_leaver_states_the_stay_and_never_pings() {
        assert_eq!(
            goodbye_text("dave", Some(3)),
            "**dave** left the server (was here 3 days). Their messages and voice history stay on the books."
        );
        assert_eq!(
            goodbye_text("sam", Some(1)),
            "**sam** left the server (was here 1 day). Their messages and voice history stay on the books."
        );
        assert_eq!(
            goodbye_text("kim", Some(0)),
            "**kim** left the server (was here less than a day). Their messages and voice history stay on the books."
        );
        assert_eq!(
            goodbye_text("lee", None),
            "**lee** left the server. Their messages and voice history stay on the books."
        );
        assert!(!goodbye_text("dave", Some(3)).contains("<@"));
        assert_eq!(
            days_in_guild(Some(1_787_529_600_000), Some(1_787_529_600_000)),
            Some(0)
        );
        assert_eq!(
            days_in_guild(
                Some(1_787_529_600_000),
                Some(1_787_529_600_000 + 3 * 86_400_000)
            ),
            Some(3)
        );
        assert_eq!(days_in_guild(None, Some(1_787_529_600_000)), None);
    }

    #[test]
    fn goodbye_is_session_only_and_channel_gated() {
        assert_eq!(
            adjudicate_goodbye(OnboardingMode::Legacy, "dave", Some(3), Some("999"), false),
            None,
            "only session mode owns GuildMemberRemove"
        );
        assert_eq!(
            adjudicate_goodbye(OnboardingMode::Anchor, "dave", Some(3), Some("999"), false),
            None
        );
        match adjudicate_goodbye(OnboardingMode::Session, "dave", Some(3), Some("999"), false) {
            Some(GoodbyeEffect::Post {
                channel_id,
                content,
                mentions,
            }) => {
                assert_eq!(mentions, MentionPolicy::None);
                assert_eq!(channel_id, "999");
                assert!(content.starts_with("**dave** left the server (was here 3 days)."));
            }
            _ => panic!("session with a channel posts the goodbye"),
        }
        // GoodbyeEffect::Post uses explicit MentionPolicy::None — the
        // executor renders allowedMentions { parse: [] }.
        assert!(matches!(
            adjudicate_goodbye(OnboardingMode::Session, "dave", None, None, false),
            Some(GoodbyeEffect::Skip { .. })
        ));
        assert!(matches!(
            adjudicate_goodbye(OnboardingMode::Session, "dave", None, Some("999"), true),
            Some(GoodbyeEffect::Skip { reason: "dry_run" })
        ));
    }

    // --- funnel rows ------------------------------------------------------------------------------

    #[test]
    fn funnel_keys_match_legacy_idempotency_shapes() {
        let prompted = prompted_row("1", "2", "3", "2026-08-24T00:00:00.000Z");
        assert_eq!(prompted.source, "channel:3");
        assert_eq!(prompted.metadata, None);
        assert_eq!(funnel_idempotency_key(&prompted), "1:2:onboarding_prompted");
        // Same member, later time: still the same key (once-per-member).
        let prompted_again = prompted_row("1", "2", "9", "2026-08-25T00:00:00.000Z");
        assert_eq!(
            funnel_idempotency_key(&prompted_again),
            funnel_idempotency_key(&prompted)
        );
        let selected = game_selected_row(
            "1",
            "2",
            &["shooters".to_owned(), "horror".to_owned()],
            "2026-08-24T00:00:42.000Z",
        );
        assert_eq!(selected.source, SOURCE_PICKER);
        assert_eq!(
            selected.metadata.as_deref(),
            Some(r#"{"picks":["shooters","horror"]}"#)
        );
        assert_eq!(
            funnel_idempotency_key(&selected),
            "1:2:game_roles_selected:2026-08-24T00:00:42.000Z"
        );
        let routed = channel_routed_row(
            "1",
            "2",
            &[GAME_HUB_CHANNEL_ID.to_owned()],
            1,
            &[],
            "2026-08-24T00:00:42.000Z",
        );
        let routed_meta: serde_json::Value =
            serde_json::from_str(routed.metadata.as_deref().expect("metadata"))
                .expect("valid json");
        assert_eq!(
            routed_meta,
            serde_json::json!({ "channels": [GAME_HUB_CHANNEL_ID], "degraded": 1 })
        );
        assert_eq!(
            funnel_idempotency_key(&routed),
            "1:2:channel_routed:2026-08-24T00:00:42.000Z"
        );
    }

    #[test]
    fn routed_row_names_unavailable_picks_only_when_a_pick_has_no_route() {
        let shooters = pick_by_key("shooters").unwrap();
        let primary = shooters.primary_channel_id.unwrap();
        // Mixed: shooters keeps its room, the hub-only pick has none.
        let mixed = plan_game_selection(&["shooters", "rocketleague"], &|id| id == primary);
        assert_eq!(mixed.unavailable_keys(), vec!["rocketleague"]);
        let row = channel_routed_row(
            "1",
            "2",
            &mixed.channel_ids,
            mixed.degraded_count,
            &mixed.unavailable_keys(),
            "2026-08-24T00:00:42.000Z",
        );
        let meta: serde_json::Value =
            serde_json::from_str(row.metadata.as_deref().unwrap()).unwrap();
        assert_eq!(
            meta,
            serde_json::json!({
                "channels": [primary],
                "degraded": 0,
                "unavailable": ["rocketleague"],
            })
        );
        // Fully routed rows keep the legacy two-key shape.
        let routed = plan_game_selection(&["shooters", "rocketleague"], &see_everything());
        assert!(routed.unavailable_keys().is_empty());
        let row = channel_routed_row(
            "1",
            "2",
            &routed.channel_ids,
            routed.degraded_count,
            &routed.unavailable_keys(),
            "2026-08-24T00:00:42.000Z",
        );
        let meta: serde_json::Value =
            serde_json::from_str(row.metadata.as_deref().unwrap()).unwrap();
        assert!(meta.get("unavailable").is_none(), "{meta}");
    }

    #[test]
    fn session_routed_row_carries_no_roles_and_names_its_source() {
        let picks = catalog();
        let plan = plan_session(&["find-players"], &see_everything(), &picks);
        let row = session_routed_row("1", "2", &plan, "2026-08-24T00:00:42.000Z");
        assert_eq!(row.source, SOURCE_SESSION_PICKER);
        // Compare as JSON values: serde_json::json! key order is an
        // implementation detail, key presence is the contract.
        let meta: serde_json::Value =
            serde_json::from_str(row.metadata.as_deref().expect("metadata")).expect("valid json");
        assert_eq!(
            meta,
            serde_json::json!({
                "picks": ["find-players"],
                "channels": [STAGING_LOOKING_TO_PLAY_CHANNEL_ID],
                "unavailable": [],
            })
        );
        // There is deliberately no game-selected builder for session mode:
        // nothing here can emit a row with an empty picks list.
    }

    #[test]
    fn select_ids_are_static_and_distinct() {
        assert_eq!(GAME_SELECT_ID, "two:onboarding:games");
        assert_eq!(SESSION_SELECT_ID, "two:onboarding:session");
        assert_ne!(GAME_SELECT_ID, SESSION_SELECT_ID);
    }
}
