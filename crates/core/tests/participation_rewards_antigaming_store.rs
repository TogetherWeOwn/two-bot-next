#![cfg(feature = "db")]

//! Store proofs restricted to the named disposable agent-testdb service.
//! Create an empty two_bot_test_local bootstrap as described in CONTRIBUTING.md,
//! then run:
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
//! python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test participation_rewards_antigaming_store
//! No DATABASE_URL or inherited application credentials are consulted.
//!
//! Failing-first contract for the anti-gaming design (test files only): daily
//! caps enforced inside the award transaction, capped attempts leaving 0-XP
//! audit rows without consuming extra state, seasonal upserts paralleling
//! lifetime XP, a decay job writing audit rows without touching lifetime XP,
//! and unchanged concurrent first-award races. Each test FAILS today because
//! the seasonal/daily-cap/decay schema and policy do not exist yet.

use two_bot_core::leveling::season_id_for_ms;
use two_bot_core::leveling_store as store;
use two_bot_core::parse_iso_millis;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const MEMBER: &str = "100000000000000001";
const OTHER: &str = "100000000000000002";

fn at(seconds: i64) -> String {
    two_bot_core::format_iso_millis(1_786_771_200_000 + seconds * 1000)
}

fn expected_season_for_fixture() -> String {
    let ms = parse_iso_millis(&at(0)).expect("fixture timestamp parses");
    let season = season_id_for_ms(ms);
    assert_eq!(
        season, "2026-Q3",
        "2026-08-15 fixture must resolve to 2026-Q3"
    );
    season
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("explicit DB test requires TWO_TEST_DATABASE_URL (test bootstrap only)");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated test database")
}

#[tokio::test]
async fn message_daily_cap_enforced_inside_tx() -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    let channel = "200000000000000001";
    // Eleven message awards inside one UTC day (61 s apart, ~11 min span).
    for index in 0..11 {
        let award = store::award_message(
            &pool,
            GUILD,
            MEMBER,
            &at(index * 61),
            Some(channel),
            None,
            false,
        )
        .await?;
        if index < 10 {
            assert!(award.awarded > 0, "award #{index} inside cap must grant XP");
        } else {
            assert_eq!(
                award.awarded, 0,
                "11th message award in one UTC day must cap to 0 XP"
            );
        }
    }
    let profile = store::profile(&pool, GUILD, MEMBER).await?;
    assert!(
        profile.message_xp <= 150,
        "message day total must stay within the 150 XP cap, got {}",
        profile.message_xp
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn voice_daily_cap_120() -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    let channel = "300000000000000001";
    // Twenty-five 60 s voice awards inside one UTC day: 25 x 5 XP uncapped.
    for index in 0..25 {
        store::award_voice(&pool, GUILD, MEMBER, 60, &at(index * 61), Some(channel)).await?;
    }
    let profile = store::profile(&pool, GUILD, MEMBER).await?;
    assert_eq!(
        profile.voice_xp, 120,
        "25 voice minutes in one UTC day must cap at exactly 120 XP"
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn capped_attempts_leave_zero_xp_audit_row() -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    let channel = "200000000000000001";
    for index in 0..11 {
        store::award_message(
            &pool,
            GUILD,
            MEMBER,
            &at(index * 61),
            Some(channel),
            None,
            false,
        )
        .await?;
    }
    // Capped attempts must write a 0-XP audit row (decision-marked) instead of
    // vanishing like cooldown rejections, without consuming extra state.
    let capped: Vec<(i32,)> = sqlx::query_as(
        "SELECT xp FROM xp_awards WHERE guild_id = $1 AND member_id = $2 AND xp = 0",
    )
    .bind(GUILD)
    .bind(MEMBER)
    .fetch_all(&pool)
    .await?;
    assert!(
        !capped.is_empty(),
        "capped attempts must leave a 0-XP xp_awards audit row"
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn seasonal_upsert_parallels_lifetime() -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    store::award_message(
        &pool,
        GUILD,
        MEMBER,
        &at(0),
        Some("200000000000000001"),
        None,
        false,
    )
    .await?;
    store::award_voice(
        &pool,
        GUILD,
        MEMBER,
        120,
        &at(61),
        Some("300000000000000001"),
    )
    .await?;
    let lifetime = store::profile(&pool, GUILD, MEMBER).await?;
    // The seasonal projection must mirror every lifetime award in the
    // event-time season (`season_id_for_ms(at(0))` = `2026-Q3`),
    // append-only, same transaction shape.
    let expected_season = expected_season_for_fixture();
    let seasonal: (i64,) = sqlx::query_as(
        "SELECT seasonal_xp FROM member_season_xp WHERE guild_id = $1 AND season_id = $2 AND member_id = $3",
    )
    .bind(GUILD)
    .bind(expected_season.as_str())
    .bind(MEMBER)
    .fetch_one(&pool)
    .await
    .expect("seasonal upsert must parallel lifetime XP");
    assert_eq!(
        seasonal.0 as u64, lifetime.xp,
        "seasonal XP must mirror lifetime XP after the same awards"
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn decay_job_writes_audit_rows_leaves_lifetime(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    let first = store::award_message(&pool, GUILD, MEMBER, &at(0), None, None, false).await?;
    assert_eq!(first.awarded, 15);
    // The Monday decay tick (seasonal only, 10 %/week past 28 d inactivity)
    // must write `source='decay'` audit rows and never touch lifetime XP.
    // `at(0)` stamps the seasonal row; 36 d later is one full overdue week
    // (grace 28 d + 7 d), so exactly one member must decay.
    let expected_season = expected_season_for_fixture();
    let decayed_members =
        store::apply_seasonal_decay(&pool, GUILD, &expected_season, &at(36 * 86400)).await?;
    assert_eq!(
        decayed_members, 1,
        "one inactive seasonal row must decay after 36 d"
    );
    let decayed: Vec<(i32,)> =
        sqlx::query_as("SELECT xp FROM xp_awards WHERE guild_id = $1 AND source = 'decay'")
            .bind(GUILD)
            .fetch_all(&pool)
            .await?;
    assert!(
        !decayed.is_empty(),
        "decay tick must write source='decay' audit rows"
    );
    let lifetime = store::profile(&pool, GUILD, MEMBER).await?;
    assert_eq!(
        lifetime.xp, first.total_xp,
        "decay must never touch lifetime XP"
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn concurrent_first_award_race_with_seasonal() -> Result<(), two_bot_core::LevelingStoreError>
{
    let fixture = database().await;
    let pool = fixture.pool().clone();
    // Same-member, same-instant first awards serialize on the cooldown row:
    // exactly one wins. The seasonal projection must mirror the winner.
    let instant = at(0);
    let (first, second) = tokio::join!(
        store::award_message(&pool, GUILD, OTHER, &instant, None, None, false),
        store::award_message(&pool, GUILD, OTHER, &instant, None, None, false),
    );
    let (first, second) = (first?, second?);
    assert!(
        (first.awarded == 15 && second.awarded == 0)
            || (first.awarded == 0 && second.awarded == 15),
        "concurrent first awards must leave exactly one 15 XP winner"
    );
    let lifetime = store::profile(&pool, GUILD, OTHER).await?;
    assert_eq!(lifetime.xp, 15);
    let expected_season = expected_season_for_fixture();
    let seasonal: (i64,) = sqlx::query_as(
        "SELECT seasonal_xp FROM member_season_xp WHERE guild_id = $1 AND season_id = $2 AND member_id = $3",
    )
    .bind(GUILD)
    .bind(expected_season.as_str())
    .bind(OTHER)
    .fetch_one(&pool)
    .await
    .expect("seasonal projection must mirror the race winner");
    assert_eq!(seasonal.0 as u64, 15);
    fixture.close().await.expect("drop test database");
    Ok(())
}
