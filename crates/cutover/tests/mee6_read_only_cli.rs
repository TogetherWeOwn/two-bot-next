//! Process-level MEE6 mode regressions on disposable agent-testdb databases.
//! CI's normal integration step supplies TWO_TEST_DATABASE_URL as an opt-in.
//! Locally: TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/postgres
//! python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test mee6_read_only_cli
use serde_json::{json, Value};
use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use two_bot_cutover::legacy_copy::options::guarded_target;

const GUILD: &str = "1545644954272137297";
const EXPORT: &str = r#"[{"id":"100000000000000001","xp":150},{"id":"100000000000000001","xp":100},{"id":"100000000000000002","xp":200},{"id":"100000000000000003","xp":250},{"id":"100000000000000004","xp":400}]"#;
type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    name: String,
    url: String,
    file: PathBuf,
    manifest: PathBuf,
}

impl TestDb {
    async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let name = format!(
            "two_bot_test_mee6_{}_{}",
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
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let scratch = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR"))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let file = scratch.join(format!("{name}.json"));
        let manifest = scratch.join(format!("{name}_manifest.json"));
        std::fs::write(&file, EXPORT)?;
        Ok(Self {
            admin,
            pool,
            name,
            url,
            file,
            manifest,
        })
    }

    fn run(&self, args: &[&str], import: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_levels-import-mee6"));
        command
            .env_clear()
            .env("TWO_DATABASE_URL", &self.url)
            .env("TWO_DB_POOL_MAX", "1")
            .args(args)
            .args(["--guild", GUILD]);
        if import {
            command
                .arg("--file")
                .arg(&self.file)
                .arg("--manifest")
                .arg(&self.manifest);
        }
        command.output().unwrap()
    }

    async fn cleanup(self) -> TestResult {
        self.pool.close().await;
        sqlx::query(database_sql("DROP DATABASE", &self.name))
            .execute(&self.admin)
            .await?;
        self.admin.close().await;
        std::fs::remove_file(self.file)?;
        if self.manifest.exists() {
            std::fs::remove_file(self.manifest)?;
        }
        Ok(())
    }
}

fn database_sql(operation: &str, name: &str) -> sqlx::AssertSqlSafe<String> {
    assert!(name.starts_with("two_bot_test_mee6_") && name.len() <= 63);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    sqlx::AssertSqlSafe(format!("{operation} \"{name}\""))
}

// Include every public relation, column/default, constraint and index, then
// every table row and sequence state (not just member_levels row counts).
// Catalog definitions: https://www.postgresql.org/docs/18/catalogs.html
async fn snapshot(pool: &PgPool) -> Result<Value, sqlx::Error> {
    let schema: Value = sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'relations', (SELECT jsonb_agg(jsonb_build_array(c.oid, c.relname, c.relkind) ORDER BY c.relname)
                FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'public'),
            'columns', (SELECT jsonb_agg(jsonb_build_array(c.relname, a.attname, a.attnum, a.atttypid, a.atttypmod, a.attnotnull, pg_get_expr(d.adbin, d.adrelid)) ORDER BY c.relname, a.attnum)
                FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
                LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
                WHERE n.nspname = 'public' AND a.attnum > 0 AND NOT a.attisdropped),
            'constraints', (SELECT jsonb_agg(jsonb_build_array(conname, pg_get_constraintdef(oid)) ORDER BY conname, oid)
                FROM pg_constraint WHERE connamespace = 'public'::regnamespace),
            'indexes', (SELECT jsonb_agg(jsonb_build_array(indexname, indexdef) ORDER BY indexname)
                FROM pg_indexes WHERE schemaname = 'public'))",
    )
    .fetch_one(pool)
    .await?;
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p', 'S') ORDER BY c.relname",
    )
    .fetch_all(pool)
    .await?;
    let mut data = serde_json::Map::new();
    for name in names {
        // Identifiers originate in this owned database's catalog, quoted safely.
        let quoted = name.replace('"', "\"\"");
        let rows: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM public.\"{quoted}\" t"
        )))
        .fetch_one(pool)
        .await?;
        data.insert(name, rows);
    }
    Ok(json!({"schema": schema, "data": data}))
}

fn successful_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

async fn read_only_scenarios(db: &TestDb) -> TestResult {
    let empty = snapshot(&db.pool).await?;
    assert_eq!(empty["data"], json!({}));
    for (args, import) in [
        (vec!["inventory"], false),
        (vec!["inventory", "--apply"], false),
        (vec![], true),
        (vec!["import"], true),
    ] {
        let output = db.run(&args, import);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("member_levels") && error.contains("does not exist"));
        assert_eq!(snapshot(&db.pool).await?, empty);
        assert!(!db.manifest.exists());
    }

    // Provision only the read-side tables: no migration ledger. A suppressed
    // migration must neither create other feature tables nor an audit row.
    sqlx::raw_sql(
        "CREATE TABLE member_levels (
            guild_id TEXT NOT NULL, member_id TEXT NOT NULL,
            xp BIGINT NOT NULL, message_xp BIGINT NOT NULL, voice_xp BIGINT NOT NULL,
            imported_xp BIGINT NOT NULL, updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            PRIMARY KEY (guild_id, member_id));
         CREATE TABLE level_import_runs (sentinel TEXT PRIMARY KEY);
         INSERT INTO level_import_runs VALUES ('unchanged');
         INSERT INTO member_levels (guild_id, member_id, xp, message_xp, voice_xp, imported_xp) VALUES
            ('1545644954272137297', '100000000000000001', 130, 20, 10, 100),
            ('1545644954272137297', '100000000000000002', 210, 10, 0, 200),
            ('1545644954272137297', '100000000000000003', 320, 15, 5, 300),
            ('1545644954272137298', '100000000000000004', 905, 5, 0, 900);",
    )
    .execute(&db.pool)
    .await?;
    let before = snapshot(&db.pool).await?;
    let inventory = json!({
        "guildId": GUILD, "memberRows": 3, "totalXp": 660,
        "totalOrganicXp": 60, "totalImportedXp": 600
    });
    for args in [vec!["inventory"], vec!["inventory", "--apply"]] {
        assert_eq!(successful_json(&db.run(&args, false)), inventory);
        assert_eq!(snapshot(&db.pool).await?, before);
    }
    for args in [vec![], vec!["import"], vec!["import", "--allow-lower"]] {
        let manifest = successful_json(&db.run(&args, true));
        let allow_lower = args.contains(&"--allow-lower");
        assert_eq!(manifest["mode"], "dry-run");
        assert_eq!(manifest["guildId"], GUILD);
        assert_eq!(manifest["inventoryBefore"], inventory);
        assert_eq!(manifest["inventoryAfter"], Value::Null);
        assert_eq!(manifest["totalXpAfterMeasured"], Value::Null);
        assert_eq!(manifest["importSummary"], Value::Null);
        assert_eq!(manifest["totalXpIn"], 1100);
        assert_eq!(manifest["uniqueXpIn"], 1000);
        assert_eq!(manifest["accounting"]["rowsIn"], 5);
        assert_eq!(manifest["accounting"]["duplicateRows"], 1);
        assert_eq!(manifest["accounting"]["uniqueMembersIn"], 4);
        assert_eq!(manifest["accounting"]["inserted"], 1);
        assert_eq!(manifest["accounting"]["updated"], if allow_lower { 2 } else { 1 });
        assert_eq!(manifest["accounting"]["unchanged"], 1);
        assert_eq!(manifest["accounting"]["skippedMembers"], if allow_lower { 0 } else { 1 });
        assert_eq!(manifest["rowsWritten"], if allow_lower { 3 } else { 2 });
        assert_eq!(manifest["importedXpWritten"], if allow_lower { 800 } else { 550 });
        assert_eq!(manifest["totalXpAfterProjected"], if allow_lower { 1060 } else { 1110 });
        assert_eq!(manifest["reconciled"], true);
        assert_eq!(manifest["reconciliationErrors"], json!([]));
        assert_eq!(manifest["file"]["bytes"], EXPORT.len());
        assert_eq!(manifest["file"]["sha256"], two_bot_cutover::mee6_xp::sha256_hex(EXPORT.as_bytes()));
        let saved: Value = serde_json::from_slice(&std::fs::read(&db.manifest)?)?;
        assert_eq!(saved, manifest);
        assert_eq!(snapshot(&db.pool).await?, before);
    }
    Ok(())
}

#[tokio::test]
async fn inventory_and_dry_run_never_migrate_or_write() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    let db = TestDb::new().await?;
    let result = read_only_scenarios(&db).await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn explicit_apply_still_bootstraps_and_imports() -> TestResult {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return Ok(());
    }
    let db = TestDb::new().await?;
    let manifest = successful_json(&db.run(&["import", "--apply"], true));
    assert_eq!(manifest["mode"], "apply");
    assert_eq!(manifest["reconciled"], true);
    assert_eq!(manifest["rowsWritten"], 4);
    assert_eq!(manifest["totalXpAfterMeasured"], 1000);
    let migrations: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&db.pool)
        .await?;
    assert!(migrations > 0);
    let xp: i64 = sqlx::query_scalar("SELECT sum(imported_xp)::bigint FROM member_levels")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(xp, 1000);
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM level_import_runs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(runs, 1);
    db.cleanup().await
}
