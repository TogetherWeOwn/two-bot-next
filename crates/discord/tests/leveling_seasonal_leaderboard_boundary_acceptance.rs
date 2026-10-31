#![cfg(feature = "db")]
//! Acceptance (boundary): at a quarter boundary the season selected by the
//! routed `/leaderboard` handler changes deterministically, and a season
//! with no rows answers honestly empty.
//!
//! The companion file `leveling_seasonal_leaderboard_acceptance.rs` proves
//! the runtime RED through today's `handle_interaction`. Deterministic
//! boundary proof needs a testable clock, so this file pins exactly one
//! builder seam, shaped like the pipeline's existing `handle_at` event-time
//! convention (ISO-millis string, no new season convention):
//!
//! ```rust,ignore
//! impl LevelingRuntime {
//!     pub async fn handle_interaction_at(
//!         &self,
//!         interaction: &Interaction,
//!         handler: HandlerId,
//!         at: &str,
//!     ) -> Result<bool, LevelingRuntimeError>;
//! }
//! ```
//!
//! `handle_interaction` must delegate to it with the wall-clock instant, so
//! both entry points select `season_id_for_ms` of the same instant. The
//! season convention itself is unchanged: seasons start Monday 00:00 UTC
//! on/after the quarter's first day (2026-10-04 is still `2026-Q3`,
//! 2026-10-05 opens `2026-Q4`). `/rank` through the same seam stays
//! lifetime-based at every instant.
//!
//! Run (disposable test DB only):
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
//! python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
//!   --test leveling_seasonal_leaderboard_boundary_acceptance -- --ignored --test-threads=1
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
use two_bot_core::{parse_iso_millis, HandlerId, OnboardingGates, OnboardingMode};
use two_bot_discord::{ActionExecutor, LevelingRuntime};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 1545644954272137297;
/// Interaction author; Q3 leader, Q4 trailer, lifetime trailer.
const MEMBER: u64 = 100000000000000001;
/// Q4 leader and lifetime leader.
const OTHER: u64 = 100000000000000002;

const JUST_BEFORE_BOUNDARY: &str = "2026-10-04T23:59:59.999Z";
const AT_BOUNDARY: &str = "2026-10-05T00:00:00.000Z";
const EMPTY_SEASON_INSTANT: &str = "2027-02-14T12:00:00.000Z";

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

fn season_at(at: &str) -> String {
    season_id_for_ms(parse_iso_millis(at).expect("fixture instant parses"))
}

fn entry(member_id: u64, seasonal_xp: u64, rank: u64) -> LeaderboardEntry {
    LeaderboardEntry {
        member_id: member_id.to_string(),
        xp: seasonal_xp,
        level: level_for_xp(seasonal_xp),
        rank,
    }
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

    async fn seed(&self, member_id: u64, lifetime_xp: i64, seasons: &[(&str, i64)]) {
        sqlx::query(
            "INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ($1,$2,$3,$3,NOW())",
        )
        .bind(GUILD.to_string())
        .bind(member_id.to_string())
        .bind(lifetime_xp)
        .execute(&self.pool)
        .await
        .unwrap();
        for &(season, xp) in seasons {
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
    }

    async fn close(self) {
        self.fixture.close().await.expect("drop test database");
    }
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn routed_leaderboard_selected_season_flips_at_the_quarter_boundary() {
    // Fixture clock sanity: the pinned instants really straddle the
    // Monday-aligned season boundary.
    assert_eq!(season_at(JUST_BEFORE_BOUNDARY), "2026-Q3");
    assert_eq!(season_at(AT_BOUNDARY), "2026-Q4");
    let empty_season = season_at(EMPTY_SEASON_INSTANT);
    assert!(!["2026-Q3", "2026-Q4"].contains(&empty_season.as_str()));

    let db = TestDb::new().await;
    db.seed(MEMBER, 100, &[("2026-Q3", 800), ("2026-Q4", 10)])
        .await;
    db.seed(OTHER, 9_000, &[("2026-Q3", 20), ("2026-Q4", 900)])
        .await;
    let defer = || ScriptedResponse::status(204);
    let edit = || ScriptedResponse::json(200, r#"{"id":"99"}"#);
    let mock = MockRest::start(
        vec![
            defer(),
            edit(),
            defer(),
            edit(),
            defer(),
            edit(),
            defer(),
            edit(),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);

    for (at, expected_entries, expected_season) in [
        (
            JUST_BEFORE_BOUNDARY,
            vec![entry(MEMBER, 800, 1), entry(OTHER, 20, 2)],
            "2026-Q3",
        ),
        (
            AT_BOUNDARY,
            vec![entry(OTHER, 900, 1), entry(MEMBER, 10, 2)],
            "2026-Q4",
        ),
        (EMPTY_SEASON_INSTANT, Vec::new(), empty_season.as_str()),
    ] {
        let before = mock.requests().len();
        assert!(runtime
            .handle_interaction_at(&interaction("leaderboard"), HandlerId::Leaderboard, at)
            .await
            .unwrap());
        let requests = mock.requests();
        assert_eq!(
            requests.len(),
            before + 2,
            "one defer plus one completion edit"
        );
        let ack: Value = serde_json::from_slice(&requests[before].body).unwrap();
        assert_eq!(ack["type"], 5, "leveling acknowledges with a defer");
        assert_ne!(ack["data"]["flags"], 64, "leaderboard defers publicly");
        let body: Value = serde_json::from_slice(&requests[before + 1].body).unwrap();
        assert_eq!(
            body["content"],
            leaderboard_reply_for_season(&expected_entries, expected_season).content,
            "at {at} the routed /leaderboard must serve season {expected_season} only"
        );
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    }

    // The same seam at the boundary instant leaves /rank lifetime-based:
    // MEMBER holds 100 lifetime XP, rank 2 of 2 behind OTHER (9,000).
    let before = mock.requests().len();
    assert!(runtime
        .handle_interaction_at(&interaction("rank"), HandlerId::Rank, AT_BOUNDARY)
        .await
        .unwrap());
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        before + 2,
        "one defer plus one completion edit"
    );
    let ack: Value = serde_json::from_slice(&requests[before].body).unwrap();
    assert_eq!(ack["type"], 5, "leveling acknowledges with a defer");
    assert_eq!(ack["data"]["flags"], 64, "/rank defers ephemerally");
    let body: Value = serde_json::from_slice(&requests[before + 1].body).unwrap();
    assert_eq!(
        body["content"],
        rank_text("Global name", level_for_xp(100), Some(2), 2, 100),
        "/rank must stay lifetime-based at the season boundary"
    );
    mock.shutdown().await;
    db.close().await;
}
