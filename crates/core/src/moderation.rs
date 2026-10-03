//! Moderation command definitions, policy core, and env gates.
//!
//! Slice 3 of TOG-9809 — the moderation LAST slice (highest regression risk,
//! so it ports last). Ports the slash-command *shapes* (parity §1 #3–#11),
//! the hierarchy/protection policy (`policy.ts`), and the enable gate
//! (`config.ts`) from legacy two-bot as framework-free data plus pure
//! functions built on the `commands` registry module. The moderation service,
//! stores, unban sweep, and Discord adapter land in a later slice; this module
//! only defines what gets published on `guild.commands.set` and who may
//! invoke it, so every shape and refusal is unit-testable without Discord.
//!
//! Source files (legacy `two-bot`, frozen `main`):
//! - shapes: `src/moderation/commands.ts` (`MODERATION_COMMAND_DATA`) —
//!   `ban`/`tempban` gate `BanMembers`, `kick` gates `KickMembers`,
//!   `timeout`/`warn` gate `ModerateMembers`, `purge` gates `ManageMessages`,
//!   `slowmode`/`lockdown`/`unlock` gate `ManageChannels`. All guild-only.
//! - policy: `src/moderation/policy.ts` (`assertModerationAllowed`,
//!   `moderationTargetProtection`) + `src/moderation/types.ts`
//!   (`requireModerationReason`, action/actor/target shapes).
//! - gates: `src/moderation/config.ts` (`loadModerationConfig`).
//!
//! Staging gate: these definitions publish only while `TWO_MODERATION=1`, and
//! stay staging-only until the soak passes (card acceptance). The guild
//! allowlist fence (`assertActivationPermitted`, TOG-3186) is enforced by the
//! boot adapter, same posture as slice 2 — this module carries no guild id.
//!
//! Deliberately out of scope: automod filters/matcher (slice 4),
//! containment/anti-nuke heat scoring (slice 5), the moderation service +
//! stores + unban sweep (needs S6 sqlx), and the moderation-audit MAC (S5).

use std::collections::{HashMap, HashSet};

use super::commands::{
    CommandDefinition, CommandOption, CommandOptionType, PERM_BAN_MEMBERS, PERM_KICK_MEMBERS,
    PERM_MANAGE_CHANNELS, PERM_MANAGE_MESSAGES, PERM_MODERATE_MEMBERS, SCHEDULE_EVERY_MINUTES_MAX,
    SCHEDULE_EVERY_MINUTES_MIN, SCHEDULE_IN_MINUTES_MAX, SCHEDULE_IN_MINUTES_MIN,
    STICKY_DEBOUNCE_MAX_SECONDS, STICKY_DEBOUNCE_MIN_SECONDS, TEMPBAN_DURATION_MAX_SECONDS,
    TEMPBAN_DURATION_MIN_SECONDS, TIMEOUT_DURATION_MAX_SECONDS, TIMEOUT_DURATION_MIN_SECONDS,
};

/// A moderation verb (parity §1 #3–#11, legacy `MODERATION_COMMANDS` order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModerationAction {
    Ban,
    TempBan,
    Kick,
    Timeout,
    Warn,
    Purge,
    Slowmode,
    Lockdown,
    Unlock,
}

impl ModerationAction {
    /// All nine verbs in legacy publish order.
    pub const ALL: [Self; 9] = [
        Self::Ban,
        Self::TempBan,
        Self::Kick,
        Self::Timeout,
        Self::Warn,
        Self::Purge,
        Self::Slowmode,
        Self::Lockdown,
        Self::Unlock,
    ];

    /// Slash-command name (`ban`, `tempban`, …).
    #[must_use]
    pub fn command_name(self) -> &'static str {
        match self {
            Self::Ban => "ban",
            Self::TempBan => "tempban",
            Self::Kick => "kick",
            Self::Timeout => "timeout",
            Self::Warn => "warn",
            Self::Purge => "purge",
            Self::Slowmode => "slowmode",
            Self::Lockdown => "lockdown",
            Self::Unlock => "unlock",
        }
    }

    /// Service action name (legacy `ModerationActionName`: `moderation.ban`, …).
    #[must_use]
    pub fn action_name(self) -> &'static str {
        match self {
            Self::Ban => "moderation.ban",
            Self::TempBan => "moderation.tempban",
            Self::Kick => "moderation.kick",
            Self::Timeout => "moderation.timeout",
            Self::Warn => "moderation.warn",
            Self::Purge => "moderation.purge",
            Self::Slowmode => "moderation.slowmode",
            Self::Lockdown => "moderation.lockdown",
            Self::Unlock => "moderation.unlock",
        }
    }

    /// Discord permission display name for denial copy (TOG-13624): what the
    /// member sees in Server Settings → Roles, not the internal action id.
    #[must_use]
    pub fn discord_permission_name(self) -> &'static str {
        match self {
            Self::Ban | Self::TempBan => "Ban Members",
            Self::Kick => "Kick Members",
            Self::Timeout | Self::Warn => "Moderate Members",
            Self::Purge => "Manage Messages",
            Self::Slowmode | Self::Lockdown | Self::Unlock => "Manage Channels",
        }
    }

    /// Discord permission gate (legacy `permissionFor` + builder flags).
    #[must_use]
    pub fn required_permission(self) -> u64 {
        crate::command_permissions::command_permission(self.command_name())
            .expect("moderation command has a permission row")
            .required_permissions
    }

    /// Member-targeted verbs (legacy `TARGET_ACTIONS`). Channel verbs skip the
    /// target checks entirely in [`assert_moderation_allowed`].
    #[must_use]
    pub fn targets_member(self) -> bool {
        crate::command_permissions::command_permission(self.command_name()).is_some_and(|row| {
            row.policy_hook == Some(crate::command_permissions::PolicyHook::MemberModeration)
        })
    }
}

impl std::fmt::Display for ModerationAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.action_name())
    }
}

/// Moderation command definitions in legacy publish order (parity #3–#11).
/// The registry caller merges these via `additional_builtins` only while
/// [`ModerationGates`] is enabled.
#[must_use]
pub fn moderation_commands() -> Vec<CommandDefinition> {
    vec![
        CommandDefinition::new("ban", "Ban a member")
            .permissions(PERM_BAN_MEMBERS)
            .options(vec![CommandOption::target(), CommandOption::reason()]),
        CommandDefinition::new("tempban", "Temporarily ban a member")
            .permissions(PERM_BAN_MEMBERS)
            .options(vec![
                CommandOption::target(),
                CommandOption::new(
                    "duration_seconds",
                    "Duration in seconds",
                    CommandOptionType::Integer,
                )
                .required()
                .min_value(TEMPBAN_DURATION_MIN_SECONDS),
                CommandOption::reason(),
            ]),
        CommandDefinition::new("kick", "Kick a member from the server (moderators only)")
            .permissions(PERM_KICK_MEMBERS)
            .options(vec![CommandOption::target(), CommandOption::reason()]),
        CommandDefinition::new("timeout", "Timeout a member")
            .permissions(PERM_MODERATE_MEMBERS)
            .options(vec![
                CommandOption::target(),
                CommandOption::new(
                    "duration_seconds",
                    "Duration in seconds",
                    CommandOptionType::Integer,
                )
                .required()
                .min_value(TIMEOUT_DURATION_MIN_SECONDS),
                CommandOption::reason(),
            ]),
        CommandDefinition::new("warn", "Record a warning for a member")
            .permissions(PERM_MODERATE_MEMBERS)
            .options(vec![CommandOption::target(), CommandOption::reason()]),
        CommandDefinition::new("purge", "Delete recent messages")
            .permissions(PERM_MANAGE_MESSAGES)
            .options(vec![
                CommandOption::new(
                    "count",
                    "Messages to delete (1-100)",
                    CommandOptionType::Integer,
                )
                .required()
                .int_range(1, 100),
                CommandOption::reason(),
            ]),
        CommandDefinition::new("slowmode", "Set channel slowmode")
            .permissions(PERM_MANAGE_CHANNELS)
            .options(vec![
                CommandOption::new(
                    "seconds",
                    "Delay in seconds (0 disables)",
                    CommandOptionType::Integer,
                )
                .required()
                .int_range(0, 21600),
                CommandOption::reason(),
            ]),
        CommandDefinition::new("lockdown", "Prevent @everyone from sending messages")
            .permissions(PERM_MANAGE_CHANNELS)
            .options(vec![CommandOption::reason()]),
        CommandDefinition::new("unlock", "Allow @everyone to send messages")
            .permissions(PERM_MANAGE_CHANNELS)
            .options(vec![CommandOption::reason()]),
    ]
}

/// The invoking member (legacy `ModerationActor`). Permission bits are the
/// Discord bitfield as `u64`; role positions are role-hierarchy ranks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModerationActor {
    pub user_id: String,
    pub role_ids: Vec<String>,
    pub highest_role_position: i64,
    pub permissions: u64,
}

/// The member under moderation (legacy `ModerationTarget`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModerationTarget {
    pub user_id: String,
    pub role_ids: Vec<String>,
    pub highest_role_position: i64,
    pub is_bot: bool,
    pub is_guild_owner: bool,
}

/// Static policy inputs (legacy `ModerationPolicy`): who is protected.
/// The audit-secret half of the legacy policy (MAC minting) belongs to S5 and
/// is deliberately absent here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModerationPolicy {
    pub owen_user_id: String,
    pub protected_role_ids: HashSet<String>,
    pub bot_user_id: Option<String>,
}

/// One adjudication input (legacy `ModerationRequest`, minus `guildId` — the
/// adapter fills the guild fence, same posture as slice 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModerationRequest {
    pub action: ModerationAction,
    pub actor: ModerationActor,
    pub target: Option<ModerationTarget>,
    pub bot_highest_role_position: Option<i64>,
    pub reason: String,
    pub duration_seconds: Option<u64>,
    pub count: Option<u64>,
    pub seconds: Option<u64>,
}

/// Facts about *who the target is*, independent of verb or hierarchy (legacy
/// `ModerationTargetProtectionReason`). Split out of the full check for
/// TOG-3092: automod message deletion needs target protection without the
/// actor-permission or hierarchy comparisons that member verbs require.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetProtection {
    GuildOwner,
    Owen,
    Bot,
    StaffRole,
}

/// Policy refusal or malformed request (legacy `ActionError` codes
/// `action_not_allowed` + `malformed` for a missing target).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("Missing required permission for {0}")]
    ActorMissingPermission(ModerationAction),
    #[error("You cannot moderate yourself")]
    TargetSelf,
    #[error("The guild owner is protected")]
    TargetGuildOwner,
    #[error("Owen is protected")]
    TargetOwen,
    #[error("Bots are protected")]
    TargetBot,
    #[error("Staff roles are protected")]
    TargetStaffRole,
    #[error("The target is equal to or above your highest role")]
    ActorHierarchy,
    #[error("The target is equal to or above Owen's highest role")]
    BotHierarchy,
    #[error("This moderation action requires a target")]
    MissingTarget,
}

/// Whether this target is protected from moderation at all, independent of
/// verb. `assert_moderation_allowed` calls this rather than repeating the
/// checks, so a protection added in one place can never go missing from the
/// other (legacy `moderationTargetProtection`).
#[must_use]
pub fn moderation_target_protection(
    target: &ModerationTarget,
    policy: &ModerationPolicy,
) -> Option<TargetProtection> {
    if target.is_guild_owner {
        return Some(TargetProtection::GuildOwner);
    }
    if target.user_id == policy.owen_user_id
        || policy.bot_user_id.as_deref() == Some(target.user_id.as_str())
    {
        return Some(TargetProtection::Owen);
    }
    if target.is_bot {
        return Some(TargetProtection::Bot);
    }
    if target
        .role_ids
        .iter()
        .any(|role| policy.protected_role_ids.contains(role))
    {
        return Some(TargetProtection::StaffRole);
    }
    None
}

/// Full policy gate (legacy `assertModerationAllowed`): permission first, then
/// — for member-targeted verbs only — target presence, self-moderation,
/// target protection, bot hierarchy, actor hierarchy, in that order.
pub fn assert_moderation_allowed(
    request: &ModerationRequest,
    policy: &ModerationPolicy,
) -> Result<(), PolicyError> {
    let required = request.action.required_permission();
    if request.actor.permissions & required != required {
        return Err(PolicyError::ActorMissingPermission(request.action));
    }
    if !request.action.targets_member() {
        return Ok(());
    }
    let target = request.target.as_ref().ok_or(PolicyError::MissingTarget)?;
    if target.user_id == request.actor.user_id {
        return Err(PolicyError::TargetSelf);
    }
    match moderation_target_protection(target, policy) {
        Some(TargetProtection::GuildOwner) => return Err(PolicyError::TargetGuildOwner),
        Some(TargetProtection::Owen) => return Err(PolicyError::TargetOwen),
        Some(TargetProtection::Bot) => return Err(PolicyError::TargetBot),
        Some(TargetProtection::StaffRole) => return Err(PolicyError::TargetStaffRole),
        None => {}
    }
    if request
        .bot_highest_role_position
        .is_some_and(|bot| bot <= target.highest_role_position)
    {
        return Err(PolicyError::BotHierarchy);
    }
    if request.actor.highest_role_position <= target.highest_role_position {
        return Err(PolicyError::ActorHierarchy);
    }
    Ok(())
}

/// Invalid audit reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReasonError {
    #[error("\"reason\" must be a non-empty string")]
    Empty,
    #[error("\"reason\" is longer than 512 characters")]
    TooLong,
}

/// Validate the mandatory audit reason: trimmed, non-empty, at most 512
/// characters (legacy `requireModerationReason`, whose JS `length` counts
/// UTF-16 code units — so an astral character costs 2, not the 1 that
/// `chars().count()` would count).
pub fn require_moderation_reason(value: &str) -> Result<String, ReasonError> {
    let reason = value.trim();
    if reason.is_empty() {
        return Err(ReasonError::Empty);
    }
    if reason.encode_utf16().count() > 512 {
        return Err(ReasonError::TooLong);
    }
    Ok(reason.to_owned())
}

/// Cap refusal for bounded numeric inputs (parity §1 +
/// `docs/property-tests.md`): tempban 60–365d, timeout 60–28d, schedule
/// 1–525600 / 60–525600 minutes, sticky debounce 1–300 seconds. The builders
/// above advertise minima (tempban/timeout expose no `max_value` per legacy
/// parity); these validators enforce the runtime ceilings. Error text names
/// the field and both bounds and never echoes the caller-supplied value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("\"{field}\" must be an integer between {min} and {max}")]
pub struct ModerationCapError {
    pub field: &'static str,
    pub min: i64,
    pub max: i64,
}

fn cap_between(
    value: Option<&serde_json::Value>,
    field: &'static str,
    min: i64,
    max: i64,
) -> Result<i64, ModerationCapError> {
    match value.and_then(serde_json::Value::as_i64) {
        Some(n) if (min..=max).contains(&n) => Ok(n),
        _ => Err(ModerationCapError { field, min, max }),
    }
}

/// Validate `tempban` `duration_seconds` wire JSON: required integer
/// 60–31,536,000 (365 days). Missing and non-integer inputs refuse.
pub fn validate_tempban_duration(
    value: Option<&serde_json::Value>,
) -> Result<i64, ModerationCapError> {
    cap_between(
        value,
        "duration_seconds",
        TEMPBAN_DURATION_MIN_SECONDS,
        TEMPBAN_DURATION_MAX_SECONDS,
    )
}

/// Validate `timeout` `duration_seconds` wire JSON: required integer
/// 60–2,419,200 (28 days, Discord's own ceiling). Missing and non-integer
/// inputs refuse.
pub fn validate_timeout_duration(
    value: Option<&serde_json::Value>,
) -> Result<i64, ModerationCapError> {
    cap_between(
        value,
        "duration_seconds",
        TIMEOUT_DURATION_MIN_SECONDS,
        TIMEOUT_DURATION_MAX_SECONDS,
    )
}

/// Validate `/schedule` `in-minutes` wire JSON: required integer 1–525,600.
pub fn validate_schedule_in_minutes(
    value: Option<&serde_json::Value>,
) -> Result<i64, ModerationCapError> {
    cap_between(
        value,
        "in-minutes",
        SCHEDULE_IN_MINUTES_MIN,
        SCHEDULE_IN_MINUTES_MAX,
    )
}

/// Validate `/schedule` `every-minutes` wire JSON: required integer 60–525,600.
pub fn validate_schedule_every_minutes(
    value: Option<&serde_json::Value>,
) -> Result<i64, ModerationCapError> {
    cap_between(
        value,
        "every-minutes",
        SCHEDULE_EVERY_MINUTES_MIN,
        SCHEDULE_EVERY_MINUTES_MAX,
    )
}

/// Validate `/sticky` `debounce` wire JSON: required integer 1–300 seconds.
/// (`None` here refuses; the service-level omitted→default-5 rule lives in
/// `sticky::normalize_debounce`.)
pub fn validate_sticky_debounce(
    value: Option<&serde_json::Value>,
) -> Result<i64, ModerationCapError> {
    cap_between(
        value,
        "debounce",
        STICKY_DEBOUNCE_MIN_SECONDS,
        STICKY_DEBOUNCE_MAX_SECONDS,
    )
}

/// Moderation env gates (legacy `src/moderation/config.ts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModerationGates {
    /// `TWO_MODERATION=1` — publish + serve moderation commands.
    pub enabled: bool,
    /// `TWO_OWEN_USER_ID` — validated as a snowflake only while enabled.
    pub owen_user_id: String,
    /// `TWO_MODERATION_PROTECTED_ROLE_IDS` — always validated (legacy checks
    /// these even when moderation is off).
    pub protected_role_ids: HashSet<String>,
}

/// Invalid moderation-gate environment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModerationGateError {
    #[error("TWO_MODERATION=1 requires TWO_OWEN_USER_ID to be Owen's Discord id.")]
    InvalidOwenUserId,
    #[error("TWO_MODERATION_PROTECTED_ROLE_IDS must contain Discord role ids.")]
    InvalidProtectedRoleId,
}

fn is_snowflake(value: &str) -> bool {
    (17..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

impl ModerationGates {
    /// Read gates from the process environment.
    pub fn from_env() -> Result<Self, ModerationGateError> {
        Self::from_map(&std::env::vars().collect())
    }

    /// Read gates from an explicit map (tests, staged config).
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, ModerationGateError> {
        let enabled = vars.get("TWO_MODERATION").is_some_and(|v| v == "1");
        let owen_user_id = vars.get("TWO_OWEN_USER_ID").cloned().unwrap_or_default();
        if enabled && !is_snowflake(&owen_user_id) {
            return Err(ModerationGateError::InvalidOwenUserId);
        }
        let mut protected_role_ids = HashSet::new();
        for role in vars
            .get("TWO_MODERATION_PROTECTED_ROLE_IDS")
            .map(String::as_str)
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !is_snowflake(role) {
                return Err(ModerationGateError::InvalidProtectedRoleId);
            }
            protected_role_ids.insert(role.to_owned());
        }
        Ok(Self {
            enabled,
            owen_user_id,
            protected_role_ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::merge_commands;
    use crate::feature_commands::feature_commands;

    const OWEN_ID: &str = "123456789012345678";
    const BOT_POS: i64 = 100;

    fn actor() -> ModerationActor {
        ModerationActor {
            user_id: "111111111111111111".to_owned(),
            role_ids: vec!["222222222222222222".to_owned()],
            highest_role_position: 50,
            permissions: PERM_BAN_MEMBERS
                | PERM_KICK_MEMBERS
                | PERM_MODERATE_MEMBERS
                | PERM_MANAGE_MESSAGES
                | PERM_MANAGE_CHANNELS,
        }
    }

    fn target() -> ModerationTarget {
        ModerationTarget {
            user_id: "333333333333333333".to_owned(),
            role_ids: vec![],
            highest_role_position: 10,
            is_bot: false,
            is_guild_owner: false,
        }
    }

    fn policy() -> ModerationPolicy {
        ModerationPolicy {
            owen_user_id: OWEN_ID.to_owned(),
            protected_role_ids: HashSet::from(["444444444444444444".to_owned()]),
            bot_user_id: Some("555555555555555555".to_owned()),
        }
    }

    fn request(action: ModerationAction) -> ModerationRequest {
        ModerationRequest {
            action,
            actor: actor(),
            target: if action.targets_member() {
                Some(target())
            } else {
                None
            },
            bot_highest_role_position: Some(BOT_POS),
            reason: "test audit reason".to_owned(),
            duration_seconds: None,
            count: None,
            seconds: None,
        }
    }

    #[test]
    fn slice3_names_match_legacy_in_order() {
        let defs = moderation_commands();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "ban", "tempban", "kick", "timeout", "warn", "purge", "slowmode", "lockdown",
                "unlock",
            ]
        );
        for (action, name) in ModerationAction::ALL.iter().zip(names.iter()) {
            assert_eq!(action.command_name(), *name);
        }
    }

    #[test]
    fn full_registry_merges_without_collision() {
        let merged = merge_commands(&[feature_commands(), moderation_commands()], &[])
            .expect("slices 1-3 merge cleanly");
        // 3 core + 16 slice-2 + 9 moderation.
        assert_eq!(merged.len(), 28);
        let names: Vec<_> = merged.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(&names[..4], ["rank", "leaderboard", "help", "attendance"]);
        assert_eq!(
            &names[19..24],
            ["ban", "tempban", "kick", "timeout", "warn"]
        );
        assert_eq!(&names[24..], ["purge", "slowmode", "lockdown", "unlock"]);
        assert!(merged.iter().all(|d| !d.dm_permission));
    }

    #[test]
    fn permission_gates_match_legacy() {
        let is = |name: &str| {
            moderation_commands()
                .iter()
                .find(|d| d.name == name)
                .expect("command exists")
                .default_member_permissions
                .clone()
        };
        let ban = Some(PERM_BAN_MEMBERS.to_string());
        let kick = Some(PERM_KICK_MEMBERS.to_string());
        let moderate = Some(PERM_MODERATE_MEMBERS.to_string());
        let channels = Some(PERM_MANAGE_CHANNELS.to_string());
        assert_eq!(is("ban"), ban);
        assert_eq!(is("tempban"), ban);
        assert_eq!(is("kick"), kick);
        assert_eq!(is("timeout"), moderate);
        assert_eq!(is("warn"), moderate);
        assert_eq!(is("purge"), Some(PERM_MANAGE_MESSAGES.to_string()));
        assert_eq!(is("slowmode"), channels);
        assert_eq!(is("lockdown"), channels);
        assert_eq!(is("unlock"), channels);
    }

    #[test]
    fn option_shapes_match_legacy() {
        fn get(name: &str) -> CommandDefinition {
            moderation_commands()
                .iter()
                .find(|d| d.name == name)
                .expect("command exists")
                .clone()
        }
        // Target verbs: required user target + required capped reason.
        for name in ["ban", "tempban", "kick", "timeout", "warn"] {
            let opts = get(name).options;
            assert_eq!(opts[0].name, "target");
            assert_eq!(opts[0].kind, CommandOptionType::User.as_u8());
            assert_eq!(opts[0].required, Some(true));
            let reason = opts.last().expect("reason option");
            assert_eq!(reason.name, "reason");
            assert_eq!(reason.required, Some(true));
            assert_eq!(reason.max_length, Some(512));
        }
        // Durations: required, min 60, no max (legacy sets no max).
        for name in ["tempban", "timeout"] {
            let duration = get(name).options[1].clone();
            assert_eq!(duration.name, "duration_seconds");
            assert_eq!(duration.kind, CommandOptionType::Integer.as_u8());
            assert_eq!(duration.required, Some(true));
            assert_eq!((duration.min_value, duration.max_value), (Some(60), None));
        }
        // Purge count 1–100, slowmode seconds 0–21600, both required.
        let count = get("purge").options[0].clone();
        assert_eq!((count.min_value, count.max_value), (Some(1), Some(100)));
        assert_eq!(count.required, Some(true));
        let seconds = get("slowmode").options[0].clone();
        assert_eq!(
            (seconds.min_value, seconds.max_value),
            (Some(0), Some(21600))
        );
        assert_eq!(seconds.required, Some(true));
        // Channel verbs carry only the reason.
        for name in ["lockdown", "unlock"] {
            assert_eq!(get(name).options.len(), 1);
        }
    }

    #[test]
    fn moderation_commands_serialize_to_wire_shape() {
        let json = serde_json::to_value(moderation_commands()).expect("serializes");
        assert_eq!(json[0]["name"], "ban");
        assert_eq!(json[0]["default_member_permissions"], "4");
        assert_eq!(json[1]["options"][1]["name"], "duration_seconds");
        assert_eq!(json[1]["options"][1]["min_value"], 60);
        assert!(json[1]["options"][1].get("max_value").is_none());
        assert_eq!(json[5]["options"][0]["max_value"], 100);
        assert_eq!(json[7]["name"], "lockdown");
        assert_eq!(json[7]["options"].as_array().expect("array").len(), 1);
    }

    #[test]
    fn policy_allows_member_and_channel_verbs() {
        for action in ModerationAction::ALL {
            assert!(assert_moderation_allowed(&request(action), &policy()).is_ok());
        }
    }

    #[test]
    fn policy_refusals_match_legacy() {
        // Missing permission.
        let mut req = request(ModerationAction::Ban);
        req.actor.permissions = 0;
        assert_eq!(
            assert_moderation_allowed(&req, &policy()),
            Err(PolicyError::ActorMissingPermission(ModerationAction::Ban))
        );
        // Member verbs require a target; channel verbs do not.
        let mut req = request(ModerationAction::Ban);
        req.target = None;
        assert_eq!(
            assert_moderation_allowed(&req, &policy()),
            Err(PolicyError::MissingTarget)
        );
        let mut req = request(ModerationAction::Purge);
        req.target = None;
        assert!(assert_moderation_allowed(&req, &policy()).is_ok());
        // Self-moderation.
        let mut req = request(ModerationAction::Kick);
        req.target.as_mut().expect("target").user_id = req.actor.user_id.clone();
        assert_eq!(
            assert_moderation_allowed(&req, &policy()),
            Err(PolicyError::TargetSelf)
        );
        // Protected targets.
        let mut req = request(ModerationAction::Warn);
        req.target.as_mut().expect("target").is_guild_owner = true;
        assert_eq!(
            assert_moderation_allowed(&req, &policy()),
            Err(PolicyError::TargetGuildOwner)
        );
        let mut req = request(ModerationAction::Warn);
        req.target.as_mut().expect("target").user_id = OWEN_ID.to_owned();
        assert_eq!(
            assert_moderation_allowed(&req, &policy()),
            Err(PolicyError::TargetOwen)
        );
        let mut req = request(ModerationAction::Warn);
        req.target.as_mut().expect("target").is_bot = true;
        assert_eq!(
            assert_moderation_allowed(&req, &policy()),
            Err(PolicyError::TargetBot)
        );
        let mut req = request(ModerationAction::Warn);
        req.target.as_mut().expect("target").role_ids = vec!["444444444444444444".to_owned()];
        assert_eq!(
            assert_moderation_allowed(&req, &policy()),
            Err(PolicyError::TargetStaffRole)
        );
        // Hierarchy: equal-or-above refused on both sides.
        let mut req = request(ModerationAction::Ban);
        req.bot_highest_role_position = Some(10);
        assert_eq!(
            assert_moderation_allowed(&req, &policy()),
            Err(PolicyError::BotHierarchy)
        );
        let mut req = request(ModerationAction::Ban);
        req.bot_highest_role_position = None;
        req.target.as_mut().expect("target").highest_role_position = 50;
        assert_eq!(
            assert_moderation_allowed(&req, &policy()),
            Err(PolicyError::ActorHierarchy)
        );
    }

    #[test]
    fn target_protection_works_without_permission_context() {
        // TOG-3092: automod deletion checks protection only.
        let mut protected = target();
        protected.role_ids = vec!["444444444444444444".to_owned()];
        assert_eq!(
            moderation_target_protection(&protected, &policy()),
            Some(TargetProtection::StaffRole)
        );
        assert_eq!(moderation_target_protection(&target(), &policy()), None);
    }

    #[test]
    fn reason_validation_trims_and_caps() {
        assert_eq!(require_moderation_reason("  spam  "), Ok("spam".to_owned()));
        assert_eq!(require_moderation_reason("   "), Err(ReasonError::Empty));
        assert_eq!(
            require_moderation_reason(&"x".repeat(513)),
            Err(ReasonError::TooLong)
        );
        assert!(require_moderation_reason(&"x".repeat(512)).is_ok());
        // Legacy JS `length` counts UTF-16 units: 256 astral characters are
        // exactly 512 units (accepted), 257 are 514 (refused). BMP text is
        // unchanged — é is 1 unit either way.
        assert!(require_moderation_reason(&"\u{1F600}".repeat(256)).is_ok());
        assert_eq!(
            require_moderation_reason(&"\u{1F600}".repeat(257)),
            Err(ReasonError::TooLong)
        );
        assert_eq!(
            require_moderation_reason(&"\u{1F600}".repeat(300)),
            Err(ReasonError::TooLong)
        );
    }

    #[test]
    fn gates_default_off() {
        let gates = ModerationGates::from_map(&HashMap::new()).expect("defaults");
        assert!(!gates.enabled);
        assert!(gates.owen_user_id.is_empty());
        assert!(gates.protected_role_ids.is_empty());
    }

    #[test]
    fn gates_enable_validates_owen_but_roles_always() {
        let vars: HashMap<String, String> = [
            ("TWO_MODERATION", "1"),
            ("TWO_OWEN_USER_ID", OWEN_ID),
            (
                "TWO_MODERATION_PROTECTED_ROLE_IDS",
                "444444444444444444, 555555555555555555",
            ),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let gates = ModerationGates::from_map(&vars).expect("parses");
        assert!(gates.enabled);
        assert_eq!(gates.protected_role_ids.len(), 2);

        // Enabled without a valid Owen id is rejected.
        let vars: HashMap<String, String> = [("TWO_MODERATION", "1")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(
            ModerationGates::from_map(&vars),
            Err(ModerationGateError::InvalidOwenUserId)
        );
        // Bad role ids are rejected even while disabled (legacy parity).
        let vars: HashMap<String, String> = [("TWO_MODERATION_PROTECTED_ROLE_IDS", "not-an-id")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(
            ModerationGates::from_map(&vars),
            Err(ModerationGateError::InvalidProtectedRoleId)
        );
    }
}
