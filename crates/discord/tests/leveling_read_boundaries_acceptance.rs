#![cfg(feature = "db")]
//! Supplemental routed boundary cases for seasonal navigation and reward previews.
//! The fixture helpers follow `leveling_read_ux_acceptance` without changing that
//! frozen oracle. Only Discord REST is mocked; SQL reads use the migrated store.

#[path = "support/rest_capture.rs"]
mod rest_capture;

use rest_capture::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    fmt,
    sync::{Arc, Mutex},
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
use two_bot_core::{HandlerId, OnboardingGates, OnboardingMode};
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
            "one callback and one edit, no role calls"
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
        let reads: Vec<_> = self
            .queries
            .iter()
            .filter(|q| {
                let sql = q.statement.to_ascii_uppercase();
                sql.contains("SELECT") && sql.contains("MEMBER_SEASON_XP")
            })
            .collect();
        assert!(!reads.is_empty(), "observe the actual seasonal store read");
        let pages: Vec<_> = reads
            .iter()
            .filter(|q| q.statement.to_ascii_uppercase().contains("LIMIT"))
            .collect();
        assert_eq!(
            pages.len(),
            1,
            "exactly one SQL-bounded data page: {reads:?}"
        );
        assert_eq!(
            pages[0].returned, expected_rows,
            "only the requested page crosses the DB boundary"
        );
        for query in reads {
            let sql = query.statement.to_ascii_uppercase();
            assert!(query.returned <= 10, "no whole-guild fetch: {query:?}");
            assert!(
                sql.contains("LIMIT") || sql.contains("COUNT("),
                "other reads are scalar counts/positions: {query:?}"
            );
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

async fn exchange(pool: PgPool, request: Interaction, handler: HandlerId) -> Exchange {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, r#"{"id":"99"}"#),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = LevelingRuntime::new(
        pool,
        Arc::new(
            ActionExecutor::with_proxy("mock-only-token".into(), Some(mock.origin())).unwrap(),
        ),
        GUILD,
        OnboardingGates {
            mode: OnboardingMode::Legacy,
            dry_run: false,
        },
    );
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

async fn seed_lifetime(pool: &PgPool, member: u64, xp: i64) {
    sqlx::query("INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ($1,$2,$3,$3,NOW())")
        .bind(GUILD.to_string()).bind(member.to_string()).bind(xp).execute(pool).await.unwrap();
}

async fn seed_board(pool: &PgPool) {
    // Same 25-member board as the frozen oracle, including the 10/11 tie.
    for index in (0..25).rev() {
        let xp = if index == 10 {
            910
        } else {
            1000 - index as i64 * 10
        };
        sqlx::query("INSERT INTO member_season_xp (guild_id,season_id,member_id,seasonal_xp,updated_at) VALUES ($1,$2,$3,$4,NOW())")
            .bind(GUILD.to_string()).bind(SEASON).bind((FIRST+index).to_string()).bind(xp)
            .execute(pool).await.unwrap();
    }
    seed_lifetime(pool, ABSENT, 99_000).await;
    for (guild, season) in [(GUILD, "2026-Q3"), (GUILD + 1, SEASON)] {
        sqlx::query("INSERT INTO member_season_xp (guild_id,season_id,member_id,seasonal_xp,updated_at) VALUES ($1,$2,$3,99000,NOW())")
            .bind(guild.to_string()).bind(season).bind(ABSENT.to_string()).execute(pool).await.unwrap();
    }
}

async fn seed_rewards(pool: &PgPool) {
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

async fn assert_mine_boundary(
    invoker: u64,
    expected_page: &str,
    first_position: u64,
    own_position: &str,
) {
    let db = database().await;
    seed_board(db.pool()).await;
    let reply = exchange(
        db.pool().clone(),
        interaction(
            "leaderboard",
            invoker,
            json!([{"name":"mine","type":5,"value":true}]),
        ),
        HandlerId::Leaderboard,
    )
    .await;
    db.close().await.unwrap();
    let content = reply.content(false);
    let text = plain(content);
    assert!(
        text.contains(expected_page),
        "exact-multiple position stays on its page: {content}"
    );
    assert!(content.contains(SEASON) && text.contains("partial"));
    let expected: Vec<_> = (first_position..first_position + 10)
        .map(|p| (p, FIRST + p - 1))
        .collect();
    let rows = board_rows(content);
    assert_eq!(
        rows, expected,
        "exactly ten absolute positions on the selected page"
    );
    assert!(
        rows.iter().any(|(_, member)| *member == invoker),
        "mine page must contain the invoker"
    );
    assert!(
        text.contains(own_position),
        "explicit own position: {content}"
    );
    assert!(
        !content.contains(&ABSENT.to_string()),
        "no foreign/historical/lifetime fallback"
    );
    reply.bounded_page_read(10);
}

#[tokio::test]
async fn mine_position_ten_stays_on_page_one_and_reads_only_rows_one_through_ten() {
    // Catches floor(position / 10) + 1 incorrectly advancing exact multiples.
    assert_mine_boundary(FIRST + 9, "page 1 of 3", 1, "your position: #10").await;
}

#[tokio::test]
async fn mine_position_twenty_stays_on_page_two_and_reads_only_rows_eleven_through_twenty() {
    assert_mine_boundary(FIRST + 19, "page 2 of 3", 11, "your position: #20").await;
}

#[tokio::test]
async fn rank_one_level_below_reward_keeps_lifetime_progress_and_singular_distance() {
    // Catches skipping the immediate next threshold with reward.level > level + 1.
    let db = database().await;
    seed_lifetime(db.pool(), FIRST, 1625).await;
    seed_rewards(db.pool()).await;
    let reply = exchange(
        db.pool().clone(),
        interaction("rank", FIRST, json!([])),
        HandlerId::Rank,
    )
    .await;
    db.close().await.unwrap();
    let content = reply.content(true);
    assert!(
        content.contains("Level **6**"),
        "lifetime level six: {content}"
    );
    assert!(
        content.contains("XP **1625** · 0/580 this level · **580** to level 7"),
        "preserved lifetime XP progress: {content}"
    );
    let text = plain(content);
    assert!(
        text.contains("next") && text.contains("reward") && text.contains("level 7"),
        "next configured threshold: {content}"
    );
    assert!(
        content.contains(ROLE_SEVEN) && !content.contains(ROLE_TWO),
        "configured level-seven role, not a past reward"
    );
    assert!(
        text.contains("1 level away") && !text.contains("1 levels"),
        "singular distance of exactly one: {content}"
    );
    assert!(!text.contains("no further"), "a reward still exists");
}

#[tokio::test]
async fn maximum_advertised_page_is_read_then_truthfully_out_of_range_not_invalid_input() {
    // Catches an exclusive decoder upper bound rejecting the advertised page 1000.
    let db = database().await;
    seed_board(db.pool()).await;
    let reply = exchange(
        db.pool().clone(),
        interaction(
            "leaderboard",
            FIRST,
            json!([{"name":"page","type":4,"value":1000}]),
        ),
        HandlerId::Leaderboard,
    )
    .await;
    db.close().await.unwrap();
    let content = reply.content(false);
    let text = plain(content);
    assert!(
        text.contains("page 1000") && text.contains("out of range"),
        "accepted page with truthful range response: {content}"
    );
    assert!(
        text.contains("try page 3") || text.contains("last page is 3"),
        "last valid page is three: {content}"
    );
    assert!(content.contains(SEASON), "current season: {content}");
    assert!(
        board_rows(content).is_empty(),
        "never substitute another page"
    );
    assert!(
        !text.contains("choose a page from 1 to 1000"),
        "maximum is valid input"
    );
    assert!(!text.contains("no xp"), "a populated season is not empty");
    let reads: Vec<_> = reply
        .queries
        .iter()
        .filter(|query| {
            let sql = query.statement.to_ascii_uppercase();
            sql.contains("SELECT") && sql.contains("MEMBER_SEASON_XP")
        })
        .collect();
    assert!(
        !reads.is_empty(),
        "valid maximum must reach the actual store"
    );
    for query in reads {
        let sql = query.statement.to_ascii_uppercase();
        if sql.contains("COUNT(") {
            assert_eq!(query.returned, 1, "scalar count, not a guild member list");
        } else {
            assert!(sql.contains("LIMIT"), "no unbounded data read: {query:?}");
            assert_eq!(query.returned, 0, "no data rows exist on page 1000");
        }
    }
}
