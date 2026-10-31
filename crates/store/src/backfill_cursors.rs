//! Resumable per-family backfill cursors (TOG-22852 AC4).
//!
//! A crash must never restart a window from zero: each endpoint family
//! checkpoints its opaque Discord pagination cursor via
//! [`put_backfill_cursor`], and the worker resumes via
//! [`get_backfill_cursor`] on boot. An untouched family has no row —
//! [`get_backfill_cursor`] returns `None` (resume-from-start), never a
//! synthesized zero cursor.
//!
//! Privacy fence (hard, same as event-store S1): rows carry ids, cursors and
//! timestamps only (`family`, `cursor`, `updated_at`). No message bodies, no
//! custom-status text, no payload columns. Both the Rust validation below and
//! the SQL `CHECK` in `0408_backfill_cursors.sql` refuse empty/zero cursors,
//! so a zero restart can never be persisted.
//!
//! The SQL here is byte-for-byte the SQL the
//! `crates/store/tests/backfill_cursors.rs` acceptance target uses, so the
//! wrapper and the tests can never drift apart.

use sqlx::{Pool, Postgres};

/// The nine backfill families from the TOG-22852 plan.
pub const BACKFILL_FAMILIES: &[&str] = &[
    "audit",
    "members",
    "history",
    "event_users",
    "reactions",
    "pins",
    "bans",
    "invites",
    "onboarding",
];

/// True when `family` is a known backfill family.
#[must_use]
pub fn is_backfill_family(family: &str) -> bool {
    BACKFILL_FAMILIES.contains(&family)
}

/// Upsert SQL, byte-for-byte the acceptance target's `put`.
const PUT_SQL: &str = "INSERT INTO backfill_cursors (family, cursor, updated_at)
         VALUES ($1, $2, now())
         ON CONFLICT (family) DO UPDATE
           SET cursor = excluded.cursor, updated_at = now()";

/// Read SQL, byte-for-byte the acceptance target's `get`.
const GET_SQL: &str = "SELECT cursor FROM backfill_cursors WHERE family = $1";

/// Cursor-store failure: a database error, or a cursor that fails
/// validation. Validation runs before any SQL, so an invalid write never
/// touches the database.
///
/// Fail closed: the `Db` variant keeps its `sqlx::Error` payload (the
/// frozen acceptance suite constructs it directly, and `?` conversions need
/// the `From` impl), but its `Display`, `Debug` and `source()` never touch
/// it, so no SQLSTATE, message, connection URL or statement detail can
/// surface through error formatting. Structured callers that need a failure
/// token use their own fixed `db_class`/`db_cause` values, never this
/// error's text.
pub enum BackfillCursorError {
    Db(sqlx::Error),
    Invalid(&'static str),
}

impl std::fmt::Display for BackfillCursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(_) => write!(f, "backfill cursor database error: database error"),
            Self::Invalid(reason) => write!(f, "invalid backfill cursor: {reason}"),
        }
    }
}

impl std::fmt::Debug for BackfillCursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Never the payload: derived `Debug` would print the whole sqlx chain.
            Self::Db(_) => write!(f, "Db"),
            Self::Invalid(reason) => write!(f, "Invalid({reason:?})"),
        }
    }
}

impl std::error::Error for BackfillCursorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Never the payload: the sqlx chain carries URLs and statements.
        None
    }
}

impl From<sqlx::Error> for BackfillCursorError {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(error)
    }
}

/// Checkpoint `family`'s cursor. Overwrites any previous cursor for the
/// family and stamps `updated_at`; other families are untouched. Refuses
/// unknown families and empty/zero cursors before any SQL.
pub async fn put_backfill_cursor(
    pool: &Pool<Postgres>,
    family: &str,
    cursor: &str,
) -> Result<(), BackfillCursorError> {
    if !is_backfill_family(family) {
        return Err(BackfillCursorError::Invalid("unknown backfill family"));
    }
    if cursor.is_empty() || cursor == "0" {
        return Err(BackfillCursorError::Invalid(
            "cursor must be a non-zero opaque id",
        ));
    }
    sqlx::query(PUT_SQL)
        .bind(family)
        .bind(cursor)
        .execute(pool)
        .await?;
    Ok(())
}

/// The durable cursor for `family`: the last checkpoint
/// [`put_backfill_cursor`] committed. `None` means the family never
/// checkpointed — resume from the start, not from zero. The read path takes
/// any family name verbatim so a new family reads as cursorless instead of
/// failing.
pub async fn get_backfill_cursor(
    pool: &Pool<Postgres>,
    family: &str,
) -> Result<Option<String>, BackfillCursorError> {
    let cursor: Option<String> = sqlx::query_scalar(GET_SQL)
        .bind(family)
        .fetch_optional(pool)
        .await?;
    Ok(cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_inventory_has_no_duplicates_or_blanks() {
        let mut seen = std::collections::HashSet::new();
        for family in BACKFILL_FAMILIES {
            assert!(!family.trim().is_empty(), "inventory has a blank family");
            assert_eq!(*family, family.trim(), "inventory family is not trimmed");
            assert!(seen.insert(family), "inventory lists {family} twice");
        }
        // The nine endpoint families anchor the inventory.
        assert_eq!(BACKFILL_FAMILIES.len(), 9);
        for anchor in [
            "audit",
            "members",
            "history",
            "event_users",
            "reactions",
            "pins",
            "bans",
            "invites",
            "onboarding",
        ] {
            assert!(is_backfill_family(anchor), "inventory must cover {anchor}");
        }
        assert!(!is_backfill_family("messages"));
        assert!(!is_backfill_family(""));
        assert!(!is_backfill_family("Audit"));
    }

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
            let err: BackfillCursorError = raw.into();
            let display = err.to_string();
            assert_eq!(display, "backfill cursor database error: database error");
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
        // The acceptance target inlines these literals; the wrapper must agree
        // byte for byte so the two can never drift apart.
        assert_eq!(
            PUT_SQL,
            "INSERT INTO backfill_cursors (family, cursor, updated_at)\n         VALUES ($1, $2, now())\n         ON CONFLICT (family) DO UPDATE\n           SET cursor = excluded.cursor, updated_at = now()"
        );
        assert_eq!(
            GET_SQL,
            "SELECT cursor FROM backfill_cursors WHERE family = $1"
        );
    }
}
