//! Read-only integrity-report acceptance on disposable agent-testdb
//! databases plus a loopback mock roster (TOG-11152) and the voice
//! ghost-channel count (TOG-13548).
//!
//! `report voice-reconcile` pairs `voice_session_start` / `voice_session_end`
//! halves per (guild, member) and recovers durations where the stored rows
//! allow it, names the blind windows in the `events.recorded_at` write series
//! with per-window `startKnown:false` counts, and averages known-start
//! durations only; `report leave-gap` classifies join-without-leave members gone
//! from the roster. Both print JSON; neither writes report data. The roster
//! read is one bounded GET (20 pages / 20,000 members max); a ceiling hit or
//! an unreadable page refuses loudly instead of counting a partial roster.
//! `report voice-ghosts` diffs tracked `voice_rooms` rows against the live
//! voice channels from one channel-listing GET: tracked-present,
//! tracked-gone and untracked-present. Existence only, never a write.
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

    async fn seed(&self, sql: String) -> TestResult {
        // Owned SQL (same AssertSqlSafe<String> pattern as the snapshot
        // below): raw_sql takes no borrowed &str, only 'static or asserted.
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&self.pool)
            .await?;
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
    db.seed(format!(
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
    // One INSERT stamps one `recorded_at` for every row: a single write
    // instant cannot bound a gap, so no window, and both unknown-start ends
    // stay visible as unattributed rather than vanishing.
    assert_eq!(report["blindWindows"]["heartbeats"].as_u64(), Some(1));
    assert_eq!(report["blindWindows"]["windows"], serde_json::json!([]));
    assert_eq!(
        report["blindWindows"]["unattributedUnknownStarts"].as_u64(),
        Some(2)
    );
    // m1 (1800s) and m5 (300s) are the measured known starts; m7 is a known
    // start with no duration; m2 and m6 are unknown starts, excluded.
    assert_eq!(report["durations"]["measured"].as_u64(), Some(2));
    assert_eq!(report["durations"]["averageSeconds"].as_f64(), Some(1050.0));
    assert_eq!(
        report["durations"]["excludedUnknownStarts"].as_u64(),
        Some(2)
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

/// An `events` row with an explicit write instant: `recorded_at` is the
/// heartbeat the blind-window report reads, `occurred_at` the event time.
fn written_event(
    event_type: &str,
    member_id: &str,
    occurred: &str,
    recorded: &str,
    source: &str,
    metadata: &str,
) -> String {
    let meta = metadata.replace('\'', "''");
    format!(
        "('{event_type}', '{member_id}', '{GUILD}', '{occurred}'::timestamptz, \
         '{recorded}'::timestamptz, '{source}', '{meta}', \
         '{GUILD}:{member_id}:{event_type}:{occurred}')"
    )
}

/// Events-write gaps against a healthy probe table (TOG-5683). The write
/// series (every row's `recorded_at` equals its `occurred_at` except the
/// backfilled row) runs 08:00 to 10:00, falls silent until 15:00 (5h), runs to
/// 17:00, falls silent until 20:30 (3.5h) and resumes. An hourly presence
/// probe reads right through both silences; the report must still name them.
/// Unknown-start ends: two in window one, one (with a numeric duration the
/// flag must still exclude) in window two, one before every window.
async fn blind_window_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    db.seed(include_str!("../migrations/0310_presence_probe.sql").to_owned())
        .await?;
    let probes: Vec<String> = (8..=21)
        .map(|hour| format!("('{GUILD}', '2026-09-20T{hour:02}:00:00.000Z', 120, NULL)"))
        .collect();
    db.seed(format!(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor) VALUES {}",
        probes.join(",")
    ))
    .await?;

    let unknown = r#"{"startKnown":false,"startedAt":null,"durationSeconds":null}"#;
    let live = |kind: &str, member: &str, at: &str, source: &str, meta: &str| {
        written_event(kind, member, at, at, source, meta)
    };
    let rows = [
        // The write series: plain proof-of-life rows on the hour.
        live(
            "member_join",
            "hb1",
            "2026-09-20T08:00:00Z",
            "gateway",
            "{}",
        ),
        live(
            "member_join",
            "hb2",
            "2026-09-20T09:00:00Z",
            "gateway",
            "{}",
        ),
        live(
            "member_join",
            "hb3",
            "2026-09-20T10:00:00Z",
            "gateway",
            "{}",
        ),
        live(
            "member_join",
            "hb4",
            "2026-09-20T15:00:00Z",
            "gateway",
            "{}",
        ),
        live(
            "member_join",
            "hb5",
            "2026-09-20T16:00:00Z",
            "gateway",
            "{}",
        ),
        live(
            "member_join",
            "hb6",
            "2026-09-20T17:00:00Z",
            "gateway",
            "{}",
        ),
        live(
            "member_join",
            "hb7",
            "2026-09-20T20:30:00Z",
            "gateway",
            "{}",
        ),
        // A backfilled row: the event happened at 12:30, inside window one,
        // but it was written at 16:30. The series is when WE wrote, so it is
        // no proof of life at 12:30 and window one stays whole.
        written_event(
            "member_join",
            "bf1",
            "2026-09-20T12:30:00Z",
            "2026-09-20T16:30:00Z",
            "backfill:log:x",
            "{}",
        ),
        // Before every window: unknown start, unattributed.
        live(
            "voice_session_end",
            "m-e",
            "2026-09-20T09:30:00Z",
            "channel:ch-a",
            unknown,
        ),
        // Known-start sessions (never counted as unknown, both measured).
        live(
            "voice_session_start",
            "m-f",
            "2026-09-20T08:10:00Z",
            "channel:ch-a",
            "{}",
        ),
        live(
            "voice_session_end",
            "m-f",
            "2026-09-20T08:40:00Z",
            "channel:ch-a",
            r#"{"startKnown":true,"startedAt":"2026-09-20T08:10:00.000Z","durationSeconds":1800}"#,
        ),
        live(
            "voice_session_start",
            "m-d",
            "2026-09-20T15:01:00Z",
            "channel:ch-a",
            "{}",
        ),
        live(
            "voice_session_end",
            "m-d",
            "2026-09-20T15:11:00Z",
            "channel:ch-a",
            r#"{"startKnown":true,"startedAt":"2026-09-20T15:01:00.000Z","durationSeconds":600}"#,
        ),
        // Window one: the bot came back and met members already in voice.
        live(
            "voice_session_end",
            "m-a",
            "2026-09-20T15:05:00Z",
            "channel:ch-a",
            unknown,
        ),
        live(
            "voice_session_end",
            "m-b",
            "2026-09-20T15:30:00Z",
            "channel:ch-b",
            unknown,
        ),
        // Window two: the flag decides, so a number on the row changes nothing.
        live(
            "voice_session_end",
            "m-c",
            "2026-09-20T20:40:00Z",
            "channel:ch-a",
            r#"{"startKnown":false,"startedAt":null,"durationSeconds":99999}"#,
        ),
    ];
    db.seed(format!(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, recorded_at, source, metadata, idempotency_key) VALUES {}",
        rows.join(",")
    ))
    .await?;

    // The fixture's premise: the probe table is healthy straight through both
    // silences, so a probe-sourced heartbeat would have hidden the gaps.
    let probe_in_gap_one: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM presence_probe WHERE observed_at > '2026-09-20T10:00:00.000Z' AND observed_at < '2026-09-20T15:00:00.000Z'",
    )
    .fetch_one(&db.pool)
    .await?;
    let probe_in_gap_two: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM presence_probe WHERE observed_at > '2026-09-20T17:00:00.000Z' AND observed_at < '2026-09-20T20:30:00.000Z'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(probe_in_gap_one >= 4 && probe_in_gap_two >= 3);
    let distinct_writes: i64 = sqlx::query_scalar("SELECT count(DISTINCT recorded_at) FROM events")
        .fetch_one(&db.pool)
        .await?;
    let seeded = db.snapshot().await?;

    let out = successful_stdout(&db.run(&["voice-reconcile", "--guild", GUILD], &[]));
    let report: serde_json::Value = serde_json::from_str(&out)?;
    let blind = &report["blindWindows"];
    assert_eq!(blind["heartbeatSource"], "events.recorded_at");
    assert_eq!(blind["heartbeats"].as_i64(), Some(distinct_writes));
    assert_eq!(blind["maxGapMs"].as_i64(), Some(2 * 3_600_000));
    // Window one is the whole 5h silence: the 12:30-occurred backfill row was
    // recorded at 16:30, so an `occurred_at` series would have split it.
    assert_eq!(
        blind["windows"],
        serde_json::json!([
            {
                "start": "2026-09-20T10:00:00.000Z",
                "end": "2026-09-20T15:00:00.000Z",
                "gapMs": 5 * 3_600_000,
                "unknownStarts": 2,
            },
            {
                "start": "2026-09-20T17:00:00.000Z",
                "end": "2026-09-20T20:30:00.000Z",
                "gapMs": 3 * 3_600_000 + 1_800_000,
                "unknownStarts": 1,
            },
        ])
    );
    assert_eq!(blind["unattributedUnknownStarts"].as_u64(), Some(1));
    // Known-start sessions are not counted: m-d and m-f appear nowhere above.
    // Durations: only the two measured known starts enter the mean; the four
    // unknown starts (one carrying 99999s) are excluded and counted.
    assert_eq!(report["durations"]["measured"].as_u64(), Some(2));
    assert_eq!(report["durations"]["averageSeconds"].as_f64(), Some(1200.0));
    assert_eq!(
        report["durations"]["excludedUnknownStarts"].as_u64(),
        Some(4)
    );
    assert_eq!(report["discordRequests"].as_u64(), Some(0));

    // A wider threshold swallows both silences; the unknown starts stay
    // counted, now all unattributed.
    let wide = successful_stdout(&db.run(
        &[
            "voice-reconcile",
            "--guild",
            GUILD,
            "--max-gap-minutes",
            "400",
        ],
        &[],
    ));
    let wide: serde_json::Value = serde_json::from_str(&wide)?;
    assert_eq!(
        wide["blindWindows"]["maxGapMs"].as_i64(),
        Some(400 * 60_000)
    );
    assert_eq!(wide["blindWindows"]["windows"], serde_json::json!([]));
    assert_eq!(
        wide["blindWindows"]["unattributedUnknownStarts"].as_u64(),
        Some(4)
    );
    // A 4h30m threshold keeps only the 5h silence. The 20:40 end now belongs
    // to it too (the latest window starting at or before an end claims it);
    // the 09:30 end predates it and stays unattributed.
    let narrow = successful_stdout(&db.run(
        &[
            "voice-reconcile",
            "--guild",
            GUILD,
            "--max-gap-minutes",
            "270",
        ],
        &[],
    ));
    let narrow: serde_json::Value = serde_json::from_str(&narrow)?;
    let narrow_windows = narrow["blindWindows"]["windows"].as_array().unwrap();
    assert_eq!(narrow_windows.len(), 1);
    assert_eq!(narrow_windows[0]["start"], "2026-09-20T10:00:00.000Z");
    assert_eq!(narrow_windows[0]["unknownStarts"].as_u64(), Some(3));
    assert_eq!(
        narrow["blindWindows"]["unattributedUnknownStarts"].as_u64(),
        Some(1)
    );

    // Read-only: no ledger table appears and every table is byte-identical,
    // the probe table included.
    let migrated: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname = 'public' AND tablename = '_sqlx_migrations')",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(!migrated, "report ran migrations");
    assert_eq!(db.snapshot().await?, seeded, "report wrote rows");
    Ok(())
}

/// Freeze the real CLI between its end and heartbeat reads. An owned view
/// waits on an advisory lock only when the third (leave) feed reads its row;
/// no timing assumption or production test hook is needed.
async fn snapshot_consistency_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    let rows = [
        written_event(
            "voice_session_start",
            "known",
            "2026-09-20T09:00:00Z",
            "2026-09-20T09:00:00Z",
            "channel:ch-a",
            "{}",
        ),
        written_event(
            "voice_session_end",
            "known",
            "2026-09-20T09:30:00Z",
            "2026-09-20T09:30:00Z",
            "channel:ch-a",
            r#"{"startKnown":true,"startedAt":"2026-09-20T09:00:00Z","durationSeconds":1800}"#,
        ),
        written_event(
            "member_leave",
            "sentinel",
            "2026-09-20T09:45:00Z",
            "2026-09-20T10:00:00Z",
            "gateway",
            "{}",
        ),
    ];
    db.seed(format!(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, recorded_at, source, metadata, idempotency_key) VALUES {}",
        rows.join(",")
    ))
    .await?;
    db.seed(
        r#"ALTER TABLE events RENAME TO fixture_events;
           CREATE FUNCTION report_leave_barrier(at timestamptz) RETURNS timestamptz
           LANGUAGE plpgsql VOLATILE AS $$
           BEGIN
               PERFORM pg_advisory_xact_lock(54801);
               RETURN at;
           END;
           $$;
           CREATE VIEW events AS
           SELECT event_type, member_id, guild_id, source, metadata, recorded_at,
                  CASE WHEN event_type = 'member_leave'
                       THEN report_leave_barrier(occurred_at)
                       ELSE occurred_at END AS occurred_at
           FROM fixture_events;"#
            .to_owned(),
    )
    .await?;
    let initial = db.snapshot().await?;
    let baseline: serde_json::Value = serde_json::from_str(&successful_stdout(
        &db.run(&["voice-reconcile", "--guild", GUILD], &[]),
    ))?;
    assert_eq!(baseline["blindWindows"]["windows"], serde_json::json!([]));
    assert_eq!(baseline["blindWindows"]["heartbeats"], 3);
    assert_eq!(baseline["durations"]["excludedUnknownStarts"], 0);
    assert_eq!(db.snapshot().await?, initial);

    let mut gate = db.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(54801)")
        .execute(&mut *gate)
        .await?;
    let reader_db = db.clone();
    let reader = tokio::task::spawn_blocking(move || {
        reader_db.run(&["voice-reconcile", "--guild", GUILD], &[])
    });
    // Always release the barrier and join the CLI before propagating a failed
    // rendezvous or writer. That keeps the owned fixture safe to tear down.
    let committed: Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                     WHERE datname = $1 AND wait_event = 'advisory'
                       AND query LIKE '%event_type = ''member_leave''%')",
                )
                .bind(&db.name)
                .fetch_one(&db.pool)
                .await?;
                if waiting {
                    return Ok::<_, sqlx::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        // One commit while the report's first two feeds are already read.
        // The new end supplies both the 15:00 write and its unknown-start count.
        let new_rows = [
            written_event(
                "voice_session_start",
                "new-open",
                "2026-09-20T12:00:00Z",
                "2026-09-20T15:00:00Z",
                "channel:ch-b",
                "{}",
            ),
            written_event(
                "voice_session_end",
                "new-unknown",
                "2026-09-20T15:00:00Z",
                "2026-09-20T15:00:00Z",
                "channel:ch-c",
                r#"{"startKnown":false,"startedAt":null,"durationSeconds":99999}"#,
            ),
            written_event(
                "member_leave",
                "new-leave",
                "2026-09-20T15:10:00Z",
                "2026-09-20T15:00:00Z",
                "gateway",
                "{}",
            ),
        ];
        db.seed(format!(
            "INSERT INTO fixture_events (event_type, member_id, guild_id, occurred_at, recorded_at, source, metadata, idempotency_key) VALUES {}",
            new_rows.join(",")
        ))
        .await?;
        db.snapshot().await.map_err(Into::into)
    }
    .await;
    let unlocked = gate.rollback().await;
    let output = reader.await?;
    unlocked?;
    let after_write = committed?;
    let during: serde_json::Value = serde_json::from_str(&successful_stdout(&output))?;
    assert_eq!(during, baseline, "report mixed pre/post-commit feeds");
    assert_eq!(db.snapshot().await?, after_write, "report wrote rows");

    // The next sweep sees the whole commit, never just its heartbeat.
    let after: serde_json::Value = serde_json::from_str(&successful_stdout(
        &db.run(&["voice-reconcile", "--guild", GUILD], &[]),
    ))?;
    assert_eq!(after["blindWindows"]["heartbeats"], 4);
    assert_eq!(
        after["blindWindows"]["windows"],
        serde_json::json!([{
            "start": "2026-09-20T10:00:00.000Z",
            "end": "2026-09-20T15:00:00.000Z",
            "gapMs": 5 * 3_600_000,
            "unknownStarts": 1,
        }])
    );
    assert_eq!(after["durations"]["excludedUnknownStarts"], 1);
    assert_eq!(after["durations"]["averageSeconds"], 1800.0);
    assert_eq!(after["durations"]["measured"], 1);
    assert_eq!(db.snapshot().await?, after_write);
    assert!(after_write.get("_sqlx_migrations").is_none());
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
    db.seed(format!(
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
    assert!(String::from_utf8_lossy(&help.stdout).contains("voice-ghosts"));

    let unknown = db.run(&["bogus-report"], &[]);
    assert_eq!(unknown.status.code(), Some(2));

    let bad_days = db.run(
        &["voice-reconcile", "--guild", GUILD, "--days", "bogus"],
        &[],
    );
    assert_eq!(bad_days.status.code(), Some(2));

    for bad_gap in ["0", "-5", "soon"] {
        let out = db.run(
            &[
                "voice-reconcile",
                "--guild",
                GUILD,
                "--max-gap-minutes",
                bad_gap,
            ],
            &[],
        );
        assert_eq!(out.status.code(), Some(2), "--max-gap-minutes {bad_gap}");
    }

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
    // The seeded write series has one 3.5h silence holding both unknown ends,
    // and the average keeps m1 (1800s) and m5 (300s) only.
    let seed_windows = seed_report["blindWindows"]["windows"].as_array().unwrap();
    assert_eq!(seed_windows.len(), 1);
    assert_eq!(seed_windows[0]["unknownStarts"], 2);
    assert_eq!(seed_report["blindWindows"]["unattributedUnknownStarts"], 0);
    assert_eq!(seed_report["durations"]["averageSeconds"], 1050.0);
    assert_eq!(seed_report["durations"]["excludedUnknownStarts"], 2);

    let seed_gap = successful_stdout(&db.run(&["leave-gap", "--seed"], &[]));
    let seed_gap_report: serde_json::Value = serde_json::from_str(&seed_gap)?;
    assert_eq!(seed_gap_report["mode"], "seeded-demo");
    assert_eq!(seed_gap_report["gaps"].as_array().unwrap().len(), 4);
    assert_eq!(seed_gap_report["fillsProposed"], 5);

    // The ghost seed needs no database and no Discord: pure existence diff.
    let seed_ghosts = successful_stdout(&db.run(&["voice-ghosts", "--seed"], &[]));
    let seed_ghost_report: serde_json::Value = serde_json::from_str(&seed_ghosts)?;
    assert_eq!(seed_ghost_report["tool"], "voice-ghosts");
    assert_eq!(seed_ghost_report["mode"], "seeded-demo");
    assert_eq!(seed_ghost_report["tracked_rooms"], 3);
    assert_eq!(
        seed_ghost_report["tracked_present"],
        serde_json::json!(["101", "102"])
    );
    assert_eq!(
        seed_ghost_report["tracked_gone"],
        serde_json::json!(["103"])
    );
    assert_eq!(
        seed_ghost_report["untracked_present"],
        serde_json::json!(["201"])
    );
    assert_eq!(seed_ghost_report["clean"], false);
    assert_eq!(seed_ghost_report["discordRequests"], 0);
    Ok(())
}

/// Two tracked rooms plus one live listing: 101 still exists, 102 was
/// deleted by hand, 201 was never tracked. Voice (type 2) and stage (13)
/// count as live; the text channel (0) and category (4) do not.
fn mock_channels() -> (String, Arc<AtomicUsize>) {
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
            let path = req.lines().next().unwrap_or_default().to_owned();
            assert!(
                path.contains("/api/v10/guilds/"),
                "unexpected channel request: {path}"
            );
            c.fetch_add(1, Ordering::SeqCst);
            let body = r#"[
                {"id": "101", "type": 2, "name": "room-101"},
                {"id": "201", "type": 2, "name": "lounge"},
                {"id": "202", "type": 13, "name": "stage"},
                {"id": "301", "type": 0, "name": "general"},
                {"id": "302", "type": 4, "name": "voice rooms"}
            ]"#;
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (base, count)
}

fn room_row(channel: &str) -> String {
    format!(
        "('{GUILD}', '{channel}', '700', '410000000000000001', '410000000000000001', \
         '7', '2026-09-20T12:00:00Z'::timestamptz)"
    )
}

async fn seed_rooms(db: &std::sync::Arc<TestDb>) -> TestResult {
    db.seed(
        "CREATE TABLE IF NOT EXISTS voice_rooms (guild_id TEXT NOT NULL, \
         channel_id TEXT NOT NULL, creator_channel_id TEXT NOT NULL, \
         owner_id TEXT NOT NULL, original_creator_id TEXT NOT NULL, \
         name_seed TEXT NOT NULL, created_at timestamptz NOT NULL, \
         PRIMARY KEY (guild_id, channel_id))"
            .to_owned(),
    )
    .await?;
    db.seed(format!(
        "INSERT INTO voice_rooms (guild_id, channel_id, creator_channel_id, \
         owner_id, original_creator_id, name_seed, created_at) VALUES {}, {}",
        room_row("101"),
        room_row("102"),
    ))
    .await
}

async fn ghost_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    seed_rooms(&db).await?;
    let seeded = db.snapshot().await?;
    let (base, count) = mock_channels();

    let out = successful_stdout(&db.run(
        &[
            "voice-ghosts",
            "--guild",
            GUILD,
            "--discord-base",
            base.as_str(),
        ],
        &[],
    ));
    let report: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(report["tool"], "voice-ghosts");
    assert_eq!(report["guild"], GUILD);
    assert_eq!(report["tracked_rooms"], 2);
    // Live voice is 101, 201 and the stage channel 202: text and category
    // never count.
    assert_eq!(report["live_voice_channels"], 3);
    assert_eq!(report["tracked_present"], serde_json::json!(["101"]));
    assert_eq!(report["tracked_gone"], serde_json::json!(["102"]));
    assert_eq!(
        report["untracked_present"],
        serde_json::json!(["201", "202"])
    );
    assert_eq!(report["clean"], false);
    assert_eq!(report["discordRequests"], 1);
    assert_eq!(count.load(Ordering::SeqCst), 1, "exactly one channel GET");

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

async fn ceiling_scenario(db: std::sync::Arc<TestDb>) -> TestResult {
    seed_gap(&db).await?;
    // Every roster page is full and advancing (1000 fresh members per
    // page), so the 20-page cap trips: the sweep refuses the partial roster
    // loudly instead of counting it.
    const BASE: u64 = 200_000_000_000_000_000;
    let (base, count) = mock_roster(|after| {
        let page = if after == 0 { 0 } else { (after - BASE) / 1000 };
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
async fn voice_reconcile_reports_events_write_gaps_with_unknown_start_counts_read_only(
) -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db("two_bot_test_report_blind", blind_window_scenario).await
}

#[tokio::test]
async fn voice_reconcile_keeps_one_snapshot_across_a_concurrent_commit() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db(
        "two_bot_test_report_snapshot",
        snapshot_consistency_scenario,
    )
    .await
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
async fn voice_ghosts_counts_tracked_vs_live_read_only() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db("two_bot_test_report_ghosts", ghost_scenario).await
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
