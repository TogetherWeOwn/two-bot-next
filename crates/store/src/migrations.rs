//! Checksum migration runner: bot range 0001–0999.
//!
//! Migrations embed at compile time via `sqlx::migrate!` and run through a
//! dedicated bookkeeping table ([`TABLE_NAME`]), so the bot and the cutover
//! tools never share a migrations table even when pointed at the same
//! database: cutover's `0001_funnel`/`0002_leveling` are `IF NOT EXISTS`
//! one-shot DDL over the same table shapes, applied under its own table
//! (`_sqlx_migrations`), while this runner owns the S6 chain under
//! [`TABLE_NAME`]. Cross-running the two against one schema is additive and
//! safe in either order; checksums are per-chain, so a same-version number
//! in the other chain can never trip a mismatch here.
//!
//! `crates/store/migrations/` is stable. The unpublished S6 foundation uses
//! **0400–0405**, preserving the legacy SQL bytes while avoiding main's
//! workspace-wide number allocations. Feature migrations must choose unused
//! bot numbers and add reviewed checksum entries in `migrations.lock`.
//! The runner picks up every `NNNN_*.sql` file in this directory; sqlx orders
//! by version. sqlx fails loudly on a
//! checksum mismatch (`VersionMismatch`) and on a previously-applied version
//! that vanished from the directory (`VersionMissing`) — both are deploy
//! failures, never silent drift. Never edit an applied migration; add a new
//! version instead.

use sqlx::{Pool, Postgres};

/// Bookkeeping table for THIS chain. Cutover's default `_sqlx_migrations`
/// table tracks its own chain on the same database without colliding.
pub const TABLE_NAME: &str = "_two_bot_migrations";

/// Embedded migrations (`crates/store/migrations/`, bot range 0001–0999).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Migration failure: checksum mismatch, missing version, dirty database, or
/// a SQL error inside a migration. Surfaces the sqlx reason unmodified.
pub type MigrationError = sqlx::migrate::MigrateError;

/// Apply pending migrations to `pool`. Idempotent; safe to run at every boot.
pub async fn migrate(pool: &Pool<Postgres>) -> Result<(), sqlx::Error> {
    if MIGRATOR.iter().any(|m| !(1..=999).contains(&m.version)) {
        return Err(sqlx::Error::InvalidArgument(
            "bot migrations must use versions 0001-0999".to_owned(),
        ));
    }
    let mut migrator = sqlx::migrate::Migrator::with_migrations(MIGRATOR.iter().cloned().collect());
    migrator.dangerous_set_table_name(TABLE_NAME);
    migrator
        .run(pool)
        .await
        .map_err(|e| sqlx::Error::Migrate(Box::new(e)))
}

/// Versions embedded in this build, ascending. Tests assert the foundation
/// set is complete (0400–0405) without pinning the
/// feature-block tail, which grows as slice cards land.
#[must_use]
pub fn embedded_versions() -> Vec<i64> {
    let mut versions: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
    versions.sort_unstable();
    versions
}
