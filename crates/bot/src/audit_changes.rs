//! Minimized projections of already-subscribed guild events. Gateway changes
//! prove observations, not who caused them. Audit-log entries remain separate
//! evidence; temporal proximity is never an attribution join.

use serde_json::{json, Value};
use twilight_cache_inmemory::InMemoryCache;
use twilight_gateway::Event;
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::Channel,
    guild::audit_log::AuditLogChange,
};
use two_bot_core::audit::{AuditEvent, AuditKind};

/// Only channel names are intentionally retained as text. Do not reuse this
/// for member names, topics, command arguments, reasons or remote error text.
pub(crate) fn channel_name(value: &str) -> String {
    let value: String = value
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == '`' { ' ' } else { c })
        .collect();
    two_bot_core::message_safety::truncate(
        &two_bot_core::message_safety::neutralize_mentions(&value),
        100,
    )
}

fn row(
    kind: AuditKind,
    guild: u64,
    target: u64,
    action: &str,
    at: &str,
    sequence: u64,
    metadata: Value,
) -> AuditEvent {
    let mut event = AuditEvent::new(
        format!("gateway-change:{guild}:{target}:{action}:{at}:{sequence}"),
        kind,
        guild.to_string(),
        at.to_owned(),
    );
    event.target_id = Some(target.to_string());
    event.action = Some(action.to_owned());
    event.metadata_json = metadata.to_string();
    event
}

fn channel_row(
    channel: &Channel,
    cache: &InMemoryCache,
    action: &str,
    at: &str,
    sequence: u64,
) -> Option<AuditEvent> {
    let guild = channel.guild_id?;
    let old = cache.channel(channel.id);
    let occupancy = cache.stats().channel_voice_states(channel.id);
    let mut event = row(
        AuditKind::ChannelChange,
        guild.get(),
        channel.id.get(),
        action,
        at,
        sequence,
        json!({
            "baseline": if old.is_some() { "cached" } else { "missing" },
            "oldName": old.as_ref().and_then(|c| c.name.as_deref()).map(channel_name),
            "newName": if action == "channel_delete" { None } else { channel.name.as_deref().map(channel_name) },
            "nameChanged": old.as_ref().map(|c| c.name != channel.name),
            "parentId": channel.parent_id.map(|id| id.to_string()),
            "position": channel.position,
            "bitrate": channel.bitrate,
            "userLimit": channel.user_limit,
            "slowmode": channel.rate_limit_per_user,
            "permissionOverwritesChanged": old.as_ref().map(|c| c.permission_overwrites != channel.permission_overwrites),
            "topicChanged": old.as_ref().map(|c| c.topic != channel.topic),
            "occupancyObserved": occupancy,
            "occupancyEvidence": "cached_voice_states_not_authoritative",
            "botReason": "unknown_gateway_observation"
        }),
    );
    // Existing sink self-channel guard also covers create/update/delete.
    event.source_channel_id = Some(channel.id.to_string());
    Some(event)
}

pub(crate) fn translate(
    event: &Event,
    cache: &InMemoryCache,
    at: &str,
    sequence: u64,
) -> Vec<AuditEvent> {
    let one = match event {
        Event::ChannelCreate(channel) => {
            channel_row(channel, cache, "channel_create", at, sequence)
        }
        Event::ChannelUpdate(channel) => {
            channel_row(channel, cache, "channel_update", at, sequence)
        }
        Event::ChannelDelete(channel) => {
            channel_row(channel, cache, "channel_delete", at, sequence)
        }
        Event::MemberAdd(member) => Some(row(
            AuditKind::MemberJoin,
            member.guild_id.get(),
            member.user.id.get(),
            "member_join",
            at,
            sequence,
            json!({}),
        )),
        Event::MemberRemove(member) => Some(row(
            AuditKind::MemberLeave,
            member.guild_id.get(),
            member.user.id.get(),
            "member_leave",
            at,
            sequence,
            json!({"cause": "unknown_not_proof_of_kick"}),
        )),
        Event::RoleCreate(role) => Some(row(
            AuditKind::RoleChange,
            role.guild_id.get(),
            role.role.id.get(),
            "role_create",
            at,
            sequence,
            json!({"permissions": role.role.permissions.bits().to_string()}),
        )),
        Event::RoleUpdate(role) => {
            let old = cache.role(role.role.id);
            Some(row(
                AuditKind::RoleChange,
                role.guild_id.get(),
                role.role.id.get(),
                "role_update",
                at,
                sequence,
                json!({
                    "oldPermissions": old.as_ref().map(|r| r.resource().permissions.bits().to_string()),
                    "newPermissions": role.role.permissions.bits().to_string(),
                    "nameChanged": old.as_ref().map(|r| r.resource().name != role.role.name),
                    "position": role.role.position,
                    "baseline": if old.is_some() { "cached" } else { "missing" }
                }),
            ))
        }
        Event::RoleDelete(role) => Some(row(
            AuditKind::RoleChange,
            role.guild_id.get(),
            role.role_id.get(),
            "role_delete",
            at,
            sequence,
            json!({}),
        )),
        Event::GuildUpdate(guild) => {
            let old = cache.guild(guild.id);
            Some(row(
                AuditKind::GuildChange,
                guild.id.get(),
                guild.id.get(),
                "guild_update",
                at,
                sequence,
                json!({
                    "baseline": if old.is_some() { "cached" } else { "missing" },
                    "nameChanged": old.as_ref().map(|g| g.name() != guild.name),
                    "ownerId": guild.owner_id.to_string(),
                    "verificationLevel": guild.verification_level,
                    "mfaLevel": guild.mfa_level,
                    "explicitContentFilter": guild.explicit_content_filter,
                    "systemChannelFlags": guild.system_channel_flags.bits().to_string()
                }),
            ))
        }
        Event::MessageDeleteBulk(deleted) => deleted.guild_id.map(|guild| {
            let mut event = row(
                AuditKind::MessageDelete,
                guild.get(),
                deleted.channel_id.get(),
                "message_bulk_delete",
                at,
                sequence,
                json!({"count": deleted.ids.len()}),
            );
            event.source_channel_id = Some(deleted.channel_id.to_string());
            event
        }),
        Event::InteractionCreate(interaction) => {
            command_event(interaction, "received", "gateway", at)
        }
        Event::GuildAuditLogEntryCreate(entry) => {
            let action = u16::from(entry.action_type);
            let (kind, label) = match action {
                1 => (AuditKind::GuildChange, "guild_update"),
                10 => (AuditKind::ChannelChange, "channel_create"),
                12 => (AuditKind::ChannelChange, "channel_delete"),
                28 => (AuditKind::MemberJoin, "bot_add"),
                30 => (AuditKind::RoleChange, "role_create"),
                31 => (AuditKind::RoleChange, "role_update"),
                32 => (AuditKind::RoleChange, "role_delete"),
                _ => return Vec::new(),
            };
            entry.guild_id.map(|guild| {
                let at = two_bot_core::format_iso_millis(
                    ((entry.id.get() >> 22) + 1_420_070_400_000) as i64,
                );
                let mut event = row(
                    kind,
                    guild.get(),
                    entry.target_id.map_or(guild.get(), |id| id.get()),
                    label,
                    &at,
                    sequence,
                    json!({"auditLogEntryId": entry.id.to_string(), "source": "discord_audit_log"}),
                );
                event.entry_id = format!("discord-audit:{guild}:{}", entry.id);
                event.target_id = entry.target_id.map(|id| id.to_string());
                event.actor_id = entry.user_id.map(|id| id.to_string());
                event
            })
        }
        _ => None,
    };
    one.into_iter().collect()
}

/// Add attribution provenance to all production rows, without altering legacy
/// domain classifier fixtures. Never manufacture an executor from a subject.
pub(crate) fn decorate(
    event: &mut AuditEvent,
    packet: &Event,
    sequence: u64,
    bot_id: Option<&str>,
) {
    let mut metadata: Value =
        serde_json::from_str(&event.metadata_json).unwrap_or_else(|_| json!({}));
    let audit_log = matches!(packet, Event::GuildAuditLogEntryCreate(_));
    let interaction = matches!(packet, Event::InteractionCreate(_));
    metadata["source"] = json!(if audit_log {
        "discord_audit_log"
    } else if interaction {
        "interaction"
    } else {
        "gateway"
    });
    metadata["sequence"] = json!(sequence);
    if !audit_log && !interaction {
        // A message author is a subject, not proof of an executor.
        event.actor_id = None;
    }
    metadata["actorConfidence"] = json!(if event.actor_id.is_some() {
        "verified"
    } else {
        "unknown"
    });
    metadata["actorType"] = json!(match packet {
        Event::InteractionCreate(interaction) =>
            interaction
                .author()
                .map_or("unknown", |user| if user.bot { "bot" } else { "user" }),
        _ if event
            .actor_id
            .as_deref()
            .is_some_and(|id| Some(id) == bot_id) =>
            "bot",
        _ => "unknown",
    });
    metadata["clock"] = json!(if audit_log {
        "audit_snowflake_utc"
    } else {
        "gateway_receipt_utc"
    });
    if let Event::GuildAuditLogEntryCreate(entry) = packet {
        // Only channel actions may expose the name diff; never guild/member/
        // role names. Unknown changes and free-form audit reasons are ignored.
        if matches!(u16::from(entry.action_type), 10..=12) {
            event.source_channel_id = entry.target_id.map(|id| id.to_string());
            for change in entry.changes.iter().take(64) {
                if let AuditLogChange::Name { old, new } = change {
                    metadata["oldName"] = json!(old.as_deref().map(channel_name));
                    metadata["newName"] = json!(new.as_deref().map(channel_name));
                }
            }
            metadata["botReason"] = json!("unknown_audit_reason_not_retained");
            metadata["occupancyEvidence"] = json!("unknown_at_audit_entry_time");
        }
    }
    event.metadata_json = metadata.to_string();
}

/// The verified invoker and command ID only: no options, component IDs,
/// interaction token, command body, username or response content.
pub(crate) fn command_event(
    interaction: &Interaction,
    outcome: &str,
    phase: &str,
    at: &str,
) -> Option<AuditEvent> {
    let guild = interaction.guild_id?;
    let command = match interaction.data.as_ref()? {
        InteractionData::ApplicationCommand(command) => command,
        _ => return None,
    };
    let mut event = row(
        AuditKind::CommandAction,
        guild.get(),
        command.id.get(),
        "command",
        at,
        0,
        json!({
            "interactionId": interaction.id.to_string(), "commandId": command.id.to_string(),
            "phase": phase, "outcome": outcome,
            "actorConfidence": if interaction.author_id().is_some() { "verified" } else { "unknown" },
            "actorType": interaction.author().map_or("unknown", |user| if user.bot { "bot" } else { "user" }), "source": "interaction",
            "semantics": "delivery_or_dispatch_not_business_success"
        }),
    );
    event.actor_id = interaction.author_id().map(|id| id.to_string());
    event.source_channel_id = interaction
        .channel
        .as_ref()
        .map(|channel| channel.id.to_string());
    event.entry_id = if phase == "gateway" {
        format!("command:{}:received", interaction.id)
    } else {
        format!(
            "command:{}:{phase}:{:032x}",
            interaction.id,
            rand::random::<u128>()
        )
    };
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use twilight_model::gateway::payload::incoming::{ChannelCreate, ChannelUpdate};
    use two_bot_discord::build_cache;
    const AT: &str = "2026-11-08T00:00:00.000Z";

    fn channel(name: &str) -> Channel {
        serde_json::from_value(json!({"id":"2", "guild_id":"1", "type":2, "name":name,
            "topic":"PRIVATE TOPIC SENTINEL", "permission_overwrites":[], "user_limit":4}))
        .expect("fixture channel")
    }

    #[test]
    fn channel_changes_keep_names_not_topics_and_never_infer_an_actor() {
        let cache = build_cache();
        cache.update(&Event::ChannelCreate(Box::new(ChannelCreate(channel(
            "Before",
        )))));
        let packet = Event::ChannelUpdate(Box::new(ChannelUpdate(channel("After"))));
        let mut events = translate(&packet, &cache, AT, 9);
        assert_eq!(events.len(), 1);
        decorate(&mut events[0], &packet, 9, None);
        let meta: Value = serde_json::from_str(&events[0].metadata_json).expect("metadata");
        assert_eq!(meta["oldName"], "Before");
        assert_eq!(meta["newName"], "After");
        assert_eq!(meta["actorConfidence"], "unknown");
        assert!(events[0].actor_id.is_none());
        assert!(!events[0].metadata_json.contains("PRIVATE TOPIC SENTINEL"));
        assert_eq!(events[0].source_channel_id.as_deref(), Some("2"));
        assert_ne!(
            translate(&packet, &cache, AT, 9)[0].entry_id,
            translate(&packet, &cache, AT, 10)[0].entry_id
        );
    }

    #[test]
    fn missing_baseline_is_explicit_and_dm_channel_is_excluded() {
        let cache = build_cache();
        let packet = Event::ChannelUpdate(Box::new(ChannelUpdate(channel("After"))));
        let event = &translate(&packet, &cache, AT, 9)[0];
        let meta: Value = serde_json::from_str(&event.metadata_json).expect("metadata");
        assert_eq!(meta["baseline"], "missing");
        assert_eq!(meta["oldName"], Value::Null);
        assert_eq!(meta["nameChanged"], Value::Null);
        let mut dm = channel("no DM recording");
        dm.guild_id = None;
        assert!(translate(
            &Event::ChannelCreate(Box::new(ChannelCreate(dm))),
            &cache,
            AT,
            9
        )
        .is_empty());
    }

    #[test]
    fn role_and_guild_changes_minimize_names_and_descriptions() {
        use twilight_model::gateway::payload::incoming::{
            GuildUpdate, RoleCreate, RoleDelete, RoleUpdate,
        };
        use twilight_model::id::Id;
        let cache = build_cache();
        let role = |permissions: &str| {
            serde_json::from_value(json!({"id":"3", "name":"ROLE NAME SENTINEL", "color":0,
            "colors":{"primary_color":0}, "hoist":false, "managed":false, "mentionable":false,
            "permissions":permissions, "position":1, "flags":0}))
            .expect("role fixture")
        };
        let create = Event::RoleCreate(RoleCreate {
            guild_id: Id::new(1),
            role: role("0"),
        });
        assert_eq!(
            translate(&create, &cache, AT, 1)[0].action.as_deref(),
            Some("role_create")
        );
        cache.update(&create);
        let update = Event::RoleUpdate(RoleUpdate {
            guild_id: Id::new(1),
            role: role("8"),
        });
        let row = &translate(&update, &cache, AT, 2)[0];
        let meta: Value = serde_json::from_str(&row.metadata_json).expect("metadata");
        assert_eq!(meta["oldPermissions"], "0");
        assert_eq!(meta["newPermissions"], "8");
        assert!(!row.metadata_json.contains("ROLE NAME SENTINEL"));
        assert_eq!(
            translate(
                &Event::RoleDelete(RoleDelete {
                    guild_id: Id::new(1),
                    role_id: Id::new(3)
                }),
                &cache,
                AT,
                3
            )[0]
            .action
            .as_deref(),
            Some("role_delete")
        );
        let guild = serde_json::from_value(json!({"id":"1", "name":"GUILD NAME SENTINEL", "owner_id":"4",
            "description":"GUILD DESCRIPTION SENTINEL", "afk_timeout":300, "default_message_notifications":0,
            "emojis":[], "explicit_content_filter":0, "features":[], "mfa_level":0, "nsfw_level":0,
            "preferred_locale":"en-US", "premium_progress_bar_enabled":false, "premium_tier":0,
            "roles":[], "system_channel_flags":0, "verification_level":0})).expect("guild fixture");
        let update = Event::GuildUpdate(Box::new(GuildUpdate(guild)));
        let row = &translate(&update, &cache, AT, 4)[0];
        assert_eq!(row.kind, AuditKind::GuildChange);
        assert!(!row.metadata_json.contains("SENTINEL"));
        let deleted = Event::ChannelDelete(Box::new(
            twilight_model::gateway::payload::incoming::ChannelDelete(channel("Before")),
        ));
        assert_eq!(
            translate(&deleted, &cache, AT, 5)[0].action.as_deref(),
            Some("channel_delete")
        );
    }

    #[test]
    fn channel_names_are_bounded_and_cannot_break_inline_code_or_ping() {
        let name = channel_name(&format!("`\n@everyone <@123>{}", "😀".repeat(150)));
        assert!(name.encode_utf16().count() <= 100);
        assert!(!name.contains('`'));
        assert!(!name.contains('\n'));
        assert!(!name.contains("@everyone"));
        assert!(!name.contains("<@123>"));
    }
}
