//! Whole-row feed discovery acceptance; offline, test-only handoff.
//!
//! `FeedRelay::list_pages(rows, content_budget)` returns deterministic page
//! bodies in input order. The runtime reserves its page/navigation overhead
//! before choosing that UTF-16 budget. The test-local fallback below exercises
//! the real legacy renderer on the baseline, so missing paging fails assertions
//! rather than compilation. An inherent method with this signature takes
//! precedence once implemented; the legacy renderer remains backward compatible.

use two_bot_core::feeds::{feed_list_text, FeedKind, FeedRelay};

trait BaselineListPages {
    fn list_pages(rows: &[FeedRelay], content_budget: usize) -> Vec<String>;
}

impl BaselineListPages for FeedRelay {
    fn list_pages(rows: &[FeedRelay], _content_budget: usize) -> Vec<String> {
        vec![feed_list_text(rows)]
    }
}

fn relay(index: usize, source: String) -> FeedRelay {
    FeedRelay {
        id: format!("{index:032x}"),
        guild_id: "100000000000000001".into(),
        channel_id: "123456789012345678".into(),
        kind: FeedKind::Rss,
        source,
        enabled: !index.is_multiple_of(5),
        last_checked_at: None,
        created_by: "100000000000000002".into(),
        created_at: index as i64,
        updated_at: 0,
    }
}

fn middle_sources(count: usize) -> Vec<FeedRelay> {
    (1..=count)
        .map(|index| {
            relay(
                index,
                format!("https://example.org/news/section-{index:02}/updates?format=rss"),
            )
        })
        .collect()
}

fn assert_whole_rows(pages: &[String], rows: &[FeedRelay], budget: usize) {
    assert!(
        !pages.is_empty(),
        "even an empty list has a useful first page"
    );
    for page in pages {
        assert!(!page.trim().is_empty(), "no blank continuation pages");
        assert!(
            page.encode_utf16().count() <= budget,
            "page exceeds {budget} UTF-16 units: {page}"
        );
    }
    let displayed: Vec<&str> = pages
        .iter()
        .flat_map(|page| page.lines())
        .filter_map(|line| line.strip_prefix('`')?.split_once('`').map(|(id, _)| id))
        .collect();
    let expected: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        displayed, expected,
        "every exact removal ID occurs once, in stable order"
    );
    for row in rows {
        let needle = format!("`{}`", row.id);
        let line = pages
            .iter()
            .flat_map(|page| page.lines())
            .find(|line| line.starts_with(&needle))
            .expect("whole row exists");
        assert!(line.contains("rss"), "ID-only fallback loses kind: {line}");
        assert!(
            line.contains(&format!("<#{}>", row.channel_id)),
            "row loses channel: {line}"
        );
    }
}

#[test]
fn feed_list_definition_exposes_optional_positive_integer_page_without_relaxing_permissions() {
    let definitions = two_bot_core::announcement_commands();
    let definition = definitions
        .iter()
        .find(|command| command.name == "feed-list")
        .unwrap();
    assert_eq!(definition.default_member_permissions.as_deref(), Some("32"));
    assert!(!definition.dm_permission);
    assert_eq!(
        definition.options.len(),
        1,
        "the only additive input is page"
    );
    let page = &definition.options[0];
    assert_eq!(page.name, "page");
    assert_eq!(page.kind, 4, "Discord Integer, not a string or number");
    assert_ne!(page.required, Some(true), "omitted page remains valid");
    assert_eq!(page.min_value, Some(1));
    assert!(page.max_value.is_none_or(|max| max >= 2));
    assert!(!page.description.trim().is_empty());
}

#[test]
fn feed_pages_empty_and_short_controls_preserve_existing_context() {
    // Qualified call keeps the baseline trait live after an inherent method lands.
    assert_eq!(
        <FeedRelay as BaselineListPages>::list_pages(&[], 1900),
        ["No feed relays configured."]
    );
    assert_eq!(
        FeedRelay::list_pages(&[], 1900),
        ["No feed relays configured."]
    );
    let rows = middle_sources(3);
    let pages = FeedRelay::list_pages(&rows, 1900);
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0], feed_list_text(&rows));
    assert_whole_rows(&pages, &rows, 1900);
    for row in &rows {
        assert!(pages[0].contains(&row.source));
    }
}

#[test]
fn feed_pages_capacity_keeps_middle_path_sources_distinguishable() {
    let rows = middle_sources(25);
    let pages = FeedRelay::list_pages(&rows, 1900);
    assert_whole_rows(&pages, &rows, 1900);
    assert!(
        pages.len() > 1,
        "page whole rows instead of squeezing every source"
    );
    for row in &rows {
        let needle = format!("`{}`", row.id);
        let line = pages
            .iter()
            .flat_map(|page| page.lines())
            .find(|line| line.starts_with(&needle))
            .unwrap();
        assert!(
            line.contains(&row.source),
            "a supported short source must stay useful: {line}"
        );
    }
    assert_eq!(
        pages,
        FeedRelay::list_pages(&rows, 1900),
        "packing is deterministic"
    );
}

#[test]
fn feed_pages_restored_excess_and_long_ids_never_hide_trailing_rows() {
    let mut rows = middle_sources(70);
    for (index, row) in rows.iter_mut().enumerate() {
        row.id = format!("restored-{index:03}-{}", "a".repeat(256));
    }
    rows.rotate_left(17);
    let pages = FeedRelay::list_pages(&rows, 1900);
    assert_whole_rows(&pages, &rows, 1900);
    assert!(pages.len() > 1);
    for row in &rows {
        assert!(
            pages.iter().any(|page| page.contains(&row.source)),
            "paging must not degrade restored rows to bare IDs"
        );
    }
}

#[test]
fn feed_pages_exact_boundary_counts_utf16_and_moves_only_whole_rows() {
    let rows = [
        FeedRelay {
            id: "boundary-a".into(),
            ..relay(1, "https://example.org/🦀a".into())
        },
        FeedRelay {
            id: "boundary-b".into(),
            ..relay(2, "https://example.org/🦀b".into())
        },
    ];
    let first = "`boundary-a` rss enabled → <#123456789012345678> https://example.org/🦀a";
    let second = "`boundary-b` rss enabled → <#123456789012345678> https://example.org/🦀b";
    // Each row has 72 UTF-16 units (64 legacy + 8 for the `enabled` state
    // word), plus one joining newline.
    assert_eq!(first.encode_utf16().count(), 72);
    assert_eq!(second.encode_utf16().count(), 72);
    assert_eq!(
        FeedRelay::list_pages(&rows, 145),
        [format!("{first}\n{second}")]
    );
    let pages = FeedRelay::list_pages(&rows, 144);
    assert_eq!(pages, [first, second]);
    assert_whole_rows(&pages, &rows, 144);
}

#[test]
fn feed_pages_astral_sources_keep_short_context_without_scalar_budget_overflow() {
    let rows: Vec<_> = (1..=25)
        .map(|index| {
            relay(
                index,
                format!(
                    "https://example.org/{}section-{index:02}?format=rss",
                    "🦀".repeat(20)
                ),
            )
        })
        .collect();
    let pages = FeedRelay::list_pages(&rows, 1900);
    assert_whole_rows(&pages, &rows, 1900);
    for row in &rows {
        let line = pages
            .iter()
            .flat_map(|page| page.lines())
            .find(|line| line.starts_with(&format!("`{}`", row.id)))
            .unwrap();
        assert!(line.contains(&row.source), "a source below the existing per-row context budget must not shrink at capacity: {line}");
    }
}

#[test]
fn feed_pages_unavoidable_per_row_truncation_is_explicit() {
    let rows = vec![relay(
        1,
        format!("https://example.org/{}?format=rss", "a".repeat(2000)),
    )];
    assert!(two_bot_core::feeds_http::validate_source(&rows[0].source).is_ok());
    let pages = FeedRelay::list_pages(&rows, 1900);
    assert_whole_rows(&pages, &rows, 1900);
    assert_eq!(pages.len(), 1);
    let line = &pages[0];
    assert!(
        line.contains('…') || line.to_ascii_lowercase().contains("truncated"),
        "source truncation must be explicit: {line}"
    );
    assert!(
        line.contains("example.org"),
        "keep useful source context: {line}"
    );
}
