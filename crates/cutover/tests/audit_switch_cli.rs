//! Operator audit-switch (`--halt/--resume/--status`) acceptance on
//! disposable agent-testdb databases. CI's normal integration step supplies
//! TWO_TEST_DATABASE_URL as an opt-in. Locally:
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/postgres
//! python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test audit_switch_cli
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use two_bot_cutover::legacy_copy::options::guarded_target;

const ACTOR: &str = "100000000000000001";
const OTHER_ACTOR: &str = "100000000000000002";

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    name: String,
    url: String,
}

impl TestDb {
    async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let name = format!(
            "two_bot_test_switch_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
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
        // Only the audit slice the switch guards: no migration ledger. The
        // switch itself must never create other feature tables by accident.
        sqlx::raw_sql(include_str!("../migrations/0340_operational_audit.sql"))
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_audit_switch"));
        command
            .env_clear()
            .env("TWO_DATABASE_URL", &self.url)
            .env("TWO_DATABASE_TLS", "local-only")
            .env("TWO_DB_POOL_MAX", "1")
            .args(args);
        command.output().unwrap()
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
    assert!(name.starts_with("two_bot_test_switch_") && name.len() <= 63);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    sqlx::AssertSqlSafe(format!("{operation} \"{name}\""))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// --halt records the halt and --status reports it with the holder and time.
async fn halt_status_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    let out = db.run(&["--halt", "--actor", ACTOR]);
    assert_eq!(out.status.code(), Some(0), "halt failed:\n{}", stderr(&out));
    assert!(
        stdout(&out).contains(ACTOR),
        "halt names actor:\n{}",
        stdout(&out)
    );

    let status = db.run(&["--status"]);
    assert_eq!(
        status.status.code(),
        Some(0),
        "status failed:\n{}",
        stderr(&status)
    );
    let body = stdout(&status);
    assert!(body.contains("HALTED"), "halt reported:\n{body}");
    assert!(body.contains(ACTOR), "holder named:\n{body}");
    assert!(body.contains("engaged_at"), "hold time named:\n{body}");
    Ok(())
}

// Repeat --halt is ok and names the first holder; --resume clears it while a
// second --resume is ok and reports clear.
async fn idempotent_cycle_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    assert_eq!(db.run(&["--halt", "--actor", ACTOR]).status.code(), Some(0));
    let again = db.run(&["--halt", "--actor", OTHER_ACTOR]);
    assert_eq!(again.status.code(), Some(0));
    // First engagement wins: the repeat names the original holder, not the loser.
    assert!(
        stdout(&again).contains(ACTOR),
        "repeat names first holder:\n{}",
        stdout(&again)
    );

    let resume = db.run(&["--resume"]);
    assert_eq!(resume.status.code(), Some(0));
    let clear = db.run(&["--status"]);
    assert!(
        stdout(&clear).contains("CLEAR"),
        "resume clears:\n{}",
        stdout(&clear)
    );
    let reclear = db.run(&["--resume"]);
    assert_eq!(reclear.status.code(), Some(0));
    assert!(
        stdout(&reclear).contains("already clear"),
        "second resume is ok:\n{}",
        stdout(&reclear)
    );
    Ok(())
}

// While halted a pending row cannot be claimed (mirror contract: no live
// send, no retry); after --resume it becomes claimable again.
async fn halt_blocks_claim_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    sqlx::query(
        "INSERT INTO operational_audit_log
           (entry_id, event_kind, guild_id, occurred_at, metadata_json, created_at,
            mirror_channel_id, delivery_state)
         VALUES ('switch-row', 'message_delete', '1545644954272137297',
                 now(), '{}', now(), '123', 'pending')",
    )
    .execute(&db.pool)
    .await?;
    assert_eq!(db.run(&["--halt", "--actor", ACTOR]).status.code(), Some(0));

    let status = db.run(&["--status"]);
    assert!(
        stdout(&status).contains("claimable_pending: 0"),
        "halt hides pending:\n{}",
        stdout(&status)
    );
    let visible: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operational_audit_log WHERE delivery_state = 'pending'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(visible, 1, "row retained, only hidden");

    assert_eq!(db.run(&["--resume"]).status.code(), Some(0));
    let back = db.run(&["--status"]);
    assert!(
        stdout(&back).contains("claimable_pending: 1"),
        "resume re-queues:\n{}",
        stdout(&back)
    );
    Ok(())
}

// A failed halt read surfaces exit 1; it never prints CLEAR.
// A missing schema plus migrations-off means the table read fails: the
// fail-open decision belongs to the mirror, not this report.
async fn read_failure_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    sqlx::query("ALTER TABLE audit_kill_switch RENAME TO unavailable_switch")
        .execute(&db.pool)
        .await?;
    let status = db.run(&["--status"]);
    assert_eq!(
        status.status.code(),
        Some(1),
        "unreadable switch must fail:\n{}",
        stderr(&status)
    );
    assert!(
        !stdout(&status).contains("CLEAR"),
        "failure never reports clear:\n{}",
        stdout(&status)
    );
    sqlx::query("ALTER TABLE unavailable_switch RENAME TO audit_kill_switch")
        .execute(&db.pool)
        .await?;
    Ok(())
}

// Usage errors exit 2: no command, two commands, halt without actor, bad
// actor, and unknown flags. The switch is untouched by all of them.
async fn usage_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    for args in [
        vec![],
        vec!["--halt", "--resume"],
        vec!["--halt"],
        vec!["--halt", "--actor", "not-a-snowflake"],
        vec!["--resume", "--actor", ACTOR],
        vec!["--status", "--bogus"],
    ] {
        let out = db.run(&args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "usage {args:?} must exit 2:\n{}",
            stderr(&out)
        );
        assert!(stderr(&out).contains("Usage"), "usage {args:?} prints help");
    }
    let untouched: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_kill_switch")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(untouched, 0, "usage errors engage nothing");
    Ok(())
}

// The fenced target refuses a live-shaped URL before connecting, exit 2, and
// the URL never appears in diagnostics. No database is needed: the fence
// fires before any connection.
fn fenced_target() -> TestResult {
    let mut command = Command::new(env!("CARGO_BIN_EXE_audit_switch"));
    command
        .env_clear()
        .env(
            "TWO_DATABASE_URL",
            "postgres://agent_test:@agent-testdb:5432/postgres",
        )
        .env("TWO_DATABASE_TLS", "local-only")
        .env("TWO_DB_POOL_MAX", "1")
        .arg("--status");
    let out = command.output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!stdout(&out).contains("postgres"), "URL leaked to stdout");
    assert!(!stderr(&out).contains("postgres"), "URL leaked to stderr");
    Ok(())
}

#[tokio::test]
async fn halt_is_reported() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db(halt_status_scenario).await
}

#[tokio::test]
async fn halt_resume_cycle_is_idempotent() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db(idempotent_cycle_scenario).await
}

#[tokio::test]
async fn halt_hides_pending_and_resume_requeues() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db(halt_blocks_claim_scenario).await
}

#[tokio::test]
async fn failed_halt_read_never_reports_clear() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db(read_failure_scenario).await
}

#[tokio::test]
async fn usage_errors_exit_2_and_engage_nothing() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db(usage_scenario).await
}

#[tokio::test]
async fn live_shaped_target_refused_before_connect() -> TestResult {
    // No opt-in and no database: the fence fires before any connection.
    fenced_target()
}
