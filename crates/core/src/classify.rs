//! Staff audit-sink classifiers (TOG-9810 S5).
//!
//! Ports the pure classification halves of the legacy audit sink, decoupled
//! from the Discord and store wiring that lands in a later slice:
//! - member-update: role/nickname delta + `entryId` digest
//!   (`src/discord/client.ts` `GuildMemberUpdate` handler)
//! - voice join/leave/move (`VoiceStateUpdate` handler; mute/deafen/camera
//!   frames are not session boundaries)
//! - raw-message packets `MESSAGE_UPDATE`/`MESSAGE_DELETE` without a gateway
//!   connection (`src/audit/discordEvents.ts` `rawMessageAuditEvent`)
//! - audit-log action table plus the correlated moderation row
//!   (`src/audit/discordEvents.ts` `MODERATION_ACTIONS` +
//!   `moderationAuditEvent`)
//!
//! Everything here is framework-free: snowflakes are strings (legacy role ids
//! sort lexicographically), instants are caller-supplied ISO strings or epoch
//! millis, and the sink/store/mirror stay behind the caller. Privacy rule
//! (legacy `formatAuditEvent`): classifiers emit IDs, counts and flags only —
//! never message content or display names.

use serde::{Deserialize, Serialize};

use sha2::{Digest, Sha256};

use base64::Engine as _;

use super::audit::AuditEvent;
use super::audit::AuditKind;
use super::mac::{
    is_auditable_action, is_channel_action, moderation_audit_entry_id, outcome_for,
    parse_moderation_audit_reason,
};

/// Discord audit-log event ids for the moderation action table. These are the
/// discord.js `AuditLogEvent` discriminants legacy matches on — plain wire
/// numbers, not positions in any client enum.
///
/// Caution for the twilight adapter: twilight-model 0.17.1's `From<u16>` for
/// `AuditLogEventType` maps member-disconnect to 17 (a copy-paste slip; the
/// `From<AuditLogEventType> for u16` direction correctly maps it to 27 and
/// `BotAdd` to 28). Match on the `u16` numbers here, never on a twilight
/// enum round-trip.
pub const AUDIT_LOG_MEMBER_KICK: u16 = 20;
pub const AUDIT_LOG_MEMBER_PRUNE: u16 = 21;
pub const AUDIT_LOG_MEMBER_BAN_ADD: u16 = 22;
pub const AUDIT_LOG_MEMBER_BAN_REMOVE: u16 = 23;
pub const AUDIT_LOG_MEMBER_UPDATE: u16 = 24;
pub const AUDIT_LOG_MEMBER_ROLE_UPDATE: u16 = 25;
pub const AUDIT_LOG_MEMBER_MOVE: u16 = 26;
pub const AUDIT_LOG_MEMBER_DISCONNECT: u16 = 27;
pub const AUDIT_LOG_MESSAGE_DELETE: u16 = 72;
pub const AUDIT_LOG_MESSAGE_BULK_DELETE: u16 = 73;
pub const AUDIT_LOG_CHANNEL_UPDATE: u16 = 11;
pub const AUDIT_LOG_CHANNEL_OVERWRITE_CREATE: u16 = 13;
pub const AUDIT_LOG_CHANNEL_OVERWRITE_UPDATE: u16 = 14;
pub const AUDIT_LOG_CHANNEL_OVERWRITE_DELETE: u16 = 15;

/// Legacy action names for the moderation audit-log table
/// (`src/audit/discordEvents.ts` `MODERATION_ACTIONS` values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModerationAuditAction {
    MemberKick,
    MemberPrune,
    MemberBan,
    MemberUnban,
    MemberUpdate,
    MemberRoleUpdate,
    MemberMove,
    MemberDisconnect,
    MessageDelete,
    MessageBulkDelete,
    ChannelUpdate,
    ChannelOverwriteCreate,
    ChannelOverwriteUpdate,
    ChannelOverwriteDelete,
}

impl ModerationAuditAction {
    /// All fourteen table rows in legacy map order.
    pub const ALL: [Self; 14] = [
        Self::MemberKick,
        Self::MemberPrune,
        Self::MemberBan,
        Self::MemberUnban,
        Self::MemberUpdate,
        Self::MemberRoleUpdate,
        Self::MemberMove,
        Self::MemberDisconnect,
        Self::MessageDelete,
        Self::MessageBulkDelete,
        Self::ChannelUpdate,
        Self::ChannelOverwriteCreate,
        Self::ChannelOverwriteUpdate,
        Self::ChannelOverwriteDelete,
    ];

    /// Legacy action string (what lands in the row's `action` field).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MemberKick => "member_kick",
            Self::MemberPrune => "member_prune",
            Self::MemberBan => "member_ban",
            Self::MemberUnban => "member_unban",
            Self::MemberUpdate => "member_update",
            Self::MemberRoleUpdate => "member_role_update",
            Self::MemberMove => "member_move",
            Self::MemberDisconnect => "member_disconnect",
            Self::MessageDelete => "message_delete",
            Self::MessageBulkDelete => "message_bulk_delete",
            Self::ChannelUpdate => "channel_update",
            Self::ChannelOverwriteCreate => "channel_overwrite_create",
            Self::ChannelOverwriteUpdate => "channel_overwrite_update",
            Self::ChannelOverwriteDelete => "channel_overwrite_delete",
        }
    }

    /// Discord audit-log event id for this action.
    #[must_use]
    pub fn audit_log_event_id(self) -> u16 {
        match self {
            Self::MemberKick => AUDIT_LOG_MEMBER_KICK,
            Self::MemberPrune => AUDIT_LOG_MEMBER_PRUNE,
            Self::MemberBan => AUDIT_LOG_MEMBER_BAN_ADD,
            Self::MemberUnban => AUDIT_LOG_MEMBER_BAN_REMOVE,
            Self::MemberUpdate => AUDIT_LOG_MEMBER_UPDATE,
            Self::MemberRoleUpdate => AUDIT_LOG_MEMBER_ROLE_UPDATE,
            Self::MemberMove => AUDIT_LOG_MEMBER_MOVE,
            Self::MemberDisconnect => AUDIT_LOG_MEMBER_DISCONNECT,
            Self::MessageDelete => AUDIT_LOG_MESSAGE_DELETE,
            Self::MessageBulkDelete => AUDIT_LOG_MESSAGE_BULK_DELETE,
            Self::ChannelUpdate => AUDIT_LOG_CHANNEL_UPDATE,
            Self::ChannelOverwriteCreate => AUDIT_LOG_CHANNEL_OVERWRITE_CREATE,
            Self::ChannelOverwriteUpdate => AUDIT_LOG_CHANNEL_OVERWRITE_UPDATE,
            Self::ChannelOverwriteDelete => AUDIT_LOG_CHANNEL_OVERWRITE_DELETE,
        }
    }

    /// Look up an action by its Discord audit-log event id. Returns `None`
    /// for event types the sink does not record (the adapter drops those).
    #[must_use]
    pub fn from_audit_log_event_id(id: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.audit_log_event_id() == id)
    }
}

/// Voice presence boundary: join, leave, or move between voice channels.
/// Mute/deafen/camera/go-live frames never reach this type — the adapter
/// drops them when the channel is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceBoundary {
    Join {
        channel_id: u64,
    },
    /// Left voice; carries the channel that was left (legacy rows record both
    /// the key's `none` half and the source field — see
    /// [`classify_voice_boundary`]).
    Leave {
        channel_id: u64,
    },
    Move {
        from_channel_id: u64,
        to_channel_id: u64,
    },
}

impl VoiceBoundary {
    /// Classify from the before/after channel pair. `None` on both sides is
    /// not a boundary (the adapter must have dropped the frame); anything
    /// else is join, leave, or move.
    #[must_use]
    pub fn classify(old_channel_id: Option<u64>, new_channel_id: Option<u64>) -> Option<Self> {
        match (old_channel_id, new_channel_id) {
            (None, None) => None,
            (None, Some(to)) => Some(Self::Join { channel_id: to }),
            (Some(from), None) => Some(Self::Leave { channel_id: from }),
            (Some(from), Some(to)) if from != to => Some(Self::Move {
                from_channel_id: from,
                to_channel_id: to,
            }),
            // Same channel on both sides: not a boundary.
            (Some(_), Some(_)) => None,
        }
    }

    #[must_use]
    pub fn kind(self) -> AuditKind {
        match self {
            Self::Join { .. } => AuditKind::VoiceJoin,
            Self::Leave { .. } => AuditKind::VoiceLeave,
            Self::Move { .. } => AuditKind::VoiceMove,
        }
    }
}

/// Member-update delta: what changed, as role-id sets plus a nickname flag.
/// Nicknames themselves never enter the row — only whether one changed. Role
/// ids are strings and sort lexicographically (legacy `[...set].sort()` on
/// string snowflakes — `"10"` sorts before `"9"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberDelta {
    pub nickname_changed: bool,
    pub added_role_ids: Vec<String>,
    pub removed_role_ids: Vec<String>,
}

impl MemberDelta {
    /// Diff two role sets. Inputs are sorted lexicographically and deduped so
    /// the digest is stable regardless of the order Discord listed the roles
    /// in.
    #[must_use]
    pub fn diff(
        nickname_changed: bool,
        old_roles: &[String],
        new_roles: &[String],
    ) -> Option<Self> {
        let mut old: Vec<&str> = old_roles.iter().map(String::as_str).collect();
        let mut new: Vec<&str> = new_roles.iter().map(String::as_str).collect();
        old.sort_unstable();
        old.dedup();
        new.sort_unstable();
        new.dedup();
        let mut added: Vec<String> = new
            .iter()
            .filter(|r| !old.contains(r))
            .map(ToString::to_string)
            .collect();
        let mut removed: Vec<String> = old
            .iter()
            .filter(|r| !new.contains(r))
            .map(ToString::to_string)
            .collect();
        added.sort_unstable();
        removed.sort_unstable();
        if !nickname_changed && added.is_empty() && removed.is_empty() {
            return None;
        }
        Some(Self {
            nickname_changed,
            added_role_ids: added,
            removed_role_ids: removed,
        })
    }

    /// The `JSON.stringify({ nicknameChanged, addedRoleIds, removedRoleIds })`
    /// body legacy hashes (insertion order, double-quoted strings).
    fn digest_body(&self) -> String {
        format!(
            "{{\"nicknameChanged\":{},\"addedRoleIds\":[{}],\"removedRoleIds\":[{}]}}",
            self.nickname_changed,
            quoted_list(&self.added_role_ids),
            quoted_list(&self.removed_role_ids),
        )
    }

    /// Bounded change digest for the `entryId` (legacy: sha256 of the JSON
    /// body, base64url, first 16 chars — distinguishes simultaneous deltas
    /// without embedding an unbounded role list in the key).
    #[must_use]
    pub fn change_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.digest_body().as_bytes());
        let hash = hasher.finalize();
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);
        encoded[..16].to_owned()
    }
}

fn quoted_list(ids: &[String]) -> String {
    ids.iter()
        .map(|id| serde_json::to_string(id).expect("string serializes"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Build the member-update audit row, or `None` when a partial old member
/// makes the baseline untrustworthy (legacy: `if (oldMember.partial) return`)
/// or nothing changed. Rollup rows share one `entry_id` namespace with the
/// per-member rows (legacy `member-update:` with a hyphen).
#[must_use]
pub fn classify_member_update(
    guild_id: u64,
    member_id: u64,
    delta: Option<MemberDelta>,
    occurred_at: &str,
    old_partial: bool,
) -> Option<AuditEvent> {
    if old_partial {
        return None;
    }
    let delta = delta?;
    let entry_id = format!(
        "member-update:{guild_id}:{member_id}:{occurred_at}:{}",
        delta.change_digest()
    );
    let mut event = AuditEvent::new(
        entry_id,
        AuditKind::MemberUpdate,
        guild_id.to_string(),
        occurred_at.to_owned(),
    );
    event.target_id = Some(member_id.to_string());
    // Same key order as the digest body (legacy object literal order).
    event.metadata_json = delta.digest_body();
    Some(event)
}

/// Build the voice audit row for one channel boundary. `at` is a single
/// instant for both halves of a move (legacy takes one `nowIso()` so an
/// end(A)/start(B) pair never looks like a gap). `is_bot` travels in
/// metadata (legacy `{ isBot }`).
///
/// Legacy entryId: `{voiceKind}:{guildId}:{memberId}:{old ?? 'none'}:{new ??
/// 'none'}:{at}` — a leave keys on the channel it left (`old`), with `none`
/// only for the never-joined half. The row's `source_channel_id` carries the
/// old channel and `destination_channel_id` the new one, exactly like the
/// legacy handler's `sourceChannelId: oldState.channelId` /
/// `destinationChannelId: newState.channelId` (null on the absent half).
#[must_use]
pub fn classify_voice_boundary(
    guild_id: u64,
    member_id: u64,
    boundary: VoiceBoundary,
    at: &str,
    is_bot: bool,
) -> AuditEvent {
    let (kind, kind_name, old, new) = match boundary {
        VoiceBoundary::Join { channel_id } => {
            (AuditKind::VoiceJoin, "voice_join", None, Some(channel_id))
        }
        VoiceBoundary::Leave { channel_id } => {
            (AuditKind::VoiceLeave, "voice_leave", Some(channel_id), None)
        }
        VoiceBoundary::Move {
            from_channel_id,
            to_channel_id,
        } => (
            AuditKind::VoiceMove,
            "voice_move",
            Some(from_channel_id),
            Some(to_channel_id),
        ),
    };
    let entry_id = format!(
        "{kind_name}:{guild_id}:{member_id}:{}:{}:{at}",
        old.map_or_else(|| "none".to_owned(), |c| c.to_string()),
        new.map_or_else(|| "none".to_owned(), |c| c.to_string()),
    );
    let mut event = AuditEvent::new(entry_id, kind, guild_id.to_string(), at.to_owned());
    event.target_id = Some(member_id.to_string());
    event.source_channel_id = old.map(|c| c.to_string());
    event.destination_channel_id = new.map(|c| c.to_string());
    event.metadata_json = serde_json::json!({ "isBot": is_bot }).to_string();
    event
}

/// Raw gateway dispatch fields the message classifier needs (framework-free
/// view of the `op`/`t`/`s`/`d` packet; the adapter extracts these from the
/// twilight frame without handing us twilight types).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDispatch<'a> {
    pub op: u8,
    pub event_type: Option<&'a str>,
    pub sequence: Option<u64>,
    pub shard_id: u32,
    pub guild_id: Option<&'a str>,
    pub channel_id: Option<&'a str>,
    pub message_id: Option<&'a str>,
    pub author_id: Option<&'a str>,
    pub edited_timestamp: Option<&'a str>,
}

/// Classify a raw dispatch into a message audit row, or `None` when the
/// packet is not a guild message update/delete (legacy
/// `rawMessageAuditEvent`). Privacy: edits record `edited_timestamp` (or the
/// shard/sequence fallback) as the identity — never the body. A normalized
/// timestamp travels in the row (legacy `validIso` returns
/// `toISOString()`); an unparseable one falls back to the shard/sequence
/// identity.
#[must_use]
pub fn classify_raw_message(packet: &RawDispatch<'_>, observed_at: &str) -> Option<AuditEvent> {
    if packet.op != 0 {
        return None;
    }
    let guild_id = packet.guild_id.filter(|s| !s.is_empty())?;
    let channel_id = packet.channel_id.filter(|s| !s.is_empty())?;
    let message_id = packet.message_id.filter(|s| !s.is_empty())?;
    match packet.event_type {
        Some("MESSAGE_DELETE") => {
            let mut event = AuditEvent::new(
                format!("message-delete:{guild_id}:{message_id}"),
                AuditKind::MessageDelete,
                guild_id.to_owned(),
                observed_at.to_owned(),
            );
            event.source_channel_id = Some(channel_id.to_owned());
            event.message_id = Some(message_id.to_owned());
            Some(event)
        }
        Some("MESSAGE_UPDATE") => {
            let edited_at = packet.edited_timestamp.and_then(normalize_iso);
            let identity = edited_at.clone().unwrap_or_else(|| {
                format!(
                    "shard-{}:sequence-{}",
                    packet.shard_id,
                    packet
                        .sequence
                        .map_or_else(|| "unknown".to_owned(), |s| s.to_string())
                )
            });
            let mut event = AuditEvent::new(
                format!("message-edit:{guild_id}:{message_id}:{identity}"),
                AuditKind::MessageEdit,
                guild_id.to_owned(),
                edited_at.unwrap_or_else(|| observed_at.to_owned()),
            );
            event.source_channel_id = Some(channel_id.to_owned());
            event.message_id = Some(message_id.to_owned());
            event.actor_id = packet
                .author_id
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            event.target_id = event.actor_id.clone();
            Some(event)
        }
        _ => None,
    }
}

/// Parse an ISO-8601 instant and normalize it to `toISOString()` shape
/// (`YYYY-MM-DDTHH:mm:ss.sssZ`, always millis, always UTC) — what legacy
/// `validIso` returns. Returns `None` for unparseable input.
///
/// Deliberate narrowing: legacy used `Date.parse`, which also accepts
/// non-ISO shapes (`2026/02/01`, `Feb 1 2026`). The port accepts RFC-3339 /
/// ISO-8601 with an explicit zone — everything Discord emits — and anything
/// else takes the shard/sequence fallback identity, same as a missing
/// timestamp.
fn normalize_iso(value: &str) -> Option<String> {
    use time::format_description::well_known::Rfc3339;
    let parsed = time::OffsetDateTime::parse(value, &Rfc3339).ok()?;
    Some(format_iso_millis(parsed))
}

/// Format an instant exactly like JS `toISOString()`: UTC, zero-padded,
/// always three millis digits.
fn format_iso_millis(dt: time::OffsetDateTime) -> String {
    let utc = dt.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
        utc.millisecond(),
    )
}

/// Format epoch millis like JS `new Date(ms).toISOString()`.
fn iso_from_millis(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map(format_iso_millis)
        .unwrap_or_else(|_| "1970-01-01T00:00:00.000Z".to_owned())
}

/// Framework-free view of one Discord audit-log entry: the fields legacy
/// `moderationAuditEvent` reads off `GuildAuditLogsEntry`. The adapter fills
/// this from the twilight/discord.js types without handing them in.
#[derive(Debug, Clone, PartialEq)]
pub struct RawAuditLogEntry {
    /// Discord audit-log event id (the `MODERATION_ACTIONS` key).
    pub action_id: u16,
    /// Discord audit-log entry id.
    pub log_entry_id: String,
    /// Executor snowflake (string).
    pub executor_id: Option<String>,
    /// Target snowflake (string); Discord may omit it.
    pub target_id: Option<String>,
    /// `X-Audit-Log-Reason` — possibly carrying our `[two-audit:v1:…]` marker.
    pub reason: Option<String>,
    /// `entry.createdTimestamp` (epoch millis).
    pub created_timestamp_ms: i64,
    /// `entry.extra.channel.id` when present and a string.
    pub extra_channel_id: Option<String>,
    /// `entry.extra.count` / `entry.extra.removed` for the affected count.
    pub extra_count: Option<serde_json::Value>,
    pub extra_removed: Option<serde_json::Value>,
}

/// Coerce an audit-log `extra` count like legacy `numberOrNull`
/// (`Number(value)`, finite only). Accepts numbers, numeric strings and
/// booleans; anything else (objects, arrays, unparseable text) is `None`.
fn number_or_null(value: Option<&serde_json::Value>) -> Option<f64> {
    let value = value?;
    let n = match value {
        serde_json::Value::Null => 0.0,
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::Bool(b) => f64::from(*b as u8),
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse::<f64>().ok()?
            }
        }
        _ => return None,
    };
    n.is_finite().then_some(n)
}

/// Render a coerced count the way `JSON.stringify` would: integral floats
/// render without a decimal point (`5`, not `5.0`).
fn count_json(n: f64) -> serde_json::Value {
    if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
        serde_json::Value::from(n as i64)
    } else {
        serde_json::Value::from(n)
    }
}

/// Build the moderation-action audit row for one audit-log entry (legacy
/// `moderationAuditEvent`), or `None` for event types the sink does not
/// record. A marker that verifies *and* names this bot as executor correlates
/// the row to the in-process moderation service (`moderation-success:` entry
/// id, `moderation.*` action, `origin: moderation_service` + outcome); every
/// other shape stays an uncorrelated `discord-audit:` row.
#[must_use]
pub fn classify_moderation_audit(
    entry: &RawAuditLogEntry,
    guild_id: &str,
    bot_user_id: Option<&str>,
    moderation_audit_secret: Option<&str>,
) -> Option<AuditEvent> {
    let table_action = ModerationAuditAction::from_audit_log_event_id(entry.action_id)?;
    let marker =
        parse_moderation_audit_reason(moderation_audit_secret, guild_id, entry.reason.as_deref());
    let correlated = marker.filter(|m| {
        bot_user_id.is_some_and(|bot| !bot.is_empty() && entry.executor_id.as_deref() == Some(bot))
            && is_auditable_action(&m.action)
    });

    let count = number_or_null(
        entry
            .extra_count
            .as_ref()
            .filter(|v| !v.is_null())
            .or(entry.extra_removed.as_ref()),
    );
    let correlated_channel = correlated.as_ref().and_then(|m| {
        if is_channel_action(&m.action) {
            entry.target_id.clone()
        } else {
            None
        }
    });

    let mut event = AuditEvent::new(
        correlated.as_ref().map_or_else(
            || format!("discord-audit:{guild_id}:{}", entry.log_entry_id),
            |m| moderation_audit_entry_id(guild_id, &m.token),
        ),
        AuditKind::ModerationAction,
        guild_id.to_owned(),
        iso_from_millis(entry.created_timestamp_ms),
    );
    event.actor_id = correlated
        .as_ref()
        .map_or_else(|| entry.executor_id.clone(), |m| Some(m.actor_id.clone()));
    event.target_id = if correlated_channel.is_some() {
        None
    } else {
        entry.target_id.clone()
    };
    event.source_channel_id = entry.extra_channel_id.clone().or(correlated_channel);
    event.action = Some(
        correlated
            .as_ref()
            .map_or_else(|| table_action.as_str().to_owned(), |m| m.action.clone()),
    );
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "auditLogEntryId".to_owned(),
        serde_json::Value::String(entry.log_entry_id.clone()),
    );
    metadata.insert(
        "count".to_owned(),
        count.map_or(serde_json::Value::Null, count_json),
    );
    if let Some(marker) = correlated.as_ref() {
        metadata.insert(
            "origin".to_owned(),
            serde_json::Value::String("moderation_service".to_owned()),
        );
        metadata.insert(
            "outcome".to_owned(),
            serde_json::Value::String(outcome_for(&marker.action).to_owned()),
        );
        if let Some(n) = count {
            metadata.insert("affected".to_owned(), count_json(n));
        }
    }
    event.metadata_json = serde_json::Value::Object(metadata).to_string();
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac::moderation_audit_reason;

    const SECRET: &str = "test-secret-at-least-32-chars-long!!";
    const GUILD: &str = "123456789012345678";
    const ACTOR: &str = "987654321098765432";

    #[test]
    fn audit_log_table_matches_discord_numbers() {
        // Spot-check the wire ids that drift most often.
        assert_eq!(ModerationAuditAction::MemberKick.audit_log_event_id(), 20);
        assert_eq!(
            ModerationAuditAction::MemberDisconnect.audit_log_event_id(),
            27
        );
        assert_eq!(
            ModerationAuditAction::MessageDelete.audit_log_event_id(),
            72
        );
        assert_eq!(
            ModerationAuditAction::MessageBulkDelete.audit_log_event_id(),
            73
        );
        assert_eq!(
            ModerationAuditAction::ChannelUpdate.audit_log_event_id(),
            11
        );
        assert_eq!(
            ModerationAuditAction::ChannelOverwriteDelete.audit_log_event_id(),
            15
        );
        // Round-trip every row.
        for action in ModerationAuditAction::ALL {
            assert_eq!(
                ModerationAuditAction::from_audit_log_event_id(action.audit_log_event_id()),
                Some(action),
                "{} must resolve",
                action.as_str()
            );
        }
        assert_eq!(ModerationAuditAction::from_audit_log_event_id(1), None);
        assert_eq!(ModerationAuditAction::from_audit_log_event_id(28), None);
    }

    #[test]
    fn voice_boundary_classification() {
        assert_eq!(
            VoiceBoundary::classify(None, Some(10)),
            Some(VoiceBoundary::Join { channel_id: 10 })
        );
        assert_eq!(
            VoiceBoundary::classify(Some(10), None),
            Some(VoiceBoundary::Leave { channel_id: 10 })
        );
        assert_eq!(
            VoiceBoundary::classify(Some(10), Some(11)),
            Some(VoiceBoundary::Move {
                from_channel_id: 10,
                to_channel_id: 11
            })
        );
        // Mute/deafen/camera frames: same channel is not a boundary.
        assert_eq!(VoiceBoundary::classify(Some(10), Some(10)), None);
        assert_eq!(VoiceBoundary::classify(None, None), None);
    }

    #[test]
    fn voice_row_shapes_match_legacy() {
        let join =
            classify_voice_boundary(1, 2, VoiceBoundary::Join { channel_id: 10 }, "AT", false);
        assert_eq!(join.kind, AuditKind::VoiceJoin);
        assert_eq!(join.entry_id, "voice_join:1:2:none:10:AT");
        assert_eq!(join.target_id.as_deref(), Some("2"));
        assert_eq!(join.source_channel_id, None);
        assert_eq!(join.destination_channel_id.as_deref(), Some("10"));

        // Leave keys on the channel it left; the row carries it as source.
        let leave =
            classify_voice_boundary(1, 2, VoiceBoundary::Leave { channel_id: 10 }, "AT", true);
        assert_eq!(leave.kind, AuditKind::VoiceLeave);
        assert_eq!(leave.entry_id, "voice_leave:1:2:10:none:AT");
        assert_eq!(leave.source_channel_id.as_deref(), Some("10"));
        assert_eq!(leave.destination_channel_id, None);

        let mv = classify_voice_boundary(
            1,
            2,
            VoiceBoundary::Move {
                from_channel_id: 10,
                to_channel_id: 11,
            },
            "AT",
            false,
        );
        assert_eq!(mv.kind, AuditKind::VoiceMove);
        assert_eq!(mv.entry_id, "voice_move:1:2:10:11:AT");
        assert_eq!(mv.source_channel_id.as_deref(), Some("10"));
        assert_eq!(mv.destination_channel_id.as_deref(), Some("11"));
    }

    #[test]
    fn member_update_row_matches_legacy() {
        let delta = MemberDelta::diff(
            true,
            &["1".to_owned(), "2".to_owned()],
            &["2".to_owned(), "3".to_owned()],
        )
        .expect("changed");
        assert_eq!(delta.added_role_ids, ["3"]);
        assert_eq!(delta.removed_role_ids, ["1"]);
        // Golden: node sha256('{"nicknameChanged":true,"addedRoleIds":["3"],"removedRoleIds":["1"]}') base64url[..16].
        assert_eq!(delta.change_digest(), "XOV6IYbl3Fu1_ixR");
        // Lexicographic string sort: "10" before "9", like JS [...set].sort().
        let lex =
            MemberDelta::diff(true, &[], &["9".to_owned(), "10".to_owned()]).expect("changed");
        assert_eq!(lex.added_role_ids, ["10", "9"]);
        assert_eq!(lex.change_digest(), "eG0n08KYgOucLcMM");

        let event = classify_member_update(1, 2, Some(delta), "2026-01-01T00:00:00.000Z", false)
            .expect("row");
        // Hyphenated namespace, per legacy `member-update:`.
        assert!(event
            .entry_id
            .starts_with("member-update:1:2:2026-01-01T00:00:00.000Z:"));
        assert_eq!(event.kind, AuditKind::MemberUpdate);
        // Privacy: only the change flag and role-id sets travel — the key
        // names are legacy (`nicknameChanged`); no display name may appear.
        assert!(event.metadata_json.contains("\"nicknameChanged\":true"));
        assert!(
            !event.metadata_json.contains("CoolNick"),
            "display names never enter rows"
        );

        // No change, or a partial baseline: no row.
        assert_eq!(
            MemberDelta::diff(false, &["1".to_owned()], &["1".to_owned()]),
            None
        );
        assert_eq!(
            classify_member_update(
                1,
                2,
                MemberDelta::diff(true, &["1".to_owned()], &["2".to_owned()]),
                "AT",
                true
            ),
            None,
            "partial old member must not report current roles as granted"
        );
    }

    #[test]
    fn raw_message_classifier_matches_legacy() {
        let delete = RawDispatch {
            op: 0,
            event_type: Some("MESSAGE_DELETE"),
            sequence: Some(7),
            shard_id: 0,
            guild_id: Some("1"),
            channel_id: Some("2"),
            message_id: Some("3"),
            author_id: None,
            edited_timestamp: None,
        };
        let event = classify_raw_message(&delete, "OBS").expect("delete row");
        assert_eq!(event.entry_id, "message-delete:1:3");
        assert_eq!(event.kind, AuditKind::MessageDelete);

        let edit = RawDispatch {
            event_type: Some("MESSAGE_UPDATE"),
            edited_timestamp: Some("2026-02-01T00:00:00.000Z"),
            author_id: Some("9"),
            ..delete
        };
        let event = classify_raw_message(&edit, "OBS").expect("edit row");
        assert_eq!(event.entry_id, "message-edit:1:3:2026-02-01T00:00:00.000Z");
        assert_eq!(event.actor_id.as_deref(), Some("9"));

        // Offsets normalize to toISOString shape (legacy validIso).
        let edit_offset = RawDispatch {
            edited_timestamp: Some("2026-02-01T00:00:00+00:00"),
            ..edit
        };
        let event = classify_raw_message(&edit_offset, "OBS").expect("offset row");
        assert_eq!(event.entry_id, "message-edit:1:3:2026-02-01T00:00:00.000Z");
        assert_eq!(event.occurred_at, "2026-02-01T00:00:00.000Z");

        // No edited timestamp: shard/sequence fallback identity.
        let edit_nots = RawDispatch {
            edited_timestamp: Some("not-a-date"),
            ..edit
        };
        let event = classify_raw_message(&edit_nots, "OBS").expect("fallback row");
        assert_eq!(event.entry_id, "message-edit:1:3:shard-0:sequence-7");

        // Non-dispatch op, unknown type, DMs: no row.
        assert_eq!(
            classify_raw_message(&RawDispatch { op: 10, ..delete }, "OBS"),
            None
        );
        assert_eq!(
            classify_raw_message(
                &RawDispatch {
                    event_type: Some("MESSAGE_CREATE"),
                    ..delete
                },
                "OBS"
            ),
            None
        );
        assert_eq!(
            classify_raw_message(
                &RawDispatch {
                    guild_id: None,
                    ..delete
                },
                "OBS"
            ),
            None
        );
    }

    #[test]
    fn nullable_executor_target_and_nullish_counts_match_legacy() {
        let mut entry = RawAuditLogEntry {
            action_id: AUDIT_LOG_MEMBER_PRUNE,
            log_entry_id: "1".into(),
            executor_id: None,
            target_id: None,
            reason: None,
            created_timestamp_ms: 0,
            extra_channel_id: None,
            extra_count: Some(serde_json::json!("invalid")),
            extra_removed: Some(serde_json::json!(5)),
        };
        let event = classify_moderation_audit(&entry, GUILD, None, None).unwrap();
        assert_eq!(event.actor_id, None);
        assert_eq!(event.target_id, None);
        // Legacy chooses count ?? removed *before* numeric coercion. An
        // invalid-but-present count does not fall back to removed.
        let meta: serde_json::Value = serde_json::from_str(&event.metadata_json).unwrap();
        assert!(meta["count"].is_null());
        entry.extra_count = Some(serde_json::Value::Null);
        let event = classify_moderation_audit(&entry, GUILD, None, None).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&event.metadata_json).unwrap();
        assert_eq!(meta["count"], 5);
        entry.extra_removed = Some(serde_json::Value::Null);
        let event = classify_moderation_audit(&entry, GUILD, None, None).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&event.metadata_json).unwrap();
        assert_eq!(meta["count"], 0);
        entry.extra_removed = None;
        let event = classify_moderation_audit(&entry, GUILD, None, None).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&event.metadata_json).unwrap();
        assert!(meta["count"].is_null());
    }

    #[test]
    fn correlated_moderation_row_matches_legacy() {
        let reason = moderation_audit_reason(
            Some(SECRET),
            GUILD,
            "idem-1",
            "moderation.ban",
            ACTOR,
            "spam",
        );
        let entry = RawAuditLogEntry {
            action_id: 22,
            log_entry_id: "111".to_owned(),
            executor_id: Some(ACTOR.to_owned()),
            target_id: Some("222222222222222222".to_owned()),
            reason: Some(reason),
            created_timestamp_ms: 1_767_225_600_000,
            extra_channel_id: None,
            extra_count: None,
            extra_removed: None,
        };
        let event =
            classify_moderation_audit(&entry, GUILD, Some(ACTOR), Some(SECRET)).expect("row");
        assert_eq!(event.kind, AuditKind::ModerationAction);
        assert!(event
            .entry_id
            .starts_with(&format!("moderation-success:{GUILD}:")));
        assert_eq!(event.action.as_deref(), Some("moderation.ban"));
        assert_eq!(event.actor_id.as_deref(), Some(ACTOR));
        assert_eq!(event.target_id.as_deref(), Some("222222222222222222"));
        assert_eq!(event.occurred_at, "2026-01-01T00:00:00.000Z");
        assert!(event
            .metadata_json
            .contains("\"origin\":\"moderation_service\""));
        assert!(event.metadata_json.contains("\"outcome\":\"banned\""));
    }

    #[test]
    fn uncorrelated_moderation_row_matches_legacy() {
        // No secret: marker cannot verify, row stays a plain discord-audit row.
        let entry = RawAuditLogEntry {
            action_id: 20,
            log_entry_id: "222".to_owned(),
            executor_id: Some("333333333333333333".to_owned()),
            target_id: Some("444444444444444444".to_owned()),
            reason: Some("[two-audit:v1:bad] whatever".to_owned()),
            created_timestamp_ms: 0,
            extra_channel_id: None,
            extra_count: Some(serde_json::json!(5)),
            extra_removed: None,
        };
        let event = classify_moderation_audit(&entry, GUILD, Some("333333333333333333"), None)
            .expect("row");
        assert_eq!(event.entry_id, format!("discord-audit:{GUILD}:222"));
        assert_eq!(event.action.as_deref(), Some("member_kick"));
        assert_eq!(event.actor_id.as_deref(), Some("333333333333333333"));
        assert_eq!(event.occurred_at, "1970-01-01T00:00:00.000Z");
        assert!(event.metadata_json.contains("\"count\":5"));
        assert!(!event.metadata_json.contains("moderation_service"));

        // Marker names another bot: uncorrelated even with the secret.
        let reason =
            moderation_audit_reason(Some(SECRET), GUILD, "idem-9", "moderation.kick", ACTOR, "x");
        let other_bot = RawAuditLogEntry {
            action_id: 20,
            log_entry_id: "223".to_owned(),
            executor_id: Some("999999999999999999".to_owned()),
            target_id: Some("444444444444444444".to_owned()),
            reason: Some(reason),
            created_timestamp_ms: 0,
            extra_channel_id: None,
            extra_count: None,
            extra_removed: None,
        };
        let event =
            classify_moderation_audit(&other_bot, GUILD, Some(ACTOR), Some(SECRET)).expect("row");
        assert_eq!(event.entry_id, format!("discord-audit:{GUILD}:223"));

        // Channel-targeted correlated action: target moves to the channel row.
        let reason = moderation_audit_reason(
            Some(SECRET),
            GUILD,
            "idem-p",
            "moderation.purge",
            ACTOR,
            "x",
        );
        let purge = RawAuditLogEntry {
            action_id: 73,
            log_entry_id: "224".to_owned(),
            executor_id: Some(ACTOR.to_owned()),
            target_id: Some("555555555555555555".to_owned()),
            reason: Some(reason),
            created_timestamp_ms: 0,
            extra_channel_id: None,
            extra_count: Some(serde_json::json!(25)),
            extra_removed: None,
        };
        let event =
            classify_moderation_audit(&purge, GUILD, Some(ACTOR), Some(SECRET)).expect("row");
        assert_eq!(event.action.as_deref(), Some("moderation.purge"));
        assert_eq!(event.target_id, None);
        assert_eq!(
            event.source_channel_id.as_deref(),
            Some("555555555555555555")
        );
        assert!(event.metadata_json.contains("\"outcome\":\"purged\""));
        assert!(event.metadata_json.contains("\"affected\":25"));

        // Unknown audit-log action: no row.
        let unknown = RawAuditLogEntry {
            action_id: 1,
            ..purge
        };
        assert_eq!(
            classify_moderation_audit(&unknown, GUILD, Some(ACTOR), Some(SECRET)),
            None
        );
    }
}
