//! Pure V10b guild-level room command access gate, derived from
//! `docs/voice-rooms.md` only.
//!
//! The caller supplies plain IDs and the effective administrator flag
//! (Manage Channels per the spec's definition of "Admin"); this module
//! authenticates nothing and performs no I/O, persistence or Discord work.
//! Room lifecycle (V1) and owner controls (V2/V3) live outside this slice.

use std::collections::BTreeMap;

use crate::Snowflake;

/// A Discord role ID. Zero is never a valid role.
pub type RoleId = Snowflake;

/// Every voice slash command (without the leading slash) that a guild-level
/// per-command role restriction may name, taken from `docs/voice-rooms.md`.
/// Matching is exact lowercase; anything else is refused by
/// [`validate_access_controls`].
pub const VOICE_COMMANDS: &[&str] = &[
    "alias",
    "alwaysprivate",
    "channelinfo",
    "create",
    "defaultlimit",
    "export",
    "group",
    "import",
    "inheritpermissions",
    "invite",
    "kick",
    "limit",
    "logging",
    "name",
    "nick",
    "ping",
    "position",
    "private",
    "public",
    "reclaim",
    "setup",
    "template",
    "templateassistant",
    "textchannels",
    "transfer",
    "unlimit",
];

/// Guild-level controls for room commands. The creation flag is a global
/// kill-switch; role gates restrict who may use commands. The runtime owns
/// persistence, routing and permission evaluation; these are supplied facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessControls {
    /// When false, joining a creator channel must not create a room.
    /// Commands on existing rooms keep working under [`may_use_command`].
    pub room_creation_enabled: bool,
    /// Optional role required to use any room command. Clearing it lifts the
    /// gate; it never tightens another restriction.
    pub required_role: Option<RoleId>,
    /// Per-command allowed roles. An absent entry means unrestricted (beyond
    /// the guild-wide gate). A present entry with an empty role list denies
    /// every non-admin: fail closed, never fail open on a misconfigured list.
    /// Remove the entry to lift the restriction.
    pub command_roles: BTreeMap<String, Vec<RoleId>>,
}

/// The invoking member. `is_admin` is the caller's effective Manage Channels
/// result for this context; `roles` carries the member's role IDs.
/// Duplicates are harmless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessMember {
    pub is_admin: bool,
    pub roles: Vec<RoleId>,
}

/// Authorization outcome for one command invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDecision {
    Allow,
    Deny(AccessDenyReason),
}

/// Why a command was refused. The order below is also the evaluation order:
/// the guild-wide role gate is checked before any per-command restriction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDenyReason {
    RequiredRole,
    CommandRestricted,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccessError {
    #[error("unknown voice command {0}")]
    UnknownCommand(String),
    #[error("role IDs must be nonzero")]
    InvalidRoleId,
}

/// Whether `command` is an exact entry of [`VOICE_COMMANDS`].
#[must_use]
pub fn is_voice_command(command: &str) -> bool {
    VOICE_COMMANDS.contains(&command)
}

/// Global creation kill-switch. When false, no new rooms are created; command
/// authorization on existing rooms is still decided by [`may_use_command`].
/// Administrators do not bypass this switch: it stops creation for the guild,
/// not for a member.
#[must_use]
pub fn may_create_room(controls: &AccessControls) -> bool {
    controls.room_creation_enabled
}

/// Authorize one command invocation:
/// - an admin is always allowed;
/// - a member without the required role gets `Deny(RequiredRole)`;
/// - a restricted command with no matching role gets `Deny(CommandRestricted)`,
///   including a present-but-empty role list;
/// - otherwise `Allow`.
///
/// Invoked names outside [`VOICE_COMMANDS`] match no per-command entry, so
/// only the guild-wide gate applies to them; the runtime must route genuine
/// voice commands here and rely on [`validate_access_controls`] to refuse
/// misspelled restriction keys before they become silent no-ops.
pub fn may_use_command(
    controls: &AccessControls,
    member: &AccessMember,
    command: &str,
) -> AccessDecision {
    if member.is_admin {
        return AccessDecision::Allow;
    }
    if let Some(required) = controls.required_role {
        if !member.roles.contains(&required) {
            return AccessDecision::Deny(AccessDenyReason::RequiredRole);
        }
    }
    if let Some(allowed) = controls.command_roles.get(command) {
        if !member.roles.iter().any(|role| allowed.contains(role)) {
            return AccessDecision::Deny(AccessDenyReason::CommandRestricted);
        }
    }
    AccessDecision::Allow
}

/// Refuse restriction maps that name unknown commands (exact lowercase match
/// against [`VOICE_COMMANDS`]) or carry zero role IDs. Unknown keys are the
/// dangerous direction: a typo would otherwise leave a command unrestricted
/// while the guild believes it is gated.
pub fn validate_access_controls(controls: &AccessControls) -> Result<(), AccessError> {
    if controls.required_role.is_some_and(|role| role == 0) {
        return Err(AccessError::InvalidRoleId);
    }
    for (command, roles) in &controls.command_roles {
        if !is_voice_command(command) {
            return Err(AccessError::UnknownCommand(command.clone()));
        }
        if roles.iter().any(|role| *role == 0) {
            return Err(AccessError::InvalidRoleId);
        }
    }
    Ok(())
}
