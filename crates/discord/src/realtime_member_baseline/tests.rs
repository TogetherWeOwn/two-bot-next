use super::{
    Id, MemberBaseline,
    MemberBaselineError::{InconsistentChunk, MalformedChunk, RetiredAttempt},
};
use serde_json::{json, to_value, Value};

fn start(expected: u64) -> MemberBaseline {
    MemberBaseline::start(Id::new(10), expected, "fresh".into())
        .unwrap()
        .0
}

fn member(id: &str, pending: bool) -> Value {
    json!({
        "user": {"id": id, "username": "not projected"},
        "nick": "Test", "roles": ["42"], "pending": pending,
        "joined_at": "2026-10-01T00:00:00Z"
    })
}

fn chunk(index: usize, count: usize, members: &[Value]) -> Value {
    json!({ "guild_id": "10", "nonce": "fresh", "chunk_index": index,
        "chunk_count": count, "members": members })
}

#[test]
fn requests_all_members_without_presences_and_bounds_admission() {
    let (_, request) = MemberBaseline::start(Id::new(10), 10_000, "fresh".into()).unwrap();
    assert_eq!(
        to_value(request).unwrap(),
        json!({"op": 8, "d": {"guild_id": "10", "query": "", "limit": 0,
            "presences": false, "nonce": "fresh"}})
    );
    for count in [0, 10_001, u64::MAX] {
        assert!(MemberBaseline::start(Id::new(10), count, "fresh".into()).is_none());
    }
    for nonce in [String::new(), "a".repeat(33)] {
        assert!(MemberBaseline::start(Id::new(10), 1, nonce).is_none());
    }
}

#[test]
fn requires_all_indices_and_exact_unique_member_count() {
    let mut incomplete = start(3);
    let short = chunk(0, 1, &[member("11", true)]).to_string();
    assert_eq!(incomplete.ingest_chunk(&short), Err(InconsistentChunk));
    assert_eq!(incomplete.ingest_chunk(&short), Err(RetiredAttempt));
    assert!(incomplete.finish().is_none());

    let mut missing_index = start(2);
    missing_index
        .ingest_chunk(&chunk(0, 2, &[member("11", true), member("12", false)]).to_string())
        .unwrap();
    assert!(missing_index.finish().is_none());

    let mut baseline = start(2);
    let nick = "😀".repeat(50);
    let mut row = member("12", false);
    row["nick"] = json!(nick);
    row["roles"] = json!(["42", "43"]);
    baseline
        .ingest_chunk(&chunk(1, 2, &[row]).to_string())
        .unwrap();
    assert!(!baseline.is_complete());
    baseline
        .ingest_chunk(&chunk(0, 2, &[member("11", true)]).to_string())
        .unwrap();
    assert_eq!(
        to_value(baseline.finish().unwrap()).unwrap(),
        json!([
            {"userId": "11", "roleIds": ["42"], "nick": "Test", "bot": false, "pending": true},
            {"userId": "12", "roleIds": ["42", "43"], "nick": nick, "bot": false, "pending": false}
        ])
    );
}

#[test]
fn ignores_unrelated_chunks_but_retires_on_intervening_member_changes() {
    let mut baseline = start(1);
    for raw in [
        r#"{"guild_id":"10","nonce":"stale","members":"not parsed"}"#,
        r#"{"guild_id":"10","members":"not parsed"}"#,
        r#"{"guild_id":"20","nonce":"fresh","members":"not parsed"}"#,
    ] {
        baseline.ingest_chunk(raw).unwrap();
        assert!(!baseline.is_complete());
    }
    let raw = chunk(0, 1, &[member("11", false)]).to_string();
    baseline.ingest_chunk(&raw).unwrap();
    baseline.member_changed(Id::new(20));
    assert!(baseline.is_complete());
    baseline.member_changed(Id::new(10));
    assert_eq!(baseline.ingest_chunk(&raw), Err(RetiredAttempt));
    assert!(baseline.finish().is_none());
}

#[test]
fn rejects_ambiguous_screening_even_with_one_complete_chunk() {
    let valid = chunk(0, 1, &[member("11", false)]).to_string();
    let mut absent = member("11", false);
    absent.as_object_mut().unwrap().remove("pending");
    for raw in [
        chunk(0, 1, &[absent]).to_string(),
        valid.replace("\"pending\":false", "\"pending\":null"),
        valid.replace("\"pending\":false", "\"pending\":false,\"pending\":true"),
        chunk(0, 1, &[json!([{"id":"11"}, [], null, false])]).to_string(),
    ] {
        let mut baseline = start(1);
        assert_eq!(baseline.ingest_chunk(&raw), Err(MalformedChunk));
        assert_eq!(baseline.ingest_chunk(&valid), Err(RetiredAttempt));
        assert!(baseline.finish().is_none());
    }
}

#[test]
fn enforces_discord_chunk_size_even_within_the_guild_limit() {
    let mut baseline = start(1001);
    let members: Vec<_> = (1..=1001)
        .map(|id| member(&id.to_string(), false))
        .collect();
    assert_eq!(
        baseline.ingest_chunk(&chunk(0, 1, &members).to_string()),
        Err(InconsistentChunk)
    );
    assert!(baseline.finish().is_none());
}

#[test]
fn retires_on_inconsistent_chunk_metadata_or_rows() {
    let mut not_found = chunk(1, 2, &[member("12", true)]);
    not_found["not_found"] = json!(["99"]);
    let mut oversized_roles = member("12", false);
    oversized_roles["roles"] = json!((1..=251).map(|id| id.to_string()).collect::<Vec<_>>());
    let mut duplicate_roles = member("12", false);
    duplicate_roles["roles"] = json!(["42", "42"]);
    let bad_nick = |nick: &str| {
        let mut row = member("12", false);
        row["nick"] = json!(nick);
        chunk(1, 2, &[row])
    };
    let valid = chunk(1, 2, &[member("12", false)]).to_string();
    for invalid in [
        chunk(1, 0, &[member("12", false)]),
        chunk(2, 2, &[member("12", false)]),
        chunk(0, 1, &[member("12", false)]),
        chunk(0, 2, &[member("12", false)]),
        chunk(1, 2, &[member("11", false)]),
        chunk(1, 2, &[member("12", false), member("13", false)]),
        chunk(1, 2, &[]),
        chunk(1, 2, &[oversized_roles]),
        chunk(1, 2, &[duplicate_roles]),
        bad_nick(""),
        bad_nick(&"a".repeat(101)),
        bad_nick(&"😀".repeat(51)),
        not_found,
    ] {
        let mut baseline = start(2);
        baseline
            .ingest_chunk(&chunk(0, 2, &[member("11", true)]).to_string())
            .unwrap();
        assert_eq!(
            baseline.ingest_chunk(&invalid.to_string()),
            Err(InconsistentChunk)
        );
        assert_eq!(baseline.ingest_chunk(&valid), Err(RetiredAttempt));
        assert!(baseline.finish().is_none());
    }
}
