//! Only disposable agent-testdb databases and generated non-credential tokens.
#![cfg(feature = "db")]

use std::{sync::Arc, time::Duration};
use two_bot_core::send_admission::{AdmissionError, PgSendAdmission, SendAdmission, SendCooldown};
use two_bot_testsupport::TestDatabase;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test database URL required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn cross_pool_atomic_admission_and_restart_do_not_expire_claims() {
    let db = database().await;
    let second = db.independent_pool().await.unwrap();
    // An unrelated search_path must NOT give this credential a second lane.
    sqlx::query("CREATE SCHEMA unrelated")
        .execute(&second)
        .await
        .unwrap();
    let mut connection = second.acquire().await.unwrap();
    sqlx::query("SET search_path TO unrelated")
        .execute(&mut *connection)
        .await
        .unwrap();
    drop(connection);
    let a = Arc::new(PgSendAdmission::new(db.pool().clone(), "fixture-token").unwrap());
    let b = Arc::new(PgSendAdmission::new(second.clone(), "Bot fixture-token").unwrap());
    let (first, other) = tokio::join!(a.admit(), b.admit());
    assert_eq!(usize::from(first.is_ok()) + usize::from(other.is_ok()), 1);
    let (permit, denied) = if let Ok(permit) = first {
        (permit, other)
    } else {
        (other.unwrap(), first)
    };
    assert!(matches!(denied, Err(AdmissionError::Blocked)));
    // Simulate process loss: no destructor releases the durable claim.
    drop(permit);
    drop(a);
    second.close().await;
    let restarted = PgSendAdmission::new(db.pool().clone(), "fixture-token").unwrap();
    assert!(matches!(
        restarted.admit().await,
        Err(AdmissionError::Blocked)
    ));
    let unrelated = PgSendAdmission::new(db.pool().clone(), "different-token").unwrap();
    unrelated
        .admit()
        .await
        .unwrap()
        .complete(None)
        .await
        .unwrap();
    let occupied: i64 =
        sqlx::query_scalar("SELECT count(*) FROM public.discord_send_admission WHERE in_flight")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(occupied, 1);
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn cooldown_extension_is_monotonic_and_indefinite_is_sticky() {
    let db = database().await;
    let gate = PgSendAdmission::new(db.pool().clone(), "fixture-token").unwrap();
    let permit = gate.admit().await.unwrap();
    gate.extend(SendCooldown::FiniteMs(120_000)).await.unwrap();
    let long: i64 = sqlx::query_scalar("SELECT hold_until_ms FROM public.discord_send_admission")
        .fetch_one(db.pool())
        .await
        .unwrap();
    permit
        .complete(Some(SendCooldown::FiniteMs(1)))
        .await
        .unwrap();
    gate.extend(SendCooldown::FiniteMs(2)).await.unwrap();
    let after: i64 = sqlx::query_scalar("SELECT hold_until_ms FROM public.discord_send_admission")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(after >= long);
    assert!(matches!(gate.admit().await, Err(AdmissionError::Blocked)));
    gate.extend(SendCooldown::Indefinite).await.unwrap();
    gate.extend(SendCooldown::FiniteMs(0)).await.unwrap();
    let hold: bool = sqlx::query_scalar("SELECT indefinite FROM public.discord_send_admission")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(hold);
    assert!(matches!(gate.admit().await, Err(AdmissionError::Blocked)));
    let overflow = PgSendAdmission::new(db.pool().clone(), "overflow-fixture").unwrap();
    overflow
        .admit()
        .await
        .unwrap()
        .complete(Some(SendCooldown::FiniteMs(u64::MAX)))
        .await
        .unwrap();
    assert!(matches!(
        overflow.admit().await,
        Err(AdmissionError::Blocked)
    ));
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn cancellation_and_storage_failure_leave_lane_occupied() {
    let db = database().await;
    let separate = db.independent_pool().await.unwrap();
    let gate = Arc::new(PgSendAdmission::new(separate.clone(), "fixture-token").unwrap());
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let task_gate = gate.clone();
    let sender = tokio::spawn(async move {
        let _permit = task_gate.admit().await.unwrap();
        entered_tx.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    entered_rx.await.unwrap();
    sender.abort();
    assert!(sender.await.unwrap_err().is_cancelled());
    let restarted = PgSendAdmission::new(db.pool().clone(), "fixture-token").unwrap();
    assert!(matches!(
        restarted.admit().await,
        Err(AdmissionError::Blocked)
    ));

    let gate = PgSendAdmission::new(separate.clone(), "completion-fixture").unwrap();
    let permit = gate.admit().await.unwrap();
    separate.close().await;
    assert_eq!(
        permit.complete(Some(SendCooldown::Indefinite)).await,
        Err(AdmissionError::Storage)
    );
    assert!(matches!(gate.admit().await, Err(AdmissionError::Storage)));
    let restarted = PgSendAdmission::new(db.pool().clone(), "completion-fixture").unwrap();
    assert!(matches!(
        restarted.admit().await,
        Err(AdmissionError::Blocked)
    ));
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn stale_completion_cannot_release_a_new_generation_and_finite_hold_expires() {
    let db = database().await;
    let gate = PgSendAdmission::new(db.pool().clone(), "fixture-token").unwrap();
    let permit = gate.admit().await.unwrap();
    // Model explicit reconciliation/replacement, not a time-based lease expiry.
    sqlx::query("UPDATE public.discord_send_admission SET generation = generation + 1")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(permit.complete(None).await, Err(AdmissionError::StaleClaim));
    assert!(matches!(gate.admit().await, Err(AdmissionError::Blocked)));
    let finite = PgSendAdmission::new(db.pool().clone(), "finite-fixture").unwrap();
    finite
        .admit()
        .await
        .unwrap()
        .complete(Some(SendCooldown::FiniteMs(500)))
        .await
        .unwrap();
    assert!(matches!(finite.admit().await, Err(AdmissionError::Blocked)));
    tokio::time::sleep(Duration::from_millis(550)).await;
    finite.admit().await.unwrap().complete(None).await.unwrap();
    db.close().await.unwrap();
}
