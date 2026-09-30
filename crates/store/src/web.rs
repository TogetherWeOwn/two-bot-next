//! `web_v1` read-only contract views (two-web read surface).
//!
//! The tables live in migrations; the views live here, embedded from
//! `crates/store/sql/web_v1.sql`, and apply with `CREATE OR REPLACE`
//! (idempotent — applying twice is a no-op). A contract view is edited over
//! its life (a v1.1 adds a column), which migrations forbid, so views are
//! versioned by the `contract_version` row in `web_contract_meta`, not by a
//! new migration file. `CREATE OR REPLACE VIEW` cannot rename, reorder,
//! remove or retype a column: a breaking change FAILS loudly here at deploy
//! time, which is the signal that the change needs a `web_v2` schema rather
//! than an edit. See `sql/web_v1.sql` header and `docs/WEBSITE_CONTRACT.md`.

use sqlx::{Pool, Postgres};

/// The embedded contract: byte-for-byte `crates/store/sql/web_v1.sql`.
pub const WEB_V1_SQL: &str = include_str!("../sql/web_v1.sql");

/// Apply the `web_v1` views to `pool`. Idempotent; safe to run at every boot
/// after [`crate::migrations::migrate`].
pub async fn apply_web_contract(pool: &Pool<Postgres>) -> Result<(), sqlx::Error> {
    let schema: String = sqlx::query_scalar("SELECT current_schema()")
        .fetch_one(pool)
        .await?;
    let contract_schema = if schema == "public" {
        "web_v1".to_owned()
    } else {
        format!("{schema}_web_v1")
    };
    let quoted = format!("\"{}\"", contract_schema.replace('"', "\"\""));
    let sql = WEB_V1_SQL.replace("web_v1", &quoted);
    // Only the schema identifier varies; double quotes are escaped above.
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await?;
    Ok(())
}

/// Contract version this build implements (matches the
/// `web_contract_meta` seed row in migration 0003).
pub const CONTRACT_VERSION: &str = "1.0";
