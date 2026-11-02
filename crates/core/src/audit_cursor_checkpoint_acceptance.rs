//! Protected call-site acceptance for the private checkpoint transaction.
//! Public delivery validates history first, so malformed/forward floors never
//! reach this store boundary through the service. Keep this module private;
//! exposing a store capability solely for integration tests is unnecessary.
#![cfg(test)]

use super::*;
use two_bot_testsupport::TestDatabase;

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("guarded test bootstrap required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create disposable checkpoint fixture")
}

async fn recovery(
    pool: &PgPool,
    entry: &str,
    boundary: &str,
    saved: &str,
) -> Result<AuditClaim, AuditStoreError> {
    let store = AuditStore::new(pool);
    let event = AuditEvent::new(
        entry.to_owned(),
        crate::audit::AuditKind::MemberUpdate,
        "18446744073709551615".to_owned(),
        "2026-09-30T00:01:02.003Z".to_owned(),
    );
    store.record(&event, Some("1111")).await?;
    let send = store.claim(entry).await?.expect("send claim");
    assert_eq!(
        store.prepare_send(&send, boundary).await?,
        PrepareSend::Prepared
    );
    sqlx::query("UPDATE operational_audit_log SET delivery_lease_until = clock_timestamp() - interval '1 second', delivery_reconcile_before = $2 WHERE entry_id = $1")
        .bind(entry).bind(saved).execute(pool).await?;
    let claim = store.claim(entry).await?.expect("recovery claim");
    assert_eq!(claim.intent(), DeliveryIntent::Reconcile);
    Ok(claim)
}

async fn snapshot(pool: &PgPool, entry: &str) -> Result<serde_json::Value, sqlx::Error> {
    sqlx::query_scalar("SELECT to_jsonb(a) FROM operational_audit_log a WHERE entry_id = $1")
        .bind(entry)
        .fetch_one(pool)
        .await
}

/// Removing floor validation or using lexical direction must fail without
/// changing state, cursor, token, lease, generation or diagnostic evidence.
#[tokio::test]
async fn checkpoint_rejects_invalid_forward_and_below_boundary_floors_without_mutation(
) -> TestResult {
    let db = database().await;
    let store = AuditStore::new(db.pool());
    for (n, (saved, boundary, floor)) in [
        ("1000001", "5000", "1000002"),
        ("998401", "5000", "1000001"),
        ("1000001", "5000", "4999"),
        ("1000001", "0", ""),
        ("1000001", "0", "0"),
        ("1000001", "0", "01"),
        ("1000001", "0", "+1"),
        ("1000001", "0", "1 "),
        ("1000001", "0", "invalid"),
        ("1000001", "0", "18446744073709551616"),
    ]
    .into_iter()
    .enumerate()
    {
        let entry = format!("checkpoint-negative-{n}");
        let claim = recovery(db.pool(), &entry, boundary, saved).await?;
        let before = snapshot(db.pool(), &entry).await?;
        let result = store.bound_reconcile_scan(&claim, Some(floor)).await;
        assert!(
            matches!(result, Err(AuditStoreError::Invalid(_))),
            "floor {floor:?} must be refused"
        );
        let after = snapshot(db.pool(), &entry).await?;
        assert!(
            after == before,
            "refused floor {floor:?} mutated the authoritative row"
        );
        assert!(
            store.renew(&claim).await?,
            "refusal preserves the live capability"
        );
    }
    db.close().await?;
    Ok(())
}

/// The private checkpoint must accept numeric progress independently of the
/// service's page validation, then reject a numerical rewind across width.
#[tokio::test]
async fn checkpoint_accepts_numeric_progress_across_decimal_width() -> TestResult {
    let db = database().await;
    let store = AuditStore::new(db.pool());
    let entry = "checkpoint-decimal-width";
    let claim = recovery(db.pool(), entry, "0", "1000001").await?;
    let original = store.get(entry).await?.unwrap();
    assert_eq!(
        store.bound_reconcile_scan(&claim, Some("998401")).await?,
        ReconcileBound::Yielded
    );
    let saved = store.get(entry).await?.unwrap();
    assert_eq!(saved.reconcile_before.as_deref(), Some("998401"));
    assert_eq!(saved.state, DeliveryState::Pending);
    assert_eq!(saved.search_before, original.search_before);
    assert_eq!(saved.nonce, original.nonce);
    assert_eq!(saved.attempts, 1);
    let released: bool = sqlx::query_scalar("SELECT delivery_claim_token IS NULL AND delivery_lease_until IS NULL FROM operational_audit_log WHERE entry_id = $1")
        .bind(entry).fetch_one(db.pool()).await?;
    assert!(released, "checkpoint atomically releases ownership");
    db.close().await?;
    Ok(())
}
