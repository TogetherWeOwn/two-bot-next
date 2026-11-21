use serde_json::json;

use super::*;

const GUILD_ID: Id<GuildMarker> = Id::new(10);

fn operation(event_type: &str, payload: &Value) -> Value {
    serde_json::to_value(
        project(GUILD_ID, event_type, payload)
            .expect("supported payload")
            .expect("allowlisted operation"),
    )
    .expect("wire operation serializes")
}

fn assert_invalid_payload(event: &str, payload: &Value) {
    let expected = Some(ProjectionError::InvalidPayload);
    assert_eq!(project(GUILD_ID, event, payload).err(), expected, "{event}");
}

fn channel(kind: u8) -> Value {
    json!({
        "id": "20", "guild_id": "10", "name": "Lobby", "type": kind,
        "parent_id": "21", "position": 3,
        "topic": "synthetic excluded channel topic", "last_message_id": "78",
        "permission_overwrites": [
            {"type": 0, "id": "10", "allow": "0", "deny": "1024"},
            {"type": 1, "id": "42", "allow": "1024", "deny": "0"}
        ]
    })
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
        assert_eq!(operation("CHANNEL_CREATE", &channel), expected);
        assert_eq!(operation("CHANNEL_UPDATE", &channel), expected);
    }
    let minimal = json!({"id": "20", "guild_id": "10", "type": 2});
    assert_eq!(
        operation("CHANNEL_DELETE", &minimal),
        json!({"topic": "channels", "op": "remove", "id": "20"})
    );
}

#[test]
fn malformed_channel_permission_state_is_not_an_ignored_event() {
    for (field, value, expected) in [
        ("name", Value::Null, ProjectionError::IncompleteChannel),
        (
            "permission_overwrites",
            Value::Null,
            ProjectionError::IncompleteChannel,
        ),
        (
            "permission_overwrites",
            json!([{"type": 8, "id": "10", "allow": "0", "deny": "0"}]),
            ProjectionError::UnsupportedOverwrite,
        ),
        ("position", json!(-1), ProjectionError::InvalidPosition),
        ("type", json!(99), ProjectionError::UnknownChannelKind),
    ] {
        let mut payload = channel(2);
        payload[field] = value;
        assert!(
            matches!(project(GUILD_ID, "CHANNEL_UPDATE", &payload), Err(error) if error == expected)
        );
    }

    let mut empty = channel(2);
    empty["permission_overwrites"] = json!([]);
    assert_eq!(
        operation("CHANNEL_UPDATE", &empty)["value"]["overwrites"],
        json!([])
    );
}

#[test]
fn voice_join_move_and_leave_only_carry_user_and_room_ids() {
    let event = "VOICE_STATE_UPDATE";
    let mut payload = json!({
        "guild_id": "10", "user_id": "42", "session_id": "synthetic-session",
        "deaf": false, "mute": false, "self_deaf": false, "self_mute": false,
        "self_video": false, "suppress": false
    });
    assert_invalid_payload(event, &payload);
    assert!(matches!(project(Id::new(11), event, &payload), Ok(None)));
    for room in ["20", "21"] {
        payload["channel_id"] = json!(room);
        let expected = json!({"topic": "voice", "op": "upsert",
            "id": "42", "value": {"userId": "42", "channelId": room}});
        assert_eq!(operation(event, &payload), expected);
    }
    payload["channel_id"] = Value::Null;
    let expected = json!({"topic": "voice", "op": "remove", "id": "42"});
    assert_eq!(operation(event, &payload), expected);
}

#[test]
fn owner_afk_and_role_changes_preserve_authorization_inputs() {
    let event = "GUILD_UPDATE";
    let mut guild = json!({
        "id": "10", "name": "not forwarded", "owner_id": "42",
        "afk_timeout": 60, "default_message_notifications": 0, "emojis": [], "features": [],
        "explicit_content_filter": 0, "mfa_level": 0, "nsfw_level": 0, "preferred_locale": "en-US",
        "premium_progress_bar_enabled": false, "premium_tier": 0, "roles": [],
        "system_channel_flags": 0, "verification_level": 0
    });
    assert_invalid_payload(event, &guild);
    assert!(matches!(project(Id::new(11), event, &guild), Ok(None)));
    for afk in [Some("20"), None] {
        guild["afk_channel_id"] = json!(afk);
        let expected = json!({"topic": "guild", "op": "upsert",
            "id": "10", "value": {"ownerId": "42", "afkChannelId": afk}});
        assert_eq!(operation(event, &guild), expected);
    }
    let role = json!({"guild_id": "10", "role": {"id": "31", "name": "not forwarded", "permissions": "3072"}});
    let expected = json!({"topic": "roles", "op": "upsert", "id": "31", "value": {"id": "31", "permissions": "3072"}});
    assert_eq!(operation("GUILD_ROLE_CREATE", &role), expected);
    assert_eq!(operation("GUILD_ROLE_UPDATE", &role), expected);
    assert_eq!(
        operation(
            "GUILD_ROLE_DELETE",
            &json!({"guild_id": "10", "role_id": "31"})
        ),
        json!({"topic": "roles", "op": "remove", "id": "31"})
    );
}

#[test]
fn decode_preserves_unmodeled_permission_bits() {
    let high_bit = 1_u64 << 63;
    let permissions = (high_bit | 1024).to_string();
    let allow = (high_bit | 2048).to_string();
    let deny = u64::MAX.to_string();
    let role = json!({"guild_id": "10", "role": {"id": "31", "permissions": permissions}});
    let channel = json!({"guild_id": "10", "id": "20", "name": "Lobby", "type": 2,
        "permission_overwrites": [{"type": 0, "id": "10", "allow": allow, "deny": deny}]});
    assert_eq!(
        operation("GUILD_ROLE_UPDATE", &role)["value"]["permissions"],
        permissions
    );
    let overwrites = &operation("CHANNEL_UPDATE", &channel)["value"]["overwrites"];
    assert_eq!(overwrites[0]["allow"], allow);
    assert_eq!(overwrites[0]["deny"], deny);
}

#[test]
fn malformed_permission_masks_cannot_replace_rows() {
    let overflow = (u128::from(u64::MAX) + 1).to_string();
    for mask in [Value::Null, json!("+0"), json!(overflow)] {
        let role = json!({"guild_id": "10", "role": {"id": "31", "permissions": mask}});
        let mut channel = channel(2);
        channel["permission_overwrites"][0]["deny"] = mask;
        assert_invalid_payload("GUILD_ROLE_UPDATE", &role);
        assert_invalid_payload("CHANNEL_UPDATE", &channel);
    }
}

#[test]
fn direct_messages_threads_other_guilds_and_unrelated_events_are_ignored() {
    let mut dm = channel(1);
    dm["guild_id"] = Value::Null;
    let mut foreign = channel(99);
    foreign["guild_id"] = json!("11");
    for (kind, payload) in [
        ("CHANNEL_CREATE", dm),
        ("CHANNEL_UPDATE", foreign),
        ("CHANNEL_UPDATE", channel(12)),
        (
            "MESSAGE_DELETE",
            json!({"guild_id": "10", "channel_id": "20", "id": "78"}),
        ),
    ] {
        assert!(matches!(project(GUILD_ID, kind, &payload), Ok(None)));
    }
}
