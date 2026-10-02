//! Operator `gateway-force-identify` acceptance. Refusals need no database;
//! the dry-run/apply scenarios run on disposable agent-testdb databases. CI's
//! normal integration step supplies TWO_TEST_DATABASE_URL as an opt-in. Locally:
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/postgres
//! python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test gateway_force_identify_cli
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use two_bot_cutover::legacy_copy::options::guarded_target;
use two_bot_cutover::LIVE_GUILD_ID;

const GUILD: &str = "100000000000000001";
const OTHER_GUILD: &str = "100000000000000002";
/// Connection refused at once: exit 1 would mean the CLI got as far as
/// connecting, so exit 2 proves a refusal fired first.
const UNREACHABLE: &str = "postgres://agent_test:@127.0.0.1:1/two_bot_test_unreachable";

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn cli(database_url: &str, guild_id: Option<&str>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gateway-force-identify"));
    command
        .env_clear()
        .env("TWO_DATABASE_URL", database_url)
        .env("TWO_DATABASE_TLS", "local-only")
        .env("TWO_DB_POOL_MAX", "1")
        .args(args);
    if let Some(guild_id) = guild_id {
        command.env("GUILD_ID", guild_id);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn refusals_exit_2_before_connecting() {
    let cases: [(Option<&str>, &[&str]); 9] = [
        (Some(OTHER_GUILD), &["--guild", GUILD]),
        (
            Some(OTHER_GUILD),
            &["--guild", GUILD, "--apply", "--reason", "boot"],
        ),
        (None, &["--guild", GUILD, "--apply", "--reason", "boot"]),
        (Some(GUILD), &["--guild", GUILD, "--apply"]),
        (
            Some(GUILD),
            &["--guild", GUILD, "--apply", "--reason", "   "],
        ),
        (Some(GUILD), &["--guild", GUILD, "--bogus"]),
        (Some(GUILD), &["--guild", GUILD, "--shard", "-1"]),
        (Some(GUILD), &["--guild", GUILD, "extra"]),
        // Arming the live guild needs the explicit fence flag.
        (
            Some(LIVE_GUILD_ID),
            &["--guild", LIVE_GUILD_ID, "--apply", "--reason", "boot"],
        ),
    ];
    for (guild_id, args) in cases {
        let out = cli(UNREACHABLE, guild_id, args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{guild_id:?} {args:?} must refuse before connecting:\n{}",
            stderr(&out)
        );
        assert!(stdout(&out).is_empty(), "refusal printed a report");
    }
}

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    name: String,
    url: String,
}

impl TestDb {
    async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let name = format!(
            "two_bot_test_force_identify_{}_{}",
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
        // Only the checkpoint slice the CLI reads; it never runs migrations.
        for sql in [
            include_str!("../migrations/0320_gateway_sessions.sql"),
            include_str!("../migrations/0321_gateway_boot_directives.sql"),
        ] {
            sqlx::raw_sql(sql).execute(&pool).await?;
        }
        Ok(Self {
            admin: admin.clone(),
            pool,
            name: name.to_owned(),
            url,
        })
    }

    fn run(&self, guild_id: &str, args: &[&str]) -> Output {
        cli(&self.url, Some(guild_id), args)
    }

    /// Row counts plus full row text of both tables, so a dry run that
    /// rewrote a value would show up even with the counts unchanged.
    async fn snapshot(&self) -> Result<(i64, i64, String), sqlx::Error> {
        sqlx::query_as(
            "SELECT (SELECT count(*) FROM gateway_sessions),
                    (SELECT count(*) FROM gateway_boot_directives),
                    coalesce((SELECT string_agg(s::text, ';' ORDER BY guild_id, shard_id)
                              FROM gateway_sessions s), '')
                    || '|' ||
                    coalesce((SELECT string_agg(d::text, ';' ORDER BY guild_id, shard_id)
                              FROM gateway_boot_directives d), '')",
        )
        .fetch_one(&self.pool)
        .await
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
    assert!(name.starts_with("two_bot_test_force_identify_") && name.len() <= 63);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    sqlx::AssertSqlSafe(format!("{operation} \"{name}\""))
}

// Dry run reports and writes nothing; a mismatched guild refuses even with
// --apply; the matching --apply arms once and never touches the checkpoint.
async fn dry_run_then_apply_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    sqlx::query(
        "INSERT INTO gateway_sessions (guild_id, shard_id, session_id, seq, resume_url, updated_at)
         VALUES ($1, 0, 'current-session', 42, 'ws://mock', now())",
    )
    .bind(GUILD)
    .execute(&db.pool)
    .await?;
    let before = db.snapshot().await?;
    assert_eq!((before.0, before.1), (1, 0));

    let dry = db.run(GUILD, &["--guild", GUILD]);
    assert_eq!(dry.status.code(), Some(0), "dry run:\n{}", stderr(&dry));
    let body = stdout(&dry);
    assert!(
        body.contains("DRY RUN (no writes)"),
        "dry run header:\n{body}"
    );
    assert!(body.contains(GUILD), "dry run names the guild:\n{body}");
    assert!(
        body.contains("shard:      0"),
        "dry run names the shard:\n{body}"
    );
    assert!(body.contains("age "), "dry run prints the age:\n{body}");
    assert!(body.contains("would RESUME"), "fresh checkpoint:\n{body}");
    assert!(body.contains("directive:  none"), "nothing armed:\n{body}");
    assert_eq!(db.snapshot().await?, before, "dry run wrote");

    let refused = db.run(
        OTHER_GUILD,
        &["--guild", GUILD, "--apply", "--reason", "boot"],
    );
    assert_eq!(refused.status.code(), Some(2), "{}", stderr(&refused));
    assert_eq!(db.snapshot().await?, before, "refused --apply wrote");

    let armed = db.run(
        GUILD,
        &[
            "--guild",
            GUILD,
            "--apply",
            "--reason",
            "first production boot",
        ],
    );
    assert_eq!(armed.status.code(), Some(0), "apply:\n{}", stderr(&armed));
    assert!(stdout(&armed).contains("ARMED"), "{}", stdout(&armed));
    let after = db.snapshot().await?;
    assert_eq!(after.1, 1, "one directive row");
    let checkpoint = |snapshot: &(i64, i64, String)| {
        let (sessions, _) = snapshot.2.split_once('|').unwrap();
        sessions.to_owned()
    };
    assert_eq!(
        checkpoint(&after),
        checkpoint(&before),
        "checkpoint rewritten"
    );
    let (reason, pending): (String, bool) =
        sqlx::query_as("SELECT reason, consumed_at IS NULL FROM gateway_boot_directives")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!((reason.as_str(), pending), ("first production boot", true));

    // A repeat keeps the pending directive; the dry run now reports it.
    let again = db.run(GUILD, &["--guild", GUILD, "--apply", "--reason", "repeat"]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert!(
        stdout(&again).contains("already armed"),
        "{}",
        stdout(&again)
    );
    assert_eq!(db.snapshot().await?, after, "repeat changed the directive");
    let report = db.run(GUILD, &["--guild", GUILD]);
    assert!(stdout(&report).contains("ARMED at"), "{}", stdout(&report));
    assert_eq!(db.snapshot().await?, after, "dry run wrote");
    Ok(())
}

#[tokio::test]
async fn dry_run_writes_nothing_and_apply_arms_once() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db(dry_run_then_apply_scenario).await
}
