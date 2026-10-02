//! Acceptance for the read-only Next-window delta report (TOG-12022).
//!
//! Seeds rows before and after `T_f` across five tables (one timestamptz,
//! one multi-column GREATEST projection, one ISO-8601 TEXT legacy column),
//! then asserts exact summary counts, unmeasurable/missing entries that are
//! reported rather than skipped, a forbidden write inside the snapshot, and
//! an NDJSON export. Real SQL, not ignored: like `mee6_import_transaction`,
//! missing/refused DB access fails rather than passing a skip.
//! Controller: python3 scripts/cargo_cache.py run -- test -p two-bot-cutover
//! --test rollback_delta_db
//! Only agent-testdb or the CI service container; never an application URL.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{Pool, Postgres, QueryBuilder};
use two_bot_cutover::legacy_verify::read_only_transaction;
use two_bot_cutover::rollback_delta::{export_delta, report, TABLE_SPECS};

static SCHEMA_SEQ: AtomicU64 = AtomicU64::new(0);

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The `T_f` baseline every seed is measured against.
const SINCE: &str = "2026-09-30T12:00:00Z";

struct TestDb {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
}

impl TestDb {
    async fn new() -> TestResult<Self> {
        // Same fixed-endpoint guard as mee6_import_transaction: never parse an
        // app/test URL. Refuse inherited PG overrides; supply an explicit
        // empty password and disable pgpass. No credential fallback.
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
            .database("postgres")
            .ssl_mode(PgSslMode::Disable);
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone())
            .await?;
        let schema = format!(
            "rollback_delta_{}_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            SCHEMA_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        // Identifier is a constant prefix plus numeric process/time IDs only.
        QueryBuilder::<Postgres>::new("CREATE SCHEMA ")
            .push(&schema)
            .build()
            .execute(&admin)
            .await?;
        let path = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |conn, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options.application_name(&schema))
            .await?;
        // Only the real cutover migrations this report reads, in our own schema.
        for migration in [
            include_str!("../migrations/0001_funnel.sql"),
            include_str!("../migrations/0002_leveling.sql"),
            include_str!("../migrations/0210_tickets.sql"),
            include_str!("../migrations/0330_guild_settings.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&pool).await?;
        }
        Self::seed(&pool).await?;
        Ok(Self {
            admin,
            pool,
            schema,
        })
    }

    /// One row on each side of `T_f` per table. The `members` pair proves the
    /// multi-column GREATEST: `m-before` joins before `T_f` and never returns,
    /// while `m-after` joins before `T_f` but is active after it.
    async fn seed(pool: &Pool<Postgres>) -> TestResult {
        sqlx::raw_sql(
            "INSERT INTO events (event_type, member_id, guild_id, occurred_at, recorded_at, source, idempotency_key) VALUES
             ('member_join', 'm1', 'g', '2026-09-29T12:00:00Z', '2026-09-29T12:00:00Z', 'test', 'e-before'),
             ('member_join', 'm2', 'g', '2026-10-01T12:00:00Z', '2026-10-01T12:00:00Z', 'test', 'e-after');
             INSERT INTO members (guild_id, member_id, joined_at, last_active_at) VALUES
             ('g', 'm-before', '2026-09-29T12:00:00Z', NULL),
             ('g', 'm-after', '2026-09-29T12:00:00Z', '2026-10-01T12:00:00Z');
             INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at) VALUES
             ('g', 'm1', 15, 15, '2026-09-29T12:00:00Z'),
             ('g', 'm2', 30, 30, '2026-10-01T12:00:00Z'),
             ('g', 'm3', 45, 45, '2026-10-01T12:00:00Z');
             INSERT INTO tickets (id, guild_id, channel_id, opener_id, status, created_at) VALUES
             ('t-before', 'g', 'ch-before', 'o1', 'closed', '2026-09-29T12:00:00Z'),
             ('t-after', 'g', 'ch-after', 'o2', 'open', '2026-10-01T12:00:00Z');
             INSERT INTO guild_settings (guild_id, key, value, version, updated_by) VALUES
             ('g', 'ROLLBACK_DELTA_PROBE', '\"probe\"', 1, 'test');",
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn finish(self) -> TestResult {
        self.pool.close().await;
        // Only this test's generated schema; never shared data.
        QueryBuilder::<Postgres>::new("DROP SCHEMA ")
            .push(&self.schema)
            .push(" CASCADE")
            .build()
            .execute(&self.admin)
            .await?;
        self.admin.close().await;
        Ok(())
    }
}

fn count_of(
    summary: &two_bot_cutover::rollback_delta::DeltaReport,
    table: &str,
) -> Option<(String, Option<i64>)> {
    summary
        .tables
        .iter()
        .find(|t| t.table == table)
        .map(|t| (t.status.clone(), t.count))
}

#[tokio::test]
async fn delta_counts_are_exact_and_nothing_is_silently_skipped() -> TestResult {
    let db = TestDb::new().await?;
    let result = async {
        let summary = report(&db.pool, SINCE).await?;
        assert_eq!(summary.version, 1);
        assert_eq!(summary.since, SINCE);
        // Exact post-T_f counts on both storage shapes: timestamptz
        // (events, member_levels, guild_settings), multi-column GREATEST
        // (members: only the row active after T_f), ISO-8601 TEXT (tickets).
        assert_eq!(
            count_of(&summary, "events"),
            Some(("measured".to_owned(), Some(1)))
        );
        assert_eq!(
            count_of(&summary, "members"),
            Some(("measured".to_owned(), Some(1)))
        );
        assert_eq!(
            count_of(&summary, "member_levels"),
            Some(("measured".to_owned(), Some(2)))
        );
        assert_eq!(
            count_of(&summary, "tickets"),
            Some(("measured".to_owned(), Some(1)))
        );
        assert_eq!(
            count_of(&summary, "guild_settings"),
            Some(("measured".to_owned(), Some(1)))
        );
        // Unmeasurable tables carry a reason, never a silent skip.
        for table in [
            "guild_settings_revision",
            "level_role_rewards",
            "rank_ladder",
            "lfg_roles",
            "moderation_channel_executions",
            "web_contract_meta",
        ] {
            let entry = summary.tables.iter().find(|t| t.table == table).unwrap_or_else(|| {
                panic!("classified table {table} missing from report")
            });
            assert_eq!(entry.status, "unmeasurable", "table {table}");
            assert!(entry.count.is_none(), "table {table}");
            assert!(
                entry.reason.as_deref().is_some_and(|r| !r.is_empty()),
                "table {table}"
            );
        }
        // Tables whose migrations were not applied here report as missing.
        let missing = summary
            .tables
            .iter()
            .find(|t| t.table == "moderation_audit")
            .expect("moderation_audit entry");
        assert_eq!(missing.status, "missing");
        assert!(missing.reason.is_some());
        // The report covers every classified table exactly once.
        assert_eq!(summary.tables.len(), TABLE_SPECS.len());
        for entry in &summary.tables {
            assert!(
                ["measured", "unmeasurable", "missing"].contains(&entry.status.as_str()),
                "table {} has unexpected status {}",
                entry.table,
                entry.status
            );
        }

        // Read-only proof in the exact snapshot setup: the session reports
        // read-only and a write to a real table fails with 25006.
        let mut conn = db.pool.acquire().await?;
        let mut tx = read_only_transaction(&mut conn).await?;
        let readonly: String = sqlx::query_scalar("SHOW transaction_read_only")
            .fetch_one(&mut *tx)
            .await?;
        assert_eq!(readonly, "on");
        let error = sqlx::query(
            "INSERT INTO events (event_type, guild_id, occurred_at, recorded_at, source, idempotency_key)
             VALUES ('member_join', 'g', now(), now(), 'test', 'forbidden')",
        )
        .execute(&mut *tx)
        .await
        .unwrap_err();
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("25006")
        );
        tx.rollback().await?;

        // Export emits one NDJSON line per post-T_f row: 1 + 1 + 2 + 1 + 1.
        let mut lines = Vec::new();
        let exported = export_delta(&db.pool, SINCE, &mut |line: String| {
            lines.push(line);
            Ok::<(), std::io::Error>(())
        })
        .await?;
        assert_eq!(exported, 6);
        assert_eq!(lines.len(), 6);
        for line in &lines {
            let value: serde_json::Value = serde_json::from_str(line)?;
            assert!(value.get("table").and_then(|t| t.as_str()).is_some());
            assert!(value.get("row").is_some());
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    db.finish().await?;
    result
}

#[test]
fn cli_refuses_missing_and_future_since_without_a_database() {
    // Usage errors parse before any connection attempt, so no database is
    // needed and none is touched.
    for args in [
        Vec::<String>::new(),
        vec!["--since".to_owned(), "2999-01-01T00:00:00Z".to_owned()],
        vec!["--since".to_owned(), "not-a-timestamp".to_owned()],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_rollback-delta"))
            .args(&args)
            .output()
            .expect("rollback-delta binary");
        assert_eq!(output.status.code(), Some(2), "args {args:?}");
        assert!(
            output.stdout.is_empty(),
            "usage errors carry no JSON summary"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--since"),
            "args {args:?}"
        );
    }
}
