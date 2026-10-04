//! Pure V9 companion text-channel rules derived from `docs/voice-rooms.md`.
//!
//! Given a per-creator toggle plus a room snapshot, this module plans the
//! companion text channel (name, category, initial visibility) and computes
//! join/leave grant diffs. It performs no I/O, uses no Discord or store
//! types, and never touches V1 room lifecycle: creation, persistence,
//! Discord calls and room-deletion effects belong to the runtime behind the
//! V1 edge, which consumes these plans.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::Snowflake;

/// Default companion name when no name is configured (or it sanitises empty).
pub const DEFAULT_TEXT_CHANNEL_NAME: &str = "voice-chat";

/// Discord text-channel names hold at most 100 characters.
pub const MAX_TEXT_CHANNEL_NAME_CHARS: usize = 100;

/// Per-creator text-channel settings. The runtime snapshots these at room
/// creation: later changes only affect channels created afterwards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextChannelSettings {
    /// Per-creator `/textchannels` toggle. Off by default; off yields no plan.
    pub enabled: bool,
    /// Configured channel name. `None` (or blank, or sanitising to nothing)
    /// falls back to [`DEFAULT_TEXT_CHANNEL_NAME`].
    pub configured_name: Option<String>,
    /// The one extra role allowed to view. `None` means occupants and admins
    /// only. `Some(guild_id)` is @everyone: the channel is visible to all.
    pub viewer_role_id: Option<Snowflake>,
}

/// Caller-supplied facts for one room. IDs are Discord snowflakes; a guild's
/// @everyone role uses the guild ID as its role ID.
#[derive(Debug, Clone, Copy)]
pub struct VoiceRoomFacts<'a> {
    pub guild_id: Snowflake,
    pub room_id: Snowflake,
    pub category_id: Snowflake,
    pub occupants: &'a [Snowflake],
    /// Effective admins for this room, who always keep View regardless of
    /// occupancy. The runtime authenticates these; they are facts here.
    pub admin_ids: &'a [Snowflake],
    /// Guild roles holding Manage Channels (V9d AC7), resolved live by the
    /// runtime via [`admin_view_roles`]. Each becomes a `Role` View allow so
    /// later-promoted admins are covered without per-room updates.
    /// Administrator roles and @everyone never appear here: they bypass
    /// overwrites or are already covered by the @everyone entry. Unlike the
    /// per-creator settings snapshot, these are resolved at plan time, so a
    /// role granted Manage Channels later covers both new and existing rooms
    /// for their initial overwrites.
    pub admin_role_ids: &'a [Snowflake],
}

/// Who one overwrite targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OverwriteTarget {
    Everyone,
    Role(Snowflake),
    Member(Snowflake),
}

/// One initial View overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelOverwrite {
    pub target: OverwriteTarget,
    pub allow_view: bool,
    pub deny_view: bool,
}

/// Creation plan plus the settings snapshot it was built from. The runtime
/// stores `settings` with the companion record so later setting changes do
/// not retroactively alter this channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChannelPlan {
    pub room_id: Snowflake,
    pub guild_id: Snowflake,
    pub name: String,
    pub category_id: Snowflake,
    pub overwrites: Vec<ChannelOverwrite>,
    pub settings: TextChannelSettings,
}

impl TextChannelPlan {
    /// True when the viewer role is @everyone: the @everyone overwrite
    /// allows View instead of denying it.
    pub fn visible_to_all(&self) -> bool {
        self.settings.viewer_role_id == Some(self.guild_id)
    }
}

/// Discord permission bits consulted by [`admin_view_roles`], matching the
/// legacy tree's discord.js `PermissionFlagsBits` values.
pub const PERMISSION_BIT_ADMINISTRATOR: u64 = 1 << 3;
pub const PERMISSION_BIT_MANAGE_CHANNELS: u64 = 1 << 4;

/// Resolve which guild roles earn a companion `Role` View allow (V9d AC7):
/// roles holding Manage Channels, minus Administrator roles (they bypass
/// overwrites, so no entry) and minus @everyone (covered by the @everyone
/// entry). Each entry is `(role_id, permission_bits)`; zero IDs are dropped.
/// Output is sorted and deduplicated.
#[must_use]
pub fn admin_view_roles(guild_id: Snowflake, roles: &[(Snowflake, u64)]) -> Vec<Snowflake> {
    let mut out = BTreeSet::new();
    for (role_id, bits) in roles {
        if *role_id == 0 || *role_id == guild_id {
            continue;
        }
        if bits & PERMISSION_BIT_ADMINISTRATOR != 0 {
            continue;
        }
        if bits & PERMISSION_BIT_MANAGE_CHANNELS != 0 {
            out.insert(*role_id);
        }
    }
    out.into_iter().collect()
}

/// Plan the companion text channel, or `None` when the per-creator toggle is
/// off (the default). The companion lives in the room's category; @everyone
/// is denied View unless the viewer role is @everyone. The viewer role and
/// admin roles get `Role` View allows; admins plus current occupants get
/// `Member` allows. Zero IDs are ignored, never emitted; the viewer role is
/// never emitted twice.
pub fn text_channel_plan(
    settings: &TextChannelSettings,
    room: &VoiceRoomFacts<'_>,
) -> Option<TextChannelPlan> {
    if !settings.enabled {
        return None;
    }
    let viewer_is_everyone = settings.viewer_role_id == Some(room.guild_id);
    let mut overwrites = vec![ChannelOverwrite {
        target: OverwriteTarget::Everyone,
        allow_view: viewer_is_everyone,
        deny_view: !viewer_is_everyone,
    }];
    let mut roles = BTreeSet::new();
    if let Some(role_id) = settings.viewer_role_id {
        if role_id != room.guild_id && role_id != 0 {
            roles.insert(role_id);
        }
    }
    roles.extend(
        room.admin_role_ids
            .iter()
            .copied()
            .filter(|id| *id != 0 && *id != room.guild_id),
    );
    for role_id in roles {
        overwrites.push(ChannelOverwrite {
            target: OverwriteTarget::Role(role_id),
            allow_view: true,
            deny_view: false,
        });
    }
    let mut members = BTreeSet::new();
    members.extend(room.admin_ids.iter().copied().filter(|id| *id != 0));
    members.extend(room.occupants.iter().copied().filter(|id| *id != 0));
    for member_id in members {
        overwrites.push(ChannelOverwrite {
            target: OverwriteTarget::Member(member_id),
            allow_view: true,
            deny_view: false,
        });
    }
    Some(TextChannelPlan {
        room_id: room.room_id,
        guild_id: room.guild_id,
        name: sanitise_channel_name(settings.configured_name.as_deref()),
        category_id: room.category_id,
        overwrites,
        settings: settings.clone(),
    })
}

/// Sanitise a configured name to Discord text-channel rules: lowercase, no
/// whitespace (runs collapse to one `-`, edges trimmed), at most 100
/// characters. Codepoints with no lowercase mapping (e.g. U+1D400) survive
/// `to_lowercase` unchanged and still uppercase, so those leftovers are
/// dropped. Blank or empty results fall back to the default name.
pub fn sanitise_channel_name(configured: Option<&str>) -> String {
    let raw = configured.unwrap_or(DEFAULT_TEXT_CHANNEL_NAME);
    let mapped: String = raw
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_uppercase())
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .take(MAX_TEXT_CHANNEL_NAME_CHARS)
        .collect();
    let mut out = String::with_capacity(mapped.len());
    let mut last_dash = false;
    for c in mapped.chars() {
        if c == '-' {
            if !last_dash && !out.is_empty() {
                out.push('-');
            }
            last_dash = true;
        } else {
            out.push(c);
            last_dash = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        DEFAULT_TEXT_CHANNEL_NAME.to_owned()
    } else {
        out
    }
}

/// Join/leave grant diff: members in `after` but not `before` are granted,
/// members in `before` but not `after` are revoked, except `protected`
/// IDs (the viewer role and admins), which are never revoked. Both lists are
/// deduplicated and sorted. An unchanged occupancy yields empty lists.
/// Removing someone from `protected` does not itself revoke them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccupancyDiff {
    pub grants: Vec<Snowflake>,
    pub revokes: Vec<Snowflake>,
}

pub fn occupancy_diff(
    before: &[Snowflake],
    after: &[Snowflake],
    protected: &[Snowflake],
) -> OccupancyDiff {
    let before_set: BTreeSet<Snowflake> = before.iter().copied().filter(|id| *id != 0).collect();
    let after_set: BTreeSet<Snowflake> = after.iter().copied().filter(|id| *id != 0).collect();
    let protected_set: BTreeSet<Snowflake> =
        protected.iter().copied().filter(|id| *id != 0).collect();
    OccupancyDiff {
        grants: after_set.difference(&before_set).copied().collect(),
        revokes: before_set
            .difference(&after_set)
            .filter(|id| !protected_set.contains(*id))
            .copied()
            .collect(),
    }
}

/// A deleted room maps to a delete of its companion text channel. The
/// runtime performs the Discord delete; this only names the room it was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompanionDeletion {
    pub room_id: Snowflake,
}

pub fn plan_companion_deletion(room_id: Snowflake) -> CompanionDeletion {
    CompanionDeletion { room_id }
}
