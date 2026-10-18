//! First rollback-journal writer acceptance: one wired write path appends
//! rollback evidence, evidence failure never breaks the primary write, and
//! the backup exclusion classification does not move. Only agent-testdb or
//! a CI service container is admitted, with the agent_test identity.
//! DATABASE_URL is never consulted.
//!
//! The wired path under test is the funnel append
//! (`PgFunnelStore::try_record` and the sync `FunnelStore::record` seam
//! over the same core): `events` heads the rollback inventory, the append
//! is insert-only, and a committed append must leave exactly one
//! `rollback_journal` row naming the inserted `events` row by its primary
//! key. Duplicates that write nothing journal nothing.
//!
//! Run: `cargo test -p two-bot-store --locked --test journal_writer -- --ignored`
//! with `TEST_DATABASE_URL` pointed at the authorized test database.
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_core::{EventType, FunnelEvent, FunnelStore};
use two_bot_store::journal::{entries_since, restorable_point, watermark_for, JournalOp};
use two_bot_store::{migrate, PgFunnelStore, DB_POOL_MAX};

#[path = "../../core/tests/support/tracing_capture.rs"]
mod tracing_capture;

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
}

impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("TEST_DATABASE_URL").expect("set TEST_DATABASE_URL for DB tests");
        let options = PgConnectOptions::from_str(&url).expect("invalid TEST_DATABASE_URL");
        let ci_container = std::env::var("CI").as_deref() == Ok("true")
            && matches!(options.get_host(), "localhost" | "127.0.0.1" | "postgres");
        assert!(
            options.get_host() == "agent-testdb" || ci_container,
            "test containers only"
        );
        assert_eq!(options.get_username(), "agent_test", "test identity only");
        // Threat-model F6: tests opt into `LocalOnly` explicitly rather than
        // inheriting the process `TWO_DATABASE_TLS` value.
        let admin = two_bot_store::connect_pool_with_tls(
            &url,
            two_bot_core::database_tls::TlsPolicy::LocalOnly,
        )
        .await
        .expect("test DB connect failed");
        let schema = format!(
            "journal_writer_test_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(DB_POOL_MAX)
            .connect_with(options.options([
                ("search_path", schema.clone()),
                ("statement_timeout", "15000ms".to_owned()),
            ]))
            .await
            .unwrap();
        migrate(&pool).await.unwrap();
        Self {
            admin,
            pool,
            schema,
        }
    }

    async fn finish(self) {
        self.pool.close().await;
        // Only schemas created by this fixture; never shared tables or roles.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {}_web_v1 CASCADE; DROP SCHEMA {} CASCADE",
            self.schema, self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }

    async fn count(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }

    async fn journal_count(&self) -> i64 {
        self.count("SELECT count(*) FROM rollback_journal").await
    }

    async fn event_ids(&self) -> Vec<i64> {
        sqlx::query_scalar("SELECT id FROM events ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }
}

/// A repeatable funnel event: member plus timestamp tells appends apart, so
/// each distinct `at` is one real insert, never an idempotent duplicate.
fn join_at(at: &str) -> FunnelEvent {
    FunnelEvent {
        guild_id: 123,
        member_id: Some(456),
        event_type: EventType::MemberJoin,
        occurred_at: at.to_owned(),
        source: "unknown".to_owned(),
        metadata: None,
        dedupe_token: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn funnel_appends_accumulate_rollback_journal_rows() {
    let f = Fixture::new().await;
    let store = PgFunnelStore::new(f.pool.clone());

    // Nothing is journaled before the wired path runs.
    assert_eq!(f.journal_count().await, 0);
    assert_eq!(watermark_for(&f.pool, "events").await.unwrap(), None);

    // Three appends of the wired path, one of them through the sync seam:
    // the writer must live in the shared append core, not in one wrapper.
    let stamps = [
        "2026-09-29T12:00:00.000Z",
        "2026-09-29T12:05:00.000Z",
        "2026-09-29T12:10:00.000Z",
    ];
    assert!(
        store
            .try_record(&join_at(stamps[0]))
            .await
            .unwrap()
            .inserted
    );
    assert!(store.record(join_at(stamps[1])).inserted);
    assert!(
        store
            .try_record(&join_at(stamps[2]))
            .await
            .unwrap()
            .inserted
    );

    // A duplicate append writes no event row, so it journals nothing.
    assert!(
        !store
            .try_record(&join_at(stamps[0]))
            .await
            .unwrap()
            .inserted
    );

    let event_ids = f.event_ids().await;
    assert_eq!(event_ids.len(), 3, "three real appends landed");
    assert_eq!(
        f.journal_count().await,
        3,
        "each committed append left exactly one journal row"
    );

    // The journal rows are complete insert entries naming the events rows
    // they evidence, in append order, with no pre-image to restore from.
    let rows = entries_since(&f.pool, "events", 0).await.unwrap();
    assert_eq!(rows.len(), 3);
    for (row, event_id) in rows.iter().zip(&event_ids) {
        assert_eq!(row.table_name, "events");
        assert_eq!(row.op, JournalOp::Insert);
        assert_eq!(
            row.row_identity,
            event_id.to_string(),
            "journal row names its events row by primary key"
        );
        assert!(row.pre_image.is_none(), "inserts carry no pre-image");
    }
    assert!(rows.windows(2).all(|pair| pair[0].id < pair[1].id));

    // The events watermark and the global restorable point track the
    // newest journaled append.
    let newest = rows.last().unwrap().id;
    assert_eq!(
        watermark_for(&f.pool, "events").await.unwrap(),
        Some(newest)
    );
    assert_eq!(restorable_point(&f.pool).await.unwrap(), newest);

    // A later run of the wired path (a fresh store over the same database,
    // as after a restart) keeps accumulating instead of starting over.
    let rerun = PgFunnelStore::new(f.pool.clone());
    assert!(
        rerun
            .try_record(&join_at("2026-09-29T12:15:00.000Z"))
            .await
            .unwrap()
            .inserted
    );
    assert_eq!(
        f.journal_count().await,
        4,
        "evidence accumulates across runs"
    );
    let tail = entries_since(&f.pool, "events", newest).await.unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(
        tail[0].row_identity,
        f.event_ids().await.last().unwrap().to_string()
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn journal_write_failure_leaves_funnel_append_unaffected_with_diagnostic() {
    let capture = tracing_capture::global();
    let f = Fixture::new().await;
    let store = PgFunnelStore::new(f.pool.clone());

    // Fault injection, local to this fixture's disposable schema: the
    // journal table is renamed away, so every journal attempt fails while
    // the primary tables stay fully writable.
    sqlx::query("ALTER TABLE rollback_journal RENAME TO rollback_journal_fault")
        .execute(&f.pool)
        .await
        .unwrap();
    let log_start = capture.text().len();

    // The primary append still succeeds end to end: outcome, event row and
    // members projection are exactly as if journaling had worked.
    let outcome = store
        .try_record(&join_at("2026-09-29T12:00:00.000Z"))
        .await
        .unwrap();
    assert!(outcome.inserted, "journal failure must not fail the append");
    assert_eq!(f.count("SELECT count(*) FROM events").await, 1);
    assert_eq!(
        f.count("SELECT count(*) FROM members WHERE guild_id = '123' AND member_id = '456'")
            .await,
        1,
        "the members projection is part of the primary write"
    );

    // The failure is evidence-only, not silent: a WARN-or-above diagnostic
    // naming the rollback journal was emitted for the faulted append.
    let logged = capture.text()[log_start..].to_lowercase();
    assert!(
        logged.contains("rollback") && (logged.contains("warn") || logged.contains("error")),
        "faulted append must emit a rollback-journal diagnostic, got: {logged}"
    );

    // No partial evidence survived the fault: no journal row, no watermark.
    sqlx::query("ALTER TABLE rollback_journal_fault RENAME TO rollback_journal")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(f.journal_count().await, 0);
    assert_eq!(watermark_for(&f.pool, "events").await.unwrap(), None);

    // The writer is not latched off by the failure: the next append on the
    // healed schema journals normally.
    assert!(
        store
            .try_record(&join_at("2026-09-29T12:05:00.000Z"))
            .await
            .unwrap()
            .inserted
    );
    assert_eq!(
        f.journal_count().await,
        1,
        "writer recovers after the fault"
    );
    assert_eq!(f.count("SELECT count(*) FROM events").await, 2);
    f.finish().await;
}

/// Named control: wiring the first writer must not move the backup
/// classification. The journal and its watermarks stay excluded (restoring
/// pre-backup watermarks could mark post-backup writes as journaled), and
/// neither table enters the dump inventory. The migration-backed coverage
/// proof remains `backup_schema_roundtrip`'s
/// `migrated_tables_are_explicitly_classified_and_catalog_fks_are_parent_first`;
/// this hermetic pin records the same invariant in the writer's own suite.
#[test]
fn backup_exclusion_classification_unchanged_for_journal_tables() {
    use two_bot_core::backup::dump_file::{DUMP_TABLES, EXCLUDED_TABLES};

    for table in ["rollback_journal", "rollback_watermarks"] {
        assert!(
            EXCLUDED_TABLES.contains(&table),
            "{table} must stay backup-excluded"
        );
        assert!(
            !DUMP_TABLES.contains(&table),
            "{table} must not enter the dump inventory"
        );
    }
    // The wired path's own table stays ordinary archived application data.
    assert!(DUMP_TABLES.contains(&"events"));
    assert!(!EXCLUDED_TABLES.contains(&"events"));
}
