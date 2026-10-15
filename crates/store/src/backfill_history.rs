//! Channel-history metadata catch-up (TOG-22852 slice 5, TOG-22950).
//!
//! Slow, resumable catch-up of channel history metadata the bot did not
//! capture live. The wire half rides the slice-1 rate-limit client as-is:
//! `GET /channels/{channel}/messages?limit={1-100}` with `before`/`after`/
//! `around` cursors through `ActionExecutor::get_json_observed`
//! (the route already exists in `raw_get_route`; every wait arrives in a
//! response header, never a literal). This module owns the two halves the
//! acceptance target pins:
//!
//! * [`history_messages_path`] — the paged path builder. It clamps `limit`
//!   into Discord's 1-100 range and admits exactly one cursor shape per read,
//!   so the wire proof in `backfill_history.rs` and this builder agree.
//! * [`put_backfill_history_message`] / [`record_backfill_history_count`] —
//!   metadata-only persistence over byte-identical SQL to the acceptance
//!   target. Resume checkpoints the floor id under family `history` via the
//!   slice-2 [`crate::backfill_cursors`] store (0408).
//!
//! Privacy fence (hard, same as event-store S1): rows carry ids, timestamps
//! and counts only. No message-body column exists; a write naming one fails
//! at the database level (the acceptance tripwire).
//!
//! Endpoint facts verified 2026-10-12 (no numbers from memory):
//! `GET /channels/{channel.id}/messages`, `limit` 1-100 (default 50),
//! `before`/`after`/`around` message-id cursors
//! (<https://docs.discord.com/developers/resources/channel#get-channel-messages>);
//! per-route `X-RateLimit-*` buckets, 429-only `Retry-After` waits, bots at
//! 50 requests/second globally
//! (<https://docs.discord.com/developers/topics/rate-limits>).

use sqlx::{Pool, Postgres};
use two_bot_core::error_chain::sanitize_chain;

/// Upsert SQL, byte-for-byte the acceptance target's `PUT_HISTORY_MESSAGE_SQL`.
const PUT_MESSAGE_SQL: &str = "INSERT INTO backfill_history_messages
    (message_id, channel_id, author_id, sent_at)
    VALUES ($1, $2, $3, $4::timestamptz)
    ON CONFLICT (message_id) DO NOTHING";

/// Count SQL, byte-for-byte the acceptance target's `RECORD_HISTORY_COUNT_SQL`.
const RECORD_COUNT_SQL: &str = "INSERT INTO backfill_history_counts
    (channel_id, message_count, updated_at)
    VALUES ($1, $2, now())
    ON CONFLICT (channel_id) DO UPDATE
      SET message_count = excluded.message_count, updated_at = now()";

/// History-store failure: a database error, or ids/count that fail validation.
/// Validation runs before any SQL, so an invalid write never touches the database.
///
/// The `Db` Display is sanitized, never the raw sqlx text: a database error
/// can carry a connection URL, so it renders through the shared chain
/// sanitizer before reaching any failure line.
#[derive(Debug)]
pub enum BackfillHistoryError {
    Db(sqlx::Error),
    Invalid(&'static str),
}

impl std::fmt::Display for BackfillHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(inner) => write!(
                f,
                "backfill history database error: {}",
                sanitize_chain(&inner.to_string())
            ),
            Self::Invalid(message) => write!(f, "invalid backfill history row: {message}"),
        }
    }
}

impl std::error::Error for BackfillHistoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(inner) => Some(inner),
            Self::Invalid(_) => None,
        }
    }
}

impl From<sqlx::Error> for BackfillHistoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(error)
    }
}

/// Build the paged history path for `GET /channels/{channel}/messages`.
///
/// `limit` clamps into Discord's 1-100 range. Exactly one of `before` (page
/// backward from the previous page's floor), `after` (page forward) or
/// `around` (anchored read) may be set; passing more than one panics, mirroring
/// the acceptance helper so a misuse fails loudly in tests rather than paging
/// the wrong window live.
#[must_use]
pub fn history_messages_path(
    channel_id: &str,
    limit: u8,
    before: Option<&str>,
    after: Option<&str>,
    around: Option<&str>,
) -> String {
    let limit = limit.clamp(1, 100);
    let mut path = format!("/channels/{channel_id}/messages?limit={limit}");
    match (before, after, around) {
        (Some(cursor), None, None) => path.push_str(&format!("&before={cursor}")),
        (None, Some(cursor), None) => path.push_str(&format!("&after={cursor}")),
        (None, None, Some(cursor)) => path.push_str(&format!("&around={cursor}")),
        (None, None, None) => {}
        _ => panic!("one cursor shape per history read"),
    }
    path
}

/// Persist one fetched message as metadata only: ids plus the send timestamp.
/// `ON CONFLICT DO NOTHING` keeps re-fetched pages idempotent without moving
/// an existing row. Refuses blank ids before any SQL.
pub async fn put_backfill_history_message(
    pool: &Pool<Postgres>,
    message_id: &str,
    channel_id: &str,
    author_id: &str,
    sent_at: &str,
) -> Result<(), BackfillHistoryError> {
    if message_id.is_empty() || channel_id.is_empty() || author_id.is_empty() {
        return Err(BackfillHistoryError::Invalid(
            "message, channel and author ids must be non-empty",
        ));
    }
    if sent_at.is_empty() {
        return Err(BackfillHistoryError::Invalid(
            "sent_at must be a non-empty timestamptz",
        ));
    }
    sqlx::query(PUT_MESSAGE_SQL)
        .bind(message_id)
        .bind(channel_id)
        .bind(author_id)
        .bind(sent_at)
        .execute(pool)
        .await?;
    Ok(())
}

/// Refresh the per-channel aggregate count. Refuses blank channels and
/// negative counts before any SQL.
pub async fn record_backfill_history_count(
    pool: &Pool<Postgres>,
    channel_id: &str,
    message_count: i64,
) -> Result<(), BackfillHistoryError> {
    if channel_id.is_empty() {
        return Err(BackfillHistoryError::Invalid(
            "channel id must be non-empty",
        ));
    }
    if message_count < 0 {
        return Err(BackfillHistoryError::Invalid(
            "message count must be non-negative",
        ));
    }
    sqlx::query(RECORD_COUNT_SQL)
        .bind(channel_id)
        .bind(message_count)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_sql_matches_the_acceptance_target_byte_for_byte() {
        // The acceptance target inlines these literals; the wrappers must agree
        // byte for byte so the two can never drift apart.
        assert_eq!(
            PUT_MESSAGE_SQL,
            "INSERT INTO backfill_history_messages\n    (message_id, channel_id, author_id, sent_at)\n    VALUES ($1, $2, $3, $4::timestamptz)\n    ON CONFLICT (message_id) DO NOTHING"
        );
        assert_eq!(
            RECORD_COUNT_SQL,
            "INSERT INTO backfill_history_counts\n    (channel_id, message_count, updated_at)\n    VALUES ($1, $2, now())\n    ON CONFLICT (channel_id) DO UPDATE\n      SET message_count = excluded.message_count, updated_at = now()"
        );
    }

    #[test]
    fn history_path_clamps_limit_and_admits_one_cursor_shape() {
        assert_eq!(
            history_messages_path("123", 2, None, None, None),
            "/channels/123/messages?limit=2"
        );
        // Discord's 1-100 range: 0 clamps to 1, 200 clamps to 100.
        assert_eq!(
            history_messages_path("123", 0, None, None, None),
            "/channels/123/messages?limit=1"
        );
        assert_eq!(
            history_messages_path("123", 200, None, None, None),
            "/channels/123/messages?limit=100"
        );
        assert_eq!(
            history_messages_path("123", 2, Some("9"), None, None),
            "/channels/123/messages?limit=2&before=9"
        );
        assert_eq!(
            history_messages_path("123", 2, None, Some("9"), None),
            "/channels/123/messages?limit=2&after=9"
        );
        assert_eq!(
            history_messages_path("123", 2, None, None, Some("9")),
            "/channels/123/messages?limit=2&around=9"
        );
    }

    #[test]
    #[should_panic(expected = "one cursor shape per history read")]
    fn history_path_rejects_two_cursor_shapes() {
        let _ = history_messages_path("123", 2, Some("1"), Some("2"), None);
    }
}
