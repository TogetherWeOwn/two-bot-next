#![cfg(test)]

//! Builder-owned pins for the skip/budget seam interactions the protected
//! acceptance suite does not cover: the skip is state-aware (behind-cursor
//! rows in an in-flight status are always re-attempted while settled Open
//! rows skip), and the budget `dropped` count covers only the post-skip
//! remainder. No database and no Discord calls; paused tokio time forces
//! budget exhaustion deterministically.
use super::*;

fn status_ticket(id: u64, status: TicketStatus) -> Ticket {
    Ticket {
        id: id.to_string(),
        guild_id: "100".into(),
        channel_id: Some((600 + id).to_string()),
        opener_id: "500".into(),
        claimed_by: None,
        status,
        created_at: id as i64,
        // A stale close start documents the late-actionable shape; the seam
        // itself keys re-attempts on status, while the staleness mapping is
        // pinned by the core `recovery_action` tests.
        closing_started_at: (status == TicketStatus::Closing)
            .then(|| id as i64 - INTERRUPTED_AFTER_MS - 1),
        closed_at: None,
    }
}

/// One process-wide capture for this binary (see
/// `tracing_capture::global`): each test reads only the text appended after
/// its own start offset. This module runs serially with the protected
/// skip/budget suite (see the isolated CI step), never in parallel with it.
fn skip_budget_log_capture() -> &'static crate::tracing_capture::Capture {
    crate::tracing_capture::global()
}

#[tokio::test(start_paused = true)]
async fn behind_cursor_in_flight_rows_are_reattempted_while_open_rows_skip() {
    // Pass 1 attempts the Open rows and parks the cursor at the last one.
    let cursor = TaskMutex::new(None);
    let first = TaskMutex::new(Vec::new());
    let failed = recovery_batch(
        &cursor,
        (1..=3)
            .map(|id| status_ticket(id, TicketStatus::Open))
            .collect(),
        |ticket| async {
            first.lock().unwrap().push(ticket.id);
            (Ok(()), None)
        },
    )
    .await;
    assert!(failed.is_none());
    assert_eq!(*first.lock().unwrap(), vec!["1", "2", "3"]);
    assert_eq!(
        *cursor.lock().unwrap(),
        Some((3, "3".to_string())),
        "first pass ends at the last row"
    );
    // Pass 2: row 2 moved to Closing after pass 1 attempted it as Open, and
    // row 1 is a still-unfinished Creating row. Both sit behind the cursor
    // but must be attempted again; the settled Open row stays skipped.
    let log_start = skip_budget_log_capture().text().len();
    let second = TaskMutex::new(Vec::new());
    let failed = recovery_batch(
        &cursor,
        vec![
            status_ticket(1, TicketStatus::Creating),
            status_ticket(2, TicketStatus::Closing),
            status_ticket(3, TicketStatus::Open),
        ],
        |ticket| async {
            second.lock().unwrap().push(ticket.id);
            (Ok(()), None)
        },
    )
    .await;
    assert!(failed.is_none());
    assert_eq!(
        *second.lock().unwrap(),
        vec!["1", "2"],
        "behind-cursor in-flight rows must be re-attempted"
    );
    assert_eq!(
        *cursor.lock().unwrap(),
        Some((3, "3".to_string())),
        "re-attempts must not rewind the cursor"
    );
    let logs = skip_budget_log_capture().text();
    let fresh = &logs[log_start..];
    let line = fresh
        .lines()
        .find(|line| line.contains("ticket recovery skipped behind-cursor rows"))
        .expect("partial-skip pass must log the skip outcome");
    assert!(
        line.contains("skipped=1"),
        "skip log must count only the drained Open row, got: {line:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn budget_dropped_count_covers_only_the_post_skip_remainder() {
    let log_start = skip_budget_log_capture().text().len();
    // Cursor mid-batch: rows 1..=4 skip, and the 11 survivors each outlast
    // the 20 s item budget, so the 180 s rows budget admits exactly 9 rows
    // (5..=13) and drops the remaining 2 — not 6, which a pre-drain total
    // would report.
    let cursor = TaskMutex::new(Some((4, "4".to_string())));
    let attempted = TaskMutex::new(Vec::new());
    let failed = recovery_batch(
        &cursor,
        (1..=15)
            .map(|id| status_ticket(id, TicketStatus::Open))
            .collect(),
        |ticket| async {
            attempted.lock().unwrap().push(ticket.id);
            tokio::time::sleep(Duration::from_secs(50)).await;
            (Ok(()), None)
        },
    )
    .await;
    assert!(matches!(failed, Some(ErrorClass::Timeout)));
    assert_eq!(
        *attempted.lock().unwrap(),
        vec!["5", "6", "7", "8", "9", "10", "11", "12", "13"],
        "attempts resume past the cursor up to the rows budget"
    );
    let logs = skip_budget_log_capture().text();
    let fresh = &logs[log_start..];
    let line = fresh
        .lines()
        .find(|line| line.contains("ticket recovery budget exhausted"))
        .expect("truncated pass must log the truncation");
    assert!(
        line.contains("dropped=2"),
        "dropped must count the post-skip remainder only, got: {line:?}"
    );
}
