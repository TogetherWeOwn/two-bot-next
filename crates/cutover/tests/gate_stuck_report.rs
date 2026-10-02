//! On-demand rules-gate stuck report acceptance on disposable agent-testdb
//! databases. CI's normal integration step supplies TWO_TEST_DATABASE_URL as
//! an opt-in. Locally: TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/postgres
//! python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test gate_stuck_report
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use two_bot_cutover::legacy_copy::options::guarded_target;

const GUILD: &str = "1545644954272137297";

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

// Parallel tests share one pid and can read the same clock tick.
static NEXT_DB: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    name: String,
    url: String,
}

impl TestDb {
    async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let name = format!(
            "two_bot_test_gate_{}_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            NEXT_DB.fetch_add(1, Ordering::Relaxed)
        );
        let url = format!("postgres://agent_test:@agent-testdb:5432/{name}");
        // Never read an application URL or substitute its credentials.
        let options = guarded_target(&url, false)?
            .ssl_mode(PgSslMode::Disable)
            .options([("timezone", "UTC"), ("statement_timeout", "10000")]);
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone().database("postgres"))
            .await?;
        sqlx::query(database_sql("CREATE DATABASE", &name))
            .execute(&admin)
            .await?;
        // Anything failing after CREATE DATABASE must not leak the database.
        match Self::provision(&admin, options, &name, url).await {
            Ok(db) => Ok(db),
            Err(error) => {
                let _ = sqlx::query(database_sql("DROP DATABASE", &name))
                    .execute(&admin)
                    .await;
                Err(error)
            }
        }
    }

    async fn provision(
        admin: &PgPool,
        options: sqlx::postgres::PgConnectOptions,
        name: &str,
        url: String,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        // Only the funnel projection the report reads: no migration ledger.
        // A suppressed migration must neither create other feature tables nor
        // an audit row.
        sqlx::raw_sql(include_str!("../migrations/0001_funnel.sql"))
            .execute(&pool)
            .await?;
        Ok(Self {
            admin: admin.clone(),
            pool,
            name: name.to_owned(),
            url,
        })
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gate_stuck_report"));
        command
            .env_clear()
            .env("TWO_DATABASE_URL", &self.url)
            .env("TWO_DB_POOL_MAX", "1")
            .args(args)
            .args(["--guild", GUILD]);
        command.output().unwrap()
    }

    async fn seed(&self, sql: &str) -> TestResult {
        // Seed SQL is static string literals at the call sites; the wrapper
        // copies into an Arc so no 'static borrow escapes this method.
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn snapshot(&self) -> Result<serde_json::Value, sqlx::Error> {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p', 'S') ORDER BY c.relname",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut data = serde_json::Map::new();
        for name in names {
            // Identifiers originate in this owned database's catalog, quoted safely.
            let quoted = name.replace('"', "\"\"");
            let rows: serde_json::Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM public.\"{quoted}\" t"
            )))
            .fetch_one(&self.pool)
            .await?;
            data.insert(name, rows);
        }
        Ok(serde_json::Value::Object(data))
    }

    /// Best-effort teardown: every step runs, the first failure is reported.
    async fn cleanup(&self) -> TestResult {
        self.pool.close().await;
        let dropped = sqlx::query(database_sql("DROP DATABASE", &self.name))
            .execute(&self.admin)
            .await
            .map(|_| ());
        self.admin.close().await;
        Ok(dropped?)
    }
}

/// Runs a scenario on a spawned task so a panicking assertion still reaches
/// teardown, then reports the scenario failure ahead of any cleanup failure.
async fn with_db<F, Fut>(scenario: F) -> TestResult
where
    F: FnOnce(std::sync::Arc<TestDb>) -> Fut,
    Fut: std::future::Future<Output = TestResult> + Send + 'static,
{
    let db = std::sync::Arc::new(TestDb::new().await?);
    let joined = tokio::spawn(scenario(db.clone())).await;
    let cleaned = db.cleanup().await;
    match joined {
        Ok(result) => result.and(cleaned),
        Err(panic) => Err(format!("scenario panicked: {panic}").into()),
    }
}

fn database_sql(operation: &str, name: &str) -> sqlx::AssertSqlSafe<String> {
    assert!(name.starts_with("two_bot_test_gate_") && name.len() <= 63);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    sqlx::AssertSqlSafe(format!("{operation} \"{name}\""))
}

fn successful_stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// Legacy rule ported to the `members` projection: joined, never cleared,
// still present, not a bot, join past the threshold. Cleared / recent /
// left / bot rows are not stuck; a missing join timestamp rides along as
// unknown instead of being guessed at.
async fn stuck_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    db.seed(
        "INSERT INTO members (guild_id, member_id, joined_at, gate_cleared_at, left_at, is_bot) VALUES
         ('1545644954272137297', '100000000000000001', now() - INTERVAL '20 days', NULL, NULL, FALSE),
         ('1545644954272137297', '100000000000000002', now() - INTERVAL '30 days', now() - INTERVAL '29 days', NULL, FALSE),
         ('1545644954272137297', '100000000000000003', now() - INTERVAL '2 days', NULL, NULL, FALSE),
         ('1545644954272137297', '100000000000000004', now() - INTERVAL '25 days', NULL, now() - INTERVAL '5 days', FALSE),
         ('1545644954272137297', '100000000000000005', now() - INTERVAL '40 days', NULL, NULL, TRUE),
         ('1545644954272137297', '100000000000000006', NULL, NULL, NULL, FALSE),
         ('9999999999999999999', '100000000000000007', now() - INTERVAL '60 days', NULL, NULL, FALSE)",
    )
    .await?;
    let seeded = db.snapshot().await?;

    let out = successful_stdout(&db.run(&[]));
    // Exactly the 20-day join and the unknown join: cleared (29d), recent
    // (2d), left, bot and other-guild rows stay out.
    assert!(
        out.contains("100000000000000001"),
        "stuck member listed:\n{out}"
    );
    assert!(
        out.contains("UNKNOWN-JOIN 100000000000000006"),
        "unknown join named, not guessed:\n{out}"
    );
    for absent in [
        "100000000000000002",
        "100000000000000003",
        "100000000000000004",
        "100000000000000005",
        "100000000000000007",
    ] {
        assert!(!out.contains(absent), "non-stuck {absent} leaked:\n{out}");
    }
    assert!(out.contains("1 stuck past 14 days"), "counts:\n{out}");

    // A tighter threshold still lists only genuinely past-threshold rows:
    // the 2-day join is not yet due at 7 days.
    let out7 = successful_stdout(&db.run(&["--threshold-days", "7"]));
    assert!(out7.contains("100000000000000001"), "stuck at 7d:\n{out7}");
    assert!(
        !out7.contains("100000000000000003"),
        "recent join not due at 7d:\n{out7}"
    );

    // Read-only: the report never migrates (no ledger) and never changes a row.
    let migrated: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname = 'public' AND tablename = '_sqlx_migrations')",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(!migrated, "report ran migrations");
    assert_eq!(db.snapshot().await?, seeded, "report wrote rows");
    Ok(())
}

async fn clean_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    db.seed(
        "INSERT INTO members (guild_id, member_id, joined_at, gate_cleared_at, left_at, is_bot) VALUES
         ('1545644954272137297', '100000000000000011', now() - INTERVAL '1 day', NULL, NULL, FALSE),
         ('1545644954272137297', '100000000000000012', now() - INTERVAL '30 days', now() - INTERVAL '29 days', NULL, FALSE)",
    )
    .await?;
    let seeded = db.snapshot().await?;
    let out = successful_stdout(&db.run(&[]));
    assert!(out.contains("Clean"), "empty case reports clean:\n{out}");
    assert_eq!(db.snapshot().await?, seeded, "clean report wrote rows");
    Ok(())
}

#[tokio::test]
async fn stuck_members_listed_read_only() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db(stuck_scenario).await
}

#[tokio::test]
async fn empty_case_reports_clean() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db(clean_scenario).await
}
