use serde_json::{json, Value};

use super::*;

const GUILD: Id<GuildMarker> = Id::new(10);

fn dispatch(kind: &str, data: Value) -> String {
    json!({"op": 0, "t": kind, "d": data}).to_string()
}

fn guild(count: u64, events: Value) -> String {
    dispatch(
        "GUILD_CREATE",
        json!({"id": "10", "member_count": count, "guild_scheduled_events": events}),
    )
}

fn event(id: usize, title: &str) -> Value {
    json!({
        "id": id.to_string(), "guild_id": "10", "entity_type": 3, "privacy_level": 2,
        "status": 1, "name": title, "entity_metadata": {"location": "Community hall"},
        "scheduled_start_time": "2026-11-21T13:00:00Z", "scheduled_end_time": "2026-11-21T14:00:00Z"
    })
}

fn chunk(nonce: &str, index: u8, count: u8, user_id: &str) -> String {
    dispatch(
        "GUILD_MEMBERS_CHUNK",
        json!({
            "guild_id": "10", "nonce": nonce, "chunk_index": index, "chunk_count": count,
            "members": [{"user": {"id": user_id}, "roles": [], "pending": false}]
        }),
    )
}

fn assert_late_chunk_ignored(intake: &mut BaselineIntake, nonce: &str) {
    assert!(matches!(
        intake.observe(&chunk(nonce, 0, 1, "31")),
        Ok(None)
    ));
}

#[test]
fn joins_the_roster_with_events_at_the_final_chunks_receive_position() {
    let mut intake = BaselineIntake::new(GUILD);
    assert!(matches!(
        intake.begin(
            &guild(2, json!([event(91, "Old"), event(92, "Gone")])),
            "first".into()
        ),
        Ok(Some(_))
    ));
    for frame in [
        chunk("first", 1, 2, "32"),
        dispatch("GUILD_SCHEDULED_EVENT_UPDATE", event(91, "New")),
        dispatch(
            "GUILD_SCHEDULED_EVENT_DELETE",
            json!({"guild_id":"10", "id":"92"}),
        ),
        dispatch("GUILD_SCHEDULED_EVENT_CREATE", event(93, "Added")),
        dispatch(
            "GUILD_MEMBER_REMOVE",
            json!({"guild_id":"11", "user":{"id":"32"}}),
        ),
    ] {
        assert!(matches!(intake.observe(&frame), Ok(None)));
    }
    let rows = intake
        .observe(&chunk("first", 0, 2, "31"))
        .expect("valid final chunk")
        .expect("complete baseline rows");
    let members = serde_json::to_value(rows.members).expect("member rows");
    assert_eq!(members[0]["userId"], "31");
    assert_eq!(members[1]["userId"], "32");
    assert_eq!(
        rows.events
            .iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
        ["New", "Added"]
    );
    assert_late_chunk_ignored(&mut intake, "first");
}

#[test]
fn member_changes_and_retirement_prevent_late_chunks_from_finishing() {
    let mut intake = BaselineIntake::new(GUILD);
    let frame = guild(1, json!([]));
    assert!(matches!(intake.begin(&frame, "one".into()), Ok(Some(_))));
    let remove = dispatch(
        "GUILD_MEMBER_REMOVE",
        json!({"guild_id":"10", "user":{"id":"31"}}),
    );
    assert!(matches!(
        intake.observe(&remove),
        Err(IntakeError::MemberChanged)
    ));
    assert_late_chunk_ignored(&mut intake, "one");

    assert!(matches!(intake.begin(&frame, "two".into()), Ok(Some(_))));
    intake.retire();
    assert!(matches!(intake.begin(&frame, "three".into()), Ok(Some(_))));
    assert_late_chunk_ignored(&mut intake, "two");
    assert!(matches!(
        intake.observe(&chunk("three", 0, 1, "32")),
        Ok(Some(_))
    ));
}

#[test]
fn replacement_requires_explicit_count_and_events_from_the_same_dispatch() {
    let mut intake = BaselineIntake::new(GUILD);
    for (attempt, data) in [
        json!({"id":"10", "guild_scheduled_events": []}),
        json!({"id":"10", "member_count": "1", "guild_scheduled_events": []}),
        json!({"id":"10", "member_count": 1}),
    ]
    .into_iter()
    .enumerate()
    {
        let nonce = format!("old-{attempt}");
        assert!(matches!(
            intake.begin(&guild(1, json!([])), nonce.clone()),
            Ok(Some(_))
        ));
        assert!(matches!(
            intake.begin(&dispatch("GUILD_CREATE", data), format!("new-{attempt}")),
            Err(IntakeError::Projection(ProjectionError::MalformedDispatch))
        ));
        assert_late_chunk_ignored(&mut intake, &nonce);
    }
    assert!(matches!(
        intake.begin(&guild(0, json!([])), "empty".into()),
        Ok(None)
    ));
}

#[test]
fn invalid_input_cannot_be_repaired_by_a_later_chunk() {
    let mut intake = BaselineIntake::new(GUILD);
    for (nonce, bad, expected) in [
        (
            "one",
            dispatch(
                "GUILD_MEMBERS_CHUNK",
                json!({"guild_id":"10", "nonce":"one"}),
            ),
            IntakeError::Chunk(MemberBaselineError::MalformedChunk),
        ),
        (
            "two",
            dispatch(
                "GUILD_SCHEDULED_EVENT_UPDATE",
                json!({"guild_id":"10", "id":"91"}),
            ),
            IntakeError::Projection(ProjectionError::MalformedDispatch),
        ),
        (
            "three",
            dispatch("GUILD_SCHEDULED_EVENT_CREATE", event(91, "")),
            IntakeError::InvalidEvents,
        ),
        (
            "four",
            r#"{"op":0,"d":{}}"#.into(),
            IntakeError::Projection(ProjectionError::MalformedDispatch),
        ),
        (
            "five",
            chunk("five", 0, 1, "31"),
            IntakeError::Chunk(MemberBaselineError::InconsistentChunk),
        ),
    ] {
        assert!(matches!(
            intake.begin(&guild(2, json!([])), nonce.into()),
            Ok(Some(_))
        ));
        assert!(matches!(intake.observe(&bad), Err(error) if error == expected));
        assert_late_chunk_ignored(&mut intake, nonce);
    }
}

#[test]
fn event_capacity_allows_replacement_but_not_an_extra_key() {
    let mut intake = BaselineIntake::new(GUILD);
    let events: Vec<_> = (1..=wire::MAX_SNAPSHOT_EVENTS)
        .map(|id| event(id, "Event"))
        .collect();
    assert!(matches!(
        intake.begin(&guild(1, json!(events)), "one".into()),
        Ok(Some(_))
    ));
    assert!(matches!(
        intake.observe(&dispatch(
            "GUILD_SCHEDULED_EVENT_UPDATE",
            event(1, "Updated")
        )),
        Ok(None)
    ));
    let extra = event(wire::MAX_SNAPSHOT_EVENTS + 1, "One too many");
    assert!(matches!(
        intake.observe(&dispatch("GUILD_SCHEDULED_EVENT_CREATE", extra)),
        Err(IntakeError::InvalidEvents)
    ));
    assert_late_chunk_ignored(&mut intake, "one");
    assert!(matches!(
        intake.begin(&guild(1, json!([event(91, "")])), "two".into()),
        Err(IntakeError::InvalidEvents)
    ));
}
