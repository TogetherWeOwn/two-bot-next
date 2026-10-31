#![cfg(test)]

//! Protected acceptance suite: ticket recovery skips rows it already passed
//! and makes skip/budget outcomes observable.
//!
//! Every 300 s `recover()` re-reads the recoverable rows and walks them
//! through `recovery_batch` with a cursor that persists across passes. Steady
//! state — no row newer than the cursor — must skip (attempt zero tickets),
//! not replay every row; a pass cut short by the 180 s rows budget must log
//! the truncation with the dropped-row count instead of breaking silently.
//! These tests pin that contract at the cursor/budget seam with no database
//! and no Discord calls, using paused tokio time to force budget exhaustion
//! deterministically. (`recover()` itself needs a store and an executor, so
//! the seam it delegates row-walking to is pinned here; the panel slot and
//! cancel-resume paths keep their existing revision tests.)
//!
//! Protected file: the paired builder builds on this branch but must never
//! edit this file. GREEN contract this suite accepts, in full:
//! - Rows with `(created_at, id)` at or behind the cursor are not attempted.
//! - A pass that skips rows logs `ticket recovery skipped behind-cursor rows`
//!   with a `skipped=<n>` field carrying the skipped-row count.
//! - A pass cut short by the rows budget logs `ticket recovery budget
//!   exhausted` at WARN with a `dropped=<n>` field carrying the count of rows
//!   never attempted. Mechanism (rotate vs drain vs early return) is the
//!   builder's choice; only this behavior is pinned.
use super::*;

fn cursor_ticket(id: u64) -> Ticket {
    Ticket {
        id: id.to_string(),
        guild_id: "100".into(),
        channel_id: Some((600 + id).to_string()),
        opener_id: "500".into(),
        claimed_by: None,
        status: TicketStatus::Open,
        created_at: id as i64,
        closing_started_at: None,
        closed_at: None,
    }
}

/// One process-wide capture for this binary (see
/// `tracing_capture::global`): each test reads only the text appended after
/// its own start offset.
fn skip_budget_log_capture() -> &'static crate::tracing_capture::Capture {
    crate::tracing_capture::global()
}

#[tokio::test(start_paused = true)]
async fn all_behind_cursor_pass_attempts_zero_tickets() {
    // Steady state: every row is at or behind the cursor, so the pass skips.
    let cursor = TaskMutex::new(Some((7, "7".to_string())));
    let attempted = TaskMutex::new(Vec::new());
    let failed = recovery_batch(
        &cursor,
        (1..=7).map(cursor_ticket).collect(),
        |ticket| async {
            attempted.lock().unwrap().push(ticket.id);
            (Ok(()), None)
        },
    )
    .await;
    assert!(failed.is_none(), "a pure skip must not fail");
    assert!(
        attempted.lock().unwrap().is_empty(),
        "all-behind-cursor pass must attempt zero tickets"
    );
    assert_eq!(
        *cursor.lock().unwrap(),
        Some((7, "7".to_string())),
        "a pure skip advances no cursor"
    );
}

#[tokio::test(start_paused = true)]
async fn skip_all_pass_records_the_skipped_row_count() {
    let log_start = skip_budget_log_capture().text().len();
    let cursor = TaskMutex::new(Some((15, "15".to_string())));
    let attempted = TaskMutex::new(Vec::new());
    recovery_batch(
        &cursor,
        (1..=15).map(cursor_ticket).collect(),
        |ticket| async {
            attempted.lock().unwrap().push(ticket.id);
            (Ok(()), None)
        },
    )
    .await;
    assert!(
        attempted.lock().unwrap().is_empty(),
        "setup: all-behind-cursor pass must attempt zero tickets"
    );
    let logs = skip_budget_log_capture().text();
    let fresh = &logs[log_start..];
    let line = fresh
        .lines()
        .find(|line| line.contains("ticket recovery skipped behind-cursor rows"))
        .expect("skip-all pass must log the skip outcome");
    assert!(
        line.contains("skipped=15"),
        "skip log must carry the skipped-row count, got: {line:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn second_pass_with_unchanged_rows_is_a_noop() {
    let cursor = TaskMutex::new(None);
    let rows = || (1..=5).map(cursor_ticket).collect::<Vec<_>>();
    let first = TaskMutex::new(Vec::new());
    let failed = recovery_batch(&cursor, rows(), |ticket| async {
        first.lock().unwrap().push(ticket.id);
        (Ok(()), None)
    })
    .await;
    assert!(failed.is_none());
    assert_eq!(
        *first.lock().unwrap(),
        vec!["1", "2", "3", "4", "5"],
        "first pass attempts every row in cursor-key order"
    );
    assert_eq!(
        *cursor.lock().unwrap(),
        Some((5, "5".to_string())),
        "first pass ends at the last row"
    );
    let second = TaskMutex::new(Vec::new());
    let failed = recovery_batch(&cursor, rows(), |ticket| async {
        second.lock().unwrap().push(ticket.id);
        (Ok(()), None)
    })
    .await;
    assert!(failed.is_none(), "a pure skip must not fail");
    assert!(
        second.lock().unwrap().is_empty(),
        "second identical pass must attempt zero tickets"
    );
    assert_eq!(
        *cursor.lock().unwrap(),
        Some((5, "5".to_string())),
        "a pure skip advances no cursor"
    );
}

#[tokio::test(start_paused = true)]
async fn mid_batch_cursor_resumes_past_the_cursor_without_over_skip() {
    // Cursor sits mid-batch: rows 1..=4 are behind, 5..=11 resume in order.
    let log_start = skip_budget_log_capture().text().len();
    let cursor = TaskMutex::new(Some((4, "4".to_string())));
    let attempted = TaskMutex::new(Vec::new());
    let failed = recovery_batch(
        &cursor,
        (1..=11).map(cursor_ticket).collect(),
        |ticket| async {
            attempted.lock().unwrap().push(ticket.id);
            (Ok(()), None)
        },
    )
    .await;
    assert!(failed.is_none());
    assert_eq!(
        *attempted.lock().unwrap(),
        vec!["5", "6", "7", "8", "9", "10", "11"],
        "resume starts past the cursor and skips nothing after it"
    );
    assert_eq!(
        *cursor.lock().unwrap(),
        Some((11, "11".to_string())),
        "resume ends at the last attempted row"
    );
    let logs = skip_budget_log_capture().text();
    let fresh = &logs[log_start..];
    let line = fresh
        .lines()
        .find(|line| line.contains("ticket recovery skipped behind-cursor rows"))
        .expect("partial-skip pass must log the skip outcome");
    assert!(
        line.contains("skipped=4"),
        "skip log must carry the skipped-row count, got: {line:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn budget_exhaustion_logs_the_truncation_with_the_dropped_count() {
    let log_start = skip_budget_log_capture().text().len();
    // Every row outlasts its 20 s item budget, so the 180 s rows budget
    // admits exactly 9 rows (9 x 20 s) and drops the remaining 3.
    let cursor = TaskMutex::new(None);
    let attempted = TaskMutex::new(Vec::new());
    let failed = recovery_batch(
        &cursor,
        (1..=12).map(cursor_ticket).collect(),
        |ticket| async {
            attempted.lock().unwrap().push(ticket.id);
            tokio::time::sleep(Duration::from_secs(50)).await;
            (Ok(()), None)
        },
    )
    .await;
    assert!(matches!(failed, Some(ErrorClass::Timeout)));
    assert_eq!(
        attempted.lock().unwrap().len(),
        9,
        "180 s rows budget admits exactly nine 20 s rows"
    );
    let logs = skip_budget_log_capture().text();
    let fresh = &logs[log_start..];
    let line = fresh
        .lines()
        .find(|line| line.contains("ticket recovery budget exhausted"))
        .expect("truncated pass must log the truncation");
    assert!(
        line.starts_with("WARN"),
        "truncation must be a warn, got: {line:?}"
    );
    assert!(
        line.contains("dropped=3"),
        "truncation log must carry the dropped-row count, got: {line:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn fresh_cursor_attempts_every_row_in_sorted_order() {
    // Control: proves the fixture observes attempts (GREEN before and after).
    let cursor = TaskMutex::new(None);
    let attempted = TaskMutex::new(Vec::new());
    let failed = recovery_batch(
        &cursor,
        [3, 1, 2].into_iter().map(cursor_ticket).collect(),
        |ticket| async {
            attempted.lock().unwrap().push(ticket.id);
            (Ok(()), None)
        },
    )
    .await;
    assert!(failed.is_none());
    assert_eq!(
        *attempted.lock().unwrap(),
        vec!["1", "2", "3"],
        "fresh cursor attempts every row in cursor-key order"
    );
    assert_eq!(
        *cursor.lock().unwrap(),
        Some((3, "3".to_string())),
        "fresh pass ends at the last row"
    );
}

#[tokio::test(start_paused = true)]
async fn empty_batch_attempts_nothing() {
    // Control: an empty batch is a no-op (GREEN before and after).
    let cursor = TaskMutex::new(Some((9, "9".to_string())));
    let attempted = TaskMutex::new(Vec::new());
    let failed = recovery_batch(&cursor, Vec::new(), |ticket| async {
        attempted.lock().unwrap().push(ticket.id);
        (Ok(()), None)
    })
    .await;
    assert!(failed.is_none());
    assert!(
        attempted.lock().unwrap().is_empty(),
        "empty batch attempts nothing"
    );
    assert_eq!(
        *cursor.lock().unwrap(),
        Some((9, "9".to_string())),
        "empty batch advances no cursor"
    );
}
