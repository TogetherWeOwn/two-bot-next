//! Protected acceptance tests: `rollback-delta import` replays a
//! `rollback-delta --export` NDJSON file into a target database.
//!
//! Contract under test (from the card spec, blind to the implementation).
//! The import is a subcommand of the existing `rollback-delta` binary:
//!
//! ```text
//! rollback-delta import --file <export.ndjson> [--apply] [--allow-live-target]
//! ```
//!
//! - Default is a dry run. It validates the whole file and prints a
//!   row-count plan as JSON on stdout — `{"version": 1, "mode": "dry-run",
//!   "tables": [{"table": ..., "rows": N}, ...], "total_rows": N}` — without
//!   opening a database (the `legacy-copy --plan` offline precedent) and
//!   without writing anything when a database is configured. With
//!   `--apply` the same plan shape is printed with `"mode": "apply"`,
//!   before any mutation.
//! - File validation refuses before any write, exit 2, nothing on stdout,
//!   with a named error per class on stderr:
//!   - malformed file (a line that is not a `{"table", "row"}` object with
//!     an object row): stderr names `malformed import file` and the
//!     1-based `line <n>`;
//!   - a table the exporter cannot emit (not a measured rollback-delta
//!     table): stderr names `unknown table` and the table;
//!   - more rows for one table than the export cap
//!     (`MAX_EXPORT_ROWS_PER_TABLE`, 50,000): stderr names
//!     `row cap exceeded` and the table.
//! - `--apply` against a live/unknown target without `--allow-live-target`
//!   refuses before any connection attempt, exit 2, naming
//!   `--allow-live-target` (the `legacy_copy::guarded_target` contract).
//!   The disposable `agent-testdb` targets used below pass `--apply`
//!   together with `--allow-live-target`, which must be accepted.
//! - Round-trip: rows exported by the existing `export_delta` from a
//!   seeded source, imported with `--apply` into an empty disposable
//!   target, reproduce exactly the measured post-`T_f` rows there (the
//!   target's own delta report recounts them). No migrations are applied
//!   by the import (no `_sqlx_migrations` ledger appears).
//! - Re-running the same import is idempotent: exit 0 and the target rows
//!   are unchanged (no duplicates, no error).
//! - A file that turns malformed after valid lines writes nothing at all
//!   on `--apply`: the refusal precedes the first write (or the write is
//!   one rolled-back transaction — externally indistinguishable, either
//!   satisfies "refuse before any write").
//!
//! Explicitly unpinned (builder's choice, do not infer from these tests):
//! handling of unmeasurable (never-exported) table names beyond refusing
//! names outside the exporter's vocabulary, conflict policy when a target
//! row already exists with different values, serial-PK (`BIGSERIAL id`)
//! handling, and any stdout fields beyond the plan shape above.
//!
//! PROTECTED: the paired builder implements production code against this
//! file and the fixtures in `fixtures/rollback_import_*.ndjson` unchanged
//! (see `fixtures/rollback_import_manifest.md`). Do not edit these tests
//! to make an implementation pass; if a test is wrong, that is a finding
//! for the test author, not a builder edit.
//!
//! The CLI-only tests need no database and run everywhere. The database
//! tests skip when `TWO_TEST_DATABASE_URL` is absent and otherwise use
//! one owned disposable database per role on `agent-testdb` (the
//! `game_activity_report_cli` pattern); never staging or production.
//! Locally:
//! `TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/agent_test
//! python3 scripts/cargo_cache.py run -- test -p two-bot-cutover
//! --test rollback_delta_import_acceptance`

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use two_bot_cutover::legacy_copy::options::guarded_target;
use two_bot_cutover::rollback_delta::{export_delta, report, MAX_EXPORT_ROWS_PER_TABLE};

/// The `T_f` baseline every seed is measured against (as in
/// `rollback_delta_db`).
const SINCE: &str = "2026-09-30T12:00:00Z";

/// Measured tables the round-trip exercises: natural primary keys only, so
/// no serial-column policy is pinned.
const ROUND_TRIP_TABLES: &[&str] = &["member_levels", "members", "tickets"];

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn scratch_base() -> PathBuf {
    // Prefer the run-scoped scratch dir when the harness provides it so test
    // trees stay out of container /tmp; fall back to temp_dir locally.
    std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
}

/// One scratch NDJSON file, removed on drop.
struct ScratchFile {
    path: PathBuf,
}

impl ScratchFile {
    fn write(tag: &str, contents: &str) -> Self {
        let path = scratch_base().join(format!(
            "rollback-import-{tag}-{}-{}.ndjson",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, contents).expect("write scratch import file");
        Self { path }
    }

    fn arg(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// Run `rollback-delta import ...` with a cleared environment (plus any
/// `TWO_DATABASE_URL` the caller supplies via `db_url`).
fn run_import(args: &[&str], db_url: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rollback-delta"));
    command.env_clear().arg("import").args(args);
    if let Some(url) = db_url {
        command
            .env("TWO_DATABASE_URL", url)
            .env("TWO_DATABASE_TLS", "local-only")
            .env("TWO_DB_POOL_MAX", "1");
    }
    command.output().expect("rollback-delta binary")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A refused import: exit 2, empty stdout, named error on stderr.
fn assert_refusal(output: &Output, tokens: &[&str]) {
    let stderr = stderr_of(output);
    assert_eq!(
        output.status.code(),
        Some(2),
        "refusal must exit 2: {stderr}"
    );
    assert!(output.stdout.is_empty(), "refusal writes no stdout plan");
    for token in tokens {
        assert!(
            stderr.contains(token),
            "stderr must name {token:?}: {stderr}"
        );
    }
}

/// The plan JSON printed by a successful run, as (mode, per-table rows,
/// total rows).
fn parse_plan(output: &Output) -> (String, BTreeMap<String, u64>, u64) {
    assert!(
        output.status.success(),
        "import failed (status {:?}): {}",
        output.status.code(),
        stderr_of(output)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is the JSON plan");
    assert_eq!(plan["version"], 1, "plan: {plan}");
    let tables: BTreeMap<String, u64> = plan["tables"]
        .as_array()
        .expect("plan tables is an array")
        .iter()
        .map(|entry| {
            (
                entry["table"].as_str().expect("plan table name").to_owned(),
                entry["rows"].as_u64().expect("plan table row count"),
            )
        })
        .collect();
    (
        plan["mode"].as_str().expect("plan mode").to_owned(),
        tables,
        plan["total_rows"].as_u64().expect("plan total_rows"),
    )
}

#[test]
fn import_dry_run_plan_counts_rows_per_table_offline() {
    // No TWO_DATABASE_URL at all: the dry-run plan is computed from the
    // file alone and opens no database.
    let output = run_import(&["--file", &fixture("rollback_import_valid.ndjson")], None);
    let (mode, tables, total) = parse_plan(&output);
    assert_eq!(mode, "dry-run");
    assert_eq!(
        tables,
        BTreeMap::from([
            ("member_levels".to_owned(), 2),
            ("members".to_owned(), 1),
            ("tickets".to_owned(), 1),
        ])
    );
    assert_eq!(total, 4);
}

#[test]
fn import_malformed_file_refuses_with_named_error() {
    // Line 2 of the fixture is not JSON; valid lines surround it.
    let output = run_import(
        &["--file", &fixture("rollback_import_malformed.ndjson")],
        None,
    );
    assert_refusal(&output, &["malformed import file", "line 2"]);

    // A JSON line missing the `row` object is malformed too.
    let missing_row = ScratchFile::write("missing-row", "{\"table\":\"member_levels\"}\n");
    let output = run_import(&["--file", &missing_row.arg()], None);
    assert_refusal(&output, &["malformed import file", "line 1"]);

    // A non-object `row` is malformed, never half-imported.
    let scalar_row = ScratchFile::write("scalar-row", "{\"table\":\"member_levels\",\"row\":42}\n");
    let output = run_import(&["--file", &scalar_row.arg()], None);
    assert_refusal(&output, &["malformed import file", "line 1"]);
}

#[test]
fn import_unknown_table_refuses_with_named_error() {
    let output = run_import(
        &["--file", &fixture("rollback_import_unknown_table.ndjson")],
        None,
    );
    assert_refusal(&output, &["unknown table", "no_such_table"]);
}

#[test]
fn import_row_cap_overrun_refuses_with_named_error() {
    // One row past the per-table export cap, generated rather than
    // committed as a 50k-line fixture.
    let mut contents = String::new();
    for index in 0..=MAX_EXPORT_ROWS_PER_TABLE {
        contents.push_str(
            &serde_json::json!({
                "table": "member_levels",
                "row": {
                    "guild_id": "g",
                    "member_id": format!("m{index}"),
                    "xp": 1,
                    "message_xp": 1,
                    "voice_xp": 0,
                    "imported_xp": 0,
                    "updated_at": "2026-10-01T12:00:00+00:00",
                },
            })
            .to_string(),
        );
        contents.push('\n');
    }
    let over_cap = ScratchFile::write("over-cap", &contents);
    let output = run_import(&["--file", &over_cap.arg()], None);
    assert_refusal(&output, &["row cap exceeded", "member_levels"]);
}

#[test]
fn import_apply_without_live_target_opt_in_refuses_before_connecting() {
    // A live/unknown target (never resolved or dialed): the refusal must
    // precede any connection attempt, so no database-open error may appear
    // and the URL must not be echoed.
    let output = run_import(
        &[
            "--file",
            &fixture("rollback_import_valid.ndjson"),
            "--apply",
        ],
        Some("postgres://fake:target-secret@live.invalid:5432/target"),
    );
    assert_refusal(&output, &["--allow-live-target"]);
    let stderr = stderr_of(&output);
    assert!(
        !stderr.contains("cannot open database"),
        "refusal precedes the connection attempt: {stderr}"
    );
    assert!(
        !stderr.contains("target-secret") && !stderr.contains("live.invalid"),
        "refusal must not echo the database URL: {stderr}"
    );
}

#[test]
fn import_missing_file_argument_and_unreadable_file_refuse() {
    let output = run_import(&[], None);
    assert_refusal(&output, &["--file"]);

    let missing = scratch_base()
        .join(format!(
            "rollback-import-absent-{}-{}.ndjson",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
        .to_string_lossy()
        .into_owned();
    let output = run_import(&["--file", &missing], None);
    assert_refusal(&output, &["cannot read import file"]);
}

// ---------------------------------------------------------------------------
// Disposable-database scenarios (skip without TWO_TEST_DATABASE_URL).
// ---------------------------------------------------------------------------

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    name: String,
    url: String,
}

impl TestDb {
    async fn new(role: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let name = format!(
            "two_bot_test_rbimport_{role}_{}_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
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
            .max_connections(2)
            .connect_with(options)
            .await?;
        // Only the migrations behind the round-trip tables. Every other
        // classified table stays absent, exactly as a partially migrated
        // target would look; the exporter already skips missing tables.
        for migration in [
            include_str!("../migrations/0001_funnel.sql"),
            include_str!("../migrations/0002_leveling.sql"),
            include_str!("../migrations/0210_tickets.sql"),
        ] {
            sqlx::raw_sql(sqlx::AssertSqlSafe(migration))
                .execute(&pool)
                .await?;
        }
        Ok(Self {
            admin: admin.clone(),
            pool,
            name: name.to_owned(),
            url,
        })
    }

    /// The same before/after-`T_f` seed shape as `rollback_delta_db`,
    /// restricted to the round-trip tables: exactly four post-`T_f` rows
    /// (2 member_levels, 1 members via the multi-column GREATEST, 1 ticket).
    async fn seed_source(&self) -> TestResult {
        sqlx::raw_sql(sqlx::AssertSqlSafe(
            "INSERT INTO members (guild_id, member_id, joined_at, last_active_at) VALUES
             ('g', 'm-before', '2026-09-29T12:00:00Z', NULL),
             ('g', 'm-after', '2026-09-29T12:00:00Z', '2026-10-01T12:00:00Z');
             INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at) VALUES
             ('g', 'm1', 15, 15, '2026-09-29T12:00:00Z'),
             ('g', 'm2', 30, 30, '2026-10-01T12:00:00Z'),
             ('g', 'm3', 45, 45, '2026-10-01T12:00:00Z');
             INSERT INTO tickets (id, guild_id, channel_id, opener_id, status, created_at) VALUES
             ('t-before', 'g', 'ch-before', 'o1', 'closed', '2026-09-29T12:00:00Z'),
             ('t-after', 'g', 'ch-after', 'o2', 'open', '2026-10-01T12:00:00Z');",
        ))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every row of one round-trip table as parsed `to_jsonb` JSON, sorted
    /// for comparison. Only the fixed table names above reach this helper.
    async fn table_rows(&self, table: &str) -> Vec<serde_json::Value> {
        assert!(ROUND_TRIP_TABLES.contains(&table));
        let rows: Vec<serde_json::Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT to_jsonb(t) FROM {table} t"
        )))
        .fetch_all(&self.pool)
        .await
        .expect("read back table rows");
        let mut rows: Vec<String> = rows.iter().map(ToString::to_string).collect();
        rows.sort();
        rows.iter()
            .map(|row| serde_json::from_str(row).expect("row JSON"))
            .collect()
    }

    async fn table_count(&self, table: &str) -> i64 {
        assert!(ROUND_TRIP_TABLES.contains(&table));
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(&self.pool)
            .await
            .expect("count table rows")
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

fn database_sql(operation: &str, name: &str) -> sqlx::AssertSqlSafe<String> {
    assert!(name.starts_with("two_bot_test_rbimport_") && name.len() <= 63);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    sqlx::AssertSqlSafe(format!("{operation} \"{name}\""))
}

/// Seed a source database and export its post-`T_f` rows with the existing
/// exporter. Returns the source (kept for its pools' lifetime), the export
/// file, and the exported lines.
async fn exported_source(
) -> Result<(TestDb, ScratchFile, Vec<String>), Box<dyn std::error::Error + Send + Sync>> {
    let source = TestDb::new("source").await?;
    source.seed_source().await?;
    let mut lines: Vec<String> = Vec::new();
    let exported = export_delta(&source.pool, SINCE, &mut |line: String| {
        lines.push(line);
        Ok::<(), std::io::Error>(())
    })
    .await?;
    assert_eq!(exported, 4, "seed exports four post-T_f rows: {lines:?}");
    let mut contents = String::new();
    for line in &lines {
        contents.push_str(line);
        contents.push('\n');
    }
    Ok((source, ScratchFile::write("export", &contents), lines))
}

/// The rows an export file carries for one table, sorted for comparison
/// with [`TestDb::table_rows`].
fn file_rows(lines: &[String], table: &str) -> Vec<serde_json::Value> {
    let mut rows: Vec<String> = lines
        .iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("export line"))
        .filter(|value| value["table"] == table)
        .map(|value| value["row"].to_string())
        .collect();
    rows.sort();
    rows.iter()
        .map(|row| serde_json::from_str(row).expect("export row"))
        .collect()
}

fn db_opted_in() -> bool {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return false;
    }
    true
}

#[tokio::test]
async fn export_then_import_round_trip_reproduces_measured_rows() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let (source, export_file, lines) = exported_source().await?;
    let target = TestDb::new("target").await?;
    let result = async {
        // Default dry-run against the configured target writes nothing.
        let output = run_import(&["--file", &export_file.arg()], Some(&target.url));
        let (mode, tables, total) = parse_plan(&output);
        assert_eq!(mode, "dry-run");
        assert_eq!(total, 4);
        assert_eq!(tables.get("member_levels"), Some(&2));
        assert_eq!(tables.get("members"), Some(&1));
        assert_eq!(tables.get("tickets"), Some(&1));
        for table in ROUND_TRIP_TABLES {
            assert_eq!(
                target.table_count(table).await,
                0,
                "dry-run wrote into {table}"
            );
        }

        // Apply with the live-target opt-in replays the file.
        let output = run_import(
            &["--file", &export_file.arg(), "--apply", "--allow-live-target"],
            Some(&target.url),
        );
        let (mode, _tables, total) = parse_plan(&output);
        assert_eq!(mode, "apply");
        assert_eq!(total, 4);

        // The target now holds exactly the exported rows...
        for table in ROUND_TRIP_TABLES {
            assert_eq!(
                target.table_rows(table).await,
                file_rows(&lines, table),
                "round-tripped rows for {table}"
            );
        }
        // ...and its own delta report recounts them as measured post-T_f rows.
        let summary = report(&target.pool, SINCE).await?;
        for (table, expected) in [("member_levels", 2), ("members", 1), ("tickets", 1)] {
            let entry = summary
                .tables
                .iter()
                .find(|entry| entry.table == table)
                .unwrap_or_else(|| panic!("{table} missing from target report"));
            assert_eq!(entry.status, "measured", "table {table}");
            assert_eq!(entry.count, Some(expected), "table {table}");
        }
        // The import applies no migrations of its own.
        let migrated: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname = 'public' AND tablename = '_sqlx_migrations')",
        )
        .fetch_one(&target.pool)
        .await?;
        assert!(!migrated, "import ran migrations");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = target.cleanup().await.and(source.cleanup().await);
    result.and(cleaned)
}

#[tokio::test]
async fn import_rerun_is_idempotent() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let (source, export_file, _lines) = exported_source().await?;
    let target = TestDb::new("rerun").await?;
    let file_arg = export_file.arg();
    let args = [
        "--file",
        file_arg.as_str(),
        "--apply",
        "--allow-live-target",
    ];
    let result = async {
        parse_plan(&run_import(&args, Some(&target.url)));
        let mut after_first = BTreeMap::new();
        for table in ROUND_TRIP_TABLES {
            after_first.insert(*table, target.table_rows(table).await);
        }
        assert_eq!(after_first["member_levels"].len(), 2);

        // The identical import again: success, and not one row changes.
        parse_plan(&run_import(&args, Some(&target.url)));
        for table in ROUND_TRIP_TABLES {
            assert_eq!(
                target.table_rows(table).await,
                after_first[*table],
                "re-run changed {table}"
            );
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = target.cleanup().await.and(source.cleanup().await);
    result.and(cleaned)
}

#[tokio::test]
async fn import_malformed_after_valid_lines_writes_nothing() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let (source, export_file, _lines) = exported_source().await?;
    // Append a garbage line after the valid export: an apply must refuse
    // the whole file instead of importing the valid prefix.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&export_file.path)?;
    writeln!(file, "{{broken trailing line")?;
    drop(file);

    let target = TestDb::new("atomic").await?;
    let result = async {
        let output = run_import(
            &[
                "--file",
                &export_file.arg(),
                "--apply",
                "--allow-live-target",
            ],
            Some(&target.url),
        );
        assert_refusal(&output, &["malformed import file", "line 5"]);
        for table in ROUND_TRIP_TABLES {
            assert_eq!(
                target.table_count(table).await,
                0,
                "refused import wrote into {table}"
            );
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = target.cleanup().await.and(source.cleanup().await);
    result.and(cleaned)
}
