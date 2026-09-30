//! Self-role panels and reaction roles with claim leases.
//!
//! Ports `src/selfRoles/` (`config.ts`, `permissions.ts`, `plan.ts`,
//! `stagingFence.ts`) plus the pure validation/planning core of
//! `src/discord/selfRoles.ts` from legacy two-bot (frozen `main`), as
//! framework-free data plus pure functions in the style of `leveling.rs` /
//! `moderation.rs`. The interaction router (TOG-10075) and the REST action
//! executor (TOG-10076) are not merged yet, so this module stops at outcome
//! enums and reply text: the follow-up wires these plans into the router and
//! emits the Discord mutations through the executor. No private dispatcher
//! or HTTP client lives here.
//!
//! Covered behaviour (parity §2, §4, §5):
//! - panel catalogue load + validation (`TWO_SELF_ROLE_PANELS`)
//! - button/select/reaction change planning: claimed add/remove/replace,
//!   exclusive (incl. color) switch semantics, duplicate deliveries as
//!   no-ops (`already_held` / `already_absent`)
//! - dispatch validation: bot `ManageRoles`, role existence, the explicit
//!   permission allowlist, channel-overwrite safety, deployment-mask drift,
//!   color-panel color, and role-hierarchy refusals
//! - claim-lease arithmetic: 5-minute lease, renew at a third of the lease,
//!   ownership as a pure comparison (the renewal timers live in the
//!   executor follow-up)
//! - audit vocabulary (`self_role_audit` outcomes) and the panel-claim
//!   target vocabulary (`self_role_panel_claims`) as data; the sqlx store
//!   module lives next to the `0200` migration in `two-bot-cutover`.
//!
//! Source files (legacy `two-bot`):
//! - `src/selfRoles/types.ts` — panel/option/audit shapes
//! - `src/selfRoles/plan.ts` — `planSelfRoleChange`, custom ids, emoji keys
//! - `src/selfRoles/permissions.ts` — allowlist + channel safety
//! - `src/selfRoles/config.ts` — catalogue load + live-role resolution
//! - `src/discord/selfRoles.ts` — `validateSelfRoleDispatch`, reply text,
//!   event ordering, lease constants
//!
//! Staging gate: panels serve only while the staging allowlist fence passes
//! (enforced by the boot adapter, same posture as `moderation.rs`); this
//! module carries no guild id.

use std::collections::{HashMap, HashSet};

/// Claim-lease duration (legacy `SELF_ROLE_CLAIM_LEASE_MS` = 5 minutes).
pub const SELF_ROLE_CLAIM_LEASE_MS: u64 = 5 * 60 * 1000;
/// Discord epoch for snowflake timestamps (legacy `DISCORD_EPOCH_MS`).
pub const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// A self-role panel surface (legacy `SelfRolePanelMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelMode {
    Button,
    Select,
    Reaction,
}

impl PanelMode {
    /// Wire/config name (`button`, `select`, `reaction`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Button => "button",
            Self::Select => "select",
            Self::Reaction => "reaction",
        }
    }

    /// Parse a wire/config name; `None` for anything else.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "button" => Some(Self::Button),
            "select" => Some(Self::Select),
            "reaction" => Some(Self::Reaction),
            _ => None,
        }
    }
}

impl std::fmt::Display for PanelMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One pickable role (legacy `SelfRoleOption`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfRoleOption {
    /// Stable, panel-scoped key used in custom ids and audit rows.
    pub key: String,
    pub label: String,
    pub role_id: String,
    /// Exact Discord permission mask approved for this role at deployment.
    pub permissions: String,
    pub emoji: Option<String>,
    pub description: Option<String>,
}

/// One configured panel (legacy `SelfRolePanel`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfRolePanel {
    /// Stable audit/config key.
    pub id: String,
    pub channel_id: String,
    pub message_id: String,
    pub mode: PanelMode,
    /// At most one role from this panel may be held after a selection.
    pub exclusive: bool,
    /// Color-Chan semantics: an exclusive set of visibly colored roles.
    pub color: bool,
    pub options: Vec<SelfRoleOption>,
}

/// Discord mutation vocabulary (legacy `operation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoleOperation {
    Add,
    Remove,
    Replace,
}

impl RoleOperation {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Replace => "replace",
        }
    }
}

/// Settled audit outcome for a dispatch (legacy `SelfRoleAuditOutcome`
/// minus `processing`, which is a store state, not a plan result).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettledOutcome {
    Assigned,
    Removed,
    Switched,
    AlreadyHeld,
    AlreadyAbsent,
    Rejected,
}

impl SettledOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assigned => "assigned",
            Self::Removed => "removed",
            Self::Switched => "switched",
            Self::AlreadyHeld => "already_held",
            Self::AlreadyAbsent => "already_absent",
            Self::Rejected => "rejected",
        }
    }
}

/// A successful change plan (legacy `SelfRolePlan` ok-branch).
///
/// `option_key` / `role_id` are the audited option (`None` only for a
/// select deselect-all, which names no option — legacy audits `null`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfRolePlan {
    pub option_key: Option<String>,
    pub role_id: Option<String>,
    pub operation: RoleOperation,
    pub add_role_ids: Vec<String>,
    pub remove_role_ids: Vec<String>,
    pub outcome: SettledOutcome,
}

/// A rejected change plan (legacy `SelfRolePlan` error branch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanRejection {
    UnknownOption {
        panel_id: String,
        option_key: String,
    },
    WrongSource {
        panel_id: String,
        expected: PanelMode,
        received: PanelMode,
    },
}

impl PlanRejection {
    /// Stable audit code (legacy `code`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownOption { .. } => "unknown_option",
            Self::WrongSource { .. } => "wrong_source",
        }
    }

    /// Human reason (legacy `reason`).
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::UnknownOption {
                panel_id,
                option_key,
            } => {
                format!("panel {panel_id} has no option {option_key}")
            }
            Self::WrongSource {
                panel_id,
                expected,
                received,
            } => format!("panel {panel_id} expects {expected}, received {received}"),
        }
    }
}

impl std::fmt::Display for PlanRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason())
    }
}

/// Plan one button/reaction toggle or explicit removal (legacy
/// `planSelfRoleChange`).
///
/// - `remove == true` plans an explicit removal (reaction-remove path).
/// - `remove == false` on a button plans a claimed add; on an exclusive
///   panel it plans a replace that drops the other held panel roles.
/// - Already-held / already-absent inputs plan empty mutations so duplicate
///   gateway deliveries are idempotent no-ops.
pub fn plan_self_role_change(
    panel: &SelfRolePanel,
    option_key: &str,
    member_role_ids: &HashSet<String>,
    source: PanelMode,
    remove: bool,
) -> Result<SelfRolePlan, PlanRejection> {
    if source != panel.mode {
        return Err(PlanRejection::WrongSource {
            panel_id: panel.id.clone(),
            expected: panel.mode,
            received: source,
        });
    }
    let option = panel
        .options
        .iter()
        .find(|o| o.key == option_key)
        .cloned()
        .ok_or(PlanRejection::UnknownOption {
            panel_id: panel.id.clone(),
            option_key: option_key.to_owned(),
        })?;

    if remove {
        let held = member_role_ids.contains(&option.role_id);
        return Ok(SelfRolePlan {
            option_key: Some(option.key.clone()),
            role_id: Some(option.role_id.clone()),
            operation: RoleOperation::Remove,
            add_role_ids: vec![],
            remove_role_ids: held.then(|| option.role_id.clone()).into_iter().collect(),
            outcome: if held {
                SettledOutcome::Removed
            } else {
                SettledOutcome::AlreadyAbsent
            },
        });
    }

    if !panel.exclusive {
        let held = member_role_ids.contains(&option.role_id);
        return Ok(SelfRolePlan {
            option_key: Some(option.key.clone()),
            role_id: Some(option.role_id.clone()),
            operation: RoleOperation::Add,
            add_role_ids: (!held)
                .then(|| option.role_id.clone())
                .into_iter()
                .collect(),
            remove_role_ids: vec![],
            outcome: if held {
                SettledOutcome::AlreadyHeld
            } else {
                SettledOutcome::Assigned
            },
        });
    }

    let remove_role_ids: Vec<String> = panel
        .options
        .iter()
        .map(|o| o.role_id.clone())
        .filter(|role_id| *role_id != option.role_id && member_role_ids.contains(role_id))
        .collect();
    let add_role_ids: Vec<String> = (!member_role_ids.contains(&option.role_id))
        .then(|| option.role_id.clone())
        .into_iter()
        .collect();
    let outcome = if !remove_role_ids.is_empty() {
        SettledOutcome::Switched
    } else if !add_role_ids.is_empty() {
        SettledOutcome::Assigned
    } else {
        SettledOutcome::AlreadyHeld
    };
    Ok(SelfRolePlan {
        option_key: Some(option.key.clone()),
        role_id: Some(option.role_id.clone()),
        operation: RoleOperation::Replace,
        add_role_ids,
        remove_role_ids,
        outcome,
    })
}

/// Plan a select-menu submission against the authoritative held set (legacy
/// `recomputeDelta` desired-role path): the submission names the exact
/// desired panel roles, and the plan is the symmetric difference.
pub fn plan_select_delta(
    panel: &SelfRolePanel,
    held_role_ids: &HashSet<String>,
    desired_role_ids: &[String],
) -> Result<SelfRolePlan, PlanRejection> {
    let offered: HashSet<&str> = panel.options.iter().map(|o| o.role_id.as_str()).collect();
    let desired: HashSet<&str> = desired_role_ids.iter().map(String::as_str).collect();
    let valid =
        desired.iter().all(|id| offered.contains(id)) && (!panel.exclusive || desired.len() <= 1);
    if !valid {
        return Err(PlanRejection::UnknownOption {
            panel_id: panel.id.clone(),
            option_key: desired_role_ids.first().cloned().unwrap_or_default(),
        });
    }
    let current: HashSet<&str> = held_role_ids
        .iter()
        .map(String::as_str)
        .filter(|id| offered.contains(id))
        .collect();
    // Keep catalogue order so repeated plans have stable mutation ordering.
    let add_role_ids: Vec<String> = panel
        .options
        .iter()
        .filter(|o| desired.contains(o.role_id.as_str()) && !current.contains(o.role_id.as_str()))
        .map(|o| o.role_id.clone())
        .collect();
    let remove_role_ids: Vec<String> = panel
        .options
        .iter()
        .filter(|o| current.contains(o.role_id.as_str()) && !desired.contains(o.role_id.as_str()))
        .map(|o| o.role_id.clone())
        .collect();
    let operation = if panel.exclusive || (!add_role_ids.is_empty() && !remove_role_ids.is_empty())
    {
        RoleOperation::Replace
    } else if !add_role_ids.is_empty() {
        RoleOperation::Add
    } else {
        RoleOperation::Remove
    };
    let outcome = if !add_role_ids.is_empty() && !remove_role_ids.is_empty() {
        SettledOutcome::Switched
    } else if !add_role_ids.is_empty() {
        SettledOutcome::Assigned
    } else if !remove_role_ids.is_empty() {
        SettledOutcome::Removed
    } else if desired.is_empty() {
        SettledOutcome::AlreadyAbsent
    } else {
        SettledOutcome::AlreadyHeld
    };
    // Select rows audit under the first desired option when any is chosen
    // (legacy audits `optionKey: values[0] ?? null`); deselect-all audits
    // a null option.
    let (option_key, role_id) = match desired_role_ids.first() {
        Some(first) => {
            let key = panel
                .options
                .iter()
                .find(|o| &o.role_id == first)
                .map(|o| o.key.clone());
            (key, Some(first.clone()))
        }
        None => (None, None),
    };
    Ok(SelfRolePlan {
        option_key,
        role_id,
        operation,
        add_role_ids,
        remove_role_ids,
        outcome,
    })
}

/// Component id for a panel (`option_key == None` is the select-menu id).
/// Component ids are Discord-signed but still parsed as untrusted input.
#[must_use]
pub fn self_role_custom_id(panel_id: &str, option_key: Option<&str>) -> String {
    match option_key {
        Some(key) => format!("two:self-role:{panel_id}:{key}"),
        None => format!("two:self-role:{panel_id}"),
    }
}

/// Parsed component id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCustomId {
    pub panel_id: String,
    pub option_key: Option<String>,
}

fn valid_key(value: &str) -> bool {
    !value.is_empty() && value.len() <= 60 && {
        let mut chars = value.bytes();
        matches!(chars.next(), Some(b'a'..=b'z' | b'0'..=b'9'))
            && chars.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
    }
}

/// Parse a component id; `None` for anything outside the panel namespace.
#[must_use]
pub fn parse_self_role_custom_id(custom_id: &str) -> Option<ParsedCustomId> {
    let rest = custom_id.strip_prefix("two:self-role:")?;
    let mut parts = rest.split(':');
    let panel_id = parts.next()?;
    let option_key = parts.next();
    if parts.next().is_some() || !valid_key(panel_id) {
        return None;
    }
    if let Some(key) = option_key {
        if !valid_key(key) {
            return None;
        }
    }
    Some(ParsedCustomId {
        panel_id: panel_id.to_owned(),
        option_key: option_key.map(str::to_owned),
    })
}

/// Map a reaction emoji to its panel option key (legacy `reactionOptionKey`).
/// `emoji_id` is the custom-emoji snowflake, `emoji_name` the unicode glyph.
#[must_use]
pub fn reaction_option_key(
    panel: &SelfRolePanel,
    emoji_id: Option<&str>,
    emoji_name: Option<&str>,
) -> Option<String> {
    let key = emoji_id.or(emoji_name)?;
    if key.is_empty() {
        return None;
    }
    panel
        .options
        .iter()
        .find(|o| o.emoji.as_deref().is_some_and(|e| emoji_identity(e) == key))
        .map(|o| o.key.clone())
}

/// Unicode stays unchanged; Discord custom-emoji mentions compare by
/// snowflake (legacy `emojiIdentity`).
#[must_use]
pub fn emoji_identity(value: &str) -> &str {
    custom_emoji_parts(value).map_or(value, |(_, id)| id)
}

/// Discord's reaction endpoint wants `name:id` rather than the component
/// mention (legacy `reactionEndpointEmoji`).
#[must_use]
pub fn reaction_endpoint_emoji(value: &str) -> String {
    match custom_emoji_parts(value) {
        Some((name, id)) => format!("{name}:{id}"),
        None => value.to_owned(),
    }
}

/// Split `<:name:id>` / `<a:name:id>` into `(name, id)` (legacy
/// `/^<a?:[^:>]+:(\d{17,20})>$/`); `None` for unicode glyphs and malformed
/// mentions.
fn custom_emoji_parts(value: &str) -> Option<(&str, &str)> {
    let mut inner = value.strip_prefix('<')?.strip_suffix('>')?;
    // Animated marker: consume the `a` only when `<a:` opens the mention.
    if let Some(rest) = inner.strip_prefix('a') {
        if rest.starts_with(':') {
            inner = rest;
        }
    }
    let rest = inner.strip_prefix(':')?;
    let (name, id) = rest.split_once(':')?;
    if name.is_empty() || name.contains(':') || !is_snowflake(id) {
        return None;
    }
    Some((name, id))
}

/// True for a Discord snowflake (legacy `/^\d{17,20}$/`).
#[must_use]
pub fn is_snowflake(value: &str) -> bool {
    (17..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

/// Discord permission bits a member-facing role may carry, as `(name, bit)`
/// in ascending bit order. Everything else is rejected, including bits
/// Discord adds after this ships; expanding the list is an explicit
/// security decision (legacy `SELF_ROLE_ALLOWED_PERMISSIONS`).
///
/// Names follow the legacy discord.js vocabulary.
pub const SELF_ROLE_ALLOWED_PERMISSIONS: &[(&str, u64)] = &[
    ("AddReactions", 1 << 6),
    ("Stream", 1 << 9),
    ("ViewChannel", 1 << 10),
    ("SendMessages", 1 << 11),
    ("EmbedLinks", 1 << 14),
    ("AttachFiles", 1 << 15),
    ("ReadMessageHistory", 1 << 16),
    ("UseExternalEmojis", 1 << 18),
    ("Connect", 1 << 20),
    ("Speak", 1 << 21),
    ("ChangeNickname", 1 << 26),
    ("UseApplicationCommands", 1 << 31),
];

/// Every known Discord permission bit, ascending, for naming a disallowed
/// mask (legacy `KNOWN_PERMISSION_BITS`).
const KNOWN_PERMISSION_BITS: &[(&str, u64)] = &[
    ("CreateInstantInvite", 1),
    ("KickMembers", 1 << 1),
    ("BanMembers", 1 << 2),
    ("Administrator", 1 << 3),
    ("ManageChannels", 1 << 4),
    ("ManageGuild", 1 << 5),
    ("AddReactions", 1 << 6),
    ("ViewAuditLog", 1 << 7),
    ("PrioritySpeaker", 1 << 8),
    ("Stream", 1 << 9),
    ("ViewChannel", 1 << 10),
    ("SendMessages", 1 << 11),
    ("SendTTSMessages", 1 << 12),
    ("ManageMessages", 1 << 13),
    ("EmbedLinks", 1 << 14),
    ("AttachFiles", 1 << 15),
    ("ReadMessageHistory", 1 << 16),
    ("MentionEveryone", 1 << 17),
    ("UseExternalEmojis", 1 << 18),
    ("ViewGuildInsights", 1 << 19),
    ("Connect", 1 << 20),
    ("Speak", 1 << 21),
    ("MuteMembers", 1 << 22),
    ("DeafenMembers", 1 << 23),
    ("MoveMembers", 1 << 24),
    ("UseVAD", 1 << 25),
    ("ChangeNickname", 1 << 26),
    ("ManageNicknames", 1 << 27),
    ("ManageRoles", 1 << 28),
    ("ManageWebhooks", 1 << 29),
    ("ManageGuildExpressions", 1 << 30),
    ("ManageEmojisAndStickers", 1 << 30),
    ("UseApplicationCommands", 1 << 31),
    ("RequestToSpeak", 1 << 32),
    ("ManageEvents", 1 << 33),
    ("ManageThreads", 1 << 34),
    ("CreatePublicThreads", 1 << 35),
    ("CreatePrivateThreads", 1 << 36),
    ("UseExternalStickers", 1 << 37),
    ("SendMessagesInThreads", 1 << 38),
    ("UseEmbeddedActivities", 1 << 39),
    ("ModerateMembers", 1 << 40),
    ("ViewCreatorMonetizationAnalytics", 1 << 41),
    ("UseSoundboard", 1 << 42),
    ("CreateGuildExpressions", 1 << 43),
    ("CreateEvents", 1 << 44),
    ("UseExternalSounds", 1 << 45),
    ("SendVoiceMessages", 1 << 46),
    ("SendPolls", 1 << 49),
    ("UseExternalApps", 1 << 50),
    ("PinMessages", 1 << 51),
    ("BypassSlowmode", 1 << 52),
];

/// Allowed-mask union of [`SELF_ROLE_ALLOWED_PERMISSIONS`].
pub const SELF_ROLE_ALLOWED_MASK: u64 = (1 << 6)
    | (1 << 9)
    | (1 << 10)
    | (1 << 11)
    | (1 << 14)
    | (1 << 15)
    | (1 << 16)
    | (1 << 18)
    | (1 << 20)
    | (1 << 21)
    | (1 << 26)
    | (1 << 31);

/// Name the first disallowed bit in a mask, or `None` when the mask is
/// self-service-safe (legacy `findSelfRoleDisallowedPermission`).
#[must_use]
pub fn find_disallowed_permission(bits: u64) -> Option<String> {
    let disallowed = bits & !SELF_ROLE_ALLOWED_MASK;
    if disallowed == 0 {
        return None;
    }
    for (name, bit) in KNOWN_PERMISSION_BITS {
        if disallowed & bit == *bit {
            return Some((*name).to_owned());
        }
    }
    Some(format!("unknown permission bits {disallowed}"))
}

/// One role overwrite on a channel (legacy `SelfRoleChannelOverwrite`;
/// `kind == 0` is a role overwrite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelOverwrite {
    pub id: String,
    pub kind: u8,
    pub allow: u64,
    pub deny: u64,
}

/// Channel snapshot for the safety check (legacy
/// `SelfRoleChannelPermissions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSnapshot {
    pub id: String,
    pub name: Option<String>,
    pub overwrites: Vec<ChannelOverwrite>,
}

/// Why a channel grant is unsafe (legacy `kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsafeGrantKind {
    UnsafePermission,
    NewChannelAccess,
}

impl UnsafeGrantKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsafePermission => "unsafe_permission",
            Self::NewChannelAccess => "new_channel_access",
        }
    }
}

/// An unsafe channel grant (legacy `findSelfRoleUnsafeChannelGrant`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsafeGrant {
    pub channel_id: String,
    pub channel_name: Option<String>,
    pub permission: String,
    pub kind: UnsafeGrantKind,
}

/// A self-service role may not unlock a channel hidden from @everyone:
/// compare the member holding only @everyone with the same member after
/// this one role is added (legacy `findSelfRoleUnsafeChannelGrant`).
#[must_use]
pub fn find_unsafe_channel_grant(
    guild_id: &str,
    role_id: &str,
    everyone_permissions: u64,
    role_permissions: u64,
    channels: &[ChannelSnapshot],
) -> Option<UnsafeGrant> {
    const VIEW_CHANNEL: u64 = 1 << 10;
    const ADMINISTRATOR: u64 = 1 << 3;
    for channel in channels {
        if let Some(overwrite) = channel
            .overwrites
            .iter()
            .find(|o| o.kind == 0 && o.id == role_id)
        {
            if let Some(disallowed) = find_disallowed_permission(overwrite.allow) {
                return Some(UnsafeGrant {
                    channel_id: channel.id.clone(),
                    channel_name: channel.name.clone(),
                    permission: disallowed,
                    kind: UnsafeGrantKind::UnsafePermission,
                });
            }
        }
        let before = effective_permissions(
            guild_id,
            everyone_permissions,
            &channel.overwrites,
            &[],
            ADMINISTRATOR,
        );
        let after = effective_permissions(
            guild_id,
            everyone_permissions | role_permissions,
            &channel.overwrites,
            &[role_id],
            ADMINISTRATOR,
        );
        if before & VIEW_CHANNEL == 0 && after & VIEW_CHANNEL != 0 {
            return Some(UnsafeGrant {
                channel_id: channel.id.clone(),
                channel_name: channel.name.clone(),
                permission: "ViewChannel".to_owned(),
                kind: UnsafeGrantKind::NewChannelAccess,
            });
        }
    }
    None
}

fn effective_permissions(
    guild_id: &str,
    base: u64,
    overwrites: &[ChannelOverwrite],
    held_role_ids: &[&str],
    administrator: u64,
) -> u64 {
    if base & administrator != 0 {
        return u64::MAX;
    }
    let mut base = base;
    if let Some(everyone) = overwrites.iter().find(|o| o.kind == 0 && o.id == guild_id) {
        base = (base & !everyone.deny) | everyone.allow;
    }
    let held: HashSet<&str> = held_role_ids.iter().copied().collect();
    let mut deny = 0u64;
    let mut allow = 0u64;
    for overwrite in overwrites {
        if overwrite.kind != 0 || overwrite.id == guild_id || !held.contains(overwrite.id.as_str())
        {
            continue;
        }
        deny |= overwrite.deny;
        allow |= overwrite.allow;
    }
    (base & !deny) | allow
}

// ---------------------------------------------------------------------------
// Catalogue config
// ---------------------------------------------------------------------------

/// Invalid panel catalogue (legacy `SelfRoleConfigError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SelfRoleConfigError {
    message: String,
}

impl SelfRoleConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The rejection message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Runtime gates (legacy `src/core/config.ts` self-role half +
/// `src/discord/selfRoles.ts` `dryRun`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfRoleGates {
    /// Parsed `TWO_SELF_ROLE_PANELS` catalogue (empty disables the feature).
    pub panels: Vec<SelfRolePanel>,
    /// `TWO_SELF_ROLE_DRY_RUN=1` — audit without mutating Discord.
    pub dry_run: bool,
}

impl SelfRoleGates {
    /// Read gates from the process environment.
    pub fn from_env() -> Result<Self, SelfRoleConfigError> {
        Self::from_map(&std::env::vars().collect())
    }

    /// Read gates from an explicit map (tests, staged config).
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, SelfRoleConfigError> {
        let raw = vars
            .get("TWO_SELF_ROLE_PANELS")
            .map(String::as_str)
            .unwrap_or("");
        Ok(Self {
            panels: parse_self_role_panels(raw)?,
            dry_run: vars.get("TWO_SELF_ROLE_DRY_RUN").is_some_and(|v| v == "1"),
        })
    }
}

/// Load the panel catalogue from a `TWO_SELF_ROLE_PANELS` value.
///
/// An empty value disables the feature. A malformed non-empty value is a
/// startup error: silently running with no pickers would leave old panels
/// clickable while the bot ignores them (legacy `loadSelfRolePanels`).
pub fn parse_self_role_panels(raw: &str) -> Result<Vec<SelfRolePanel>, SelfRoleConfigError> {
    if raw.trim().is_empty() {
        return Ok(vec![]);
    }
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|_| SelfRoleConfigError::new("TWO_SELF_ROLE_PANELS must be valid JSON."))?;
    let items = value
        .as_array()
        .ok_or_else(|| SelfRoleConfigError::new("TWO_SELF_ROLE_PANELS must be a JSON array."))?;
    let mut panels = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        panels.push(parse_panel(item, index)?);
    }
    let mut panel_ids = HashSet::new();
    let mut message_ids = HashSet::new();
    let mut role_panels: HashMap<&str, &str> = HashMap::new();
    for panel in &panels {
        if !panel_ids.insert(panel.id.as_str()) {
            return Err(SelfRoleConfigError::new(format!(
                "duplicate panel id \"{}\"",
                panel.id
            )));
        }
        if !message_ids.insert(panel.message_id.as_str()) {
            return Err(SelfRoleConfigError::new(format!(
                "message {} is assigned to more than one panel",
                panel.message_id
            )));
        }
        for option in &panel.options {
            if let Some(prior) = role_panels.insert(option.role_id.as_str(), panel.id.as_str()) {
                return Err(SelfRoleConfigError::new(format!(
                    "role {} is assigned to both panel \"{prior}\" and panel \"{}\"",
                    option.role_id, panel.id
                )));
            }
        }
    }
    Ok(panels)
}

/// A live role snapshot for catalogue resolution (legacy
/// `SelfRoleResolvedRole`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRole {
    pub id: String,
    pub name: Option<String>,
    pub permissions: u64,
    /// Discord integer color; zero/`None` means no visible color.
    pub color: Option<u32>,
}

/// Resolve the deployment catalogue against Discord before any controls
/// register: the JSON alone cannot prove a snowflake is safe; the live
/// role permission mask is the authorization boundary (legacy
/// `validateSelfRolePanelRoles`).
pub fn validate_panel_roles(
    panels: &[SelfRolePanel],
    roles: &[ResolvedRole],
    channels: &[ChannelSnapshot],
    guild_id: Option<&str>,
) -> Result<(), SelfRoleConfigError> {
    let by_id: HashMap<&str, &ResolvedRole> = roles.iter().map(|r| (r.id.as_str(), r)).collect();
    if !panels.is_empty() && !channels.is_empty() {
        let everyone = guild_id.and_then(|id| by_id.get(id));
        if guild_id.is_none() || everyone.is_none() {
            return Err(SelfRoleConfigError::new(
                "the guild id and @everyone role are required to validate channel access",
            ));
        }
    }
    for panel in panels {
        for option in &panel.options {
            let role = by_id.get(option.role_id.as_str()).ok_or_else(|| {
                SelfRoleConfigError::new(format!(
                    "panel \"{}\" option \"{}\" role {} does not exist",
                    panel.id, option.key, option.role_id
                ))
            })?;
            if let Some(disallowed) = find_disallowed_permission(role.permissions) {
                return Err(SelfRoleConfigError::new(format!(
                    "panel \"{}\" option \"{}\" role {}{} has disallowed permission {disallowed}",
                    panel.id,
                    option.key,
                    option.role_id,
                    role_name(role),
                )));
            }
            if role.permissions.to_string() != option.permissions {
                return Err(SelfRoleConfigError::new(format!(
                    "panel \"{}\" option \"{}\" role {}{} permission mask changed from {} to {}",
                    panel.id,
                    option.key,
                    option.role_id,
                    role_name(role),
                    option.permissions,
                    role.permissions,
                )));
            }
            if panel.color && role.color.unwrap_or(0) == 0 {
                return Err(SelfRoleConfigError::new(format!(
                    "panel \"{}\" option \"{}\" role {}{} does not have a visible Discord color",
                    panel.id,
                    option.key,
                    option.role_id,
                    role_name(role),
                )));
            }
            if let Some(guild) = guild_id {
                if let Some(everyone) = guild_id.and_then(|id| by_id.get(id)) {
                    if let Some(grant) = find_unsafe_channel_grant(
                        guild,
                        &option.role_id,
                        everyone.permissions,
                        role.permissions,
                        channels,
                    ) {
                        return Err(SelfRoleConfigError::new(format!(
                            "panel \"{}\" option \"{}\" role {}{} has disallowed effective channel permission {} in channel {}{}",
                            panel.id,
                            option.key,
                            option.role_id,
                            role_name(role),
                            grant.permission,
                            grant.channel_id,
                            grant
                                .channel_name
                                .map_or(String::new(), |n| format!(" (\"{n}\")")),
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

fn role_name(role: &ResolvedRole) -> String {
    role.name
        .as_deref()
        .map_or(String::new(), |n| format!(" (\"{n}\")"))
}

fn parse_panel(
    value: &serde_json::Value,
    index: usize,
) -> Result<SelfRolePanel, SelfRoleConfigError> {
    let at = format!("panel[{index}]");
    let panel = value
        .as_object()
        .ok_or_else(|| SelfRoleConfigError::new(format!("{at} must be an object")))?;
    let id = short_key(get(panel, &at, "id")?, &format!("{at}.id"))?;
    let channel_id = snowflake(get(panel, &at, "channelId")?, &format!("{at}.channelId"))?;
    let message_id = snowflake(get(panel, &at, "messageId")?, &format!("{at}.messageId"))?;
    let mode = match get(panel, &at, "mode")?.as_str() {
        Some("button") => PanelMode::Button,
        Some("select") => PanelMode::Select,
        Some("reaction") => PanelMode::Reaction,
        _ => {
            return Err(SelfRoleConfigError::new(format!(
                "{at}.mode must be \"button\", \"select\", or \"reaction\""
            )));
        }
    };
    let exclusive =
        optional_boolean(get_opt(panel, "exclusive"), &format!("{at}.exclusive"))?.unwrap_or(false);
    let color = optional_boolean(get_opt(panel, "color"), &format!("{at}.color"))?.unwrap_or(false);
    if color && !exclusive {
        return Err(SelfRoleConfigError::new(format!(
            "{at}.color requires exclusive=true"
        )));
    }
    let options_value = get(panel, &at, "options")?;
    let options_items = options_value
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            SelfRoleConfigError::new(format!("{at}.options must be a non-empty array"))
        })?;
    if mode == PanelMode::Button && options_items.len() > 25 {
        return Err(SelfRoleConfigError::new(format!(
            "{at} has more than Discord's 25-button limit"
        )));
    }
    if mode == PanelMode::Select && options_items.len() > 25 {
        return Err(SelfRoleConfigError::new(format!(
            "{at} has more than Discord's 25-option select limit"
        )));
    }
    let mut option_keys = HashSet::new();
    let mut role_ids = HashSet::new();
    let mut emojis = HashSet::new();
    let mut options = Vec::with_capacity(options_items.len());
    for (option_index, item) in options_items.iter().enumerate() {
        let oat = format!("{at}.options[{option_index}]");
        let o = item
            .as_object()
            .ok_or_else(|| SelfRoleConfigError::new(format!("{oat} must be an object")))?;
        let key = short_key(get(o, &oat, "key")?, &format!("{oat}.key"))?;
        if mode == PanelMode::Button
            && self_role_custom_id(&id, Some(&key)).encode_utf16().count() > 100
        {
            return Err(SelfRoleConfigError::new(format!(
                "{oat}.key combined with {at}.id exceeds Discord's 100-character custom id limit"
            )));
        }
        let label_max = if mode == PanelMode::Button { 80 } else { 100 };
        let label = capped_text(get(o, &oat, "label")?, &format!("{oat}.label"), label_max)?;
        let role_id = snowflake(get(o, &oat, "roleId")?, &format!("{oat}.roleId"))?;
        let permissions =
            permission_mask(get(o, &oat, "permissions")?, &format!("{oat}.permissions"))?;
        if let Some(disallowed) = find_disallowed_permission(permissions) {
            return Err(SelfRoleConfigError::new(format!(
                "{oat}.roleId {role_id} has disallowed permission {disallowed} in {oat}.permissions"
            )));
        }
        let emoji = optional_text(get_opt(o, "emoji"), &format!("{oat}.emoji"), 100)?;
        let description = optional_text(
            get_opt(o, "description"),
            &format!("{oat}.description"),
            100,
        )?;
        let emoji_key = emoji.as_deref().map(emoji_identity);

        if !option_keys.insert(key.clone()) {
            return Err(SelfRoleConfigError::new(format!(
                "{at} has duplicate option key \"{key}\""
            )));
        }
        if !role_ids.insert(role_id.clone()) {
            return Err(SelfRoleConfigError::new(format!(
                "{at} offers role {role_id} more than once"
            )));
        }
        if mode == PanelMode::Reaction && emoji.is_none() {
            return Err(SelfRoleConfigError::new(format!("{oat}.emoji is required")));
        }
        if mode == PanelMode::Reaction {
            if let Some(ek) = emoji_key {
                if !emojis.insert(ek.to_owned()) {
                    return Err(SelfRoleConfigError::new(format!(
                        "{at} has duplicate reaction emoji \"{}\"",
                        emoji.as_deref().unwrap_or("")
                    )));
                }
            }
        }
        options.push(SelfRoleOption {
            key,
            label,
            role_id,
            permissions: permissions.to_string(),
            emoji,
            description,
        });
    }
    Ok(SelfRolePanel {
        id,
        channel_id,
        message_id,
        mode,
        exclusive,
        color,
        options,
    })
}

fn get<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    at: &str,
    key: &str,
) -> Result<&'a serde_json::Value, SelfRoleConfigError> {
    obj.get(key)
        .ok_or_else(|| SelfRoleConfigError::new(format!("{at}.{key} must be present")))
}

fn get_opt<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a serde_json::Value> {
    obj.get(key)
}

// Match legacy JavaScript string.length: astral characters use two UTF-16 units.
fn capped_text(
    value: &serde_json::Value,
    at: &str,
    max: usize,
) -> Result<String, SelfRoleConfigError> {
    match value.as_str() {
        Some(s) if !s.trim().is_empty() && s.encode_utf16().count() <= max => Ok(s.to_owned()),
        _ => Err(SelfRoleConfigError::new(format!(
            "{at} must be a non-empty string no longer than {max} characters"
        ))),
    }
}

fn optional_text(
    value: Option<&serde_json::Value>,
    at: &str,
    max: usize,
) -> Result<Option<String>, SelfRoleConfigError> {
    match value {
        None => Ok(None),
        Some(v) => capped_text(v, at, max).map(Some),
    }
}

fn short_key(value: &serde_json::Value, at: &str) -> Result<String, SelfRoleConfigError> {
    let key = capped_text(value, at, 60)?;
    if !valid_key(&key) {
        return Err(SelfRoleConfigError::new(format!(
            "{at} must contain only lowercase letters, digits, _ or -"
        )));
    }
    Ok(key)
}

fn snowflake(value: &serde_json::Value, at: &str) -> Result<String, SelfRoleConfigError> {
    match value.as_str() {
        Some(s) if is_snowflake(s) => Ok(s.to_owned()),
        _ => Err(SelfRoleConfigError::new(format!(
            "{at} must be a Discord snowflake string"
        ))),
    }
}

fn permission_mask(value: &serde_json::Value, at: &str) -> Result<u64, SelfRoleConfigError> {
    match value.as_str() {
        Some(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
            s.parse::<u64>().map_err(|_| invalid_mask(at))
        }
        _ => Err(invalid_mask(at)),
    }
}

fn invalid_mask(at: &str) -> SelfRoleConfigError {
    SelfRoleConfigError::new(format!("{at} must be a Discord permission bitfield string"))
}

fn optional_boolean(
    value: Option<&serde_json::Value>,
    at: &str,
) -> Result<Option<bool>, SelfRoleConfigError> {
    match value {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| SelfRoleConfigError::new(format!("{at} must be boolean")))
            .map(Some),
    }
}

// ---------------------------------------------------------------------------
// Dispatch validation
// ---------------------------------------------------------------------------

/// A role snapshot for dispatch validation (framework-free view of the
/// discord.js `Role` fields `validateSelfRoleDispatch` reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchRole {
    pub id: String,
    pub permissions: u64,
    /// Discord integer color; zero means no visible color.
    pub color: u32,
    /// Integration-managed roles can never be self-served.
    pub managed: bool,
    /// Role-hierarchy rank; the bot may only touch roles below its own.
    pub position: i64,
}

/// Dispatch-time validation input (legacy `validateSelfRoleDispatch`
/// arguments, framework-free).
pub struct DispatchCheck<'a> {
    pub panel: &'a SelfRolePanel,
    pub role_ids: &'a [String],
    pub roles: &'a [DispatchRole],
    pub channels: &'a [ChannelSnapshot],
    pub guild_id: &'a str,
    pub bot_has_manage_roles: bool,
    pub bot_highest_position: i64,
}

/// A dispatch-time refusal (legacy `{ code, reason, publicMessage }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchFailure {
    pub code: &'static str,
    pub reason: String,
    pub public_message: &'static str,
}

/// Validate a planned mutation against live Discord state before any role
/// call: bot capability, role existence and safety, deployment-mask drift,
/// color presence, and role hierarchy (legacy `validateSelfRoleDispatch`).
#[must_use]
pub fn validate_self_role_dispatch(check: &DispatchCheck<'_>) -> Option<DispatchFailure> {
    const MANAGE_ROLES: &str =
        "I cannot manage roles in this server. Staff have been notified in the logs.";
    const UNSAFE_ROLE: &str =
        "That role is not safe for self-service. Staff have been notified in the logs.";
    const UNVERIFIABLE: &str =
        "I could not verify that role. Staff have been notified in the logs.";
    if !check.bot_has_manage_roles {
        return Some(DispatchFailure {
            code: "missing_manage_roles",
            reason: "bot member does not have Manage Roles".to_owned(),
            public_message: MANAGE_ROLES,
        });
    }
    let mut seen = HashSet::new();
    for role_id in check.role_ids {
        if !seen.insert(role_id.as_str()) {
            continue;
        }
        let Some(role) = check.roles.iter().find(|r| &r.id == role_id) else {
            return Some(DispatchFailure {
                code: "missing_role",
                reason: format!("role {role_id} is not in the guild cache"),
                public_message: "That role no longer exists.",
            });
        };
        if let Some(disallowed) = find_disallowed_permission(role.permissions) {
            return Some(DispatchFailure {
                code: "disallowed_role_permission",
                reason: format!("role {role_id} has disallowed permission {disallowed}"),
                public_message: UNSAFE_ROLE,
            });
        }
        let everyone = check.roles.iter().find(|r| r.id == check.guild_id);
        if !check.channels.is_empty() && everyone.is_none() {
            return Some(DispatchFailure {
                code: "missing_everyone_role",
                reason: format!("guild @everyone role {} was not fetched", check.guild_id),
                public_message: UNVERIFIABLE,
            });
        }
        if let Some(everyone) = everyone {
            if let Some(grant) = find_unsafe_channel_grant(
                check.guild_id,
                role_id,
                everyone.permissions,
                role.permissions,
                check.channels,
            ) {
                return Some(DispatchFailure {
                    code: "disallowed_channel_permission",
                    reason: format!(
                        "role {role_id} has disallowed effective channel permission {} in channel {}",
                        grant.permission, grant.channel_id
                    ),
                    public_message: UNSAFE_ROLE,
                });
            }
        }
        match check.panel.options.iter().find(|o| &o.role_id == role_id) {
            Some(option) if option.permissions == role.permissions.to_string() => {}
            Some(option) => {
                return Some(DispatchFailure {
                    code: "role_permissions_changed",
                    reason: format!(
                        "role {role_id} permission mask changed from {} to {}",
                        option.permissions, role.permissions
                    ),
                    public_message: "That role changed after this panel was configured. Staff have been notified in the logs.",
                });
            }
            None => {
                return Some(DispatchFailure {
                    code: "role_permissions_changed",
                    reason: format!("role {role_id} is not configured on panel {}", check.panel.id),
                    public_message: "That role changed after this panel was configured. Staff have been notified in the logs.",
                });
            }
        }
        if check.panel.color && role.color == 0 {
            return Some(DispatchFailure {
                code: "missing_role_color",
                reason: format!("color-panel role {role_id} does not have a visible Discord color"),
                public_message:
                    "That color role has no visible color. Staff have been notified in the logs.",
            });
        }
        if role.managed || role.position >= check.bot_highest_position {
            return Some(DispatchFailure {
                code: "role_hierarchy",
                reason: format!("role {role_id} is managed or not below the bot"),
                public_message: "I cannot manage that role because the role hierarchy is wrong.",
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Replies, leases, ordering
// ---------------------------------------------------------------------------

/// Ephemeral reply for a settled outcome (legacy `editReply` content in
/// `applyClaimedRoleDelta`).
#[must_use]
pub fn self_role_reply(outcome: SettledOutcome, color_panel: bool) -> &'static str {
    match outcome {
        SettledOutcome::Assigned => "Role added.",
        SettledOutcome::Removed => "Role removed.",
        SettledOutcome::Switched if color_panel => "Color updated.",
        SettledOutcome::Switched => "Role updated.",
        SettledOutcome::AlreadyHeld | SettledOutcome::AlreadyAbsent => {
            "No role changes were needed."
        }
        SettledOutcome::Rejected => {
            "I could not change that role. Staff have been notified in the logs."
        }
    }
}

/// Renewal delay for a lease: a third of the lease, at least 1ms, so a
/// renewal always lands before expiry (legacy `renewAfterMs`).
#[must_use]
pub fn self_role_renew_after_ms(lease_ms: u64) -> u64 {
    (lease_ms / 3).max(1)
}

/// True while the caller still owns a claim: the stored expiry must be
/// strictly in the future (legacy `ownsClaim` / `ownsPanelClaim`).
#[must_use]
pub fn self_role_claim_owned(expires_at_ms: u64, now_ms: u64) -> bool {
    expires_at_ms > now_ms
}

/// Total order for exclusive-panel events: snowflake dispatches sort by
/// `(timestamp, id)`; generated ids sort by `(event timestamp, event id)`.
/// Reuse the original `now_ms` timestamp on retries. The full generated id
/// is a collision-free tie-breaker across workers, with no local sequence;
/// at the same timestamp, snowflakes sort before generated ids.
#[must_use]
pub fn event_order_for_event_id(event_id: &str, now_ms: u64) -> String {
    if is_snowflake(event_id) {
        let snowflake: u64 = event_id.parse().unwrap_or(0);
        let timestamp = (snowflake >> 22) + DISCORD_EPOCH_MS;
        return format!("{:013}:{event_id:0>20}", timestamp.min(9_999_999_999_999));
    }
    format!("{now_ms:013}:generated:{event_id}")
}

/// Derive the order of a stored event id when the row predates explicit
/// ordering (legacy `eventOrderFromSnowflake`); `None` for generated ids.
#[must_use]
pub fn event_order_from_snowflake(event_id: &str) -> Option<String> {
    if !is_snowflake(event_id) {
        return None;
    }
    let snowflake: u64 = event_id.parse().ok()?;
    let timestamp = (snowflake >> 22) + DISCORD_EPOCH_MS;
    Some(format!(
        "{:013}:{event_id:0>20}",
        timestamp.min(9_999_999_999_999)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: &str = "111111111111111111";
    const ROLE_A: &str = "222222222222222222";
    const ROLE_B: &str = "333333333333333333";
    const ROLE_C: &str = "444444444444444444";
    const MSG: &str = "555555555555555555";

    fn panel() -> SelfRolePanel {
        SelfRolePanel {
            id: "games".to_owned(),
            channel_id: GUILD.to_owned(),
            message_id: MSG.to_owned(),
            mode: PanelMode::Button,
            exclusive: false,
            color: false,
            options: vec![
                SelfRoleOption {
                    key: "chess".to_owned(),
                    label: "Chess".to_owned(),
                    role_id: ROLE_A.to_owned(),
                    permissions: "0".to_owned(),
                    emoji: None,
                    description: None,
                },
                SelfRoleOption {
                    key: "go".to_owned(),
                    label: "Go".to_owned(),
                    role_id: ROLE_B.to_owned(),
                    permissions: "0".to_owned(),
                    emoji: None,
                    description: None,
                },
            ],
        }
    }

    fn exclusive_panel() -> SelfRolePanel {
        let mut panel = panel();
        panel.id = "color".to_owned();
        panel.exclusive = true;
        panel
    }

    fn held(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn button_add_remove_and_duplicates_are_idempotent() {
        let panel = panel();
        // Fresh claim adds.
        let plan = plan_self_role_change(&panel, "chess", &held(&[]), PanelMode::Button, false)
            .expect("plans");
        assert_eq!(plan.operation, RoleOperation::Add);
        assert_eq!(plan.add_role_ids, [ROLE_A]);
        assert!(plan.remove_role_ids.is_empty());
        assert_eq!(plan.outcome, SettledOutcome::Assigned);
        // Duplicate delivery is a no-op, not a second mutation.
        let plan =
            plan_self_role_change(&panel, "chess", &held(&[ROLE_A]), PanelMode::Button, false)
                .expect("plans");
        assert!(plan.add_role_ids.is_empty());
        assert_eq!(plan.outcome, SettledOutcome::AlreadyHeld);
        // Explicit removal revokes; removing what is absent is a no-op.
        let plan =
            plan_self_role_change(&panel, "chess", &held(&[ROLE_A]), PanelMode::Button, true)
                .expect("plans");
        assert_eq!(
            (plan.operation, plan.outcome),
            (RoleOperation::Remove, SettledOutcome::Removed)
        );
        assert_eq!(plan.remove_role_ids, [ROLE_A]);
        let plan = plan_self_role_change(&panel, "chess", &held(&[]), PanelMode::Button, true)
            .expect("plans");
        assert!(plan.remove_role_ids.is_empty());
        assert_eq!(plan.outcome, SettledOutcome::AlreadyAbsent);
    }

    #[test]
    fn reaction_add_and_remove_duplicates_are_noops() {
        let mut panel = panel();
        panel.mode = PanelMode::Reaction;
        let add = plan_self_role_change(&panel, "chess", &held(&[]), PanelMode::Reaction, false)
            .expect("add");
        assert_eq!(add.add_role_ids, [ROLE_A]);
        let duplicate_add = plan_self_role_change(
            &panel,
            "chess",
            &held(&[ROLE_A]),
            PanelMode::Reaction,
            false,
        )
        .expect("duplicate add");
        assert_eq!(duplicate_add.outcome, SettledOutcome::AlreadyHeld);
        assert!(duplicate_add.add_role_ids.is_empty());
        let remove =
            plan_self_role_change(&panel, "chess", &held(&[ROLE_A]), PanelMode::Reaction, true)
                .expect("remove");
        assert_eq!(remove.remove_role_ids, [ROLE_A]);
        let duplicate_remove =
            plan_self_role_change(&panel, "chess", &held(&[]), PanelMode::Reaction, true)
                .expect("duplicate remove");
        assert_eq!(duplicate_remove.outcome, SettledOutcome::AlreadyAbsent);
        assert!(duplicate_remove.remove_role_ids.is_empty());
    }

    #[test]
    fn select_plans_are_stable_and_preserve_unrelated_roles() {
        let panel = panel();
        let desired = vec![ROLE_B.to_owned(), ROLE_A.to_owned()];
        let roles = held(&[ROLE_C]);
        for _ in 0..20 {
            let plan = plan_select_delta(&panel, &roles, &desired).expect("plan");
            assert_eq!(plan.add_role_ids, [ROLE_A, ROLE_B]);
            assert!(plan.remove_role_ids.is_empty());
        }
        let plan = plan_select_delta(&panel, &held(&[ROLE_A, ROLE_B, ROLE_C]), &[]).expect("clear");
        assert_eq!(plan.remove_role_ids, [ROLE_A, ROLE_B]);
    }

    #[test]
    fn exclusive_replace_switches_and_collapses_duplicates() {
        let panel = exclusive_panel();
        // Holding B and picking A switches exactly those two roles.
        let plan =
            plan_self_role_change(&panel, "chess", &held(&[ROLE_B]), PanelMode::Button, false)
                .expect("plans");
        assert_eq!(plan.operation, RoleOperation::Replace);
        assert_eq!(plan.add_role_ids, [ROLE_A]);
        assert_eq!(plan.remove_role_ids, [ROLE_B]);
        assert_eq!(plan.outcome, SettledOutcome::Switched);
        // Re-delivering the same pick after the switch is already-held.
        let plan =
            plan_self_role_change(&panel, "chess", &held(&[ROLE_A]), PanelMode::Button, false)
                .expect("plans");
        assert!(plan.add_role_ids.is_empty() && plan.remove_role_ids.is_empty());
        assert_eq!(plan.outcome, SettledOutcome::AlreadyHeld);
        // Reaction-remove on an exclusive panel only drops its own role.
        let plan = plan_self_role_change(
            &panel,
            "chess",
            &held(&[ROLE_A, ROLE_C]),
            PanelMode::Button,
            true,
        )
        .expect("plans");
        assert_eq!(plan.remove_role_ids, [ROLE_A]);
    }

    #[test]
    fn plan_rejects_unknown_option_and_wrong_source() {
        let panel = panel();
        let err = plan_self_role_change(&panel, "nope", &held(&[]), PanelMode::Button, false)
            .expect_err("unknown option");
        assert_eq!(err.code(), "unknown_option");
        assert_eq!(err.reason(), "panel games has no option nope");
        let err = plan_self_role_change(&panel, "chess", &held(&[]), PanelMode::Select, false)
            .expect_err("wrong source");
        assert_eq!(err.code(), "wrong_source");
        assert_eq!(err.reason(), "panel games expects button, received select");
    }

    #[test]
    fn select_delta_names_exact_desired_state() {
        let panel = panel();
        // Non-exclusive multi-pick: pure add plans an add (legacy
        // `recomputeDelta` only names `replace` for mixed add+remove).
        let plan = plan_select_delta(
            &panel,
            &held(&[ROLE_A]),
            &[ROLE_A.to_owned(), ROLE_B.to_owned()],
        )
        .expect("plans");
        assert_eq!(plan.operation, RoleOperation::Add);
        assert_eq!(plan.add_role_ids, [ROLE_B]);
        assert!(plan.remove_role_ids.is_empty());
        assert_eq!(plan.outcome, SettledOutcome::Assigned);
        // Deselect-all removes every held panel role.
        let plan = plan_select_delta(&panel, &held(&[ROLE_A]), &[]).expect("plans");
        assert_eq!(plan.remove_role_ids, [ROLE_A]);
        assert_eq!(plan.outcome, SettledOutcome::Removed);
        // Exclusive panels refuse multi-role targets; unknown roles refuse.
        let exclusive = exclusive_panel();
        assert!(plan_select_delta(
            &exclusive,
            &held(&[]),
            &[ROLE_A.to_owned(), ROLE_B.to_owned()]
        )
        .is_err());
        assert!(plan_select_delta(&panel, &held(&[]), &["999999999999999999".to_owned()]).is_err());
    }

    #[test]
    fn select_noops_distinguish_empty_and_already_held_targets() {
        let mut panel = panel();
        panel.mode = PanelMode::Select;
        // Empty desired and held panel sets are a deselect-all no-op,
        // including when unrelated roles are held.
        for roles in [held(&[]), held(&[ROLE_C])] {
            let plan = plan_select_delta(&panel, &roles, &[]).expect("plans");
            assert_eq!(
                (plan.operation, plan.outcome),
                (RoleOperation::Remove, SettledOutcome::AlreadyAbsent)
            );
            assert!(plan.add_role_ids.is_empty() && plan.remove_role_ids.is_empty());
            assert_eq!((plan.option_key, plan.role_id), (None, None));
        }
        // Nonempty desired roles already held still audit as already-held.
        let plan = plan_select_delta(&panel, &held(&[ROLE_A, ROLE_C]), &[ROLE_A.to_owned()])
            .expect("plans");
        assert_eq!(plan.outcome, SettledOutcome::AlreadyHeld);
        assert!(plan.add_role_ids.is_empty() && plan.remove_role_ids.is_empty());
        assert_eq!(plan.option_key.as_deref(), Some("chess"));
        assert_eq!(plan.role_id.as_deref(), Some(ROLE_A));
    }

    #[test]
    fn custom_ids_round_trip_and_reject_garbage() {
        assert_eq!(
            self_role_custom_id("games", Some("chess")),
            "two:self-role:games:chess"
        );
        assert_eq!(self_role_custom_id("games", None), "two:self-role:games");
        assert_eq!(
            parse_self_role_custom_id("two:self-role:games:chess"),
            Some(ParsedCustomId {
                panel_id: "games".to_owned(),
                option_key: Some("chess".to_owned()),
            })
        );
        assert_eq!(
            parse_self_role_custom_id("two:self-role:games"),
            Some(ParsedCustomId {
                panel_id: "games".to_owned(),
                option_key: None,
            })
        );
        for bad in [
            "two:self-role:",
            "two:self-role:Games",
            "two:self-role:a:b:c",
            "other:games:chess",
            "two:self-role:games:CHESS!",
            "",
        ] {
            assert_eq!(parse_self_role_custom_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn emoji_keys_match_legacy_shapes() {
        // Unicode passes through; mentions compare by id.
        assert_eq!(emoji_identity("♟️"), "♟️");
        assert_eq!(
            emoji_identity("<:chess:666666666666666666>"),
            "666666666666666666"
        );
        assert_eq!(
            emoji_identity("<a:wave:777777777777777777>"),
            "777777777777777777"
        );
        assert_eq!(
            reaction_endpoint_emoji("<:chess:666666666666666666>"),
            "chess:666666666666666666"
        );
        assert_eq!(reaction_endpoint_emoji("♟️"), "♟️");
        let mut panel = panel();
        panel.mode = PanelMode::Reaction;
        panel.options[0].emoji = Some("<:chess:666666666666666666>".to_owned());
        panel.options[1].emoji = Some("♟️".to_owned());
        assert_eq!(
            reaction_option_key(&panel, Some("666666666666666666"), Some("chess")),
            Some("chess".to_owned())
        );
        assert_eq!(
            reaction_option_key(&panel, None, Some("♟️")),
            Some("go".to_owned())
        );
        assert_eq!(reaction_option_key(&panel, None, Some("🎲")), None);
        assert_eq!(reaction_option_key(&panel, None, None), None);
    }

    #[test]
    fn allowlist_blocks_privilege_but_permits_cosmetic_bits() {
        assert_eq!(find_disallowed_permission(0), None);
        assert_eq!(find_disallowed_permission(SELF_ROLE_ALLOWED_MASK), None);
        // Single cosmetic bits pass.
        for bit in [1 << 6, 1 << 11, 1 << 20, 1 << 26, 1 << 31] {
            assert_eq!(find_disallowed_permission(bit), None);
        }
        // Privilege is named, including future high bits by name.
        assert_eq!(
            find_disallowed_permission(1 << 3),
            Some("Administrator".to_owned())
        );
        assert_eq!(
            find_disallowed_permission(1 << 28),
            Some("ManageRoles".to_owned())
        );
        assert_eq!(
            find_disallowed_permission(1 << 40),
            Some("ModerateMembers".to_owned())
        );
        assert_eq!(
            find_disallowed_permission(1 << 60),
            Some("unknown permission bits 1152921504606846976".to_owned())
        );
    }

    #[test]
    fn channel_safety_blocks_hidden_channel_unlocks() {
        // Role overwrite granting ManageMessages is unsafe.
        let channels = [ChannelSnapshot {
            id: "999999999999999999".to_owned(),
            name: None,
            overwrites: vec![ChannelOverwrite {
                id: ROLE_A.to_owned(),
                kind: 0,
                allow: 1 << 13,
                deny: 0,
            }],
        }];
        let grant = find_unsafe_channel_grant(GUILD, ROLE_A, 0, 0, &channels).expect("unsafe");
        assert_eq!(grant.permission, "ManageMessages");
        assert_eq!(grant.kind, UnsafeGrantKind::UnsafePermission);
        // @everyone cannot see the channel but the role grants ViewChannel.
        let channels = [ChannelSnapshot {
            id: "999999999999999999".to_owned(),
            name: Some("secret".to_owned()),
            overwrites: vec![ChannelOverwrite {
                id: ROLE_A.to_owned(),
                kind: 0,
                allow: 1 << 10,
                deny: 0,
            }],
        }];
        let grant =
            find_unsafe_channel_grant(GUILD, ROLE_A, 0, 1 << 11, &channels).expect("unlock");
        assert_eq!(grant.permission, "ViewChannel");
        assert_eq!(grant.kind, UnsafeGrantKind::NewChannelAccess);
        // Public channel with cosmetic overwrites is safe.
        let channels = [ChannelSnapshot {
            id: "999999999999999999".to_owned(),
            name: None,
            overwrites: vec![],
        }];
        assert_eq!(
            find_unsafe_channel_grant(GUILD, ROLE_A, 1 << 10, 1 << 11, &channels),
            None
        );
    }

    fn dispatch_role(id: &str) -> DispatchRole {
        DispatchRole {
            id: id.to_owned(),
            permissions: 0,
            color: 0,
            managed: false,
            position: 5,
        }
    }

    fn check<'a>(
        panel: &'a SelfRolePanel,
        roles: &'a [DispatchRole],
        role_ids: &'a [String],
    ) -> DispatchCheck<'a> {
        DispatchCheck {
            panel,
            role_ids,
            roles,
            channels: &[],
            guild_id: GUILD,
            bot_has_manage_roles: true,
            bot_highest_position: 10,
        }
    }

    #[test]
    fn dispatch_refuses_hierarchy_and_capability_gaps() {
        let panel = panel();
        let ids = [ROLE_A.to_owned()];
        // Happy path: no failure.
        let roles = vec![dispatch_role(ROLE_A)];
        assert_eq!(
            validate_self_role_dispatch(&check(&panel, &roles, &ids)),
            None
        );
        // Bot without ManageRoles.
        let roles = vec![dispatch_role(ROLE_A)];
        let mut full = check(&panel, &roles, &ids);
        full.bot_has_manage_roles = false;
        assert_eq!(
            validate_self_role_dispatch(&full).map(|f| f.code),
            Some("missing_manage_roles")
        );
        // Managed role and role above the bot both refuse hierarchy.
        let mut roles = vec![dispatch_role(ROLE_A)];
        roles[0].managed = true;
        assert_eq!(
            validate_self_role_dispatch(&check(&panel, &roles, &ids)).map(|f| f.code),
            Some("role_hierarchy")
        );
        let mut roles = vec![dispatch_role(ROLE_A)];
        roles[0].managed = false;
        roles[0].position = 10;
        let failure = validate_self_role_dispatch(&check(&panel, &roles, &ids)).expect("refused");
        assert_eq!(failure.code, "role_hierarchy");
        assert_eq!(
            failure.public_message,
            "I cannot manage that role because the role hierarchy is wrong."
        );
        // Unknown role id.
        let roles: Vec<DispatchRole> = vec![];
        assert_eq!(
            validate_self_role_dispatch(&check(&panel, &roles, &ids)).map(|f| f.code),
            Some("missing_role")
        );
    }

    #[test]
    fn dispatch_refuses_drift_privilege_and_color_gaps() {
        let panel = panel();
        let ids = [ROLE_A.to_owned()];
        // Live mask drifted from the deployment approval.
        let mut roles = vec![dispatch_role(ROLE_A)];
        roles[0].permissions = 1 << 11;
        assert_eq!(
            validate_self_role_dispatch(&check(&panel, &roles, &ids)).map(|f| f.code),
            Some("role_permissions_changed")
        );
        // Live privilege is refused even when the panel approves it.
        let mut privileged = panel.clone();
        privileged.options[0].permissions = (1 << 3).to_string();
        let mut roles = vec![dispatch_role(ROLE_A)];
        roles[0].permissions = 1 << 3;
        assert_eq!(
            validate_self_role_dispatch(&check(&privileged, &roles, &ids)).map(|f| f.code),
            Some("disallowed_role_permission")
        );
        // Color panel with a colorless role.
        let mut color_panel = exclusive_panel();
        color_panel.color = true;
        let roles = vec![dispatch_role(ROLE_A)];
        assert_eq!(
            validate_self_role_dispatch(&check(&color_panel, &roles, &ids)).map(|f| f.code),
            Some("missing_role_color")
        );
    }

    #[test]
    fn catalogue_parses_and_rejects_bad_shape() {
        assert_eq!(parse_self_role_panels("").expect("empty"), vec![]);
        assert_eq!(parse_self_role_panels("  ").expect("blank"), vec![]);
        let raw = serde_json::json!([{
            "id": "games",
            "channelId": GUILD,
            "messageId": MSG,
            "mode": "button",
            "options": [{
                "key": "chess", "label": "Chess",
                "roleId": ROLE_A, "permissions": "2048",
            }],
        }])
        .to_string();
        let panels = parse_self_role_panels(&raw).expect("parses");
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].options[0].permissions, "2048");
        // Malformed JSON, non-array, and bad mode are startup errors.
        assert!(parse_self_role_panels("{bad").is_err());
        assert!(parse_self_role_panels("{}").is_err());
        assert!(parse_self_role_panels(
            r#"[{"id":"a","channelId":"1","messageId":"1","mode":"poll","options":[]}]"#
        )
        .is_err());
        //Privileged deployment masks are rejected at parse time.
        let raw = serde_json::json!([{
            "id": "games",
            "channelId": GUILD,
            "messageId": MSG,
            "mode": "button",
            "options": [{
                "key": "admin", "label": "Admin",
                "roleId": ROLE_A, "permissions": "8",
            }],
        }])
        .to_string();
        let err = parse_self_role_panels(&raw).expect_err("privileged");
        assert!(
            err.message()
                .contains("disallowed permission Administrator"),
            "{err}"
        );
        // Reaction options require emoji; color requires exclusive.
        let raw = serde_json::json!([{
            "id": "react",
            "channelId": GUILD,
            "messageId": MSG,
            "mode": "reaction",
            "options": [{
                "key": "chess", "label": "Chess",
                "roleId": ROLE_A, "permissions": "0",
            }],
        }])
        .to_string();
        assert!(parse_self_role_panels(&raw).is_err());
        // Duplicate panel ids, shared messages, and shared roles are rejected.
        let two = |second: serde_json::Value| {
            serde_json::json!([
                {"id": "one", "channelId": GUILD, "messageId": MSG, "mode": "button",
                 "options": [{"key": "a", "label": "A", "roleId": ROLE_A, "permissions": "0"}]},
                second,
            ])
            .to_string()
        };
        assert!(parse_self_role_panels(&two(serde_json::json!(
            {"id": "one", "channelId": GUILD, "messageId": "666666666666666666", "mode": "button",
             "options": [{"key": "b", "label": "B", "roleId": ROLE_B, "permissions": "0"}]})))
        .is_err());
        assert!(parse_self_role_panels(&two(serde_json::json!(
            {"id": "two", "channelId": GUILD, "messageId": MSG, "mode": "button",
             "options": [{"key": "b", "label": "B", "roleId": ROLE_B, "permissions": "0"}]})))
        .is_err());
        assert!(parse_self_role_panels(&two(serde_json::json!(
            {"id": "two", "channelId": GUILD, "messageId": "666666666666666666", "mode": "button",
             "options": [{"key": "b", "label": "B", "roleId": ROLE_A, "permissions": "0"}]})))
        .is_err());
    }

    #[test]
    fn catalogue_rejects_oversized_composed_button_ids() {
        let id = "p".repeat(60);
        let key = "k".repeat(25);
        assert_eq!(
            self_role_custom_id(&id, Some(&key)).encode_utf16().count(),
            100
        );
        let mut raw = serde_json::json!([{
            "id": id,
            "channelId": GUILD,
            "messageId": MSG,
            "mode": "button",
            "options": [{
                "key": key, "label": "Chess",
                "roleId": ROLE_A, "permissions": "0", "emoji": "♟️",
            }],
        }]);
        assert!(parse_self_role_panels(&raw.to_string()).is_ok());
        let key = "k".repeat(26);
        assert_eq!(
            self_role_custom_id(&id, Some(&key)).encode_utf16().count(),
            101
        );
        raw[0]["options"][0]["key"] = serde_json::json!(key);
        let err = parse_self_role_panels(&raw.to_string()).expect_err("oversized custom id");
        assert!(
            err.message().contains("100-character custom id limit"),
            "{err}"
        );
        // Selects use only the panel id; reaction options have no custom id.
        for mode in ["select", "reaction"] {
            raw[0]["mode"] = serde_json::json!(mode);
            assert!(parse_self_role_panels(&raw.to_string()).is_ok(), "{mode}");
        }
    }

    #[test]
    fn catalogue_labels_use_surface_specific_utf16_limits() {
        let mut raw = serde_json::json!([{
            "id": "games",
            "channelId": GUILD,
            "messageId": MSG,
            "mode": "button",
            "options": [{
                "key": "chess", "label": "Chess",
                "roleId": ROLE_A, "permissions": "0",
            }],
        }]);
        for (mode, max) in [("button", 80), ("select", 100)] {
            raw[0]["mode"] = serde_json::json!(mode);
            for label in ["a".repeat(max), "🎲".repeat(max / 2)] {
                assert_eq!(label.encode_utf16().count(), max);
                raw[0]["options"][0]["label"] = serde_json::json!(label);
                assert!(parse_self_role_panels(&raw.to_string()).is_ok(), "{mode}");
                raw[0]["options"][0]["label"] = serde_json::json!(format!("{label}a"));
                let err = parse_self_role_panels(&raw.to_string()).expect_err("oversized label");
                assert!(err.message().contains("options[0].label"), "{err}");
            }
        }
    }

    #[test]
    fn capped_text_counts_utf16_units_at_astral_boundaries() {
        let text = "🎲".repeat(50);
        let value = serde_json::json!(text);
        assert_eq!(capped_text(&value, "text", 100).expect("100 units"), text);
        assert!(optional_text(Some(&value), "text", 100).is_ok());
        let value = serde_json::json!(format!("{text}a"));
        assert!(capped_text(&value, "text", 100).is_err());
        assert!(optional_text(Some(&value), "text", 100).is_err());
    }

    #[test]
    fn gates_default_off_and_read_env() {
        let gates = SelfRoleGates::from_map(&HashMap::new()).expect("defaults");
        assert!(gates.panels.is_empty() && !gates.dry_run);
        let vars: HashMap<String, String> = [("TWO_SELF_ROLE_DRY_RUN", "1")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert!(SelfRoleGates::from_map(&vars).expect("parses").dry_run);
    }

    #[test]
    fn replies_lease_math_and_event_order_match_legacy() {
        assert_eq!(
            self_role_reply(SettledOutcome::Assigned, false),
            "Role added."
        );
        assert_eq!(
            self_role_reply(SettledOutcome::Removed, false),
            "Role removed."
        );
        assert_eq!(
            self_role_reply(SettledOutcome::Switched, true),
            "Color updated."
        );
        assert_eq!(
            self_role_reply(SettledOutcome::Switched, false),
            "Role updated."
        );
        assert_eq!(
            self_role_reply(SettledOutcome::AlreadyHeld, false),
            "No role changes were needed."
        );
        // Renew at a third of the lease; ownership needs a future expiry.
        assert_eq!(self_role_renew_after_ms(SELF_ROLE_CLAIM_LEASE_MS), 100_000);
        assert_eq!(self_role_renew_after_ms(2), 1);
        assert!(self_role_claim_owned(200, 100));
        assert!(!self_role_claim_owned(100, 100));
        assert!(!self_role_claim_owned(50, 100));
        // Snowflake orders sort by (timestamp, id), ignoring the supplied clock.
        let order = event_order_for_event_id("100000000000000000", 0);
        assert_eq!(order, "1443912257910:00100000000000000000", "{order}");
        assert_eq!(
            event_order_for_event_id("100000000000000000", 1_700_000_000_000),
            order
        );
        assert_eq!(
            event_order_from_snowflake("100000000000000000"),
            Some(order)
        );
        assert_eq!(event_order_from_snowflake("reaction:abc"), None);
    }

    #[test]
    fn generated_event_order_is_unique_and_stable_across_workers() {
        let timestamp = 1_700_000_000_000;
        let first = std::thread::spawn(move || {
            event_order_for_event_id("reaction:worker-a:0001", timestamp)
        })
        .join()
        .expect("first worker");
        let second = std::thread::spawn(move || {
            event_order_for_event_id("reaction:worker-b:0001", timestamp)
        })
        .join()
        .expect("second worker");
        assert_eq!(first, "1700000000000:generated:reaction:worker-a:0001");
        assert_eq!(second, "1700000000000:generated:reaction:worker-b:0001");
        assert_ne!(first, second);
        assert!(first < second);
        // Replaying after another worker's event must not change the order.
        assert_eq!(
            event_order_for_event_id("reaction:worker-a:0001", timestamp),
            first
        );
        assert_eq!(
            event_order_for_event_id("reaction:worker-b:0001", timestamp),
            second
        );
        assert!(first < event_order_for_event_id("reaction:worker-a:0001", timestamp + 1));
        // The generated namespace cannot collide with a snowflake at the same millisecond.
        let snowflake = event_order_for_event_id("100000000000000000", 0);
        let generated = event_order_for_event_id("reaction:abc", 1_443_912_257_910);
        assert!(snowflake < generated);
    }

    #[test]
    fn panel_resolution_rejects_drift_and_unsafe_grants() {
        let raw = serde_json::json!([{
            "id": "games",
            "channelId": GUILD,
            "messageId": MSG,
            "mode": "button",
            "options": [{
                "key": "chess", "label": "Chess",
                "roleId": ROLE_A, "permissions": "2048",
            }],
        }])
        .to_string();
        let panels = parse_self_role_panels(&raw).expect("parses");
        let live = |permissions: u64| {
            vec![
                ResolvedRole {
                    id: GUILD.to_owned(),
                    name: None,
                    permissions: 1 << 10,
                    color: None,
                },
                ResolvedRole {
                    id: ROLE_A.to_owned(),
                    name: Some("Chess".to_owned()),
                    permissions,
                    color: None,
                },
            ]
        };
        assert!(validate_panel_roles(&panels, &live(1 << 11), &[], None).is_ok());
        // Drifted live mask is a startup error.
        assert!(validate_panel_roles(&panels, &live(1 << 11 | 1 << 14), &[], None).is_err());
        // Missing channel-access context is a startup error when given channels.
        let channels = [ChannelSnapshot {
            id: "999999999999999999".to_owned(),
            name: None,
            overwrites: vec![],
        }];
        assert!(validate_panel_roles(&panels, &live(1 << 11), &channels, None).is_err());
    }
}
