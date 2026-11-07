use super::{claimed, identity, subject, TestDb};
use sqlx::{PgConnection, PgPool};
use std::time::Duration;
use two_bot_core::botlog::{assemble_botlog_page, validate_botlog_read, BotlogCursor};

const GUILD: &str = "123456789012345678";
// Old-image-shaped insertion, including supplied identities that must not bypass
// the fence. A shared intent also exercises multiple audit rows per intent.
const INSERT: &str = "INSERT INTO public.internal_action_log
    (audit_id, created_at, intent_id, phase, caller_hash, action, guild_id,
     target_id, response_code, http_status, evidence_code, affected)
    SELECT 1, '2000-01-01Z', intent_id, 'terminal', caller_hash, 'guild.add_member',
           guild_id, '234567890123456789', 'success', 200, 'executor', 1
    FROM public.internal_idempotency ORDER BY intent_id OFFSET $1 LIMIT 1 RETURNING
    (extract(epoch FROM created_at) * 1000000)::bigint, audit_id";

async fn insert(connection: &mut PgConnection, offset: i64) -> (i64, i64) {
    sqlx::query_as(INSERT)
        .bind(offset)
        .fetch_one(connection)
        .await
        .unwrap()
}

async fn fixture() -> TestDb {
    let db = TestDb::new().await;
    for key in [
        "order-fixture-a",
        "order-fixture-b",
        "order-fixture-c",
        "order-fixture-d",
    ] {
        claimed(
            db.store()
                .claim(&identity(key, "guild.add_member", b"order"), &subject())
                .await
                .unwrap(),
        );
    }
    db
}

async fn page(pool: &PgPool, after: BotlogCursor) -> (Vec<String>, String) {
    let body =
        serde_json::from_value(serde_json::json!({"after": after.encode(), "limit": 1})).unwrap();
    let request = validate_botlog_read(&body).unwrap();
    let rows = two_bot_core::internal_action_store::InternalActionStore::new(pool.clone())
        .list_botlog_page(GUILD, after.after_micros, after.after_audit_id, 1)
        .await
        .unwrap();
    let page = assemble_botlog_page(rows, &std::collections::HashMap::new(), &request);
    (
        page.entries.into_iter().map(|entry| entry.seq).collect(),
        page.next,
    )
}

async fn concurrent_writer(abort: bool) {
    let db = fixture().await;
    let other = db.independent_pool().await;
    let mut a = db.pool.begin().await.unwrap();
    let first = insert(&mut a, 0).await;
    let first_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *a)
        .await
        .unwrap();
    let mut b = other.begin().await.unwrap();
    let second_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *b)
        .await
        .unwrap();
    let writer = tokio::spawn(async move {
        let tuple = insert(&mut b, 1).await;
        b.commit().await.unwrap();
        tuple
    });
    // A database-observed blocking relationship, not a sleep or a scheduler
    // assumption, proves B reached the trigger but cannot escape ahead of A.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT $1 = ANY(pg_blocking_pids($2))")
                .bind(first_pid)
                .bind(second_pid)
                .fetch_one(&db.pool)
                .await
                .unwrap();
            if blocked {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second writer must block behind first");
    let (empty, next) = page(&db.pool, BotlogCursor::zero()).await;
    assert!(empty.is_empty());
    assert_eq!(next, BotlogCursor::zero().encode());
    assert!(!writer.is_finished());
    if abort {
        a.rollback().await.unwrap();
    } else {
        a.commit().await.unwrap();
    }
    let second = tokio::time::timeout(Duration::from_secs(3), writer)
        .await
        .unwrap()
        .unwrap();
    if abort {
        assert!(second >= first);
    } else {
        assert!(second > first);
    }
    let (entries, next) = page(&db.pool, BotlogCursor::zero()).await;
    assert_eq!(entries.len(), 1);
    let cursor = BotlogCursor::decode(&next).unwrap();
    let (resumed, last) = page(&db.pool, cursor).await;
    assert_eq!(resumed.len(), usize::from(!abort));
    if !abort {
        assert_ne!(resumed[0], entries[0]);
        assert_eq!(
            BotlogCursor::decode(&last).unwrap().after_audit_id,
            second.1
        );
    }
    assert!(page(&db.pool, BotlogCursor::decode(&last).unwrap())
        .await
        .0
        .is_empty());
    other.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn commit_fence_blocks_later_writer_and_resumes_without_loss() {
    tokio::time::timeout(Duration::from_secs(30), concurrent_writer(false))
        .await
        .unwrap();
}

#[tokio::test]
async fn aborted_writer_does_not_publish_or_strand_the_cursor() {
    tokio::time::timeout(Duration::from_secs(30), concurrent_writer(true))
        .await
        .unwrap();
}

#[tokio::test]
async fn allocation_uses_fence_arrival_not_transaction_start_and_clamps_clock() {
    let db = fixture().await;
    let other = db.independent_pool().await;
    let mut early = db.pool.begin().await.unwrap();
    sqlx::query("SELECT transaction_timestamp()")
        .execute(&mut *early)
        .await
        .unwrap();
    let mut later = other.begin().await.unwrap();
    let first = insert(&mut later, 0).await;
    later.commit().await.unwrap();
    // Simulate a clock retreat without replacing clock_timestamp: advance the
    // transactional high water past wall clock and require exact ties + IDs.
    sqlx::query("UPDATE public.internal_audit_order SET last_created_at = clock_timestamp() + interval '1 day'")
        .execute(&other).await.unwrap();
    let second = insert(&mut early, 1).await;
    let third = insert(&mut early, 2).await;
    assert!(second > first);
    assert_eq!(third, (second.0, second.1 + 1));
    early.commit().await.unwrap();
    // A conflict/no-op can consume a gap; a later successful row stays above it.
    sqlx::query("INSERT INTO public.internal_action_log SELECT * FROM public.internal_action_log LIMIT 1 ON CONFLICT DO NOTHING")
        .execute(&other).await.unwrap();
    let mut connection = other.acquire().await.unwrap();
    let fourth = insert(&mut connection, 3).await;
    assert!(fourth > third);
    drop(connection);
    other.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn activation_barrier_drains_old_inserts_and_preserves_their_tuples() {
    let db = fixture().await;
    // Reconstruct the pre-activation catalog, retaining the prepared real
    // helper definitions exactly as the historical bootstrap route does.
    sqlx::raw_sql("DROP TRIGGER internal_audit_allocate_order ON public.internal_action_log; DROP TABLE public.internal_audit_order")
        .execute(&db.pool).await.unwrap();
    let other = db.independent_pool().await;
    let mut old_writer = db.pool.begin().await.unwrap();
    let old_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *old_writer)
        .await
        .unwrap();
    let old_sql = INSERT.replace(
        "SELECT 1, '2000-01-01Z'",
        "SELECT 5000, clock_timestamp() + interval '1 day'",
    );
    let old_tuple: (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(old_sql))
        .bind(0_i64)
        .fetch_one(&mut *old_writer)
        .await
        .unwrap();
    let mut migration = other.begin().await.unwrap();
    let migration_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *migration)
        .await
        .unwrap();
    let installer = tokio::spawn(async move {
        sqlx::raw_sql(include_str!(
            "../../../cutover/migrations/0474_internal_audit_order.sql"
        ))
        .execute(&mut *migration)
        .await
        .unwrap();
        migration.commit().await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT $1 = ANY(pg_blocking_pids($2))")
                .bind(old_pid)
                .bind(migration_pid)
                .fetch_one(&db.pool)
                .await
                .unwrap();
            if blocked {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("activation must drain the old writer");
    old_writer.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), installer)
        .await
        .unwrap()
        .unwrap();
    let preserved: (i64, i64) = sqlx::query_as("SELECT (extract(epoch FROM created_at) * 1000000)::bigint, audit_id FROM public.internal_action_log WHERE audit_id = 5000")
        .fetch_one(&db.pool).await.unwrap();
    assert_eq!(preserved, old_tuple);
    let mut connection = other.acquire().await.unwrap();
    let allocated = insert(&mut connection, 1).await;
    assert_eq!(allocated, (old_tuple.0, old_tuple.1 + 1));
    drop(connection);
    other.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn allocator_refuses_missing_overflow_and_state_reset_but_allows_erasure() {
    let db = fixture().await;
    for statement in [
        "DELETE FROM public.internal_audit_order",
        "TRUNCATE public.internal_audit_order",
        "UPDATE public.internal_audit_order SET last_audit_id = 0",
        "UPDATE public.internal_audit_order SET last_created_at = '1970-01-01Z'",
        "UPDATE public.internal_audit_order SET singleton = false",
        "UPDATE public.internal_audit_order SET last_created_at = 'infinity'",
    ] {
        assert!(
            sqlx::raw_sql(sqlx::AssertSqlSafe(statement))
                .execute(&db.pool)
                .await
                .is_err(),
            "{statement}"
        );
    }
    let before: (i64, i64) = sqlx::query_as("SELECT (extract(epoch FROM last_created_at) * 1000000)::bigint, last_audit_id FROM public.internal_audit_order")
        .fetch_one(&db.pool).await.unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("UPDATE public.internal_audit_order SET last_audit_id = 9223372036854775807")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(sqlx::query(INSERT)
        .bind(0_i64)
        .execute(&mut *tx)
        .await
        .is_err());
    tx.rollback().await.unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    // Fault injection as the disposable DB owner, not a runtime bypass path.
    sqlx::raw_sql("ALTER TABLE public.internal_audit_order DISABLE TRIGGER internal_audit_order_guard; DELETE FROM public.internal_audit_order")
        .execute(&mut *tx).await.unwrap();
    assert!(sqlx::query(INSERT)
        .bind(0_i64)
        .execute(&mut *tx)
        .await
        .is_err());
    tx.rollback().await.unwrap();
    let after: (i64, i64) = sqlx::query_as("SELECT (extract(epoch FROM last_created_at) * 1000000)::bigint, last_audit_id FROM public.internal_audit_order")
        .fetch_one(&db.pool).await.unwrap();
    assert_eq!(after, before);
    assert!(
        sqlx::query("UPDATE public.internal_action_log SET audit_id = audit_id + 100")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM public.internal_action_log")
            .execute(&db.pool)
            .await
            .unwrap()
            .rows_affected()
            > 0
    );
    db.cleanup().await;
}
