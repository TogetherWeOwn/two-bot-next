#![cfg(feature = "db")]

//! Breaks caught: SQL `<` instead of `<=` skips exact cutoff equality;
//! rounding updated_at instead of flooring milliseconds delays eligibility;
//! ignoring the watermark reapplies decay early or charges two weeks at 42d.
//! These assertions execute the real store, not a substitute SQL predicate.

use two_bot_core::leveling_store as store;
use two_bot_core::parse_iso_millis;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const MEMBER: &str = "610000000000000000";
const SEASON: &str = "2026-Q3";
const UPDATED: &str = "2026-08-15T00:00:00.000900Z";

type Audit = (String, String, i32, Option<String>, bool);

async fn cutoff_case(
    initial_xp: i64,
    initial_weeks: i32,
    tick: &str,
    expected_age_ms: i64,
    expected: (u64, i64, i32, i32),
) {
    let (expected_count, expected_xp, expected_weeks, expected_loss) = expected;
    // Literal calendar clocks straddle 35d / 42d, not the .000900 seed's
    // microsecond instant. Rust's millisecond floor is the existing contract.
    assert_eq!(parse_iso_millis(UPDATED), Some(1_786_752_000_000));
    assert_eq!(
        parse_iso_millis(tick).unwrap() - 1_786_752_000_000,
        expected_age_ms
    );
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("cutoff acceptance requires disposable TWO_TEST_DATABASE_URL");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO member_season_xp
         (guild_id, season_id, member_id, seasonal_xp, decayed_weeks, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6::text::timestamptz)",
    )
    .bind(GUILD)
    .bind(SEASON)
    .bind(MEMBER)
    .bind(initial_xp)
    .bind(initial_weeks)
    .bind(UPDATED)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO member_levels
         (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at)
         VALUES ($1, $2, 1500, 300, 200, 1000, $3::text::timestamptz)",
    )
    .bind(GUILD)
    .bind(MEMBER)
    .bind(UPDATED)
    .execute(db.pool())
    .await
    .unwrap();
    let stored_fraction: String = sqlx::query_scalar(
        "SELECT to_char(updated_at AT TIME ZONE 'UTC', 'US') FROM member_season_xp",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let first = store::apply_seasonal_decay(db.pool(), GUILD, SEASON, tick).await;
    let second = store::apply_seasonal_decay(db.pool(), GUILD, SEASON, tick).await;
    let third = store::apply_seasonal_decay(db.pool(), GUILD, SEASON, tick).await;
    let seasonal: (i64, i32, bool) = sqlx::query_as(
        "SELECT seasonal_xp, decayed_weeks, updated_at = $4::text::timestamptz
         FROM member_season_xp WHERE guild_id = $1 AND season_id = $2 AND member_id = $3",
    )
    .bind(GUILD)
    .bind(SEASON)
    .bind(MEMBER)
    .bind(UPDATED)
    .fetch_one(db.pool())
    .await
    .unwrap();
    let lifetime: (i64, i64, i64, i64, bool) = sqlx::query_as(
        "SELECT xp, message_xp, voice_xp, imported_xp, updated_at = $3::text::timestamptz
         FROM member_levels WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(GUILD)
    .bind(MEMBER)
    .bind(UPDATED)
    .fetch_one(db.pool())
    .await
    .unwrap();
    let audits: Vec<Audit> = sqlx::query_as(
        "SELECT member_id, source, xp, decision, occurred_at = $2::text::timestamptz
         FROM xp_awards WHERE guild_id = $1 ORDER BY id",
    )
    .bind(GUILD)
    .bind(tick)
    .fetch_all(db.pool())
    .await
    .unwrap();
    // Close before behavioral assertions, including the intended mutant failure.
    db.close().await.unwrap();
    assert_eq!(
        stored_fraction, "000900",
        "fixture must retain sub-ms precision"
    );
    assert_eq!(
        first.unwrap(),
        expected_count,
        "cutoff at {tick}, watermark {initial_weeks}"
    );
    assert_eq!(
        second.unwrap(),
        0,
        "same-tick repeat is a truthful drain marker"
    );
    assert_eq!(third.unwrap(), 0, "a drained tick remains drained");
    assert_eq!(seasonal, (expected_xp, expected_weeks, true));
    assert_eq!(
        lifetime,
        (1500, 300, 200, 1000, true),
        "decay preserves every lifetime component and timestamp"
    );
    let expected_audits = if expected_count == 0 {
        vec![]
    } else {
        vec![(
            MEMBER.into(),
            "decay".into(),
            expected_loss,
            Some("seasonal_decay".into()),
            true,
        )]
    };
    assert_eq!(
        audits, expected_audits,
        "exact amount/decision/time, no duplicate or no-op audit"
    );
}

#[tokio::test]
async fn first_week_minus_one_millisecond_is_not_eligible() {
    cutoff_case(
        1000,
        0,
        "2026-09-18T23:59:59.999Z",
        3_023_999_999,
        (0, 1000, 0, 0),
    )
    .await;
}

#[tokio::test]
async fn first_week_exact_cutoff_is_eligible_with_fractional_updated_at() {
    cutoff_case(
        1000,
        0,
        "2026-09-19T00:00:00.000Z",
        3_024_000_000,
        (1, 900, 1, 100),
    )
    .await;
}

#[tokio::test]
async fn first_week_plus_one_millisecond_is_eligible() {
    cutoff_case(
        1000,
        0,
        "2026-09-19T00:00:00.001Z",
        3_024_000_001,
        (1, 900, 1, 100),
    )
    .await;
}

#[tokio::test]
async fn next_week_minus_one_millisecond_is_not_eligible() {
    cutoff_case(
        900,
        1,
        "2026-09-25T23:59:59.999Z",
        3_628_799_999,
        (0, 900, 1, 0),
    )
    .await;
}

#[tokio::test]
async fn next_week_exact_cutoff_is_eligible_with_fractional_updated_at() {
    cutoff_case(
        900,
        1,
        "2026-09-26T00:00:00.000Z",
        3_628_800_000,
        (1, 810, 2, 90),
    )
    .await;
}

#[tokio::test]
async fn next_week_plus_one_millisecond_is_eligible() {
    cutoff_case(
        900,
        1,
        "2026-09-26T00:00:00.001Z",
        3_628_800_001,
        (1, 810, 2, 90),
    )
    .await;
}
