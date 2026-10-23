#![cfg(feature = "db")]
//! Acceptance: `/leaderboard` serves the approved current-season board at
//! the command boundary (design AC3), while `/rank` stays lifetime-based.
//!
//! Every test here drives the real routed handler
//! (`LevelingRuntime::handle_interaction`, the same entry point the shared
//! command runtime calls) against a disposable migrated store and the mock
//! Discord REST capture. Only APIs that exist today are used, so the
//! failures below are runtime failures on the current implementation — the
//! handler still reads lifetime `member_levels` XP and renders without a
//! season label — not compile errors against an invented API. The
//! quarter-boundary clock seam is pinned separately in
//! `leveling_seasonal_leaderboard_boundary_acceptance.rs`.
//!
//! Current season is derived at runtime with the established convention
//! (`season_id_for_ms(now_ms)`), so the production handler may read the wall
//! clock exactly as other runtime code does. A run that happens to straddle
//! the Monday 00:00 UTC quarter boundary could seed the outgoing season;
//! that window is one midnight per quarter and the boundary file covers the
//! deterministic side.
//!
//! Pinned semantics for the builder:
//! - The board reads `member_season_xp` for `season_id_for_ms(now_ms)` only;
//!   prior-season rows and lifetime totals never appear, and an empty
//!   current season never falls back to lifetime.
//! - Ordering stays XP descending, member id ascending; the routed page is
//!   the existing default top 10.
//! - Each entry's `xp` and `level` derive from `seasonal_xp`, exactly as the
//!   store today derives both from the XP column it reads, and the reply is
//!   the existing pure `leaderboard_reply_for_season` render (explicit
//!   season label, mention-safe).
//! - `/rank`, lifetime totals and the guild fence are unchanged.
//!
//! Run (disposable test DB only):
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
//! python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
//!   --test leveling_seasonal_leaderboard_acceptance -- --ignored --test-threads=1
#[path = "support/rest_capture.rs"]
mod rest_capture;

use rest_capture::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use twilight_model::application::interaction::Interaction;
use two_bot_core::leveling::{
    leaderboard_reply_for_season, level_for_xp, rank_text, season_id_for_ms, LeaderboardEntry,
};
use two_bot_core::{HandlerId, OnboardingGates, OnboardingMode};
use two_bot_discord::{ActionExecutor, LevelingRuntime};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 1545644954272137297;
const FOREIGN_GUILD: u64 = 1111111111111111111;
/// Interaction author; also the divergent fixture's lifetime runner-up.
const MEMBER: u64 = 100000000000000001;
const SEASON_LEADER: u64 = 100000000000000002;
const TIE_LOW_ID: u64 = 100000000000000003;
const TIE_HIGH_ID: u64 = 100000000000000004;
const PRIOR_ONLY: u64 = 100000000000000005;
const MID_PACK: u64 = 100000000000000006;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis() as i64
}

fn current_season() -> String {
    season_id_for_ms(now_ms())
}

/// Season before `season` (`YYYY-QN` string math; Q1 wraps to prior Q4).
fn previous_season(season: &str) -> String {
    let (year, quarter) = season.split_once("-Q").expect("season id shape");
    let year: i64 = year.parse().expect("season year");
    let quarter: u32 = quarter.parse().expect("season quarter");
    if quarter > 1 {
        format!("{year:04}-Q{}", quarter - 1)
    } else {
        format!("{:04}-Q4", year - 1)
    }
}

fn user(id: u64) -> Value {
    json!({"id":id.to_string(),"username":"username","global_name":"Global name","discriminator":"0","avatar":null,"bot":false})
}

fn member() -> Value {
    json!({"user":user(MEMBER),"roles":[],"joined_at":null,"deaf":false,"mute":false,"pending":false,"flags":0,"permissions":"0"})
}

fn interaction(name: &str) -> Interaction {
    let data = json!({"id":"1","name":name,"type":1,"options":[]});
    serde_json::from_value(json!({"id":"7","application_id":"8","type":2,"token":"mock-callback","version":1,"guild_id":GUILD.to_string(),"member":member(),"locale":"en-US","data":data,"entitlements":[],"authorizing_integration_owners":{"0":GUILD.to_string()}})).unwrap()
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

struct TestDb {
    fixture: TestDatabase,
    pool: PgPool,
}

impl TestDb {
    async fn new() -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit DB test requires TWO_TEST_DATABASE_URL (test bootstrap only)");
        let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated test database; no credential fallback");
        let pool = fixture.pool().clone();
        Self { fixture, pool }
    }

    /// Lifetime row carried entirely by imported XP (the schema requires
    /// `xp = message_xp + voice_xp + imported_xp`).
    async fn seed_lifetime(&self, member_id: u64, xp: i64) {
        sqlx::query(
            "INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ($1,$2,$3,$3,NOW())",
        )
        .bind(GUILD.to_string())
        .bind(member_id.to_string())
        .bind(xp)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn seed_season(&self, member_id: u64, season: &str, xp: i64) {
        sqlx::query(
            "INSERT INTO member_season_xp (guild_id,season_id,member_id,seasonal_xp,updated_at) VALUES ($1,$2,$3,$4,NOW())",
        )
        .bind(GUILD.to_string())
        .bind(season)
        .bind(member_id.to_string())
        .bind(xp)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// Opposite lifetime / current-season orders plus prior-season rows.
    /// Lifetime: PRIOR_ONLY 9500 > MEMBER 9000 > TIE_LOW_ID 7000 > MID_PACK
    /// 200 > SEASON_LEADER 100 > TIE_HIGH_ID 50. Current season:
    /// SEASON_LEADER 500 > TIE_LOW_ID = TIE_HIGH_ID 300 > MID_PACK 200 >
    /// MEMBER 10, PRIOR_ONLY absent.
    async fn seed_divergent(&self, current: &str, prior: &str) {
        for (member_id, xp) in [
            (MEMBER, 9_000),
            (SEASON_LEADER, 100),
            (TIE_LOW_ID, 7_000),
            (TIE_HIGH_ID, 50),
            (PRIOR_ONLY, 9_500),
            (MID_PACK, 200),
        ] {
            self.seed_lifetime(member_id, xp).await;
        }
        for (member_id, xp) in [
            (MEMBER, 10),
            (SEASON_LEADER, 500),
            (TIE_LOW_ID, 300),
            (TIE_HIGH_ID, 300),
            (MID_PACK, 200),
        ] {
            self.seed_season(member_id, current, xp).await;
        }
        self.seed_season(PRIOR_ONLY, prior, 9_000).await;
        self.seed_season(MEMBER, prior, 8_000).await;
    }

    async fn close(self) {
        self.fixture.close().await.expect("drop test database");
    }
}

fn reply_bodies(mock: &MockRest) -> Vec<Value> {
    mock.requests()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn entry(member_id: u64, seasonal_xp: u64, rank: u64) -> LeaderboardEntry {
    LeaderboardEntry {
        member_id: member_id.to_string(),
        xp: seasonal_xp,
        level: level_for_xp(seasonal_xp),
        rank,
    }
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn routed_leaderboard_ranks_current_season_not_lifetime_or_prior_season() {
    let db = TestDb::new().await;
    let current = current_season();
    let prior = previous_season(&current);
    assert_ne!(current, prior);
    db.seed_divergent(&current, &prior).await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, r#"{"id":"99"}"#),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);

    assert!(runtime
        .handle_interaction(&interaction("leaderboard"), HandlerId::Leaderboard)
        .await
        .unwrap());
    let bodies = reply_bodies(&mock);
    assert_eq!(bodies.len(), 2, "one defer plus one completion edit");
    assert_eq!(bodies[0]["type"], 5, "leveling acknowledges with a defer");
    assert_ne!(
        bodies[0]["data"]["flags"], 64,
        "leaderboard defers publicly"
    );
    let expected = vec![
        entry(SEASON_LEADER, 500, 1),
        entry(TIE_LOW_ID, 300, 2),
        entry(TIE_HIGH_ID, 300, 3),
        entry(MID_PACK, 200, 4),
        entry(MEMBER, 10, 5),
    ];
    let content = bodies[1]["content"].as_str().unwrap();
    assert_eq!(
        content,
        leaderboard_reply_for_season(&expected, &current).content,
        "routed /leaderboard must render only current-season rows, XP-desc/member-id-asc, with the season label"
    );
    assert!(
        !content.contains(&PRIOR_ONLY.to_string()),
        "prior-season-only member must not appear on the current-season board"
    );
    // Public, mention-safe rendering is preserved on the completion edit.
    assert_eq!(bodies[1]["allowed_mentions"]["parse"], json!([]));
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn routed_leaderboard_empty_current_season_never_falls_back_to_lifetime() {
    let db = TestDb::new().await;
    let current = current_season();
    let prior = previous_season(&current);
    // Lifetime and prior-season rows exist; the current season is empty.
    db.seed_lifetime(MEMBER, 9_000).await;
    db.seed_lifetime(PRIOR_ONLY, 9_500).await;
    db.seed_season(PRIOR_ONLY, &prior, 9_000).await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, r#"{"id":"99"}"#),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);

    assert!(runtime
        .handle_interaction(&interaction("leaderboard"), HandlerId::Leaderboard)
        .await
        .unwrap());
    let bodies = reply_bodies(&mock);
    assert_eq!(bodies.len(), 2, "one defer plus one completion edit");
    assert_eq!(bodies[0]["type"], 5, "leveling acknowledges with a defer");
    assert_eq!(
        bodies[1]["content"],
        leaderboard_reply_for_season(&[], &current).content,
        "an empty current season must answer honestly empty, never lifetime totals"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn routed_leaderboard_display_size_is_top_ten_of_current_season() {
    let db = TestDb::new().await;
    let current = current_season();
    // Twelve members, seasonal XP 1200 down to 100 by id order; lifetime XP
    // runs the opposite way so the lifetime leader is seasonal rank 12.
    let first = MEMBER;
    for offset in 0..12_u64 {
        let member_id = first + offset;
        db.seed_season(member_id, &current, 1_200 - 100 * (offset as i64))
            .await;
        db.seed_lifetime(member_id, 100 + 100 * (offset as i64))
            .await;
    }
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, r#"{"id":"99"}"#),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);

    assert!(runtime
        .handle_interaction(&interaction("leaderboard"), HandlerId::Leaderboard)
        .await
        .unwrap());
    let bodies = reply_bodies(&mock);
    assert_eq!(bodies.len(), 2, "one defer plus one completion edit");
    assert_eq!(bodies[0]["type"], 5, "leveling acknowledges with a defer");
    let content = bodies[1]["content"].as_str().unwrap();
    let expected: Vec<LeaderboardEntry> = (0..10_u64)
        .map(|offset| entry(first + offset, 1_200 - 100 * offset, offset + 1))
        .collect();
    assert_eq!(
        content,
        leaderboard_reply_for_season(&expected, &current).content,
        "routed /leaderboard shows the top 10 current-season entries"
    );
    assert_eq!(content.lines().count(), 11, "header plus 10 rows");
    assert!(
        !content.contains(&(first + 11).to_string()),
        "the lifetime leader at seasonal rank 12 must not appear"
    );
    mock.shutdown().await;
    db.close().await;
}

/// Valid/control case: passes on the current implementation and must keep
/// passing after the seasonal switch. `/rank` stays lifetime-based, and the
/// guild fence still refuses foreign-guild leveling interactions.
#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn routed_rank_stays_lifetime_and_foreign_guild_stays_fenced() {
    let db = TestDb::new().await;
    let current = current_season();
    let prior = previous_season(&current);
    db.seed_divergent(&current, &prior).await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, r#"{"id":"99"}"#),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);

    for name in ["rank", "leaderboard"] {
        let mut foreign = interaction(name);
        foreign.guild_id = Some(twilight_model::id::Id::new(FOREIGN_GUILD));
        let handler = if name == "rank" {
            HandlerId::Rank
        } else {
            HandlerId::Leaderboard
        };
        assert!(
            !runtime.handle_interaction(&foreign, handler).await.unwrap(),
            "foreign-guild {name} must not be handled"
        );
    }
    assert!(mock.requests().is_empty(), "foreign guild sends no reply");

    assert!(runtime
        .handle_interaction(&interaction("rank"), HandlerId::Rank)
        .await
        .unwrap());
    let bodies = reply_bodies(&mock);
    assert_eq!(bodies.len(), 2, "one defer plus one completion edit");
    assert_eq!(bodies[0]["type"], 5, "leveling acknowledges with a defer");
    assert_eq!(bodies[0]["data"]["flags"], 64, "/rank defers ephemerally");
    // MEMBER holds 9,000 lifetime XP: rank 2 of 6 behind PRIOR_ONLY (9,500),
    // whatever the seasonal board shows.
    assert_eq!(
        bodies[1]["content"],
        rank_text("Global name", level_for_xp(9_000), Some(2), 6, 9_000),
        "/rank must keep reading lifetime totals"
    );
    mock.shutdown().await;
    db.close().await;
}
