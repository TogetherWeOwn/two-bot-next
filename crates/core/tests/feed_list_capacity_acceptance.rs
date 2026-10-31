//! Feed-list capacity acceptance (pure, offline).
//!
//! Failing-first contract: at the supported capacity of 25 feed relays per
//! guild (disabled relays included), `/feed-list` must keep every relay's
//! exact removal id discoverable in its single ephemeral response. The
//! response stays within Discord's 2,000 UTF-16 unit limit, so long sources
//! shrink to fit; trailing rows are never dropped to make room for earlier
//! sources. Empty and short lists keep their current useful output.
//!
//! Test files only; no production code is touched here. The 25-relay tests
//! FAIL today (the whole list is truncated at 2,000 units after each source
//! is capped at 128, so trailing ids vanish) and must turn green under the
//! Builder without changing these tests. The empty/short controls pass
//! today and pin output the fix must not regress.

use two_bot_core::feeds::*;

/// Real relay ids are 32 hex characters; use that exact shape so the
/// per-row budget in these tests matches production rows.
fn relay_id(index: usize) -> String {
    format!("{index:032x}")
}

fn relay(index: usize, source: String) -> FeedRelay {
    let kind = [FeedKind::Rss, FeedKind::Youtube, FeedKind::Twitch][index % 3];
    FeedRelay {
        id: relay_id(index),
        guild_id: "guild-1".into(),
        // A realistic 19-digit channel snowflake.
        channel_id: "1234567890123456789".into(),
        kind,
        source,
        // Every fifth relay is disabled, including the trailing one:
        // disabled relays still count toward capacity and stay listed.
        enabled: !index.is_multiple_of(5),
        last_checked_at: None,
        created_by: "actor".into(),
        created_at: 0,
        updated_at: 0,
    }
}

fn assert_every_id_discoverable(text: &str, indexes: std::ops::RangeInclusive<usize>) {
    assert!(
        text.encode_utf16().count() <= 2000,
        "response exceeds the 2,000 UTF-16 unit limit ({} units)",
        text.encode_utf16().count()
    );
    let mut previous: Option<usize> = None;
    for index in indexes {
        let needle = format!("`{}`", relay_id(index));
        let position = text
            .find(&needle)
            .unwrap_or_else(|| panic!("relay {index} id {needle} not discoverable in: {text}"));
        if let Some(previous) = previous {
            assert!(position > previous, "relay ids stay in list order: {text}");
        }
        previous = Some(position);
    }
}

#[test]
fn feed_list_empty_keeps_useful_output() {
    assert_eq!(feed_list_text(&[]), "No feed relays configured.");
}

#[test]
fn feed_list_short_list_keeps_full_sources() {
    let feeds: Vec<_> = (1..=3)
        .map(|index| relay(index, format!("https://example.org/feed-{index}.xml")))
        .collect();
    let text = feed_list_text(&feeds);

    assert_every_id_discoverable(&text, 1..=3);
    for feed in &feeds {
        assert!(
            text.contains(&feed.source),
            "short list keeps the full source {}: {text}",
            feed.source
        );
    }
}

#[test]
fn feed_list_at_25_relays_with_long_sources_keeps_every_removal_id() {
    let feeds: Vec<_> = (1..=25)
        .map(|index| {
            relay(
                index,
                format!("https://example.org/{}/feed-{index}", "a".repeat(300)),
            )
        })
        .collect();
    let text = feed_list_text(&feeds);

    assert_every_id_discoverable(&text, 1..=25);
}

#[test]
fn feed_list_at_25_relays_with_astral_sources_keeps_every_removal_id() {
    // Each crab is one Unicode scalar but two UTF-16 units, so a source of
    // 90 crabs alone spans 180 units before any per-row capping.
    let feeds: Vec<_> = (1..=25)
        .map(|index| {
            relay(
                index,
                format!("https://example.org/{}feed-{index}", "🦀".repeat(90)),
            )
        })
        .collect();
    let text = feed_list_text(&feeds);

    assert_every_id_discoverable(&text, 1..=25);
}
