//! End-to-end cutover rehearsal on disposable agent-testdb databases.
//!
//! Chains the runbook order in one test: migrate the Next schema, `legacy_copy`
//! dry-run plan then apply, `legacy_verify`, MEE6 import from the rehearsal
//! fixture, then rerun everything to prove idempotency. A final drift case
//! mutates one target row and requires the verifier to fail naming that table.
//!
//! Real SQL acceptance, not ignored: the CI integration step supplies
//! disposable Postgres like the MEE6 transaction suite. Missing/refused DB
//! access fails rather than passing a skip. No network, no staging, no
//! workflow change: `--workspace --test '*'` picks this file up automatically.
//!
//! Controller: python3 scripts/cargo_cache.py run -- test -p two-bot-cutover
//! --test cutover_rehearsal

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::{Connection, PgConnection, PgPool};
use two_bot_cutover::legacy_copy::{
    copy, mapping, options::guarded_target, CopyMode, Receipt, Table,
};
use two_bot_cutover::legacy_mapping::MappingSpec;
use two_bot_cutover::legacy_verify::verify;
use two_bot_cutover::{connect, run_mee6_import, CutoverDb};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

const GUILD: &str = "777777777777777777";
const IMPORTED_AT: &str = "2026-10-02T00:00:00Z";
const REPLAY_AT: &str = "2026-10-02T01:00:00Z";
const EXPORT: &[u8] = include_bytes!("fixtures/mee6_rehearsal_export.json");

struct Rehearsal {
    admin: PgPool,
    source: PgPool,
    target: PgPool,
    db: CutoverDb,
    names: [String; 2],
    host: String,
}

impl Rehearsal {
    async fn new() -> TestResult<Self> {
        // Same fixed-endpoint guard as the MEE6 transaction suite: the host is
        // chosen, never parsed from an app URL. No credential fallback.
        for key in ["PGOPTIONS", "PGSSLCERT", "PGSSLKEY", "PGSSLROOTCERT"] {
            assert!(
                std::env::var_os(key).is_none(),
                "inherited PG settings refused"
            );
        }
        // Fixed hostname like the legacy-copy suite: CI resolves it to its
        // service, and the copier's target guard only accepts agent-testdb.
        let host = "agent-testdb";
        let suffix = format!(
            "{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        let names = [
            format!("two_bot_test_rehearsal_{suffix}_source"),
            format!("two_bot_test_rehearsal_{suffix}_target"),
        ];
        // The copier's own fail-closed target guard: only owned
        // two_bot_test_* databases on agent-testdb connect.
        let options = guarded_target(
            &format!("postgres://agent_test:@{host}:5432/{}", names[1]),
            false,
        )?
        .ssl_mode(PgSslMode::Disable)
        .options([("timezone", "UTC"), ("statement_timeout", "10000")]);
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone().database("postgres"))
            .await?;
        for name in &names {
            sqlx::query(database_sql("CREATE DATABASE", name))
                .execute(&admin)
                .await?;
        }
        let source = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone().database(&names[0]))
            .await?;
        let target = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.database(&names[1]))
            .await?;
        // The MEE6 import lands on the target through the public CutoverDb
        // API, exactly like the operator tool. This applies the Next schema
        // now, so the migrate in `initialize` below is a tracked no-op.
        let db = connect(
            &format!(
                "postgres://agent_test:@{host}:5432/{}?sslmode=disable",
                names[1]
            ),
            2,
            false,
        )
        .await?;
        Ok(Self {
            admin,
            source,
            target,
            db,
            names,
            host: host.to_owned(),
        })
    }

    async fn cleanup(self) -> TestResult {
        self.db.close().await;
        self.source.close().await;
        self.target.close().await;
        for name in &self.names {
            sqlx::query(database_sql("DROP DATABASE", name))
                .execute(&self.admin)
                .await?;
        }
        self.admin.close().await;
        Ok(())
    }
}

fn verify_options(host: &str, name: &str) -> sqlx::postgres::PgConnectOptions {
    sqlx::postgres::PgConnectOptions::new_without_pgpass()
        .host(host)
        .port(5432)
        .username("agent_test")
        .password("")
        .database(name)
        .ssl_mode(PgSslMode::Disable)
}

async fn verify_pair(
    host: &str,
    source_name: &str,
    target_name: &str,
) -> TestResult<(PgConnection, PgConnection)> {
    let source = PgConnection::connect_with(&verify_options(host, source_name)).await?;
    let target = PgConnection::connect_with(&verify_options(host, target_name)).await?;
    Ok((source, target))
}

fn database_sql(operation: &str, name: &str) -> sqlx::AssertSqlSafe<String> {
    assert!(name.starts_with("two_bot_test_rehearsal_") && name.len() <= 63);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    sqlx::AssertSqlSafe(format!("{operation} \"{name}\""))
}

/// Translate the compiled copy registry into the versioned JSON mapping the
/// verifier reads, so copy and verify can never disagree on groups, keys or
/// casts. Pending groups travel as pending, never silently dropped.
fn rehearsal_spec_json() -> String {
    let mut tables = Vec::new();
    let mut pending = Vec::new();
    for group in mapping::GROUPS {
        if group.status == "ready" {
            for table in group.tables {
                tables.push(serde_json::json!({
                    "group": group.name,
                    "source": table.source,
                    "target": table.target,
                    "keys": table.keys,
                    "conflict": table.conflict,
                    "conflict_policy": match table.mode {
                        CopyMode::Upsert => "upsert",
                        CopyMode::AppendOnly => "insert_only",
                    },
                    "columns": table.columns.iter().map(|c| serde_json::json!({
                        "source": c.source,
                        "target": c.target,
                        "pg_type": c.pg_type,
                    })).collect::<Vec<_>>(),
                }));
            }
        } else {
            pending.push(serde_json::json!({
                "group": group.name,
                "reason": group.reason,
            }));
        }
    }
    serde_json::json!({
        "version": 1,
        "tables": tables,
        "pending_groups": pending,
    })
    .to_string()
}

fn ready_groups() -> Vec<String> {
    mapping::GROUPS
        .iter()
        .filter(|group| group.status == "ready")
        .map(|group| group.name.to_owned())
        .collect()
}

/// Pending or unknown groups fail loudly rather than being omitted
/// (`docs/cutover.md:204`). No database needed.
#[test]
fn pending_groups_fail_loudly_in_copy_and_verify() {
    let ready = ready_groups();
    let spec = MappingSpec::parse(&rehearsal_spec_json()).unwrap();
    assert_eq!(spec.select(&ready).unwrap().len(), 25);
    assert_eq!(mapping::select(&ready).unwrap().len(), 25);
    for group in mapping::GROUPS.iter().filter(|g| g.status != "ready") {
        let copy_error = mapping::select(&[group.name.to_owned()]).unwrap_err();
        assert!(
            copy_error.contains(group.name),
            "copy must name the refused group: {copy_error}"
        );
        let verify_error = spec
            .select(&[group.name.to_owned()])
            .unwrap_err()
            .to_string();
        assert!(
            verify_error.contains(group.name),
            "verify must name the refused group: {verify_error}"
        );
    }
    // An empty selection with pending groups outstanding refuses instead of
    // claiming complete parity; `all` refuses on the copy side too.
    assert!(spec
        .select(&[])
        .unwrap_err()
        .to_string()
        .contains("pending group"));
    assert!(mapping::select(&["all".to_owned()]).is_err());
}

fn group_planned(tables: &[Table], receipts: &[Receipt]) -> BTreeMap<&str, i64> {
    let mut totals = BTreeMap::new();
    for (table, receipt) in tables.iter().zip(receipts) {
        *totals.entry(table.group).or_default() += receipt.source_rows;
    }
    totals
}

async fn member_snapshot(db: &CutoverDb) -> TestResult<Value> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(m) ORDER BY guild_id, member_id), '[]'::jsonb)
         FROM member_levels m",
    )
    .fetch_one(db.pool())
    .await?)
}

async fn audit_counts(db: &CutoverDb) -> TestResult<(i64, i64)> {
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM level_import_runs")
        .fetch_one(db.pool())
        .await?;
    let awards: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM xp_awards")
        .fetch_one(db.pool())
        .await?;
    Ok((runs, awards))
}

async fn initialize(source: &PgPool, target: &PgPool) -> TestResult {
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy_migrations");
    let mut paths: Vec<_> = std::fs::read_dir(fixture)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    paths.retain(|p| p.extension().is_some_and(|e| e == "sql"));
    paths.sort();
    assert_eq!(
        paths.len(),
        54,
        "the pinned 0001–0042 set has repeated prefixes"
    );
    for path in paths {
        // Pinned, vendored SQL files only; no operator-supplied path or SQL.
        sqlx::raw_sql(sqlx::AssertSqlSafe(std::fs::read_to_string(&path)?))
            .execute(source)
            .await
            .map_err(|error| format!("{}: {error}", path.file_name().unwrap().to_string_lossy()))?;
    }
    // The complete Next schema, the same set the operator migrates.
    sqlx::migrate!("./migrations").run(target).await?;
    sqlx::raw_sql(include_str!("fixtures/legacy_copy_seed.sql"))
        .execute(source)
        .await?;
    Ok(())
}

async fn scenario(
    source: &PgPool,
    target: &PgPool,
    db: &CutoverDb,
    host: &str,
    source_name: &str,
    target_name: &str,
) -> TestResult {
    initialize(source, target).await?;
    let tables = mapping::select(&["ready".to_owned()])?;
    assert_eq!(tables.len(), 25);
    let groups = ready_groups();
    let spec = MappingSpec::parse(&rehearsal_spec_json())?;

    // 1–2. Dry-run plan, then apply. Planned and applied counts agree.
    let dry = copy(source, target, &tables, 100, false).await?;
    for (table, receipt) in tables.iter().zip(&dry) {
        assert!(receipt.source_rows > 0, "{} fixture is empty", table.source);
        assert_eq!(receipt.changed, 0);
        assert_eq!(receipt.batches, 0);
    }
    let applied = copy(source, target, &tables, 100, true).await?;
    for (planned, receipt) in dry.iter().zip(&applied) {
        assert_eq!(receipt.source_rows, planned.source_rows);
    }
    assert!(applied.iter().any(|r| r.changed > 0));
    assert_eq!(
        group_planned(&tables, &dry),
        group_planned(&tables, &applied)
    );

    // 3. Verify: per-group verified counts equal planned/applied, all match.
    let (mut source_conn, mut target_conn) = verify_pair(host, source_name, target_name).await?;
    let report = verify(&mut source_conn, &mut target_conn, &spec, &groups, 10).await?;
    assert!(report.matches);
    let mut verified: BTreeMap<&str, i64> = BTreeMap::new();
    for table in &report.tables {
        assert!(table.matches, "{} does not verify", table.source);
        assert_eq!(table.source_rows, table.target_rows);
        *verified.entry(table.group.as_str()).or_default() += table.source_rows as i64;
    }
    assert_eq!(verified, group_planned(&tables, &dry));
    source_conn.close().await?;
    target_conn.close().await?;

    // 4. MEE6 import from the fixture: dry-run writes nothing, apply lands the
    // plan, and replay changes zero member rows.
    let before = (member_snapshot(db).await?, audit_counts(db).await?);
    let plan = run_mee6_import(
        db,
        GUILD,
        "mee6_rehearsal_export.json",
        EXPORT,
        false,
        false,
        IMPORTED_AT,
    )
    .await?;
    assert!(plan.reconciled, "{:?}", plan.reconciliation_errors);
    assert_eq!(plan.mode, "dry-run");
    assert_eq!(plan.accounting.inserted, 2);
    assert_eq!(
        (member_snapshot(db).await?, audit_counts(db).await?),
        before
    );

    let manifest = run_mee6_import(
        db,
        GUILD,
        "mee6_rehearsal_export.json",
        EXPORT,
        true,
        false,
        IMPORTED_AT,
    )
    .await?;
    assert!(manifest.reconciled, "{:?}", manifest.reconciliation_errors);
    assert_eq!(manifest.mode, "apply");
    assert_eq!(manifest.rows_written, 2);
    assert_eq!(manifest.imported_xp_written, 70);
    assert_eq!(manifest.accounting.inserted, 2);
    assert_eq!(manifest.accounting.updated, 0);
    let after_apply = member_snapshot(db).await?;

    // 5. Rerun everything: the copy replays zero rows even with the imported
    // members present (upsert-only, never deletes), and the import replays
    // without touching a member row.
    let replay = copy(source, target, &tables, 100, true).await?;
    assert!(replay.iter().all(|r| r.changed == 0));
    let second = copy(source, target, &tables, 100, false).await?;
    assert_eq!(
        group_planned(&tables, &second),
        group_planned(&tables, &dry)
    );
    let import_replay = run_mee6_import(
        db,
        GUILD,
        "mee6_rehearsal_export.json",
        EXPORT,
        true,
        false,
        REPLAY_AT,
    )
    .await?;
    assert!(
        import_replay.reconciled,
        "{:?}",
        import_replay.reconciliation_errors
    );
    assert_eq!(import_replay.rows_written, 0);
    assert_eq!(import_replay.accounting.inserted, 0);
    assert_eq!(import_replay.accounting.updated, 0);
    assert_eq!(import_replay.accounting.unchanged, 2);
    assert_eq!(member_snapshot(db).await?, after_apply);

    // 6. Drift: one changed target row fails verification and names the table.
    sqlx::query("UPDATE events SET source='drifted rehearsal' WHERE id=2")
        .execute(target)
        .await?;
    let (mut source_conn, mut target_conn) = verify_pair(host, source_name, target_name).await?;
    let drifted = verify(&mut source_conn, &mut target_conn, &spec, &groups, 10).await?;
    source_conn.close().await?;
    target_conn.close().await?;
    assert!(!drifted.matches);
    let failing: Vec<_> = drifted.tables.iter().filter(|t| !t.matches).collect();
    assert_eq!(failing.len(), 1);
    assert_eq!(failing[0].source, "events");
    assert_eq!(failing[0].target, "events");
    assert_eq!(failing[0].source_rows, failing[0].target_rows);
    Ok(())
}

// The scenario runs in a task holding cloned handles so the parent still
// owns cleanup when an assertion panics in the scenario task.
#[tokio::test]
async fn cutover_rehearsal_end_to_end() -> TestResult {
    let ctx = Rehearsal::new().await?;
    let result = tokio::spawn(scenario(
        ctx.source.clone(),
        ctx.target.clone(),
        ctx.db.clone(),
        ctx.host.clone(),
        ctx.names[0].clone(),
        ctx.names[1].clone(),
    ))
    .await;
    ctx.cleanup().await?;
    result?
}
