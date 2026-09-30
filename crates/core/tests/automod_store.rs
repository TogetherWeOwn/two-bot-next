#![cfg(feature = "db")]

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::str::FromStr;
use two_bot_core::automod_runtime::{
    AutomodMatch, DeliveryKey, FunnelDisposition, MessageDeliveryKind, MessageSubject,
    STAGING_GUILD_ID,
};
use two_bot_core::automod_store::{
    AutomodStore, ClaimResult, CompletionKind, DeliveryClaim, StoredOutcome,
};
use two_bot_core::AutomodFilter;

// No DATABASE_URL fallback. Tests can reach only the named disposable container,
// or the explicitly opted-in CI service container (same user/empty password).
// The CI service container only provisions the default `postgres` database, so
// CI mode connects there; per-process schemas keep the suites isolated.
fn test_options() -> PgConnectOptions {
    let (host, database) = if std::env::var("AUTOMOD_CI_TESTDB").as_deref() == Ok("1") {
        assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
        ("127.0.0.1", "postgres")
    } else {
        ("agent-testdb", "agent_test")
    };
    PgConnectOptions::from_str(&format!("postgres://agent_test@{host}:5432/{database}")).unwrap()
}

fn key(id: &str, revision: &str, dry_run: bool) -> DeliveryKey {
    DeliveryKey {
        guild_id: STAGING_GUILD_ID.into(),
        message_id: id.into(),
        kind: MessageDeliveryKind::Create,
        dry_run,
        request_hash: revision.into(),
    }
}

fn subject(id: &str) -> MessageSubject {
    MessageSubject {
        guild_id: STAGING_GUILD_ID.into(),
        message_id: id.into(),
        channel_id: "222222222222222222".into(),
        author_id: "444444444444444444".into(),
    }
}

async fn acquire(store: &AutomodStore, key: &DeliveryKey) -> DeliveryClaim {
    let ClaimResult::Acquired(claim) = store.claim(key).await.unwrap() else {
        panic!("expected claim")
    };
    claim
}

#[tokio::test]
#[ignore = "requires agent-testdb, or the opt-in CI Postgres service container"]
async fn durable_claims_ledger_concurrency_and_recovery() {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(test_options())
        .await
        .expect("agent-testdb must be available; never substitute credentials or another database");
    let schema = format!("automod_test_{}", std::process::id());
    // This process owns this newly created schema; no IF NOT EXISTS hides a collision.
    // Audited identifier: constant ASCII prefix + numeric process ID only.
    sqlx::QueryBuilder::<sqlx::Postgres>::new("CREATE SCHEMA ")
        .push(&schema)
        .build()
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect_with(test_options().options([("search_path", schema.as_str())]))
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../cutover/migrations/0220_automod.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0221_automod_delivery_claims.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0222_automod_counted_claim.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0223_automod_preserved_match.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let store = AutomodStore::new(pool.clone());
    let at = "2026-09-30T00:00:00.000Z";

    // Concurrent duplicate delivery has exactly one winner; others cannot execute.
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            store.claim(&key("one", "revision", false)).await.unwrap()
        }));
    }
    let mut claims = Vec::new();
    let mut in_flight = 0;
    for task in tasks {
        match task.await.unwrap() {
            ClaimResult::Acquired(claim) => claims.push(claim),
            ClaimResult::InFlight => in_flight += 1,
            ClaimResult::Replayed(_) => panic!("not completed yet"),
            ClaimResult::Preserved(_, _) => panic!("no decision preserved yet"),
        }
    }
    assert_eq!(claims.len(), 1);
    assert_eq!(in_flight, 19);
    let claim = claims.pop().unwrap();
    let first = store
        .record_violation(&claim, &subject("one"), AutomodFilter::BadWords, at)
        .await
        .unwrap();
    assert_eq!(first.count, 1);
    assert!(first.inserted);
    assert!(store.mark_mutation_started(&claim).await.unwrap());
    assert!(
        !store.mark_mutation_started(&claim).await.unwrap(),
        "must not send the effects twice"
    );
    assert!(
        !store.release_unmutated(&claim).await.unwrap(),
        "uncertain delete keeps its claim"
    );
    let receipt = StoredOutcome {
        matched: true,
        deleted: true,
        outcome: CompletionKind::Deleted,
    };
    assert!(store.complete(&claim, &receipt).await.unwrap());
    assert!(!store.complete(&claim, &receipt).await.unwrap());
    assert!(
        matches!(store.claim(&key("one", "revision", false)).await.unwrap(), ClaimResult::Replayed(r) if r == receipt)
    );

    // Distinct edited deliveries can race, but the same message increments only once.
    let mut tasks = Vec::new();
    for i in 0..20 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            let claim = acquire(&store, &key("two", &format!("edit-{i}"), false)).await;
            store
                .record_violation(&claim, &subject("two"), AutomodFilter::AttachmentType, at)
                .await
                .unwrap()
        }));
    }
    let mut inserts = 0;
    for task in tasks {
        let row = task.await.unwrap();
        assert_eq!(row.count, 2);
        inserts += usize::from(row.inserted);
    }
    assert_eq!(inserts, 1);
    let claim3 = acquire(&store, &key("three", "create", false)).await;
    let third = store
        .record_violation(&claim3, &subject("three"), AutomodFilter::InviteLink, at)
        .await
        .unwrap();
    assert_eq!(third.count, 3);
    assert!(third.inserted);
    let delayed = acquire(&store, &key("one", "delayed-edit", false)).await;
    let duplicate = store
        .record_violation(&delayed, &subject("one"), AutomodFilter::BadWords, at)
        .await
        .unwrap();
    assert!(!duplicate.inserted);
    assert_eq!(duplicate.count, 3);
    let mut wrong_author = subject("one");
    wrong_author.author_id = "999999999999999999".into();
    assert!(store
        .record_violation(&delayed, &wrong_author, AutomodFilter::BadWords, at)
        .await
        .is_err());

    // Dry run is fenced at persistence, not just at the plan/executor seam.
    let dry = acquire(&store, &key("dry", "create", true)).await;
    assert!(!store.mark_mutation_started(&dry).await.unwrap());
    assert!(store
        .record_violation(&dry, &subject("dry"), AutomodFilter::BadWords, at)
        .await
        .is_err());
    assert!(store.complete(&dry, &receipt).await.is_err());
    assert!(store
        .complete(
            &dry,
            &StoredOutcome {
                matched: true,
                deleted: false,
                outcome: CompletionKind::DryRun
            }
        )
        .await
        .unwrap());
    assert!(matches!(
        store.claim(&key("dry", "create", true)).await.unwrap(),
        ClaimResult::Replayed(_)
    ));
    assert!(matches!(
        store.claim(&key("dry", "create", false)).await.unwrap(),
        ClaimResult::Acquired(_)
    ));

    // Safe release allows retry but the previous token cannot complete/delete the new claim.
    let safe_key = key("safe", "revision", false);
    let old = acquire(&store, &safe_key).await;
    assert!(store.release_unmutated(&old).await.unwrap());
    let new = acquire(&store, &safe_key).await;
    assert!(!store.complete(&old, &receipt).await.unwrap());
    assert!(!store.mark_mutation_started(&old).await.unwrap());
    assert!(!store.release_unmutated(&old).await.unwrap());
    assert!(store.mark_mutation_started(&new).await.unwrap());
    assert!(matches!(
        store.claim(&safe_key).await.unwrap(),
        ClaimResult::InFlight
    ));
    assert!(!store.release_unmutated(&new).await.unwrap());

    // A counted claim survives safe release: no REST effect or mutation fence
    // ran, but the ledger already holds this delivery. The retry replays the
    // retained claim instead of acquiring a fresh one that plans
    // AlreadyProcessed with no effects.
    let counted_key = key("counted", "revision", false);
    let counted = acquire(&store, &counted_key).await;
    let counted_record = store
        .record_violation(&counted, &subject("counted"), AutomodFilter::BadWords, at)
        .await
        .unwrap();
    assert!(counted_record.inserted);
    assert!(
        !store.release_unmutated(&counted).await.unwrap(),
        "counted ledger row keeps its claim for reconciliation"
    );
    assert!(matches!(
        store.claim(&counted_key).await.unwrap(),
        ClaimResult::InFlight
    ));
    assert!(store.mark_mutation_started(&counted).await.unwrap());
    let counted_receipt = StoredOutcome {
        matched: true,
        deleted: true,
        outcome: CompletionKind::Deleted,
    };
    assert!(store.complete(&counted, &counted_receipt).await.unwrap());
    assert!(matches!(
        store.claim(&counted_key).await.unwrap(),
        ClaimResult::Replayed(r) if r == counted_receipt
    ));
    // Counting twice on the same retained claim is refused: the ledger commit
    // and the counted fence are one transaction.
    assert!(store
        .record_violation(&counted, &subject("counted"), AutomodFilter::BadWords, at)
        .await
        .is_err());

    // A released pre-count decision replays after unrelated traffic sweeps
    // the mutable in-memory repeat history: preserve the inspect decision,
    // release before counting, then the same-revision retry rotates ownership
    // and replays the IDs/reason code without re-running the tracker.
    let preserved_key = key("preserved", "revision", false);
    let preserved_first = acquire(&store, &preserved_key).await;
    let preserved_match = AutomodMatch {
        subject: subject("preserved"),
        filter: AutomodFilter::BadWords,
        funnel: FunnelDisposition::CaptureOnly,
    };
    assert!(store
        .preserve_match(&preserved_first, &preserved_match)
        .await
        .unwrap());
    assert!(
        !store
            .preserve_match(&preserved_first, &preserved_match)
            .await
            .unwrap(),
        "preserving twice is idempotent"
    );
    assert!(store.release_unmutated(&preserved_first).await.unwrap());
    let ClaimResult::Preserved(retry, replayed) = store.claim(&preserved_key).await.unwrap() else {
        panic!("released decision must replay to the retry")
    };
    assert_eq!(replayed.subject, subject("preserved"));
    assert_eq!(replayed.filter, AutomodFilter::BadWords);
    assert_eq!(replayed.funnel, FunnelDisposition::CaptureOnly);
    let preserved_record = store
        .record_violation(&retry, &subject("preserved"), replayed.filter, at)
        .await
        .unwrap();
    assert!(preserved_record.inserted);
    assert!(store.mark_mutation_started(&retry).await.unwrap());
    let preserved_receipt = StoredOutcome {
        matched: true,
        deleted: true,
        outcome: CompletionKind::Deleted,
    };
    assert!(store.complete(&retry, &preserved_receipt).await.unwrap());
    assert!(matches!(
        store.claim(&preserved_key).await.unwrap(),
        ClaimResult::Replayed(r) if r == preserved_receipt
    ));
    // The released first token is stale: it cannot count, start, or settle.
    assert!(store
        .record_violation(
            &preserved_first,
            &subject("preserved"),
            AutomodFilter::BadWords,
            at
        )
        .await
        .is_err());
    assert!(!store.mark_mutation_started(&preserved_first).await.unwrap());
    assert!(!store
        .complete(&preserved_first, &preserved_receipt)
        .await
        .unwrap());

    // IDs/reason codes only: neither legacy table has message content columns.
    // Five processed messages: one/two/three plus the counted-reconciliation
    // and preserved-decision messages above, all from the same author.
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM automod_processed_messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 5);
    let (count,): (i32,) = sqlx::query_as(
        "SELECT violation_count FROM automod_violations WHERE user_id = '444444444444444444'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 5);
    let (text_columns,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM information_schema.columns WHERE table_schema = current_schema()
         AND column_name IN ('content', 'message_content', 'matched_excerpt')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(text_columns, 0);

    pool.close().await;
    sqlx::QueryBuilder::<sqlx::Postgres>::new("DROP SCHEMA ")
        .push(&schema)
        .push(" CASCADE")
        .build()
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
