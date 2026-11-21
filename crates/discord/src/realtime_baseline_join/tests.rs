use super::*;
use serde_json::json;
use time::OffsetDateTime;

const GUILD: Id<GuildMarker> = Id::new(10);
const NOW: WireTime = WireTime(OffsetDateTime::UNIX_EPOCH);

fn frame(kind: &str, body: Value) -> String {
    json!({"op": 0, "t": kind, "d": body}).to_string()
}

fn start(join: &mut BaselineJoin) -> String {
    let mut bytes = [0; 16];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut bytes)
        .expect("request ID");
    let nonce = hex::encode(bytes);
    let body = json!({
        "id":"10", "owner_id":"31", "afk_channel_id":null, "member_count":2,
        "channels":[room("20", "Initial room")], "roles":[{"id":"10","permissions":"0"}],
        "voice_states":[{"user_id":"31","channel_id":"20"}], "guild_scheduled_events":[]
    });
    let started = join.begin(&frame("GUILD_CREATE", body), nonce.clone());
    assert!(matches!(started, Ok(Some(_))));
    nonce
}

fn room(id: &str, name: &str) -> Value {
    json!({"id":id,"guild_id":"10","name":name,"type":2,"parent_id":null,"permission_overwrites":[]})
}

fn chunk(nonce: &str, index: u8, user: &str, roles: Value) -> String {
    let body = json!({
        "guild_id":"10", "nonce":nonce, "chunk_index":index, "chunk_count":2,
        "members":[{"user":{"id":user},"roles":roles,"pending":false}]
    });
    frame("GUILD_MEMBERS_CHUNK", body)
}

fn voice(room: &str) -> String {
    let body = json!({
        "guild_id":"10", "channel_id":room, "user_id":"31", "session_id":"not a wire field",
        "deaf":false,"mute":false,"self_deaf":false,"self_mute":false,"self_video":false,"suppress":false
    });
    frame("VOICE_STATE_UPDATE", body)
}

fn remove_room(guild: &str, room: &str) -> String {
    let body = json!({"guild_id":guild,"id":room,"type":2});
    frame("CHANNEL_DELETE", body)
}

fn role(kind: &str, mask: &str) -> String {
    frame(
        kind,
        json!({"guild_id":"10","role":{"id":"41","permissions":mask}}),
    )
}

fn pending(join: &mut BaselineJoin, raw: &str) {
    assert!(matches!(join.observe(raw, NOW), Ok(None)));
}

#[test]
fn joins_changes_between_chunks_without_guessing_a_cascade() {
    let mut join = BaselineJoin::new(GUILD);
    let nonce = start(&mut join);
    let event = json!({
        "guild_id":"10", "id":"80", "channel_id":"21", "entity_type":2, "privacy_level":2,
        "status":1, "name":"Community event", "scheduled_start_time":"2026-11-21T13:00:00Z"
    });
    for raw in [
        chunk(&nonce, 0, "31", json!(["41"])),
        frame("CHANNEL_CREATE", room("21", "New room")),
        role("GUILD_ROLE_CREATE", "16"),
        frame("GUILD_SCHEDULED_EVENT_CREATE", event),
        remove_room("10", "20"),
        voice("21"),
        frame("CHANNEL_UPDATE", room("21", "Final room")),
        role("GUILD_ROLE_UPDATE", "32"),
        remove_room("11", "21"),
    ] {
        pending(&mut join, &raw);
    }
    let final_chunk = chunk(&nonce, 1, "32", json!([]));
    let result = join.observe(&final_chunk, NOW);
    let topics = result.expect("valid references").expect("all six topics");
    let json = serde_json::to_value(topics).expect("serialize topics");
    assert_eq!(json["guild"], json!({"ownerId":"31","afkChannelId":null}));
    assert_eq!(json["channels"].as_array().expect("rooms").len(), 1);
    assert_eq!(json["channels"][0]["name"], "Final room");
    assert_eq!(json["roles"][1], json!({"id":"41","permissions":"32"}));
    assert_eq!(json["members"][0]["roleIds"], json!(["41"]));
    assert_eq!(json["voice"], json!([{"userId":"31","channelId":"21"}]));
    assert_eq!(json["events"][0]["channelId"], "21");
    pending(&mut join, &final_chunk);
}

#[test]
fn a_dangling_voice_or_member_reference_consumes_the_attempt() {
    for mutation in [
        remove_room("10", "20"),
        frame("GUILD_ROLE_DELETE", json!({"guild_id":"10","role_id":"41"})),
        voice("99"),
    ] {
        let mut join = BaselineJoin::new(GUILD);
        let nonce = start(&mut join);
        pending(&mut join, &role("GUILD_ROLE_CREATE", "16"));
        pending(&mut join, &chunk(&nonce, 0, "31", json!(["41"])));
        pending(&mut join, &mutation);
        let last = chunk(&nonce, 1, "32", json!([]));
        let result = join.observe(&last, NOW);
        assert!(matches!(result, Err(JoinError::InvalidTopics)));
        pending(&mut join, &last);
    }
}

#[test]
fn ambiguous_topology_retires_both_collectors() {
    for raw in [
        r#"{"op":0,"t":"CHANNEL_DELETE","d":{"guild_id":"10","guild_id":"11","id":"20","type":2}}"#,
        r#"{"op":0,"t":"CHANNEL_DELETE","d":{"id":"20","type":2}}"#,
        r#"{"op":0,"t":"CHANNEL_UPDATE","d":{"id":"20","guild_id":"10","type":11}}"#,
    ] {
        let mut join = BaselineJoin::new(GUILD);
        let nonce = start(&mut join);
        pending(&mut join, &chunk(&nonce, 0, "31", json!([])));
        let result = join.observe(raw, NOW);
        assert!(matches!(result, Err(JoinError::AmbiguousDispatch)));
        pending(&mut join, &chunk(&nonce, 1, "32", json!([])));
    }
}

#[test]
fn a_retired_generation_cannot_finish_its_replacement() {
    let mut join = BaselineJoin::new(GUILD);
    let old = start(&mut join);
    pending(&mut join, &chunk(&old, 0, "31", json!([])));
    let unavailable = frame("GUILD_DELETE", json!({"id":"10","unavailable":true}));
    pending(&mut join, &unavailable);
    let old_last = chunk(&old, 1, "32", json!([]));
    pending(&mut join, &old_last);
    let current = start(&mut join);
    pending(&mut join, &chunk(&current, 0, "31", json!([])));
    pending(&mut join, &old_last);
    assert!(matches!(
        join.observe(&chunk(&current, 1, "32", json!([])), NOW),
        Ok(Some(_))
    ));
}
