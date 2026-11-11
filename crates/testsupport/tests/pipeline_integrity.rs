use std::time::Duration;
use two_bot_testsupport::pipeline_integrity::{
    add_observer, completed_statement_profile, ServerSnapshot,
};
use two_bot_testsupport::TestDatabase;

#[tokio::test]
async fn completion_reader_accepts_a_stop_marker_appended_after_initial_eof() {
    let raw = std::env::var("TWO_TEST_DATABASE_URL").unwrap();
    let db = TestDatabase::create(&raw, &sqlx::migrate::Migrator::DEFAULT)
        .await
        .unwrap();
    let observer = TestDatabase::create(&raw, &sqlx::migrate::Migrator::DEFAULT)
        .await
        .unwrap();
    // Normal/coverage sweeps log only this synthetic connection,
    // not every statement in their unrelated database suites.
    let mut writer = db.pool().acquire().await.unwrap();
    sqlx::raw_sql("SET log_min_duration_statement = 0")
        .execute(&mut *writer)
        .await
        .unwrap();
    // Setup can exceed both the measured line cap and one reader chunk.
    // It must not invalidate the later bounded measurement window.
    // Only a fixed synthetic character is interpolated; no external input.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SELECT '{}'",
        "x".repeat(128 * 1024)
    )))
    .execute(&mut *writer)
    .await
    .unwrap();
    sqlx::raw_sql("SELECT 'pipeline-accounting-start'")
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::raw_sql("SELECT 42")
        .execute(&mut *writer)
        .await
        .unwrap();
    // Model the collector delivering the final record after the reader's
    // first EOF, without stopping a server process.
    let stop = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        sqlx::raw_sql("SELECT 'pipeline-accounting-stop'")
            .execute(&mut *writer)
            .await
    });
    let profile = completed_statement_profile(observer.pool(), db.name()).await;
    let stopped = stop.await.unwrap();
    db.close().await.unwrap();
    observer.close().await.unwrap();
    stopped.unwrap();
    assert_eq!(profile.unwrap().values().sum::<i64>(), 1);
}

#[tokio::test]
async fn server_snapshots_require_disabled_autovacuum() {
    let raw = std::env::var("TWO_TEST_DATABASE_URL").unwrap();
    let db = TestDatabase::create(&raw, &sqlx::migrate::Migrator::DEFAULT)
        .await
        .unwrap();
    let (db, observer) = add_observer(&raw, db).await.unwrap();
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .execute(observer.pool())
        .await
        .unwrap();
    let autovacuum: bool = sqlx::query_scalar("SELECT current_setting('autovacuum')::boolean")
        .fetch_one(observer.pool())
        .await
        .unwrap();
    let snapshot = ServerSnapshot::read(observer.pool(), db.name()).await;
    db.close().await.unwrap();
    observer.close().await.unwrap();
    if autovacuum {
        assert!(
            snapshot.unwrap_err().to_string().contains("autovacuum"),
            "background maintenance transactions must invalidate server accounting"
        );
    } else {
        snapshot.unwrap();
    }
}

#[tokio::test]
async fn observer_setup_failure_awaits_first_fixture_cleanup() {
    let raw = std::env::var("TWO_TEST_DATABASE_URL").unwrap();
    let db = TestDatabase::create(&raw, &sqlx::migrate::Migrator::DEFAULT)
        .await
        .unwrap();
    let held = db.pool().acquire().await.unwrap();
    // A checked-out connection blocks verified close. A Drop-only cleanup
    // returns early here and can be lost when the main runtime shuts down.
    let mut cleanup = tokio::spawn(add_observer("invalid fixture URL", db));
    let premature = tokio::time::timeout(Duration::from_millis(20), &mut cleanup).await;
    drop(held);
    let (returned_early, outcome) = match premature {
        Ok(result) => (true, result.unwrap()),
        Err(_) => (false, cleanup.await.unwrap()),
    };
    assert!(
        !returned_early,
        "observer error returned before verified first-fixture cleanup"
    );
    assert!(outcome.is_err(), "invalid observer URL must still fail");
}
