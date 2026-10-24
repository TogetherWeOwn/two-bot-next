//! Protected numeric/occurrence controls for event-link normalization.
//! Link acceptance is exercised at the guild-aware runtime boundary, without
//! prescribing a new public parser API or discarding the link's guild early.

use two_bot_core::{parse_attendance_occurrence, validate_event_id, validate_occurrence_id};

#[test]
fn existing_representable_numeric_event_ids_remain_unchanged() {
    for id in [
        "12345678901234567",
        "123456789012345678",
        "1546451670500642999",
        "18446744073709551615",
    ] {
        assert_eq!(validate_event_id(id).expect("numeric event id"), id);
        let occurrence = parse_attendance_occurrence(id).expect("bare occurrence");
        assert_eq!(occurrence.anchor_event_id, id);
        assert_eq!(occurrence.canonical_id, id);
    }
}

#[test]
fn numeric_event_ids_must_fit_a_nonzero_discord_snowflake() {
    for id in [
        "18446744073709551616",
        "99999999999999999999",
        "00000000000000000",
    ] {
        assert!(
            validate_event_id(id).is_err(),
            "accepted non-snowflake {id}"
        );
    }
}

#[test]
fn attendance_anchors_cannot_hide_overflow_or_zero_behind_a_label() {
    for occurrence in [
        "18446744073709551616:weekly",
        "99999999999999999999:weekly",
        "00000000000000000:weekly",
    ] {
        assert!(
            parse_attendance_occurrence(occurrence).is_err(),
            "accepted {occurrence}"
        );
    }
}

#[test]
fn labeled_occurrences_keep_the_existing_trim_and_utf16_boundary() {
    let padded = " 1546451670500642999 : weekly ";
    let validated = validate_occurrence_id(padded).expect("trim occurrence");
    let parsed = parse_attendance_occurrence(&validated).expect("trusted numeric anchor");
    assert_eq!(parsed.anchor_event_id, "1546451670500642999");
    assert_eq!(parsed.canonical_id, "1546451670500642999:weekly");
    let exact = format!("1546451670500642999:{}", "é".repeat(108));
    assert_eq!(exact.encode_utf16().count(), 128);
    assert!(validate_occurrence_id(&exact).is_ok());
    assert!(validate_occurrence_id(&format!("{exact}é")).is_err());
    assert!(validate_occurrence_id(&"🦀".repeat(65)).is_err());
}
