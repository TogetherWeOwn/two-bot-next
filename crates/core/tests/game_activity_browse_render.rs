//! Pure-renderer pins for the selectable Discord browse view
//! (`render_game_activity_discord_browse`): deterministic ten-row pages in
//! both orderings, honest out-of-range reporting, and readable durations
//! that never wrap at 24 hours.

use two_bot_core::game_activity::{
    render_game_activity_discord_browse, GameMixBrowseFormat, GameMixBrowseSort, GameMixBrowseView,
    GuildGameAggregate,
};

const GUILD: u64 = 100_000_000_000_000_010;

fn aggregate(game: &str, total_play_ms: i64, in_voice_ms: i64) -> GuildGameAggregate {
    GuildGameAggregate {
        guild_id: GUILD.to_string(),
        game: Some(game.to_owned()),
        total_play_ms,
        in_voice_ms,
        session_count: 1,
    }
}

fn page(rows: &[GuildGameAggregate], number: usize, sort: GameMixBrowseSort) -> String {
    match render_game_activity_discord_browse(GUILD, rows, number, sort, GameMixBrowseFormat::Json)
    {
        GameMixBrowseView::Page(text) => text,
        view => panic!("expected a page, got {view:?}"),
    }
}

fn row_names(text: &str) -> Vec<String> {
    let start = text.find("```json\n").expect("json fence") + 8;
    let end = text[start..].find("\n```").expect("closing fence") + start;
    let value: serde_json::Value = serde_json::from_str(&text[start..end]).expect("complete JSON");
    value["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["game"].as_str().expect("game label").to_owned())
        .collect()
}

#[test]
fn pages_reach_every_row_once_in_both_orders() {
    let rows: Vec<_> = (0..13)
        .map(|index| {
            aggregate(
                &format!("Game{index:02}"),
                100_000 + index * 1000,
                13_000 - index * 1000,
            )
        })
        .collect();
    for (sort, expected) in [
        (GameMixBrowseSort::Play, (0..13).rev().collect::<Vec<_>>()),
        (GameMixBrowseSort::Voice, (0..13).collect::<Vec<_>>()),
    ] {
        let seen: Vec<String> = (1..=2)
            .flat_map(|number| row_names(&page(&rows, number, sort)))
            .collect();
        assert_eq!(
            seen,
            expected
                .iter()
                .map(|index| format!("Game{index:02}"))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn secondary_totals_break_primary_ties_in_both_orders_across_the_page_boundary() {
    for sort in [GameMixBrowseSort::Play, GameMixBrowseSort::Voice] {
        // Ascending input/title order opposes descending secondary totals.
        let rows: Vec<_> = (0..12)
            .map(|index| match sort {
                GameMixBrowseSort::Play => {
                    aggregate(&format!("Tie{index:02}"), 60_000, index * 1000)
                }
                GameMixBrowseSort::Voice => {
                    aggregate(&format!("Tie{index:02}"), 60_000 + index * 1000, 30_000)
                }
            })
            .collect();
        assert_eq!(
            row_names(&page(&rows, 1, sort)),
            [
                "Tie11", "Tie10", "Tie09", "Tie08", "Tie07", "Tie06", "Tie05", "Tie04", "Tie03",
                "Tie02",
            ],
            "secondary totals must decide the first ten rows for {sort:?}"
        );
        assert_eq!(
            row_names(&page(&rows, 2, sort)),
            ["Tie01", "Tie00"],
            "secondary totals must decide the remaining rows for {sort:?}"
        );
    }
}

#[test]
fn full_ties_order_by_game_title_across_the_page_boundary() {
    let rows: Vec<_> = (0..12)
        .rev()
        .map(|index| aggregate(&format!("Tie{index:02}"), 60_000, 30_000))
        .collect();
    let seen: Vec<String> = (1..=2)
        .flat_map(|number| row_names(&page(&rows, number, GameMixBrowseSort::Voice)))
        .collect();
    assert_eq!(
        seen,
        (0..12)
            .map(|index| format!("Tie{index:02}"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn out_of_range_reports_the_real_page_bound() {
    let rows = vec![aggregate("Solo", 60_000, 0)];
    assert_eq!(
        render_game_activity_discord_browse(
            GUILD,
            &rows,
            2,
            GameMixBrowseSort::Play,
            GameMixBrowseFormat::Json,
        ),
        GameMixBrowseView::OutOfRange { total: 1, pages: 1 }
    );
    assert_eq!(
        render_game_activity_discord_browse(
            GUILD,
            &[],
            2,
            GameMixBrowseSort::Play,
            GameMixBrowseFormat::Summary,
        ),
        GameMixBrowseView::OutOfRange { total: 0, pages: 1 }
    );
}

#[test]
fn summary_durations_use_accumulated_hours_without_wrapping() {
    let rows = vec![
        aggregate("Solo", 3_723_000, 62_000),
        aggregate("Huge", i64::MAX, 9_007_199_254_740_993),
    ];
    let GameMixBrowseView::Page(text) = render_game_activity_discord_browse(
        GUILD,
        &rows,
        1,
        GameMixBrowseSort::Play,
        GameMixBrowseFormat::Summary,
    ) else {
        panic!("expected a summary page");
    };
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("- `"))
            .collect::<Vec<_>>(),
        [
            "- `Huge` — play 2562047788015h 12m 55s · in voice 2501999792h 59m 0s · 1 sessions",
            "- `Solo` — play 1h 2m 3s · in voice 0h 1m 2s · 1 sessions",
        ],
        "game identity, play, voice and sessions must stay bound: {text}"
    );
    assert!(text.contains("1h 2m 3s"), "{text}");
    assert!(text.contains("0h 1m 2s"), "{text}");
    assert!(text.contains("2562047788015h 12m 55s"), "{text}");
    assert!(text.contains("2501999792h 59m 0s"), "{text}");
    assert!(text.contains("overlap"), "{text}");
}

#[test]
fn summary_wraps_hostile_labels_in_inline_code() {
    let rows = vec![aggregate("[click](https://evil.example) `x`", 60_000, 0)];
    let GameMixBrowseView::Page(text) = render_game_activity_discord_browse(
        GUILD,
        &rows,
        1,
        GameMixBrowseSort::Play,
        GameMixBrowseFormat::Summary,
    ) else {
        panic!("expected a summary page");
    };
    assert!(
        text.contains("- `[click](https://evil.example)"),
        "masked-link label must sit inside inline code: {text}"
    );
    assert!(
        text.contains("` — play"),
        "inline code must close before the durations: {text}"
    );
    assert_eq!(
        text.matches('`').count(),
        2,
        "label backticks are neutralised so only the wrapper pair remains: {text}"
    );
}
