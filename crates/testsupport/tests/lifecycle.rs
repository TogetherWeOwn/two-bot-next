use two_bot_testsupport::TestDatabase;

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
    peer.close().await;
    let a_name = a.name().to_owned();
    a.close().await.unwrap();
    let (exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(a_name)
            .fetch_one(b.pool())
            .await
            .unwrap();
    assert!(!exists);

    // Count only databases created by this integration-test process, not other
    // suites running concurrently against the same disposable server.
    let prefix = format!("two_bot_test_{:x}_%", std::process::id());
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_database WHERE datname LIKE $1")
        .bind(&prefix)
        .fetch_one(b.pool())
        .await
        .unwrap();
    let broken = sqlx::migrate!("./tests/broken_migrations");
    assert!(TestDatabase::create(&url, &broken).await.is_err());
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_database WHERE datname LIKE $1")
        .bind(&prefix)
        .fetch_one(b.pool())
        .await
        .unwrap();
    assert_eq!(before, after, "migration failure leaked a database");
    let b_name = b.name().to_owned();
    b.close().await.unwrap();

    // A fresh fixture can see both old databases are gone. Its migration also
    // proves that teardown did not modify/drop the shared bootstrap database.
    let witness = TestDatabase::create(&url, &migrations).await.unwrap();
    let (exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(b_name)
            .fetch_one(witness.pool())
            .await
            .unwrap();
    assert!(!exists);
    witness.close().await.unwrap();
}
