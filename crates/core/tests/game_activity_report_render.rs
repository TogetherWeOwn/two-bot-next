//! Acceptance pins for the read-only aggregate game-activity operator
//! report renderer (pure layer; the DB and CLI halves live in
//! `crates/cutover/tests/game_activity_report_db.rs` and
//! `game_activity_report_cli.rs`).
//!
//! The contract these tests pin, mirroring the reengagement renderer's
//! placement next to its domain type (`crates/core/src/inactivity.rs`):
//!
//! - `render_game_activity_report(guild_id, rows)` emits one JSON object:
//!   `{"guild_id": <echo>, "rows": [...], "truncated": <bool>}` plus a
//!   `"hint"` string exactly when `rows` is empty. Each row is exactly
//!   `{"game", "total_play_ms", "in_voice_ms", "session_count"}` — there is
//!   no member/user field anywhere (the aggregates table carries no member
//!   column by construction, and the report must not invent one).
//! - Untitled games (`game: None`, stored as `''`) render as the placeholder
//!   `GAME_ACTIVITY_UNTITLED_PLACEHOLDER` = `"(untitled)"`.
//! - Ordering is deterministic and input-order independent:
//!   `total_play_ms` desc, then `in_voice_ms` desc, then game title asc
//!   with the untitled row first among full ties.
//! - At most `GAME_ACTIVITY_REPORT_MAX_ROWS` (= 100) rows render;
//!   `"truncated": true` when bounded input was cut.
//! - Rows belonging to another guild never render, even if a caller hands
//!   them in (defense in depth behind the guild-scoped store read).
//! - `render_game_activity_unscoped()` is the no-`--guild` output: the same
//!   object shape with `"guild_id": null`, no rows, and a hint naming
//!   `--guild` (the `render_reengagement_unscoped` precedent).

use two_bot_core::game_activity::{
    render_game_activity_report, render_game_activity_unscoped, GuildGameAggregate,
    GAME_ACTIVITY_REPORT_MAX_ROWS, GAME_ACTIVITY_UNTITLED_PLACEHOLDER,
};

const GUILD: &str = "100000000000000010";
const OTHER_GUILD: &str = "100000000000000011";
const MIN_MS: i64 = 60_000;

fn aggregate(
    guild_id: &str,
    game: Option<&str>,
    total_play_ms: i64,
    in_voice_ms: i64,
    session_count: u64,
) -> GuildGameAggregate {
    GuildGameAggregate {
        guild_id: guild_id.to_owned(),
        game: game.map(str::to_owned),
        total_play_ms,
        in_voice_ms,
        session_count,
    }
}

fn parse(report: &str) -> serde_json::Value {
    serde_json::from_str(report).expect("report is one JSON object")
}

fn rows_of(report: &serde_json::Value) -> &Vec<serde_json::Value> {
    report["rows"].as_array().expect("rows is an array")
}

#[test]
fn seeded_rows_render_exactly_in_pinned_order_with_placeholder() {
    // Deliberately shuffled input: rendering must not inherit caller order.
    let rows = vec![
        aggregate(GUILD, Some("Apex"), 40 * MIN_MS, MIN_MS, 3),
        aggregate(GUILD, None, MIN_MS, 0, 1),
        aggregate(GUILD, Some("Zelda"), 40 * MIN_MS, 2 * MIN_MS, 2),
    ];
    let parsed = parse(&render_game_activity_report(GUILD, &rows));
    assert_eq!(parsed["guild_id"], GUILD);
    assert_eq!(parsed["truncated"], false);
    assert!(
        parsed.get("hint").is_none(),
        "hint is an empty-report field only: {parsed}"
    );
    // total desc, in-voice desc breaks the 40-minute tie, untitled last
    // because its totals are smallest (not because it is untitled).
    assert_eq!(
        rows_of(&parsed),
        &vec![
            serde_json::json!({"game": "Zelda", "total_play_ms": 40 * MIN_MS, "in_voice_ms": 2 * MIN_MS, "session_count": 2}),
            serde_json::json!({"game": "Apex", "total_play_ms": 40 * MIN_MS, "in_voice_ms": MIN_MS, "session_count": 3}),
            serde_json::json!({"game": "(untitled)", "total_play_ms": MIN_MS, "in_voice_ms": 0, "session_count": 1}),
        ]
    );
    assert_eq!(GAME_ACTIVITY_UNTITLED_PLACEHOLDER, "(untitled)");
}

#[test]
fn full_ties_order_by_title_with_untitled_first_and_output_is_stable() {
    let rows = vec![
        aggregate(GUILD, Some("Zelda"), 10 * MIN_MS, MIN_MS, 1),
        aggregate(GUILD, Some("Apex"), 10 * MIN_MS, MIN_MS, 9),
        aggregate(GUILD, None, 10 * MIN_MS, MIN_MS, 5),
    ];
    let forward = render_game_activity_report(GUILD, &rows);
    let mut reversed = rows.clone();
    reversed.reverse();
    assert_eq!(
        forward,
        render_game_activity_report(GUILD, &reversed),
        "rendering is input-order independent"
    );
    assert_eq!(forward, render_game_activity_report(GUILD, &rows));
    let parsed = parse(&forward);
    let games: Vec<_> = rows_of(&parsed)
        .iter()
        .map(|row| row["game"].as_str().expect("game is a string"))
        .collect();
    assert_eq!(games, ["(untitled)", "Apex", "Zelda"]);
}

#[test]
fn another_guilds_rows_never_render() {
    let rows = vec![
        aggregate(
            OTHER_GUILD,
            Some("SecretOtherGuildGame"),
            999 * MIN_MS,
            0,
            7,
        ),
        aggregate(GUILD, Some("Apex"), MIN_MS, 0, 1),
    ];
    let report = render_game_activity_report(GUILD, &rows);
    assert!(
        !report.contains("SecretOtherGuildGame"),
        "other guild's game leaked:\n{report}"
    );
    assert!(
        !report.contains(OTHER_GUILD),
        "other guild id leaked:\n{report}"
    );
    let parsed = parse(&report);
    assert_eq!(rows_of(&parsed).len(), 1);
    assert_eq!(rows_of(&parsed)[0]["game"], "Apex");
}

#[test]
fn empty_guild_renders_header_shape_plus_hint() {
    let parsed = parse(&render_game_activity_report(GUILD, &[]));
    // The "header" of the JSON contract: the object shape is always present,
    // so an empty guild still answers with guild echo, empty rows, the
    // truncation flag and the hint.
    assert_eq!(parsed["guild_id"], GUILD);
    assert!(rows_of(&parsed).is_empty());
    assert_eq!(parsed["truncated"], false);
    assert_eq!(parsed["hint"], "no game activity recorded for this guild");
}

#[test]
fn unscoped_render_names_the_guild_flag_in_its_hint() {
    let parsed = parse(&render_game_activity_unscoped());
    assert_eq!(parsed["guild_id"], serde_json::Value::Null);
    assert!(rows_of(&parsed).is_empty());
    assert_eq!(parsed["truncated"], false);
    let hint = parsed["hint"].as_str().expect("unscoped hint is a string");
    assert!(hint.contains("--guild"), "hint names the flag: {hint}");
}

#[test]
fn row_limit_is_one_hundred_and_truncation_is_flagged() {
    assert_eq!(GAME_ACTIVITY_REPORT_MAX_ROWS, 100);
    // 105 rows with strictly increasing play time; the five smallest drop.
    let rows: Vec<_> = (0..105)
        .map(|i| aggregate(GUILD, Some(&format!("Game {i:03}")), (i + 1) * MIN_MS, 0, 1))
        .collect();
    let parsed = parse(&render_game_activity_report(GUILD, &rows));
    assert_eq!(parsed["truncated"], true);
    let rendered = rows_of(&parsed);
    assert_eq!(rendered.len(), GAME_ACTIVITY_REPORT_MAX_ROWS);
    assert_eq!(rendered[0]["game"], "Game 104", "largest total first");
    assert_eq!(
        rendered.last().expect("rows render")["game"],
        "Game 005",
        "the five smallest totals are the ones cut"
    );
}

#[test]
fn exactly_at_the_limit_is_not_truncated() {
    let rows: Vec<_> = (0..GAME_ACTIVITY_REPORT_MAX_ROWS)
        .map(|i| aggregate(GUILD, Some(&format!("Game {i:03}")), MIN_MS, 0, 1))
        .collect();
    let parsed = parse(&render_game_activity_report(GUILD, &rows));
    assert_eq!(parsed["truncated"], false);
    assert_eq!(rows_of(&parsed).len(), GAME_ACTIVITY_REPORT_MAX_ROWS);
}

#[test]
fn output_carries_no_member_surface_and_exact_key_sets() {
    let rows = vec![aggregate(GUILD, Some("Apex"), MIN_MS, 0, 1)];
    let report = render_game_activity_report(GUILD, &rows);
    let lowered = report.to_lowercase();
    for forbidden in ["member", "user", "player"] {
        assert!(
            !lowered.contains(forbidden),
            "report gained a per-person surface ({forbidden}):\n{report}"
        );
    }
    let parsed = parse(&report);
    let mut top_keys: Vec<_> = parsed
        .as_object()
        .expect("top level is an object")
        .keys()
        .map(String::as_str)
        .collect();
    top_keys.sort_unstable();
    assert_eq!(top_keys, ["guild_id", "rows", "truncated"]);
    let mut row_keys: Vec<_> = rows_of(&parsed)[0]
        .as_object()
        .expect("row is an object")
        .keys()
        .map(String::as_str)
        .collect();
    row_keys.sort_unstable();
    assert_eq!(
        row_keys,
        ["game", "in_voice_ms", "session_count", "total_play_ms"]
    );
}

#[test]
fn free_text_game_titles_round_trip_through_json() {
    // Game titles are presence-supplied free text: quotes, commas and
    // newlines must survive as data, never break the document shape.
    let title = "Quote \"Comma, Newline\nTab\tEnd\"";
    let rows = vec![aggregate(GUILD, Some(title), MIN_MS, 0, 1)];
    let parsed = parse(&render_game_activity_report(GUILD, &rows));
    assert_eq!(rows_of(&parsed)[0]["game"], title);
}
