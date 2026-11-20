//! Voice heat persistence and retention on a disposable, fully migrated database.

#![cfg(feature = "db")]

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use two_bot_core::voice_heat::{truncate_to_hour, Accumulator, HourBucket};
use two_bot_core::voice_heat_store::{
    count_stale_voice_heat, flush_hour_buckets, prune_voice_heat, VoiceHeatStoreError,
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

#[tokio::test]
async fn retention_shares_strict_cutoff_and_rolls_back() {
    let fixture = connect().await;
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

fn bucket(hour_start: OffsetDateTime, samples: i64, sum: i64) -> HourBucket {
    let observed = hour_start + Duration::minutes(55);
    let mut accumulator = Accumulator::default();
    accumulator.record(observed, sum);
    let mut bucket = accumulator
        .completed(truncate_to_hour(observed) + Duration::hours(1))
        .pop()
        .expect("completed fixture bucket");
    // Allow exact cutoff offsets and synthetic aggregate values in store tests.
    bucket.hour_start = hour_start;
    bucket.samples = samples;
    bucket.occupancy_sum = sum;
    bucket.last_observed_at = (samples > 0).then_some(observed);
    bucket
}

type StoredBucket = (OffsetDateTime, i32, i64, Option<OffsetDateTime>);

fn stored_bucket(bucket: &HourBucket) -> StoredBucket {
    (
        bucket.hour_start,
        i32::try_from(bucket.samples).unwrap(),
        bucket.occupancy_sum,
        bucket.last_observed_at,
    )
}

async fn stored_buckets(pool: &PgPool) -> Vec<StoredBucket> {
    sqlx::query_as(
        "SELECT hour_start_utc, samples, occupancy_sum, last_observed_at
         FROM public.voice_heat_hours ORDER BY hour_start_utc DESC",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn seed_expired_hour(pool: &PgPool, hour_start: OffsetDateTime) {
    sqlx::query("INSERT INTO public.voice_heat_hours (hour_start_utc) VALUES ($1)")
        .bind(hour_start)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn flush_is_idempotent_and_empty_writes_nothing() {
    let fixture = connect().await;
    let pool = fixture.pool();
    let now = truncate_to_hour(OffsetDateTime::now_utc());
    let stale = now - Duration::days(40);
    seed_expired_hour(pool, stale).await;

    assert_eq!(flush_hour_buckets(pool, &[]).await.unwrap(), 0);
    assert_eq!(stored_buckets(pool).await, [(stale, 0, 0, None)]);
    let buckets = [
        bucket(now - Duration::hours(3), 12, 48),
        bucket(now - Duration::hours(4), 11, 22),
    ];
    assert_eq!(flush_hour_buckets(pool, &buckets).await.unwrap(), 2);
    assert_eq!(flush_hour_buckets(pool, &buckets).await.unwrap(), 0);

    let mut changed = buckets[0];
    changed.samples = 1;
    changed.occupancy_sum = 99;
    changed.last_observed_at = Some(changed.hour_start + Duration::minutes(1));
    assert_eq!(flush_hour_buckets(pool, &[changed]).await.unwrap(), 0);
    assert_eq!(
        stored_buckets(pool).await,
        buckets.each_ref().map(stored_bucket)
    );
    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn timed_out_backlog_retry_reaches_tail_without_rewriting_committed_prefix() {
    let fixture = connect().await;
    let pool = fixture.pool();
    let now = truncate_to_hour(OffsetDateTime::now_utc());
    let backlog: Vec<_> = (1..=335)
        .map(|h| bucket(now - Duration::hours(h), 12, h))
        .collect();
    // An earlier attempt committed this prefix before the timeout and retry.
    assert_eq!(flush_hour_buckets(pool, &backlog[..3]).await.unwrap(), 3);
    sqlx::raw_sql(
        "CREATE FUNCTION public.slow_heat_statement() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM pg_sleep(0.5); RETURN NULL; END $$;
         CREATE TRIGGER slow_heat_statement AFTER INSERT ON public.voice_heat_hours
         FOR EACH STATEMENT EXECUTE FUNCTION public.slow_heat_statement()",
    )
    .execute(pool)
    .await
    .unwrap();
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(100),
        flush_hour_buckets(pool, &backlog)
    )
    .await
    .is_err());

    let mut retry = backlog.clone();
    for row in &mut retry[..3] {
        row.occupancy_sum = 9999;
    }
    let written = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        flush_hour_buckets(pool, &retry),
    )
    .await
    .expect("bulk retry advances within budget")
    .unwrap();
    assert!(written <= 332, "the committed prefix cannot be rewritten");
    assert_eq!(
        stored_buckets(pool).await,
        backlog.iter().map(stored_bucket).collect::<Vec<_>>()
    );
    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn flush_prunes_and_filters_with_the_same_strict_cutoff() {
    let fixture = connect().await;
    sqlx::raw_sql(
        "CREATE SCHEMA flush_test_clock;
         CREATE FUNCTION flush_test_clock.now() RETURNS timestamptz
             LANGUAGE sql STABLE AS $$ SELECT TIMESTAMPTZ '2025-03-17 12:00:00+00' $$;",
    )
    .execute(fixture.pool())
    .await
    .unwrap();
    let pool = fixture
        .pool_with_search_path("flush_test_clock, pg_catalog, public")
        .await
        .unwrap();
    let now: OffsetDateTime = sqlx::query_scalar("SELECT now()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let cutoff = now - Duration::days(35);
    let stale = bucket(cutoff - Duration::microseconds(1), 1, 10);
    let boundary = bucket(cutoff, 1, 20);
    let fresh = bucket(cutoff + Duration::microseconds(1), 0, 0);
    seed_expired_hour(&pool, stale.hour_start).await;

    assert_eq!(
        flush_hour_buckets(&pool, &[stale, boundary, fresh])
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        stored_buckets(&pool).await,
        [stored_bucket(&fresh), stored_bucket(&boundary)]
    );
    fixture.close().await.expect("drop test database");
}

fn assert_db_error(error: VoiceHeatStoreError, code: &str) {
    assert_eq!(error.to_string(), "voice_heat_retention_db");
    let VoiceHeatStoreError::Db(sqlx::Error::Database(error)) = error else {
        panic!("expected a database error");
    };
    assert_eq!(error.code().as_deref(), Some(code));
}

#[tokio::test]
async fn prune_failure_prevents_insert() {
    let fixture = connect().await;
    let pool = fixture.pool();
    let now = truncate_to_hour(OffsetDateTime::now_utc());
    let stale = now - Duration::days(40);
    seed_expired_hour(pool, stale).await;
    sqlx::raw_sql(
        "CREATE FUNCTION public.refuse_heat_write() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF TG_OP = 'DELETE' THEN
                 RAISE EXCEPTION 'prune refused' USING ERRCODE = '55000';
             END IF;
             RAISE EXCEPTION 'insert must not be attempted' USING ERRCODE = '23514';
         END $$;
         CREATE TRIGGER refuse_heat_write BEFORE DELETE OR INSERT ON public.voice_heat_hours
         FOR EACH STATEMENT EXECUTE FUNCTION public.refuse_heat_write()",
    )
    .execute(pool)
    .await
    .unwrap();

    let fresh = bucket(now - Duration::hours(1), 1, 10);
    assert_db_error(
        flush_hour_buckets(pool, &[fresh]).await.unwrap_err(),
        "55000",
    );
    assert_eq!(stored_buckets(pool).await, [(stale, 0, 0, None)]);
    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn insert_failure_rolls_back_prune() {
    let fixture = connect().await;
    let pool = fixture.pool();
    let now = truncate_to_hour(OffsetDateTime::now_utc());
    let stale = now - Duration::days(40);
    seed_expired_hour(pool, stale).await;
    sqlx::raw_sql(
        "CREATE FUNCTION public.require_pruned_heat() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF EXISTS (SELECT 1 FROM public.voice_heat_hours) THEN
                 RAISE EXCEPTION 'stale hour still visible' USING ERRCODE = '55000';
             END IF;
             RETURN NULL;
         END $$;
         CREATE TRIGGER require_pruned_heat BEFORE INSERT ON public.voice_heat_hours
         FOR EACH STATEMENT EXECUTE FUNCTION public.require_pruned_heat()",
    )
    .execute(pool)
    .await
    .unwrap();

    // The trigger proves the stale row was deleted before this CHECK fails.
    let valid = bucket(now - Duration::hours(1), 1, 10);
    let invalid = bucket(now - Duration::hours(2), 1, -1);
    assert_db_error(
        flush_hour_buckets(pool, &[valid, invalid])
            .await
            .unwrap_err(),
        "23514",
    );
    assert_eq!(stored_buckets(pool).await, [(stale, 0, 0, None)]);
    fixture.close().await.expect("drop test database");
}
