use super::*;
use serde_json::json;

const GUILD: GuildId = Id::new(10);
const BAD_CHUNK: &str = r#"{"op":0,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","nonce":"{nonce}","chunk_index":1,"chunk_count":2,"members":[{"user":{"id":"32","extra":1,"extra":2},"roles":[],"pending":false}]}}"#;
const NOW: WireTime = WireTime(time::OffsetDateTime::UNIX_EPOCH);
const VOICE: &str = r#"{"op":0,"t":"VOICE_STATE_UPDATE","d":{"guild_id":"10","user_id":"31","channel_id":"21",
    "session_id":"synthetic-session","deaf":false,"mute":false,"self_deaf":false,"self_mute":false,"self_video":false,"suppress":false}}"#;
const DROP_ROOM: &str = r#"{"op":0,"t":"CHANNEL_DELETE","d":{"guild_id":"10","id":"20","type":2}}"#;
const ADD_ROLE: &str = r#"{"op":0,"t":"GUILD_ROLE_CREATE","d":{"guild_id":"10","role":{"id":"41","permissions":"16"}}}"#;

fn frame(kind: &str, body: Value) -> String {
    json!({"op":0,"t":kind,"d":body}).to_string()
}

fn request_id() -> String {
    let mut bytes = [0; 16];
    let rng = rustls::crypto::ring::default_provider().secure_random;
    rng.fill(&mut bytes).expect("random ID");
    hex::encode(bytes)
}

fn start(join: &mut BaselineJoin, id: &str, roles: &[&str]) {
    let create = r#"{"op":0,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"31","afk_channel_id":null,"member_count":2,
        "channels":[{"id":"20","name":"Initial","type":2,"permission_overwrites":[]},
                    {"id":"22","name":"Category","type":4,"position":3,"permission_overwrites":[]}],
        "roles":[{"id":"10","permissions":"0"}],"voice_states":[{"user_id":"31","channel_id":"20"}],
        "guild_scheduled_events":[]}}"#;
    let bad = create.replace(r#""member_count""#, r#""x":1,"x":2,"member_count""#);
    assert_eq!(join.begin(&bad, id.into()).err(), Some(AmbiguousDispatch));
    assert!(matches!(join.begin(create, id.to_owned()), Ok(Some(_))));
    pending(join, &chunk(id, 0, "31", json!(roles)));
}

fn room(id: &str, name: &str) -> Value {
    json!({"id":id,"guild_id":"10","name":name,"type":2,"parent_id":null,"permission_overwrites":[]})
}

fn chunk(id: &str, index: u8, user: &str, roles: Value) -> String {
    let body = json!({"guild_id":"10", "nonce":id, "chunk_index":index, "chunk_count":2,
        "members":[{"user":{"id":user},"roles":roles,"pending":false}]});
    frame("GUILD_MEMBERS_CHUNK", body)
}

fn pending(join: &mut BaselineJoin, raw: &str) {
    assert!(join.observe(raw, NOW).expect(raw).is_none(), "{raw}");
}

#[test]
fn joins_changes_between_chunks_without_guessing_a_cascade() {
    let mut join = BaselineJoin::new(GUILD);
    let id = request_id();
    start(&mut join, &id, &["41"]);
    let event = json!({"guild_id":"10", "id":"80", "channel_id":"21", "entity_type":2,
        "privacy_level":2, "status":1, "name":"Community event",
        "scheduled_start_time":"2026-11-21T13:00:00Z"});
    for raw in [
        frame("CHANNEL_CREATE", room("21", "New room")).as_str(),
        ADD_ROLE,
        &frame("GUILD_SCHEDULED_EVENT_CREATE", event),
        DROP_ROOM,
        VOICE,
        &frame("CHANNEL_UPDATE", room("21", "Final room")),
        r#"{"op":0,"t":"CHANNEL_UPDATE","d":{"guild_id":"10","id":"22","type":4,"name":"Renamed","permission_overwrites":[]}}"#,
        r#"{"op":0,"t":"GUILD_UPDATE","d":{"id":"10","owner_id":"32","afk_channel_id":"21","name":"Guild",
            "afk_timeout":60,"default_message_notifications":0,"emojis":[],"features":[],"explicit_content_filter":0,
            "mfa_level":0,"nsfw_level":0,"preferred_locale":"en-US","premium_progress_bar_enabled":false,"premium_tier":0,"roles":[],"system_channel_flags":0,"verification_level":0}}"#,
        r#"{"op":0,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"41","permissions":"32"}}}"#,
        r#"{"op":0,"t":"CHANNEL_DELETE","d":{"guild_id":"11","id":"21","id":"22","type":2}}"#,
    ] {
        pending(&mut join, raw);
    }
    let last = chunk(&id, 1, "32", json!([]));
    let topics = join.observe(&last, NOW).expect("valid topics");
    let json = serde_json::to_value(topics.expect("joined")).expect("JSON");
    assert_eq!(json["guild"], json!({"ownerId":"32","afkChannelId":"21"}));
    assert_eq!(json["channels"].as_array().expect("channels").len(), 2);
    assert_eq!(json["channels"][0]["name"], "Final room");
    assert_eq!(json["channels"][1]["name"], "Renamed");
    assert_eq!(json["channels"][1]["position"], 3);
    assert_eq!(json["roles"][1], json!({"id":"41","permissions":"32"}));
    assert_eq!(json["members"][0]["roleIds"], json!(["41"]));
    assert_eq!(json["voice"], json!([{"userId":"31","channelId":"21"}]));
    assert_eq!(json["events"][0]["channelId"], "21");
    pending(&mut join, &last);
}

#[test]
fn a_dangling_voice_or_member_reference_consumes_the_attempt() {
    for raw in [
        DROP_ROOM,
        r#"{"op":0,"t":"GUILD_ROLE_DELETE","d":{"guild_id":"10","role_id":"41"}}"#,
        &VOICE.replace(r#""21""#, r#""99""#),
    ] {
        let mut join = BaselineJoin::new(GUILD);
        let id = request_id();
        start(&mut join, &id, &["41"]);
        pending(&mut join, ADD_ROLE);
        pending(&mut join, raw);
        let last = chunk(&id, 1, "32", json!([]));
        assert!(matches!(join.observe(&last, NOW), Err(InvalidTopics)));
        pending(&mut join, &last);
    }
}

#[test]
fn malformed_rows_retire_both_collectors_before_replacement() {
    let mut room_with_duplicates = room("20", "Room");
    let entry = json!({"id":"10","type":0,"allow":"0","deny":"0"});
    room_with_duplicates["permission_overwrites"] = json!([entry, entry]);
    let id = request_id();
    for raw in [
        r#"{"op":0,"t":"CHANNEL_DELETE","d":{"guild_id":"10","guild_id":"11","id":"20","type":2}}"#,
        r#"{"op":0,"t":"CHANNEL_DELETE","d":{"id":"20","type":2}}"#,
        r#"{"op":0,"t":"CHANNEL_UPDATE","d":{"guild_id":"10","id":"20","type":2,"name":"Room","permission_overwrites":[]}}"#,
        r#"{"op":0,"t":"CHANNEL_UPDATE","d":{"guild_id":"10","id":"20","type":11}}"#,
        &frame("CHANNEL_UPDATE", room("20", "")),
        &frame("CHANNEL_UPDATE", room_with_duplicates),
        r#"{"op":0,"t":"GUILD_SCHEDULED_EVENT_DELETE","d":{"guild_id":"10","id":"80","extra":1,"extra":2}}"#,
        &BAD_CHUNK.replace("{nonce}", &id),
    ] {
        let mut join = BaselineJoin::new(GUILD);
        start(&mut join, &id, &[]);
        let error = join.observe(raw, NOW).err();
        assert!(matches!(error, Some(AmbiguousDispatch | InvalidTopics)));
        pending(&mut join, &frame("CHANNEL_UPDATE", room("20", "Restored")));
        pending(&mut join, &chunk(&id, 1, "32", json!([])));
    }
}

#[test]
fn a_retired_generation_cannot_finish_its_replacement() {
    let mut join = BaselineJoin::new(GUILD);
    let [first, retired, active] = std::array::from_fn(|_| request_id());
    let unavailable = r#"{"op":0,"t":"GUILD_DELETE","d":{"id":"10","unavailable":true}}"#;
    let resumed = r#"{"op":0,"t":"RESUMED","d":{}}"#;
    for (id, raw) in [(&first, unavailable), (&retired, resumed)] {
        start(&mut join, id, &[]);
        pending(&mut join, raw);
        pending(&mut join, &chunk(id, 1, "32", json!([])));
    }
    start(&mut join, &active, &[]);
    pending(&mut join, &BAD_CHUNK.replace("{nonce}", &retired));
    let last = chunk(&active, 1, "32", json!([]));
    assert!(matches!(join.observe(&last, NOW), Ok(Some(_))));
}
