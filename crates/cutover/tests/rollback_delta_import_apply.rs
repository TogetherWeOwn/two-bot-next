//! Builder-owned database tests for `rollback-delta import`'s apply path.
//!
//! The protected acceptance file (`rollback_delta_import_acceptance.rs`)
//! exercises the binary end to end on natural-key tables only and pins no
//! serial-column or conflict-divergence policy. These tests call
//! [`two_bot_cutover::rollback_import::apply_import`] in-process against
//! disposable `agent-testdb` databases, so coverage sees the apply paths and
//! the review's rollback hazards stay pinned: FK child-before-parent files
//! still apply, re-runs report inserted vs skipped rows, owned sequences
//! advance past imported maxima, and files without `events` rows never touch
//! an `events` table.
//!
//! Same disposable-database pattern as the protected acceptance file: one
//! owned database per test on `agent-testdb`, never staging or production.
//! Skips without `TWO_TEST_DATABASE_URL`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use two_bot_cutover::legacy_copy::options::guarded_target;
use two_bot_cutover::rollback_import::{apply_import, parse_import};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    name: String,
}

impl TestDb {
    async fn new(
        role: &str,
        migrations: &[&str],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let name = format!(
            "two_bot_test_rbapply_{role}_{}_{}_{}",
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
        match Self::provision(&admin, options, &name, migrations).await {
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
        migrations: &[&str],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;
        for migration in migrations.iter().copied() {
            sqlx::raw_sql(sqlx::AssertSqlSafe(migration))
                .execute(&pool)
                .await?;
        }
        Ok(Self {
            admin: admin.clone(),
            pool,
            name: name.to_owned(),
        })
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
    assert!(name.starts_with("two_bot_test_rbapply_") && name.len() <= 63);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    sqlx::AssertSqlSafe(format!("{operation} \"{name}\""))
}

fn db_opted_in() -> bool {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return false;
    }
    true
}

fn line(table: &str, row: &serde_json::Value) -> String {
    serde_json::json!({"table": table, "row": row}).to_string()
}

fn ticket_row(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "guild_id": "g",
        "channel_id": format!("ch-{id}"),
        "opener_id": "o1",
        "status": "closed",
        "created_at": "2026-10-01T12:00:00+00:00",
        "closed_at": "2026-10-02T12:00:00+00:00",
    })
}

fn transcript_row(ticket_id: &str) -> serde_json::Value {
    serde_json::json!({
        "ticket_id": ticket_id,
        "guild_id": "g",
        "channel_id": format!("ch-{ticket_id}"),
        "opener_id": "o1",
        "content": "closed with a transcript",
        "message_count": 3,
        "created_at": "2026-10-01T13:00:00+00:00",
        "purge_after": "2027-01-01T00:00:00+00:00",
    })
}

/// A child row listed before its parent still applies: the import replays
/// parent-first, so the transcript's immediate FK to `tickets` is satisfied.
#[tokio::test]
async fn apply_import_reorders_child_before_parent() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new("fkorder", &[include_str!("../migrations/0210_tickets.sql")]).await?;
    let result = async {
        // Exporter (TABLE_SPECS) order puts the transcript first; keep that
        // file order here so the test pins the reorder, not the fixture.
        let contents = format!(
            "{}\n{}\n",
            line("ticket_transcripts", &transcript_row("t-fk1")),
            line("tickets", &ticket_row("t-fk1")),
        );
        let rows = parse_import(&contents).expect("valid fk fixture");
        let outcome = apply_import(&db.pool, &rows).await.expect("fk import");
        assert_eq!(outcome.inserted, 2, "both rows land");
        assert_eq!(outcome.skipped, 0, "nothing pre-existed");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ticket_transcripts")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count, 1, "transcript row present");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// Re-running an identical import inserts nothing and reports every row as
/// already present instead of claiming fresh writes.
#[tokio::test]
async fn apply_import_reports_inserted_vs_skipped() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new("counts", &[include_str!("../migrations/0210_tickets.sql")]).await?;
    let result = async {
        let contents = format!(
            "{}\n{}\n",
            line("tickets", &ticket_row("t-c1")),
            line("tickets", &ticket_row("t-c2")),
        );
        let rows = parse_import(&contents).expect("valid count fixture");
        let first = apply_import(&db.pool, &rows).await.expect("first import");
        assert_eq!((first.inserted, first.skipped, first.total()), (2, 0, 2));
        let second = apply_import(&db.pool, &rows).await.expect("rerun import");
        assert_eq!((second.inserted, second.skipped, second.total()), (0, 2, 2));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tickets")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count, 2, "re-run duplicates nothing");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// An explicitly imported `BIGSERIAL` id advances its sequence in the same
/// transaction: the bot's next id-less insert lands past the import instead
/// of colliding with it.
#[tokio::test]
async fn apply_import_advances_owned_sequences() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new(
        "sequences",
        &[include_str!("../migrations/0002_leveling.sql")],
    )
    .await?;
    let result = async {
        // Id 1 collides with the sequence's first value on a fresh target:
        // without the advancement this test's follow-up insert fails with a
        // duplicate primary key.
        let contents = format!(
            "{}\n",
            line(
                "xp_awards",
                &serde_json::json!({
                    "id": 1,
                    "guild_id": "g",
                    "member_id": "m1",
                    "source": "message",
                    "xp": 5,
                    "occurred_at": "2026-10-01T12:00:00+00:00",
                }),
            ),
        );
        let rows = parse_import(&contents).expect("valid award fixture");
        let outcome = apply_import(&db.pool, &rows).await.expect("award import");
        assert_eq!((outcome.inserted, outcome.skipped), (1, 0));
        let next: i64 = sqlx::query_scalar(
            "INSERT INTO xp_awards (guild_id, member_id, source, xp, occurred_at) \
             VALUES ('g', 'm2', 'voice', 7, '2026-10-02T12:00:00+00:00') RETURNING id",
        )
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(next, 2, "sequence resumes past the imported maximum");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// Files without `events` rows never touch an `events` table: a partially
/// migrated target that lacks it still imports.
#[tokio::test]
async fn apply_import_without_events_needs_no_events_table() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    // 0210 only: no `events` table exists on this target.
    let db = TestDb::new(
        "noevents",
        &[include_str!("../migrations/0210_tickets.sql")],
    )
    .await?;
    let result = async {
        let contents = format!("{}\n", line("tickets", &ticket_row("t-ne1")));
        let rows = parse_import(&contents).expect("valid no-events fixture");
        let outcome = apply_import(&db.pool, &rows)
            .await
            .expect("no-events import");
        assert_eq!((outcome.inserted, outcome.skipped), (1, 0));
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}
