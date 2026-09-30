//! Operational audit model: metadata-only staff audit sink (TOG-9810 S5).
//!
//! Ports the `OperationalAuditEvent` row shape, channel routing, sink routing
//! decisions, kill-switch snapshots, and delivery nonces from legacy two-bot:
//! - row + routing: `src/audit/events.ts` (`OperationalAuditEvent`,
//!   `auditEventIdentity`, `formatAuditEvent`)
//! - sink routing: `src/audit/service.ts` `record()` (tamper-drop guard,
//!   guild scoping, `channelFor` mirror fallback)
//! - kill switch: `src/audit/service.ts` `deliveryHalted()` (read-fails-open,
//!   log only on transitions, process boots silent)
//! - nonce: `src/audit/store.ts` `deliveryNonce()`
//!
//! Privacy rule (legacy `formatAuditEvent`): no message bodies, usernames or
//! nicknames enter classifier output. These are pure row/formatting/routing
//! primitives, not a running sink. The durable store, pending queue, delivery
//! claims and Discord mirror still need runtime wiring.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use base64::Engine as _;

/// The seven operational kinds accepted by legacy `AuditSink::record`.
/// Legacy also defines `rota_notice`, but refuses it before record/claim:
/// those notices require separate eligibility and recovery guards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    MessageEdit,
    MessageDelete,
    MemberUpdate,
    VoiceJoin,
    VoiceLeave,
    VoiceMove,
    ModerationAction,
}

impl AuditKind {
    /// Stable snake_case name matching two-bot `OperationalAuditKind`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MessageEdit => "message_edit",
            Self::MessageDelete => "message_delete",
            Self::MemberUpdate => "member_update",
            Self::VoiceJoin => "voice_join",
            Self::VoiceLeave => "voice_leave",
            Self::VoiceMove => "voice_move",
            Self::ModerationAction => "moderation_action",
        }
    }
}

/// Which Discord mirror channel a row drains to. Mirrors the two-bot
/// `AuditChannel` union; the mirror senders arrive in a later slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditChannel {
    Audit,
    Voice,
    Moderation,
}

impl AuditChannel {
    /// Channel for a classified kind (two-bot `channel` field convention:
    /// voice deltas go to the voice channel, moderation actions to the
    /// moderation channel, everything else to audit).
    #[must_use]
    pub fn for_kind(kind: AuditKind) -> Self {
        match kind {
            AuditKind::VoiceJoin | AuditKind::VoiceLeave | AuditKind::VoiceMove => Self::Voice,
            AuditKind::ModerationAction => Self::Moderation,
            _ => Self::Audit,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Audit => "audit",
            Self::Voice => "voice",
            Self::Moderation => "moderation",
        }
    }
}

/// One metadata-only audit row. Snowflake IDs stay strings (never parsed to
/// integers in the sink) and `metadata_json` carries small classifier context
/// only — never message content or display names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Stable dedupe key (two-bot `entryId` conventions, per classifier).
    pub entry_id: String,
    pub kind: AuditKind,
    pub channel: AuditChannel,
    pub guild_id: String,
    /// ISO-8601 UTC instant the event occurred.
    pub occurred_at: String,
    pub actor_id: Option<String>,
    pub target_id: Option<String>,
    pub source_channel_id: Option<String>,
    pub destination_channel_id: Option<String>,
    pub message_id: Option<String>,
    pub action: Option<String>,
    /// Small JSON object; classifiers keep it to IDs, counts and flags.
    pub metadata_json: String,
}

impl AuditEvent {
    /// Minimal constructor; `channel` derives from `kind`, metadata defaults
    /// to `{}`. Classifiers fill the optional fields they own.
    #[must_use]
    pub fn new(entry_id: String, kind: AuditKind, guild_id: String, occurred_at: String) -> Self {
        Self {
            channel: AuditChannel::for_kind(kind),
            entry_id,
            kind,
            guild_id,
            occurred_at,
            actor_id: None,
            target_id: None,
            source_channel_id: None,
            destination_channel_id: None,
            message_id: None,
            action: None,
            metadata_json: "{}".to_owned(),
        }
    }

    /// Identity marker prefix used when a row is mirrored to Discord
    /// (two-bot `auditEventIdentity`).
    #[must_use]
    pub fn identity(&self) -> String {
        format!("audit-event:{};", self.entry_id)
    }
}

/// True when mirrored `content` carries this row's identity marker (two-bot
/// `hasAuditEventIdentity`: `content.startsWith(\`${identity} · \`)`).
#[must_use]
pub fn has_audit_event_identity(content: &str, entry_id: &str) -> bool {
    content.starts_with(&format!("audit-event:{entry_id}; · "))
}

/// Render a row for its Discord mirror message (two-bot `formatAuditEvent`).
/// Metadata arrays truncate: empty renders `none`, long ones keep a fitting
/// prefix plus ` (+N omitted)` (scalar values truncate to 300 chars). Whole
/// messages over 2000 chars truncate to 1996 + `...`.
#[must_use]
pub fn format_audit_event(event: &AuditEvent) -> String {
    let mut fields = vec![
        event.identity(),
        format!("**{}**", event.kind.as_str().replace('_', " ")),
        format!("at {}", event.occurred_at),
        format!(
            "target `{}`",
            event.target_id.as_deref().unwrap_or("unknown")
        ),
    ];
    if let Some(actor) = event.actor_id.as_deref() {
        fields.push(format!("actor `{actor}`"));
    }
    if let Some(message) = event.message_id.as_deref() {
        fields.push(format!("message `{message}`"));
    }
    if let Some(source) = event.source_channel_id.as_deref() {
        fields.push(format!("from <#{source}>"));
    }
    if let Some(dest) = event.destination_channel_id.as_deref() {
        fields.push(format!("to <#{dest}>"));
    }
    if let Some(action) = event.action.as_deref() {
        fields.push(format!("action `{action}`"));
    }
    let metadata: serde_json::Value =
        serde_json::from_str(&event.metadata_json).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(map) = metadata {
        let rendered: Vec<String> = map
            .into_iter()
            .map(|(key, value)| format!("{key}=`{}`", format_metadata_value(&value)))
            .collect();
        if !rendered.is_empty() {
            fields.push(rendered.join(" "));
        }
    }
    truncate_discord_content(&fields.join(" · "))
}

/// Format one metadata value (two-bot `formatMetadata`).
fn format_metadata_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                return "none".to_owned();
            }
            let items: Vec<String> = items
                .iter()
                .map(|item| match item {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            let mut included: Vec<&str> = Vec::new();
            for (index, item) in items.iter().enumerate() {
                let candidate = included
                    .iter()
                    .copied()
                    .chain(std::iter::once(item.as_str()))
                    .collect::<Vec<_>>()
                    .join(",");
                let omitted = items.len() - index - 1;
                let indicator = if omitted > 0 {
                    format!(" (+{omitted} omitted)")
                } else {
                    String::new()
                };
                if format!("{candidate}{indicator}").encode_utf16().count() > 300 {
                    break;
                }
                included.push(item);
            }
            let omitted = items.len() - included.len();
            if omitted == 0 {
                return included.join(",");
            }
            let indicator = format!("+{omitted} omitted");
            if included.is_empty() {
                indicator
            } else {
                format!("{} ({indicator})", included.join(","))
            }
        }
        serde_json::Value::String(s) => utf16_prefix(s, 300),
        other => utf16_prefix(&other.to_string(), 300),
    }
}

/// Truncate a mirror message to Discord's 2000-char limit (two-bot
/// `truncateDiscordContent`).
fn truncate_discord_content(content: &str) -> String {
    if content.encode_utf16().count() <= 2_000 {
        return content.to_owned();
    }
    format!("{}...", utf16_prefix(content, 1_996))
}

// JS string limits count UTF-16 units. Never slice a Rust string at a byte
// offset: even the static separator is non-ASCII, and metadata may be Unicode.
fn utf16_prefix(value: &str, limit: usize) -> String {
    let mut units = 0;
    value
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= limit
        })
        .collect()
}

/// Mirror channel ids the sink routes to (two-bot `AuditChannelIds`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditChannelIds {
    pub audit: Option<String>,
    pub voice: Option<String>,
    pub moderation: Option<String>,
}

impl AuditChannelIds {
    /// Configured channel a row's kind drains to (two-bot `channelFor`:
    /// voice falls back to audit, moderation falls back to audit).
    #[must_use]
    pub fn channel_for(&self, channel: AuditChannel) -> Option<&str> {
        match channel {
            AuditChannel::Voice => self.voice.as_deref().or(self.audit.as_deref()),
            AuditChannel::Moderation => self.moderation.as_deref().or(self.audit.as_deref()),
            AuditChannel::Audit => self.audit.as_deref(),
        }
    }
}

/// The sink's pure routing decision for one row (two-bot `record()` routing
/// half, before the store/claim/send wiring that needs Discord + Postgres).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkRoute {
    /// Row stored durably but never mirrored: its `source_channel_id` is one
    /// of the mirror channels, so mirroring it would create a feedback loop.
    /// Legacy still stores first (`inserted` is real), then logs
    /// `operational_audit_tamper_recorded` and returns without sending.
    TamperLoop { stored: bool },
    /// A mirror channel is requested but the row belongs to a different
    /// guild than the configured mirror guild — logged as
    /// `operational_audit_undeliverable`, not retried as a delivery.
    WrongGuild { requested_channel_id: String },
    /// No mirror channel resolves (channel family unconfigured): stored only.
    StoreOnly,
    /// Mirror to this channel id.
    Mirror { mirror_channel_id: String },
}

/// Route one row through the sink (two-bot `record()` routing half).
///
/// `configured` is the set of mirror channel ids (legacy `configured`); a row
/// whose `source_channel_id` is one of them is a tamper loop and is never
/// mirrored. `stored` reports whether the durable write (which the caller
/// owns) succeeded — the tamper arm preserves it.
#[must_use]
pub fn route_for_sink(
    event: &AuditEvent,
    channels: &AuditChannelIds,
    configured: &[String],
    mirror_guild_id: Option<&str>,
    stored: bool,
) -> SinkRoute {
    if event
        .source_channel_id
        .as_deref()
        .is_some_and(|source| configured.iter().any(|id| id == source))
    {
        return SinkRoute::TamperLoop { stored };
    }
    let Some(requested) = channels
        .channel_for(event.channel)
        .filter(|id| !id.is_empty())
    else {
        return SinkRoute::StoreOnly;
    };
    // Legacy: `requestedChannelId && options.guildId &&
    // event.guildId === options.guildId`. Mirrors stay disabled unless events
    // are constrained to one configured guild.
    if mirror_guild_id.is_some_and(|g| !g.is_empty() && g == event.guild_id) {
        SinkRoute::Mirror {
            mirror_channel_id: requested.to_owned(),
        }
    } else {
        SinkRoute::WrongGuild {
            requested_channel_id: requested.to_owned(),
        }
    }
}

/// Kill-switch snapshot for one `deliveryHalted()` read (two-bot TOG-3187).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KillSwitchSnapshot {
    /// Currently observed state (`None` = never read yet this process).
    pub observed_halted: Option<bool>,
    /// This read's value (`true` = halted).
    pub halted: bool,
    /// Whether the read itself failed (store unreachable).
    pub read_failed: bool,
}

impl KillSwitchSnapshot {
    /// Decide the effective halt and whether to log a transition line.
    /// Legacy `deliveryHalted()`: a read failure fails open (`false`) with
    /// `operational_audit_kill_switch_read_failed`; a process that boots
    /// already halted logs `..._engaged` once; one that boots clear says
    /// nothing (`was !== null` gate); only real transitions log after that.
    #[must_use]
    pub fn decide(self) -> KillSwitchDecision {
        if self.read_failed {
            return KillSwitchDecision {
                halted: false,
                log: Some(KillSwitchLog::ReadFailed),
                observed_halted: self.observed_halted,
            };
        }
        let log = if self.observed_halted == Some(self.halted) {
            None
        } else if self.halted || self.observed_halted.is_some() {
            Some(if self.halted {
                KillSwitchLog::Engaged
            } else {
                KillSwitchLog::Disengaged
            })
        } else {
            None
        };
        KillSwitchDecision {
            halted: self.halted,
            log,
            observed_halted: Some(self.halted),
        }
    }
}

/// Outcome of one kill-switch read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KillSwitchDecision {
    pub halted: bool,
    pub log: Option<KillSwitchLog>,
    pub observed_halted: Option<bool>,
}

/// Which transition line the read emits, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillSwitchLog {
    ReadFailed,
    Engaged,
    Disengaged,
}

/// Discord message nonce for one entry id (two-bot `deliveryNonce`):
/// `oa_` + first 22 base64url chars of `sha256(entry_id)` — 25 chars total,
/// inside Discord's 25-char nonce limit.
#[must_use]
pub fn delivery_nonce(entry_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(entry_id.as_bytes());
    let hash = hasher.finalize();
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);
    format!("oa_{}", &encoded[..22])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_kinds_route_to_voice_channel() {
        for kind in [
            AuditKind::VoiceJoin,
            AuditKind::VoiceLeave,
            AuditKind::VoiceMove,
        ] {
            assert_eq!(AuditChannel::for_kind(kind), AuditChannel::Voice);
        }
        assert_eq!(
            AuditChannel::for_kind(AuditKind::ModerationAction),
            AuditChannel::Moderation
        );
        assert_eq!(
            AuditChannel::for_kind(AuditKind::MemberUpdate),
            AuditChannel::Audit
        );
    }

    #[test]
    fn kind_names_match_two_bot() {
        // All seven legacy OperationalAuditKind values, no more.
        for kind in [
            AuditKind::MessageEdit,
            AuditKind::MessageDelete,
            AuditKind::MemberUpdate,
            AuditKind::VoiceJoin,
            AuditKind::VoiceLeave,
            AuditKind::VoiceMove,
            AuditKind::ModerationAction,
        ] {
            let name = kind.as_str();
            let back: AuditKind =
                serde_json::from_value(serde_json::Value::String(name.to_owned()))
                    .expect("name round-trips");
            assert_eq!(back, kind);
        }
        assert_eq!(AuditKind::MemberUpdate.as_str(), "member_update");
        assert_eq!(AuditKind::ModerationAction.as_str(), "moderation_action");
    }

    #[test]
    fn event_round_trips_through_json() {
        let event = AuditEvent::new(
            "member-update:1:2:at:digest".to_owned(),
            AuditKind::MemberUpdate,
            "1".to_owned(),
            "2026-01-01T00:00:00.000Z".to_owned(),
        );
        let json = serde_json::to_string(&event).expect("serializes");
        let back: AuditEvent = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(event, back);
        assert_eq!(back.channel, AuditChannel::Audit);
    }

    #[test]
    fn format_matches_legacy_shape() {
        let mut event = AuditEvent::new(
            "moderation-success:1:token".to_owned(),
            AuditKind::ModerationAction,
            "1".to_owned(),
            "2026-01-01T00:00:00.000Z".to_owned(),
        );
        event.target_id = Some("2".to_owned());
        event.actor_id = Some("9".to_owned());
        event.action = Some("moderation.ban".to_owned());
        event.metadata_json = serde_json::json!({"outcome": "banned"}).to_string();
        let text = format_audit_event(&event);
        assert!(text.starts_with("audit-event:moderation-success:1:token;"));
        assert!(text.contains("**moderation action**"));
        assert!(text.contains("target `2`"));
        assert!(text.contains("actor `9`"));
        assert!(text.contains("action `moderation.ban`"));
        assert!(text.contains("outcome=`banned`"));
        assert!(has_audit_event_identity(
            &format!("{text} extra"),
            "moderation-success:1:token"
        ));
        // The bare identity prefix alone is not a match (legacy ` · ` gate).
        assert!(!has_audit_event_identity(
            "audit-event:moderation-success:1:token;",
            "moderation-success:1:token"
        ));
    }

    #[test]
    fn channel_fallback_matches_legacy() {
        let channels = AuditChannelIds {
            audit: Some("audit-ch".to_owned()),
            voice: None,
            moderation: None,
        };
        // Voice/moderation fall back to audit; audit has no fallback.
        assert_eq!(channels.channel_for(AuditChannel::Voice), Some("audit-ch"));
        assert_eq!(
            channels.channel_for(AuditChannel::Moderation),
            Some("audit-ch")
        );
        let empty = AuditChannelIds::default();
        assert_eq!(empty.channel_for(AuditChannel::Audit), None);
    }

    #[test]
    fn sink_routing_matches_legacy_record() {
        let channels = AuditChannelIds {
            audit: Some("audit-ch".to_owned()),
            voice: Some("voice-ch".to_owned()),
            moderation: Some("mod-ch".to_owned()),
        };
        let configured = vec![
            "audit-ch".to_owned(),
            "voice-ch".to_owned(),
            "mod-ch".to_owned(),
        ];
        let mut event = AuditEvent::new(
            "e1".to_owned(),
            AuditKind::MemberUpdate,
            "guild-1".to_owned(),
            "AT".to_owned(),
        );
        // Same guild: mirrors.
        assert_eq!(
            route_for_sink(&event, &channels, &configured, Some("guild-1"), true),
            SinkRoute::Mirror {
                mirror_channel_id: "audit-ch".to_owned()
            }
        );
        // Different guild: wrong-guild, even though a channel is configured.
        assert_eq!(
            route_for_sink(&event, &channels, &configured, Some("other"), true),
            SinkRoute::WrongGuild {
                requested_channel_id: "audit-ch".to_owned()
            }
        );
        // No guild fence: mirrors disabled (legacy `mirrors are disabled
        // unless events are constrained to one configured guild`).
        assert_eq!(
            route_for_sink(&event, &channels, &configured, None, true),
            SinkRoute::WrongGuild {
                requested_channel_id: "audit-ch".to_owned()
            }
        );
        // Source is a mirror channel: tamper loop, never mirrored — but the
        // durable write result is preserved.
        event.source_channel_id = Some("audit-ch".to_owned());
        assert_eq!(
            route_for_sink(&event, &channels, &configured, Some("guild-1"), true),
            SinkRoute::TamperLoop { stored: true }
        );
        assert_eq!(
            route_for_sink(&event, &channels, &configured, Some("guild-1"), false),
            SinkRoute::TamperLoop { stored: false }
        );
        // Unconfigured family: store only.
        let bare = AuditEvent::new(
            "e2".to_owned(),
            AuditKind::VoiceJoin,
            "guild-1".to_owned(),
            "AT".to_owned(),
        );
        assert_eq!(
            route_for_sink(
                &bare,
                &AuditChannelIds::default(),
                &[],
                Some("guild-1"),
                true
            ),
            SinkRoute::StoreOnly
        );
    }

    #[test]
    fn empty_resolved_destination_stores_only_in_every_guild() {
        let channels = AuditChannelIds {
            audit: Some(String::new()),
            voice: Some(String::new()),
            moderation: Some(String::new()),
        };
        for kind in [
            AuditKind::MemberUpdate,
            AuditKind::VoiceJoin,
            AuditKind::ModerationAction,
        ] {
            let event =
                AuditEvent::new("empty-channel".into(), kind, "guild-1".into(), "AT".into());
            for guild in [Some("guild-1"), Some("other"), Some(""), None] {
                assert_eq!(
                    route_for_sink(&event, &channels, &[], guild, true),
                    SinkRoute::StoreOnly,
                );
                // Absent voice/moderation channels also resolve to the empty audit fallback.
                let fallback = AuditChannelIds {
                    audit: Some(String::new()),
                    ..Default::default()
                };
                assert_eq!(
                    route_for_sink(&event, &fallback, &[], guild, true),
                    SinkRoute::StoreOnly
                );
            }
        }
    }

    #[test]
    fn kill_switch_matches_legacy_transitions() {
        // Boot clear: silent.
        let d = KillSwitchSnapshot {
            observed_halted: None,
            halted: false,
            read_failed: false,
        }
        .decide();
        assert_eq!(
            d,
            KillSwitchDecision {
                halted: false,
                log: None,
                observed_halted: Some(false)
            }
        );
        // Boot halted: engaged, once.
        let d = KillSwitchSnapshot {
            observed_halted: None,
            halted: true,
            read_failed: false,
        }
        .decide();
        assert_eq!(d.log, Some(KillSwitchLog::Engaged));
        // Steady state: silent.
        let d = KillSwitchSnapshot {
            observed_halted: Some(true),
            halted: true,
            read_failed: false,
        }
        .decide();
        assert_eq!(d.log, None);
        // Transitions log.
        let d = KillSwitchSnapshot {
            observed_halted: Some(true),
            halted: false,
            read_failed: false,
        }
        .decide();
        assert_eq!(d.log, Some(KillSwitchLog::Disengaged));
        // Read failure fails open with its own line, observation unchanged.
        let d = KillSwitchSnapshot {
            observed_halted: Some(true),
            halted: true,
            read_failed: true,
        }
        .decide();
        assert_eq!(
            d,
            KillSwitchDecision {
                halted: false,
                log: Some(KillSwitchLog::ReadFailed),
                observed_halted: Some(true)
            }
        );
    }

    #[test]
    fn mirror_formatter_bounds_utf16_without_panicking() {
        let mut event = AuditEvent::new(
            "unicode".into(),
            AuditKind::MemberUpdate,
            "1".into(),
            "AT".into(),
        );
        event.metadata_json = serde_json::json!({
            "roles": (0..200).map(|i| i.to_string()).collect::<Vec<_>>(),
            "missing": null,
            "empty": [],
            "a": "😀".repeat(200), "b": "é".repeat(400),
            "c": "😀".repeat(200), "d": "é".repeat(400),
            "e": "😀".repeat(200), "f": "é".repeat(400),
            "g": "😀".repeat(200), "h": "é".repeat(400),
        })
        .to_string();
        let rendered = format_audit_event(&event);
        assert!(rendered.encode_utf16().count() <= 2_000);
        assert!(rendered.ends_with("..."));
        assert_eq!(format_metadata_value(&serde_json::Value::Null), "null");
        assert_eq!(format_metadata_value(&serde_json::json!([])), "none");
        assert_eq!(
            format_metadata_value(&serde_json::json!("😀".repeat(200)))
                .encode_utf16()
                .count(),
            300
        );
        let roles = format_metadata_value(&serde_json::json!((0..200)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()));
        assert!(roles.encode_utf16().count() <= 300);
        assert!(roles.contains("omitted"));
    }

    #[test]
    fn delivery_nonce_matches_legacy() {
        // Golden: node `deliveryNonce('member-update:1:2:AT:XOV6IYbl3Fu1_ixR')`.
        assert_eq!(
            delivery_nonce("member-update:1:2:AT:XOV6IYbl3Fu1_ixR"),
            "oa_TJJE4P3ORZN5zj-Sj1wVco"
        );
        let nonce = delivery_nonce("anything");
        assert_eq!(nonce.len(), 25);
        assert!(nonce.starts_with("oa_"));
    }
}
