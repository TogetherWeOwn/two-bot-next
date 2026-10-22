//! Supplemental pure-renderer acceptance for the one-based page API.
//! Defensive Unicode/oversized rows exercise the renderer, not name validation.
//! No database, command publication, or Discord connection is needed.

use two_bot_core::custom_commands::{format_command_list_page, StoredCommand};

fn row(name: String, text_trigger: Option<String>, enabled: bool) -> StoredCommand {
    StoredCommand {
        guild_id: "2222".to_owned(),
        name,
        description: "Renderer edge fixture".to_owned(),
        template: "Hello".to_owned(),
        text_trigger,
        enabled,
    }
}

fn assert_page_metadata(content: &str, page: usize, total: usize) {
    let metadata = content
        .lines()
        .find(|line| line.starts_with("Page "))
        .expect("nonempty pages identify their page and total");
    let fields: Vec<_> = metadata.split_whitespace().take(4).collect();
    assert_eq!(
        fields,
        ["Page", &page.to_string(), "of", &total.to_string()]
    );
    assert!(content.encode_utf16().count() <= 2000, "{content}");
    if page < total {
        assert!(
            content.contains(&format!("/command-list page:{}", page + 1)),
            "nonfinal page must offer the next page: {content}"
        );
    } else {
        assert!(
            !content.contains(&format!("/command-list page:{}", total + 1)),
            "final page must not offer a nonexistent next page: {content}"
        );
    }
}

#[test]
fn list_page_rejects_zero_and_past_end_without_wrapping_or_panicking() {
    let rows = [row("faq".to_owned(), None, true)];
    assert_eq!(
        format_command_list_page(&rows, 1),
        Ok("/faq — on\n\nPage 1 of 1".to_owned())
    );
    for page in [0, 2, usize::MAX] {
        assert!(
            format_command_list_page(&rows, page).is_err(),
            "page {page} must be rejected rather than clamped or wrapped"
        );
    }
}

#[test]
fn list_page_empty_copy_has_no_footer_or_phantom_page() {
    assert_eq!(
        format_command_list_page(&[], 1),
        Ok("No custom commands defined.".to_owned())
    );
    for page in [0, 2, usize::MAX] {
        assert!(format_command_list_page(&[], page).is_err());
    }
}

#[test]
fn list_page_exact_utf16_ceiling_keeps_the_entire_row_and_footer() {
    // '/' + 1,981 name units + ' — on' = 1,987; the single-page footer
    // adds 13 units. Astral names have more UTF-8 bytes but the same budget.
    for name in ["x".repeat(1981), format!("{}x", "😀".repeat(990))] {
        let expected = format!("/{name} — on\n\nPage 1 of 1");
        assert_eq!(expected.encode_utf16().count(), 2000);
        let rows = [row(name, None, true)];
        assert_eq!(format_command_list_page(&rows, 1), Ok(expected));
        assert!(format_command_list_page(&rows, 2).is_err());
    }
}

#[test]
fn list_page_mixed_rows_survive_every_page_once_in_input_order() {
    let mut rows = Vec::new();
    let mut expected = Vec::new();
    // Descending names make accidental sorting visible. Vary width, trigger
    // presence and enabled state so dropping an inconvenient row cannot pass.
    for index in (0..61).rev() {
        let name = format!("c{index:02}{}", "😀".repeat(10 + index % 17));
        let state = if index % 2 == 0 { "on" } else { "off" };
        let trigger = (index % 3 != 0).then(|| format!("!{index:02}{}", "🚀".repeat(17)));
        let line = match &trigger {
            Some(trigger) => format!("/{name} (or {trigger}) — {state}"),
            None => format!("/{name} — {state}"),
        };
        rows.push(row(name, trigger, index % 2 == 0));
        expected.push(line);
    }

    let mut pages: Vec<String> = Vec::new();
    // A nonempty page contains at least one row: at most rows.len() pages.
    // Stop at the first refusal; the independent row oracle detects any
    // prematurely reported end, omission, duplication, or partial row.
    for page in 1..=rows.len() + 1 {
        match format_command_list_page(&rows, page) {
            Ok(content) => pages.push(content),
            Err(_) => break,
        }
    }
    assert!(pages.len() > 1, "fixture must require multiple pages");
    assert!(pages.len() <= rows.len(), "no empty phantom pages");
    let mut actual = Vec::new();
    for (index, content) in pages.iter().enumerate() {
        let page = index + 1;
        assert_page_metadata(content, page, pages.len());
        assert_eq!(
            format_command_list_page(&rows, page),
            Ok(content.clone()),
            "same snapshot/page must render deterministically"
        );
        let lines: Vec<_> = content
            .split_once("\n\n")
            .expect("row body is separated from metadata")
            .0
            .lines()
            .map(str::to_owned)
            .collect();
        assert!(!lines.is_empty(), "every nonempty-list page carries rows");
        actual.extend(lines);
    }
    assert_eq!(actual, expected, "all rows remain complete and ordered");
    assert!(format_command_list_page(&rows, pages.len() + 1).is_err());
    assert!(format_command_list_page(&rows, usize::MAX).is_err());
}

#[test]
fn list_page_metadata_and_budget_hold_across_page_and_total_digit_boundaries() {
    for total in [9, 10, 99, 100] {
        // Each name alone is 1,024 UTF-16 units, so two whole rows cannot
        // share a page. These fixtures force the total independently of the
        // packer's strategy, including pages 9/10 and 99/100.
        let rows: Vec<_> = (0..total)
            .map(|index| {
                row(
                    format!("z{:03}{}", total - index, "😀".repeat(510)),
                    Some(format!("!{index:03}")),
                    index % 2 == 0,
                )
            })
            .collect();
        for page in 1..=total {
            let index = page - 1;
            let state = if index % 2 == 0 { "on" } else { "off" };
            let expected = format!(
                "/z{:03}{} (or !{index:03}) — {state}",
                total - index,
                "😀".repeat(510)
            );
            let content = format_command_list_page(&rows, page).expect("whole row fits");
            assert_page_metadata(&content, page, total);
            assert_eq!(
                content
                    .split_once("\n\n")
                    .expect("row body is separated from metadata")
                    .0,
                expected,
                "page {page}/{total} must retain exactly its input row"
            );
        }
        assert!(format_command_list_page(&rows, total + 1).is_err());
    }
}

#[test]
fn list_page_rejects_an_individually_oversized_row_instead_of_truncating() {
    // Each rendered row is exactly 2,001 UTF-16 units before metadata.
    // The astral cases catch scalar-count budgeting; trigger overflow must
    // not be hidden by truncating just the optional trigger.
    let cases = [
        row("x".repeat(1995), None, true),
        row(format!("{}x", "😀".repeat(997)), None, true),
        row("x".to_owned(), Some("!".repeat(1988)), true),
        row(
            "x".to_owned(),
            Some(format!("!!{}", "🚀".repeat(993))),
            true,
        ),
    ];
    for stored in cases {
        assert!(
            format_command_list_page(&[stored], 1).is_err(),
            "an oversized whole row cannot produce a successful partial page"
        );
    }
}
