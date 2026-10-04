//! Rollback journal acceptance: every write journaled, watermark monotonic,
//! restorable point queryable. Only agent-testdb or a CI service container is
//! admitted, with the agent_test identity. DATABASE_URL is never consulted.
//!
//! Run: `cargo test -p two-bot-store --locked --test journal -- --ignored`
//! with `TEST_DATABASE_URL` pointed at the authorized test database.
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_store::journal::{
    advance_watermark, entries_since, identity, is_covered, record, restorable_point,
    watermark_for, JournalEntry, JournalOp, COVERED_TABLES,
};
use two_bot_store::{migrate, DB_POOL_MAX};

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
            "journal_test_{}_{}",
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
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }

    async fn journal_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM rollback_journal")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

fn insert(table: &str, row: &str) -> JournalEntry {
    JournalEntry {
        table_name: table.to_owned(),
        row_identity: row.to_owned(),
        op: JournalOp::Insert,
        pre_image: None,
    }
}

fn update(table: &str, row: &str) -> JournalEntry {
    JournalEntry {
        table_name: table.to_owned(),
        row_identity: row.to_owned(),
        op: JournalOp::Update,
        pre_image: Some(r#"{"left_at":null}"#.to_owned()),
    }
}

fn delete(table: &str, row: &str) -> JournalEntry {
    JournalEntry {
        table_name: table.to_owned(),
        row_identity: row.to_owned(),
        op: JournalOp::Delete,
        pre_image: Some(r#"{"guild_id":"42","member_id":"123"}"#.to_owned()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn journal_captures_insert_update_delete_and_returns_a_restorable_point() {
    let f = Fixture::new().await;
    // No writer has journaled yet: no cursor and a zero restorable point —
    // not proof of zero writes, so the procedure reconciles from baseline.
    assert_eq!(watermark_for(&f.pool, "members").await.unwrap(), None);
    assert_eq!(restorable_point(&f.pool).await.unwrap(), 0);

    let row = identity(&["42", "123"]);
    let first = record(&f.pool, &insert("members", &row)).await.unwrap();
    let second = record(&f.pool, &update("members", &row)).await.unwrap();
    let third = record(&f.pool, &delete("members", &row)).await.unwrap();
    assert!(first < second && second < third);

    // The table cursor tracks the newest committed row; the global point is
    // the newest journal id across all tables.
    assert_eq!(
        watermark_for(&f.pool, "members").await.unwrap(),
        Some(third)
    );
    assert_eq!(restorable_point(&f.pool).await.unwrap(), third);

    // Replay reads every row after the baseline, in journal order, with the
    // pre-images updates and deletes restore from.
    let replay = entries_since(&f.pool, "members", 0).await.unwrap();
    assert_eq!(replay.len(), 3);
    let ops: Vec<JournalOp> = replay.iter().map(|entry| entry.op).collect();
    assert_eq!(
        ops,
        vec![JournalOp::Insert, JournalOp::Update, JournalOp::Delete]
    );
    assert!(replay[0].pre_image.is_none());
    assert!(replay[1].pre_image.is_some() && replay[2].pre_image.is_some());
    assert!(replay.windows(2).all(|pair| pair[0].id < pair[1].id));

    // A later baseline excludes already-reconciled rows.
    let tail = entries_since(&f.pool, "members", second).await.unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].op, JournalOp::Delete);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn watermark_stays_monotonic_under_concurrent_stale_advances() {
    let f = Fixture::new().await;
    let row = identity(&["42", "999"]);
    let committed = record(&f.pool, &insert("sticky_messages", &row))
        .await
        .unwrap();
    // Concurrent writers race stale cursors against the committed id; every
    // stale advance must be a no-op, never a rewind.
    let mut handles = Vec::new();
    for stale in 0..committed {
        let pool = f.pool.clone();
        handles.push(tokio::spawn(async move {
            advance_watermark(&pool, "sticky_messages", stale).await
        }));
    }
    for handle in handles {
        handle.await.unwrap().unwrap();
    }
    assert_eq!(
        watermark_for(&f.pool, "sticky_messages").await.unwrap(),
        Some(committed)
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn validation_refuses_uncovered_and_imageless_writes_before_any_sql() {
    let f = Fixture::new().await;
    let bad_table = JournalEntry {
        table_name: "pg_stat_activity".to_owned(),
        ..insert("members", "42:1")
    };
    assert!(record(&f.pool, &bad_table).await.is_err());
    assert!(advance_watermark(&f.pool, "pg_stat_activity", 1)
        .await
        .is_err());
    let no_image = JournalEntry {
        pre_image: None,
        ..delete("members", "42:1")
    };
    assert!(record(&f.pool, &no_image).await.is_err());
    // Rejected entries never opened a transaction: the journal is untouched.
    assert_eq!(f.journal_count().await, 0);
    assert_eq!(watermark_for(&f.pool, "members").await.unwrap(), None);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn every_covered_table_accepts_a_journal_write() {
    let f = Fixture::new().await;
    assert!(!COVERED_TABLES.is_empty());
    for (index, table) in COVERED_TABLES.iter().enumerate() {
        assert!(is_covered(table), "{table} must stay in the inventory");
        let row = identity(&[table, &index.to_string()]);
        let id = record(&f.pool, &insert(table, &row)).await.unwrap();
        assert_eq!(watermark_for(&f.pool, table).await.unwrap(), Some(id));
    }
    assert_eq!(f.journal_count().await, COVERED_TABLES.len() as i64);
    assert_eq!(
        restorable_point(&f.pool).await.unwrap(),
        f.journal_count().await
    );
    f.finish().await;
}
