//! Real SQL acceptance, not ignored: the existing CI integration step supplies
//! disposable Postgres. Missing/refused DB access fails rather than passing a skip.
//! Controller: python3 scripts/cargo_cache.py run -- test -p two-bot-cutover
//! --test mee6_import_transaction -- --nocapture
//! No inherited app URL, migrations in a shared schema, or Discord calls.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{Pool, Postgres};
use two_bot_cutover::{
    connect, run_mee6_import, CutoverDb, ImportError, ImportManifest, LevelInventory, SkipReason,
};

const GUILD: &str = "111111111111111111";
const FOREIGN_GUILD: &str = "222222222222222222";
const IMPORTED_AT: &str = "2026-10-01T00:00:00Z";
const REPLAY_AT: &str = "2026-10-01T01:00:00Z";
const EXPORT: &[u8] = br#"{"players":[
    {"id":"100000000000000001","xp":30},
    {"id":"100000000000000001","xp":40},
    {"id":"100000000000000002","xp":40},
    {"id":"100000000000000003","xp":10},
    {"id":"100000000000000005","xp":50}
]}"#;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

// Parallel tests share one pid and can read the same clock tick.
static NEXT_DB: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    admin: Pool<Postgres>,
    db: CutoverDb,
    name: String,
}

impl TestDb {
    async fn new() -> TestResult<Self> {
        // Same fixed-endpoint guard as settings_db: never parse an app/test URL.
        // Refuse inherited PG overrides; supply an explicit empty password and
        // disable pgpass for the admin connection. No credential fallback.
        for key in ["PGOPTIONS", "PGSSLCERT", "PGSSLKEY", "PGSSLROOTCERT"] {
            assert!(
                std::env::var_os(key).is_none(),
                "inherited PG settings refused"
            );
        }
        let host = if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
            "127.0.0.1"
        } else {
            "agent-testdb"
        };
        let options = PgConnectOptions::new_without_pgpass()
            .host(host)
            .port(5432)
            .username("agent_test")
            .password("")
            .database("agent_test")
            .ssl_mode(PgSslMode::Disable);
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options)
            .await?;
        let name = format!(
            "mee6_test_{}_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            NEXT_DB.fetch_add(1, Ordering::Relaxed)
        );
        // Like legacy_verify_db, use an owned database so the public CutoverDb
        // API cannot open another connection outside a test search_path.
        // Identifier is a fixed prefix plus decimal PID/time/sequence only.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await?;
        let result = async {
            let db = connect(
                &format!("postgres://agent_test:@{host}:5432/{name}?sslmode=disable"),
                1,
                true,
            )
            .await?;
            // Only the real leveling migration, in our just-created database.
            let seeded = async {
                sqlx::raw_sql(include_str!("../migrations/0002_leveling.sql"))
                    .execute(db.pool())
                    .await?;
                sqlx::raw_sql(include_str!("fixtures/mee6_import_seed.sql"))
                    .execute(db.pool())
                    .await?;
                Ok::<_, sqlx::Error>(())
            }
            .await;
            if let Err(error) = seeded {
                db.close().await;
                return Err(error);
            }
            Ok::<_, sqlx::Error>(db)
        }
        .await;
        match result {
            Ok(db) => Ok(Self { admin, db, name }),
            Err(error) => {
                Self::drop_database(&admin, &name).await?;
                admin.close().await;
                Err(error.into())
            }
        }
    }

    async fn drop_database(admin: &Pool<Postgres>, name: &str) -> TestResult {
        // Exact generated database only; do not reset shared test data.
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE {name}")))
            .execute(admin)
            .await?;
        Ok(())
    }

    async fn finish(self) -> TestResult {
        self.db.close().await;
        Self::drop_database(&self.admin, &self.name).await?;
        self.admin.close().await;
        Ok(())
    }
}

async fn members(db: &CutoverDb, guild: Option<&str>) -> TestResult<Value> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(m) ORDER BY guild_id, member_id), '[]'::jsonb)
         FROM member_levels m WHERE $1::text IS NULL OR guild_id = $1",
    )
    .bind(guild)
    .fetch_one(db.pool())
    .await?)
}

async fn audits(db: &CutoverDb) -> TestResult<Value> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb) FROM level_import_runs a",
    )
    .fetch_one(db.pool())
    .await?)
}

async fn awards(db: &CutoverDb) -> TestResult<Value> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb) FROM xp_awards a",
    )
    .fetch_one(db.pool())
    .await?)
}

fn inventory(rows: u64, total: u64, organic: u64, imported: u64) -> LevelInventory {
    LevelInventory {
        guild_id: GUILD.to_owned(),
        member_rows: rows,
        total_xp: total,
        total_organic_xp: organic,
        total_imported_xp: imported,
    }
}

fn assert_plan(manifest: &ImportManifest) {
    assert!(manifest.reconciled, "{:?}", manifest.reconciliation_errors);
    assert!(manifest.reconciliation_errors.is_empty());
    assert_eq!(manifest.guild_id, GUILD);
    assert_eq!(manifest.inventory_before, inventory(4, 230, 85, 145));
    assert_eq!(manifest.total_xp_in, 170);
    assert_eq!(manifest.unique_xp_in, 140);
    assert_eq!(manifest.total_xp_after_projected, 300);
    assert_eq!(manifest.rows_written, 2);
    assert_eq!(manifest.imported_xp_written, 90);
    assert_eq!(
        serde_json::to_value(&manifest.accounting).unwrap(),
        json!({"rowsIn":5,"duplicateRows":1,"uniqueMembersIn":4,
            "inserted":1,"updated":1,"unchanged":1,"skippedMembers":1,"balances":true})
    );
    assert_eq!(manifest.skipped.len(), 2);
    assert_eq!(manifest.skipped_by_reason["duplicate_row"], 1);
    assert_eq!(manifest.skipped_by_reason["would_lower_imported_xp"], 1);
    assert_eq!(manifest.skipped_by_reason["exceeds_xp_ceiling"], 0);
    let lower = manifest
        .skipped
        .iter()
        .find(|row| row.reason == SkipReason::WouldLowerImportedXp)
        .expect("lowering row must be named, not silently dropped");
    assert_eq!(lower.member_id, "100000000000000003");
    assert_eq!(lower.xp, 10);
    assert!(lower.detail.contains("--allow-lower"));
}

async fn dry_run(db: CutoverDb) -> TestResult {
    let before = (
        members(&db, None).await?,
        audits(&db).await?,
        awards(&db).await?,
    );
    let manifest = run_mee6_import(
        &db,
        GUILD,
        "fixture.json",
        EXPORT,
        false,
        false,
        IMPORTED_AT,
    )
    .await?;
    assert_plan(&manifest);
    assert_eq!(manifest.mode, "dry-run");
    assert!(manifest.inventory_after.is_none());
    assert!(manifest.total_xp_after_measured.is_none());
    assert!(manifest.import_summary.is_none());
    assert_eq!(
        (
            members(&db, None).await?,
            audits(&db).await?,
            awards(&db).await?
        ),
        before,
        "dry-run must preserve ALL member columns and both audit tables, across guilds"
    );
    Ok(())
}

async fn assert_summary(db: &CutoverDb, manifest: &ImportManifest, at: &str) -> TestResult {
    let summary = manifest
        .import_summary
        .as_ref()
        .expect("apply audit summary");
    let expected = serde_json::to_value(summary)?;
    let stored: Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('sourceRows', source_rows, 'uniqueMembers', unique_members,
            'inserted', inserted, 'updated', updated, 'unchanged', unchanged,
            'duplicateRows', duplicate_rows, 'totalImportedXp', total_imported_xp)
         FROM level_import_runs WHERE guild_id = $1 AND source = 'mee6'
            AND imported_at = $2::timestamptz",
    )
    .bind(GUILD)
    .bind(at)
    .fetch_one(db.pool())
    .await?;
    assert_eq!(
        stored, expected,
        "persisted audit must match the returned summary"
    );
    Ok(())
}

async fn apply_and_replay(db: CutoverDb) -> TestResult {
    let foreign_before = members(&db, Some(FOREIGN_GUILD)).await?;
    let audits_before = audits(&db).await?;
    let awards_before = awards(&db).await?;
    let manifest =
        run_mee6_import(&db, GUILD, "fixture.json", EXPORT, true, false, IMPORTED_AT).await?;
    assert_plan(&manifest);
    assert_eq!(manifest.mode, "apply");
    assert_eq!(manifest.inventory_after, Some(inventory(5, 300, 85, 215)));
    assert_eq!(manifest.total_xp_after_measured, Some(300));
    assert_eq!(
        serde_json::to_value(&manifest.import_summary)?,
        json!({"sourceRows":4,"uniqueMembers":3,"inserted":1,"updated":1,
            "unchanged":1,"duplicateRows":1,"totalImportedXp":130})
    );
    assert_summary(&db, &manifest, IMPORTED_AT).await?;
    let actual: Value = sqlx::query_scalar(
        "SELECT jsonb_agg(jsonb_build_array(member_id, xp, message_xp, voice_xp, imported_xp,
            updated_at = $2::timestamptz) ORDER BY member_id)
         FROM member_levels WHERE guild_id = $1",
    )
    .bind(GUILD)
    .bind(IMPORTED_AT)
    .fetch_one(db.pool())
    .await?;
    assert_eq!(
        actual,
        json!([
            ["100000000000000001", 80, 30, 10, 40, true],
            ["100000000000000002", 60, 15, 5, 40, false],
            ["100000000000000003", 95, 9, 6, 80, false],
            ["100000000000000004", 15, 6, 4, 5, false],
            ["100000000000000005", 50, 0, 0, 50, true]
        ])
    );
    let after_apply = members(&db, None).await?;
    assert_eq!(members(&db, Some(FOREIGN_GUILD)).await?, foreign_before);
    let audit_rows = audits(&db).await?;
    assert_eq!(audit_rows.as_array().unwrap().len(), 3);
    assert_eq!(
        &audit_rows.as_array().unwrap()[..2],
        audits_before.as_array().unwrap()
    );
    assert_eq!(awards(&db).await?, awards_before);

    let replay =
        run_mee6_import(&db, GUILD, "fixture.json", EXPORT, true, false, REPLAY_AT).await?;
    assert!(replay.reconciled, "{:?}", replay.reconciliation_errors);
    assert!(replay.reconciliation_errors.is_empty());
    assert!(replay.accounting.balances);
    assert_eq!(replay.accounting.inserted, 0);
    assert_eq!(replay.accounting.updated, 0);
    assert_eq!(replay.accounting.unchanged, 3);
    assert_eq!(replay.accounting.skipped_members, 1);
    assert_eq!(replay.rows_written, 0);
    assert_eq!(replay.imported_xp_written, 0);
    assert_eq!(replay.inventory_before, inventory(5, 300, 85, 215));
    assert_eq!(
        replay.inventory_after,
        Some(replay.inventory_before.clone())
    );
    assert_eq!(replay.total_xp_after_projected, 300);
    assert_eq!(replay.total_xp_after_measured, Some(300));
    assert_eq!(
        serde_json::to_value(&replay.import_summary)?,
        json!({"sourceRows":4,"uniqueMembers":3,"inserted":0,"updated":0,
            "unchanged":3,"duplicateRows":1,"totalImportedXp":130})
    );
    assert_summary(&db, &replay, REPLAY_AT).await?;
    assert_eq!(
        members(&db, None).await?,
        after_apply,
        "replay must not even change timestamps"
    );
    let replay_audits = audits(&db).await?;
    assert_eq!(replay_audits.as_array().unwrap().len(), 4);
    assert_eq!(
        &replay_audits.as_array().unwrap()[..3],
        audit_rows.as_array().unwrap()
    );
    assert_eq!(awards(&db).await?, awards_before);
    Ok(())
}

async fn rollback(db: CutoverDb) -> TestResult {
    let before = (
        members(&db, None).await?,
        audits(&db).await?,
        awards(&db).await?,
    );
    sqlx::raw_sql(include_str!("fixtures/mee6_import_fail_audit.sql"))
        .execute(db.pool())
        .await?;
    let error = run_mee6_import(&db, GUILD, "fixture.json", EXPORT, true, false, IMPORTED_AT)
        .await
        .expect_err("final audit failure must not return a successful manifest");
    assert!(
        error
            .to_string()
            .contains("fixture: final import audit insert failed after member writes"),
        "operator needs the actual late failure: {error}"
    );
    let ImportError::Db(sqlx::Error::Database(error)) = error else {
        panic!("expected the injected Postgres error, not an earlier validation failure");
    };
    assert_eq!(error.code().as_deref(), Some("P0001"));
    assert_eq!(
        (
            members(&db, None).await?,
            audits(&db).await?,
            awards(&db).await?
        ),
        before,
        "late audit failure must roll back inserts, updates, timestamps and audit records"
    );
    Ok(())
}

// Run exercises in tasks so cleanup also happens after a failing assertion.
#[tokio::test]
async fn dry_run_changes_no_member_or_audit_rows() -> TestResult {
    let scratch = TestDb::new().await?;
    let result = tokio::spawn(dry_run(scratch.db.clone())).await;
    scratch.finish().await?;
    result?
}

#[tokio::test]
async fn apply_preserves_organic_xp_and_replay_reports_unchanged() -> TestResult {
    let scratch = TestDb::new().await?;
    let result = tokio::spawn(apply_and_replay(scratch.db.clone())).await;
    scratch.finish().await?;
    result?
}

#[tokio::test]
async fn final_audit_failure_rolls_back_all_members_and_audits() -> TestResult {
    let scratch = TestDb::new().await?;
    let result = tokio::spawn(rollback(scratch.db.clone())).await;
    scratch.finish().await?;
    result?
}
