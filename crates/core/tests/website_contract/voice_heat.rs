use super::{connect, Postgres, TestDatabase};

#[derive(sqlx::FromRow)]
struct HeatCell {
    dow: i32,
    hour: i32,
    avg_in_voice: Option<f64>,
    sample_days: i64,
    coverage: f64,
    measured_through: Option<String>,
    source: String,
}

type HeatTx<'a> = sqlx::Transaction<'a, Postgres>;

async fn heat_transaction(fixture: &TestDatabase) -> HeatTx<'_> {
    let mut tx = fixture.pool().begin().await.unwrap();
    // Pin now() across each test, and make non-UTC session time observable.
    sqlx::query("SET LOCAL TIME ZONE 'Asia/Kathmandu'")
        .execute(&mut *tx)
        .await
        .unwrap();
    // Only this disposable session installs the candidate. The released
    // contract remains unchanged until the publication slice.
    sqlx::raw_sql(
        "CREATE TEMP VIEW voice_heat_candidate AS
WITH bounds AS (
  SELECT
    (date_trunc('day', now() AT TIME ZONE 'UTC') - interval '28 days') AT TIME ZONE 'UTC' AS window_start,
    date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' AS window_end
),
scoped AS (
  SELECT h.hour_start_utc, h.samples, h.occupancy_sum, h.last_observed_at
  FROM voice_heat_hours h, bounds b
  WHERE h.hour_start_utc >= b.window_start
    AND h.hour_start_utc < b.window_end
    AND h.samples > 0
),
provenance AS (
  SELECT
    (SELECT count(DISTINCT (hour_start_utc AT TIME ZONE 'UTC')::date) FROM scoped) AS sample_days,
    (SELECT max(last_observed_at) FROM scoped) AS measured_through_ts
),
cells AS (
  SELECT
    ((EXTRACT(ISODOW FROM hour_start_utc AT TIME ZONE 'UTC'))::int - 1) AS dow,
    (EXTRACT(HOUR FROM hour_start_utc AT TIME ZONE 'UTC'))::int AS hour,
    sum(occupancy_sum)::double precision / NULLIF(sum(samples), 0) AS avg_in_voice,
    sum(samples)::double precision / 48.0 AS coverage
  FROM scoped
  GROUP BY 1, 2
),
grid AS (
  SELECT days.dow, hours.hour
  FROM generate_series(0, 6) AS days(dow)
  CROSS JOIN generate_series(0, 23) AS hours(hour)
)
SELECT
  grid.dow,
  grid.hour,
  cells.avg_in_voice,
  provenance.sample_days,
  COALESCE(cells.coverage, 0.0) AS coverage,
  web_v1._iso(provenance.measured_through_ts) AS measured_through,
  CASE WHEN provenance.sample_days >= 14
         AND (SELECT count(*) FROM cells WHERE coverage >= 0.8) = 168
       THEN 'measured' ELSE 'insufficient' END AS source
FROM grid
LEFT JOIN cells ON cells.dow = grid.dow AND cells.hour = grid.hour
CROSS JOIN provenance
ORDER BY grid.dow, grid.hour;",
    )
        .execute(&mut *tx)
        .await
        .unwrap();
    tx
}

async fn heat_grid(tx: &mut HeatTx<'_>) -> Vec<HeatCell> {
    let rows: Vec<HeatCell> =
        sqlx::query_as("SELECT * FROM voice_heat_candidate ORDER BY dow, hour")
            .fetch_all(&mut **tx)
            .await
            .unwrap();
    assert_eq!(rows.len(), 168);
    for (index, cell) in (0_i32..).zip(&rows) {
        assert_eq!((cell.dow, cell.hour), (index / 24, index % 24));
    }
    rows
}

fn assert_heat_header(rows: &[HeatCell], days: i64, source: &str, watermark: Option<&str>) {
    for cell in rows {
        assert_eq!(cell.sample_days, days);
        assert_eq!(cell.source, source);
        assert_eq!(cell.measured_through.as_deref(), watermark);
    }
}

async fn sql_heat(tx: &mut HeatTx<'_>, sql: &'static str) {
    sqlx::query(sql).execute(&mut **tx).await.unwrap();
}

#[tokio::test]
async fn voice_heat_keeps_gaps_distinct_from_valid_zero_and_observation_time() {
    let fixture = connect().await;
    let mut tx = heat_transaction(&fixture).await;
    for gaps in [false, true] {
        if gaps {
            sql_heat(&mut tx, "INSERT INTO voice_heat_hours (hour_start_utc) VALUES
                ((date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') - interval '3 hours')").await;
        }
        let rows = heat_grid(&mut tx).await;
        assert_heat_header(&rows, 0, "insufficient", None);
        assert!(rows
            .iter()
            .all(|c| c.avg_in_voice.is_none() && c.coverage == 0.0));
    }
    // Lower/upper time bounds must be UTC despite the session timezone.
    let midnight: time::OffsetDateTime =
        sqlx::query_scalar("SELECT date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    let hour = midnight - time::Duration::hours(4);
    for (at, samples, sum) in [
        (hour, 12, 0_i64),
        (hour - time::Duration::days(7), 6, 60),
        (midnight, 12, 999),
        (midnight - time::Duration::hours(28 * 24 + 1), 12, 999),
    ] {
        sqlx::query(
            "INSERT INTO voice_heat_hours VALUES ($1, $2, $3, $1::timestamptz + interval '5 min')",
        )
        .bind(at)
        .bind(samples)
        .bind(sum)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    for minutes in [5, 55] {
        let observation = hour + time::Duration::minutes(minutes);
        sqlx::query("UPDATE voice_heat_hours SET last_observed_at = $2 WHERE hour_start_utc = $1")
            .bind(hour)
            .bind(observation)
            .execute(&mut *tx)
            .await
            .unwrap();
        let watermark = observation
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let watermark = watermark.replace('Z', ".000Z");
        let rows = heat_grid(&mut tx).await;
        assert_heat_header(&rows, 2, "insufficient", Some(&watermark));
        let filled: Vec<_> = rows.iter().filter(|c| c.avg_in_voice.is_some()).collect();
        assert_eq!(filled.len(), 1);
        let cell = filled[0];
        assert_eq!(
            (cell.dow, cell.hour),
            (i32::from(hour.weekday().number_days_from_monday()), 20)
        );
        assert_eq!(cell.coverage, 18.0 / 48.0);
        assert_eq!(cell.avg_in_voice, Some(60.0 / 18.0));
    }
    sqlx::query("DELETE FROM voice_heat_hours WHERE hour_start_utc = $1")
        .bind(hour - time::Duration::days(7))
        .execute(&mut *tx)
        .await
        .unwrap();
    let rows = heat_grid(&mut tx).await;
    let real_zero: Vec<_> = rows
        .iter()
        .filter(|c| c.avg_in_voice == Some(0.0))
        .collect();
    assert_eq!(real_zero.len(), 1);
    assert_eq!(real_zero[0].coverage, 12.0 / 48.0);
    let last_zero = (hour + time::Duration::minutes(55))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
        .replace('Z', ".000Z");
    assert_heat_header(&rows, 1, "insufficient", Some(&last_zero));
    tx.rollback().await.unwrap();
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn voice_heat_requires_39_samples_in_every_cell() {
    let fixture = connect().await;
    let mut tx = heat_transaction(&fixture).await;
    sql_heat(
        &mut tx,
        "INSERT INTO voice_heat_hours
        SELECT t, 12, 36, t + interval '55 min'
        FROM generate_series(-672, -1) AS hours(n), LATERAL (
          SELECT (date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC')
                 + n * interval '1 hour' AS t
        ) bucket",
    )
    .await;
    let dense = heat_grid(&mut tx).await;
    let watermark = dense[0].measured_through.as_deref();
    assert!(watermark.is_some());
    assert_heat_header(&dense, 28, "measured", watermark);
    assert!(dense
        .iter()
        .all(|c| c.coverage == 1.0 && c.avg_in_voice == Some(3.0)));

    for (samples, source) in [(3, "measured"), (2, "insufficient")] {
        sqlx::query(
            "UPDATE voice_heat_hours SET samples = $1, occupancy_sum = 3 * $1
            WHERE hour_start_utc = (SELECT min(hour_start_utc) FROM voice_heat_hours)",
        )
        .bind(samples)
        .execute(&mut *tx)
        .await
        .unwrap();
        let rows = heat_grid(&mut tx).await;
        assert_heat_header(&rows, 28, source, watermark);
        assert_eq!(rows.iter().filter(|c| c.coverage < 1.0).count(), 1);
        assert!(rows
            .iter()
            .any(|c| c.coverage == f64::from(36 + samples) / 48.0));
    }

    sql_heat(
        &mut tx,
        "DELETE FROM voice_heat_hours WHERE
        (hour_start_utc AT TIME ZONE 'UTC')::time = '00:00' AND
        EXTRACT(ISODOW FROM hour_start_utc AT TIME ZONE 'UTC') = (
            SELECT EXTRACT(ISODOW FROM min(hour_start_utc) AT TIME ZONE 'UTC') FROM voice_heat_hours
        )",
    )
    .await;
    let rows = heat_grid(&mut tx).await;
    assert_heat_header(&rows, 28, "insufficient", watermark);
    assert_eq!(
        rows.iter()
            .filter(|c| c.coverage == 0.0 && c.avg_in_voice.is_none())
            .count(),
        1
    );
    assert_eq!(rows.iter().filter(|c| c.coverage == 1.0).count(), 167);

    sql_heat(&mut tx, "UPDATE voice_heat_hours
        SET samples = 0, occupancy_sum = 0, last_observed_at = NULL
        WHERE hour_start_utc < (date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') - interval '648 hours'").await;
    let rows = heat_grid(&mut tx).await;
    assert_heat_header(&rows, 27, "insufficient", watermark);
    assert_eq!(
        rows.iter().filter(|c| c.coverage == 36.0 / 48.0).count(),
        23
    );
    tx.rollback().await.unwrap();
    fixture.close().await.unwrap();
}
