#![no_main]

use libfuzzer_sys::fuzz_target;
use two_bot_core::rsvp::{
    checkin_classification, checkin_idempotency_key, checkin_metadata_json, checkin_source,
    checkin_source_event_id, partition_rsvps, AttendanceProof, RsvpRecord, RsvpStatus,
};

/// Bounds keep every allocation a function of the (libFuzzer-capped) input:
/// at most this many NUL/newline-separated fields are read, each truncated to
/// this many chars before reaching a constructor.
const MAX_FIELDS: usize = 64;
const MAX_RECORDS: usize = 16;
const MAX_FIELD_CHARS: usize = 256;
const MAX_IDEMPOTENCY_PAIRS: usize = 16;
const MAX_CLASSIFICATION_BYTES: usize = 32;

/// Synthetic identities only; the store never sees them and no validator runs
/// here, so any fixed string would do. Kept snowflake-shaped for readability.
const GUILD_ID: &str = "11111111111111111";
const EVENT_ID: &str = "22222222222222222";
const RESPONDED_AT: &str = "2026-09-10T10:00:00.000Z";

const PROOFS: [AttendanceProof; 4] = [
    AttendanceProof::HostCheckin,
    AttendanceProof::DurableCheckin,
    AttendanceProof::Voice600s,
    AttendanceProof::Rsvp,
];

fn truncate_chars(s: &str) -> String {
    s.chars().take(MAX_FIELD_CHARS).collect()
}

fuzz_target!(|data: &[u8]| {
    // Empty-partition base case: independent of the fuzzer input.
    let empty = partition_rsvps(&[]);
    assert_eq!(empty.counts(), (0, 0, 0));
    assert_eq!(empty.total(), 0);

    // Empty-string base case for the idempotency constructors.
    assert_eq!(checkin_source_event_id("", ""), ":");
    assert_eq!(checkin_source(""), "event:");
    assert_eq!(checkin_idempotency_key("", ""), "event-attended::");

    // Arbitrary bytes stay useful with invalid UTF-8: lossy conversion feeds
    // every string-taking constructor below.
    let text = String::from_utf8_lossy(data);
    let mut fields: Vec<String> = text.split(['\0', '\n']).map(truncate_chars).collect();
    if fields.len() > MAX_FIELDS {
        fields.truncate(MAX_FIELDS);
    }

    // --- Classification: pin the documented bot/human contract exactly.
    // Any S5 classifier change must update this target on its own card; a
    // silent semantic drift fails the fuzzer instead.
    for i in 0..fields.len() {
        let flag = i % 2 == 0;
        let first = checkin_classification(flag);
        let second = checkin_classification(flag);
        assert_eq!(first, second);
        if flag {
            assert_eq!(first.classification, "bot");
            assert_eq!(first.matched_rule, "discord_bot");
        } else {
            assert_eq!(first.classification, "eligible_human");
            assert_eq!(first.matched_rule, "no_exclusion_matched");
        }
    }
    for byte in data.iter().take(MAX_CLASSIFICATION_BYTES) {
        let flag = byte % 2 == 1;
        let classification = checkin_classification(flag);
        assert_eq!(classification, checkin_classification(flag));
        assert!(matches!(
            (classification.classification, classification.matched_rule),
            ("bot", "discord_bot") | ("eligible_human", "no_exclusion_matched")
        ));
    }

    // --- Partition: bounded record sequence with forced user-id reuse over a
    // 4-id synthetic pool, statuses cycling legacy order, then the partition
    // invariants (total, counts, per-bucket order).
    let mut records = Vec::new();
    for (i, field) in fields.iter().take(MAX_RECORDS).enumerate() {
        let pool = (field.len() + i) % 4;
        records.push(RsvpRecord {
            guild_id: GUILD_ID.to_owned(),
            event_id: EVENT_ID.to_owned(),
            user_id: format!("rsvp-fuzz-user-{pool}"),
            status: RsvpStatus::ALL[i % RsvpStatus::ALL.len()],
            responded_at: RESPONDED_AT.to_owned(),
        });
    }
    assert!(records.len() <= MAX_RECORDS);
    let totals = partition_rsvps(&records);
    assert_eq!(totals.total(), records.len());
    assert_eq!(
        totals.counts(),
        (
            totals.going.len(),
            totals.interested.len(),
            totals.declined.len()
        )
    );
    assert_eq!(
        totals.total(),
        totals.going.len() + totals.interested.len() + totals.declined.len()
    );
    let expected_going: Vec<String> = records
        .iter()
        .filter(|r| r.status == RsvpStatus::Going)
        .map(|r| r.user_id.clone())
        .collect();
    let expected_interested: Vec<String> = records
        .iter()
        .filter(|r| r.status == RsvpStatus::Interested)
        .map(|r| r.user_id.clone())
        .collect();
    let expected_declined: Vec<String> = records
        .iter()
        .filter(|r| r.status == RsvpStatus::Declined)
        .map(|r| r.user_id.clone())
        .collect();
    assert_eq!(totals.going, expected_going);
    assert_eq!(totals.interested, expected_interested);
    assert_eq!(totals.declined, expected_declined);

    // --- Idempotency-key constructors: consecutive field pairs as
    // (occurrence, actor), exact-format plus determinism plus byte-bounded
    // allocation. A trailing odd field pairs with the empty string so no
    // input is silently dropped.
    let mut pairs = 0;
    let mut pair_iter = fields.iter().peekable();
    while pair_iter.peek().is_some() && pairs < MAX_IDEMPOTENCY_PAIRS {
        let occurrence = pair_iter.next().map(String::as_str).unwrap_or("");
        let actor = pair_iter.next().map(String::as_str).unwrap_or("");
        assert!(occurrence.chars().count() <= MAX_FIELD_CHARS);
        assert!(actor.chars().count() <= MAX_FIELD_CHARS);

        let source_event_id = checkin_source_event_id(occurrence, actor);
        assert_eq!(source_event_id, format!("{occurrence}:{actor}"));
        assert_eq!(source_event_id, checkin_source_event_id(occurrence, actor));

        let source = checkin_source(occurrence);
        assert_eq!(source, format!("event:{occurrence}"));
        assert_eq!(source, checkin_source(occurrence));

        let key = checkin_idempotency_key(occurrence, actor);
        assert_eq!(key, format!("event-attended:{occurrence}:{actor}"));
        assert_eq!(key, checkin_idempotency_key(occurrence, actor));
        assert!(key.starts_with("event-attended:"));
        // Byte-bounded: truncated chars keep multibyte input within 4x.
        assert!(occurrence.len() <= MAX_FIELD_CHARS * 4);
        assert!(actor.len() <= MAX_FIELD_CHARS * 4);
        assert_eq!(
            key.len(),
            "event-attended:".len() + occurrence.len() + 1 + actor.len()
        );

        // Metadata JSON round-trips the same arbitrary occurrence for every
        // proof variant; the corpus deliberately includes quotes, backslashes
        // and astral text to stress the escaping.
        let proof = PROOFS[pairs % PROOFS.len()];
        let metadata = checkin_metadata_json(occurrence, proof);
        let parsed: serde_json::Value =
            serde_json::from_str(&metadata).expect("metadata is valid JSON");
        assert_eq!(
            parsed["eventOccurrenceId"],
            serde_json::Value::from(occurrence)
        );
        assert_eq!(parsed["proof"], serde_json::Value::from(proof.as_str()));
        assert_eq!(
            metadata,
            checkin_metadata_json(occurrence, proof),
            "metadata constructor is deterministic"
        );

        pairs += 1;
    }
});
