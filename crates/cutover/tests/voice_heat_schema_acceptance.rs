//! Disposable acceptance for the dormant heat schema and rollback inventory.

use sqlx::{Pool, Postgres};
use two_bot_cutover::{rollback_delta::report, staging_migrate::MIGRATOR as CUTOVER};
use two_bot_store::migrate;
use two_bot_testsupport::TestDatabase;

type HeatShape = (Vec<(String, String, String, Option<String>)>, Vec<String>);

async fn voice_heat_shape(pool: &Pool<Postgres>) -> HeatShape {
    let columns: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT column_name::text, data_type::text, is_nullable::text, column_default::text
         FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = 'voice_heat_hours'
         ORDER BY ordinal_position",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        columns.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        [
            "hour_start_utc",
            "samples",
            "occupancy_sum",
            "last_observed_at"
        ],
        "only aggregate columns are allowed"
    );
    let constraints: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint
         WHERE conrelid = 'voice_heat_hours'::regclass ORDER BY conname",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(constraints
        .iter()
        .any(|c| c == "PRIMARY KEY (hour_start_utc)"));
    let version: String = sqlx::query_scalar("SELECT contract_version FROM web_contract_meta")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(version, "1.0");
    (columns, constraints)
}

async fn voice_heat_hours_constraints_bind_observation_to_samples(pool: &Pool<Postgres>) {
    for (samples, sum, seconds, violation) in [
        (12, 30_i64, Some(3599), None),
        (0, 0, None, None),
        (1, 0, Some(0), None),
        (5, 10, None, Some("voice_heat_observation_valid")),
        (0, 0, Some(0), Some("voice_heat_observation_valid")),
        (13, 10, Some(0), Some("voice_heat_samples_bounded")),
        (-1, 0, Some(0), Some("voice_heat_samples_bounded")),
        (5, -1, Some(0), Some("voice_heat_sum_nonneg")),
        (5, 10, Some(-1), Some("voice_heat_observation_valid")),
        (5, 10, Some(3600), Some("voice_heat_observation_valid")),
        (0, 3, None, Some("voice_heat_observation_valid")),
    ] {
        let mut tx = pool.begin().await.unwrap();
        let result = sqlx::query(
            "INSERT INTO voice_heat_hours (hour_start_utc, samples, occupancy_sum, last_observed_at)
             VALUES ('2026-01-01 00:00Z', $1, $2,
                     '2026-01-01 00:00Z'::timestamptz + $3::int * interval '1 second')",
        )
        .bind(samples)
        .bind(sum)
        .bind(seconds)
        .execute(&mut *tx)
        .await;
        match violation {
            Some(expected) => {
                let error = result.unwrap_err();
                let error = error.as_database_error().expect("CHECK violation");
                assert_eq!(error.code().as_deref(), Some("23514"));
                let actual = error.constraint();
                // Negative samples violate both checks; do not depend on which
                // constraint PostgreSQL reports first.
                assert!(
                    actual == Some(expected)
                        || (samples < 0 && actual == Some("voice_heat_observation_valid")),
                    "expected {expected} for {samples} samples, got {actual:?}"
                );
            }
            None => assert_eq!(result.unwrap().rows_affected(), 1),
        }
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn voice_heat_migrations_converge_from_either_chain() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("test bootstrap required");
    let mut reference = None;
    for cutover_first in [true, false] {
        let db = TestDatabase::create(&url, &sqlx::migrate::Migrator::DEFAULT)
            .await
            .expect("create empty fixture");
        let pool = db.pool();
        let summary = report(pool, "2026-01-01T00:00:00Z").await.unwrap();
        let heat = summary
            .tables
            .iter()
            .find(|t| t.table == "voice_heat_hours")
            .unwrap();
        assert_eq!(
            serde_json::to_string(heat).unwrap(),
            r#"{"table":"voice_heat_hours","status":"missing","reason":"table not present in this database"}"#
        );
        if cutover_first {
            CUTOVER.run(pool).await.unwrap();
        } else {
            migrate(pool).await.unwrap();
        }
        let shape = voice_heat_shape(pool).await;
        if let Some(reference) = &reference {
            assert_eq!(&shape, reference, "chain-only schemas must match");
        }
        voice_heat_hours_constraints_bind_observation_to_samples(pool).await;
        sqlx::query("INSERT INTO voice_heat_hours (hour_start_utc) VALUES ('2026-01-01 00:00Z')")
            .execute(pool)
            .await
            .unwrap();
        for _ in 0..2 {
            if cutover_first {
                migrate(pool).await.unwrap();
                CUTOVER.run(pool).await.unwrap();
            } else {
                CUTOVER.run(pool).await.unwrap();
                migrate(pool).await.unwrap();
            }
            assert_eq!(voice_heat_shape(pool).await, shape);
            let heat: (i32, i64, bool) = sqlx::query_as(
                "SELECT samples, occupancy_sum, last_observed_at IS NULL FROM voice_heat_hours",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            assert_eq!(
                heat,
                (0, 0, true),
                "the other chain must preserve existing rows"
            );
        }
        for (chain, sql) in [
            (
                &CUTOVER,
                "SELECT version, checksum FROM _sqlx_migrations WHERE success AND description = 'voice heat hours'",
            ),
            (
                &two_bot_store::MIGRATOR,
                "SELECT version, checksum FROM _two_bot_migrations WHERE success AND description = 'voice heat hours'",
            ),
        ] {
            let migration = chain
                .iter()
                .find(|m| m.description == "voice heat hours")
                .expect("each chain has a heat migration");
            let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(sql).fetch_all(pool).await.unwrap();
            assert_eq!(rows, [(migration.version, migration.checksum.to_vec())]);
        }
        reference = Some(shape);
        db.close().await.expect("verify teardown");
    }
}
