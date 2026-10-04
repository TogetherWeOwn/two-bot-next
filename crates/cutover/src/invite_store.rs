//! Restart-safe write of the standalone capture's invite-counter baseline.
//!
//! The counters are the next capture's "previous reading", so a half-written
//! set would silently corrupt the following window. Upsert, absent-code
//! removal and the common `captured_at` stamp therefore commit as one unit.

use std::collections::HashSet;

use sqlx::PgPool;

/// One invite's counter as seen at capture time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterRow {
    pub code: String,
    pub uses: i64,
    pub inviter_id: Option<String>,
    pub channel_id: Option<String>,
}

/// Replace the guild's counter baseline with `rows`, stamped `captured_at`.
///
/// All-or-nothing: any failure rolls back and leaves the previous baseline
/// (counters and timestamps) untouched. Other guilds' rows are never touched.
pub async fn store_counters(
    pool: &PgPool,
    guild_id: &str,
    rows: &[CounterRow],
    captured_at: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut seen: HashSet<&str> = HashSet::new();
    for row in rows {
        seen.insert(row.code.as_str());
        sqlx::query(
            "INSERT INTO invite_snapshots (guild_id, code, uses, inviter_id, channel_id, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6::timestamptz)
             ON CONFLICT (guild_id, code) DO UPDATE SET
               uses = excluded.uses, inviter_id = excluded.inviter_id,
               channel_id = excluded.channel_id, updated_at = excluded.updated_at",
        )
        .bind(guild_id)
        .bind(&row.code)
        .bind(row.uses)
        .bind(&row.inviter_id)
        .bind(&row.channel_id)
        .bind(captured_at)
        .execute(&mut *tx)
        .await?;
    }
    let prev: Vec<(String,)> =
        sqlx::query_as("SELECT code FROM invite_snapshots WHERE guild_id = $1")
            .bind(guild_id)
            .fetch_all(&mut *tx)
            .await?;
    for (code,) in prev {
        if !seen.contains(code.as_str()) {
            sqlx::query("DELETE FROM invite_snapshots WHERE guild_id = $1 AND code = $2")
                .bind(guild_id)
                .bind(&code)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await
}
