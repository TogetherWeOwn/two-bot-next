#![cfg(feature = "db")]

//! Acceptance: seasonal decay pages through a guild's season instead of
//! loading it whole (`apply_seasonal_decay` fetch-all, hot-path bounds item
//! B1).
//!
//! Protected test-first handoff: the paired Builder pages the decay read
//! (LIMIT + keyset, or an equivalent chunked cursor) without editing this
//! file. Contract pinned here:
//!
//! - One `apply_seasonal_decay` call decays at most 25 members and returns
//!   the count decayed on that pass; it returns 0 only when no decayable row
//!   remains. Zero is the drain marker: a nonzero return obliges another
//!   pass, and repeat-until-0 drains every eligible member.
//! - Each member decays exactly once per tick (one `source='decay'` audit row
//!   carrying the decayed amount); decay math is unchanged (28-day grace,
//!   then 10 % multiplicative per full overdue week past the
//!   `decayed_weeks` watermark); lifetime XP is never touched.
//!
//! RED status on the unmodified tree: `single_pass_decays_at_most_the_cap`
//! fails behaviorally (one call decays all 30 seeded members, want 25).
//! Every other test in this file passes before and after the change.
//!
//! Database tests use the authorized disposable test database only
//! (`TWO_TEST_DATABASE_URL`, `agent-testdb` principal): they skip with a
//! diagnostic when the URL is absent locally and fail closed in CI.

use two_bot_core::leveling::{
    season_id_for_ms, seasonal_decay_new_weeks, seasonal_xp_after_decay_weeks,
    SEASONAL_DECAY_GRACE_DAYS,
};
use two_bot_core::leveling_store as store;
use two_bot_core::{format_iso_millis, parse_iso_millis};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const SEED_XP: i64 = 1000;
const DECAY_CAP: u64 = 25;
const OVER_CAP: usize = 30;

fn at(seconds: i64) -> String {
    format_iso_millis(1_786_771_200_000 + seconds * 1000)
}

fn season_for_fixture() -> String {
    let ms = parse_iso_millis(&at(0)).expect("fixture timestamp parses");
    let season = season_id_for_ms(ms);
    assert_eq!(
        season, "2026-Q3",
        "2026-08-15 fixture must resolve to 2026-Q3"
    );
    season
}

fn member(i: usize) -> String {
    format!("610000000000{i:06}")
}

/// The Monday-tick clock: 36 days after the last award is one full overdue
/// week (28-day grace + 7 days), so every seeded member owes exactly one
/// week of decay.
fn tick_iso() -> String {
    at(36 * 86400)
}

fn bootstrap_url(test: &str) -> Option<String> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure decay-paging acceptance"
            );
            eprintln!("SKIP {test}: TWO_TEST_DATABASE_URL not set");
            None
        }
    }
}

async fn fixture(bootstrap: &str) -> TestDatabase {
    TestDatabase::create(bootstrap, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap()
}

async fn seed_season(pool: &sqlx::PgPool, season: &str, members: usize) {
    for i in 0..members {
        sqlx::query(
            "INSERT INTO member_season_xp
             (guild_id, season_id, member_id, seasonal_xp, decayed_weeks, updated_at)
             VALUES ($1, $2, $3, $4, 0, $5::text::timestamptz)",
        )
        .bind(GUILD)
        .bind(season)
        .bind(member(i))
        .bind(SEED_XP)
        .bind(at(0))
        .execute(pool)
        .await
        .unwrap();
    }
}

#[test]
fn decay_math_pins() {
    assert_eq!(SEASONAL_DECAY_GRACE_DAYS, 28, "grace stays 28 days");
    assert_eq!(seasonal_xp_after_decay_weeks(1000, 0), 1000);
    assert_eq!(seasonal_xp_after_decay_weeks(1000, 1), 900);
    assert_eq!(seasonal_xp_after_decay_weeks(1000, 2), 810);
    assert_eq!(
        seasonal_decay_new_weeks(28, 0),
        0,
        "grace edge owes nothing"
    );
    assert_eq!(seasonal_decay_new_weeks(36, 0), 1, "36 days owes one week");
    assert_eq!(
        seasonal_decay_new_weeks(36, 1),
        0,
        "a rerun of the same tick applies zero"
    );
}

#[tokio::test]
async fn single_pass_decays_at_most_the_cap() {
    let Some(bootstrap) = bootstrap_url("decay single pass") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let season = season_for_fixture();
    seed_season(db.pool(), &season, OVER_CAP).await;
    let first = store::apply_seasonal_decay(db.pool(), GUILD, &season, &tick_iso())
        .await
        .expect("decay tick runs");
    assert_eq!(
        first, DECAY_CAP,
        "one pass decays exactly the 25-row cap, got {first} of {OVER_CAP}"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn repeat_passes_drain_every_member_exactly_once() {
    let Some(bootstrap) = bootstrap_url("decay drain") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let season = season_for_fixture();
    seed_season(db.pool(), &season, OVER_CAP).await;
    let mut total = 0u64;
    for _ in 0..5 {
        let pass = store::apply_seasonal_decay(db.pool(), GUILD, &season, &tick_iso())
            .await
            .expect("decay pass runs");
        total += pass;
        if pass == 0 {
            break;
        }
    }
    assert_eq!(total, OVER_CAP as u64, "repeat-until-0 drains all 30");
    let rows: Vec<(String, i64, i32)> = sqlx::query_as(
        "SELECT member_id, seasonal_xp, decayed_weeks FROM member_season_xp
         WHERE guild_id = $1 AND season_id = $2",
    )
    .bind(GUILD)
    .bind(&season)
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), OVER_CAP);
    for (id, xp, weeks) in &rows {
        assert_eq!(*xp, 900, "member {id} decays 1000 -> 900");
        assert_eq!(*weeks, 1, "member {id} advances one watermark week");
    }
    let audits: Vec<(String, i32)> = sqlx::query_as(
        "SELECT member_id, xp FROM xp_awards WHERE guild_id = $1 AND source = 'decay'",
    )
    .bind(GUILD)
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(audits.len(), OVER_CAP, "one decay audit row per member");
    for (id, xp) in &audits {
        assert_eq!(*xp, 100, "member {id} audit carries the decayed 100");
    }
    let lifetime: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM member_levels")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(lifetime, 0, "decay never touches lifetime XP");
    db.close().await.unwrap();
}

#[tokio::test]
async fn at_cap_single_pass_completes() {
    let Some(bootstrap) = bootstrap_url("decay at cap") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let season = season_for_fixture();
    seed_season(db.pool(), &season, DECAY_CAP as usize).await;
    let first = store::apply_seasonal_decay(db.pool(), GUILD, &season, &tick_iso())
        .await
        .expect("decay tick runs");
    assert_eq!(first, DECAY_CAP, "exactly 25 decay in one pass");
    let second = store::apply_seasonal_decay(db.pool(), GUILD, &season, &tick_iso())
        .await
        .expect("second pass runs");
    assert_eq!(second, 0, "nothing remains after a full pass");
    db.close().await.unwrap();
}

#[tokio::test]
async fn empty_season_decays_nothing() {
    let Some(bootstrap) = bootstrap_url("decay empty season") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let season = season_for_fixture();
    let decayed = store::apply_seasonal_decay(db.pool(), GUILD, &season, &tick_iso())
        .await
        .expect("decay tick runs");
    assert_eq!(decayed, 0, "empty season is already drained");
    db.close().await.unwrap();
}
