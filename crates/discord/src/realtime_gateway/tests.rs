use serde_json::{json, Value};
use twilight_model::gateway::payload::incoming::{
    ChannelCreate, ChannelDelete, ChannelUpdate, GuildUpdate, MessageDelete, RoleCreate,
    RoleDelete, RoleUpdate, VoiceStateUpdate,
};

use super::*;

const GUILD_ID: Id<GuildMarker> = Id::new(10);

fn operation(event: &Event) -> Value {
    serde_json::to_value(
        project(GUILD_ID, event)
            .expect("supported payload")
            .expect("allowlisted operation"),
    )
    .expect("wire operation serializes")
}

fn channel(kind: u8) -> Channel {
    serde_json::from_value(json!({
        "id": "20", "guild_id": "10", "name": "Lobby", "type": kind,
        "parent_id": "21", "position": 3,
        "topic": "synthetic excluded channel topic", "last_message_id": "78",
        "permission_overwrites": [
            {"type": 0, "id": "10", "allow": "0", "deny": "1024"},
            {"type": 1, "id": "42", "allow": "1024", "deny": "0"}
        ]
    }))
    .expect("channel fixture")
}

#[test]
fn channels_keep_only_allowlisted_fields_and_exact_overwrites() {
    for (kind, wire_kind) in [(2, "voice"), (13, "stage"), (4, "category"), (0, "other")] {
        let channel = channel(kind);
        let expected = json!({
            "topic": "channels", "op": "upsert", "id": "20",
            "value": {"id": "20", "name": "Lobby", "kind": wire_kind,
                "parentId": "21", "position": 3,
                "overwrites": [
                    {"kind": "role", "id": "10", "allow": "0", "deny": "1024"},
                    {"kind": "member", "id": "42", "allow": "1024", "deny": "0"}
                ]}
        });
        assert_eq!(
            operation(&Event::ChannelCreate(Box::new(ChannelCreate(
                channel.clone()
            )))),
            expected
        );
        assert_eq!(
            operation(&Event::ChannelUpdate(Box::new(ChannelUpdate(channel)))),
            expected
        );
    }

    let minimal: Channel = serde_json::from_value(json!({
        "id": "20", "guild_id": "10", "type": 2
    }))
    .expect("channel delete does not need an upsert row");
    assert_eq!(
        operation(&Event::ChannelDelete(Box::new(ChannelDelete(minimal)))),
        json!({"topic": "channels", "op": "remove", "id": "20"})
    );
}

#[test]
fn malformed_channel_permission_state_is_not_an_ignored_event() {
    let missing_name = Channel {
        name: None,
        ..channel(2)
    };
    let missing_overwrites = Channel {
        permission_overwrites: None,
        ..channel(2)
    };
    let mut unknown_overwrite = channel(2);
    unknown_overwrite
        .permission_overwrites
        .as_mut()
        .expect("overwrites")[0]
        .kind = PermissionOverwriteType::Unknown(8);
    let negative_position = Channel {
        position: Some(-1),
        ..channel(2)
    };

    for (channel, expected) in [
        (missing_name, ProjectionError::IncompleteChannel),
        (missing_overwrites, ProjectionError::IncompleteChannel),
        (unknown_overwrite, ProjectionError::UnsupportedOverwrite),
        (negative_position, ProjectionError::InvalidPosition),
        (channel(99), ProjectionError::UnknownChannelKind),
    ] {
        assert!(matches!(
            project(GUILD_ID, &Event::ChannelUpdate(Box::new(ChannelUpdate(channel)))),
            Err(error) if error == expected
        ));
    }

    let empty = Channel {
        permission_overwrites: Some(vec![]),
        ..channel(2)
    };
    assert_eq!(
        operation(&Event::ChannelUpdate(Box::new(ChannelUpdate(empty))))["value"]["overwrites"],
        json!([])
    );
}

#[test]
fn voice_join_move_and_leave_only_carry_user_and_room_ids() {
    for (room, expected) in [
        (
            Some("20"),
            json!({"topic": "voice", "op": "upsert", "id": "42", "value": {"userId": "42", "channelId": "20"}}),
        ),
        (
            Some("21"),
            json!({"topic": "voice", "op": "upsert", "id": "42", "value": {"userId": "42", "channelId": "21"}}),
        ),
        (None, json!({"topic": "voice", "op": "remove", "id": "42"})),
    ] {
        let event = Event::VoiceStateUpdate(Box::new(serde_json::from_value::<VoiceStateUpdate>(json!({
            "guild_id": "10", "channel_id": room, "user_id": "42", "session_id": "synthetic-session",
            "deaf": false, "mute": false, "self_deaf": false, "self_mute": false,
            "self_video": false, "suppress": false
        })).expect("voice state fixture")));
        assert_eq!(operation(&event), expected);
        assert!(matches!(project(Id::new(11), &event), Ok(None)));
    }
}

#[test]
fn owner_afk_and_role_changes_preserve_authorization_inputs() {
    let guild = serde_json::from_value::<GuildUpdate>(json!({
        "id": "10", "name": "not forwarded", "owner_id": "42", "afk_channel_id": "20",
        "afk_timeout": 60, "default_message_notifications": 0, "emojis": [], "features": [],
        "explicit_content_filter": 0, "mfa_level": 0, "nsfw_level": 0, "preferred_locale": "en-US",
        "premium_progress_bar_enabled": false, "premium_tier": 0, "roles": [],
        "system_channel_flags": 0, "verification_level": 0
    }))
    .expect("guild update fixture");
    assert_eq!(
        operation(&Event::GuildUpdate(Box::new(guild))),
        json!({
            "topic": "guild", "op": "upsert", "id": "10", "value": {"ownerId": "42", "afkChannelId": "20"}
        })
    );

    let role = serde_json::from_value::<Role>(json!({
        "id": "31", "name": "not forwarded", "permissions": "3072", "position": 1,
        "color": 0, "colors": {"primary_color": 0}, "flags": 0,
        "hoist": false, "managed": false, "mentionable": false
    }))
    .expect("role fixture");
    let expected = json!({"topic": "roles", "op": "upsert", "id": "31", "value": {"id": "31", "permissions": "3072"}});
    assert_eq!(
        operation(&Event::RoleCreate(RoleCreate {
            guild_id: GUILD_ID,
            role: role.clone()
        })),
        expected
    );
    assert_eq!(
        operation(&Event::RoleUpdate(RoleUpdate {
            guild_id: GUILD_ID,
            role
        })),
        expected
    );
    assert_eq!(
        operation(&Event::RoleDelete(RoleDelete {
            guild_id: GUILD_ID,
            role_id: Id::new(31)
        })),
        json!({"topic": "roles", "op": "remove", "id": "31"})
    );
}

#[test]
fn direct_messages_threads_other_guilds_and_unrelated_events_are_ignored() {
    let dm = Channel {
        guild_id: None,
        ..channel(1)
    };
    let foreign = Channel {
        guild_id: Some(Id::new(11)),
        ..channel(99)
    };
    for event in [
        Event::ChannelCreate(Box::new(ChannelCreate(dm))),
        Event::ChannelUpdate(Box::new(ChannelUpdate(foreign))),
        Event::ChannelUpdate(Box::new(ChannelUpdate(channel(12)))),
        Event::MessageDelete(MessageDelete {
            guild_id: Some(GUILD_ID),
            channel_id: Id::new(20),
            id: Id::new(78),
        }),
    ] {
        assert!(matches!(project(GUILD_ID, &event), Ok(None)));
    }
}
