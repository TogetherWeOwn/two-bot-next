//! Strict admission tests use only disposable databases and synthetic tokens.
#![cfg(feature = "db")]

use two_bot_core::send_admission::{
    AdmissionError, PgSendAdmission, SendAdmission, SendCooldown, IN_FLIGHT_LEASE_MS,
};
use two_bot_testsupport::TestDatabase;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test database URL required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn strict_fence_survives_restarts_and_old_dml() {
    let db = database().await;
    let gate = PgSendAdmission::new(db.pool().clone(), "strict-fixture").unwrap();
    drop(gate.admit_strict().await.unwrap());
    sqlx::query("UPDATE public.discord_send_admission SET in_flight_since_ms = (extract(epoch FROM clock_timestamp()) * 1000)::bigint - $1 - 1")
        .bind(IN_FLIGHT_LEASE_MS).execute(db.pool()).await.unwrap();
    let second = db.independent_pool().await.unwrap();
    let restarted = PgSendAdmission::new(second.clone(), "strict-fixture").unwrap();
    assert!(matches!(
        restarted.admit().await,
        Err(AdmissionError::Blocked)
    ));
    assert!(matches!(
        restarted.admit_strict().await,
        Err(AdmissionError::Blocked)
    ));
    let before: (i64, bool, Option<i64>) = sqlx::query_as("SELECT generation, in_flight, pending_settlement_generation FROM public.discord_send_admission")
        .fetch_one(&second).await.unwrap();
    // The actual pre-change reclaim and completion statements must fail atomically.
    let reclaim = sqlx::query("UPDATE public.discord_send_admission SET in_flight = TRUE, generation = generation + 1, in_flight_since_ms = (extract(epoch FROM clock_timestamp()) * 1000)::bigint WHERE token_key = $1 AND in_flight AND NOT indefinite AND hold_until_ms <= (extract(epoch FROM clock_timestamp()) * 1000)::bigint AND in_flight_since_ms <= (extract(epoch FROM clock_timestamp()) * 1000)::bigint - $2 RETURNING generation")
        .bind(sqlx::query_scalar::<_, String>("SELECT token_key FROM public.discord_send_admission").fetch_one(&second).await.unwrap())
        .bind(IN_FLIGHT_LEASE_MS).fetch_optional(&second).await;
    assert!(reclaim.is_err());
    let completion = sqlx::query("UPDATE public.discord_send_admission SET indefinite = indefinite OR $3, hold_until_ms = GREATEST(hold_until_ms, CASE WHEN $4::bigint IS NULL THEN 0 ELSE (extract(epoch FROM clock_timestamp()) * 1000)::bigint + $4 END), in_flight = FALSE WHERE token_key = $1 AND generation = $2 AND in_flight")
        .bind(sqlx::query_scalar::<_, String>("SELECT token_key FROM public.discord_send_admission").fetch_one(&second).await.unwrap())
        .bind(before.0).bind(false).bind(None::<i64>).execute(&second).await;
    assert!(completion.is_err());
    let after: (i64, bool, Option<i64>) = sqlx::query_as("SELECT generation, in_flight, pending_settlement_generation FROM public.discord_send_admission").fetch_one(&second).await.unwrap();
    assert_eq!(before, after);
    let ordinary = PgSendAdmission::new(second, "ordinary-fixture").unwrap();
    drop(ordinary.admit().await.unwrap());
    sqlx::query("UPDATE public.discord_send_admission SET in_flight_since_ms = (extract(epoch FROM clock_timestamp()) * 1000)::bigint - $1 - 1 WHERE pending_settlement_generation IS NULL")
        .bind(IN_FLIGHT_LEASE_MS).execute(db.pool()).await.unwrap();
    ordinary
        .admit()
        .await
        .unwrap()
        .complete(None)
        .await
        .unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn exact_settlement_preserves_holds_and_rejects_stale() {
    let db = database().await;
    let gate = PgSendAdmission::new(db.pool().clone(), "finite-fixture").unwrap();
    let permit = gate.admit_strict().await.unwrap();
    gate.extend(SendCooldown::FiniteMs(120_000)).await.unwrap();
    let before: i64 = sqlx::query_scalar("SELECT hold_until_ms FROM public.discord_send_admission")
        .fetch_one(db.pool())
        .await
        .unwrap();
    permit
        .complete(Some(SendCooldown::FiniteMs(1)))
        .await
        .unwrap();
    let row: (bool, Option<i64>, i64) = sqlx::query_as("SELECT in_flight, pending_settlement_generation, hold_until_ms FROM public.discord_send_admission").fetch_one(db.pool()).await.unwrap();
    assert!(!row.0 && row.1.is_none() && row.2 >= before);
    let indefinite = PgSendAdmission::new(db.pool().clone(), "indefinite-fixture").unwrap();
    indefinite
        .admit_strict()
        .await
        .unwrap()
        .complete(Some(SendCooldown::Indefinite))
        .await
        .unwrap();
    let held: (bool, Option<i64>) = sqlx::query_as("SELECT in_flight, pending_settlement_generation FROM public.discord_send_admission WHERE indefinite")
        .fetch_one(db.pool()).await.unwrap();
    assert_eq!(held, (false, None));
    assert!(matches!(
        indefinite.admit_strict().await,
        Err(AdmissionError::Blocked)
    ));
    let success = PgSendAdmission::new(db.pool().clone(), "success-fixture").unwrap();
    success
        .admit_strict()
        .await
        .unwrap()
        .complete(None)
        .await
        .unwrap();
    let stale = success.admit_strict().await.unwrap();
    // Model an explicitly authorized reconciliation before a replacement claim.
    sqlx::query("UPDATE public.discord_send_admission SET in_flight = FALSE, pending_settlement_generation = NULL WHERE pending_settlement_generation IS NOT NULL").execute(db.pool()).await.unwrap();
    let replacement = success.admit_strict().await.unwrap();
    assert_eq!(stale.complete(None).await, Err(AdmissionError::StaleClaim));
    assert!(matches!(
        success.admit().await,
        Err(AdmissionError::Blocked)
    ));
    success.extend(SendCooldown::Indefinite).await.unwrap();
    replacement.complete(None).await.unwrap();
    assert!(matches!(
        success.admit().await,
        Err(AdmissionError::Blocked)
    ));
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn strict_missing_schema_never_downgrades() {
    let db = database().await;
    sqlx::query(
        "ALTER TABLE public.discord_send_admission DROP COLUMN pending_settlement_generation",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let gate = PgSendAdmission::new(db.pool().clone(), "fixture").unwrap();
    assert!(matches!(
        gate.admit_strict().await,
        Err(AdmissionError::Storage)
    ));
    gate.admit().await.unwrap().complete(None).await.unwrap();
    db.close().await.unwrap();
}
