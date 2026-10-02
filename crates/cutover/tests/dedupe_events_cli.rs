//! Hermetic refusals plus one disposable database on agent-testdb only.
//! python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test dedupe_events_cli -- --include-ignored
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use two_bot_cutover::LIVE_GUILD_ID;

const GUILD: &str = "100000000000000001";
const OTHER_GUILD: &str = "100000000000000002";
type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn run(args: &[&str], url: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dedupe-events"));
    command.env_clear().args(args);
    if let Some(url) = url {
        command.env("TWO_DATABASE_URL", url);
    }
    command.output().unwrap()
}

fn refused(args: &[&str], message: &str) {
    // Poison URL: a usage refusal must happen before URL validation/connection.
    let output = run(args, Some("not-a-database-url"));
    assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(message), "{args:?}: {stderr}");
    assert!(!stderr.contains("database"), "{args:?}: {stderr}");
}

#[test]
fn guild_is_required_and_validated_before_database_access() {
    refused(&[], "missing --guild");
    for guild in [
        "",
        "123",
        "abc",
        "10000000000000000x",
        "00000000000000000",
        "0100000000000000001",
        "18446744073709551616",
        "100000000000000000001",
    ] {
        refused(&["--guild", guild], "must be a Discord snowflake");
        refused(
            &[&format!("--guild={guild}")],
            "must be a Discord snowflake",
        );
    }
    refused(&["--guild"], "must be a Discord snowflake");
    refused(&["--guild", "--apply"], "must be a Discord snowflake");
    refused(
        &["--guild", GUILD, "--guild", OTHER_GUILD],
        "duplicate option",
    );
    refused(
        &["--guild", GUILD, &format!("--guild={OTHER_GUILD}")],
        "duplicate option",
    );
}

#[test]
fn opt_ins_must_be_unambiguous_bare_flags() {
    for flag in ["--apply", "--dry-run", "--allow-live-guild"] {
        for value in ["false", "true", "0", "1", "no", ""] {
            refused(
                &["--guild", GUILD, &format!("{flag}={value}")],
                "must be bare flags",
            );
            refused(&["--guild", GUILD, flag, value], "unexpected positional");
        }
        refused(&["--guild", GUILD, flag, flag], "duplicate option");
    }
    refused(
        &["--guild", GUILD, "--apply", "--dry-run"],
        "cannot be combined",
    );
    refused(
        &["--guild", GUILD, "--dry-run", "--apply"],
        "cannot be combined",
    );
    refused(&["--guild", GUILD, "--aply"], "unknown option");
    refused(&["--guild", GUILD, "input.json"], "unexpected positional");
}

#[test]
fn live_guild_requires_a_bare_opt_in_even_in_dry_run() {
    for mode in [None, Some("--dry-run"), Some("--apply")] {
        let mut args = vec!["--guild", LIVE_GUILD_ID];
        if let Some(mode) = mode {
            args.push(mode);
        }
        refused(&args, "Refusing live guild");
        args.push("--allow-live-guild=false");
        refused(&args, "must be bare flags");
    }
}

fn database_sql(operation: &str, name: &str) -> sqlx::AssertSqlSafe<String> {
    assert!(name.starts_with("two_bot_test_dedupe_") && name.len() <= 63);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    sqlx::AssertSqlSafe(format!("{operation} \"{name}\""))
}

async fn schema(pool: &PgPool) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT to_jsonb(c) FROM information_schema.columns c WHERE table_schema = 'public'
         ORDER BY table_name, ordinal_position",
    )
    .fetch_all(pool)
    .await
}

async fn rows(pool: &PgPool) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    sqlx::query_scalar("SELECT to_jsonb(e) FROM events e ORDER BY id")
        .fetch_all(pool)
        .await
}

async fn scenarios(pool: PgPool, url: String) -> TestResult {
    // Empty schema: a default invocation may fail to read, but must not migrate.
    assert!(schema(&pool).await?.is_empty());
    let output = run(&["--guild", GUILD], Some(&url));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("read failed"));
    assert!(schema(&pool).await?.is_empty());

    // Minimal existing schema deliberately lacks the migrations ledger.
    sqlx::raw_sql(
        "CREATE TABLE events (
            id BIGINT PRIMARY KEY, event_type TEXT NOT NULL, member_id TEXT,
            guild_id TEXT NOT NULL, occurred_at TIMESTAMPTZ NOT NULL, source TEXT NOT NULL
         );
         INSERT INTO events VALUES
            (1, 'member_join', 'same-member', '100000000000000002', '2025-01-01T00:00:00Z', 'foreign'),
            (2, 'member_join', 'same-member', '100000000000000001', '2025-01-01T00:00:01Z', 'a'),
            (3, 'member_join', 'same-member', '100000000000000001', '2025-01-01T00:00:02Z', 'b'),
            (4, 'member_join', 'same-member', '100000000000000002', '2025-01-01T00:00:03Z', 'b'),
            (5, 'member_leave', 'same-member', '100000000000000001', '2025-01-01T00:02:00Z', 'a'),
            (6, 'member_leave', 'same-member', '100000000000000001', '2025-01-01T00:03:00Z', 'b'),
            (7, 'member_join', 'same-source', '100000000000000001', '2025-01-01T00:00:00Z', 'a'),
            (8, 'member_join', 'same-source', '100000000000000001', '2025-01-01T00:00:01Z', 'a'),
            (9, 'member_join', 'anchored', '100000000000000001', '2025-01-01T00:00:00Z', 'a'),
            (10, 'member_join', 'anchored', '100000000000000001', '2025-01-01T00:15:00Z', 'b'),
            (11, 'member_join', 'anchored', '100000000000000001', '2025-01-01T00:15:00.001Z', 'c'),
            (12, 'member_join', NULL, '100000000000000001', '2025-01-01T00:00:00Z', 'a'),
            (13, 'member_join', NULL, '100000000000000001', '2025-01-01T00:00:01Z', 'b'),
            (14, 'message_sent', 'same-member', '100000000000000001', '2025-01-01T00:00:00Z', 'a'),
            (15, 'message_sent', 'same-member', '100000000000000001', '2025-01-01T00:00:01Z', 'b'),
            (16, 'member_join', 'cross-only', '100000000000000001', '2025-01-01T00:00:00.002Z', 'b'),
            (17, 'member_join', 'cross-only', '100000000000000002', '2025-01-01T00:00:00.001Z', 'a');
         INSERT INTO events
         SELECT 1000+n, 'member_join', 'chunk-member', '100000000000000001',
                '2025-01-01T00:00:00Z'::timestamptz + n * interval '1 second', 'source-' || n
         FROM generate_series(0, 202) n;",
    )
    .execute(&pool)
    .await?;
    let before_schema = schema(&pool).await?;
    let before = rows(&pool).await?;
    assert_eq!(before.len(), 220);

    for args in [vec!["--guild", GUILD], vec!["--guild", GUILD, "--dry-run"]] {
        let output = run(&args, Some(&url));
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout)?;
        assert!(stdout.contains("DRY RUN"), "{stdout}");
        assert!(
            stdout.contains("would delete 205 rows; 217 events currently"),
            "{stdout}"
        );
        assert_eq!(rows(&pool).await?, before);
        assert_eq!(schema(&pool).await?, before_schema);
    }

    // Live opt-in on a non-live guild is not an apply opt-in.
    let output = run(&["--guild", GUILD, "--allow-live-guild"], Some(&url));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(rows(&pool).await?, before);
    assert_eq!(schema(&pool).await?, before_schema);

    let output = run(&[&format!("--guild={GUILD}"), "--apply"], Some(&url));
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("deleted 205 rows; 12 events now"),
        "{stdout}"
    );
    let after = rows(&pool).await?;
    let expected: Vec<_> = before
        .iter()
        .filter(|row| {
            let id = row["id"].as_i64().unwrap();
            ![3, 6, 10].contains(&id) && !(1001..=1202).contains(&id)
        })
        .cloned()
        .collect();
    assert_eq!(after, expected);
    let foreign = |data: &[serde_json::Value]| {
        data.iter()
            .filter(|row| row["guild_id"] == OTHER_GUILD)
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(foreign(&after), foreign(&before));
    assert_eq!(schema(&pool).await?, before_schema);

    let output = run(&["--apply", "--guild", GUILD], Some(&url));
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stdout)?.contains("deleted 0 rows; 12 events now"));
    assert_eq!(rows(&pool).await?, after);
    Ok(())
}

#[tokio::test]
async fn default_is_read_only_and_apply_is_guild_scoped() -> TestResult {
    let name = format!(
        "two_bot_test_dedupe_{}_{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    // Hard-coded authorized test service; never inherit any application DB URL.
    let options = PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .ssl_mode(PgSslMode::Disable)
        .options([("statement_timeout", "10000")]);
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options.clone().database("postgres"))
        .await?;
    sqlx::query(database_sql("CREATE DATABASE", &name))
        .execute(&admin)
        .await?;
    let url = format!("postgres://agent_test:@agent-testdb:5432/{name}?sslmode=disable");
    // Cleanup on a connection failure as well as an assertion panic.
    let result = match PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.database(&name))
        .await
    {
        Ok(pool) => {
            let result = tokio::spawn(scenarios(pool.clone(), url)).await;
            pool.close().await;
            result.map_err(Into::into).and_then(|result| result)
        }
        Err(error) => Err(error.into()),
    };
    sqlx::query(database_sql("DROP DATABASE", &name))
        .execute(&admin)
        .await?;
    admin.close().await;
    result
}
