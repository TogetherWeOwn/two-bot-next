//! Protected runtime acceptance for browsing guild game aggregates.
//! Breaks caught: ignored selectors, repeated/dropped pages, unstable ties,
//! play-only ordering, rounded counters, JSON clipping, misleading durations,
//! pre-I/O validation/authority regressions, and writes/Discord side effects.
//!
//! Selector contract: optional page (positive bounded integer), sort
//! (play|voice), format (json|summary). Pages contain ten rows except the last;
//! optionless output and the operator CLI retain their existing JSON contract.
//! JSON metadata names are not pinned: selected page/sort may be in prose or
//! JSON. Summary layout is free; labelled h/m/s or HH:MM:SS durations work.

#![cfg(test)]

use super::*;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use two_bot_core::message_safety::contains_mass_mention;
use two_bot_testsupport::TestDatabase;

const COMMAND: &str = "game-mix";
const FOREIGN: &str = "9999";
const ACTOR: u64 = 887_766_554_433_221_101;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable test bootstrap");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database; never application credentials")
}

async fn seed(pool: &PgPool, guild: &str, game: &str, play: i64, voice: i64, sessions: i64) {
    sqlx::query("INSERT INTO game_activity_guild_aggregates (guild_id, game, total_play_ms, in_voice_ms, session_count) VALUES ($1,$2,$3,$4,$5)")
        .bind(guild).bind(game).bind(play).bind(voice).bind(sessions)
        .execute(pool).await.expect("seed synthetic aggregate");
}

async fn state(pool: &PgPool) -> (String, i64) {
    let rows = sqlx::query_scalar::<_, String>("SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY guild_id, game), '[]'::jsonb)::text FROM game_activity_guild_aggregates t")
        .fetch_one(pool).await.expect("snapshot every aggregate column");
    let audits = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log")
        .fetch_one(pool)
        .await
        .expect("snapshot audit count");
    (rows, audits)
}

fn selected(page: i64, sort: &str, format: &str) -> Vec<CommandDataOption> {
    vec![
        option("page", CommandOptionValue::Integer(page)),
        option("sort", CommandOptionValue::String(sort.to_owned())),
        option("format", CommandOptionValue::String(format.to_owned())),
    ]
}

fn staff(options: Vec<CommandDataOption>) -> Interaction {
    from_member(
        slash(COMMAND, Some(CHANNEL), options),
        ACTOR,
        Permissions::MANAGE_GUILD,
    )
}

async fn dispatch(pool: PgPool, interaction: Interaction, guild: u64) -> Vec<RestRequest> {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(RouterGates {
            configured_guild: Some(guild),
            ..gates(false, false)
        }),
        guild,
        false,
    );
    let guard = runtime.dispatch_guard();
    runtime.initialize_custom_commands_for_test(
        crate::gateway_commands::GatewayCommandConfig::from_map_for_activation(
            guild,
            &HashMap::from([("TWO_AUTOMATIONS".to_owned(), "0".to_owned())]),
            false,
        )
        .expect("test gateway configuration"),
    );
    let _ = runtime.dispatch(&interaction_event(interaction));
    wait_for(
        || (0..6).all(|lane| runtime.lane_in_flight(lane) == 0),
        "game-mix browse settles",
    )
    .await;
    let requests = mock.requests();
    drop(guard);
    drop(runtime);
    mock.shutdown().await;
    requests
}

fn content(requests: &[RestRequest]) -> String {
    assert!(!requests.is_empty(), "one private answer required");
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/callback"));
    let initial: Value = serde_json::from_slice(&requests[0].body).expect("callback JSON");
    assert_eq!(initial["data"]["flags"], 64, "private acknowledgement");
    let body = if initial["type"] == 5 {
        assert_eq!(requests.len(), 2, "defer + edit only, no DM/post/mutation");
        assert_eq!(requests[1].method, "PATCH");
        assert!(requests[1].path.ends_with("/messages/@original"));
        serde_json::from_slice::<Value>(&requests[1].body).expect("completion JSON")
    } else {
        assert_eq!(initial["type"], 4);
        assert_eq!(requests.len(), 1, "one answer, no other REST");
        initial["data"].clone()
    };
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    let text = body["content"].as_str().expect("content");
    assert!(
        text.encode_utf16().count() <= 2000,
        "whole Discord UTF-16 budget"
    );
    assert!(!contains_mass_mention(text), "mass mention escaped");
    assert!(
        !text.contains("<@") && !text.contains("<#"),
        "Discord pills inert"
    );
    assert!(!text.contains(&ACTOR.to_string()), "no invoker identity");
    assert!(!text.contains("ForeignOnly"), "guild isolation");
    text.to_owned()
}

fn report(text: &str) -> Value {
    let fences: Vec<_> = text.match_indices("```").collect();
    assert_eq!(
        fences.len(),
        2,
        "one complete JSON fence, no label breakout: {text}"
    );
    let start = fences[0].0;
    assert!(text[start..].starts_with("```json\n"));
    let value: Value =
        serde_json::from_str(&text[start + 8..fences[1].0]).expect("complete JSON, never clipped");
    assert_eq!(value["guild_id"], GUILD_S);
    for row in value["rows"].as_array().expect("rows") {
        let keys: BTreeSet<_> = row
            .as_object()
            .expect("row object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from(["game", "in_voice_ms", "session_count", "total_play_ms"])
        );
    }
    value
}

fn names(value: &Value) -> Vec<String> {
    value["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["game"].as_str().expect("game label").to_owned())
        .collect()
}

fn selection_notice(text: &str, page: usize, sort: &str) {
    let lower = text.to_lowercase();
    assert!(
        lower.contains("page") && lower.contains(&page.to_string()),
        "selected page identified: {text}"
    );
    assert!(lower.contains(sort), "selected ordering identified: {text}");
}

fn correction(text: &str, selector: &str) {
    assert!(text.encode_utf16().count() <= 500, "bounded correction");
    assert!(
        !text.contains("```json"),
        "bad selector must not serve a report"
    );
    let lower = text.to_lowercase();
    assert!(
        lower.contains(selector) && lower.contains(COMMAND),
        "name a corrective invocation: {text}"
    );
    assert!(
        !lower.contains("no game activity"),
        "invalid/out-of-range is not empty success"
    );
}

// A predicate (not a projected column) observes actual aggregate scans even
// under COUNT(*). Zero is the assertion: projection-before-LIMIT row counts
// are deliberately not used as a query-count or reader-bound oracle.
async fn observe_reads(pool: &PgPool) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        "CREATE SEQUENCE game_mix_read_probe;
         ALTER TABLE game_activity_guild_aggregates RENAME TO game_mix_saved_rows;
         CREATE FUNCTION game_mix_read_seen() RETURNS boolean LANGUAGE plpgsql VOLATILE AS $$
         BEGIN PERFORM nextval('game_mix_read_probe'); RETURN true; END $$;
         CREATE VIEW game_activity_guild_aggregates WITH (security_barrier=true) AS
         SELECT * FROM game_mix_saved_rows WHERE game_mix_read_seen();",
    ))
    .execute(pool)
    .await
    .expect("install disposable read observer");
}

async fn was_read(pool: &PgPool) -> bool {
    sqlx::query_scalar("SELECT is_called FROM game_mix_read_probe")
        .fetch_one(pool)
        .await
        .expect("read nontransactional observer")
}

async fn readonly_pool(db: &TestDatabase) -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET default_transaction_read_only = on")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(db.pool().connect_options().as_ref().clone())
        .await
        .expect("read-only pool in the guarded owned test database")
}

#[test]
fn selectors_publish_bounded_optional_choices_without_a_guild_or_member_override() {
    let definitions = router_with_commands(gates(false, false))
        .publish_set(&[])
        .expect("publish set");
    let wire = two_bot_discord::publish_commands(&definitions);
    let command = wire
        .iter()
        .find(|command| command.name == COMMAND)
        .expect("game-mix published with feature gates off");
    let value = serde_json::to_value(command).expect("published wire");
    assert_eq!(value["default_member_permissions"], "32");
    let options = value["options"]
        .as_array()
        .expect("optional browse selectors published");
    let selectors: BTreeSet<_> = options
        .iter()
        .map(|o| o["name"].as_str().expect("option name"))
        .collect();
    assert_eq!(
        selectors,
        BTreeSet::from(["format", "page", "sort"]),
        "no production, member or guild opt-in"
    );
    for option in options {
        assert_ne!(
            option["required"], true,
            "legacy optionless call remains valid"
        );
    }
    let page = options.iter().find(|o| o["name"] == "page").expect("page");
    assert_eq!(page["type"], 4, "integer page");
    assert_eq!(page["min_value"], 1);
    assert!(
        page["max_value"]
            .as_f64()
            .is_some_and(|n| n.is_finite() && n >= 2.0 && n <= i32::MAX as f64),
        "finite published page bound"
    );
    for (name, expected) in [
        ("sort", vec!["play", "voice"]),
        ("format", vec!["json", "summary"]),
    ] {
        let selector = options
            .iter()
            .find(|o| o["name"] == name)
            .expect("selector");
        assert_eq!(selector["type"], 3);
        let choices: BTreeSet<_> = selector["choices"]
            .as_array()
            .expect("bounded choices")
            .iter()
            .map(|choice| choice["value"].as_str().expect("string choice"))
            .collect();
        assert_eq!(choices, expected.into_iter().collect());
    }
}

#[tokio::test]
async fn paging_reaches_all_thirteen_rows_once_in_both_orders_without_writes() {
    let db = database().await;
    for index in (0..13).rev() {
        seed(
            db.pool(),
            GUILD_S,
            &format!("Game{index:02}"),
            100_000 + index * 1000,
            13_000 - index * 1000,
            index + 1,
        )
        .await;
    }
    seed(db.pool(), FOREIGN, "ForeignOnly", i64::MAX, i64::MAX, 1).await;
    let before = state(db.pool()).await;
    let pool = readonly_pool(&db).await;
    let mut replies = Vec::new();
    for sort in ["play", "voice"] {
        for page in [1, 2] {
            replies.push(dispatch(pool.clone(), staff(selected(page, sort, "json")), GUILD).await);
        }
    }
    pool.close().await;
    let after = state(db.pool()).await;
    db.close().await.expect("drop owned fixture");
    assert_eq!(
        before, after,
        "aggregate totals/timestamps and audit log untouched"
    );
    for (sort_index, sort) in ["play", "voice"].iter().enumerate() {
        let mut seen = Vec::new();
        for page in [1, 2] {
            let text = content(&replies[sort_index * 2 + page - 1]);
            selection_notice(&text, page, sort);
            let value = report(&text);
            assert_eq!(
                value["rows"].as_array().expect("rows").len(),
                if page == 1 { 10 } else { 3 },
                "do not drop whole rows to fit"
            );
            seen.extend(names(&value));
        }
        let expected: Vec<_> = if *sort == "play" {
            (0..13).rev().collect()
        } else {
            (0..13).collect()
        };
        assert_eq!(
            seen,
            expected
                .iter()
                .map(|index| format!("Game{index:02}"))
                .collect::<Vec<_>>(),
            "every available row, deterministic order"
        );
        assert_eq!(
            seen.iter().collect::<BTreeSet<_>>().len(),
            13,
            "no duplicate page boundary"
        );
    }
}

#[tokio::test]
async fn full_ties_use_stable_game_identity_across_a_page_boundary_and_repeat() {
    let db = database().await;
    for index in (0..12).rev() {
        seed(
            db.pool(),
            GUILD_S,
            &format!("Tie{index:02}"),
            60_000,
            30_000,
            1,
        )
        .await;
    }
    let mut replies = Vec::new();
    for sort in ["play", "voice"] {
        for page in [1, 2, 1] {
            replies.push(
                dispatch(
                    db.pool().clone(),
                    staff(selected(page, sort, "json")),
                    GUILD,
                )
                .await,
            );
        }
    }
    db.close().await.expect("drop owned fixture");
    for group in replies.chunks(3) {
        let first = names(&report(&content(&group[0])));
        let last = names(&report(&content(&group[1])));
        assert_eq!(
            first,
            names(&report(&content(&group[2]))),
            "unchanged snapshot repeats deterministically"
        );
        let all: Vec<_> = first.into_iter().chain(last).collect();
        assert_eq!(all.len(), 12);
        assert_eq!(
            all.iter().collect::<BTreeSet<_>>().len(),
            12,
            "tied rows neither repeat nor vanish"
        );
        let mut sorted = all.clone();
        sorted.sort();
        let mut reverse = sorted.clone();
        reverse.reverse();
        assert!(
            all == sorted || all == reverse,
            "stable ascending or descending game identity, not insertion order"
        );
    }
}

#[tokio::test]
async fn optionless_json_and_cli_golden_stay_unchanged() {
    let db = database().await;
    seed(db.pool(), GUILD_S, "Solo", 3_723_000, 62_000, 7).await;
    seed(db.pool(), FOREIGN, "ForeignOnly", i64::MAX, i64::MAX, 1).await;
    let requests = dispatch(db.pool().clone(), staff(Vec::new()), GUILD).await;
    let aggregates = two_bot_core::game_activity_store::read_guild_aggregates(db.pool(), GUILD_S)
        .await
        .expect("existing real aggregate reader");
    let cli = two_bot_core::game_activity::render_game_activity_report(GUILD_S, &aggregates);
    db.close().await.expect("drop owned fixture");
    let golden = json!({"guild_id":GUILD_S,"rows":[{"game":"Solo","total_play_ms":3_723_000,"in_voice_ms":62_000,"session_count":7}],"truncated":false});
    assert_eq!(
        report(&content(&requests)),
        golden,
        "optionless JSON keys and numbers"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&cli).expect("CLI JSON"),
        golden,
        "CLI independent golden"
    );
}

#[tokio::test]
async fn selectors_work_individually_with_legacy_defaults_for_omitted_selectors() {
    let db = database().await;
    for index in 0..13 {
        seed(
            db.pool(),
            GUILD_S,
            &format!("Game{index:02}"),
            100_000 + index * 1000,
            13_000 - index * 1000,
            1,
        )
        .await;
    }
    let page = dispatch(
        db.pool().clone(),
        staff(vec![option("page", CommandOptionValue::Integer(2))]),
        GUILD,
    )
    .await;
    let voice = dispatch(
        db.pool().clone(),
        staff(vec![option(
            "sort",
            CommandOptionValue::String("voice".to_owned()),
        )]),
        GUILD,
    )
    .await;
    let summary = dispatch(
        db.pool().clone(),
        staff(vec![option(
            "format",
            CommandOptionValue::String("summary".to_owned()),
        )]),
        GUILD,
    )
    .await;
    db.close().await.expect("drop owned fixture");
    let page_text = content(&page);
    assert_eq!(names(&report(&page_text)), ["Game02", "Game01", "Game00"]);
    selection_notice(&page_text, 2, "play");
    let voice_text = content(&voice);
    assert_eq!(
        names(&report(&voice_text)),
        (0..10).map(|i| format!("Game{i:02}")).collect::<Vec<_>>()
    );
    selection_notice(&voice_text, 1, "voice");
    let summary_text = content(&summary);
    assert!(
        !summary_text.contains("```json"),
        "explicit summary selection"
    );
    selection_notice(&summary_text, 1, "play");
}

#[tokio::test]
async fn zero_one_and_ten_rows_have_honest_bounds_and_out_of_range_is_not_empty() {
    let db = database().await;
    let mut replies = Vec::new();
    for count in [0, 1, 10] {
        if count == 1 {
            seed(db.pool(), GUILD_S, "Small00", 60_000, 0, 1).await;
        }
        if count == 10 {
            for index in 1..10 {
                seed(
                    db.pool(),
                    GUILD_S,
                    &format!("Small{index:02}"),
                    60_000,
                    0,
                    1,
                )
                .await;
            }
        }
        replies.push((
            count,
            dispatch(db.pool().clone(), staff(selected(1, "play", "json")), GUILD).await,
            dispatch(db.pool().clone(), staff(selected(2, "play", "json")), GUILD).await,
        ));
    }
    db.close().await.expect("drop owned fixture");
    for (count, first, outside) in replies {
        let text = content(&first);
        let value = report(&text);
        assert_eq!(value["rows"].as_array().expect("rows").len(), count);
        selection_notice(&text, 1, "play");
        if count == 0 {
            assert!(
                text.to_lowercase().contains("no game activity"),
                "honest empty explanation"
            );
        }
        correction(&content(&outside), "page");
    }
}

fn duration_present(text: &str, h: u64, m: u64, s: u64) -> bool {
    let compact: String = text
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',')
        .collect();
    compact.contains(&format!("{h}h{m}m{s}s"))
        || compact.contains(&format!("{h}:{m:02}:{s:02}"))
        || compact.contains(&format!("{h:02}:{m:02}:{s:02}"))
        || (h == 0 && compact.contains(&format!("{m}m{s}s")))
        || compact.contains(&format!(
            "{h}hour{}{}minute{}{}second",
            if h == 1 { "" } else { "s" },
            m,
            if m == 1 { "" } else { "s" },
            s
        ))
}

#[tokio::test]
async fn explicit_summary_distinguishes_accumulated_play_and_overlapping_voice_with_units() {
    let db = database().await;
    seed(db.pool(), GUILD_S, "Solo", 3_723_000, 62_000, 7).await;
    let before = state(db.pool()).await;
    let pool = readonly_pool(&db).await;
    let replies = dispatch(pool.clone(), staff(selected(1, "voice", "summary")), GUILD).await;
    pool.close().await;
    let after = state(db.pool()).await;
    db.close().await.expect("drop owned fixture");
    assert_eq!(before, after);
    let text = content(&replies);
    assert!(
        !text.contains("```json"),
        "readable summary is explicitly selected, not raw JSON"
    );
    assert!(text.contains("Solo"));
    assert!(
        duration_present(&text, 1, 2, 3),
        "3723000ms is 1h 2m 3s: {text}"
    );
    assert!(
        duration_present(&text, 0, 1, 2),
        "62000ms is 0h 1m 2s: {text}"
    );
    selection_notice(&text, 1, "voice");
    let lower = text.to_lowercase();
    assert!(
        lower.contains("play") && lower.contains("voice") && lower.contains("overlap"),
        "distinct accumulated play and overlapping voice, not unique elapsed time: {text}"
    );
    assert!(
        lower.contains("accumulat") || lower.contains("sum"),
        "state accumulated nature: {text}"
    );
    assert!(
        !lower.contains("last 7 days") && !lower.contains("last 30 days"),
        "no invented reporting window"
    );
}

#[tokio::test]
async fn summary_pages_preserve_all_rows_and_large_accumulated_hours_do_not_wrap() {
    let db = database().await;
    for index in 0..13 {
        seed(
            db.pool(),
            GUILD_S,
            &format!("Long{index:02}"),
            i64::MAX - index * 1000,
            9_007_199_254_740_993 - index * 1000,
            1,
        )
        .await;
    }
    let pool = readonly_pool(&db).await;
    let mut replies = Vec::new();
    for page in [1, 2, 3] {
        replies.push(
            dispatch(
                pool.clone(),
                staff(selected(page, "play", "summary")),
                GUILD,
            )
            .await,
        );
    }
    pool.close().await;
    db.close().await.expect("drop owned fixture");
    for (page, reply) in replies[..2].iter().enumerate() {
        let text = content(reply);
        assert!(!text.contains("```json"));
        selection_notice(&text, page + 1, "play");
        for index in 0..13 {
            let expected = if (page == 0 && index < 10) || (page == 1 && index >= 10) {
                1
            } else {
                0
            };
            assert_eq!(
                text.matches(&format!("Long{index:02}")).count(),
                expected,
                "summary pages preserve complete row identities"
            );
        }
    }
    let first = content(&replies[0]);
    assert!(
        duration_present(&first, 2_562_047_788_015, 12, 55),
        "BIGINT play expressed as accumulated hours, no 24-hour wrap: {first}"
    );
    assert!(
        duration_present(&first, 2_501_999_792, 59, 0),
        "large voice overlap duration: {first}"
    );
    correction(&content(&replies[2]), "page");
}

#[tokio::test]
async fn hostile_large_rows_keep_every_numeric_row_and_complete_json_under_utf16_budget() {
    let db = database().await;
    for index in 0..13 {
        let name = format!(
            "G{index:02}{}",
            "😀\\\"\n\t``` @everyone <@887766554433221101> ".repeat(60)
        );
        seed(
            db.pool(),
            GUILD_S,
            &name,
            i64::MAX - index,
            9_007_199_254_740_993 - index,
            9_007_199_254_740_995 - index,
        )
        .await;
    }
    let mut replies = Vec::new();
    for page in [1, 2] {
        replies.push(
            dispatch(
                db.pool().clone(),
                staff(selected(page, "play", "json")),
                GUILD,
            )
            .await,
        );
    }
    db.close().await.expect("drop owned fixture");
    let mut seen = Vec::new();
    for (page, reply) in replies.iter().enumerate() {
        let text = content(reply);
        let value = report(&text);
        let rows = value["rows"].as_array().expect("rows");
        assert_eq!(
            rows.len(),
            if page == 0 { 10 } else { 3 },
            "shorten labels, never silently discard rows"
        );
        for row in rows {
            let play = row["total_play_ms"].as_i64().expect("exact BIGINT play");
            let index = i64::MAX - play;
            assert_eq!(row["in_voice_ms"], 9_007_199_254_740_993_i64 - index);
            assert_eq!(row["session_count"], 9_007_199_254_740_995_i64 - index);
            let label = row["game"].as_str().expect("readable shortened label");
            assert!(
                !label.is_empty() && !label.contains(['\r', '\n', '\t']),
                "safe single-line label"
            );
            seen.push(index);
        }
        let lower = text.to_lowercase();
        assert!(
            lower.contains("shorten") || lower.contains("abbreviat"),
            "explain label shortening, not lost games: {text}"
        );
    }
    assert_eq!(
        seen,
        (0..13).collect::<Vec<_>>(),
        "all large counters reachable without JSON clipping or float rounding"
    );
}

#[tokio::test]
async fn malformed_selectors_are_corrected_before_any_aggregate_read() {
    let db = database().await;
    seed(db.pool(), GUILD_S, "ReadWitness", 60_000, 0, 1).await;
    observe_reads(db.pool()).await;
    let cases = vec![
        ("page", vec![option("page", CommandOptionValue::Integer(0))]),
        (
            "page",
            vec![option("page", CommandOptionValue::Integer(-1))],
        ),
        (
            "page",
            vec![option("page", CommandOptionValue::Integer(i64::MAX))],
        ),
        (
            "page",
            vec![option("page", CommandOptionValue::String("2".to_owned()))],
        ),
        (
            "page",
            vec![
                option("page", CommandOptionValue::Integer(1)),
                option("page", CommandOptionValue::Integer(1)),
            ],
        ),
        (
            "sort",
            vec![option(
                "sort",
                CommandOptionValue::String("PLAY".to_owned()),
            )],
        ),
        ("sort", vec![option("sort", CommandOptionValue::Integer(1))]),
        (
            "sort",
            vec![
                option("sort", CommandOptionValue::String("play".to_owned())),
                option("sort", CommandOptionValue::String("voice".to_owned())),
            ],
        ),
        (
            "format",
            vec![option(
                "format",
                CommandOptionValue::String("csv".to_owned()),
            )],
        ),
        (
            "format",
            vec![option("format", CommandOptionValue::Boolean(true))],
        ),
        (
            "format",
            vec![
                option("format", CommandOptionValue::String("json".to_owned())),
                option("format", CommandOptionValue::String("summary".to_owned())),
            ],
        ),
        (
            "game-mix",
            vec![option(
                "guild",
                CommandOptionValue::String(FOREIGN.to_owned()),
            )],
        ),
        (
            "game-mix",
            vec![option(
                "allow-live-guild",
                CommandOptionValue::Boolean(true),
            )],
        ),
        (
            "game-mix",
            vec![option("member", CommandOptionValue::User(Id::new(ACTOR)))],
        ),
        (
            "game-mix",
            vec![option("page", CommandOptionValue::SubCommand(Vec::new()))],
        ),
    ];
    let mut results = Vec::new();
    for (selector, options) in cases {
        sqlx::query("SELECT setval('game_mix_read_probe', 1, false)")
            .execute(db.pool())
            .await
            .expect("reset owned read observer");
        let requests = dispatch(db.pool().clone(), staff(options), GUILD).await;
        results.push((selector, requests, was_read(db.pool()).await));
    }
    // Positive control proves the observer is attached to the real reader.
    let control = dispatch(db.pool().clone(), staff(Vec::new()), GUILD).await;
    let control_read = was_read(db.pool()).await;
    db.close().await.expect("drop owned fixture");
    assert!(control_read, "positive aggregate-read observer control");
    assert_eq!(names(&report(&content(&control))), ["ReadWitness"]);
    for (selector, requests, read) in results {
        assert!(!read, "invalid {selector} crossed the pre-read boundary");
        correction(&content(&requests), selector);
    }
}

#[tokio::test]
async fn selectors_never_bypass_staff_guild_or_live_refusal_before_reads() {
    let db = database().await;
    seed(db.pool(), GUILD_S, "ReadWitness", 60_000, 0, 1).await;
    let live: u64 = two_bot_core::backup::guild_config::LIVE_GUILD_ID
        .parse()
        .expect("live guild constant");
    seed(
        db.pool(),
        &live.to_string(),
        "LiveReadWitness",
        60_000,
        0,
        1,
    )
    .await;
    observe_reads(db.pool()).await;
    let mut results = Vec::new();
    for format in ["json", "summary"] {
        let options = selected(1, "voice", format);
        let mut no_staff = staff(options.clone());
        no_staff
            .member
            .as_mut()
            .expect("fixture member")
            .permissions = Some(Permissions::empty());
        results.push(dispatch(db.pool().clone(), no_staff, GUILD).await);
        let mut missing_authority = staff(options.clone());
        missing_authority
            .member
            .as_mut()
            .expect("fixture member")
            .permissions = None;
        results.push(dispatch(db.pool().clone(), missing_authority, GUILD).await);
        let mut live_call = staff(options.clone());
        set_guild(&mut live_call, live);
        results.push(dispatch(db.pool().clone(), live_call, live).await);
        let mut foreign = staff(options.clone());
        set_guild(&mut foreign, 9999);
        assert!(
            dispatch(db.pool().clone(), foreign, GUILD).await.is_empty(),
            "foreign guild ignored before REST"
        );
        let mut dm = staff(options);
        dm.guild_id = None;
        dm.member = None;
        assert!(
            dispatch(db.pool().clone(), dm, GUILD).await.is_empty(),
            "DM ignored before REST"
        );
    }
    let read = was_read(db.pool()).await;
    db.close().await.expect("drop owned fixture");
    assert!(!read, "authority/live-guild refusals precede storage");
    for result in results {
        let text = content(&result);
        assert!(
            !text.contains("```json") && !text.contains("ReadWitness"),
            "refusal never serves data"
        );
    }
}

#[tokio::test]
async fn pages_do_not_silently_inherit_the_operator_cli_hundred_row_render_cap() {
    let db = database().await;
    for index in 0..105 {
        seed(
            db.pool(),
            GUILD_S,
            &format!("All{index:03}"),
            200_000 - index * 1000,
            0,
            1,
        )
        .await;
    }
    let mut replies = Vec::new();
    for page in 1..=11 {
        replies.push(
            dispatch(
                db.pool().clone(),
                staff(selected(page, "play", "json")),
                GUILD,
            )
            .await,
        );
    }
    db.close().await.expect("drop owned fixture");
    let mut all = Vec::new();
    for reply in replies {
        all.extend(names(&report(&content(&reply))));
    }
    assert_eq!(
        all,
        (0..105)
            .map(|index| format!("All{index:03}"))
            .collect::<Vec<_>>(),
        "all available aggregates, not just the CLI's first hundred"
    );
}

#[tokio::test]
async fn reporting_never_reads_member_bearing_tables_even_if_they_are_empty() {
    let db = database().await;
    seed(db.pool(), GUILD_S, "AggregateOnly", 60_000, 0, 1).await;
    sqlx::query("CREATE SEQUENCE game_mix_member_read_probe")
        .execute(db.pool())
        .await
        .expect("owned observer");
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT c.table_name FROM information_schema.columns c
         JOIN pg_tables t ON t.schemaname=c.table_schema AND t.tablename=c.table_name
         WHERE c.table_schema='public' AND c.column_name IN ('member_id','user_id','player_id')
         ORDER BY c.table_name",
    )
    .fetch_all(db.pool())
    .await
    .expect("catalog, never member data");
    assert!(!tables.is_empty(), "real member-store surfaces were found");
    for (index, table) in tables.iter().enumerate() {
        let quoted = table.replace('"', "\"\"");
        // A volatile set-returning function runs even for an empty backing
        // store. A projected expression over an empty table would miss reads.
        let sql = format!(
            "ALTER TABLE \"{quoted}\" RENAME TO mix_member_saved_{index};
             CREATE FUNCTION mix_member_probe_{index}() RETURNS SETOF mix_member_saved_{index}
             LANGUAGE plpgsql VOLATILE AS $$ BEGIN
             PERFORM nextval('game_mix_member_read_probe'); RETURN; END $$;
             CREATE VIEW \"{quoted}\" AS SELECT * FROM mix_member_probe_{index}();"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(db.pool())
            .await
            .expect("replace owned member store with read spy");
    }
    let mut replies = Vec::new();
    for format in ["json", "summary"] {
        replies.push(dispatch(db.pool().clone(), staff(selected(1, "play", format)), GUILD).await);
    }
    let read: bool = sqlx::query_scalar("SELECT is_called FROM game_mix_member_read_probe")
        .fetch_one(db.pool())
        .await
        .expect("member-read witness");
    // Independently prove an empty-table COUNT(*) activates this observer.
    let quoted = tables[0].replace('"', "\"\"");
    let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM \"{quoted}\""
    )))
    .fetch_one(db.pool())
    .await
    .expect("positive observer control");
    let control: bool = sqlx::query_scalar("SELECT is_called FROM game_mix_member_read_probe")
        .fetch_one(db.pool())
        .await
        .expect("positive observer witness");
    db.close().await.expect("drop owned fixture");
    assert!(!read, "guild report must not inspect per-member stores");
    assert!(
        control && count == 0,
        "spy detects reads even when zero rows are returned"
    );
    for reply in replies {
        assert!(
            content(&reply).contains("AggregateOnly"),
            "aggregate report remains available without member reads"
        );
    }
}

#[tokio::test]
async fn every_reply_and_refusal_path_attempts_zero_table_writes_and_no_unrelated_rest() {
    let db = database().await;
    seed(db.pool(), GUILD_S, "NoEffects", 60_000, 0, 1).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        "CREATE SEQUENCE game_mix_write_probe;
         CREATE FUNCTION game_mix_refuse_write() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM nextval('game_mix_write_probe'); RAISE EXCEPTION 'fixture refuses report writes'; END $$;
         DO $$ DECLARE item record; BEGIN
         FOR item IN SELECT tablename FROM pg_tables WHERE schemaname='public' LOOP
         EXECUTE format('CREATE TRIGGER game_mix_write_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION game_mix_refuse_write()', item.tablename);
         END LOOP; END $$;"
    )).execute(db.pool()).await.expect("write-attempt guards on every table in the owned database");
    let mut replies = Vec::new();
    for format in ["json", "summary"] {
        replies.push(dispatch(db.pool().clone(), staff(selected(1, "play", format)), GUILD).await);
        replies.push(dispatch(db.pool().clone(), staff(selected(2, "play", format)), GUILD).await);
        replies.push(dispatch(db.pool().clone(), staff(selected(0, "play", format)), GUILD).await);
        let mut denied = staff(selected(1, "play", format));
        denied.member.as_mut().expect("member").permissions = Some(Permissions::empty());
        replies.push(dispatch(db.pool().clone(), denied, GUILD).await);
        let live: u64 = two_bot_core::backup::guild_config::LIVE_GUILD_ID
            .parse()
            .expect("live guild");
        let mut live_call = staff(selected(1, "play", format));
        set_guild(&mut live_call, live);
        replies.push(dispatch(db.pool().clone(), live_call, live).await);
    }
    sqlx::query("ALTER TABLE game_activity_guild_aggregates RENAME TO game_mix_outage_rows")
        .execute(db.pool())
        .await
        .expect("owned store outage, no migration");
    replies.push(dispatch(db.pool().clone(), staff(selected(1, "play", "json")), GUILD).await);
    let writes: bool = sqlx::query_scalar("SELECT is_called FROM game_mix_write_probe")
        .fetch_one(db.pool())
        .await
        .expect("write-attempt witness survives rollback");
    // Independent positive control: even a zero-row, rolled-back statement
    // must trip the nontransactional observer. Never infer from row counts.
    let refused =
        sqlx::query("UPDATE game_mix_outage_rows SET total_play_ms=total_play_ms WHERE false")
            .execute(db.pool())
            .await
            .is_err();
    let control: bool = sqlx::query_scalar("SELECT is_called FROM game_mix_write_probe")
        .fetch_one(db.pool())
        .await
        .expect("positive write witness");
    db.close().await.expect("drop owned fixture");
    assert!(
        !writes,
        "reports/refusals never attempt a table mutation or audit write"
    );
    assert!(
        refused && control,
        "write spy detects a rolled-back zero-row write"
    );
    for reply in replies {
        let _ = content(&reply);
    }
}

#[tokio::test]
async fn storage_failure_is_unavailable_not_a_successful_empty_page() {
    let db = database().await;
    sqlx::query("ALTER TABLE game_activity_guild_aggregates RENAME TO unavailable_game_mix_rows")
        .execute(db.pool())
        .await
        .expect("disposable missing-store fixture");
    let mut replies = Vec::new();
    for format in ["json", "summary"] {
        replies.push(dispatch(db.pool().clone(), staff(selected(1, "play", format)), GUILD).await);
    }
    db.close().await.expect("drop owned fixture");
    for reply in replies {
        let text = content(&reply);
        let lower = text.to_lowercase();
        assert!(!text.contains("```json") && !lower.contains("no game activity"));
        assert!(
            lower.contains("try again") || lower.contains("unavailable"),
            "honest unavailable correction: {text}"
        );
        assert!(
            !lower.contains("sql") && !lower.contains("relation"),
            "no raw storage diagnostics"
        );
    }
}
