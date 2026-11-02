//! Builder pin: config conflict-updates bump `updated_at` (starboard foundation).
//!
//! `rollback_delta` measures `starboard_config` by `updated_at` alone
//! (`TableMeasure::Columns(&["updated_at"])`), so the `updated_at =
//! clock_timestamp()` arm of the wrapper upsert is load-bearing: losing it
//! would make every later config change invisible to rollback-delta export.
//! The acceptance suites never read `updated_at` after a wrapper update, so
//! this builder-owned target pins it here (the protected files stay
//! byte-identical to the Test Author's manifests).
//!
//! The test is deterministic: it backdates the row by an hour through raw
//! SQL, updates through the wrapper, and asserts via raw SQL that
//! `updated_at` strictly advances past the backdated value and that the row
//! is visible to a rollback-delta-style `updated_at >= watermark` query.
//! Removing the bump turns this suite red.
//!
//! Only agent-testdb or a CI service container is admitted, with the
//! agent_test identity. DATABASE_URL is never consulted.
//!
//! Run: `cargo test -p two-bot-store --locked --test starboard_config_updated_at -- --ignored`
//! with `TEST_DATABASE_URL` pointed at the authorized test database.
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_store::starboard::{update_starboard_config, StarboardConfig};
use two_bot_store::{migrate, DB_POOL_MAX};

const GUILD: i64 = 100_000_000_000_000_001;
const BOARD_CHANNEL: i64 = 100_000_000_000_000_010;

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
        let admin = two_bot_store::connect_pool_with_tls(
            &url,
            two_bot_core::database_tls::TlsPolicy::LocalOnly,
        )
        .await
        .expect("test DB connect failed");
        let schema = format!(
            "starboard_updated_at_test_{}_{}",
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

    async fn finish(self) {
        self.pool.close().await;
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
async fn config_conflict_update_bumps_updated_at_for_rollback_delta() {
    let f = Fixture::new().await;
    update_starboard_config(&f.pool, GUILD, &StarboardConfig::default())
        .await
        .unwrap();
    // Backdate through raw SQL so the advance assert needs no clock sleep.
    sqlx::query("UPDATE starboard_config SET updated_at = updated_at - interval '1 hour' WHERE guild_id = $1")
        .bind(GUILD)
        .execute(&f.pool)
        .await
        .unwrap();
    let before: String =
        sqlx::query_scalar("SELECT updated_at::text FROM starboard_config WHERE guild_id = $1")
            .bind(GUILD)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    let changed = StarboardConfig {
        board_channel_id: Some(BOARD_CHANNEL),
        threshold: 5,
        ..StarboardConfig::default()
    };
    update_starboard_config(&f.pool, GUILD, &changed)
        .await
        .unwrap();
    let advanced: bool = sqlx::query_scalar(
        "SELECT updated_at > $1::timestamptz FROM starboard_config WHERE guild_id = $2",
    )
    .bind(&before)
    .bind(GUILD)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(
        advanced,
        "a wrapper conflict-update must strictly advance updated_at past {before}"
    );
    // Rollback-delta-style watermark: a watermark between the backdated value
    // and now must still select the row, or rollback export misses the change.
    let visible: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM starboard_config
         WHERE guild_id = $1 AND updated_at >= ($2::timestamptz + interval '30 minutes')",
    )
    .bind(GUILD)
    .bind(&before)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        visible, 1,
        "the updated row must be visible to an updated_at watermark query"
    );
    f.finish().await;
}
