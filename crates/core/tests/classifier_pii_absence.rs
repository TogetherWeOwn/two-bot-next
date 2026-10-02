//! Classifier PII-absence acceptance (TOG-12630).
//!
//! Contract (`docs/audit-core.md`): classifier output carries IDs, role lists,
//! counts and flags — never message bodies, nicknames, usernames or free-form
//! moderation reasons. `AuditEvent.metadata_json` carries small classifier
//! context only.
//!
//! Two layers, existing public classify API only:
//! - Exercised: the string inputs that can carry free text (a raw message's
//!   `edited_timestamp`, a moderation `reason`) take hostile values, which
//!   must not surface in any row field, the decoded metadata, the serialized
//!   row or the Discord mirror text.
//! - API-unreachable: nicknames, usernames and message bodies have no
//!   parameter at all (`MemberDelta::diff` takes a change flag, voice takes
//!   channel IDs, `RawDispatch` has no body). Each row kind pins its exact
//!   metadata key set, so a future name/body/reason field fails whatever the
//!   input.
//!
//! Pure, no DB, no Discord.

use two_bot_core::audit::{format_audit_event, AuditEvent};
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

/// Snowflake-looking id forged inside each hostile string's JSON braces.
const FORGED_ID: &str = "999888777666555444";
/// Hostile message-body shape, fed through `edited_timestamp`.
const HOSTILE_BODY: &str = concat!(
    "HostileBody-b21e44 {\"message\":\"999888777666555444\"} ",
    "@everyone https://example.invalid/login?password=not-a-real-password-0000"
);
/// Hostile free-form moderation suffix: passed through `reason`, must not leak.
const HOSTILE_REASON: &str = concat!(
    "HostileReason-c84d11 {\"target\":\"999888777666555444\"} ",
    "@everyone https://example.invalid/reset?token=not-a-real-token-0000"
);

const MEMBER_KEYS: &[&str] = &["nicknameChanged", "addedRoleIds", "removedRoleIds"];
const VOICE_KEYS: &[&str] = &["isBot"];
const RAW_MESSAGE_KEYS: &[&str] = &[];
const UNCORRELATED_KEYS: &[&str] = &["auditLogEntryId", "count"];
const CORRELATED_KEYS: &[&str] = &["auditLogEntryId", "count", "origin", "outcome"];
const CORRELATED_COUNTED_KEYS: &[&str] =
    &["auditLogEntryId", "count", "origin", "outcome", "affected"];

/// Marker MAC key from the public, non-production vector fixture shared with
/// the in-crate MAC/classifier tests (`mac::moderation_test_vectors`).
fn fixture_mac_key() -> String {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/moderation-mac.json");
    let json = std::fs::read_to_string(path).expect("public MAC vector fixture");
    let vectors: serde_json::Value = serde_json::from_str(&json).expect("valid MAC vectors");
    vectors[0]["secret"]
        .as_str()
        .expect("fixture vector key")
        .to_owned()
}

fn collect_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        serde_json::Value::Object(map) => {
            for (key, v) in map {
                out.push(key.clone());
                collect_strings(v, out);
            }
        }
        _ => {}
    }
}

/// Every string a row stores or mirrors: the serialized row, the Discord
/// mirror text, each field value unescaped (a field added later is covered
/// without editing this list) and each decoded metadata key/value.
fn surfaces(event: &AuditEvent) -> Vec<String> {
    let row = serde_json::to_value(event).expect("row serializes");
    let mut out = vec![
        serde_json::to_string(event).expect("row serializes"),
        format_audit_event(event),
    ];
    collect_strings(&row, &mut out);
    let meta: serde_json::Value =
        serde_json::from_str(&event.metadata_json).expect("metadata is JSON");
    collect_strings(&meta, &mut out);
    out
}

fn assert_absent(event: &AuditEvent, hostile: &str, ctx: &str) {
    let haystack = surfaces(event);
    // Whole string plus each fragment: JSON escaping, mention neutralization
    // or truncation in the mirror can split a verbatim match.
    let needles = std::iter::once(hostile)
        .chain(hostile.split_whitespace())
        .chain([FORGED_ID]);
    for needle in needles {
        // The failure message names the case only; it never echoes row contents.
        assert!(
            !haystack.iter().any(|s| s.contains(needle)),
            "{ctx}: hostile input surfaces in row"
        );
    }
}

/// Pin the exact metadata key set: a future name/body/reason key fails here
/// whatever the input.
fn assert_metadata_keys(event: &AuditEvent, expected: &[&str], ctx: &str) {
    let meta: serde_json::Value =
        serde_json::from_str(&event.metadata_json).expect("metadata is JSON");
    let mut keys: Vec<&str> = meta
        .as_object()
        .expect("metadata is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    let mut want = expected.to_vec();
    want.sort_unstable();
    assert!(
        keys == want,
        "{ctx}: metadata key set drifts from {expected:?}"
    );
}

#[test]
fn member_update_never_carries_nicknames() {
    // Nickname-only change: the API takes a flag, so only the flag travels.
    let delta = MemberDelta::diff(true, &["1".to_owned()], &["1".to_owned()]).expect("flag row");
    assert!(delta.nickname_changed);
    let event = classify_member_update(GUILD_NUM, MEMBER_NUM, Some(delta), AT, false).expect("row");
    assert_metadata_keys(&event, MEMBER_KEYS, "member nickname-only");
    let meta: serde_json::Value = serde_json::from_str(&event.metadata_json).unwrap();
    assert!(
        meta["nicknameChanged"] == true,
        "member nickname-only: flag"
    );

    // Role change plus nickname flag.
    let delta = MemberDelta::diff(true, &["1".to_owned()], &["2".to_owned()]).expect("role row");
    let event = classify_member_update(GUILD_NUM, MEMBER_NUM, Some(delta), AT, false).expect("row");
    assert_metadata_keys(&event, MEMBER_KEYS, "member role+nick");
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
        // Metadata is the `{ isBot }` flag only.
        assert_metadata_keys(&event, VOICE_KEYS, ctx);
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
    // `RawDispatch` has no body field; both row kinds carry no metadata.
    let event = classify_raw_message(&delete_packet(), AT).expect("delete row");
    assert_metadata_keys(&event, RAW_MESSAGE_KEYS, "message delete");

    let edit = RawDispatch {
        event_type: Some("MESSAGE_UPDATE"),
        edited_timestamp: Some("2026-02-01T00:00:00.000Z"),
        author_id: Some(ACTOR_STR),
        ..delete_packet()
    };
    let event = classify_raw_message(&edit, AT).expect("edit row");
    assert_metadata_keys(&event, RAW_MESSAGE_KEYS, "message edit");

    // A hostile non-date edited timestamp takes the shard/sequence fallback
    // identity and the observed instant — the hostile text itself must not
    // land in the entry id, `occurred_at` or any other surface.
    let hostile_ts = RawDispatch {
        event_type: Some("MESSAGE_UPDATE"),
        edited_timestamp: Some(HOSTILE_BODY),
        author_id: Some(ACTOR_STR),
        ..delete_packet()
    };
    let event = classify_raw_message(&hostile_ts, AT).expect("fallback row");
    assert_absent(&event, HOSTILE_BODY, "message edit fallback");
    assert_metadata_keys(&event, RAW_MESSAGE_KEYS, "message edit fallback");
    assert!(
        event.entry_id.ends_with(":shard-0:sequence-7"),
        "message edit fallback: identity is not shard/sequence"
    );
    assert!(
        event.occurred_at == AT,
        "message edit fallback: occurred_at is not the observed instant"
    );
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

fn correlated_entry(reason: String, extra_count: Option<serde_json::Value>) -> RawAuditLogEntry {
    RawAuditLogEntry {
        action_id: AUDIT_LOG_MEMBER_BAN_ADD,
        log_entry_id: "111".to_owned(),
        executor_id: Some(ACTOR_STR.to_owned()),
        target_id: Some("777777777777777777".to_owned()),
        reason: Some(reason),
        created_timestamp_ms: 1_767_225_600_000,
        extra_channel_id: None,
        extra_count,
        extra_removed: None,
    }
}

#[test]
fn moderation_audit_never_carries_freeform_reasons() {
    // Correlated rows: the marker verifies, but the human suffix must not leak
    // into `action`/`actor_id` (marker-derived) or anywhere else.
    let mac_key = fixture_mac_key();
    let reason = moderation_audit_reason(
        Some(&mac_key),
        GUILD_STR,
        "idem-pii-1",
        "moderation.ban",
        ACTOR_STR,
        HOSTILE_REASON,
    );
    for (ctx, extra_count, keys) in [
        ("correlated row", None, CORRELATED_KEYS),
        (
            "correlated counted row",
            Some(serde_json::json!(3)),
            CORRELATED_COUNTED_KEYS,
        ),
    ] {
        let entry = correlated_entry(reason.clone(), extra_count);
        let event = classify_moderation_audit(&entry, GUILD_STR, Some(ACTOR_STR), Some(&mac_key))
            .expect("correlated row");
        assert_absent(&event, HOSTILE_REASON, ctx);
        assert_metadata_keys(&event, keys, ctx);
        assert!(
            event.action.as_deref() == Some("moderation.ban"),
            "{ctx}: action is not the marker action"
        );
    }

    // Uncorrelated row: plain hostile reason with no verifiable marker keeps
    // only the action table string.
    let entry = uncorrelated_entry(Some(HOSTILE_REASON.to_owned()));
    let event = classify_moderation_audit(&entry, GUILD_STR, Some(ACTOR_STR), None)
        .expect("uncorrelated row");
    assert_absent(&event, HOSTILE_REASON, "uncorrelated row");
    assert_metadata_keys(&event, UNCORRELATED_KEYS, "uncorrelated row");
    assert!(
        event.action.as_deref() == Some("member_kick"),
        "uncorrelated row: action is not the table string"
    );
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
    let mac_key = fixture_mac_key();
    assert_eq!(
        classify_moderation_audit(&unknown, GUILD_STR, Some(ACTOR_STR), Some(&mac_key)),
        None
    );
}
