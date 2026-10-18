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
/// Fail closed: the `Db` variant keeps its `sqlx::Error` payload (the
/// frozen acceptance suite constructs it directly, and `?` conversions need
/// the `From` impl), but its `Display`, `Debug` and `source()` never touch
/// it, so no SQLSTATE, message, connection URL or statement detail can
/// surface through error formatting. Structured callers that need a failure
/// token use their own fixed `db_class`/`db_cause` values, never this
/// error's text.
pub enum BackfillHistoryError {
    Db(sqlx::Error),
    Invalid(&'static str),
}

impl std::fmt::Display for BackfillHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(_) => write!(f, "backfill history database error: database error"),
            Self::Invalid(reason) => write!(f, "invalid backfill history row: {reason}"),
        }
    }
}

impl std::fmt::Debug for BackfillHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Never the payload: derived `Debug` would print the whole sqlx chain.
            Self::Db(_) => write!(f, "Db"),
            Self::Invalid(reason) => write!(f, "Invalid({reason:?})"),
        }
    }
}

impl std::error::Error for BackfillHistoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Never the payload: the sqlx chain carries URLs and statements.
        None
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
/// `around` (anchored read) may be set; passing more than one returns
/// [`BackfillHistoryError::Invalid`], mirroring the acceptance helper so a
/// misuse surfaces as a typed error the caller can handle rather than
/// paging the wrong window live.
pub fn history_messages_path(
    channel_id: &str,
    limit: u8,
    before: Option<&str>,
    after: Option<&str>,
    around: Option<&str>,
) -> Result<String, BackfillHistoryError> {
    if [before.is_some(), after.is_some(), around.is_some()]
        .into_iter()
        .filter(|present| *present)
        .count()
        > 1
    {
        return Err(BackfillHistoryError::Invalid(
            "one cursor shape per history read",
        ));
    }
    let limit = limit.clamp(1, 100);
    let mut path = format!("/channels/{channel_id}/messages?limit={limit}");
    if let Some(cursor) = before {
        path.push_str(&format!("&before={cursor}"));
    } else if let Some(cursor) = after {
        path.push_str(&format!("&after={cursor}"));
    } else if let Some(cursor) = around {
        path.push_str(&format!("&around={cursor}"));
    }
    Ok(path)
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
    fn db_error_display_and_debug_hide_sqlx_text() {
        // Fail-closed regression: ordinary non-pattern text such as
        // `invalid member display name` matches no token redactor, so the
        // fixed Display/Debug/source must drop it structurally. The
        // `InvalidArgument` case is the strongest one: its message is
        // verbatim caller text, the exact shape a pattern redactor keeps.
        for raw in [
            sqlx::Error::InvalidArgument("invalid member display name".into()),
            sqlx::Error::RowNotFound,
        ] {
            let err: BackfillHistoryError = raw.into();
            let display = err.to_string();
            assert_eq!(display, "backfill history database error: database error");
            assert!(
                !display.contains("invalid member display name"),
                "Display leaked sqlx text: {display}"
            );
            let debug = format!("{err:?}");
            assert!(
                !debug.contains("invalid member display name"),
                "Debug leaked sqlx text: {debug}"
            );
            assert!(
                std::error::Error::source(&err).is_none(),
                "source chain re-exposed the sqlx error"
            );
        }
    }

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
            history_messages_path("123", 2, None, None, None).unwrap(),
            "/channels/123/messages?limit=2"
        );
        // Discord's 1-100 range: 0 clamps to 1, 200 clamps to 100.
        assert_eq!(
            history_messages_path("123", 0, None, None, None).unwrap(),
            "/channels/123/messages?limit=1"
        );
        assert_eq!(
            history_messages_path("123", 200, None, None, None).unwrap(),
            "/channels/123/messages?limit=100"
        );
        assert_eq!(
            history_messages_path("123", 2, Some("9"), None, None).unwrap(),
            "/channels/123/messages?limit=2&before=9"
        );
        assert_eq!(
            history_messages_path("123", 2, None, Some("9"), None).unwrap(),
            "/channels/123/messages?limit=2&after=9"
        );
        assert_eq!(
            history_messages_path("123", 2, None, None, Some("9")).unwrap(),
            "/channels/123/messages?limit=2&around=9"
        );
    }

    #[test]
    fn history_path_rejects_every_multi_cursor_shape_with_a_typed_error() {
        for (before, after, around) in [
            (Some("1"), Some("2"), None),
            (Some("1"), None, Some("3")),
            (None, Some("2"), Some("3")),
            (Some("1"), Some("2"), Some("3")),
        ] {
            // `expect_err` plus a non-constant `matches!` keeps the test free
            // of `panic!` literals (rust-panic-in-prod gate) and of
            // `assert!(false, ..)` (clippy::assertions-on-constants).
            let err = history_messages_path("123", 2, before, after, around)
                .expect_err("expected typed Invalid cursor error");
            assert!(
                matches!(
                    err,
                    BackfillHistoryError::Invalid(reason) if reason.contains("cursor")
                ),
                "error reason must name the cursor problem, got: {err:?}"
            );
        }
    }
}
