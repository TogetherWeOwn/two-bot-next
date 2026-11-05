//! Voice-heat store acceptance against a disposable, fully migrated database.
//!
//! Requires `TWO_TEST_DATABASE_URL` naming a disposable test bootstrap. Each
//! test owns a unique database created from the cutover migrations.

#![cfg(feature = "db")]

use time::{Duration, OffsetDateTime};
use two_bot_core::voice_heat_store::{
    count_stale_voice_heat, flush_hour_buckets, prune_voice_heat, truncate_to_hour, HourBucket,
    VOICE_HEAT_RETENTION_DAYS,
};
use two_bot_testsupport::TestDatabase;

async fn connect() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("TWO_TEST_DATABASE_URL is required (disposable test bootstrap)");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated test database")
}

fn bucket(hours_ago: i64, samples: i64, sum: i64) -> HourBucket {
    let hour_start = truncate_to_hour(OffsetDateTime::now_utc() - Duration::hours(hours_ago));
    HourBucket {
        hour_start,
        samples,
        occupancy_sum: sum,
        last_observed_at: (samples > 0).then_some(hour_start + Duration::minutes(55)),
    }
}

#[tokio::test]
async fn schema_contains_only_hourly_aggregate_columns() {
    let fixture = connect().await;
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = 'voice_heat_hours'
         ORDER BY ordinal_position",
    )
    .fetch_all(fixture.pool())
    .await
    .expect("aggregate schema");
    assert_eq!(
        columns,
        [
            "hour_start_utc",
            "samples",
            "occupancy_sum",
            "last_observed_at"
        ]
    );
    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn flush_is_idempotent_and_empty_writes_nothing() {
    let fixture = connect().await;
    let pool = fixture.pool().clone();

    assert_eq!(
        flush_hour_buckets(&pool, &[]).await.expect("empty write"),
        0
    );
    let buckets = vec![bucket(3, 12, 48), bucket(4, 11, 22)];
    assert_eq!(
        flush_hour_buckets(&pool, &buckets)
            .await
            .expect("first flush"),
        2
    );
    // A retried flush after the drain claims nothing, never double-counts.
    assert_eq!(
        flush_hour_buckets(&pool, &buckets)
            .await
            .expect("retry flush"),
        0
    );
    let changed = HourBucket {
        samples: 1,
        occupancy_sum: 99,
        last_observed_at: Some(buckets[0].hour_start + Duration::minutes(1)),
        ..buckets[0]
    };
    assert_eq!(
        flush_hour_buckets(&pool, &[changed])
            .await
            .expect("conflicting replay"),
        0,
        "a changed replay cannot overwrite the immutable count, sum or timestamp"
    );
    let mut stored: Vec<(i32, i64)> =
        sqlx::query_as("SELECT samples, occupancy_sum FROM voice_heat_hours")
            .fetch_all(&pool)
            .await
            .expect("read rows");
    stored.sort_unstable();
    assert_eq!(stored, vec![(11, 22), (12, 48)]);
    for bucket in &buckets {
        let observed: Option<OffsetDateTime> = sqlx::query_scalar(
            "SELECT last_observed_at FROM voice_heat_hours WHERE hour_start_utc = $1",
        )
        .bind(bucket.hour_start)
        .fetch_one(&pool)
        .await
        .expect("stored observation time");
        assert_eq!(observed, bucket.last_observed_at);
    }

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn prune_keeps_fresh_rows_and_counts_stale_ones() {
    let fixture = connect().await;
    let pool = fixture.pool().clone();

    flush_hour_buckets(&pool, &[bucket(40 * 24, 12, 30), bucket(10 * 24, 12, 24)])
        .await
        .expect("seed rows");
    assert_eq!(
        count_stale_voice_heat(&pool, VOICE_HEAT_RETENTION_DAYS)
            .await
            .expect("dry-run count"),
        1
    );
    // A refused horizon deletes nothing, not everything.
    assert!(
        prune_voice_heat(&pool, 0).await.is_err(),
        "zero-day horizon must refuse"
    );
    assert_eq!(
        prune_voice_heat(&pool, VOICE_HEAT_RETENTION_DAYS)
            .await
            .expect("prune"),
        1
    );
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM voice_heat_hours")
        .fetch_one(&pool)
        .await
        .expect("count kept");
    assert_eq!(kept, 1, "fresh rows survive");

    fixture.close().await.expect("drop test database");
}
