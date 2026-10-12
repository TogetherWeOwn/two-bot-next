//! Audit-log 45d catch-up (backfill worker slice 3).
//!
//! Slow, resumable catch-up of guild audit-log entries the bot did not
//! capture live. The wire half rides the slice-1 rate-limit client as-is:
//! `GET /guilds/{guild}/audit-logs?limit={1-100}` with `before`/`after`
//! cursors through [`two_bot_discord::ActionExecutor::get_json_observed`]
//! (the route lives in `raw_get_route`, which refuses an out-of-range limit
//! without I/O; every wait arrives in a response header, never a literal).
//! This module owns the halves the acceptance target pins:
//!
//! * [`audit_logs_path`] — the paged path builder. It clamps `limit` into
//!   Discord's 1-100 range and carries the `before`/`after` cursors, so the
//!   wire proof in the acceptance target and this builder agree.
//! * [`put_backfill_audit_entry`] / [`apply_audit_page`] — ids/timestamps/enums
//!   persistence over byte-identical SQL to the acceptance target, dropping
//!   `reason`/`options` bodies before storage and skipping entries older than
//!   the 45d window. Resume checkpoints the floor id under family `"audit"`
//!   via the slice-2 [`crate::backfill_cursors`] store (0408).
//!
//! Privacy fence (hard, same as event-store S1): rows carry ids, timestamps
//! and enum fields only (`guild_id`, `entry_id`, `action_type`, `user_id`,
//! `target_id`, `occurred_at`). No reason text, no options/body columns; a
//! write naming one fails at the database level (no such column exists).
//!
//! Endpoint facts verified 2026-10-12 (no numbers from memory):
//! `GET /guilds/{guild.id}/audit-logs`, "`limit`: Maximum number of entries
//! (between 1-100) to return, defaults to 50", plus `user_id`, `action_type`,
//! "`before`: Entries with ID less than a specific audit log entry ID",
//! "`after`: Entries with ID greater than a specific audit log entry ID"
//! (<https://docs.discord.com/developers/resources/audit-log>);
//! per-route `X-RateLimit-*` buckets, 429-only `Retry-After` waits, bots at
//! 50 requests/second globally, 401/403/429 toward the 10k-per-10-minute
//! invalid-request cutoff
//! (<https://docs.discord.com/developers/topics/rate-limits>).

use sqlx::{Pool, Postgres};
use thiserror::Error;

/// Cursor family for the audit-log window in the slice-2 cursor store.
pub const AUDIT_BACKFILL_FAMILY: &str = "audit";

/// Catch-up window, days: entries older than this are dropped, never stored.
pub const AUDIT_BACKFILL_WINDOW_DAYS: i64 = 45;

/// Upsert SQL, byte-for-byte the acceptance target's insert.
const PUT_AUDIT_ENTRY_SQL: &str = "INSERT INTO backfill_audit_entries
             (guild_id, entry_id, action_type, user_id, target_id, occurred_at)
         VALUES ($1, $2, $3, $4, $5, $6::timestamptz)
         ON CONFLICT (guild_id, entry_id) DO NOTHING";

/// Audit-store failure: a database error, or ids/timestamps that fail
/// validation. Validation runs before any SQL, so an invalid write never
/// touches the database.
#[derive(Debug, Error)]
pub enum BackfillAuditError {
    #[error("backfill audit database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("invalid backfill audit row: {0}")]
    Invalid(&'static str),
}

/// Build the paged audit-log path for `GET /guilds/{guild}/audit-logs`.
///
/// `limit` clamps into Discord's 1-100 range. `before` pages backward from
/// the previous page's floor, `after` walks back toward newer entries; both
/// may be set together as a bounded range read. The executor re-renders the
/// query through twilight's route Display, so cursor order on the wire may
/// differ — the acceptance target parses by key, never by position.
#[must_use]
pub fn audit_logs_path(
    guild_id: &str,
    limit: u32,
    before: Option<&str>,
    after: Option<&str>,
) -> String {
    let limit = limit.clamp(1, 100);
    let mut path = format!("/guilds/{guild_id}/audit-logs?limit={limit}");
    if let Some(cursor) = before {
        path.push_str(&format!("&before={cursor}"));
    }
    if let Some(cursor) = after {
        path.push_str(&format!("&after={cursor}"));
    }
    path
}

/// True when `occurred_at` falls inside the 45d catch-up window starting at
/// `window_start`. Same-format RFC3339 UTC timestamps sort chronologically,
/// so the boundary is a string comparison — the same semantics the
/// acceptance target pins (44d in, 46d out). Live callers pass a `window_start`
/// of `now - 45 days` in the same format as the entry timestamps.
#[must_use]
pub fn audit_entry_in_window(occurred_at: &str, window_start: &str) -> bool {
    occurred_at >= window_start
}

/// Persist one fetched audit-log entry as ids/timestamps/enums only.
/// `ON CONFLICT DO NOTHING` keeps re-fetched pages idempotent without moving
/// an existing row. Refuses blank ids or a missing timestamp before any SQL;
/// `reason`/`options` bodies never reach this signature by construction.
pub async fn put_backfill_audit_entry(
    pool: &Pool<Postgres>,
    guild_id: &str,
    entry_id: &str,
    action_type: i32,
    user_id: Option<&str>,
    target_id: Option<&str>,
    occurred_at: &str,
) -> Result<(), BackfillAuditError> {
    if guild_id.is_empty() || entry_id.is_empty() {
        return Err(BackfillAuditError::Invalid(
            "guild and entry ids must be non-empty",
        ));
    }
    if occurred_at.is_empty() {
        return Err(BackfillAuditError::Invalid(
            "occurred_at must be a non-empty timestamptz",
        ));
    }
    if user_id.is_some_and(str::is_empty) || target_id.is_some_and(str::is_empty) {
        return Err(BackfillAuditError::Invalid(
            "user and target ids must be non-empty when present",
        ));
    }
    sqlx::query(PUT_AUDIT_ENTRY_SQL)
        .bind(guild_id)
        .bind(entry_id)
        .bind(action_type)
        .bind(user_id)
        .bind(target_id)
        .bind(occurred_at)
        .execute(pool)
        .await?;
    Ok(())
}

/// Map one audit-log page to storage: keep ids/timestamps/enums, drop
/// `reason`/`options` bodies, enforce the 45d window. Returns the persisted
/// entry ids in page order. Entries missing an id, action type or timestamp
/// are skipped without failing the page, so one malformed entry cannot lose
/// its well-formed siblings.
pub async fn apply_audit_page(
    pool: &Pool<Postgres>,
    guild_id: &str,
    page: &serde_json::Value,
    window_start: &str,
) -> Result<Vec<String>, BackfillAuditError> {
    let mut kept = Vec::new();
    let entries = page["audit_log_entries"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for entry in &entries {
        let Some(occurred) = entry["occurred_at"].as_str() else {
            continue;
        };
        if !audit_entry_in_window(occurred, window_start) {
            continue;
        }
        let Some(id) = entry["id"].as_str() else {
            continue;
        };
        let Some(action) = entry["action_type"].as_u64() else {
            continue;
        };
        let user = entry["user_id"].as_str();
        let target = entry["target_id"].as_str();
        put_backfill_audit_entry(pool, guild_id, id, action as i32, user, target, occurred).await?;
        kept.push(id.to_owned());
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_sql_matches_the_acceptance_target_byte_for_byte() {
        // The acceptance target inlines this literal; the wrapper must agree
        // byte for byte so the two can never drift apart.
        assert_eq!(
            PUT_AUDIT_ENTRY_SQL,
            "INSERT INTO backfill_audit_entries\n             (guild_id, entry_id, action_type, user_id, target_id, occurred_at)\n         VALUES ($1, $2, $3, $4, $5, $6::timestamptz)\n         ON CONFLICT (guild_id, entry_id) DO NOTHING"
        );
    }

    #[test]
    fn audit_path_clamps_limit_and_carries_before_after() {
        assert_eq!(
            audit_logs_path("424242", 2, None, None),
            "/guilds/424242/audit-logs?limit=2"
        );
        // Discord's 1-100 range: 0 clamps to 1, 101 clamps to 100.
        assert_eq!(
            audit_logs_path("424242", 0, None, None),
            "/guilds/424242/audit-logs?limit=1"
        );
        assert_eq!(
            audit_logs_path("424242", 101, None, None),
            "/guilds/424242/audit-logs?limit=100"
        );
        assert_eq!(
            audit_logs_path("424242", 2, Some("1003"), None),
            "/guilds/424242/audit-logs?limit=2&before=1003"
        );
        assert_eq!(
            audit_logs_path("424242", 2, None, Some("1002")),
            "/guilds/424242/audit-logs?limit=2&after=1002"
        );
    }

    #[test]
    fn audit_window_pins_the_45d_boundary() {
        // Same boundary the acceptance target pins: 44d inside, 46d outside.
        assert!(audit_entry_in_window(
            "2026-08-29T00:00:00.000000+00:00",
            "2026-08-28T00:00:00.000000+00:00"
        ));
        assert!(!audit_entry_in_window(
            "2026-08-27T00:00:00.000000+00:00",
            "2026-08-28T00:00:00.000000+00:00"
        ));
        assert_eq!(AUDIT_BACKFILL_FAMILY, "audit");
        assert_eq!(AUDIT_BACKFILL_WINDOW_DAYS, 45);
    }
}
