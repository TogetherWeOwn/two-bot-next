//! Rehearse the concurrent-index recovery in docs/migrations.md.

use super::{Fixture, TestResult};
use serde_json::Value;
use sqlx::{
    migrate::{MigrateError, Migration, MigrationType, Migrator},
    Connection, Executor, PgConnection, PgPool, SqlSafeStr,
};
use two_bot_cutover::staging_migrate::MIGRATOR;

const WARNING_INDEX_SQL: &str =
    include_str!("../../migration-candidates/moderation_warning_history_order.sql");
// Outside the bot allocation. Only the disposable migrator can discover it.
const FIXTURE_VERSION: i64 = 10_000;

const INSERT_WARNING: &str = "INSERT INTO public.moderation_warnings
    (id, guild_id, user_id, actor_id, reason, request_id, created_at)
    VALUES ('fixture-warning', '1', '2', '3', 'synthetic reason',
            'fixture-request', '2026-10-01T12:34:56-05:00')";

async fn warnings(conn: &mut PgConnection) -> Result<Vec<Value>, sqlx::Error> {
    // Compare the instant even across the retained TEXT-to-timestamptz upgrade.
    sqlx::query_scalar(
        "SELECT to_jsonb(w) || jsonb_build_object('created_at',
                    extract(epoch FROM created_at::timestamptz))
         FROM public.moderation_warnings AS w ORDER BY id",
    )
    .fetch_all(conn)
    .await
}

async fn ledger(conn: &mut PgConnection) -> Result<Vec<Value>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT to_jsonb(m) || jsonb_build_object('checksum', encode(checksum, 'hex'))
         FROM public._sqlx_migrations AS m ORDER BY version",
    )
    .fetch_all(conn)
    .await
}

async fn index_state(conn: &mut PgConnection) -> Result<Option<(bool, bool)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT indisvalid, indisready FROM pg_index
         WHERE indexrelid = to_regclass('public.idx_moderation_warnings_history_order')
           AND indrelid = 'public.moderation_warnings'::regclass",
    )
    .fetch_optional(conn)
    .await
}

async fn attempt(migrator: &Migrator, pool: &PgPool) -> Result<(), MigrateError> {
    let mut conn = pool.acquire().await?;
    conn.execute("SET statement_timeout = '15s'").await?;
    conn.execute("SET lock_timeout = '2s'").await?;
    let result = migrator.run(&mut *conn).await;
    // On an error SQLx can leave its session advisory lock held. Close the
    // session as a failed migration process would before inspecting or retrying.
    conn.close().await?;
    result
}

fn expect_candidate_failure(result: Result<(), MigrateError>, code: &str) {
    let Err(MigrateError::ExecuteMigration(error, FIXTURE_VERSION)) = result else {
        panic!("expected fixture migration failure, got {result:?}");
    };
    let error = error.as_database_error().expect("PostgreSQL build error");
    assert_eq!(error.code().as_deref(), Some(code), "{error}");
}

async fn rehearse(pool: PgPool, mut blocker: PgConnection, retained: bool) -> TestResult {
    let mut conn = pool.acquire().await?;
    let original_warning = if retained {
        conn.execute(include_str!(
            "../../../core/tests/fixtures/member_moderation_legacy.sql"
        ))
        .await?;
        conn.execute(INSERT_WARNING).await?;
        let before = warnings(&mut conn).await?;
        MIGRATOR.run(&mut *conn).await?;
        assert_eq!(warnings(&mut conn).await?, before);
        before
    } else {
        MIGRATOR.run(&mut *conn).await?;
        conn.execute(INSERT_WARNING).await?;
        warnings(&mut conn).await?
    };
    assert_eq!(original_warning.len(), 1);
    let typed: bool = sqlx::query_scalar(
        "SELECT pg_typeof(created_at) = 'timestamptz'::regtype FROM public.moderation_warnings",
    )
    .fetch_one(&mut *conn)
    .await?;
    assert!(
        typed,
        "the current migration chain must supply native timestamps"
    );
    assert_eq!(index_state(&mut conn).await?, None);
    let original_ledger = ledger(&mut conn).await?;
    drop(conn);

    let candidate = Migration::new(
        FIXTURE_VERSION,
        "warning history fixture".into(),
        MigrationType::Simple,
        WARNING_INDEX_SQL.into_sql_str(),
        WARNING_INDEX_SQL.starts_with("-- no-transaction"),
    );
    let migrator = Migrator::with_migrations(
        MIGRATOR
            .iter()
            .cloned()
            .chain([candidate.clone()])
            .collect(),
    );

    // Hold a writer lock before starting the build. The concurrent index's
    // initial catalog entry can commit, but the build times out waiting for
    // writers before its first scan.
    let mut writer = blocker.begin().await?;
    writer
        .execute("LOCK TABLE public.moderation_warnings IN ROW EXCLUSIVE MODE")
        .await?;
    let interrupted = attempt(&migrator, &pool).await;
    writer.rollback().await?;
    expect_candidate_failure(interrupted, "55P03");

    let mut conn = pool.acquire().await?;
    assert_eq!(index_state(&mut conn).await?, Some((false, false)));
    assert_eq!(ledger(&mut conn).await?, original_ledger);
    assert_eq!(warnings(&mut conn).await?, original_warning);
    drop(conn);

    // A blind retry refuses the existing name instead of recording success.
    expect_candidate_failure(attempt(&migrator, &pool).await, "42P07");
    let mut conn = pool.acquire().await?;
    assert_eq!(index_state(&mut conn).await?, Some((false, false)));
    assert_eq!(ledger(&mut conn).await?, original_ledger);
    assert_eq!(warnings(&mut conn).await?, original_warning);

    conn.execute("SET statement_timeout = '15s'").await?;
    conn.execute("DROP INDEX CONCURRENTLY IF EXISTS public.idx_moderation_warnings_history_order")
        .await?;
    assert_eq!(index_state(&mut conn).await?, None);
    drop(conn);

    attempt(&migrator, &pool).await?;
    let mut conn = pool.acquire().await?;
    assert_eq!(index_state(&mut conn).await?, Some((true, true)));
    assert_eq!(warnings(&mut conn).await?, original_warning);
    let mut recovered_ledger = ledger(&mut conn).await?;
    let applied = recovered_ledger.pop().expect("candidate ledger row");
    assert_eq!(applied["version"], FIXTURE_VERSION);
    assert_eq!(applied["success"], true);
    assert_eq!(applied["checksum"], hex::encode(&candidate.checksum));
    assert_eq!(recovered_ledger, original_ledger);
    Ok(())
}

async fn exercise(retained: bool) -> TestResult {
    let fixture = Fixture::new().await?;
    let outcome = async {
        let pool = fixture.migrator_pool().await?;
        let blocker = fixture.scratch().await?;
        tokio::spawn(rehearse(pool, blocker, retained)).await?
    }
    .await;
    fixture.finish().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb or CI Postgres service"]
async fn warning_history_recovers_on_current_schema() -> TestResult {
    exercise(false).await
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb or CI Postgres service"]
async fn warning_history_recovers_on_retained_schema() -> TestResult {
    exercise(true).await
}
