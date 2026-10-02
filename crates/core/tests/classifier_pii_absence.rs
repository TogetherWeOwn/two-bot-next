//! Classifier PII-absence acceptance (TOG-12630).
//!
//! Contract (`docs/audit-core.md`): classifier output carries IDs, role lists,
//! counts and flags — never message bodies, nicknames, usernames or free-form
//! moderation reasons. `AuditEvent.metadata_json` carries small classifier
//! context only.
//!
//! These tests pin the existing public classify API only, with hostile inputs
//! held aside: nicknames / bodies / reasons containing snowflake-looking
//! digits, JSON braces, `@everyone`, and URL shapes must never surface
//! verbatim in any row's `metadata_json`, `identity()` or `entry_id`.
//! Pure, no DB, no Discord.

use two_bot_core::audit::AuditEvent;
use two_bot_core::classify::{
    classify_member_update, classify_moderation_audit, classify_raw_message,
    classify_voice_boundary, MemberDelta, RawAuditLogEntry, RawDispatch, VoiceBoundary,
    AUDIT_LOG_MEMBER_BAN_ADD,
};
use two_bot_core::mac::moderation_audit_reason;

const GUILD_NUM: u64 = 111_111_111_111_111_111;
const MEMBER_NUM: u64 = 222_222_222_222_222_222;
const GUILD_STR: &str = "111111111111111111";
const ACTOR_STR: &str = "333333333333333333";
const AT: &str = "2026-03-01T00:00:00.000Z";

/// Hostile display name held aside: the member API only takes a change flag,
/// so this string must never reach a row.
const HOSTILE_NICK: &str = concat!(
    "HostileNick-7f3a9c {\"forged\":\"999888777666555444\"} ",
    "@everyone https://example.invalid/cb?code=not-a-real-code-0000"
);
/// Hostile message body held aside: the raw-message API never takes a body.
const HOSTILE_BODY: &str = concat!(
    "HostileBody-b21e44 {\"message\":\"999888777666555444\"} ",
    "@everyone https://example.invalid/login?password=not-a-real-password-0000"
);
/// Hostile free-form moderation suffix: passed through `reason`, must not leak.
const HOSTILE_REASON: &str = concat!(
    "HostileReason-c84d11 {\"target\":\"999888777666555444\"} ",
    "@everyone https://example.invalid/reset?token=not-a-real-token-0000"
);
/// Hostile username held aside: voice rows only take channel/member IDs.
const HOSTILE_USER: &str = concat!(
    "HostileUser-d52f77 {\"user\":\"999888777666555444\"} ",
    "@everyone https://example.invalid/u?next=https://example.invalid/"
);

/// Test-only marker secret (low entropy, never a real credential).
const TEST_SECRET: &str = "pii-absence-test-secret";

fn surfaces(event: &AuditEvent) -> String {
    format!(
        "{}\n{}\n{}",
        event.entry_id,
        event.identity(),
        event.metadata_json
    )
}

fn assert_absent(event: &AuditEvent, hostile: &str, ctx: &str) {
    let haystack = surfaces(event);
    assert!(
        !haystack.contains(hostile),
        "{ctx}: hostile input surfaces verbatim in row\n hostile={hostile:?}\n row={haystack:?}"
    );
}

fn assert_absent_all(event: &AuditEvent, ctx: &str) {
    for hostile in [HOSTILE_NICK, HOSTILE_BODY, HOSTILE_REASON, HOSTILE_USER] {
        assert_absent(event, hostile, ctx);
    }
}

#[test]
fn member_update_never_carries_nicknames() {
    // Nickname-only change for the hostile nick: only the flag may travel.
    let delta = MemberDelta::diff(true, &["1".to_owned()], &["1".to_owned()]).expect("flag row");
    assert!(delta.nickname_changed);
    let event = classify_member_update(GUILD_NUM, MEMBER_NUM, Some(delta), AT, false).expect("row");
    assert_absent_all(&event, "member nickname-only");
    let meta: serde_json::Value = serde_json::from_str(&event.metadata_json).unwrap();
    assert_eq!(meta["nicknameChanged"], true);

    // Role change plus nickname flag.
    let delta = MemberDelta::diff(true, &["1".to_owned()], &["2".to_owned()]).expect("role row");
    let event = classify_member_update(GUILD_NUM, MEMBER_NUM, Some(delta), AT, false).expect("row");
    assert_absent_all(&event, "member role+nick");
}

#[test]
fn voice_boundaries_never_carry_names() {
    for (ctx, boundary) in [
        ("voice join", VoiceBoundary::Join { channel_id: 10 }),
        ("voice leave", VoiceBoundary::Leave { channel_id: 10 }),
        (
            "voice move",
            VoiceBoundary::Move {
                from_channel_id: 10,
                to_channel_id: 11,
            },
        ),
    ] {
        let event = classify_voice_boundary(GUILD_NUM, MEMBER_NUM, boundary, AT, false);
        assert_absent_all(&event, ctx);
        // Metadata is the `{ isBot }` flag only.
        let meta: serde_json::Value = serde_json::from_str(&event.metadata_json).unwrap();
        assert_eq!(meta.as_object().unwrap().len(), 1, "{ctx}: metadata grows");
        assert!(meta.get("isBot").is_some(), "{ctx}: isBot flag stays");
    }
}

fn delete_packet() -> RawDispatch<'static> {
    RawDispatch {
        op: 0,
        event_type: Some("MESSAGE_DELETE"),
        sequence: Some(7),
        shard_id: 0,
        guild_id: Some(GUILD_STR),
        channel_id: Some("444444444444444444"),
        message_id: Some("555555555555555555"),
        author_id: None,
        edited_timestamp: None,
    }
}

#[test]
fn raw_message_never_carries_bodies() {
    let packet = delete_packet();
    let event = classify_raw_message(&packet, AT).expect("delete row");
    assert_absent_all(&event, "message delete");
    assert_absent(&event, HOSTILE_BODY, "message delete body");

    let edit = RawDispatch {
        event_type: Some("MESSAGE_UPDATE"),
        edited_timestamp: Some("2026-02-01T00:00:00.000Z"),
        author_id: Some(ACTOR_STR),
        ..delete_packet()
    };
    let event = classify_raw_message(&edit, AT).expect("edit row");
    assert_absent(&event, HOSTILE_BODY, "message edit body");

    // A hostile non-date edited timestamp takes the shard/sequence fallback —
    // the hostile text itself must not land in the entry id.
    let hostile_ts = RawDispatch {
        edited_timestamp: Some(HOSTILE_BODY),
        ..delete_packet()
    };
    let hostile_ts = RawDispatch {
        event_type: Some("MESSAGE_UPDATE"),
        author_id: Some(ACTOR_STR),
        ..hostile_ts
    };
    let event = classify_raw_message(&hostile_ts, AT).expect("fallback row");
    assert!(
        !event.entry_id.contains(HOSTILE_BODY),
        "hostile timestamp leaks into entry id: {}",
        event.entry_id
    );
    assert_absent_all(&event, "message fallback");
}

fn uncorrelated_entry(reason: Option<String>) -> RawAuditLogEntry {
    RawAuditLogEntry {
        action_id: 20,
        log_entry_id: "222".to_owned(),
        executor_id: Some("666666666666666666".to_owned()),
        target_id: Some("777777777777777777".to_owned()),
        reason,
        created_timestamp_ms: 0,
        extra_channel_id: None,
        extra_count: Some(serde_json::json!(5)),
        extra_removed: None,
    }
}

#[test]
fn moderation_audit_never_carries_freeform_reasons() {
    // Correlated row: the marker verifies, but the human suffix must not leak.
    let reason = moderation_audit_reason(
        Some(TEST_SECRET),
        GUILD_STR,
        "idem-pii-1",
        "moderation.ban",
        ACTOR_STR,
        HOSTILE_REASON,
    );
    let entry = RawAuditLogEntry {
        action_id: AUDIT_LOG_MEMBER_BAN_ADD,
        log_entry_id: "111".to_owned(),
        executor_id: Some(ACTOR_STR.to_owned()),
        target_id: Some("777777777777777777".to_owned()),
        reason: Some(reason),
        created_timestamp_ms: 1_767_225_600_000,
        extra_channel_id: None,
        extra_count: None,
        extra_removed: None,
    };
    let event = classify_moderation_audit(&entry, GUILD_STR, Some(ACTOR_STR), Some(TEST_SECRET))
        .expect("correlated row");
    assert_absent(&event, HOSTILE_REASON, "correlated reason suffix");
    assert_absent_all(&event, "correlated row");
    assert_eq!(event.action.as_deref(), Some("moderation.ban"));

    // Uncorrelated row: plain hostile reason with no verifiable marker.
    let entry = uncorrelated_entry(Some(HOSTILE_REASON.to_owned()));
    let event = classify_moderation_audit(&entry, GUILD_STR, Some(ACTOR_STR), None)
        .expect("uncorrelated row");
    assert_absent(&event, HOSTILE_REASON, "uncorrelated reason");
    assert_absent_all(&event, "uncorrelated row");
}

#[test]
fn role_ids_sort_lexicographically_with_stable_digest() {
    // Legacy `[...set].sort()` on string snowflakes: "10" before "9".
    let a = MemberDelta::diff(true, &[], &["9".to_owned(), "10".to_owned()]).expect("changed");
    let b = MemberDelta::diff(true, &[], &["10".to_owned(), "9".to_owned()]).expect("changed");
    assert_eq!(a.added_role_ids, ["10", "9"]);
    assert_eq!(b.added_role_ids, ["10", "9"]);
    assert_eq!(
        a.change_digest(),
        b.change_digest(),
        "digest is order-stable"
    );
    // Golden digest pins the exact JSON body + hash the entry id embeds.
    assert_eq!(a.change_digest(), "eG0n08KYgOucLcMM");

    let removed =
        MemberDelta::diff(false, &["9".to_owned(), "10".to_owned()], &[]).expect("changed");
    assert_eq!(removed.removed_role_ids, ["10", "9"]);

    // The digest survives into the member-update entry id deterministically.
    let left = classify_member_update(
        GUILD_NUM,
        MEMBER_NUM,
        MemberDelta::diff(true, &[], &["9".to_owned(), "10".to_owned()]),
        AT,
        false,
    )
    .expect("row");
    let right = classify_member_update(
        GUILD_NUM,
        MEMBER_NUM,
        MemberDelta::diff(true, &[], &["10".to_owned(), "9".to_owned()]),
        AT,
        false,
    )
    .expect("row");
    assert_eq!(left.entry_id, right.entry_id);
}

#[test]
fn empty_inputs_yield_no_rows() {
    // Member: no change, empty sets, missing delta, or partial baseline.
    assert_eq!(
        MemberDelta::diff(false, &["1".to_owned()], &["1".to_owned()]),
        None
    );
    assert_eq!(MemberDelta::diff(false, &[], &[]), None);
    assert_eq!(
        classify_member_update(GUILD_NUM, MEMBER_NUM, None, AT, false),
        None
    );
    assert_eq!(
        classify_member_update(
            GUILD_NUM,
            MEMBER_NUM,
            MemberDelta::diff(true, &["1".to_owned()], &["2".to_owned()]),
            AT,
            true,
        ),
        None,
        "partial old member must not report current roles as granted"
    );

    // Voice: no boundary, or same channel on both sides.
    assert_eq!(VoiceBoundary::classify(None, None), None);
    assert_eq!(VoiceBoundary::classify(Some(10), Some(10)), None);

    // Raw message: non-dispatch op, unknown type, DM, empty guild.
    // `RawDispatch` is not `Copy`, so each case spreads a fresh packet.
    assert_eq!(
        classify_raw_message(
            &RawDispatch {
                op: 10,
                ..delete_packet()
            },
            AT
        ),
        None
    );
    assert_eq!(
        classify_raw_message(
            &RawDispatch {
                event_type: Some("MESSAGE_CREATE"),
                ..delete_packet()
            },
            AT
        ),
        None
    );
    assert_eq!(
        classify_raw_message(
            &RawDispatch {
                guild_id: None,
                ..delete_packet()
            },
            AT
        ),
        None
    );
    assert_eq!(
        classify_raw_message(
            &RawDispatch {
                guild_id: Some(""),
                ..delete_packet()
            },
            AT
        ),
        None
    );

    // Moderation: unknown audit-log action records nothing.
    let unknown = RawAuditLogEntry {
        action_id: 1,
        ..uncorrelated_entry(None)
    };
    assert_eq!(
        classify_moderation_audit(&unknown, GUILD_STR, Some(ACTOR_STR), Some(TEST_SECRET)),
        None
    );
}
