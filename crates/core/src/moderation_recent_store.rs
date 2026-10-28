//! Read side of `/moderation-recent`. Enabled only by the core `db` feature.
//!
//! One guild-scoped, `LIMIT`-bounded `SELECT` over the saved audit ledger
//! plus warning rows that no audit row corroborates. Nothing here claims,
//! sweeps, retries, repairs or deletes: it never opens a transaction and
//! never selects claim, idempotency or metadata columns.

use sqlx::{PgPool, Row};

use crate::moderation_recent::{RecentRow, RECENT_WINDOW};

/// The newest saved rows for `guild_id`, ordered `created_at DESC` then
/// `request_id ASC` so same-instant rows never depend on insertion order.
///
/// A warning recorded through the member runtime has an audit row and a
/// warnings row sharing one `request_id`; the audit row wins and the
/// warnings row is only projected when no audit row carries its identity.
///
/// # Errors
///
/// Returns the driver error unchanged; callers must not render it.
pub async fn read_recent_moderation(
    pool: &PgPool,
    guild_id: &str,
) -> Result<Vec<RecentRow>, sqlx::Error> {
    let limit = i64::try_from(RECENT_WINDOW).unwrap_or(i64::MAX);
    let rows = sqlx::query(
        "SELECT request_id, actor_id, action, target_id, channel_id, reason, outcome,
                to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') || 'T'
                || to_char(created_at AT TIME ZONE 'UTC', 'HH24:MI:SS') || 'Z' AS created_text
           FROM (
             (SELECT request_id, actor_id, action, target_id, channel_id, reason, outcome,
                     created_at
                FROM moderation_audit
               WHERE guild_id = $1
               ORDER BY created_at DESC, request_id ASC
               LIMIT $2)
             UNION ALL
             (SELECT w.request_id, w.actor_id, 'moderation.warn' AS action,
                     w.user_id AS target_id, NULL::text AS channel_id, w.reason,
                     'warned' AS outcome, w.created_at
                FROM moderation_warnings AS w
               WHERE w.guild_id = $1
                 AND NOT EXISTS (
                   SELECT 1 FROM moderation_audit AS a
                    WHERE a.guild_id = w.guild_id AND a.request_id = w.request_id
                 )
               ORDER BY w.created_at DESC, w.request_id ASC
               LIMIT $2)
           ) AS ledger
          ORDER BY created_at DESC, request_id ASC
          LIMIT $2",
    )
    .bind(guild_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(RecentRow {
                request_id: row.try_get("request_id")?,
                actor_id: row.try_get("actor_id")?,
                action: row.try_get("action")?,
                target_id: row.try_get("target_id")?,
                channel_id: row.try_get("channel_id")?,
                reason: row.try_get("reason")?,
                outcome: row.try_get("outcome")?,
                created_at: row.try_get("created_text")?,
            })
        })
        .collect()
}
