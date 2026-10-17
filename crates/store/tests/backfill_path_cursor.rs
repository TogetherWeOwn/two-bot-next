//! Acceptance: an impossible cursor combination returns a typed error
//! instead of panicking.
//!
//! [`history_messages_path`] builds the paged path for
//! `GET /channels/{channel}/messages`. Discord admits exactly one cursor
//! shape per read (`before`, `after` or `around`), so passing two or more
//! cursors is a caller bug — but it must surface as a typed
//! [`BackfillHistoryError::Invalid`] the caller can handle, never as a panic
//! that takes down the catch-up task.
//!
//! Contract pinned here (the implementation must satisfy it without
//! editing this file):
//!
//! * `history_messages_path(channel_id, limit, before, after, around)`
//!   returns `Result<String, BackfillHistoryError>`.
//! * Zero or one cursor: `Ok(path)` byte-identical to the path the
//!   panic-era builder produced, including the 1-100 `limit` clamp and
//!   verbatim cursor/channel interpolation (no new validation).
//! * Two or three cursors: `Err(BackfillHistoryError::Invalid(reason))`
//!   where `reason` names the cursor problem; the call returns normally
//!   on every such combination, repeatably, with no panic.
//!
//! These tests are pure path-builder tests: no database, no network.

use two_bot_store::backfill_history::{history_messages_path, BackfillHistoryError};

/// Unwrap an `Ok` path or fail the test naming the unexpected error.
fn ok_path(result: Result<String, BackfillHistoryError>) -> String {
    match result {
        Ok(path) => path,
        Err(err) => panic!("expected Ok path, got Err: {err}"),
    }
}

/// Assert a multi-cursor call errs with the typed validation variant
/// (not `Db`, not a panic) and that the reason names the cursor problem.
fn assert_invalid_cursor_error(result: Result<String, BackfillHistoryError>) {
    match result {
        Ok(path) => panic!("expected typed cursor error, got Ok path: {path}"),
        Err(BackfillHistoryError::Invalid(reason)) => {
            assert!(
                reason.contains("cursor"),
                "error reason must name the cursor problem, got: {reason:?}"
            );
        }
        Err(BackfillHistoryError::Db(_)) => {
            panic!("expected Invalid cursor error, got Db error")
        }
    }
}

// --- Green paths: zero or one cursor, byte-identical to the old builder ---

#[test]
fn no_cursor_path_is_ok_and_byte_identical() {
    assert_eq!(
        ok_path(history_messages_path("123", 2, None, None, None)),
        "/channels/123/messages?limit=2"
    );
}

#[test]
fn before_cursor_path_is_ok_and_byte_identical() {
    assert_eq!(
        ok_path(history_messages_path("123", 2, Some("9"), None, None)),
        "/channels/123/messages?limit=2&before=9"
    );
}

#[test]
fn after_cursor_path_is_ok_and_byte_identical() {
    assert_eq!(
        ok_path(history_messages_path("123", 2, None, Some("9"), None)),
        "/channels/123/messages?limit=2&after=9"
    );
}

#[test]
fn around_cursor_path_is_ok_and_byte_identical() {
    assert_eq!(
        ok_path(history_messages_path("123", 2, None, None, Some("9"))),
        "/channels/123/messages?limit=2&around=9"
    );
}

#[test]
fn limit_clamps_into_discord_range_on_ok_paths() {
    // Discord's 1-100 range: 0 clamps to 1, values above 100 clamp to 100.
    assert_eq!(
        ok_path(history_messages_path("123", 0, None, None, None)),
        "/channels/123/messages?limit=1"
    );
    assert_eq!(
        ok_path(history_messages_path("123", 1, None, None, None)),
        "/channels/123/messages?limit=1"
    );
    assert_eq!(
        ok_path(history_messages_path("123", 100, None, None, None)),
        "/channels/123/messages?limit=100"
    );
    assert_eq!(
        ok_path(history_messages_path("123", 200, None, None, None)),
        "/channels/123/messages?limit=100"
    );
    // The clamp composes with a single cursor.
    assert_eq!(
        ok_path(history_messages_path("123", 0, Some("9"), None, None)),
        "/channels/123/messages?limit=1&before=9"
    );
}

#[test]
fn channel_and_cursor_values_interpolate_verbatim() {
    assert_eq!(
        ok_path(history_messages_path(
            "987654321",
            50,
            None,
            Some("111222333"),
            None
        )),
        "/channels/987654321/messages?limit=50&after=111222333"
    );
    // A present-but-empty cursor is still exactly one cursor shape: the
    // old builder appended it verbatim, so the new one must too (no new
    // cursor-semantics validation sneaks in with the error change).
    assert_eq!(
        ok_path(history_messages_path("123", 2, Some(""), None, None)),
        "/channels/123/messages?limit=2&before="
    );
}

// --- Red paths: every impossible combination errs, none panics ---

#[test]
fn before_and_after_cursor_combination_returns_typed_error() {
    assert_invalid_cursor_error(history_messages_path("123", 2, Some("1"), Some("2"), None));
}

#[test]
fn before_and_around_cursor_combination_returns_typed_error() {
    assert_invalid_cursor_error(history_messages_path("123", 2, Some("1"), None, Some("3")));
}

#[test]
fn after_and_around_cursor_combination_returns_typed_error() {
    assert_invalid_cursor_error(history_messages_path("123", 2, None, Some("2"), Some("3")));
}

#[test]
fn all_three_cursors_combination_returns_typed_error() {
    assert_invalid_cursor_error(history_messages_path(
        "123",
        2,
        Some("1"),
        Some("2"),
        Some("3"),
    ));
}

#[test]
fn multi_cursor_error_is_not_rescued_by_limit_clamping() {
    // Validation must not depend on the limit: an out-of-range limit that
    // would clamp on a valid call still errs on an impossible combination.
    assert_invalid_cursor_error(history_messages_path("123", 0, Some("1"), Some("2"), None));
    assert_invalid_cursor_error(history_messages_path(
        "123",
        200,
        None,
        Some("2"),
        Some("3"),
    ));
}

#[test]
fn multi_cursor_error_is_repeatable_and_builder_stays_usable() {
    // The builder is a pure function: the same impossible input errs the
    // same way twice (no panic, no latched state), and a valid call made
    // afterwards still succeeds byte-identically.
    assert_invalid_cursor_error(history_messages_path(
        "123",
        2,
        Some("1"),
        Some("2"),
        Some("3"),
    ));
    assert_invalid_cursor_error(history_messages_path(
        "123",
        2,
        Some("1"),
        Some("2"),
        Some("3"),
    ));
    assert_eq!(
        ok_path(history_messages_path("123", 2, Some("9"), None, None)),
        "/channels/123/messages?limit=2&before=9"
    );
}
