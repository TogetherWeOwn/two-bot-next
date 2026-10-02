//! Read-only integrity-report acceptance on disposable agent-testdb
//! databases plus a loopback mock roster (TOG-11152).
//!
//! `report voice-reconcile` pairs `voice_session_start` / `voice_session_end`
//! halves per (guild, member) and recovers durations where the stored rows
//! allow it; `report leave-gap` classifies join-without-leave members gone
//! from the roster. Both print JSON; neither writes report data. The roster
//! read is one bounded GET (20 pages / 20,000 members max); a ceiling hit or
//! an unreadable page refuses loudly instead of counting a partial roster.
//!
//! Fixtures are the legacy seeds (`buildSeedHalves` / `buildSeedGapData` in
//! legacy two-bot `src/analytics/`), rewritten as SQL plus a mock roster.
//! `TWO_TEST_DATABASE_URL` is the opt-in (same guard as gate_stuck_report):
//! without it the tests skip, with it (CI's `--test '*'` integration step)
//! they run for real on disposable databases.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use two_bot_cutover::legacy_copy::options::guarded_target;

const GUILD: &str = "1545644954272137297";

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    name: String,
    url: String,
}

// Parallel tests share one pid and can read the same clock tick.
static NEXT_DB: AtomicUsize = AtomicUsize::new(0);

impl TestDb {
    async fn new(prefix: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let name = format!(
            "{prefix}_{}_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            NEXT_DB.fetch_add(1, Ordering::Relaxed)
        );
        assert!(name.len() <= 63);
        assert!(name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
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
            .max_connections(2)
            .connect_with(options)
            .await?;
        // Only the funnel projection the reports read: no migration ledger.
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

    fn run(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_report"));
        command
            .env_clear()
            .env("TWO_DATABASE_URL", &self.url)
            .env("TWO_DATABASE_TLS", "local-only")
            .env("TWO_DB_POOL_MAX", "1");
        for (k, v) in extra_env {
            command.env(k, v);
        }
        command.args(args);
        command.output().unwrap()
    }

    async fn seed(&self, sql: &str) -> TestResult {
        sqlx::raw_sql(sql).execute(&self.pool).await?;
        Ok(())
    }

    async fn snapshot(&self) -> Result<serde_json::Value, sqlx::Error> {
        // Tables only: sequences (relkind 'S') have no composite row type,
        // so to_jsonb(t) on them fails with 42809. The snapshot asserts the
        // reports wrote no rows; sequences carry none.
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p') ORDER BY c.relname",
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
async fn with_db<F, Fut>(prefix: &'static str, scenario: F) -> TestResult
where
    F: FnOnce(std::sync::Arc<TestDb>) -> Fut,
    Fut: std::future::Future<Output = TestResult> + Send + 'static,
{
    let db = std::sync::Arc::new(TestDb::new(prefix).await?);
    let joined = tokio::spawn(scenario(db.clone())).await;
    let cleaned = db.cleanup().await;
    match joined {
        Ok(result) => result.and(cleaned),
        Err(panic) => Err(format!("scenario panicked: {panic}").into()),
    }
}

fn database_sql(operation: &str, name: &str) -> sqlx::AssertSqlSafe<String> {
    assert!(name.starts_with("two_bot_test_") && name.len() <= 63);
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

/// Serve `respond(after)` for every request (same `after=` paging contract
/// as the member-pagination regression); returns (base, request counter).
fn mock_roster(respond: impl Fn(u64) -> String + Send + 'static) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let line = req.lines().next().unwrap_or_default();
            let after = line
                .split("after=")
                .nth(1)
                .and_then(|r| r.split(['&', ' ']).next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            c.fetch_add(1, Ordering::SeqCst);
            let body = respond(after);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (base, count)
}

fn member(id: &str) -> String {
    format!(
        r#"{{"user":{{"id":"{id}","username":"u","discriminator":"0","avatar":null}},"roles":[],"joined_at":"2024-01-01T00:00:00.000000+00:00","deaf":false,"mute":false,"flags":0}}"#
    )
}

fn event(event_type: &str, member_id: &str, at: &str, source: &str, metadata: &str) -> String {
    let meta = metadata.replace('\'', "''");
    format!(
        "('{event_type}', '{member_id}', '{GUILD}', '{at}'::timestamptz, '{source}', '{meta}', \
         '{GUILD}:{member_id}:{event_type}:{at}')"
    )
}

/// Legacy `buildSeedHalves` as SQL: m1 healthy complete; m2 restart-gap;
/// m3 server-leave; m4 still-open; m5 first start superseded + second
/// complete; m6 unknown end with no start; m7 metadata-recompute.
async fn seed_voice(db: &std::sync::Arc<TestDb>) -> TestResult {
    let rows = [
        event(
            "voice_session_start",
            "m1",
            "2026-09-20T10:00:00Z",
            "channel:ch-a",
            "{}",
        ),
        event(
            "voice_session_end",
            "m1",
            "2026-09-20T10:30:00Z",
            "channel:ch-a",
            r#"{"startKnown":true,"startedAt":"2026-09-20T10:00:00.000Z","durationSeconds":1800}"#,
        ),
        event(
            "voice_session_start",
            "m2",
            "2026-09-20T09:00:00Z",
            "channel:ch-a",
            "{}",
        ),
        event(
            "voice_session_end",
            "m2",
            "2026-09-20T11:00:00Z",
            "channel:ch-a",
            r#"{"startKnown":false,"startedAt":null,"durationSeconds":null}"#,
        ),
        event(
            "voice_session_start",
            "m3",
            "2026-09-20T10:00:00Z",
            "channel:ch-a",
            "{}",
        ),
        event(
            "member_leave",
            "m3",
            "2026-09-20T10:10:00Z",
            "gateway",
            "{}",
        ),
        event(
            "voice_session_start",
            "m4",
            "2026-09-20T10:00:00Z",
            "channel:ch-b",
            "{}",
        ),
        event(
            "voice_session_start",
            "m5",
            "2026-09-20T10:00:00Z",
            "channel:ch-a",
            "{}",
        ),
        event(
            "voice_session_start",
            "m5",
            "2026-09-20T11:00:00Z",
            "channel:ch-b",
            "{}",
        ),
        event(
            "voice_session_end",
            "m5",
            "2026-09-20T11:05:00Z",
            "channel:ch-b",
            r#"{"startKnown":true,"startedAt":"2026-09-20T11:00:00.000Z","durationSeconds":300}"#,
        ),
        event(
            "voice_session_end",
            "m6",
            "2026-09-20T11:00:00Z",
            "channel:ch-a",
            r#"{"startKnown":false,"startedAt":null,"durationSeconds":null}"#,
        ),
        event(
            "voice_session_start",
            "m7",
            "2026-09-20T08:30:00Z",
            "channel:ch-a",
            "{}",
        ),
        event(
            "voice_session_end",
            "m7",
            "2026-09-20T09:00:00Z",
            "channel:ch-a",
            r#"{"startKnown":true,"startedAt":"2026-09-20T08:30:00.000Z","durationSeconds":null}"#,
        ),
    ];
    db.seed(&format!(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key) VALUES {}",
        rows.join(",")
    ))
    .await
}

async fn voice_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    seed_voice(&db).await?;
    let seeded = db.snapshot().await?;

    let out = successful_stdout(&db.run(&["voice-reconcile", "--guild", GUILD], &[]));
    let report: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(report["tool"], "voice-reconcile");
    assert_eq!(report["guild"], GUILD);
    assert_eq!(report["complete"].as_u64(), Some(2));
    assert_eq!(report["skipped"].as_u64(), Some(0));
    let resolutions: Vec<&str> = report["resolved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["resolution"].as_str().unwrap())
        .collect();
    for want in ["restart-gap", "server-leave", "metadata-recompute"] {
        assert!(
            resolutions.contains(&want),
            "missing {want}: {resolutions:?}"
        );
    }
    assert_eq!(resolutions.len(), 3);
    let reasons: Vec<&str> = report["unresolvable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["reason"].as_str().unwrap())
        .collect();
    for want in ["still-open", "superseded", "no-start-on-file"] {
        assert!(reasons.contains(&want), "missing {want}: {reasons:?}");
    }
    assert_eq!(reasons.len(), 3);
    // The restart-gap duration recomputes from the start row (2h = 7200s).
    let m2 = report["resolved"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["memberId"] == "m2")
        .unwrap();
    assert_eq!(m2["durationSeconds"].as_i64(), Some(7200));
    // Every resolved session carries a finite non-negative duration.
    for r in report["resolved"].as_array().unwrap() {
        let d = r["durationSeconds"].as_i64().unwrap();
        assert!(d >= 0, "negative duration: {r}");
    }
    assert_eq!(report["discordRequests"].as_u64(), Some(0));

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

/// Legacy `buildSeedGapData` as SQL + mock roster: m-present (snowflake
/// ...001) on the roster is correct, not a gap; m-clean (...002) resolved;
/// m-pre (...003) pre-coverage; m-miss (...004) log-miss; m-raid (...005)
/// raid-residue; m-rejoin (...006) rejoin-gap with 2 fills. The roster holds
/// ...001 only: everyone else reads as off-roster.
async fn seed_gap(db: &std::sync::Arc<TestDb>) -> TestResult {
    let rows = [
        event(
            "member_join",
            "100000000000000001",
            "2025-06-01T10:00:00Z",
            "backfill:log:x",
            "{}",
        ),
        event(
            "member_join",
            "100000000000000002",
            "2025-05-01T10:00:00Z",
            "backfill:log:x",
            "{}",
        ),
        event(
            "member_leave",
            "100000000000000002",
            "2025-05-10T10:00:00Z",
            "backfill:log:x",
            "{}",
        ),
        event(
            "member_join",
            "100000000000000003",
            "2023-01-15T10:00:00Z",
            "backfill:log:x",
            "{}",
        ),
        event(
            "member_join",
            "100000000000000004",
            "2025-06-15T10:00:00Z",
            "backfill:log:x",
            "{}",
        ),
        event(
            "member_join",
            "100000000000000005",
            "2025-07-06T21:00:00Z",
            "backfill:log:x",
            "{}",
        ),
        event(
            "member_join",
            "100000000000000006",
            "2024-05-01T10:00:00Z",
            "backfill:log:x",
            "{}",
        ),
        event(
            "member_join",
            "100000000000000006",
            "2024-09-01T10:00:00Z",
            "backfill:log:x",
            "{}",
        ),
    ];
    db.seed(&format!(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key) VALUES {}",
        rows.join(",")
    ))
    .await
}

async fn gap_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    seed_gap(&db).await?;
    let seeded = db.snapshot().await?;
    // The mock roster holds only ...001 (m-present).
    let (base, _) = mock_roster(|_| format!("[{}]", member("100000000000000001")));

    let out = successful_stdout(&db.run(
        &[
            "leave-gap",
            "--guild",
            GUILD,
            "--floor",
            "2024-01-01T00:00:00.000Z",
            "--discord-base",
            base.as_str(),
        ],
        &[],
    ));
    let report: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(report["tool"], "leave-gap");
    assert_eq!(report["guild"], GUILD);
    let kinds: Vec<&str> = report["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["kind"].as_str().unwrap())
        .collect();
    for want in ["log-miss", "pre-coverage", "raid-residue", "rejoin-gap"] {
        assert!(kinds.contains(&want), "missing {want}: {kinds:?}");
    }
    assert_eq!(kinds.len(), 4, "one gap per kind: {kinds:?}");
    assert_eq!(report["present"].as_u64(), Some(1));
    assert_eq!(report["resolved"].as_u64(), Some(1));
    // 4 gaps, fills 1+1+1+2 = 5 (legacy pins this count).
    assert_eq!(report["fillsProposed"].as_u64(), Some(5));
    // Every proposed fill is bounded THAT-not-WHEN.
    for g in report["gaps"].as_array().unwrap() {
        for f in g["fills"].as_array().unwrap() {
            assert_eq!(f["bound"], "earliest-possible");
        }
        assert!(!g["detail"].as_str().unwrap().is_empty());
    }
    assert_eq!(report["discordRequests"].as_u64(), Some(1));

    let migrated: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname = 'public' AND tablename = '_sqlx_migrations')",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(!migrated, "report ran migrations");
    assert_eq!(db.snapshot().await?, seeded, "report wrote rows");
    Ok(())
}

async fn usage_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    // --help boots with no database needed, but here the DB exists already.
    let help = db.run(&["--help"], &[]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("voice-reconcile"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("leave-gap"));

    let unknown = db.run(&["bogus-report"], &[]);
    assert_eq!(unknown.status.code(), Some(2));

    let bad_days = db.run(
        &["voice-reconcile", "--guild", GUILD, "--days", "bogus"],
        &[],
    );
    assert_eq!(bad_days.status.code(), Some(2));

    let bad_floor = db.run(
        &["leave-gap", "--guild", GUILD, "--floor", "not-a-date"],
        &[],
    );
    assert_eq!(bad_floor.status.code(), Some(3));

    // Seeded demos need no token and no roster: pure fixtures.
    let seed_voice = successful_stdout(&db.run(&["voice-reconcile", "--seed"], &[]));
    let seed_report: serde_json::Value = serde_json::from_str(&seed_voice)?;
    assert_eq!(seed_report["mode"], "seeded-demo");
    assert_eq!(seed_report["resolved"].as_array().unwrap().len(), 3);
    assert_eq!(seed_report["unresolvable"].as_array().unwrap().len(), 3);
    assert_eq!(seed_report["complete"], 2);

    let seed_gap = successful_stdout(&db.run(&["leave-gap", "--seed"], &[]));
    let seed_gap_report: serde_json::Value = serde_json::from_str(&seed_gap)?;
    assert_eq!(seed_gap_report["mode"], "seeded-demo");
    assert_eq!(seed_gap_report["gaps"].as_array().unwrap().len(), 4);
    assert_eq!(seed_gap_report["fillsProposed"], 5);
    Ok(())
}

async fn ceiling_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    seed_gap(&db).await?;
    // Every roster page is full and advancing (1000 fresh members per
    // page), so the 20-page cap trips: the sweep refuses the partial roster
    // loudly instead of counting it.
    const BASE: u64 = 200_000_000_000_000_000;
    let (base, count) = mock_roster(|after| {
        let page = if after == 0 {
            0
        } else {
            (after - BASE) / 1000;
        };
        format!(
            "[{}]",
            (1..=1000)
                .map(|i| member(&(BASE + page * 1000 + i).to_string()))
                .collect::<Vec<_>>()
                .join(",")
        )
    });
    let out = db.run(
        &[
            "leave-gap",
            "--guild",
            GUILD,
            "--discord-base",
            base.as_str(),
        ],
        &[],
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        20,
        "roster stopped at the page cap"
    );
    assert_eq!(out.status.code(), Some(1), "ceiling must fail loudly");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("never presented as complete"),
        "ceiling stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

#[tokio::test]
async fn voice_reconcile_matches_legacy_fixtures_read_only() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db("two_bot_test_report_voice", voice_scenario).await
}

#[tokio::test]
async fn leave_gap_matches_legacy_fixtures_read_only() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db("two_bot_test_report_gap", gap_scenario).await
}

#[tokio::test]
async fn report_usage_contract() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db("two_bot_test_report_usage", usage_scenario).await
}

#[tokio::test]
async fn roster_ceiling_refuses_partial_roster() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db("two_bot_test_report_cap", ceiling_scenario).await
}
