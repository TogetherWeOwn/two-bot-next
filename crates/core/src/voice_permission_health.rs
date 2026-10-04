//! Pure V10a permission-health evaluator, written from `docs/voice-rooms.md` §V10 only.
//!
//! The caller supplies guild permission state, category/channel overwrites,
//! bot identity and notice-candidate availability as plain data. This module
//! resolves effective permissions in Discord's order, attributes missing
//! permissions to a guild, category or channel level, picks the first working
//! error-notice destination and bounds notice repeats over timestamps. It
//! performs no I/O, holds no Discord, store or clock types, and does not
//! depend on V1 room lifecycle code.
//!
//! Findings, targets and tracked failures carry enums and numeric IDs only.
//! No name, message text, URL or token can be represented here, so rendering
//! them into a notice or `/setup` listing cannot leak user-provided text.
//! Choosing display wording and sending notices stay with the V10 runtime.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use serde::Serialize;

use crate::health::{VoicePermission, VoicePermissionScope};
use crate::Snowflake;

/// Discord permission bits used by the V10 health check, as plain values so
/// this module stays free of Discord wire types.
pub const PERM_ADMINISTRATOR: u64 = 8;
/// Manage Channels.
pub const PERM_MANAGE_CHANNELS: u64 = 16;
/// View Channel.
pub const PERM_VIEW_CHANNEL: u64 = 1024;
/// Connect.
pub const PERM_CONNECT: u64 = 1 << 20;
/// Move Members.
pub const PERM_MOVE_MEMBERS: u64 = 1 << 24;
/// Manage Roles.
pub const PERM_MANAGE_ROLES: u64 = 1 << 28;

/// The four V10 permissions every creator category and room channel needs.
const REQUIRED: [(VoicePermission, u64); 4] = [
    (VoicePermission::ManageChannels, PERM_MANAGE_CHANNELS),
    (VoicePermission::MoveMembers, PERM_MOVE_MEMBERS),
    (VoicePermission::ManageRoles, PERM_MANAGE_ROLES),
    (VoicePermission::ViewChannel, PERM_VIEW_CHANNEL),
];

/// Who one overwrite row applies to. The caller maps the guild's @everyone
/// role to [`OverwriteTarget::Everyone`]; no ID comparison lives here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwriteTarget {
    Everyone,
    Role(Snowflake),
    Member(Snowflake),
}

/// One permission overwrite row: allow and deny masks for its target. A bit
/// present in both masks is allowed (allow applies after deny).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionOverwrite {
    pub target: OverwriteTarget,
    pub allow: u64,
    pub deny: u64,
}

/// Allow/deny masks for one resolution step. Absent steps use `None`;
/// [`OverwriteMasks::default`] is the neutral mask (allows and denies
/// nothing), also used to accumulate combined role overwrites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OverwriteMasks {
    pub allow: u64,
    pub deny: u64,
}

/// Standard Discord channel resolution: base, then @everyone, then the
/// combined role overwrites, then the member overwrite. Each step applies
/// `(permissions & !deny) | allow`, so within combined roles allow wins over
/// deny. A guild-level Administrator base bypasses every overwrite and
/// yields all bits. Administrator is not a channel permission: an overwrite
/// can neither grant nor revoke it, so its bit is ignored in overwrite masks
/// rather than treated as a bypass that would hide missing permissions.
#[must_use]
pub fn resolve_effective_permissions(
    base: u64,
    everyone: Option<OverwriteMasks>,
    role_overrides: &[OverwriteMasks],
    member: Option<OverwriteMasks>,
) -> u64 {
    if base & PERM_ADMINISTRATOR != 0 {
        return u64::MAX;
    }
    let mut permissions = base;
    if let Some(masks) = everyone {
        permissions = apply(permissions, masks);
    }
    if !role_overrides.is_empty() {
        let mut combined = OverwriteMasks::default();
        for masks in role_overrides {
            combined.allow |= masks.allow;
            combined.deny |= masks.deny;
        }
        permissions = apply(permissions, combined);
    }
    if let Some(masks) = member {
        permissions = apply(permissions, masks);
    }
    permissions
}

fn apply(permissions: u64, masks: OverwriteMasks) -> u64 {
    ((permissions & !masks.deny) | masks.allow) & !PERM_ADMINISTRATOR
}

/// One missing permission attributed to the outermost level responsible for
/// it. Only enums and IDs: safe to render into a notice or `/setup` listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PermissionFinding {
    pub permission: VoicePermission,
    pub scope: VoicePermissionScope,
    /// Set only for category-scope findings: the category whose overwrite
    /// denied the permission.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_id: Option<Snowflake>,
    /// Set only for channel-scope findings: the channel whose overwrite
    /// denied the permission.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<Snowflake>,
}

/// Evaluate the bot's effective permissions on a creator category and one
/// channel inside it.
///
/// As in Discord, the category and the channel each resolve from the guild
/// base with their own overwrite rows; a channel does not stack on its
/// category's overwrites (a synced channel simply carries copies of them).
/// Each required permission missing from either level yields exactly one
/// finding at the outermost responsible level: the guild base lacks it, else
/// the category overwrite removed it (naming `category_id`), else the
/// channel overwrite removed it. Overwrite `allow`s rescue a missing guild
/// base only where they apply, and a guild-level Administrator base yields
/// no findings. Role overwrites apply only for `bot_roles`; member
/// overwrites only for `bot_id`.
#[must_use]
pub fn evaluate_permissions(
    guild_perms: u64,
    category_id: Snowflake,
    category_overrides: &[PermissionOverwrite],
    channel_id: Snowflake,
    channel_overrides: &[PermissionOverwrite],
    bot_id: Snowflake,
    bot_roles: &[Snowflake],
) -> Vec<PermissionFinding> {
    if guild_perms & PERM_ADMINISTRATOR != 0 {
        return Vec::new();
    }
    let (category_everyone, category_roles, category_member) =
        partition(category_overrides, bot_id, bot_roles);
    let (channel_everyone, channel_roles, channel_member) =
        partition(channel_overrides, bot_id, bot_roles);
    let category_effective = resolve_effective_permissions(
        guild_perms,
        category_everyone,
        &category_roles,
        category_member,
    );
    let channel_effective = resolve_effective_permissions(
        guild_perms,
        channel_everyone,
        &channel_roles,
        channel_member,
    );
    let mut findings = Vec::new();
    for (permission, bit) in REQUIRED {
        let category_missing = category_effective & bit == 0;
        if !category_missing && channel_effective & bit != 0 {
            continue;
        }
        let (scope, category, channel) = if guild_perms & bit == 0 {
            (VoicePermissionScope::Guild, None, None)
        } else if category_missing {
            (VoicePermissionScope::Category, Some(category_id), None)
        } else {
            (VoicePermissionScope::Channel, None, Some(channel_id))
        };
        findings.push(PermissionFinding {
            permission,
            scope,
            category_id: category,
            channel_id: channel,
        });
    }
    findings
}

/// Diagnose only the permissions required by a refused write on the actual
/// checked surface. Unlike the general health check, a parent category cannot
/// explain a missing bit on an unsynced channel. The caller supplies the same
/// effective permissions and required mask used by its write gate.
#[must_use]
pub fn evaluate_write_permissions(
    guild_base: u64,
    effective: u64,
    required: u64,
    surface_scope: VoicePermissionScope,
    surface_id: Snowflake,
) -> Vec<PermissionFinding> {
    if guild_base & PERM_ADMINISTRATOR != 0 {
        return Vec::new();
    }
    let permissions = [
        (VoicePermission::ManageChannels, PERM_MANAGE_CHANNELS),
        (VoicePermission::MoveMembers, PERM_MOVE_MEMBERS),
        (VoicePermission::ManageRoles, PERM_MANAGE_ROLES),
        (VoicePermission::ViewChannel, PERM_VIEW_CHANNEL),
        (VoicePermission::Connect, PERM_CONNECT),
    ];
    permissions
        .into_iter()
        .filter(|(_, bit)| required & *bit != 0 && effective & *bit == 0)
        .map(|(permission, bit)| {
            let scope = if guild_base & bit == 0 {
                VoicePermissionScope::Guild
            } else {
                surface_scope
            };
            PermissionFinding {
                permission,
                scope,
                category_id: (scope == VoicePermissionScope::Category).then_some(surface_id),
                channel_id: (scope == VoicePermissionScope::Channel).then_some(surface_id),
            }
        })
        .collect()
}

fn partition(
    overwrites: &[PermissionOverwrite],
    bot_id: Snowflake,
    bot_roles: &[Snowflake],
) -> (
    Option<OverwriteMasks>,
    Vec<OverwriteMasks>,
    Option<OverwriteMasks>,
) {
    let mut everyone = OverwriteMasks::default();
    let mut has_everyone = false;
    let mut roles = Vec::new();
    let mut member = OverwriteMasks::default();
    let mut has_member = false;
    for overwrite in overwrites {
        let masks = OverwriteMasks {
            allow: overwrite.allow,
            deny: overwrite.deny,
        };
        match overwrite.target {
            OverwriteTarget::Everyone => {
                everyone.allow |= masks.allow;
                everyone.deny |= masks.deny;
                has_everyone = true;
            }
            OverwriteTarget::Role(role_id) if bot_roles.contains(&role_id) => {
                roles.push(masks);
            }
            OverwriteTarget::Member(member_id) if member_id == bot_id => {
                member.allow |= masks.allow;
                member.deny |= masks.deny;
                has_member = true;
            }
            OverwriteTarget::Role(_) | OverwriteTarget::Member(_) => {}
        }
    }
    (
        has_everyone.then_some(everyone),
        roles,
        has_member.then_some(member),
    )
}

/// Availability snapshot for the V10 notice fallback chain. `None` IDs and
/// `false` reachability both mean "try the next destination". IDs are numeric
/// only; no names or message text travel here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoticeCandidates {
    pub system_channel_id: Option<Snowflake>,
    pub setup_user_id: Option<Snowflake>,
    pub setup_user_dm_reachable: bool,
    pub owner_id: Option<Snowflake>,
    pub owner_dm_reachable: bool,
    pub creator_channel_id: Option<Snowflake>,
}

/// First working V10 notice destination. `SystemChannel` carries the last
/// setup user for the mention; the adapter drops the mention when it is
/// `None` but still posts to the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum NoticeTarget {
    SystemChannel {
        channel_id: Snowflake,
        #[serde(skip_serializing_if = "Option::is_none")]
        mention_user_id: Option<Snowflake>,
    },
    UserDm {
        user_id: Snowflake,
    },
    CreatorChannel {
        channel_id: Snowflake,
    },
}

/// V10 fallback order: guild system channel, then DM to the last setup user,
/// then DM to the guild owner, then the creator channel's chat. Returns
/// `None` only when no destination is available.
#[must_use]
pub fn notice_target(candidates: NoticeCandidates) -> Option<NoticeTarget> {
    if let Some(channel_id) = candidates.system_channel_id {
        return Some(NoticeTarget::SystemChannel {
            channel_id,
            mention_user_id: candidates.setup_user_id,
        });
    }
    if let (Some(user_id), true) = (candidates.setup_user_id, candidates.setup_user_dm_reachable) {
        return Some(NoticeTarget::UserDm { user_id });
    }
    if let (Some(user_id), true) = (candidates.owner_id, candidates.owner_dm_reachable) {
        return Some(NoticeTarget::UserDm { user_id });
    }
    candidates
        .creator_channel_id
        .map(|channel_id| NoticeTarget::CreatorChannel { channel_id })
}

/// Total sends per tracked failure before notices stop: one initial notice
/// plus two repeats. A repeat is due only after its backoff elapses; after
/// the third send the failure stays listed but silent until resolved.
pub const NOTICE_MAX_SENDS: u32 = 3;
/// Minimum delay since the previous send before repeat `i` (index = sends so
/// far): the first notice goes immediately, the first repeat after 5 minutes,
/// the second after 30 minutes.
pub const NOTICE_BACKOFF_MS: [u64; 3] = [0, 5 * 60 * 1_000, 30 * 60 * 1_000];

const _: () = assert!(NOTICE_BACKOFF_MS.len() == NOTICE_MAX_SENDS as usize);

/// One failure retained for bounded repeats and `/setup` listing. Only enums
/// and IDs; the adapter already dropped all source text when classifying it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct TrackedFailure {
    pub guild_id: Snowflake,
    /// Category, channel or system-channel ID the failure belongs to, when
    /// the diagnostic alone does not locate it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<Snowflake>,
    pub diagnostic: crate::health::VoiceDiagnostic,
}

#[derive(Debug, Clone, Copy)]
struct ThrottleEntry {
    /// Notices actually sent; 0 for a failure observed but not yet sent.
    sends: u32,
    /// Time of the last send; meaningless while `sends` is 0.
    last_sent_ms: u64,
}

/// Pure repeat state over caller-supplied timestamps: "a few times, then
/// stop". The caller owns persistence and supplies `now_ms`. On each health
/// check it [`observe`](Self::observe)s every detected failure, so `/setup`
/// lists it even when no notice target is available or the send fails;
/// records each actual send; and resolves failures the check no longer
/// reports, so a recurrence starts a fresh budget. `current_failures` backs
/// the `/setup` failure list in deterministic (guild, location, diagnostic)
/// order.
#[derive(Debug, Clone, Default)]
pub struct NoticeThrottle {
    entries: BTreeMap<TrackedFailure, ThrottleEntry>,
}

impl NoticeThrottle {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Track a detected failure for `/setup` without counting a send.
    /// Inserts it with no sends when absent, so it is listed and immediately
    /// due; an already tracked failure keeps its send count and timestamp.
    /// Returns true when the failure was newly tracked.
    pub fn observe(&mut self, failure: TrackedFailure) -> bool {
        match self.entries.entry(failure) {
            Entry::Occupied(_) => false,
            Entry::Vacant(slot) => {
                slot.insert(ThrottleEntry {
                    sends: 0,
                    last_sent_ms: 0,
                });
                true
            }
        }
    }

    /// True when a notice for `failure` may be sent at `now_ms`: fewer than
    /// [`NOTICE_MAX_SENDS`] sends so far and the backoff since the previous
    /// send elapsed. Unknown and observed-but-unsent failures are due.
    #[must_use]
    pub fn should_notify(&self, failure: TrackedFailure, now_ms: u64) -> bool {
        let Some(entry) = self.entries.get(&failure) else {
            return true;
        };
        if entry.sends >= NOTICE_MAX_SENDS {
            return false;
        }
        let delay = NOTICE_BACKOFF_MS[entry.sends as usize];
        now_ms.saturating_sub(entry.last_sent_ms) >= delay
    }

    /// Record one sent notice, tracking the failure if it was not observed
    /// first. Late duplicates beyond the budget keep the failure listed but
    /// silent.
    pub fn record_sent(&mut self, failure: TrackedFailure, now_ms: u64) {
        self.entries
            .entry(failure)
            .and_modify(|entry| {
                entry.sends = entry.sends.saturating_add(1).min(NOTICE_MAX_SENDS);
                entry.last_sent_ms = now_ms;
            })
            .or_insert(ThrottleEntry {
                sends: 1,
                last_sent_ms: now_ms,
            });
    }

    /// Drop a resolved failure so a recurrence starts a fresh budget.
    /// Returns true when the failure was tracked.
    pub fn resolve(&mut self, failure: TrackedFailure) -> bool {
        self.entries.remove(&failure).is_some()
    }

    /// Drop every failure for one guild. Returns the number removed.
    pub fn clear_guild(&mut self, guild_id: Snowflake) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|failure, _| failure.guild_id != guild_id);
        before - self.entries.len()
    }

    /// Currently failing entries for `/setup`, in deterministic order.
    #[must_use]
    pub fn current_failures(&self) -> Vec<TrackedFailure> {
        self.entries.keys().copied().collect()
    }
}
