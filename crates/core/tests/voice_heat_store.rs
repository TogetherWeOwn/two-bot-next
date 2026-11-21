//! Retention acceptance on a disposable, fully migrated database.

#![cfg(feature = "db")]

use time::{Duration, OffsetDateTime};
use two_bot_core::voice_heat_store::{
    count_stale_voice_heat, prune_voice_heat, VoiceHeatStoreError, VOICE_HEAT_RETENTION_DAYS,
};
use two_bot_testsupport::TestDatabase;

#[tokio::test]
async fn retention_shares_strict_cutoff_and_rolls_back() {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("TWO_TEST_DATABASE_URL is required (disposable test bootstrap)");
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated test database");
    let pool = fixture.pool();
    let mut tx = pool.begin().await.unwrap();
    // Freeze now() in this transaction so the window always crosses spring-forward.
    sqlx::raw_sql(
        "CREATE SCHEMA retention_test_clock;
         CREATE FUNCTION retention_test_clock.now() RETURNS timestamptz
             LANGUAGE sql STABLE AS $$ SELECT TIMESTAMPTZ '2025-03-17 12:00:00+00' $$;
         SET LOCAL search_path = retention_test_clock, pg_catalog, public;
         SET LOCAL TIME ZONE 'America/New_York';",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let (now, calendar_cutoff): (OffsetDateTime, OffsetDateTime) =
        sqlx::query_as("SELECT now(), now() - interval '35 days'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    // A calendar-day cutoff would delete even the two retained buckets.
    let cutoff = now - Duration::days(35);
    assert_eq!(calendar_cutoff, cutoff + Duration::hours(1));
    let stale = cutoff - Duration::microseconds(1);
    let fresh = cutoff + Duration::microseconds(1);
    sqlx::query("INSERT INTO public.voice_heat_hours (hour_start_utc) VALUES ($1), ($2), ($3)")
        .bind(stale)
        .bind(cutoff)
        .bind(fresh)
        .execute(pool)
        .await
        .unwrap();

    assert!(matches!(
        count_stale_voice_heat(&mut *tx, 0).await,
        Err(VoiceHeatStoreError::InvalidHorizon)
    ));
    assert!(matches!(
        prune_voice_heat(&mut *tx, 0).await,
        Err(VoiceHeatStoreError::InvalidHorizon)
    ));
    let predicted = count_stale_voice_heat(&mut *tx, VOICE_HEAT_RETENTION_DAYS)
        .await
        .unwrap();
    assert_eq!(predicted, 1);
    assert_eq!(
        prune_voice_heat(&mut *tx, VOICE_HEAT_RETENTION_DAYS)
            .await
            .unwrap(),
        predicted
    );
    let read_hours = "SELECT hour_start_utc FROM public.voice_heat_hours ORDER BY hour_start_utc";
    let kept: Vec<OffsetDateTime> = sqlx::query_scalar(read_hours)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(kept, [cutoff, fresh]);

    tx.rollback().await.unwrap();
    let restored: Vec<OffsetDateTime> = sqlx::query_scalar(read_hours)
        .fetch_all(pool)
        .await
        .unwrap();
    assert_eq!(restored, [stale, cutoff, fresh]);
    fixture.close().await.expect("drop test database");
}
