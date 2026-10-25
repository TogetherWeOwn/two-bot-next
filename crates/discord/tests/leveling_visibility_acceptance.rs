#![cfg(feature = "db")]
//! Protected behavioral contract for private XP details and explicit lifetime standings.
//! Run with the disposable bootstrap from CONTRIBUTING.md, never application credentials:
//! python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
//!   --test leveling_visibility_acceptance -- --test-threads=1
//! Existing seasonal navigation/reward tests remain independently owned and unchanged.

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::Arc;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{HandlerId, OnboardingGates, OnboardingMode};
use two_bot_discord::{leveling_runtime::LevelingRuntimeError, ActionExecutor, LevelingRuntime};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 1545644954272137297;
const A: u64 = 100000000000000001;
const B: u64 = 100000000000000002;
const C: u64 = 100000000000000003;
const AT: &str = "2026-11-20T12:00:00.000Z";

fn interaction(name: &str, options: Value, member: u64) -> Interaction {
    serde_json::from_value(json!({
        "id":"7", "application_id":"8", "type":2, "token":"mock-visibility",
        "version":1, "guild_id":GUILD.to_string(), "locale":"en-US",
        "member":{"user":{"id":member.to_string(), "username":"Player", "discriminator":"0",
            "global_name":"Player", "avatar":null, "bot":false}, "roles":[], "flags":0,
            "joined_at":null, "deaf":false, "mute":false, "permissions":"0"},
        "data":{"id":"1", "name":name, "type":1, "options":options},
        "entitlements":[], "authorizing_integration_owners":{"0":GUILD.to_string()}
    }))
    .expect("valid synthetic interaction")
}

fn details(value: bool) -> Value {
    json!([{"name":"details", "type":5, "value":value}])
}

fn scope(value: &str) -> Value {
    json!([{"name":"scope", "type":3, "value":value}])
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

struct Fixture {
    db: TestDatabase,
    read_pool: PgPool,
    mock: MockRest,
    runtime: LevelingRuntime,
    before: Vec<String>,
}

async fn snapshot(pool: &PgPool) -> Vec<String> {
    // Whole row values, not just counts: consuming cooldown/reward state or changing
    // an existing XP projection must fail even when the row count stays constant.
    let mut rows = Vec::new();
    for table in [
        "member_levels",
        "member_season_xp",
        "xp_awards",
        "xp_cooldowns",
        "level_role_rewards",
    ] {
        let query =
            format!("SELECT row_to_json(t)::text FROM {table} t ORDER BY row_to_json(t)::text");
        let values: Vec<String> = sqlx::query_scalar(&query).fetch_all(pool).await.unwrap();
        rows.push(format!("{table}:{values:?}"));
    }
    rows
}

impl Fixture {
    async fn new() -> Self {
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create disposable fixture; no credential fallback");
        // Distinct source counters reveal swapped/omitted labels. The migrated
        // schema enforces their sum; do not weaken that constraint in this fixture.
        for (guild, member, xp, message, voice, imported) in [
            (GUILD, A, 1234_i64, 137_i64, 59_i64, 1038_i64),
            (GUILD, B, 2000, 0, 0, 2000),
            (GUILD, C, 0, 0, 0, 0),
            (GUILD + 1, A + 90, 99999, 0, 0, 99999),
        ] {
            sqlx::query("INSERT INTO member_levels (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at) VALUES ($1,$2,$3,$4,$5,$6,NOW())")
                .bind(guild.to_string()).bind(member.to_string()).bind(xp)
                .bind(message).bind(voice).bind(imported).execute(db.pool()).await.unwrap();
        }
        for (guild, season, member, xp) in [
            (GUILD, "2026-Q4", A, 900_i64),
            (GUILD, "2026-Q4", B, 100),
            (GUILD, "2026-Q3", C, 99999),
            (GUILD + 1, "2026-Q4", A + 90, 99999),
        ] {
            sqlx::query("INSERT INTO member_season_xp (guild_id, season_id, member_id, seasonal_xp, updated_at) VALUES ($1,$2,$3,$4,NOW())")
                .bind(guild.to_string()).bind(season).bind(member.to_string()).bind(xp)
                .execute(db.pool()).await.unwrap();
        }
        sqlx::query("INSERT INTO level_role_rewards (guild_id,level,role_id) VALUES ($1,1,'200000000000000001')")
            .bind(GUILD.to_string()).execute(db.pool()).await.unwrap();
        let before = snapshot(db.pool()).await;
        let read_pool = PgPoolOptions::new()
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
            .unwrap();
        let responses = (0..20)
            .flat_map(|_| {
                [
                    ScriptedResponse::status(204),
                    ScriptedResponse::json(200, json!({"id":"99"})),
                ]
            })
            .collect();
        let mock = MockRest::start(responses, ScriptedResponse::status(500)).await;
        let runtime = runtime(read_pool.clone(), &mock);
        Self {
            db,
            read_pool,
            mock,
            runtime,
            before,
        }
    }

    async fn invoke(
        &self,
        name: &str,
        options: Value,
        member: u64,
        at: &str,
    ) -> Result<bool, LevelingRuntimeError> {
        let handler = if name == "rank" {
            HandlerId::Rank
        } else {
            HandlerId::Leaderboard
        };
        self.runtime
            .handle_interaction_at(&interaction(name, options, member), handler, at)
            .await
    }

    async fn finish(self) -> Vec<Value> {
        let after = snapshot(self.db.pool()).await;
        let requests = self.mock.requests();
        self.read_pool.close().await;
        self.mock.shutdown().await;
        self.db.close().await.expect("drop disposable fixture");
        assert_eq!(
            after, self.before,
            "reads never change XP, cooldown, awards, or rewards"
        );
        assert_eq!(
            requests.len() % 2,
            0,
            "one ack and completion per handled command"
        );
        let mut replies = Vec::new();
        for pair in requests.chunks_exact(2) {
            assert_eq!(
                (pair[0].method.as_str(), pair[0].path.as_str()),
                ("POST", "/api/v10/interactions/7/mock-visibility/callback")
            );
            assert_eq!(
                (pair[1].method.as_str(), pair[1].path.as_str()),
                (
                    "PATCH",
                    "/api/v10/webhooks/8/mock-visibility/messages/@original"
                ),
                "no notifications or role grants"
            );
            let ack: Value = serde_json::from_slice(&pair[0].body).unwrap();
            let edit: Value = serde_json::from_slice(&pair[1].body).unwrap();
            assert_eq!(ack["type"], 5, "one defer, never a second initial response");
            assert_eq!(edit["allowed_mentions"]["parse"], json!([]));
            assert!(edit["content"].as_str().unwrap().chars().count() <= 2000);
            replies.push(json!({"ack":ack, "edit":edit}));
        }
        replies
    }
}

fn content(reply: &Value) -> &str {
    reply["edit"]["content"].as_str().unwrap()
}

fn contains_number(text: &str, number: &str) -> bool {
    text.replace(',', "").contains(number)
}

#[tokio::test]
async fn rank_details_expose_independent_sources_without_recomputing_stored_total() {
    let f = Fixture::new().await;
    let plain = f.invoke("rank", json!([]), A, AT).await;
    let explicit_plain = f.invoke("rank", details(false), A, AT).await;
    let detailed = f.invoke("rank", details(true), A, AT).await;
    let replies = f.finish().await;
    for result in [plain, explicit_plain, detailed] {
        assert!(result.unwrap());
    }
    assert_eq!(replies.len(), 3);
    assert_eq!(
        content(&replies[0]),
        content(&replies[1]),
        "details=false preserves default output"
    );
    for reply in &replies {
        assert_eq!(reply["ack"]["data"]["flags"], 64, "rank remains private");
    }
    let text = content(&replies[2]);
    for (label, number) in [
        ("message", "137"),
        ("voice", "59"),
        ("import", "1038"),
        ("total", "1234"),
    ] {
        assert!(
            text.lines()
                .any(|line| line.to_lowercase().contains(label) && contains_number(line, number)),
            "details must label {label} XP ({number}), got {text}"
        );
    }
    assert!(
        text.to_lowercase().contains("adjust") || text.to_lowercase().contains("differ"),
        "explain that sources can differ from stored total: {text}"
    );
    assert!(
        text.contains("#2"),
        "rank remains lifetime-based, not source-sum or seasonal"
    );
}

#[tokio::test]
async fn explicit_lifetime_scope_uses_opposite_order_and_labels_lifetime() {
    let f = Fixture::new().await;
    let result = f.invoke("leaderboard", scope("lifetime"), A, AT).await;
    let replies = f.finish().await;
    assert!(result.unwrap());
    assert_eq!(replies.len(), 1);
    let text = content(&replies[0]);
    assert!(
        text.to_lowercase().contains("lifetime"),
        "explicit scope must be labeled: {text}"
    );
    let rows: Vec<_> = text.lines().filter(|line| line.contains("<@")).collect();
    assert_eq!(rows.len(), 3);
    for (row, member, xp) in [
        (rows[0], B, "2000"),
        (rows[1], A, "1234"),
        (rows[2], C, "0"),
    ] {
        assert!(
            row.contains(&format!("<@{member}>")) && contains_number(row, xp),
            "lifetime ordering/XP: {row}"
        );
    }
    assert_ne!(
        replies[0]["ack"]["data"]["flags"], 64,
        "board keeps existing public visibility"
    );
    assert!(
        !text.contains("partial"),
        "lifetime is not a partial season"
    );
}

#[tokio::test]
async fn default_and_explicit_seasonal_keep_partial_marker_and_season_order() {
    let f = Fixture::new().await;
    let implicit = f.invoke("leaderboard", json!([]), A, AT).await;
    let explicit = f.invoke("leaderboard", scope("seasonal"), A, AT).await;
    let replies = f.finish().await;
    assert!(implicit.unwrap() && explicit.unwrap());
    assert_eq!(replies.len(), 2);
    assert_eq!(content(&replies[0]), content(&replies[1]));
    let text = content(&replies[0]);
    assert!(text.contains("2026-Q4") && text.contains("partial: early-season XP not counted"));
    let rows: Vec<_> = text.lines().filter(|line| line.contains("<@")).collect();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].contains(&A.to_string()) && rows[0].contains("900 XP"));
    assert!(rows[1].contains(&B.to_string()) && rows[1].contains("100 XP"));
    assert!(
        !text.contains(&(A + 90).to_string()),
        "foreign guild stays hidden"
    );
}

#[tokio::test]
async fn empty_season_does_not_substitute_nonempty_lifetime() {
    let f = Fixture::new().await;
    let a = f
        .invoke("leaderboard", json!([]), A, "2027-02-01T00:00:00.000Z")
        .await;
    let b = f
        .invoke(
            "leaderboard",
            scope("seasonal"),
            A,
            "2027-02-01T00:00:00.000Z",
        )
        .await;
    let replies = f.finish().await;
    assert!(a.unwrap() && b.unwrap());
    assert_eq!(replies.len(), 2);
    for reply in &replies {
        let text = content(reply);
        assert!(text.contains("2027-Q1") && text.contains("No XP has been earned yet."));
        assert!(!text.contains("<@"), "no lifetime fallback");
        assert!(!text.contains("partial"), "later quarter is not partial");
    }
}

#[tokio::test]
async fn detailed_rank_preserves_absent_versus_stored_zero() {
    let f = Fixture::new().await;
    let absent = f.invoke("rank", details(true), A + 99, AT).await;
    let zero = f.invoke("rank", details(true), C, AT).await;
    let replies = f.finish().await;
    assert!(absent.unwrap() && zero.unwrap());
    assert_eq!(replies.len(), 2);
    assert!(
        content(&replies[0]).contains("Unranked")
            && content(&replies[0]).contains("no XP recorded")
    );
    assert!(content(&replies[1]).contains("#3") && !content(&replies[1]).contains("Unranked"));
    for reply in &replies {
        assert_eq!(reply["ack"]["data"]["flags"], 64);
    }
}

async fn closed_pool() -> PgPool {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .unwrap();
    pool.close().await;
    pool
}

#[tokio::test]
async fn malformed_new_options_are_rejected_before_ack_or_store() {
    let cases = [
        ("rank", json!([{"name":"details","type":3,"value":"true"}])),
        ("rank", json!([{"name":"details","type":4,"value":1}])),
        (
            "rank",
            json!([{"name":"details","type":5,"value":true},{"name":"details","type":5,"value":false}]),
        ),
        ("rank", json!([{"name":"detail","type":5,"value":true}])),
        ("leaderboard", scope("all")),
        ("leaderboard", scope("Lifetime")),
        ("leaderboard", scope(" lifetime")),
        ("leaderboard", scope("")),
        (
            "leaderboard",
            json!([{"name":"scope","type":5,"value":true}]),
        ),
        ("leaderboard", json!([{"name":"scope","type":4,"value":1}])),
        (
            "leaderboard",
            json!([{"name":"scope","type":3,"value":"seasonal"},{"name":"scope","type":3,"value":"lifetime"}]),
        ),
        (
            "leaderboard",
            json!([{"name":"scopes","type":3,"value":"lifetime"}]),
        ),
    ];
    let mut results = Vec::new();
    for (name, options) in cases {
        let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
        let rt = runtime(closed_pool().await, &mock);
        let handler = if name == "rank" {
            HandlerId::Rank
        } else {
            HandlerId::Leaderboard
        };
        let result = rt
            .handle_interaction_at(&interaction(name, options.clone(), A), handler, AT)
            .await;
        let requests = mock.requests();
        mock.shutdown().await;
        results.push((
            name,
            options,
            matches!(result, Err(LevelingRuntimeError::InvalidInteraction)),
            requests.len(),
        ));
    }
    for (name, options, invalid, requests) in results {
        assert!(
            invalid,
            "{name} {options} must be invalid input, not accepted/store failure"
        );
        assert_eq!(
            requests, 0,
            "malformed input is rejected before acknowledgement"
        );
    }
}

#[tokio::test]
async fn foreign_guild_and_guildless_new_options_are_silent_before_store() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rt = runtime(closed_pool().await, &mock);
    let mut results = Vec::new();
    for (name, options, handler) in [
        ("rank", details(true), HandlerId::Rank),
        ("leaderboard", scope("lifetime"), HandlerId::Leaderboard),
    ] {
        for guild in [None, Some(twilight_model::id::Id::new(GUILD + 1))] {
            let mut input = interaction(name, options.clone(), A);
            input.guild_id = guild;
            results.push(rt.handle_interaction_at(&input, handler, AT).await);
        }
    }
    let requests = mock.requests();
    mock.shutdown().await;
    for result in results {
        assert!(!result.unwrap());
    }
    assert!(requests.is_empty());
}

#[tokio::test]
async fn lifetime_dispatch_keeps_top_ten_bound_and_member_id_tie_order() {
    let mut f = Fixture::new().await;
    for offset in (10..24).rev() {
        sqlx::query("INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ($1,$2,3000,3000,NOW())")
            .bind(GUILD.to_string()).bind((A + offset).to_string())
            .execute(f.db.pool()).await.unwrap();
    }
    f.before = snapshot(f.db.pool()).await;
    let result = f.invoke("leaderboard", scope("lifetime"), A, AT).await;
    let replies = f.finish().await;
    assert!(result.unwrap());
    assert_eq!(replies.len(), 1);
    let rows: Vec<_> = content(&replies[0])
        .lines()
        .filter(|line| line.contains("<@"))
        .collect();
    assert_eq!(
        rows.len(),
        10,
        "explicit lifetime does not invent pagination or unbounded output"
    );
    for (index, row) in rows.iter().enumerate() {
        assert!(
            row.contains(&format!("<@{}>", A + 10 + index as u64)),
            "equal totals break ties by member id: {row}"
        );
        assert!(contains_number(row, "3000"));
    }
}

#[tokio::test]
async fn targeted_details_keep_target_identity_and_hostile_name_sanitization() {
    let f = Fixture::new().await;
    let mut input = serde_json::to_value(interaction("rank", json!([]), B)).unwrap();
    input["data"]["options"] = json!([
        {"name":"member","type":6,"value":A.to_string()},
        {"name":"details","type":5,"value":true}
    ]);
    input["data"]["resolved"] = json!({"users":{A.to_string():{
        "id":A.to_string(), "username":"Target", "discriminator":"0", "avatar":null,
        "global_name":"**forged**\n<@100000000000000099> @everyone", "bot":false
    }}});
    let input: Interaction = serde_json::from_value(input).unwrap();
    let result = f
        .runtime
        .handle_interaction_at(&input, HandlerId::Rank, AT)
        .await;
    let replies = f.finish().await;
    assert!(result.unwrap());
    assert_eq!(replies.len(), 1);
    let text = content(&replies[0]);
    assert!(
        contains_number(text, "1234") && !contains_number(text, "2000"),
        "details describe the target, not invoker"
    );
    assert!(
        !text.contains("<@100000000000000099>"),
        "untrusted name cannot forge a mention token"
    );
    assert!(
        !text.contains("**forged**\n"),
        "untrusted name cannot inject a new formatted line"
    );
    assert_eq!(replies[0]["ack"]["data"]["flags"], 64);
}

#[tokio::test]
async fn new_read_modes_complete_store_failure_once_with_sanitized_unavailable() {
    for (name, options, handler, private) in [
        ("rank", details(true), HandlerId::Rank, true),
        (
            "leaderboard",
            scope("lifetime"),
            HandlerId::Leaderboard,
            false,
        ),
    ] {
        let mock = MockRest::start(
            vec![
                ScriptedResponse::status(204),
                ScriptedResponse::json(200, json!({"id":"99"})),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        let rt = runtime(closed_pool().await, &mock);
        let result = rt
            .handle_interaction_at(&interaction(name, options, A), handler, AT)
            .await;
        let requests = mock.requests();
        mock.shutdown().await;
        assert!(matches!(result, Err(LevelingRuntimeError::Store(_))));
        assert_eq!(requests.len(), 2, "one ack, one unavailable completion");
        let ack: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let reply: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(ack["type"], 5);
        assert_eq!(ack["data"]["flags"] == 64, private);
        assert_eq!(requests[1].method, "PATCH");
        assert!(requests[1].path.ends_with("/messages/@original"));
        let text = reply["content"].as_str().unwrap().to_lowercase();
        assert!(
            text.contains("unavailable") && text.contains("try"),
            "actionable unavailable: {text}"
        );
        for forbidden in ["postgres", "sql", "pool", "agent_test", "mock-visibility"] {
            assert!(!text.contains(forbidden));
        }
        assert_eq!(reply["allowed_mentions"]["parse"], json!([]));
    }
}
