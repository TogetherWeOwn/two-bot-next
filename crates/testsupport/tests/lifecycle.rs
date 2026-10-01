use std::time::Duration;
use two_bot_testsupport::TestDatabase;

async fn assert_statement_timeout(pool: &sqlx::PgPool) {
    let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(timeout, "5s");
    // The other transaction holds this row, so execution (not pool acquisition)
    // must fail with the server's query-cancelled code within the shared bound.
    let error = tokio::time::timeout(
        Duration::from_secs(8),
        sqlx::query("UPDATE fixture_rows SET value = 'blocked' WHERE id = 1").execute(pool),
    )
    .await
    .expect("statement timeout must bound a held-lock query")
    .expect_err("locked statement must time out");
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("57014")
    );
}

async fn process_database_count(pool: &sqlx::PgPool, prefix: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM pg_database WHERE starts_with(datname::text, $1)")
        .bind(prefix)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn databases_are_migrated_isolated_and_removed_even_after_setup_failure() {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => return,
        Err(error) => panic!("invalid test bootstrap configuration: {error}"),
    };
    let migrations = sqlx::migrate!("./tests/migrations");
    let (a, b) = tokio::join!(
        TestDatabase::create(&url, &migrations),
        TestDatabase::create(&url, &migrations),
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_ne!(a.name(), b.name());
    let (migration_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(a.pool())
        .await
        .unwrap();
    assert_eq!(migration_count, 1);
    sqlx::query("INSERT INTO fixture_rows VALUES (1, 'only A')")
        .execute(a.pool())
        .await
        .unwrap();
    let peer = a.independent_pool().await.unwrap();
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM fixture_rows")
        .fetch_one(&peer)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM fixture_rows")
        .fetch_one(b.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);

    let mut lock = a.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM fixture_rows WHERE id = 1 FOR UPDATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    tokio::join!(
        assert_statement_timeout(a.pool()),
        assert_statement_timeout(&peer),
    );
    lock.rollback().await.unwrap();
    // Leave the independent pool open: the fixture owns its teardown too.
    let a_name = a.name().to_owned();
    a.close().await.unwrap();
    assert!(peer.is_closed());
    let (exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(a_name)
            .fetch_one(b.pool())
            .await
            .unwrap();
    assert!(!exists);

    // Underscores and the PID separator are literal, not LIKE wildcards. Check
    // deliberately overlapping synthetic PIDs before counting real databases.
    let matches: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM (VALUES ('two_bot_test_12_ab_0'),
         ('two_bot_test_123_ab_0'), ('twoXbotXtestX12_ab_0')) AS names(name)
         WHERE starts_with(name, $1)",
    )
    .bind("two_bot_test_12_")
    .fetch_all(b.pool())
    .await
    .unwrap();
    assert_eq!(matches, ["two_bot_test_12_ab_0"]);
    // Count only this integration-test process, not concurrent suites.
    let prefix = format!("two_bot_test_{:x}_", std::process::id());
    let before = process_database_count(b.pool(), &prefix).await;
    let broken = sqlx::migrate!("./tests/broken_migrations");
    assert!(TestDatabase::create(&url, &broken).await.is_err());
    let after = process_database_count(b.pool(), &prefix).await;
    assert_eq!(before, after, "migration failure leaked a database");

    // Reproduce the eight-way lifecycle contention from the core store suite,
    // while leaving peer pools open. Teardown must remain verified, not skipped
    // or retried after the existing five-second SQL deadline.
    let mut creating = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let url = url.clone();
        creating.spawn(async move {
            let migrations = sqlx::migrate!("./tests/migrations");
            TestDatabase::create(&url, &migrations).await.unwrap()
        });
    }
    let mut fixtures = Vec::new();
    while let Some(result) = creating.join_next().await {
        fixtures.push(result.unwrap());
    }
    let mut closing = tokio::task::JoinSet::new();
    for fixture in fixtures {
        let peer = fixture.independent_pool().await.unwrap();
        closing.spawn(async move {
            fixture.close().await.unwrap();
            assert!(peer.is_closed());
        });
    }
    while let Some(result) = closing.join_next().await {
        result.unwrap();
    }
    assert_eq!(process_database_count(b.pool(), &prefix).await, before);

    // Explicit close is blocked on a checked-out connection. Abort its caller
    // after teardown starts, then release the connection; owned cleanup must
    // continue independently while this runtime remains alive.
    let b_name = b.name().to_owned();
    let pool = b.pool().clone();
    let held = pool.acquire().await.unwrap();
    let close = tokio::spawn(b.close());
    tokio::time::timeout(Duration::from_secs(2), async {
        while !pool.is_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("close must start before cancellation");
    close.abort();
    assert!(close.await.unwrap_err().is_cancelled());
    drop(held);

    // A fresh fixture sees both old databases disappear. Its migration also
    // proves that teardown did not modify/drop the shared bootstrap database.
    let witness = TestDatabase::create(&url, &migrations).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
                    .bind(&b_name)
                    .fetch_one(witness.pool())
                    .await
                    .unwrap();
            if !exists {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancelled caller must not leak its database");
    witness.close().await.unwrap();
}
