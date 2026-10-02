//! Pure V8a room-permission builder derived from `docs/voice-rooms.md`.
//!
//! Given the override list of the inheritance source (the creator channel by
//! default, the category, or a chosen channel), this module computes the
//! **complete override set a new room is created with**. Overrides go in at
//! creation and are never patched afterwards, so the runtime must pass the
//! returned list to the channel-create call atomically.
//!
//! This module performs no I/O, uses no Discord wire types, stores nothing,
//! and has no dependency on V1 room lifecycle state. All IDs and permission
//! facts are caller-supplied; authentication, guild routing, and the actual
//! channel create belong to the runtime (parent card TOG-10106).
//!
//! Permission model: plain `u64` bitfields with the `PERM_*` constants
//! below, matching Discord's permission bit layout for the bits this builder
//! touches. Only those bits are ever named; every other bit passes through
//! untouched inside copied source overrides.
//!
//! Builder rules, in order:
//!
//! 1. Without Manage Roles the bot cannot legally set any override, so the
//!    plan is [`RoomPermissionPlan::SyncToCategory`], never a list. No other
//!    input is validated on this path.
//! 2. Otherwise the source overrides are copied verbatim, except that each
//!    override is normalized so deny wins over allow on the same target
//!    (`allow &= !deny`). The three [`InheritanceSource`] variants behave
//!    identically here; the caller resolves which channel's overrides to
//!    pass.
//! 3. Private rooms deny Connect to @everyone while preserving whatever View
//!    allow the source had ("keep View" preserves, never creates: a source
//!    with no @everyone entry yields `allow: 0, deny: CONNECT`). A source
//!    View deny is likewise preserved; inheritance never makes a room more
//!    visible than its source.
//! 4. The owner always receives a member override on their own room only:
//!    [`OWNER_ALLOW_BITS`] plus the caller-supplied extra set clamped to
//!    [`OWNER_EXTRA_MASK`]. Neither set can contain Manage Roles or
//!    Administrator, so there is no privilege escalation. A source deny on
//!    the owner still wins over these grants.
//! 5. In private rooms only, the admin-configured required role (which can
//!    never be @everyone) receives View/Connect/Speak
//!    ([`REQUIRED_ROLE_ALLOW_BITS`]) so its members keep basic access
//!    despite the @everyone Connect deny. A source deny still wins.
//! 6. The output holds at most one override per `(id, kind)` target.
//!
//! Security contract, enforced by table and property tests in
//! `crates/core/tests/voice_permissions.rs`: (a) no output override allows a
//! bit the source override for the same target did not allow, except the
//! documented owner bits and, on its own target only, the required-role
//! bits; (b) deny wins over allow for every output target
//! (`allow & deny == 0`); (c) no duplicate targets.

use crate::Snowflake;

// Permission bits touched by this builder.
pub const PERM_ADMINISTRATOR: u64 = 1 << 3;
pub const PERM_MANAGE_CHANNELS: u64 = 1 << 4;
pub const PERM_PRIORITY_SPEAKER: u64 = 1 << 8;
pub const PERM_STREAM: u64 = 1 << 9;
pub const PERM_VIEW_CHANNEL: u64 = 1 << 10;
pub const PERM_CONNECT: u64 = 1 << 20;
pub const PERM_SPEAK: u64 = 1 << 21;
pub const PERM_MUTE_MEMBERS: u64 = 1 << 22;
pub const PERM_DEAFEN_MEMBERS: u64 = 1 << 23;
pub const PERM_MOVE_MEMBERS: u64 = 1 << 24;
pub const PERM_USE_VAD: u64 = 1 << 25;
pub const PERM_MANAGE_ROLES: u64 = 1 << 28;
pub const PERM_MANAGE_EVENTS: u64 = 1 << 33;

/// Exact permission bits granted to the owner on their own room. The owner
/// can always see, join, and be heard in their room (View, Connect, Speak,
/// Stream, voice activity, priority speaker) and can manage it (Manage
/// Channels for rename/limit/privacy, mute/deafen/move for running the
/// room). Deliberately absent: Manage Roles and Administrator, so owning a
/// room never escalates privilege.
pub const OWNER_ALLOW_BITS: u64 = PERM_VIEW_CHANNEL
    | PERM_CONNECT
    | PERM_SPEAK
    | PERM_STREAM
    | PERM_USE_VAD
    | PERM_PRIORITY_SPEAKER
    | PERM_MANAGE_CHANNELS
    | PERM_MUTE_MEMBERS
    | PERM_DEAFEN_MEMBERS
    | PERM_MOVE_MEMBERS;

/// Caller-configured extra owner bits are clamped to this mask: the owner
/// base set plus Manage Events. Manage Roles and Administrator can never
/// pass through, however the caller sets them.
pub const OWNER_EXTRA_MASK: u64 = OWNER_ALLOW_BITS | PERM_MANAGE_EVENTS;

/// Bits granted to the admin-configured required role in private rooms so
/// its members keep basic access despite the @everyone Connect deny.
pub const REQUIRED_ROLE_ALLOW_BITS: u64 = PERM_VIEW_CHANNEL | PERM_CONNECT | PERM_SPEAK;

/// The only bit a private room adds to @everyone: Connect denied, room stays
/// listed.
pub const PRIVATE_EVERYONE_DENY: u64 = PERM_CONNECT;

/// Which channel the new room inherits its overrides from. The pure builder
/// treats all three identically; the caller resolves the override list from
/// the chosen channel. Kept on the input so the runtime decision is explicit
/// and auditable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InheritanceSource {
    CreatorChannel,
    Category,
    ChosenChannel,
}

/// Role or member target of one channel override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverrideKind {
    Role,
    Member,
}

/// One channel permission override: allow/deny bitfields for a single
/// role or member target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelOverride {
    pub id: Snowflake,
    pub kind: OverrideKind,
    pub allow: u64,
    pub deny: u64,
}

/// Complete input for one room creation. IDs are caller-authenticated facts:
/// `owner_id` is the room owner's user ID, `everyone_role_id` the guild's
/// @everyone role, and `required_role` an optional admin-configured role
/// that must keep access to private rooms. `owner_extra_allow` is clamped
/// to [`OWNER_EXTRA_MASK`]; setting escalation bits there is silently
/// dropped, never an error, so a misconfigured admin cannot escalate a room
/// owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomPermissionInput<'a> {
    pub source: InheritanceSource,
    pub source_overrides: &'a [ChannelOverride],
    pub bot_can_manage_roles: bool,
    pub owner_id: Snowflake,
    pub owner_extra_allow: u64,
    pub private: bool,
    pub everyone_role_id: Snowflake,
    pub required_role: Option<Snowflake>,
}

/// The complete creation-time plan for one room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomPermissionPlan {
    /// The bot lacks Manage Roles and cannot legally set overrides; the new
    /// room syncs to its category instead.
    SyncToCategory,
    /// The complete override set to include in the channel-create call.
    /// Never patched afterwards. At most one entry per `(id, kind)`, and
    /// every entry satisfies `allow & deny == 0` (deny wins).
    Overrides(Vec<ChannelOverride>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PermissionPlanError {
    #[error("Owner, @everyone, required-role, and source target ids must be nonzero.")]
    InvalidId,
    #[error("Source override target appears more than once.")]
    DuplicateSourceTarget,
    #[error("The required role cannot be @everyone: its grant would fight the private-room deny.")]
    RequiredRoleIsEveryone,
}

/// Compute the complete override set for one new room. The same inputs
/// always produce the same plan; nothing is mutated or stored.
pub fn plan_room_overrides(
    input: &RoomPermissionInput<'_>,
) -> Result<RoomPermissionPlan, PermissionPlanError> {
    if !input.bot_can_manage_roles {
        return Ok(RoomPermissionPlan::SyncToCategory);
    }
    require_id(input.owner_id)?;
    require_id(input.everyone_role_id)?;
    if let Some(required) = input.required_role {
        require_id(required)?;
        if required == input.everyone_role_id {
            return Err(PermissionPlanError::RequiredRoleIsEveryone);
        }
    }
    for source in input.source_overrides {
        require_id(source.id)?;
    }
    let mut out: Vec<ChannelOverride> = Vec::with_capacity(input.source_overrides.len() + 2);
    for source in input.source_overrides {
        if out
            .iter()
            .any(|o| o.id == source.id && o.kind == source.kind)
        {
            return Err(PermissionPlanError::DuplicateSourceTarget);
        }
        out.push(normalized(*source));
    }

    if input.private {
        upsert(
            &mut out,
            input.everyone_role_id,
            OverrideKind::Role,
            0,
            PRIVATE_EVERYONE_DENY,
        );
        if let Some(required) = input.required_role {
            upsert(
                &mut out,
                required,
                OverrideKind::Role,
                REQUIRED_ROLE_ALLOW_BITS,
                0,
            );
        }
    }
    upsert(
        &mut out,
        input.owner_id,
        OverrideKind::Member,
        OWNER_ALLOW_BITS | (input.owner_extra_allow & OWNER_EXTRA_MASK),
        0,
    );
    Ok(RoomPermissionPlan::Overrides(out))
}

fn require_id(id: Snowflake) -> Result<(), PermissionPlanError> {
    if id == 0 {
        Err(PermissionPlanError::InvalidId)
    } else {
        Ok(())
    }
}

/// Deny wins over allow for the same target.
fn normalized(mut entry: ChannelOverride) -> ChannelOverride {
    entry.allow &= !entry.deny;
    entry
}

/// Merge one grant into the output: OR allow and deny into any existing
/// entry for the target (creating it unless the merged result is empty),
/// then re-normalize so deny wins. Merging rather than pushing is what keeps
/// the output duplicate-free.
fn upsert(
    out: &mut Vec<ChannelOverride>,
    id: Snowflake,
    kind: OverrideKind,
    allow: u64,
    deny: u64,
) {
    match out.iter_mut().find(|o| o.id == id && o.kind == kind) {
        Some(existing) => {
            existing.allow |= allow;
            existing.deny |= deny;
            *existing = normalized(*existing);
        }
        None => {
            let merged = normalized(ChannelOverride {
                id,
                kind,
                allow,
                deny,
            });
            if merged.allow != 0 || merged.deny != 0 {
                out.push(merged);
            }
        }
    }
}
