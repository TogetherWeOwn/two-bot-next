//! Pending-evidence refusals render the relay id inert inside the trusted
//! code span; ordinary ids and the count/refusal semantics stay unchanged.

use two_bot_core::feeds::{feed_pending_evidence_refusal_text, FeedPendingEvidenceOperation};

const HOSTILE_IDS: [(&str, &str); 2] = [
    ("relay` **forged**", "relay forged"),
    ("relay```\n**forged**", "relay forged"),
];

#[test]
fn hostile_ids_cannot_reopen_the_code_span() {
    for (operation, unchanged) in [
        (
            FeedPendingEvidenceOperation::Pause,
            "was not paused and remains enabled",
        ),
        (
            FeedPendingEvidenceOperation::Remove,
            "was not removed and remains saved",
        ),
    ] {
        for (id, shown) in HOSTILE_IDS {
            let text = feed_pending_evidence_refusal_text(operation, id, 3);
            assert_eq!(
                text,
                format!(
                    "Feed relay `{shown}` {unchanged}: snapshot of 3 unresolved delivery evidence. \
                     Wait for reconciliation and recovery to settle, then inspect via /feed-inspect. \
                     Retry after recovery settles."
                )
            );
            assert_eq!(text.matches('`').count(), 2, "{text}");
            assert!(!text.contains("**"), "{text}");
            assert!(text.encode_utf16().count() <= 2000);
        }
        let plain = feed_pending_evidence_refusal_text(operation, "selected-relay", 1);
        assert!(plain.starts_with("Feed relay `selected-relay` "), "{plain}");
        assert!(plain.contains("snapshot of 1 unresolved"), "{plain}");
    }
}

#[test]
fn maximum_length_hostile_id_stays_bounded() {
    let id = "`*".repeat(64);
    let text = feed_pending_evidence_refusal_text(FeedPendingEvidenceOperation::Remove, &id, 99);
    assert!(text.encode_utf16().count() <= 2000, "{}", text.len());
    assert_eq!(text.matches('`').count(), 2);
}
