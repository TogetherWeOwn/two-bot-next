use serde_json::{json, Value};

use super::*;
use IntakeError::{Chunk, InvalidEvents, Projection};
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

fn fixture_nonce(attempt: u8) -> String {
    format!("{}-{attempt}", std::process::id())
}

fn chunk(attempt: u8, index: u8, count: u8, user_id: &str) -> String {
    let data = json!({
        "guild_id": "10", "nonce": fixture_nonce(attempt), "chunk_index": index, "chunk_count": count,
        "members": [{"user": {"id": user_id}, "roles": [], "pending": false}]
    });
    dispatch("GUILD_MEMBERS_CHUNK", data)
}

fn start(intake: &mut BaselineIntake, count: u64, events: Value, attempt: u8) {
    let request = intake.begin(&guild(count, events), fixture_nonce(attempt));
    assert!(request.expect("valid guild create").is_some());
}

fn finish(intake: &mut BaselineIntake, attempt: u8, chunks: u8) -> BaselineRows {
    let result = intake.observe(&chunk(attempt, 0, chunks, "31"));
    result.expect("valid final chunk").expect("baseline rows")
}

fn assert_late_chunk_ignored(intake: &mut BaselineIntake, attempt: u8) {
    let result = intake.observe(&chunk(attempt, 0, 1, "31"));
    assert!(matches!(result, Ok(None)));
}

#[test]
fn joins_the_roster_with_events_at_the_final_chunks_receive_position() {
    let mut intake = BaselineIntake::new(GUILD);
    let initial = json!([event(91, "Old"), event(92, "Gone")]);
    start(&mut intake, 2, initial, 1);
    let deleted = json!({"guild_id":"10", "id":"92"});
    let foreign = json!({"guild_id":"11", "user":{"id":"32"}});
    for frame in [
        chunk(1, 1, 2, "32"),
        dispatch("GUILD_SCHEDULED_EVENT_DELETE", deleted),
        dispatch("GUILD_SCHEDULED_EVENT_UPDATE", event(91, "Replaced")),
        dispatch("GUILD_SCHEDULED_EVENT_CREATE", event(91, "New")),
        dispatch("GUILD_MEMBER_REMOVE", foreign),
    ] {
        assert!(matches!(intake.observe(&frame), Ok(None)));
    }
    let rows = finish(&mut intake, 1, 2);
    let members = serde_json::to_value(rows.members).expect("member rows");
    assert_eq!(members[0]["userId"], "31");
    assert_eq!(members[1]["userId"], "32");
    assert_eq!(rows.events.len(), 1);
    assert_eq!(rows.events[0].title, "New");
    assert_late_chunk_ignored(&mut intake, 1);
}

#[test]
fn member_changes_and_retirement_prevent_late_chunks_from_finishing() {
    let mut intake = BaselineIntake::new(GUILD);
    start(&mut intake, 1, json!([]), 1);
    let member = json!({"guild_id":"10", "user":{"id":"31"}});
    let result = intake.observe(&dispatch("GUILD_MEMBER_REMOVE", member));
    assert_eq!(result.err(), Some(IntakeError::MemberChanged));
    assert_late_chunk_ignored(&mut intake, 1);
    start(&mut intake, 1, json!([]), 2);
    intake.retire();
    start(&mut intake, 1, json!([]), 3);
    assert_late_chunk_ignored(&mut intake, 2);
    finish(&mut intake, 3, 1);
}

#[test]
fn replacement_requires_explicit_count_and_events_from_the_same_dispatch() {
    for data in [
        json!({"id":"10", "guild_scheduled_events": []}),
        json!({"id":"10", "member_count": "1", "guild_scheduled_events": []}),
        json!({"id":"10", "member_count": 1}),
    ] {
        let mut intake = BaselineIntake::new(GUILD);
        start(&mut intake, 1, json!([]), 1);
        let bad_start = intake.begin(&dispatch("GUILD_CREATE", data), fixture_nonce(2));
        assert_eq!(bad_start.err(), Some(Projection(MalformedDispatch)));
        assert_late_chunk_ignored(&mut intake, 1);
    }
    let mut intake = BaselineIntake::new(GUILD);
    let empty = intake.begin(&guild(0, json!([])), fixture_nonce(1));
    assert!(matches!(empty, Ok(None)));
    let invalid = intake.begin(&guild(1, json!([event(91, "")])), fixture_nonce(2));
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
    fields.remove("entity_metadata");
    fields.remove("scheduled_end_time");
    let update = |row| dispatch("GUILD_SCHEDULED_EVENT_UPDATE", row);
    let data = json!({"guild_id":"10", "nonce":fixture_nonce(1)});
    let bad_chunk = dispatch("GUILD_MEMBERS_CHUNK", data);
    let empty_title = dispatch("GUILD_SCHEDULED_EVENT_CREATE", event(91, ""));
    let incomplete = update(json!({"guild_id":"10", "id":"91"}));
    use MemberBaselineError::{InconsistentChunk, MalformedChunk};
    for (bad, expected) in [
        (bad_chunk, Chunk(MalformedChunk)),
        (chunk(1, 0, 2, "31"), Chunk(InconsistentChunk)),
        (r#"{"op":0,"d":{}}"#.into(), Projection(MalformedDispatch)),
        (empty_title, Projection(UnsupportedEvent)),
        (incomplete, Projection(MalformedDispatch)),
        (update(terminal), Projection(MalformedDispatch)),
        (update(empty_terminal), Projection(UnsupportedEvent)),
        (update(external), Projection(UnsupportedEvent)),
    ] {
        let mut intake = BaselineIntake::new(GUILD);
        start(&mut intake, 1, json!([event(91, "Existing")]), 1);
        assert_eq!(intake.observe(&bad).err(), Some(expected), "{bad}");
        assert_late_chunk_ignored(&mut intake, 1);
    }
}

#[test]
fn event_capacity_allows_replacement_but_not_an_extra_key() {
    let mut intake = BaselineIntake::new(GUILD);
    let events = json!((1..=wire::MAX_SNAPSHOT_EVENTS)
        .map(|id| event(id, "Event"))
        .collect::<Vec<_>>());
    start(&mut intake, 1, events.clone(), 1);
    let update = dispatch("GUILD_SCHEDULED_EVENT_UPDATE", event(1, "Updated"));
    assert!(matches!(intake.observe(&update), Ok(None)));
    let rows = finish(&mut intake, 1, 1);
    assert_eq!(rows.events.len(), wire::MAX_SNAPSHOT_EVENTS);
    assert_eq!(rows.events[0].title, "Updated");
    start(&mut intake, 1, events, 2);
    let extra = event(wire::MAX_SNAPSHOT_EVENTS + 1, "One too many");
    let extra = dispatch("GUILD_SCHEDULED_EVENT_CREATE", extra);
    assert_eq!(intake.observe(&extra).err(), Some(InvalidEvents));
    assert_late_chunk_ignored(&mut intake, 2);
}
