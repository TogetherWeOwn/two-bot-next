//! Channel-list budget regressions using production-shaped, no-hold rows.
//! Disabled rows have no localized delivery suffix; recurring definitions may
//! remain disabled. No database, arbitrary padding, or delivery-hold fixture.

use two_bot_core::scheduled::{schedule_list_channel_pages, schedule_list_line};

const CHANNEL: &str = "1234567890123456789";
const DUE: &str = "2030-01-01T00:00:00.000Z";
const HEADING: &str = "Schedules in this channel:\n";

fn rows(count: u32, recurring: u32) -> Vec<String> {
    (0..count)
        .map(|n| {
            let id = format!("a1b2c3d4e5f60718293a4b5c{n:08x}");
            let interval = if n < recurring { Some(3600) } else { None };
            let line = schedule_list_line(&id, CHANNEL, DUE, interval, false);
            let cadence = if n < recurring { " every 60m" } else { "" };
            // Independently specified complete row, not an expectation derived
            // from the pager or its length/formatting helpers.
            assert_eq!(
                line,
                format!(
                    "`{id}` `<#1234567890123456789>` 2030-01-01T00:00:00.000Z{cadence} (disabled)"
                )
            );
            assert_eq!(
                line.encode_utf16().count(),
                if n < recurring { 105 } else { 95 }
            );
            line
        })
        .collect()
}

fn assert_whole_replies(pages: &[String], expected: &[String], footers: &[&str]) {
    let mut found = Vec::new();
    for (page, footer) in pages.iter().zip(footers) {
        assert!(
            page.encode_utf16().count() <= 2000,
            "heading, whole rows and footer must fit the final reply: {page}"
        );
        let body = page.strip_prefix(HEADING).expect("heading on every reply");
        let body = body.strip_suffix(footer).expect("exact global page footer");
        found.extend(body.lines());
    }
    assert_eq!(pages.len(), footers.len());
    assert_eq!(
        found,
        expected.iter().map(String::as_str).collect::<Vec<_>>(),
        "every complete row and distinct 32-hex id exactly once, in order"
    );
}

#[test]
fn channel_heading_reservation_splits_otherwise_fitting_no_hold_rows() {
    // Break caught: remove only the heading reservation from the row budget.
    // 14 * 95 + 6 * 105 + 19 separators = 1979; heading adds 27 = 2006.
    let rows = rows(20, 6);
    assert_eq!(rows.join("\n").encode_utf16().count(), 1979);
    let pages = schedule_list_channel_pages(&rows, 0, false);
    assert_whole_replies(&pages, &rows, &["\nPage 1 of 2", "\nPage 2 of 2"]);
    assert_eq!(
        pages[0]
            .lines()
            .filter(|line| line.starts_with('`'))
            .count(),
        19
    );
}

#[test]
fn channel_footer_reservation_reflows_a_tight_global_window() {
    // Break caught: reserve the heading but not the footer, or size the
    // footer with local rather than global numbers. The first 20 rows need
    // 1961 units: heading + local footer would fit at 2000, but the global
    // footer needs 2002. The 21st row forces a multi-page window and footers.
    let mut rows = rows(21, 4);
    rows[0] = schedule_list_line(
        "a1b2c3d4e5f60718293a4b5c00000000",
        CHANNEL,
        DUE,
        Some(360_000),
        false,
    );
    assert_eq!(
        rows[0],
        "`a1b2c3d4e5f60718293a4b5c00000000` `<#1234567890123456789>` 2030-01-01T00:00:00.000Z every 6000m (disabled)"
    );
    assert_eq!(rows[..20].join("\n").encode_utf16().count(), 1961);
    let pages = schedule_list_channel_pages(&rows, 9, false);
    assert_whole_replies(&pages, &rows, &["\nPage 10 of 11", "\nPage 11 of 11"]);
    assert_eq!(
        pages[0]
            .lines()
            .filter(|line| line.starts_with('`'))
            .count(),
        19
    );
}

#[test]
fn channel_global_footer_keeps_an_exactly_full_reply_whole() {
    // Positive boundary: 20 rows join to 1959; heading 27 + footer 14 = 2000.
    // Over-reserving must not unnecessarily move the twentieth row.
    let rows = rows(21, 4);
    assert_eq!(rows[..20].join("\n").encode_utf16().count(), 1959);
    let pages = schedule_list_channel_pages(&rows, 9, false);
    assert_whole_replies(&pages, &rows, &["\nPage 10 of 11", "\nPage 11 of 11"]);
    assert_eq!(pages[0].encode_utf16().count(), 2000);
    assert_eq!(
        pages[0]
            .lines()
            .filter(|line| line.starts_with('`'))
            .count(),
        20
    );
}

#[test]
fn channel_single_page_window_stays_unlabelled_even_with_offset_and_hidden_rows() {
    // These 20 rows plus the heading fit at 1996 without any footer. Preserve
    // the existing single-window convention, including a hidden continuation.
    let rows = rows(20, 5);
    let expected = format!("Schedules in this channel:\n{}", rows.join("\n"));
    assert_eq!(expected.encode_utf16().count(), 1996);
    for (offset, hidden) in [(0, false), (9, false), (9, true)] {
        assert_eq!(
            schedule_list_channel_pages(&rows, offset, hidden),
            [expected.clone()]
        );
    }
}
