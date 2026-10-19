//! Acceptance: the backfill CLIs fail explicitly when discovery is
//! unavailable, instead of silently scanning nothing and reporting success.
//!
//! Guild-channel discovery feeds both `backfill` and `backfill-messages`;
//! active-thread discovery feeds `backfill-messages`. An error, an
//! unreadable/None response, or a dropped connection on either step must
//! make the process exit nonzero with a fixed, step-specific diagnostic
//! that echoes no remote body, credential or database detail — and must
//! never print a successful exact-completeness claim (`exactly`). A
//! successful *empty* discovery result is a different outcome and keeps
//! its current success semantics, as do successful non-empty scans.
//!
//! The tests drive the real binaries (same CLI harness shape as
//! `report_cli.rs`): a loopback mock Discord REST selected with
//! `--discord-base`, `--dry-run` so no backfill rows are written, and a
//! disposable agent-testdb database the binary migrates itself.
//! `TWO_TEST_DATABASE_URL` is the opt-in: without it the tests skip, with
//! it (CI's `--test '*'` integration step) they run for real.

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
const TOKEN: &str = "test-token-not-a-secret";
/// Planted in mocked error/unreadable bodies: no diagnostic may echo it.
const REMOTE_MARKER: &str = "REMOTE-BODY-MARKER-7f3d9a";
const CHANNEL_ID: &str = "300000000000000001";

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

static NEXT_DB: AtomicUsize = AtomicUsize::new(0);

struct TestDb {
    admin: PgPool,
    name: String,
    url: String,
}

impl TestDb {
    /// An empty disposable database; the CLI under test applies the
    /// migrations itself on its first run, exactly as an operator run
    /// against a fresh database would.
    async fn new(prefix: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let name = format!(
            "{prefix}_{}_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            NEXT_DB.fetch_add(1, Ordering::Relaxed)
        );
        assert!(name.len() <= 63, "database name too long: {name}");
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
            .connect_with(options.database("postgres"))
            .await?;
        sqlx::query(database_sql("CREATE DATABASE", &name))
            .execute(&admin)
            .await?;
        Ok(Self { admin, name, url })
    }

    async fn cleanup(&self) -> TestResult {
        let dropped = sqlx::query(database_sql("DROP DATABASE", &self.name))
            .execute(&self.admin)
            .await
            .map(|_| ());
        self.admin.close().await;
        Ok(dropped?)
    }
}

fn database_sql(operation: &str, name: &str) -> sqlx::AssertSqlSafe<String> {
    assert!(name.starts_with("two_bot_test_") && name.len() <= 63);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    sqlx::AssertSqlSafe(format!("{operation} \"{name}\""))
}

/// Runs a scenario on a spawned task so a panicking assertion still reaches
/// teardown, then reports the scenario failure ahead of any cleanup failure.
async fn with_db<F, Fut>(prefix: &'static str, scenario: F) -> TestResult
where
    F: FnOnce(Arc<TestDb>) -> Fut,
    Fut: std::future::Future<Output = TestResult> + Send + 'static,
{
    let db = Arc::new(TestDb::new(prefix).await?);
    let joined = tokio::spawn(scenario(db.clone())).await;
    let cleaned = db.cleanup().await;
    match joined {
        Ok(result) => result.and(cleaned),
        Err(panic) => Err(format!("scenario panicked: {panic}").into()),
    }
}

/// One mocked route's behavior for a single CLI run.
#[derive(Clone)]
enum Route {
    /// Reply with this HTTP status and JSON body.
    Json(u16, String),
    /// Accept the connection and drop it without any response.
    Drop,
}

#[derive(Default)]
struct Hits {
    members: AtomicUsize,
    channels: AtomicUsize,
    threads_active: AtomicUsize,
    messages: AtomicUsize,
}

/// Loopback Discord stand-in. Routes are matched on the request line; every
/// other guild route the CLIs use (invites, archived threads) answers with
/// a valid empty result so only the route under test can fail the run.
fn mock_discord(members: Route, channels: Route, threads_active: Route) -> (String, Arc<Hits>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let hits = Arc::new(Hits::default());
    let h = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let line = req.lines().next().unwrap_or_default();
            let route = if line.contains("/threads/active") {
                h.threads_active.fetch_add(1, Ordering::SeqCst);
                threads_active.clone()
            } else if line.contains("/members") {
                h.members.fetch_add(1, Ordering::SeqCst);
                members.clone()
            } else if line.contains("/invites") {
                Route::Json(200, "[]".to_owned())
            } else if line.contains("/threads/archived/") {
                Route::Json(
                    200,
                    r#"{"threads":[],"members":[],"has_more":false}"#.to_owned(),
                )
            } else if line.contains("/messages") {
                h.messages.fetch_add(1, Ordering::SeqCst);
                Route::Json(200, "[]".to_owned())
            } else if line.contains("/guilds/") && line.contains("/channels") {
                h.channels.fetch_add(1, Ordering::SeqCst);
                channels.clone()
            } else {
                Route::Json(404, "{}".to_owned())
            };
            match route {
                Route::Drop => {
                    // Close without a response: a transport failure.
                    drop(s);
                }
                Route::Json(status, body) => {
                    let reason = match status {
                        200 => "OK",
                        403 => "Forbidden",
                        404 => "Not Found",
                        _ => "Status",
                    };
                    let _ = write!(
                        s,
                        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            }
        }
    });
    (base, hits)
}

fn member(id: &str) -> String {
    format!(
        r#"{{"user":{{"id":"{id}","username":"u","discriminator":"0","avatar":null}},"roles":[],"joined_at":"2024-01-01T00:00:00.000000+00:00","deaf":false,"mute":false,"flags":0}}"#
    )
}

fn one_member() -> Route {
    Route::Json(200, format!("[{}]", member("100000000000000001")))
}

fn empty_channels() -> Route {
    Route::Json(200, "[]".to_owned())
}

fn empty_threads() -> Route {
    Route::Json(
        200,
        r#"{"threads":[],"members":[],"has_more":false}"#.to_owned(),
    )
}

fn refusal() -> Route {
    Route::Json(
        403,
        format!(r#"{{"message":"{REMOTE_MARKER} missing access","code":50001}}"#),
    )
}

fn unreadable() -> Route {
    Route::Json(200, format!(r#"{{"detail":"{REMOTE_MARKER}"}}"#))
}

fn text_channel(name: &str) -> Route {
    Route::Json(
        200,
        format!(r#"[{{"id":"{CHANNEL_ID}","type":0,"name":"{name}","position":0}}]"#),
    )
}

fn run_cli(db: &TestDb, bin: &str, base: &str) -> Output {
    Command::new(bin)
        .env_clear()
        .env("TWO_DATABASE_URL", &db.url)
        .env("TWO_DATABASE_TLS", "local-only")
        .env("TWO_DB_POOL_MAX", "1")
        .env("DISCORD_TOKEN", TOKEN)
        .env("DISCORD_GUILD_ID", GUILD)
        .args(["--dry-run", "--max-pages=1", "--discord-base", base])
        .output()
        .unwrap()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(case: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{case}: expected exit 0, got {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout_of(output),
        stderr_of(output)
    );
}

/// The failed-discovery contract: nonzero exit, no exact-completeness
/// claim, and one bounded diagnostic that names the failed discovery step
/// without echoing the remote body, the token, or the database URL.
fn assert_discovery_refusal(case: &str, db: &TestDb, output: &Output, step_word: &str) {
    let stdout = stdout_of(output);
    let stderr = stderr_of(output);
    assert!(
        !output.status.success(),
        "{case}: failed discovery must exit nonzero, but the CLI succeeded\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("exactly"),
        "{case}: failed discovery printed an exact-completeness claim\nstdout:\n{stdout}"
    );
    assert!(
        !stderr.trim().is_empty(),
        "{case}: failed discovery printed no diagnostic"
    );
    assert!(
        stderr.len() < 2048,
        "{case}: diagnostic is not bounded ({} bytes):\n{stderr}",
        stderr.len()
    );
    let lower = stderr.to_lowercase();
    assert!(
        lower.contains(step_word),
        "{case}: diagnostic does not name the {step_word} discovery step:\n{stderr}"
    );
    assert!(
        [
            "discover",
            "unavailable",
            "failed",
            "could not",
            "refus",
            "unreadable"
        ]
        .iter()
        .any(|w| lower.contains(w)),
        "{case}: diagnostic does not say discovery failed:\n{stderr}"
    );
    for forbidden in [REMOTE_MARKER, TOKEN, db.url.as_str()] {
        assert!(
            !stdout.contains(forbidden) && !stderr.contains(forbidden),
            "{case}: output echoes forbidden detail {forbidden:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

async fn messages_scenario(db: Arc<TestDb>) -> TestResult {
    let bin = env!("CARGO_BIN_EXE_backfill-messages");

    // Control: a successful empty discovery keeps its success semantics,
    // including the exact-completeness wording for a complete (empty) scan.
    let (base, hits) = mock_discord(one_member(), empty_channels(), empty_threads());
    let out = run_cli(&db, bin, &base);
    assert_success("messages valid-empty control", &out);
    assert!(
        stdout_of(&out).contains("exactly"),
        "messages valid-empty control lost its exact wording:\n{}",
        stdout_of(&out)
    );
    assert_eq!(hits.channels.load(Ordering::SeqCst), 1, "channel discovery");
    assert_eq!(
        hits.threads_active.load(Ordering::SeqCst),
        1,
        "active-thread discovery"
    );

    // Control: one conversation channel with an empty history still scans
    // exactly one channel and completes exactly.
    let (base, hits) = mock_discord(one_member(), text_channel("general"), empty_threads());
    let out = run_cli(&db, bin, &base);
    assert_success("messages one-channel control", &out);
    let stdout = stdout_of(&out);
    let scanned = stdout
        .lines()
        .find(|l| l.contains("channels scanned"))
        .unwrap_or_default()
        .to_owned();
    assert!(
        scanned.trim_end().ends_with('1'),
        "messages one-channel control scanned line: {scanned:?}\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("exactly"),
        "messages one-channel control lost its exact wording:\n{stdout}"
    );
    assert!(hits.messages.load(Ordering::SeqCst) >= 1, "channel scanned");

    // Failed guild-channel discovery: refusal, unreadable body, transport.
    for (case, channels) in [
        ("messages channels refusal", refusal()),
        ("messages channels unreadable", unreadable()),
        ("messages channels transport", Route::Drop),
    ] {
        let (base, hits) = mock_discord(one_member(), channels, empty_threads());
        let out = run_cli(&db, bin, &base);
        assert!(
            hits.channels.load(Ordering::SeqCst) >= 1,
            "{case}: channel discovery was never attempted"
        );
        assert_discovery_refusal(case, &db, &out, "channel");
    }

    // Failed active-thread discovery with channel discovery healthy.
    for (case, threads) in [
        ("messages active-threads refusal", refusal()),
        ("messages active-threads unreadable", unreadable()),
        ("messages active-threads transport", Route::Drop),
    ] {
        let (base, hits) = mock_discord(one_member(), empty_channels(), threads);
        let out = run_cli(&db, bin, &base);
        assert!(
            hits.threads_active.load(Ordering::SeqCst) >= 1,
            "{case}: active-thread discovery was never attempted"
        );
        assert_discovery_refusal(case, &db, &out, "thread");
    }
    Ok(())
}

async fn backfill_scenario(db: Arc<TestDb>) -> TestResult {
    let bin = env!("CARGO_BIN_EXE_backfill");

    // Control: one listed member, a successful empty channel discovery,
    // and an empty invite list complete successfully.
    let (base, hits) = mock_discord(one_member(), empty_channels(), empty_threads());
    let out = run_cli(&db, bin, &base);
    assert_success("backfill valid-empty control", &out);
    assert!(
        stdout_of(&out).contains("Discord requests"),
        "backfill valid-empty control did not finish its report:\n{}",
        stdout_of(&out)
    );
    assert!(hits.members.load(Ordering::SeqCst) >= 1, "members listed");
    assert_eq!(hits.channels.load(Ordering::SeqCst), 1, "channel discovery");

    // Control: one log channel with an empty history is probed and the run
    // still completes.
    let (base, hits) = mock_discord(one_member(), text_channel("member-log"), empty_threads());
    let out = run_cli(&db, bin, &base);
    assert_success("backfill one-channel control", &out);
    assert!(
        stdout_of(&out).contains("Discord requests"),
        "backfill one-channel control did not finish its report:\n{}",
        stdout_of(&out)
    );
    assert!(
        hits.messages.load(Ordering::SeqCst) >= 1,
        "log channel probed"
    );

    // Failed guild-channel discovery: refusal, unreadable body, transport.
    // The member list stays healthy so the run reaches channel discovery.
    for (case, channels) in [
        ("backfill channels refusal", refusal()),
        ("backfill channels unreadable", unreadable()),
        ("backfill channels transport", Route::Drop),
    ] {
        let (base, hits) = mock_discord(one_member(), channels, empty_threads());
        let out = run_cli(&db, bin, &base);
        assert!(
            hits.channels.load(Ordering::SeqCst) >= 1,
            "{case}: channel discovery was never attempted"
        );
        assert_discovery_refusal(case, &db, &out, "channel");
    }
    Ok(())
}

#[tokio::test]
async fn backfill_messages_discovery_failures_refuse_and_empty_discovery_succeeds() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db("two_bot_test_bfmsg_disc", messages_scenario).await
}

#[tokio::test]
async fn backfill_discovery_failures_refuse_and_empty_discovery_succeeds() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    with_db("two_bot_test_bf_disc", backfill_scenario).await
}
