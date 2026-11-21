use super::*;
use serde_json::{json, Value};
use IntakeError::{Chunk, InvalidEvents, MemberChanged, Projection};
use MemberBaselineError::{InconsistentChunk, MalformedChunk};
use ProjectionError::{MalformedDispatch, UnsupportedEvent};

const GUILD: Id<GuildMarker> = Id::new(10);

fn dispatch(kind: &str, data: Value) -> String {
    json!({"op": 0, "t": kind, "d": data}).to_string()
}

fn guild(count: u64, events: Value) -> String {
    let data = json!({"id": "10", "member_count": count, "guild_scheduled_events": events});
    dispatch("GUILD_CREATE", data)
}

fn event(id: usize, title: &str) -> Value {
    json!({
        "id": id.to_string(), "guild_id": "10", "entity_type": 3, "privacy_level": 2,
        "status": 1, "name": title, "entity_metadata": {"location": "Community hall"},
        "scheduled_start_time": "2026-11-21T13:00:00Z", "scheduled_end_time": "2026-11-21T14:00:00Z"
    })
}

fn request_id() -> String {
    let mut bytes = [0; 16];
    let rng = rustls::crypto::ring::default_provider().secure_random;
    rng.fill(&mut bytes).expect("random ID");
    hex::encode(bytes)
}

fn chunk(id: &str, index: u8, count: u8, user_id: &str) -> String {
    let data = json!({
        "guild_id": "10", "nonce": id, "chunk_index": index, "chunk_count": count,
        "members": [{"user": {"id": user_id}, "roles": [], "pending": false}]
    });
    dispatch("GUILD_MEMBERS_CHUNK", data)
}

fn start(intake: &mut BaselineIntake, count: u64, events: Value, id: &str) {
    let request = intake.begin(&guild(count, events), id.to_owned());
    assert!(request.expect("valid guild create").is_some());
}

fn finish(intake: &mut BaselineIntake, id: &str, chunks: u8) -> BaselineRows {
    let result = intake.observe(&chunk(id, 0, chunks, "31"));
    result.expect("valid final chunk").expect("baseline rows")
}
fn assert_late_chunk_ignored(intake: &mut BaselineIntake, id: &str) {
    assert!(matches!(intake.observe(&chunk(id, 0, 1, "31")), Ok(None)));
}

#[test]
fn joins_the_roster_with_events_at_the_final_chunks_receive_position() {
    let mut intake = BaselineIntake::new(GUILD);
    let id = request_id();
    let initial = json!([event(91, "Old"), event(92, "Gone")]);
    start(&mut intake, 2, initial, &id);
    let deleted = json!({"guild_id":"10", "id":"92"});
    let foreign = json!({"guild_id":"11", "user":{"id":"32"}});
    for frame in [
        chunk(&id, 1, 2, "32"),
        dispatch("GUILD_SCHEDULED_EVENT_DELETE", deleted),
        dispatch("GUILD_SCHEDULED_EVENT_UPDATE", event(91, "Replaced")),
        dispatch("GUILD_SCHEDULED_EVENT_CREATE", event(91, "New")),
        dispatch("GUILD_MEMBER_REMOVE", foreign),
    ] {
        assert!(matches!(intake.observe(&frame), Ok(None)));
    }
    let rows = finish(&mut intake, &id, 2);
    assert_eq!(rows.members[0].user_id.0.get(), 31);
    assert_eq!(rows.members[1].user_id.0.get(), 32);
    assert_eq!(rows.events.len(), 1);
    assert_eq!(rows.events[0].title, "New");
    assert_late_chunk_ignored(&mut intake, &id);
}

#[test]
fn member_changes_and_retirement_prevent_late_chunks_from_finishing() {
    let mut intake = BaselineIntake::new(GUILD);
    let [first, retired, active] = std::array::from_fn(|_| request_id());
    start(&mut intake, 1, json!([]), &first);
    let member = json!({"guild_id":"10", "user":{"id":"31"}});
    let change = dispatch("GUILD_MEMBER_REMOVE", member);
    assert_eq!(intake.observe(&change).err(), Some(MemberChanged));
    assert_late_chunk_ignored(&mut intake, &first);
    start(&mut intake, 1, json!([]), &retired);
    intake.retire();
    start(&mut intake, 1, json!([]), &active);
    assert_late_chunk_ignored(&mut intake, &retired);
    finish(&mut intake, &active, 1);
}

#[test]
fn replacement_requires_explicit_count_and_events_from_the_same_dispatch() {
    for data in [
        json!({"id":"10", "guild_scheduled_events": []}),
        json!({"id":"10", "member_count": "1", "guild_scheduled_events": []}),
        json!({"id":"10", "member_count": 1}),
    ] {
        let mut intake = BaselineIntake::new(GUILD);
        let id = request_id();
        start(&mut intake, 1, json!([]), &id);
        let bad_start = intake.begin(&dispatch("GUILD_CREATE", data), request_id());
        assert_eq!(bad_start.err(), Some(Projection(MalformedDispatch)));
        assert_late_chunk_ignored(&mut intake, &id);
    }
    let mut intake = BaselineIntake::new(GUILD);
    let empty = intake.begin(&guild(0, json!([])), request_id());
    assert!(matches!(empty, Ok(None)));
    let invalid = intake.begin(&guild(1, json!([event(91, "")])), request_id());
    assert_eq!(invalid.err(), Some(Projection(UnsupportedEvent)));
}

#[test]
fn invalid_input_cannot_be_repaired_by_a_later_chunk() {
    let mut empty_terminal = event(91, "");
    empty_terminal["status"] = json!(4);
    let mut terminal = empty_terminal.clone();
    terminal["name"] = json!("Cancelled");
    terminal["scheduled_start_time"] = json!("not-a-date");
    let mut external = event(91, "External");
    let fields = external.as_object_mut().expect("event object");
    fields.retain(|key, _| key != "entity_metadata" && key != "scheduled_end_time");
    let update = |row| dispatch("GUILD_SCHEDULED_EVENT_UPDATE", row);
    let id = request_id();
    let bad_chunk = dispatch("GUILD_MEMBERS_CHUNK", json!({"guild_id":"10", "nonce":id}));
    let empty_title = dispatch("GUILD_SCHEDULED_EVENT_CREATE", event(91, ""));
    let incomplete = update(json!({"guild_id":"10", "id":"91"}));
    for (bad, expected) in [
        (bad_chunk, Chunk(MalformedChunk)),
        (chunk(&id, 0, 2, "31"), Chunk(InconsistentChunk)),
        (r#"{"op":0,"d":{}}"#.into(), Projection(MalformedDispatch)),
        (empty_title, Projection(UnsupportedEvent)),
        (incomplete, Projection(MalformedDispatch)),
        (update(terminal), Projection(MalformedDispatch)),
        (update(empty_terminal), Projection(UnsupportedEvent)),
        (update(external), Projection(UnsupportedEvent)),
    ] {
        let mut intake = BaselineIntake::new(GUILD);
        start(&mut intake, 1, json!([event(91, "Existing")]), &id);
        assert_eq!(intake.observe(&bad).err(), Some(expected), "{bad}");
        assert_late_chunk_ignored(&mut intake, &id);
    }
}

#[test]
fn event_capacity_allows_replacement_but_not_an_extra_key() {
    let mut intake = BaselineIntake::new(GUILD);
    let [first, overflow] = std::array::from_fn(|_| request_id());
    let max = wire::MAX_SNAPSHOT_EVENTS;
    let events: Vec<_> = (1..=max).map(|id| event(id, "Event")).collect();
    start(&mut intake, 1, json!(events), &first);
    let update = dispatch("GUILD_SCHEDULED_EVENT_UPDATE", event(1, "Updated"));
    assert!(matches!(intake.observe(&update), Ok(None)));
    let rows = finish(&mut intake, &first, 1);
    assert_eq!(rows.events.len(), max);
    assert_eq!(rows.events[0].title, "Updated");
    start(&mut intake, 1, json!(events), &overflow);
    let extra = dispatch("GUILD_SCHEDULED_EVENT_CREATE", event(max + 1, "Extra"));
    assert_eq!(intake.observe(&extra).err(), Some(InvalidEvents));
    assert_late_chunk_ignored(&mut intake, &overflow);
}
