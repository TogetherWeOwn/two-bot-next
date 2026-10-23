#![cfg(feature = "db")]
//! Protected RED handoff: seasonal read navigation and configured reward preview.
//! Only the existing routed `handle_interaction_at` seam is called; there is
//! no test-only renderer, paging implementation, or production-source pin.
//!
//! Contract choices: page size stays ten, page is one-based and finitely capped
//! in registration, `mine:true` opens the invoker's seasonal page, page+mine
//! is invalid. Omitted options preserve the existing exact default response.
//! Explicit pages label page/season and keep absolute one-based positions.
//! Reward preview means the first configured threshold strictly ABOVE lifetime
//! level; an already-held Discord role cannot change that threshold.
//!
//! SQLx query events observe the actual statements and returned row counts:
//! one bounded page SELECT, never a whole-guild load sliced in Rust. Scalar
//! position/count reads are allowed. Scope/ordering fixtures exercise the real
//! migrated store; previous season, foreign guild and absent/erased rows must
//! not be substituted. This slice adds no new membership/XP eligibility policy.
//!
//! Run with the guarded disposable bootstrap only (no ignored tests):
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
//! python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
//!   --test leveling_read_ux_acceptance -- --test-threads=1

#[path = "support/rest_capture.rs"]
mod rest_capture;

use rest_capture::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::{
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::{
    field::{Field, Visit},
    instrument::WithSubscriber,
    Event, Subscriber,
};
use tracing_subscriber::{
    layer::{Context, SubscriberExt},
    Layer,
};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{commands::core_commands, HandlerId, OnboardingGates, OnboardingMode};
use two_bot_discord::{leveling_runtime::LevelingRuntimeError, ActionExecutor, LevelingRuntime};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 1545644954272137297;
const FIRST: u64 = 100000000000000001;
const ABSENT: u64 = FIRST + 100;
const ROLE_TWO: &str = "200000000000000002";
const ROLE_SEVEN: &str = "200000000000000007";
const AT: &str = "2026-10-23T12:00:00.000Z";
const SEASON: &str = "2026-Q4";

#[derive(Clone, Default)]
struct QueryCapture(Arc<Mutex<Vec<QueryEvent>>>);

#[derive(Default, Debug)]
struct QueryEvent {
    statement: String,
    summary: String,
    returned: u64,
}

impl Visit for QueryEvent {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "db.statement" => self.statement = value.into(),
            "summary" => self.summary = value.into(),
            _ => {}
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "rows_returned" {
            self.returned = value;
        }
    }
    fn record_debug(&mut self, _field: &Field, _value: &dyn fmt::Debug) {}
}

impl<S: Subscriber> Layer<S> for QueryCapture {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            let mut row = QueryEvent::default();
            event.record(&mut row);
            if row.statement.is_empty() {
                row.statement = row.summary.clone();
            }
            self.0.lock().unwrap().push(row);
        }
    }
}

struct Exchange {
    result: Result<bool, LevelingRuntimeError>,
    bodies: Vec<Value>,
    queries: Vec<QueryEvent>,
}

impl Exchange {
    fn content(&self, ephemeral: bool) -> &str {
        assert!(
            matches!(self.result, Ok(true)),
            "routed result: {:?}",
            self.result
        );
        assert_eq!(
            self.bodies.len(),
            2,
            "one callback plus one edit; no role read/grant or extra reply"
        );
        assert_eq!(self.bodies[0]["type"], 5, "acknowledge before store work");
        if ephemeral {
            assert_eq!(self.bodies[0]["data"]["flags"], 64);
        } else {
            assert_ne!(self.bodies[0]["data"]["flags"], 64);
        }
        let body = &self.bodies[1];
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
        assert!(body["allowed_mentions"]["users"]
            .as_array()
            .is_none_or(Vec::is_empty));
        assert!(body["allowed_mentions"]["roles"]
            .as_array()
            .is_none_or(Vec::is_empty));
        let content = body["content"].as_str().expect("text completion");
        assert!(
            content.encode_utf16().count() <= 2000,
            "bounded Discord reply"
        );
        content
    }

    fn bounded_page_read(&self, expected_rows: u64) {
        let board_reads: Vec<_> = self
            .queries
            .iter()
            .filter(|q| {
                let sql = q.statement.to_ascii_uppercase();
                sql.contains("SELECT") && sql.contains("MEMBER_SEASON_XP")
            })
            .collect();
        assert!(
            !board_reads.is_empty(),
            "SQL observer must see the real seasonal read"
        );
        let pages: Vec<_> = board_reads
            .iter()
            .filter(|q| q.statement.to_ascii_uppercase().contains("LIMIT"))
            .collect();
        assert_eq!(
            pages.len(),
            1,
            "exactly one SQL-bounded data page, not one query per member: {board_reads:?}"
        );
        assert_eq!(
            pages[0].returned, expected_rows,
            "only the requested page crosses the DB boundary"
        );
        for query in board_reads {
            let sql = query.statement.to_ascii_uppercase();
            assert!(
                query.returned <= 10,
                "never fetch the whole guild to slice in memory: {query:?}"
            );
            assert!(sql.contains("LIMIT") || sql.contains("COUNT("), "other reads must be scalar counts/positions, not unbounded member lists: {query:?}");
        }
    }
}

fn interaction(name: &str, invoker: u64, options: Value) -> Interaction {
    serde_json::from_value(json!({
        "id":"7","application_id":"8","type":2,"token":"mock-callback","version":1,
        "guild_id":GUILD.to_string(),"locale":"en-US","entitlements":[],
        "authorizing_integration_owners":{"0":GUILD.to_string()},
        "member":{"user":{"id":invoker.to_string(),"username":"reader","global_name":"Reader","discriminator":"0","avatar":null,"bot":false},"roles":[],"joined_at":null,"deaf":false,"mute":false,"pending":false,"flags":0,"permissions":"0"},
        "data":{"id":"1","name":name,"type":1,"options":options}
    })).expect("valid slash fixture")
}

fn page(number: i64) -> Value {
    json!([{"name":"page","type":4,"value":number}])
}

fn mine() -> Value {
    json!([{"name":"mine","type":5,"value":true}])
}

fn runtime(pool: PgPool, mock: &MockRest) -> LevelingRuntime {
    LevelingRuntime::new(
        pool,
        Arc::new(
            ActionExecutor::with_proxy("mock-only-token".into(), Some(mock.origin())).unwrap(),
        ),
        GUILD,
        OnboardingGates {
            mode: OnboardingMode::Legacy,
            dry_run: false,
        },
    )
}

async fn exchange(pool: PgPool, request: Interaction, handler: HandlerId) -> Exchange {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, r#"{"id":"99"}"#),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(pool, &mock);
    let capture = QueryCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let result = runtime
        .handle_interaction_at(&request, handler, AT)
        .with_subscriber(subscriber)
        .await;
    let bodies = mock
        .requests()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    mock.shutdown().await;
    let queries = std::mem::take(&mut *capture.0.lock().unwrap());
    Exchange {
        result,
        bodies,
        queries,
    }
}

async fn database() -> TestDatabase {
    let url =
        std::env::var("TWO_TEST_DATABASE_URL").expect("guarded disposable bootstrap required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated fixture, no credential fallback")
}

async fn seed_board(pool: &PgPool, count: u64) {
    // Reverse insertion, and a tie across positions 10/11: insertion order and
    // RANK() with shared positions must not replace the stored-ID tie break.
    for index in (0..count).rev() {
        let xp = if index == 10 {
            910
        } else {
            1000 - index as i64 * 10
        };
        sqlx::query("INSERT INTO member_season_xp (guild_id,season_id,member_id,seasonal_xp,updated_at) VALUES ($1,$2,$3,$4,NOW())")
            .bind(GUILD.to_string()).bind(SEASON).bind((FIRST+index).to_string()).bind(xp)
            .execute(pool).await.unwrap();
    }
    // Huge lifetime/historical/foreign totals must never enter any page/count.
    seed_lifetime(pool, ABSENT, 99_000).await;
    for (guild, season) in [(GUILD, "2026-Q3"), (GUILD + 1, SEASON)] {
        sqlx::query("INSERT INTO member_season_xp (guild_id,season_id,member_id,seasonal_xp,updated_at) VALUES ($1,$2,$3,99000,NOW())")
            .bind(guild.to_string()).bind(season).bind(ABSENT.to_string()).execute(pool).await.unwrap();
    }
}

async fn seed_lifetime(pool: &PgPool, member: u64, xp: i64) {
    sqlx::query("INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ($1,$2,$3,$3,NOW())")
        .bind(GUILD.to_string()).bind(member.to_string()).bind(xp).execute(pool).await.unwrap();
}

async fn seed_rewards(pool: &PgPool) {
    // Deliberately sparse and inserted out of order; a foreign-guild reward
    // between them must not become the next threshold.
    for (guild, level, role) in [
        (GUILD, 7, ROLE_SEVEN),
        (GUILD, 2, ROLE_TWO),
        (GUILD + 1, 4, "200000000000000004"),
    ] {
        sqlx::query("INSERT INTO level_role_rewards (guild_id,level,role_id) VALUES ($1,$2,$3)")
            .bind(guild.to_string())
            .bind(level)
            .bind(role)
            .execute(pool)
            .await
            .unwrap();
    }
}

fn plain(text: &str) -> String {
    text.replace(['*', '`'], "").to_ascii_lowercase()
}

fn assert_page_label(content: &str, page: u64, total: u64) {
    let text = plain(content);
    assert!(
        content.contains(SEASON),
        "season label survives navigation: {content}"
    );
    assert!(
        text.contains(&format!("page {page} of {total}"))
            || text.contains(&format!("page {page}/{total}")),
        "concise truthful one-based page/total label: {content}"
    );
    assert!(
        text.contains("partial"),
        "do not erase the first-season completeness warning"
    );
}

fn board_rows(content: &str) -> Vec<(u64, u64)> {
    content
        .lines()
        .filter_map(|line| {
            let (position, rest) = line.strip_prefix("**")?.split_once(".** <@")?;
            let (member, _) = rest.split_once('>')?;
            Some((
                position.parse().expect("numeric position"),
                member.parse().expect("member ID"),
            ))
        })
        .collect()
}

fn assert_rows(content: &str, first_position: u64, count: u64) {
    let expected: Vec<_> = (first_position..first_position + count)
        .map(|position| (position, FIRST + position - 1))
        .collect();
    assert_eq!(
        board_rows(content),
        expected,
        "only this page, absolute positions, deterministic ties"
    );
    assert!(
        !content.contains(&ABSENT.to_string()),
        "no historical/foreign/lifetime fallback"
    );
}

#[tokio::test]
async fn control_omitted_options_preserve_exact_public_seasonal_top_ten_and_bounded_sql() {
    let db = database().await;
    seed_board(db.pool(), 25).await;
    let reply = exchange(
        db.pool().clone(),
        interaction("leaderboard", FIRST, json!([])),
        HandlerId::Leaderboard,
    )
    .await;
    db.close().await.unwrap();
    let content = reply.content(false);
    assert_rows(content, 1, 10);
    assert_eq!(
        content.lines().count(),
        11,
        "legacy default header plus ten rows"
    );
    assert_eq!(
        content.lines().next(),
        Some("**TWO XP Leaderboard (2026-Q4 — partial: early-season XP not counted)**")
    );
    assert!(content.contains("**10.** <@100000000000000010> · level **4** · 910 XP"));
    reply.bounded_page_read(10);
}

#[tokio::test]
async fn page_two_starts_at_eleven_after_a_cross_page_tie_and_fetches_only_ten() {
    let db = database().await;
    seed_board(db.pool(), 25).await;
    let reply = exchange(
        db.pool().clone(),
        interaction("leaderboard", FIRST, page(2)),
        HandlerId::Leaderboard,
    )
    .await;
    db.close().await.unwrap();
    let content = reply.content(false);
    assert_rows(content, 11, 10);
    assert_page_label(content, 2, 3);
    assert!(content.contains("**11.** <@100000000000000011> · level **4** · 910 XP"));
    reply.bounded_page_read(10);
}

#[tokio::test]
async fn final_page_has_only_positions_twenty_one_through_twenty_five() {
    let db = database().await;
    seed_board(db.pool(), 25).await;
    let reply = exchange(
        db.pool().clone(),
        interaction("leaderboard", FIRST, page(3)),
        HandlerId::Leaderboard,
    )
    .await;
    db.close().await.unwrap();
    let content = reply.content(false);
    assert_rows(content, 21, 5);
    assert_page_label(content, 3, 3);
    reply.bounded_page_read(5);
}

#[tokio::test]
async fn exact_multiple_and_singleton_boundaries_have_no_phantom_page() {
    let mut replies = Vec::new();
    for (count, requested, first_position, rows, total) in
        [(20, 2, 11, 10, 2), (21, 3, 21, 1, 3), (1, 1, 1, 1, 1)]
    {
        let db = database().await;
        seed_board(db.pool(), count).await;
        let reply = exchange(
            db.pool().clone(),
            interaction("leaderboard", FIRST, page(requested)),
            HandlerId::Leaderboard,
        )
        .await;
        db.close().await.unwrap();
        replies.push((reply, requested, first_position, rows, total));
    }
    for (reply, requested, first_position, rows, total) in replies {
        let content = reply.content(false);
        assert_rows(content, first_position, rows);
        assert_page_label(content, requested as u64, total);
        reply.bounded_page_read(rows);
    }
}

#[tokio::test]
async fn explicit_first_page_and_mine_false_do_not_skip_or_reorder_the_default_rows() {
    let db = database().await;
    seed_board(db.pool(), 25).await;
    let reply = exchange(
        db.pool().clone(),
        interaction(
            "leaderboard",
            FIRST,
            json!([{"name":"page","type":4,"value":1},{"name":"mine","type":5,"value":false}]),
        ),
        HandlerId::Leaderboard,
    )
    .await;
    db.close().await.unwrap();
    let content = reply.content(false);
    assert_rows(content, 1, 10);
    assert_page_label(content, 1, 3);
}

#[tokio::test]
async fn own_page_uses_seasonal_position_outside_top_ten_not_lifetime_or_held_roles() {
    let db = database().await;
    seed_board(db.pool(), 25).await;
    seed_lifetime(db.pool(), FIRST + 21, 500_000).await;
    let reply = exchange(
        db.pool().clone(),
        interaction("leaderboard", FIRST + 21, mine()),
        HandlerId::Leaderboard,
    )
    .await;
    db.close().await.unwrap();
    let content = reply.content(false);
    assert_rows(content, 21, 5);
    assert_page_label(content, 3, 3);
    let text = plain(content);
    assert!(
        text.contains("your position: 22") || text.contains("your position: #22"),
        "clear own position, not just a row: {content}"
    );
    reply.bounded_page_read(5);
}

#[tokio::test]
async fn tied_member_own_page_uses_unique_eleventh_position_not_shared_tenth_rank() {
    let db = database().await;
    seed_board(db.pool(), 25).await;
    let reply = exchange(
        db.pool().clone(),
        interaction("leaderboard", FIRST + 10, mine()),
        HandlerId::Leaderboard,
    )
    .await;
    db.close().await.unwrap();
    let content = reply.content(false);
    assert_rows(content, 11, 10);
    assert_page_label(content, 2, 3);
    let text = plain(content);
    assert!(
        text.contains("your position: 11") || text.contains("your position: #11"),
        "tie breaks to unique position eleven: {content}"
    );
}

#[tokio::test]
async fn own_page_without_season_row_is_unranked_never_fabricated_from_lifetime() {
    let db = database().await;
    seed_board(db.pool(), 25).await;
    // The privacy service deletes XP projections on opt-out. The suppression
    // remains; navigation must not recreate a row or infer a rank from history.
    sqlx::query("INSERT INTO privacy_opt_outs (guild_id,member_id) VALUES ($1,$2)")
        .bind(GUILD.to_string())
        .bind(ABSENT.to_string())
        .execute(db.pool())
        .await
        .unwrap();
    let reply = exchange(
        db.pool().clone(),
        interaction("leaderboard", ABSENT, mine()),
        HandlerId::Leaderboard,
    )
    .await;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM member_season_xp WHERE guild_id=$1 AND season_id=$2 AND member_id=$3",
    )
    .bind(GUILD.to_string())
    .bind(SEASON)
    .bind(ABSENT.to_string())
    .fetch_one(db.pool())
    .await
    .unwrap();
    db.close().await.unwrap();
    let content = reply.content(false);
    assert!(
        plain(content).contains("unranked"),
        "truthful absent position: {content}"
    );
    assert!(
        plain(content).contains("page") && content.contains('1'),
        "useful route back to page one: {content}"
    );
    assert!(
        board_rows(content).is_empty(),
        "do not silently show an unrelated own page"
    );
    assert_eq!(count, 0, "read-only privacy boundary");
}

#[tokio::test]
async fn empty_season_and_out_of_range_page_are_distinct_and_offer_a_correction() {
    let db = database().await;
    seed_board(db.pool(), 25).await;
    let overflow = exchange(
        db.pool().clone(),
        interaction("leaderboard", FIRST, page(4)),
        HandlerId::Leaderboard,
    )
    .await;
    sqlx::query("DELETE FROM member_season_xp WHERE guild_id=$1 AND season_id=$2")
        .bind(GUILD.to_string())
        .bind(SEASON)
        .execute(db.pool())
        .await
        .unwrap();
    let empty = exchange(
        db.pool().clone(),
        interaction("leaderboard", FIRST, page(1)),
        HandlerId::Leaderboard,
    )
    .await;
    db.close().await.unwrap();
    let out = overflow.content(false);
    assert!(
        board_rows(out).is_empty(),
        "no top-ten fallback for missing page"
    );
    let text = plain(out);
    assert!(
        text.contains("page") && text.contains('3'),
        "last valid page correction: {out}"
    );
    assert!(
        !text.contains("no xp has been earned"),
        "a populated season is not empty"
    );
    let content = empty.content(false);
    assert!(content.contains(SEASON));
    assert!(board_rows(content).is_empty());
    assert!(
        plain(content).contains("no xp") || plain(content).contains("no seasonal xp"),
        "honest empty season: {content}"
    );
    assert!(!content.contains("page 0") && !content.contains("/0"));
}

#[tokio::test]
async fn invalid_pages_and_conflicting_or_wrong_typed_options_refuse_before_acquiring_a_database() {
    // Closed lazy pool: not even a disposable DB connection is opened here.
    // Any attempt to acquire fails, so successful corrective replies prove the
    // validation boundary independently of SQL logs or an expensive query.
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .unwrap();
    pool.close().await;
    let mut options = vec![
        page(0),
        page(-1),
        page(9_007_199_254_740_991),
        json!([{"name":"page","type":3,"value":"2"}]),
        json!([{"name":"mine","type":3,"value":"yes"}]),
        json!([{"name":"page","type":4,"value":2},{"name":"mine","type":5,"value":true}]),
        json!([{"name":"page","type":4,"value":1},{"name":"page","type":4,"value":2}]),
    ];
    // Also refuse one above whatever finite cap the registration advertises.
    if let Some(max) = core_commands()
        .iter()
        .find(|c| c.name == "leaderboard")
        .and_then(|c| c.options.iter().find(|o| o.name == "page"))
        .and_then(|o| o.max_value)
    {
        options.push(page(max.saturating_add(1)));
    }
    let mut failures = Vec::new();
    for options in options {
        let reply = exchange(
            pool.clone(),
            interaction("leaderboard", FIRST, options.clone()),
            HandlerId::Leaderboard,
        )
        .await;
        let handled = matches!(
            reply.result,
            Ok(true) | Err(LevelingRuntimeError::InvalidInteraction)
        );
        let text = reply
            .bodies
            .last()
            .and_then(|body| {
                body["content"]
                    .as_str()
                    .or_else(|| body["data"]["content"].as_str())
            })
            .map(plain)
            .unwrap_or_default();
        let one_response = match reply.bodies.as_slice() {
            [immediate] => immediate["type"] == 4,
            [defer, edit] => defer["type"] == 5 && edit["content"].is_string(),
            _ => false,
        };
        if !handled
            || !one_response
            || !(text.contains("page") || text.contains("mine"))
            || !(text.contains('1') || text.contains("true") || text.contains("choose"))
            || text.contains("unavailable")
            || !reply.queries.is_empty()
        {
            failures.push((options, format!("{:?}", reply.result), text, reply.queries));
        }
    }
    assert!(
        failures.is_empty(),
        "invalid inputs must get actionable correction without any DB acquire/read: {failures:?}"
    );
}

#[tokio::test]
async fn stored_zero_xp_has_a_position_but_absent_member_does_not() {
    let db = database().await;
    sqlx::query("INSERT INTO member_season_xp (guild_id,season_id,member_id,seasonal_xp,updated_at) VALUES ($1,$2,$3,0,NOW())")
        .bind(GUILD.to_string()).bind(SEASON).bind(FIRST.to_string()).execute(db.pool()).await.unwrap();
    let reply = exchange(
        db.pool().clone(),
        interaction("leaderboard", FIRST, mine()),
        HandlerId::Leaderboard,
    )
    .await;
    db.close().await.unwrap();
    let content = reply.content(false);
    assert_rows(content, 1, 1);
    assert_page_label(content, 1, 1);
    assert!(!plain(content).contains("unranked"));
}

#[tokio::test]
async fn rank_previews_sparse_next_threshold_from_store_preserving_existing_xp_progress() {
    let db = database().await;
    seed_lifetime(db.pool(), FIRST, 500).await; // level 3: floor 475, next 770
    seed_rewards(db.pool()).await;
    let mut request = interaction("rank", FIRST, json!([]));
    request.member.as_mut().unwrap().roles =
        vec![twilight_model::id::Id::new(ROLE_SEVEN.parse().unwrap())];
    let reply = exchange(db.pool().clone(), request, HandlerId::Rank).await;
    db.close().await.unwrap();
    let content = reply.content(true);
    assert!(content.contains("Level **3**"));
    assert!(
        content.contains("XP **500** · 25/295 this level · **270** to level 4"),
        "preserve existing progress, do not replace it with reward progress"
    );
    let text = plain(content);
    assert!(
        text.contains("next")
            && text.contains("reward")
            && text.contains("level 7")
            && text.contains("4 levels"),
        "next configured threshold and remaining levels: {content}"
    );
    assert!(
        content.contains(ROLE_SEVEN),
        "role identity from configured store"
    );
    assert!(
        !content.contains(ROLE_TWO),
        "do not advertise a past reward"
    );
    assert!(
        !content.contains("200000000000000004"),
        "foreign-guild ladder is not considered"
    );
}

#[tokio::test]
async fn rank_at_or_above_final_reward_and_empty_ladder_are_explicit_not_zero_levels_remaining() {
    let mut replies = Vec::new();
    for (xp, rewards) in [(2205, true), (2900, true), (500, false)] {
        let db = database().await;
        seed_lifetime(db.pool(), FIRST, xp).await;
        if rewards {
            seed_rewards(db.pool()).await;
        }
        replies.push(
            exchange(
                db.pool().clone(),
                interaction("rank", FIRST, json!([])),
                HandlerId::Rank,
            )
            .await,
        );
        db.close().await.unwrap();
    }
    for reply in replies {
        let content = reply.content(true);
        let text = plain(content);
        assert!(
            text.contains("no further") && text.contains("reward"),
            "explicit end-of-ladder state: {content}"
        );
        assert!(
            !text.contains("0 levels remaining"),
            "never promise an already-reached reward"
        );
    }
}

#[tokio::test]
async fn rank_with_no_xp_previews_first_reward_without_creating_a_profile() {
    let db = database().await;
    seed_rewards(db.pool()).await;
    let reply = exchange(
        db.pool().clone(),
        interaction("rank", FIRST, json!([])),
        HandlerId::Rank,
    )
    .await;
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM member_levels")
        .fetch_one(db.pool())
        .await
        .unwrap();
    db.close().await.unwrap();
    let content = reply.content(true);
    let text = plain(content);
    assert!(text.contains("unranked"));
    assert!(content.contains("XP **0** · 0/100 this level · **100** to level 1"));
    assert!(
        text.contains("level 2") && text.contains("2 levels") && content.contains(ROLE_TWO),
        "first configured reward at level two: {content}"
    );
    assert_eq!(rows, 0, "reads never create XP profiles");
}

#[tokio::test]
async fn targeted_rank_uses_target_level_not_invoker_and_neutralizes_hostile_display_data() {
    let db = database().await;
    seed_lifetime(db.pool(), FIRST, 2205).await;
    seed_lifetime(db.pool(), FIRST + 1, 500).await;
    seed_rewards(db.pool()).await;
    let hostile = format!(
        "@everyone <@&{ROLE_SEVEN}> **fake**\n[click](https://invalid.example/) {}",
        "🦀".repeat(2500)
    );
    let mut request = serde_json::to_value(interaction(
        "rank",
        FIRST,
        json!([{"name":"member","type":6,"value":(FIRST+1).to_string()}]),
    ))
    .unwrap();
    request["data"]["resolved"] = json!({
        "users":{(FIRST+1).to_string():{"id":(FIRST+1).to_string(),"username":"target","global_name":hostile,"discriminator":"0","avatar":null,"bot":false}},
        "roles":{(ROLE_SEVEN):{"id":ROLE_SEVEN,"name":"@everyone **untrusted role**","color":0,"colors":{"primary_color":0,"secondary_color":null,"tertiary_color":null},"flags":0,"hoist":false,"managed":false,"mentionable":true,"permissions":"0","position":1}}
    });
    let reply = exchange(
        db.pool().clone(),
        serde_json::from_value(request).unwrap(),
        HandlerId::Rank,
    )
    .await;
    db.close().await.unwrap();
    let content = reply.content(true);
    let text = plain(content);
    assert!(
        text.contains("level 7") && text.contains("4 levels"),
        "target's configured reward, not the final-level invoker"
    );
    assert!(content.contains("Level **3**"));
    assert!(
        !content.contains("@everyone")
            && !content.contains("[click](")
            && !content.contains("**fake**")
    );
    assert!(
        !content.contains("untrusted role"),
        "do not infer rewards or display names from stale resolved roles"
    );
}

#[tokio::test]
async fn missing_reward_store_answers_unavailable_once_not_a_fabricated_no_further_reward() {
    let db = database().await;
    seed_lifetime(db.pool(), FIRST, 500).await;
    sqlx::query("ALTER TABLE level_role_rewards RENAME TO fixture_unavailable_rewards")
        .execute(db.pool())
        .await
        .unwrap();
    let reply = exchange(
        db.pool().clone(),
        interaction("rank", FIRST, json!([])),
        HandlerId::Rank,
    )
    .await;
    db.close().await.unwrap();
    assert!(
        matches!(reply.result, Err(LevelingRuntimeError::Store(_))),
        "configured reward read failure must propagate: {:?}",
        reply.result
    );
    assert_eq!(
        reply.bodies.len(),
        2,
        "one acknowledgement and one truthful unavailable edit"
    );
    assert_eq!(reply.bodies[0]["type"], 5);
    let text = plain(reply.bodies[1]["content"].as_str().unwrap());
    assert!(text.contains("unavailable") && (text.contains("retry") || text.contains("try again")));
    assert!(
        !text.contains("no further") && !text.contains("fixture_unavailable_rewards"),
        "neither fabricated success nor SQL details"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn control_paging_acknowledges_while_every_database_connection_is_held() {
    let db = database().await;
    seed_board(db.pool(), 25).await;
    let mut held = Vec::new();
    for _ in 0..5 {
        held.push(db.pool().acquire().await.unwrap());
    }
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, r#"{"id":"99"}"#),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool().clone(), &mock);
    let request = interaction("leaderboard", FIRST, page(2));
    let pending = runtime.handle_interaction_at(&request, HandlerId::Leaderboard, AT);
    tokio::pin!(pending);
    let (acknowledged, completed) = tokio::select! {
        result = &mut pending => (false, Some(result)),
        result = tokio::time::timeout(Duration::from_secs(2), async {
            while mock.requests().is_empty() { tokio::task::yield_now().await; }
        }) => (result.is_ok(), None),
    };
    let before_release = mock.requests();
    drop(held);
    let result = match completed {
        Some(result) => result,
        None => pending.await,
    };
    let bodies = mock.requests();
    mock.shutdown().await;
    db.close().await.unwrap();
    assert!(acknowledged, "ack must not wait for a DB connection");
    assert_eq!(
        before_release.len(),
        1,
        "only the acknowledgement can complete before the read"
    );
    let ack: Value = serde_json::from_slice(&before_release[0].body).unwrap();
    assert_eq!(ack["type"], 5);
    assert!(matches!(result, Ok(true)));
    assert_eq!(
        bodies.len(),
        2,
        "one completion after releasing DB capacity"
    );
}

#[tokio::test]
async fn control_new_inputs_do_not_bypass_the_existing_guild_fence() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .unwrap();
    pool.close().await;
    for options in [page(2), mine()] {
        let mut request = interaction("leaderboard", FIRST, options);
        request.guild_id = Some(twilight_model::id::Id::new(GUILD + 1));
        let reply = exchange(pool.clone(), request, HandlerId::Leaderboard).await;
        assert!(matches!(reply.result, Ok(false)));
        assert!(
            reply.bodies.is_empty(),
            "foreign guild receives no acknowledgement or reply"
        );
        assert!(
            reply.queries.is_empty(),
            "foreign guild does not touch the store"
        );
    }
}
