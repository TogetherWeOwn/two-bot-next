//! sqlx connection pool: max 5 + 15 s statement timeout (legacy parity).
//!
//! Mirrors legacy `src/store/postgresDriver.ts` (`TWO_DB_POOL_MAX ?? 5`,
//! `statementTimeoutMillis ?? 15_000`) and the cutover seam
//! (`two-bot-cutover::db::connect`). The statement timeout rides the
//! connection options, so no per-connection `SET` is needed.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Pool, Postgres};
use thiserror::Error;
use two_bot_core::database_tls::{self, TlsPolicy};

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
    #[error("unsupported database URL parameter")]
    UnsupportedParameter,
    #[error("{0}")]
    Tls(&'static str),
    #[error("invalid TWO_DATABASE_TLS setting")]
    TlsSetting,
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
    /// `levels-import-rewards-probe` precedent). The `TWO_DATABASE_TLS`
    /// policy applies: unset means `required` (see `docs/database-tls.md`).
    pub async fn connect(url: &str, skip_migrations: bool) -> Result<Self, ConnectError> {
        let pool = connect_pool_with_tls(url, tls_policy_from_env()?).await?;
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
/// and tests that manage schema themselves. The `TWO_DATABASE_TLS` policy
/// applies: unset means `required` (see `docs/database-tls.md`).
pub async fn connect_pool(url: &str) -> Result<Pool<Postgres>, ConnectError> {
    connect_pool_with_tls(url, tls_policy_from_env()?).await
}

/// Read the one TLS policy setting; an unset value is `Required`.
fn tls_policy_from_env() -> Result<TlsPolicy, ConnectError> {
    let value = std::env::var_os(database_tls::POLICY_SETTING);
    // A non-UTF-8 value parses as "" and is refused like any unknown value.
    TlsPolicy::from_setting(value.as_ref().map(|v| v.to_str().unwrap_or("")))
        .map_err(|_| ConnectError::TlsSetting)
}

/// [`connect_pool`] with an explicit TLS policy (tests pass `LocalOnly`).
pub async fn connect_pool_with_tls(
    url: &str,
    tls: TlsPolicy,
) -> Result<Pool<Postgres>, ConnectError> {
    if url.trim().is_empty() {
        return Err(ConnectError::MissingUrl);
    }
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return Err(ConnectError::NotPostgres);
    }
    two_bot_core::database_url::validate(url).map_err(|message| match message {
        "unsupported database URL parameter" => ConnectError::UnsupportedParameter,
        _ => ConnectError::InvalidUrl,
    })?;
    // Threat-model F6: refuse plaintext/unverified modes and the wrong host
    // class before SQLx parses the URL (see `docs/database-tls.md`).
    database_tls::enforce(url, tls).map_err(ConnectError::Tls)?;
    // Passfile diagnostics stay suppressed during the synchronous parse, but a
    // well-formed entry still supplies the password (see `database_url`).
    // Validation already passed, so a residual parse failure is an invalid URL.
    let parsed =
        two_bot_core::database_url::connect_options(url).map_err(|_| ConnectError::InvalidUrl)?;
    let mut options = database_tls::apply(parsed, tls);
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
