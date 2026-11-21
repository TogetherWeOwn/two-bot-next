use serde_json::{json, Value};

use super::*;

const GUILD_ID: Id<GuildMarker> = Id::new(10);

fn dispatch(kind: &str, body: Value) -> String {
    json!({"op": 0, "s": 12, "t": kind, "d": body}).to_string()
}

fn operation(kind: &str, body: Value) -> Value {
    let op = project(GUILD_ID, &dispatch(kind, body))
        .expect("supported dispatch")
        .expect("allowlisted operation");
    let bytes = wire::encode_delta(
        wire_id(GUILD_ID),
        9,
        wire::WireTime(OffsetDateTime::UNIX_EPOCH),
        &[op],
    )
    .expect("valid realtime delta");
    let decoded: Value = serde_json::from_slice(&bytes).expect("delta JSON");
    decoded["ops"][0].clone()
}

#[test]
fn membership_changes_keep_only_v1_fields_and_explicit_screening_state() {
    let mut body = json!({
        "guild_id": "10", "user": {"id": "42", "username": "excluded", "avatar": "excluded"},
        "roles": ["18", "20"], "nick": "Theo", "pending": true,
        "communication_disabled_until": "2026-11-21T15:00:00Z"
    });
    assert_eq!(
        operation("GUILD_MEMBER_ADD", body.clone()),
        json!({"topic": "members", "op": "upsert", "id": "42", "value": {
            "userId": "42", "roleIds": ["18", "20"], "nick": "Theo", "bot": false, "pending": true
        }})
    );
    body["user"]["bot"] = json!(true);
    body["roles"] = json!([]);
    body["nick"] = Value::Null;
    body["pending"] = json!(false);
    assert_eq!(
        operation("GUILD_MEMBER_UPDATE", body),
        json!({"topic": "members", "op": "upsert", "id": "42", "value": {
            "userId": "42", "roleIds": [], "nick": null, "bot": true, "pending": false
        }})
    );
    assert_eq!(
        operation(
            "GUILD_MEMBER_REMOVE",
            json!({"guild_id": "10", "user": {"id": "42"}})
        ),
        json!({"topic": "members", "op": "remove", "id": "42"})
    );
}

#[test]
fn missing_null_or_duplicate_screening_cannot_become_cleared() {
    let body = json!({"guild_id": "10", "user": {"id": "42"}, "roles": []});
    for kind in ["GUILD_MEMBER_ADD", "GUILD_MEMBER_UPDATE"] {
        for input in [
            body.clone(),
            json!({"guild_id": "10", "user": {"id": "42"}, "roles": [], "pending": null}),
        ] {
            assert!(matches!(
                project(GUILD_ID, &dispatch(kind, input)),
                Err(ProjectionError::MissingScreeningState)
            ));
        }
    }
    assert!(matches!(
        project(
            GUILD_ID,
            r#"{"op":0,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"10","user":{"id":"42"},"roles":[],"pending":true,"pending":false}}"#
        ),
        Err(ProjectionError::MalformedDispatch)
    ));
}

fn event() -> Value {
    json!({
        "guild_id": "10", "id": "94", "entity_type": 3, "privacy_level": 2, "status": 1,
        "channel_id": null, "name": "Tournament", "description": "Doors open soon",
        "entity_metadata": {"location": "Venue"}, "creator_id": "42", "user_count": 7,
        "recurrence_rule": {"excluded": true}, "image": "excluded",
        "scheduled_start_time": "2026-11-21T15:00:00.123456+02:00",
        "scheduled_end_time": "2026-11-21T17:00:00+02:00"
    })
}

#[test]
fn scheduled_event_projection_uses_v1_fields_and_normalizes_timestamps() {
    assert_eq!(
        operation("GUILD_SCHEDULED_EVENT_CREATE", event()),
        json!({"topic": "events", "op": "upsert", "id": "94", "value": {
            "id": "94", "channelId": null, "status": "scheduled", "title": "Tournament",
            "description": "Doors open soon", "location": "Venue",
            "startsAt": "2026-11-21T13:00:00.123456Z", "endsAt": "2026-11-21T15:00:00Z"
        }})
    );
    for entity_type in [1, 2] {
        let mut input = event();
        input["entity_type"] = json!(entity_type);
        input["status"] = json!(2);
        input["channel_id"] = json!("21");
        input["description"] = Value::Null;
        input["scheduled_start_time"] = json!("2026-11-21T13:00:00.123456Z");
        input["scheduled_end_time"] = Value::Null;
        assert_eq!(
            operation("GUILD_SCHEDULED_EVENT_UPDATE", input),
            json!({"topic": "events", "op": "upsert", "id": "94", "value": {
                "id": "94", "channelId": "21", "status": "active", "title": "Tournament",
                "description": null, "location": null, "startsAt": "2026-11-21T13:00:00.123456Z", "endsAt": null
            }})
        );
    }
}

#[test]
fn terminal_updates_and_deletes_remove_the_event() {
    let removed = json!({"topic": "events", "op": "remove", "id": "94"});
    for status in [3, 4] {
        let mut input = event();
        input["status"] = json!(status);
        assert_eq!(operation("GUILD_SCHEDULED_EVENT_UPDATE", input), removed);
    }
    let delete = json!({"guild_id": "10", "id": "94"});
    assert_eq!(operation("GUILD_SCHEDULED_EVENT_DELETE", delete), removed);
}

#[test]
fn unknown_or_inconsistent_event_visibility_fails_closed() {
    for (field, value) in [
        ("status", json!(99)),
        ("privacy_level", json!(99)),
        ("entity_type", json!(99)),
        ("channel_id", json!("21")),
        ("entity_type", json!(2)),
    ] {
        let mut input = event();
        input[field] = value;
        assert!(matches!(
            project(GUILD_ID, &dispatch("GUILD_SCHEDULED_EVENT_UPDATE", input)),
            Err(ProjectionError::UnsupportedEvent)
        ));
    }
}

#[test]
fn unrelated_dispatches_are_not_projected_and_missing_scope_is_an_error() {
    for raw in [
        dispatch(
            "GUILD_MEMBER_UPDATE",
            json!({"guild_id": "11", "pending": "malformed"}),
        ),
        dispatch("GUILD_SCHEDULED_EVENT_CREATE", json!({"guild_id": "11"})),
        dispatch(
            "GUILD_SCHEDULED_EVENT_USER_ADD",
            json!({"guild_id": "10", "user_id": "42"}),
        ),
        r#"{"op":0,"t":"MESSAGE_CREATE","d":{"guild_id":"10","content":"not forwarded"}}"#.into(),
        r#"{"op": 11, "d": null}"#.into(),
        r#"{"op": 11, "t": "GUILD_MEMBER_ADD", "d": null}"#.into(),
        r#"{"op": 11, "t": 42, "d": null}"#.into(),
    ] {
        assert!(matches!(project(GUILD_ID, &raw), Ok(None)), "{raw}");
    }
    for raw in [
        r#"{"op":0,"d":{"guild_id":"10"}}"#.into(),
        r#"{"op":0,"t":null,"d":{"guild_id":"10"}}"#.into(),
        r#"{"op":0,"t":42,"d":{"guild_id":"10"}}"#.into(),
        r#"{"op":0,"t":{"GUILD_SCHEDULED_EVENT_DELETE":null},"d":{"guild_id":"10","id":"94"}}"#.into(),
        r#"[0,"GUILD_SCHEDULED_EVENT_DELETE",{"guild_id":"10","id":"94"}]"#.into(),
        dispatch("GUILD_MEMBER_REMOVE", json!({"user": {"id": "42"}})),
        dispatch("GUILD_SCHEDULED_EVENT_DELETE", json!(["10"])),
        r#"{"op":0,"t":"GUILD_SCHEDULED_EVENT_DELETE","d":{"guild_id":"11","guild_id":"10","id":"94"}}"#.into(),
        r#"{"op":0,"t":"GUILD_MEMBER_REMOVE","d":null}"#.into(),
    ] {
        assert!(matches!(
            project(GUILD_ID, &raw),
            Err(ProjectionError::MalformedDispatch)
        ));
    }
}
