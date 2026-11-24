use serde_json::json;
use two_bot_core::member_moderation_store::WarningHistoryEntry;

use super::*;

const TARGET: &str = "18446744073709551614";
const EARLIEST: &str = "0001-01-01T00:00:00Z";
const LATEST: &str = "9999-12-31T23:59:59.999999999Z";

fn entry(index: u8, time: &str, recorder_id: Option<u64>) -> WarningHistoryEntry {
    WarningHistoryEntry {
        reference: format!("W-{index:064x}"),
        recorded_at_utc: time.to_owned(),
        recorder_id,
    }
}

fn page(entries: Vec<WarningHistoryEntry>) -> WarningHistoryPage {
    WarningHistoryPage {
        entries,
        next: None,
    }
}

#[test]
fn retains_projected_rows_and_disables_mentions() {
    let page = page(vec![
        entry(1, LATEST, Some(u64::MAX)),
        entry(2, "2026-11-24T00:00:00.000001Z", None),
        entry(3, EARLIEST, Some(0)),
    ]);
    let data = render_warning_history(TARGET, &page).expect("valid page");
    let body = format!(
        "Recorded user ID {TARGET}\n{} | {LATEST} | Actor ID 18446744073709551615\n\
         {} | 2026-11-24T00:00:00.000001Z | Actor unavailable\n\
         {} | {EARLIEST} | Actor unavailable\nReasons are not shown in this view.",
        page.entries[0].reference, page.entries[1].reference, page.entries[2].reference
    );
    assert_eq!(data.content.as_deref(), Some(body.as_str()));
    assert_eq!(body.matches(TARGET).count(), 1);
    let mentions = data
        .allowed_mentions
        .as_ref()
        .expect("explicit mention policy");
    assert!(mentions.parse.is_empty());
    assert!(mentions.users.is_empty());
    assert!(mentions.roles.is_empty());
    assert!(!mentions.replied_user);
    assert_eq!(
        serde_json::to_value(data).expect("serializable reply data"),
        json!({"content": body, "allowed_mentions": {"parse": []}})
    );
}

#[test]
fn zero_rows_render_only_the_heading_and_notice() {
    let data = render_warning_history("1", &page(vec![])).expect("valid empty projection");
    assert_eq!(
        data.content.as_deref(),
        Some("Recorded user ID 1\nReasons are not shown in this view.")
    );
}

#[test]
fn keeps_five_complete_rows_within_both_budgets() {
    let page = page((1..=5).map(|i| entry(i, LATEST, Some(u64::MAX))).collect());
    let data = render_warning_history(TARGET, &page).expect("valid five-row page");
    let body = data.content.expect("page body");
    assert!(body.is_ascii());
    assert!(body.encode_utf16().count() <= 1_900);
    let lines: Vec<_> = body.split_inclusive('\n').collect();
    assert_eq!(lines.len(), 7);
    for (row, entry) in lines[1..=5].iter().zip(&page.entries) {
        assert_eq!(
            *row,
            format!(
                "{} | {LATEST} | Actor ID 18446744073709551615\n",
                entry.reference
            )
        );
        assert!(row.encode_utf16().count() <= 140);
    }
}

#[test]
fn refuses_six_rows_instead_of_returning_a_partial_page() {
    let page = page((1..=6).map(|i| entry(i, LATEST, None)).collect());
    assert_eq!(
        render_warning_history(TARGET, &page),
        Err(WarningHistoryUnavailable)
    );
}

#[test]
fn refuses_noncanonical_targets() {
    let page = page(vec![entry(1, EARLIEST, None)]);
    for target in [
        "",
        "0",
        "01",
        "+1",
        "1 ",
        "18446744073709551616",
        "<@1>",
        "１",
    ] {
        assert_eq!(
            render_warning_history(target, &page),
            Err(WarningHistoryUnavailable)
        );
    }
}

#[test]
fn refuses_a_page_if_any_reference_is_malformed() {
    for invalid in [
        "W-".to_owned(),
        format!("R-{}", "a".repeat(64)),
        format!("W-{}", "a".repeat(65)),
        format!("W-A{}", "a".repeat(63)),
        format!("W-`{}", "a".repeat(63)),
        format!("W-\n{}", "a".repeat(63)),
        format!("W-\u{202e}{}", "a".repeat(61)),
    ] {
        let mut last = entry(2, EARLIEST, None);
        last.reference = invalid;
        let page = page(vec![entry(1, LATEST, None), last]);
        assert_eq!(
            render_warning_history(TARGET, &page),
            Err(WarningHistoryUnavailable)
        );
    }
}

#[test]
fn refuses_a_page_if_any_timestamp_is_malformed() {
    for invalid in [
        "0000-01-01T00:00:00Z",
        "10000-01-01T00:00:00Z",
        "2026-02-29T00:00:00Z",
        "2026-11-24T00:00:60Z",
        "2026-11-24T00:00:00+01:00",
        "2026-11-24T00:00:00.0Z",
        "2026-11-24T00:00:00.1234567890000Z",
        "2026-11-24T00:00:00@everyoneZ",
        "2026-11-24T00:00:00\n`Z",
        "2026-11-24T00:00:00\u{2066}Z",
        "https://example.invalid/Z",
    ] {
        let page = page(vec![entry(1, LATEST, None), entry(2, invalid, None)]);
        assert_eq!(
            render_warning_history(TARGET, &page),
            Err(WarningHistoryUnavailable)
        );
    }
}
