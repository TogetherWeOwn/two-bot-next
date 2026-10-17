//! CLI acceptance for the `game-activity-report` operator bin on a
//! disposable agent-testdb database (the `gate_stuck_report.rs` pattern:
//! one owned database per scenario, the real binary spawned against it).
//!
//! Pinned operator contract, modeled on `reengagement-list`:
//! - `game-activity-report --guild <ID>` with `TWO_DATABASE_URL` prints the
//!   JSON report on stdout and exits 0 — populated and empty guilds alike
//!   (an empty guild prints the header shape plus the hint).
//! - Without `--guild` it prints the unscoped hint and exits 0 without
//!   opening a database (no `TWO_DATABASE_URL` needed).
//! - Every run is read-only: the aggregates table is byte-identical after
//!   the run and no migration ledger appears.
//!
//! Skips at runtime when `TWO_TEST_DATABASE_URL` is absent. Locally:
//! `TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/agent_test
//! python3 scripts/cargo_cache.py run -- test -p two-bot-cutover
//! --test game_activity_report_cli`
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use two_bot_cutover::legacy_copy::options::guarded_target;

const GUILD: &str = "100000000000000010";
const OTHER_GUILD: &str = "100000000000000011";
const EMPTY_GUILD: &str = "100000000000000012";
const MIN_MS: i64 = 60_000;

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
            "two_bot_test_gamereport_{}_{}_{}",
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
        // Only the aggregates table the report reads: no migration ledger.
        sqlx::raw_sql(include_str!(
            "../migrations/0441_game_activity_guild_aggregates.sql"
        ))
        .execute(&pool)
        .await?;
        Ok(Self {
            admin: admin.clone(),
            pool,
            name: name.to_owned(),
            url,
        })
    }

    fn run(&self, guild: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_game-activity-report"))
            .env_clear()
            .env("TWO_DATABASE_URL", &self.url)
            .env("TWO_DATABASE_TLS", "local-only")
            .env("TWO_DB_POOL_MAX", "1")
            .args(["--guild", guild])
            .output()
            .unwrap()
    }

    async fn seed(&self, guild: &str, game: &str, total: i64, voice: i64, sessions: i64) {
        sqlx::query(
            "INSERT INTO game_activity_guild_aggregates
                 (guild_id, game, total_play_ms, in_voice_ms, session_count)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(guild)
        .bind(game)
        .bind(total)
        .bind(voice)
        .bind(sessions)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn table_witness(&self) -> serde_json::Value {
        sqlx::query_scalar(
            "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY t.guild_id, t.game), '[]'::jsonb)
               FROM game_activity_guild_aggregates t",
        )
        .fetch_one(&self.pool)
        .await
        .unwrap()
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
    assert!(name.starts_with("two_bot_test_gamereport_") && name.len() <= 63);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    sqlx::AssertSqlSafe(format!("{operation} \"{name}\""))
}

fn successful_stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "CLI failed (status {:?}): {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

async fn report_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    db.seed(GUILD, "Apex", 40 * MIN_MS, MIN_MS, 3).await;
    db.seed(GUILD, "", MIN_MS, 0, 1).await;
    db.seed(GUILD, "Zelda", 40 * MIN_MS, 2 * MIN_MS, 2).await;
    db.seed(OTHER_GUILD, "OtherGuildOnlyGame", 99 * MIN_MS, 0, 9)
        .await;
    let seeded = db.table_witness().await;

    let out = successful_stdout(&db.run(GUILD));
    let parsed: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(parsed["guild_id"], GUILD);
    assert_eq!(
        parsed["rows"],
        serde_json::json!([
            {"game": "Zelda", "total_play_ms": 40 * MIN_MS, "in_voice_ms": 2 * MIN_MS, "session_count": 2},
            {"game": "Apex", "total_play_ms": 40 * MIN_MS, "in_voice_ms": MIN_MS, "session_count": 3},
            {"game": "(untitled)", "total_play_ms": MIN_MS, "in_voice_ms": 0, "session_count": 1},
        ]),
        "bin renders the seeded guild's exact rows:\n{out}"
    );
    assert!(
        !out.contains("OtherGuildOnlyGame") && !out.contains(OTHER_GUILD),
        "another guild's rows rendered:\n{out}"
    );

    // Empty guild: header shape plus hint, exit 0.
    let empty_out = successful_stdout(&db.run(EMPTY_GUILD));
    let empty: serde_json::Value = serde_json::from_str(&empty_out)?;
    assert_eq!(empty["guild_id"], EMPTY_GUILD);
    assert!(empty["rows"].as_array().unwrap().is_empty());
    assert_eq!(empty["hint"], "no game activity recorded for this guild");

    // Read-only: no migration ledger, no changed row (counts, totals and
    // updated_at all ride in the witness).
    let migrated: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname = 'public' AND tablename = '_sqlx_migrations')",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(!migrated, "report ran migrations");
    assert_eq!(db.table_witness().await, seeded, "report wrote rows");
    Ok(())
}

#[tokio::test]
async fn bin_reports_seeded_guild_and_empty_guild_read_only() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db(report_scenario).await
}

#[test]
fn bin_without_guild_prints_unscoped_hint_and_exits_zero_without_a_database() {
    // No TWO_DATABASE_URL at all: the unscoped path must not open a database
    // (the reengagement-list precedent).
    let output = Command::new(env!("CARGO_BIN_EXE_game-activity-report"))
        .env_clear()
        .output()
        .unwrap();
    let out = successful_stdout(&output);
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("unscoped output is JSON");
    assert_eq!(parsed["guild_id"], serde_json::Value::Null);
    assert!(parsed["rows"].as_array().unwrap().is_empty());
    let hint = parsed["hint"].as_str().expect("hint is a string");
    assert!(hint.contains("--guild"), "hint names the flag: {hint}");
}
