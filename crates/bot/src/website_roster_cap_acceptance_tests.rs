//! Acceptance: the website roster read is page-capped like the bot-floor scan.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch,
//! keep every test here verbatim, and make the RED tests pass by changing
//! production code only (`crates/bot/src/website_jobs.rs`). Do not edit the
//! fixtures or assertions: they are the spec. The existing
//! `website_jobs_tests` that call `roster()` directly are not protected;
//! update them for the new signature.
//!
//! Today `roster()` pages `GET /guilds/{guild}/members?limit=1000&after=`
//! until a short page and gathers every member into one unbounded `Vec`, for
//! both the counter and the rank tick. Twenty lines below it,
//! `bot_floor_scan` stops after eleven pages (ten full pages plus one
//! termination probe) and answers `Truncated` instead of a partial count.
//!
//! The contract under test is observed only through `website_jobs::run_once`.
//! No new symbol is named here, so the baseline compiles and fails on
//! behavior, and `roster()` is free to take a page budget and answer
//! `Complete(Vec)` / `Truncated`, mirroring `BotFloorScan`:
//!
//! - Budget: a counter or rank tick reads at most eleven member pages, each
//!   with `limit=1000` and `after=` the previous page's last member id
//!   (`after=0` first). A short page within the budget completes the roster.
//! - RED, truncated: when the eleventh page is still full, completion is
//!   unproven. The tick sends nothing else (no twelfth page, no guild read,
//!   no self-heal read, no role write) and publishes nothing: every website
//!   row, timestamp and the contract pin stay byte-identical. It returns
//!   `Ok(())` like the ungrounded-history skip or `Err(Configuration)` like
//!   the unusable-ladder skip, never a wire or database failure. It logs
//!   exactly one WARN line carrying `job = "counter"` or `job = "rank"`
//!   whose message contains `roster truncated` and `publication skipped`.
//!   Fixtures: exactly 11,000 members (eleven full pages, the boundary) and
//!   11,500 members (a twelve-page roster).
//! - Control, complete: one short page (999 members), a full page plus an
//!   empty probe (1,000), ten full pages plus an empty eleventh (10,000) and
//!   ten full pages plus a short eleventh (10,999) publish exactly what the
//!   unbounded read publishes today, over exactly today's requests.
//! - Control, cursor: a member id that does not strictly increase past the
//!   cursor still refuses the whole tick as `Err(Rest)` with nothing
//!   published: when it reopens the eleventh page, inside a full eleventh
//!   page (an error, never a truncation), and inside an early page.
//!
//! Offline only: a disposable test database (`TWO_TEST_DATABASE_URL`; the
//! tests skip locally without it and CI always supplies it) plus a stateless
//! Discord double. No live Discord.
#![cfg(test)]

use std::collections::HashMap;

use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::sync::{watch, Mutex};
use two_bot_core::apply_web_contract;
use two_bot_discord::executor::ActionExecutor;
use two_bot_testsupport::TestDatabase;

use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::jobs::ErrorClass;
use crate::tracing_capture;
use crate::website_jobs::{run_once, Kind};

const GUILD: &str = "2222";
const MEMBER_PAGE: &str = "/api/v10/guilds/2222/members?";
const GUILD_OBJECT: &str = "/api/v10/guilds/2222";

/// The eleven cursors of the page budget: `after=0`, then the last id of
/// each full page (fixture member ids run 1, 2, 3, … without gaps).
const BUDGET_CURSORS: [u64; 11] = [
    0, 1_000, 2_000, 3_000, 4_000, 5_000, 6_000, 7_000, 8_000, 9_000, 10_000,
];

/// One raid-window joiner on file per `RAID_ANOMALIES` window: present
/// humans who never posted, so every roster listing them excludes them.
const RAID_JOINERS: [u64; 3] = [7, 17, 27];

/// A guild of members `1..=total`. Every tenth id is a bot; member 1 holds
/// Prospect, member 2 Prospect and Member, and the last human the whole
/// ladder, so the final page shapes the rank snapshot. With `heal_bait`,
/// member 3 holds Member without Prospect: a ladder the permitted rank tick
/// would self-heal with a role grant.
#[derive(Clone, Copy)]
struct Roster {
    total: u64,
    heal_bait: bool,
}

impl Roster {
    fn complete(total: u64) -> Self {
        Self {
            total,
            heal_bait: false,
        }
    }

    fn last_human(self) -> u64 {
        if self.total.is_multiple_of(10) {
            self.total - 1
        } else {
            self.total
        }
    }

    fn member(self, id: u64) -> Value {
        let roles: &[&str] = if id == 1 {
            &["11"]
        } else if id == 2 {
            &["11", "12"]
        } else if id == 3 && self.heal_bait {
            &["12"]
        } else if id == self.last_human() {
            &["11", "12", "13", "14", "15"]
        } else {
            &[]
        };
        json!({"user": {"id": id.to_string(), "bot": id.is_multiple_of(10)}, "roles": roles})
    }

    /// Discord's answer for a cursor: the next ids past it, ascending, at
    /// most `limit` of them.
    fn page(self, after: u64, limit: u64) -> Value {
        let last = after.saturating_add(limit).min(self.total);
        Value::Array(
            (after.saturating_add(1)..=last)
                .map(|id| self.member(id))
                .collect(),
        )
    }
}

/// `limit` and `after` from a member-page query; anything else is refused.
fn page_query(query: &str) -> Option<(u64, u64)> {
    let (mut limit, mut after) = (None, None);
    for pair in query.split('&') {
        match pair.split_once('=')? {
            ("limit", value) => limit = Some(value.parse().ok()?),
            ("after", value) => after = Some(value.parse().ok()?),
            _ => return None,
        }
    }
    Some((limit?, after?))
}

/// The five-rung ladder below the bot's own role, with positions.
fn guild_object() -> Value {
    json!({"roles": [
        {"id": "10", "name": "Bot", "position": 99},
        {"id": "11", "name": "Prospect", "position": 1},
        {"id": "12", "name": "Member", "position": 2},
        {"id": "13", "name": "Soldier", "position": 3},
        {"id": "14", "name": "Veteran", "position": 4},
        {"id": "15", "name": "Legend", "position": 5},
    ]})
}

/// A stateless Discord double for the guild. Member pages are cut from
/// `roster` by cursor the way Discord pages them, unless `scripted` holds a
/// page for that cursor (malformed fixtures). The guild object, the
/// self-heal identity reads and role writes answer like Discord. Anything
/// else is a 404, so an unexpected read fails fast instead of riding the
/// executor's 5xx retries.
async fn discord(roster: Roster, scripted: HashMap<u64, Value>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        if request.method == "PUT" && request.path.starts_with("/api/v10/guilds/2222/members/") {
            return ScriptedResponse::status(204);
        }
        if request.method != "GET" {
            return ScriptedResponse::status(404);
        }
        if let Some(query) = request.path.strip_prefix(MEMBER_PAGE) {
            let Some((limit, after)) = page_query(query) else {
                return ScriptedResponse::status(400);
            };
            let page = match scripted.get(&after) {
                Some(page) => page.clone(),
                None => roster.page(after, limit.min(1000)),
            };
            return ScriptedResponse::json(200, page);
        }
        match request.path.as_str() {
            GUILD_OBJECT => ScriptedResponse::json(200, guild_object()),
            "/api/v10/users/@me" => ScriptedResponse::json(200, json!({"id": "999", "bot": true})),
            "/api/v10/guilds/2222/members/999" => {
                ScriptedResponse::json(200, json!({"roles": ["10"]}))
            }
            _ => ScriptedResponse::status(404),
        }
    })
    .await
}

/// Member-page cursors in request order, and every other request as
/// `METHOD path`. A member page must ask for `limit=1000` and nothing else.
fn wire(mock: &MockRest) -> (Vec<u64>, Vec<String>) {
    let mut cursors = Vec::new();
    let mut others = Vec::new();
    for request in mock.requests() {
        match request.path.strip_prefix(MEMBER_PAGE).and_then(page_query) {
            Some((1000, after)) if request.method == "GET" => cursors.push(after),
            _ => others.push(format!("{} {}", request.method, request.path)),
        }
    }
    (cursors, others)
}

async fn database() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP website roster cap acceptance: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated agent-testdb fixture");
    apply_web_contract(db.pool()).await.unwrap();
    // Ground every raid window so both ticks reach the roster read.
    for (anomaly, id) in two_bot_core::RAID_ANOMALIES.iter().zip(RAID_JOINERS) {
        let (start, _) = two_bot_core::window_bounds(anomaly.start, anomaly.end).unwrap();
        sqlx::query("INSERT INTO members (guild_id, member_id, joined_at, is_bot) VALUES ($1,$2,$3::timestamptz,FALSE)")
            .bind(GUILD)
            .bind(id.to_string())
            .bind(start)
            .execute(db.pool())
            .await
            .unwrap();
    }
    Some(db)
}

/// One tick under the permitted identity: a truncated rank tick must not
/// heal either.
async fn tick(kind: Kind, pool: &PgPool, mock: &MockRest) -> Result<(), ErrorClass> {
    crate::gateway::ensure_crypto_provider();
    let rest = ActionExecutor::with_proxy(
        "synthetic-roster-cap-test-token".to_owned(),
        Some(mock.origin()),
    )
    .unwrap();
    let (_stop, shutdown) = watch::channel(false);
    run_once(kind, pool, &rest, GUILD, &Mutex::new(()), &shutdown, true).await
}

/// Every website row, every field and timestamp included.
async fn mirror(pool: &PgPool) -> Vec<String> {
    let mut rows = Vec::new();
    for table in [
        "web_contract_meta",
        "guild_counters",
        "counter_snapshots",
        "rank_ladder",
        "rank_snapshots",
        "member_ranks",
        "member_exclusions",
        "scheduled_events",
    ] {
        // Fixed fixture-only identifiers.
        let sql = sqlx::AssertSqlSafe(format!(
            "SELECT '{table} ' || to_jsonb(t)::text FROM {table} t ORDER BY to_jsonb(t)::text"
        ));
        rows.extend(
            sqlx::query_scalar::<_, String>(sql)
                .fetch_all(pool)
                .await
                .unwrap(),
        );
    }
    rows
}

/// The published count in the live counter and in its audit trail.
async fn counter(pool: &PgPool) -> (i32, i32) {
    let mut counts = Vec::new();
    for query in [
        "SELECT human_member_count FROM guild_counters WHERE guild_id=$1",
        "SELECT human_member_count FROM counter_snapshots WHERE guild_id=$1",
    ] {
        counts.push(
            sqlx::query_scalar::<_, i32>(query)
                .bind(GUILD)
                .fetch_one(pool)
                .await
                .unwrap(),
        );
    }
    (counts[0], counts[1])
}

async fn contract_pin(pool: &PgPool) -> Option<String> {
    sqlx::query_scalar("SELECT guild_id FROM web_contract_meta WHERE singleton = TRUE")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Publish a small complete roster through both ticks, so "nothing
/// published" is measured against live rows instead of empty tables.
async fn publish_prior(pool: &PgPool) {
    let mock = discord(Roster::complete(25), HashMap::new()).await;
    for kind in [Kind::Counter, Kind::Rank] {
        assert_eq!(
            tick(kind, pool, &mock).await,
            Ok(()),
            "prior complete publication"
        );
    }
    mock.shutdown().await;
    // 25 members, less bots 10 and 20, less raid joiners 7 and 17.
    assert_eq!(
        counter(pool).await,
        (21, 21),
        "the prior publication is live"
    );
}

/// WARN lines naming `job` that give the truncation reason.
fn truncation_warnings(logs: &str, job: &str) -> usize {
    let (quoted, bare) = (format!("job=\"{job}\""), format!("job={job}"));
    logs.lines()
        .filter(|line| line.starts_with("WARN "))
        .filter(|line| {
            line.split_whitespace()
                .any(|field| field == quoted || field == bare)
        })
        .filter(|line| line.contains("roster truncated") && line.contains("publication skipped"))
        .count()
}

/// RED: today the tick reads the twelfth page and publishes the whole
/// roster. Every violation is collected so one run shows the full gap.
async fn truncated_roster_skips_publication(kind: Kind, job: &str) {
    let capture = tracing_capture::global();
    let Some(db) = database().await else {
        return;
    };
    let pool = db.pool();
    publish_prior(pool).await;
    let mut violations = Vec::new();
    for total in [11_000, 11_500] {
        let mock = discord(
            Roster {
                total,
                heal_bait: true,
            },
            HashMap::new(),
        )
        .await;
        let before = mirror(pool).await;
        let start = capture.text().len();
        let outcome = tick(kind, pool, &mock).await;
        let logs = capture.text().split_off(start);
        let (cursors, others) = wire(&mock);
        mock.shutdown().await;
        if !matches!(outcome, Ok(()) | Err(ErrorClass::Configuration)) {
            violations.push(format!(
                "{total} members: outcome {outcome:?}; a truncation is neither a wire nor a database failure"
            ));
        }
        if cursors != BUDGET_CURSORS {
            violations.push(format!(
                "{total} members: member pages read at cursors {cursors:?}; the budget is exactly {BUDGET_CURSORS:?}"
            ));
        }
        if !others.is_empty() {
            violations.push(format!(
                "{total} members: requests beyond the member-page budget: {others:?}"
            ));
        }
        if mirror(pool).await != before {
            violations.push(format!(
                "{total} members: a truncated roster changed the published website rows"
            ));
        }
        let warnings = truncation_warnings(&logs, job);
        if warnings != 1 {
            violations.push(format!(
                "{total} members: {warnings} WARN lines with job={job}, `roster truncated` and `publication skipped`; expected exactly one"
            ));
        }
    }
    db.close().await.expect("drop disposable test database");
    assert!(
        violations.is_empty(),
        "truncated {job} tick:\n{}",
        violations.join("\n")
    );
}

#[tokio::test]
async fn truncated_roster_skips_counter_publication_with_an_explicit_reason() {
    truncated_roster_skips_publication(Kind::Counter, "counter").await;
}

#[tokio::test]
async fn truncated_roster_skips_rank_publication_and_never_heals() {
    truncated_roster_skips_publication(Kind::Rank, "rank").await;
}

/// Control: in-budget rosters publish the same counter over the same
/// requests. Humans = total - bots (every tenth id) - raid joiners 7, 17, 27.
#[tokio::test]
async fn in_budget_rosters_publish_the_same_counter() {
    let Some(db) = database().await else {
        return;
    };
    let pool = db.pool();
    for (total, pages, humans) in [
        (999, 1, 897),
        (1_000, 2, 897),
        (10_000, 11, 8_997),
        (10_999, 11, 9_897),
    ] {
        let mock = discord(Roster::complete(total), HashMap::new()).await;
        assert_eq!(
            tick(Kind::Counter, pool, &mock).await,
            Ok(()),
            "{total} members"
        );
        let (cursors, others) = wire(&mock);
        mock.shutdown().await;
        assert_eq!(
            cursors,
            BUDGET_CURSORS[..pages],
            "{total} members: member pages"
        );
        assert!(others.is_empty(), "{total} members: {others:?}");
        assert_eq!(
            counter(pool).await,
            (humans, humans),
            "{total} members: published count"
        );
        assert_eq!(contract_pin(pool).await.as_deref(), Some(GUILD));
    }
    db.close().await.expect("drop disposable test database");
}

/// Control: in-budget rosters publish the same rank snapshot over the same
/// requests: counters, the five rungs, one member-rank row per counted
/// human, the raid exclusions and the ladder role ids.
#[tokio::test]
async fn in_budget_rosters_publish_the_same_rank_snapshot() {
    let Some(db) = database().await else {
        return;
    };
    let pool = db.pool();
    for (total, pages, humans, legend) in [
        (999, 1, 897, 999),
        (1_000, 2, 897, 999),
        (10_000, 11, 8_997, 9_999),
        (10_999, 11, 9_897, 10_999),
    ] {
        let mock = discord(Roster::complete(total), HashMap::new()).await;
        assert_eq!(
            tick(Kind::Rank, pool, &mock).await,
            Ok(()),
            "{total} members"
        );
        let (cursors, others) = wire(&mock);
        mock.shutdown().await;
        assert_eq!(
            cursors,
            BUDGET_CURSORS[..pages],
            "{total} members: member pages"
        );
        assert_eq!(others, ["GET /api/v10/guilds/2222"], "{total} members");
        assert_eq!(
            counter(pool).await,
            (humans, humans),
            "{total} members: published count"
        );
        let rungs: Vec<(String, i32, i32)> = sqlx::query_as(
            "SELECT rank_key, member_count, holders_count FROM rank_snapshots WHERE guild_id=$1 ORDER BY rank_key",
        )
        .bind(GUILD)
        .fetch_all(pool)
        .await
        .unwrap();
        let rungs: Vec<_> = rungs
            .iter()
            .map(|(key, highest, holders)| (key.as_str(), *highest, *holders))
            .collect();
        assert_eq!(
            rungs,
            [
                ("legend", 1, 1),
                ("member", 1, 2),
                ("prospect", 1, 3),
                ("soldier", 0, 1),
                ("veteran", 0, 1),
            ],
            "{total} members: rank rungs"
        );
        let ranks: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT member_id, rank_key FROM member_ranks WHERE guild_id=$1 ORDER BY member_id::bigint",
        )
        .bind(GUILD)
        .fetch_all(pool)
        .await
        .unwrap();
        let expected: Vec<(String, Option<String>)> = (1..=total)
            .filter(|id| !id.is_multiple_of(10) && !RAID_JOINERS.contains(id))
            .map(|id| {
                let rank = match id {
                    1 => Some("prospect"),
                    2 => Some("member"),
                    _ if id == legend => Some("legend"),
                    _ => None,
                };
                (id.to_string(), rank.map(str::to_owned))
            })
            .collect();
        assert_eq!(
            ranks.len(),
            humans as usize,
            "{total} members: member-rank rows"
        );
        if let Some(row) = (0..expected.len()).find(|&row| ranks.get(row) != expected.get(row)) {
            panic!(
                "{total} members: member_ranks row {row} is {:?}, expected {:?}",
                ranks.get(row),
                expected.get(row)
            );
        }
        let excluded: Vec<String> = sqlx::query_scalar(
            "SELECT member_id FROM member_exclusions WHERE guild_id=$1 ORDER BY member_id::bigint",
        )
        .bind(GUILD)
        .fetch_all(pool)
        .await
        .unwrap();
        assert_eq!(excluded, ["7", "17", "27"], "{total} members: exclusions");
        let ladder: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT rank_key, role_id FROM rank_ladder ORDER BY rank_order")
                .fetch_all(pool)
                .await
                .unwrap();
        let ladder: Vec<_> = ladder
            .iter()
            .map(|(key, role)| (key.as_str(), role.as_deref()))
            .collect();
        assert_eq!(
            ladder,
            [
                ("prospect", Some("11")),
                ("member", Some("12")),
                ("soldier", Some("13")),
                ("veteran", Some("14")),
                ("legend", Some("15")),
            ],
            "{total} members: ladder"
        );
        assert_eq!(contract_pin(pool).await.as_deref(), Some(GUILD));
    }
    db.close().await.expect("drop disposable test database");
}

/// Control: the strictly-increasing cursor check survives under the cap.
#[tokio::test]
async fn cursor_regressions_still_refuse_the_tick_under_the_cap() {
    let Some(db) = database().await else {
        return;
    };
    let pool = db.pool();
    publish_prior(pool).await;
    let roster = Roster::complete(10_500);
    let mut full_eleventh: Vec<u64> = (10_001..=11_000).collect();
    // 10,500 now follows 10,501: a step back inside a full eleventh page.
    full_eleventh.swap(499, 500);
    let cases = [
        (
            "the eleventh page reopens at the cursor",
            10_000,
            vec![10_000, 10_001, 10_002],
            11,
        ),
        (
            "a full eleventh page steps back mid-page",
            10_000,
            full_eleventh,
            11,
        ),
        (
            "the second page steps back inside the page",
            1_000,
            vec![1_001, 1_003, 1_002],
            2,
        ),
    ];
    for (kind, job) in [(Kind::Counter, "counter"), (Kind::Rank, "rank")] {
        for (case, after, ids, pages) in &cases {
            let page = Value::Array(ids.iter().map(|&id| roster.member(id)).collect());
            let mock = discord(roster, HashMap::from([(*after, page)])).await;
            let before = mirror(pool).await;
            assert_eq!(
                tick(kind, pool, &mock).await,
                Err(ErrorClass::Rest),
                "{job}: {case}"
            );
            let (cursors, others) = wire(&mock);
            mock.shutdown().await;
            assert_eq!(cursors, BUDGET_CURSORS[..*pages], "{job}: {case}");
            assert!(others.is_empty(), "{job}: {case}: {others:?}");
            assert_eq!(
                mirror(pool).await,
                before,
                "{job}: {case} changed the published website rows"
            );
        }
    }
    db.close().await.expect("drop disposable test database");
}
