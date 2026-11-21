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
        let payload = json!({
            "guild_id": "10", "channel_id": room, "user_id": "42", "session_id": "synthetic-session",
            "deaf": false, "mute": false, "self_deaf": false, "self_mute": false,
            "self_video": false, "suppress": false
        });
        assert_eq!(operation("VOICE_STATE_UPDATE", &payload), expected);
        assert!(matches!(
            project(Id::new(11), "VOICE_STATE_UPDATE", &payload),
            Ok(None)
        ));
    }
}

#[test]
fn owner_afk_and_role_changes_preserve_authorization_inputs() {
    let guild = json!({
        "id": "10", "name": "not forwarded", "owner_id": "42", "afk_channel_id": "20",
        "afk_timeout": 60, "default_message_notifications": 0, "emojis": [], "features": [],
        "explicit_content_filter": 0, "mfa_level": 0, "nsfw_level": 0, "preferred_locale": "en-US",
        "premium_progress_bar_enabled": false, "premium_tier": 0, "roles": [],
        "system_channel_flags": 0, "verification_level": 0
    });
    assert_eq!(
        operation("GUILD_UPDATE", &guild),
        json!({
            "topic": "guild", "op": "upsert", "id": "10", "value": {"ownerId": "42", "afkChannelId": "20"}
        })
    );
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
    let role = serde_json::from_str(
        r#"{"guild_id":"10","role":{"id":"31","permissions":"9223372036854776832"}}"#,
    )
    .expect("gateway role payload");
    let channel = serde_json::from_str(r#"{"guild_id":"10","id":"20","name":"Lobby","type":2,
        "permission_overwrites":[{"type":0,"id":"10","allow":"9223372036854777856","deny":"18446744073709551615"}]}"#)
        .expect("gateway channel payload");
    assert_eq!(
        operation("GUILD_ROLE_UPDATE", &role)["value"]["permissions"],
        "9223372036854776832"
    );
    let overwrites = &operation("CHANNEL_UPDATE", &channel)["value"]["overwrites"];
    assert_eq!(overwrites[0]["allow"], "9223372036854777856");
    assert_eq!(overwrites[0]["deny"], "18446744073709551615");
}

#[test]
fn malformed_permission_masks_cannot_replace_rows() {
    for mask in [Value::Null, json!("+0"), json!("18446744073709551616")] {
        let role = json!({"guild_id": "10", "role": {"id": "31", "permissions": mask}});
        let mut channel = channel(2);
        channel["permission_overwrites"][0]["deny"] = mask;
        for (kind, payload) in [("GUILD_ROLE_UPDATE", role), ("CHANNEL_UPDATE", channel)] {
            assert!(matches!(
                project(GUILD_ID, kind, &payload),
                Err(ProjectionError::InvalidPayload)
            ));
        }
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
