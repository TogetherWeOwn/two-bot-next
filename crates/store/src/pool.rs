//! sqlx connection pool: max 5 + 15 s statement timeout (legacy parity).
//!
//! Mirrors legacy `src/store/postgresDriver.ts` (`TWO_DB_POOL_MAX ?? 5`,
//! `statementTimeoutMillis ?? 15_000`) and the cutover seam
//! (`two-bot-cutover::db::connect`). The statement timeout rides the
//! connection options, so no per-connection `SET` is needed.

use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use thiserror::Error;

/// Legacy pool default (`TWO_DB_POOL_MAX ?? 5`).
pub const DB_POOL_MAX: u32 = 5;
/// Legacy statement timeout (`statementTimeoutMillis ?? 15_000`).
pub const STATEMENT_TIMEOUT_MS: u64 = 15_000;

/// Pool construction failure. Messages never include the URL (it may carry
/// credentials).
#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("database URL is required")]
    MissingUrl,
    #[error("only Postgres is supported: database URL must use postgres:// or postgresql://")]
    NotPostgres,
    #[error("invalid database URL")]
    InvalidUrl,
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Runtime store handle: the pool plus whether boot migrations apply.
#[derive(Debug, Clone)]
pub struct Store {
    pool: Pool<Postgres>,
}

impl Store {
    /// Open the pool and apply pending migrations (unless `skip_migrations`).
    ///
    /// A read-only probe passes `skip_migrations = true` so it can never
    /// build schema on a database it was pointed at by accident (cutover
    /// `levels-import-rewards-probe` precedent).
    pub async fn connect(url: &str, skip_migrations: bool) -> Result<Self, ConnectError> {
        let pool = connect_pool(url).await?;
        let store = Self { pool };
        if !skip_migrations {
            if let Err(err) = async {
                store.migrate().await?;
                store.apply_web_contract().await
            }
            .await
            {
                store.pool.close().await;
                return Err(ConnectError::Db(err));
            }
        }
        Ok(store)
    }

    /// Wrap an existing pool (tests); no migrations run.
    #[must_use]
    pub fn from_pool(pool: Pool<Postgres>) -> Self {
        Self { pool }
    }

    /// Borrow the pool for direct queries.
    #[must_use]
    pub fn pool(&self) -> &Pool<Postgres> {
        &self.pool
    }

    /// Apply the crate's embedded migrations.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        crate::migrations::migrate(&self.pool).await
    }

    /// Apply the `web_v1` contract views (idempotent `CREATE OR REPLACE`).
    pub async fn apply_web_contract(&self) -> Result<(), sqlx::Error> {
        crate::web::apply_web_contract(&self.pool).await
    }

    /// Drain the pool.
    pub async fn close(self) {
        self.pool.close().await;
    }
}

/// Build the pool without running migrations. Shared by [`Store::connect`]
/// and tests that manage schema themselves.
pub async fn connect_pool(url: &str) -> Result<Pool<Postgres>, ConnectError> {
    if url.trim().is_empty() {
        return Err(ConnectError::MissingUrl);
    }
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return Err(ConnectError::NotPostgres);
    }
    let mut options = PgConnectOptions::from_str(url).map_err(|_| ConnectError::InvalidUrl)?;
    options = options.options([("statement_timeout", format!("{STATEMENT_TIMEOUT_MS}ms"))]);
    // `statement_timeout` only guards statements; a stuck TCP connect needs
    // its own bound so a dead host fails the boot probe instead of hanging it.
    let pool = PgPoolOptions::new()
        .max_connections(DB_POOL_MAX)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await?;
    Ok(pool)
}

/// Ping the database: `true` when a trivial query answers.
pub async fn ping(pool: &Pool<Postgres>) -> bool {
    sqlx::query("SELECT 1").fetch_one(pool).await.is_ok()
}
