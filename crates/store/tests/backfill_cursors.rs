//! Backfill cursor store acceptance (TOG-22852 AC4, paired failing tests for TOG-22939).
//!
//! Test files ONLY: this target compiles green but each test FAILS until the
//! builder lands the cursor store (currently: `relation "backfill_cursors"
//! does not exist`). The build card implements:
//!
//! * a `backfill_cursors` table keyed by family holding ids/cursors/timestamps
//!   only (privacy fence: no message bodies, no status text), applied from the
//!   store migration chain so [`migrate`] creates it;
//! * a small `two_bot_store::backfill_cursors` wrapper (suggested:
//!   `BACKFILL_FAMILIES`, `put_backfill_cursor`, `get_backfill_cursor`) over
//!   the exact SQL below, so the wrapper and these tests agree byte for byte.
//!
//! What the tests prove (TOG-22852 AC4 "Resumable cursors persisted"):
//!
//! 1. cursor insert/update per backfill family (audit, members, history,
//!    event users, reactions, pins, bans, invites, onboarding);
//! 2. crash-restart resumes from the persisted cursor, never from zero
//!    (kill-and-resume fixture);
//! 3. concurrent families never clobber each other's cursors;
//! 4. cursor rows hold ids/cursors/timestamps only.
//!
//! Only agent-testdb or a CI service container is admitted, with the
//! agent_test identity. DATABASE_URL is never consulted.
//!
//! Run: `cargo test -p two-bot-store --locked --test backfill_cursors -- --ignored`
//! with `TEST_DATABASE_URL` pointed at the authorized test database.
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_store::{migrate, DB_POOL_MAX};

/// The nine backfill families from the TOG-22852 plan (CEO decision).
const BACKFILL_FAMILIES: [&str; 9] = [
    "audit",
    "members",
    "history",
    "event_users",
    "reactions",
    "pins",
    "bans",
    "invites",
    "onboarding",
];

/// Upsert a family's cursor. Mirrors the builder's `put_backfill_cursor`.
async fn put(pool: &Pool<Postgres>, family: &str, cursor: &str) {
    sqlx::query(
        "INSERT INTO backfill_cursors (family, cursor, updated_at)
         VALUES ($1, $2, now())
         ON CONFLICT (family) DO UPDATE
           SET cursor = excluded.cursor, updated_at = now()",
    )
    .bind(family)
    .bind(cursor)
    .execute(pool)
    .await
    .unwrap();
}

/// Read a family's cursor: `None` when the family never checkpointed.
/// Mirrors the builder's `get_backfill_cursor`.
async fn get(pool: &Pool<Postgres>, family: &str) -> Option<String> {
    sqlx::query_scalar("SELECT cursor FROM backfill_cursors WHERE family = $1")
        .bind(family)
        .fetch_optional(pool)
        .await
        .unwrap()
}

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
    url: String,
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
            "backfill_cursor_test_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = Self::connect(&url, &schema).await;
        migrate(&pool).await.unwrap();
        Self {
            admin,
            pool,
            schema,
            url,
        }
    }

    async fn connect(url: &str, schema: &str) -> Pool<Postgres> {
        let options = PgConnectOptions::from_str(url).expect("invalid TEST_DATABASE_URL");
        PgPoolOptions::new()
            .max_connections(DB_POOL_MAX)
            .connect_with(options.options([
                ("search_path", schema.to_owned()),
                ("statement_timeout", "15000ms".to_owned()),
            ]))
            .await
            .unwrap()
    }

    /// Simulate a crash-restart: drop the live pool and reconnect to the same
    /// schema, so the test reads only what was durably persisted.
    async fn restart_pool(&mut self) {
        self.pool.close().await;
        self.pool = Self::connect(&self.url, &self.schema).await;
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn backfill_cursors_persist_per_family_insert_and_update() {
    // AC4 (1): every backfill family holds its own insertable/updatable cursor.
    let f = Fixture::new().await;
    for family in BACKFILL_FAMILIES {
        assert_eq!(
            get(&f.pool, family).await,
            None,
            "{family} must start cursorless, not zero"
        );
        put(&f.pool, family, "111111111111111111").await;
        assert_eq!(
            get(&f.pool, family).await,
            Some("111111111111111111".to_owned()),
            "{family} must return its inserted cursor"
        );
        put(&f.pool, family, "222222222222222222").await;
        assert_eq!(
            get(&f.pool, family).await,
            Some("222222222222222222".to_owned()),
            "{family} update must overwrite the cursor"
        );
        let stamped: bool = sqlx::query_scalar(
            "SELECT updated_at IS NOT NULL FROM backfill_cursors WHERE family = $1",
        )
        .bind(family)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert!(stamped, "{family} must stamp updated_at on every write");
    }
    // Families stay independent: rewriting one never moves another.
    put(&f.pool, "audit", "999999999999999999").await;
    assert_eq!(
        get(&f.pool, "members").await,
        Some("222222222222222222".to_owned())
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn backfill_cursor_survives_crash_restart_and_never_resets_to_zero() {
    // AC4 (2): kill-and-resume fixture. The worker restarts from the persisted
    // cursor, never by replaying a window from zero.
    let mut f = Fixture::new().await;
    put(&f.pool, "members", "333333333333333333").await;
    put(&f.pool, "history", "444444444444444444").await;
    f.restart_pool().await;
    assert_eq!(
        get(&f.pool, "members").await,
        Some("333333333333333333".to_owned()),
        "restart must resume members from its persisted cursor"
    );
    assert_eq!(
        get(&f.pool, "history").await,
        Some("444444444444444444".to_owned()),
        "restart must resume history from its persisted cursor"
    );
    // An untouched family is cursorless (resume-from-start), never a
    // synthesized zero cursor row.
    assert_eq!(get(&f.pool, "reactions").await, None);
    let zero_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM backfill_cursors WHERE cursor IN ('', '0')")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(zero_rows, 0, "no zero/empty cursor row may exist");
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn concurrent_families_never_clobber_each_other() {
    // AC4 (3): all nine families advance at once; each keeps its own cursor.
    let f = Fixture::new().await;
    let mut handles = Vec::new();
    for (index, family) in BACKFILL_FAMILIES.iter().enumerate() {
        let pool = f.pool.clone();
        let family = (*family).to_owned();
        let cursor = format!("5555000000000000{index:02}");
        handles.push(tokio::spawn(async move {
            for _ in 0..10 {
                put(&pool, &family, &cursor).await;
                assert_eq!(get(&pool, &family).await, Some(cursor.clone()));
            }
            (family, cursor)
        }));
    }
    for handle in handles {
        let (family, cursor) = handle.await.unwrap();
        assert_eq!(
            get(&f.pool, &family).await,
            Some(cursor),
            "{family} must keep its own cursor under concurrency"
        );
    }
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn backfill_cursor_rows_hold_ids_cursors_timestamps_only() {
    // AC4 (4) + privacy fence: cursor rows carry ids/cursors/timestamps only.
    // No message bodies, no custom-status text, no payload columns.
    let f = Fixture::new().await;
    put(&f.pool, "pins", "666666666666666666").await;
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, data_type FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = 'backfill_cursors' ORDER BY 1",
    )
    .bind(&f.schema)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("cursor".to_owned(), "text".to_owned()),
            ("family".to_owned(), "text".to_owned()),
            (
                "updated_at".to_owned(),
                "timestamp with time zone".to_owned()
            ),
        ],
        "cursor rows must hold exactly family/cursor/updated_at"
    );
    // The stored cursor round-trips byte-for-byte: an opaque id, never a body.
    assert_eq!(
        get(&f.pool, "pins").await,
        Some("666666666666666666".to_owned())
    );
    f.finish().await;
}
