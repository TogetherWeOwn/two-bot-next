//! Staff-only `/moderation-recent`: a bounded, read-only view of the newest
//! saved moderation ledger rows.
//!
//! Pure projection and rendering. The store read lives in
//! [`crate::moderation_recent_store`]; the Discord lifecycle, strict option
//! decode and live staff recheck live in the bot crate. Rows are what the
//! ledger saved, never live-verified Discord state, and the renderer says so.
//!
//! Outcomes are classified, never relabelled: only the ten applied outcome
//! words render `applied`, `denied`/`refused` render `denied`, and everything
//! else (`failed`, `in_progress`, `unban_failed`, anything unknown) renders
//! `uncertain`. Pages pack whole rows into one Discord message.

use crate::message_safety::{text_len, truncate, CONTENT_LIMIT};

/// Newest saved rows read per invocation. The read is `LIMIT`-bounded to
/// this window and paginated in memory.
pub const RECENT_WINDOW: usize = 100;
/// Highest selectable page. [`ROW_UNIT_CAP`] guarantees at least six rows per
/// page, so a full window never needs more than 17 pages.
pub const RECENT_MAX_PAGE: i64 = 20;
/// Reply for an empty ledger.
pub const RECENT_EMPTY_REPLY: &str = "No recent moderation actions in this server.";
/// Reply for a page below 1 or past the last page.
pub const RECENT_INVALID_PAGE_REPLY: &str =
    "Invalid page. Run /moderation-recent page:1, then use the next-page hint.";
/// Reply when the ledger cannot be read (no raw error is ever shown).
pub const RECENT_UNAVAILABLE_REPLY: &str =
    "Recent moderation actions are not available right now. Please try again shortly.";

const HEADER: &str =
    "Saved ledger rows, not live-verified Discord state (newest 100, newest first):";
/// Worst-case footer on a page with a successor: `Page 20 of 20` plus the
/// next-page hint, each on its own line.
const FOOTER_RESERVE: usize = 48;
/// Worst-case footer on the last page: `Page 20 of 20` on its own line.
const LAST_FOOTER_RESERVE: usize = 16;
const REQUEST_CAP: usize = 48;
const ACTION_CAP: usize = 32;
const OUTCOME_CAP: usize = 24;
const ID_CAP: usize = 24;
const REASON_CAP: usize = 80;

/// Outcome words that mean the saved action was applied.
const APPLIED_OUTCOMES: [&str; 10] = [
    "banned",
    "temporarily_banned",
    "kicked",
    "timed_out",
    "warned",
    "unbanned",
    "purged",
    "slowmode_updated",
    "locked_down",
    "unlocked",
];

/// What a saved outcome word honestly means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecentOutcome {
    Applied,
    Denied,
    Uncertain,
}

impl RecentOutcome {
    /// Classify a saved outcome word. Unknown words are uncertain: never
    /// applied, never silently denied.
    #[must_use]
    pub fn classify(outcome: &str) -> Self {
        if APPLIED_OUTCOMES.contains(&outcome) {
            Self::Applied
        } else if matches!(outcome, "denied" | "refused") {
            Self::Denied
        } else {
            Self::Uncertain
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Denied => "denied",
            Self::Uncertain => "uncertain",
        }
    }
}

/// One saved ledger row, projected to the fields staff may see. Claim
/// tokens, idempotency keys and metadata are never selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentRow {
    pub request_id: String,
    pub actor_id: String,
    pub action: String,
    pub target_id: Option<String>,
    pub channel_id: Option<String>,
    pub reason: String,
    pub outcome: String,
    /// `YYYY-MM-DDTHH:MM:SSZ` in UTC, formatted by the store.
    pub created_at: String,
}

/// One rendered page, or why there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecentView {
    Page(String),
    Empty,
    OutOfRange,
}

/// Make a stored value inert for display: control characters fold to a
/// space, backticks become apostrophes (the value is rendered inside a code
/// span, where Discord renders mentions, markdown and links as plain text),
/// and the result is cut to `cap` UTF-16 units with an ellipsis.
fn inert(value: &str, cap: usize) -> String {
    let cleaned: String = value
        .chars()
        .map(|ch| match ch {
            '`' => '\'',
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect();
    if text_len(&cleaned) <= cap {
        return cleaned;
    }
    let mut cut = truncate(&cleaned, cap.saturating_sub(1));
    cut.push('…');
    cut
}

fn code(value: &str, cap: usize) -> String {
    format!("`{}`", inert(value, cap))
}

/// Render one row as a single line: identity, action, outcome, target, acting
/// staff member, UTC time and the short reason. Compact on purpose: a typical
/// shift page lists a dozen rows.
#[must_use]
pub fn render_row(row: &RecentRow) -> String {
    let outcome = RecentOutcome::classify(&row.outcome);
    let target = match (&row.target_id, &row.channel_id) {
        (Some(target), _) => format!("target {}", inert(target, ID_CAP)),
        (None, Some(channel)) => format!("channel {}", inert(channel, ID_CAP)),
        (None, None) => "target -".to_owned(),
    };
    format!(
        "{} {} {} ({}) {} by {} {} {}",
        inert(&row.request_id, REQUEST_CAP),
        inert(&row.action, ACTION_CAP),
        outcome.word(),
        inert(&row.outcome, OUTCOME_CAP),
        target,
        inert(&row.actor_id, ID_CAP),
        inert(&row.created_at, 20),
        code(&row.reason, REASON_CAP),
    )
}

/// Pack rendered rows into pages, each whole rows within the Discord budget.
/// A page that can take every remaining row under the shorter last-page
/// footer does, so a ledger that fits one message is never split.
fn pack(lines: &[String]) -> Vec<Vec<&str>> {
    let base = CONTENT_LIMIT - text_len(HEADER);
    let (budget, last_budget) = (base - FOOTER_RESERVE, base - LAST_FOOTER_RESERVE);
    let mut pages: Vec<Vec<&str>> = Vec::new();
    let mut used = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let cost = text_len(line) + 1;
        let remaining: usize = lines[index..].iter().map(|l| text_len(l) + 1).sum();
        match pages.last_mut() {
            Some(page) if used + remaining <= last_budget || used + cost <= budget => {
                page.push(line);
                used += cost;
            }
            _ => {
                pages.push(vec![line]);
                used = cost;
            }
        }
    }
    pages
}

/// Render one 1-based page of the newest rows (already ordered newest first).
#[must_use]
pub fn render_page(rows: &[RecentRow], page: i64) -> RecentView {
    if rows.is_empty() {
        return if page == 1 {
            RecentView::Empty
        } else {
            RecentView::OutOfRange
        };
    }
    let lines: Vec<String> = rows.iter().take(RECENT_WINDOW).map(render_row).collect();
    let pages = pack(&lines);
    let total = pages.len();
    let Some(selected) = usize::try_from(page)
        .ok()
        .filter(|page| (1..=total).contains(page))
    else {
        return RecentView::OutOfRange;
    };
    let mut out = String::from(HEADER);
    for line in &pages[selected - 1] {
        out.push('\n');
        out.push_str(line);
    }
    out.push_str(&format!("\nPage {selected} of {total}"));
    if selected < total {
        out.push_str(&format!("\nNext: /moderation-recent page:{}", selected + 1));
    }
    RecentView::Page(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(index: usize) -> RecentRow {
        RecentRow {
            request_id: format!("req-{index:03}"),
            actor_id: "88".to_owned(),
            action: "moderation.warn".to_owned(),
            target_id: Some("3333".to_owned()),
            channel_id: None,
            reason: "x".repeat(300),
            outcome: "warned".to_owned(),
            created_at: "2026-10-12T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn outcomes_are_never_relabelled() {
        for word in APPLIED_OUTCOMES {
            assert_eq!(RecentOutcome::classify(word), RecentOutcome::Applied);
        }
        for word in ["denied", "refused"] {
            assert_eq!(RecentOutcome::classify(word), RecentOutcome::Denied);
        }
        for word in ["failed", "in_progress", "unban_failed", "", "Banned", "?"] {
            assert_eq!(RecentOutcome::classify(word), RecentOutcome::Uncertain);
        }
    }

    #[test]
    fn worst_case_rows_still_fit_the_page_cap() {
        let worst = RecentRow {
            request_id: "r".repeat(200),
            actor_id: "9".repeat(200),
            action: "a".repeat(200),
            target_id: Some("8".repeat(200)),
            channel_id: Some("7".repeat(200)),
            reason: "😀".repeat(200),
            outcome: "o".repeat(200),
            created_at: "2026-10-12T00:00:00Z".to_owned(),
        };
        let rows: Vec<RecentRow> = (0..RECENT_WINDOW).map(|_| worst.clone()).collect();
        let lines: Vec<String> = rows.iter().map(render_row).collect();
        let pages = pack(&lines);
        assert!(
            i64::try_from(pages.len()).is_ok_and(|pages| pages <= RECENT_MAX_PAGE),
            "a full window of maximal rows needs {} pages",
            pages.len()
        );
        for number in 1..=pages.len() {
            let view = render_page(&rows, number as i64);
            assert!(matches!(view, RecentView::Page(_)), "page {number} renders");
            if let RecentView::Page(text) = view {
                assert!(text_len(&text) <= CONTENT_LIMIT, "page {number} fits");
            }
        }
    }

    #[test]
    fn pages_cover_every_row_once_and_stay_in_budget() {
        let rows: Vec<RecentRow> = (0..RECENT_WINDOW).map(row).collect();
        let mut seen = 0usize;
        let mut number = 1;
        loop {
            match render_page(&rows, number) {
                RecentView::Page(text) => {
                    assert!(text_len(&text) <= CONTENT_LIMIT);
                    seen += text.lines().filter(|line| line.contains("req-")).count();
                    number += 1;
                }
                RecentView::OutOfRange => break,
                RecentView::Empty => {
                    assert!(seen > 0, "a non-empty ledger is never Empty");
                    break;
                }
            }
        }
        assert_eq!(seen, RECENT_WINDOW);
    }

    #[test]
    fn empty_and_out_of_range_are_distinct() {
        assert_eq!(render_page(&[], 1), RecentView::Empty);
        assert_eq!(render_page(&[], 2), RecentView::OutOfRange);
        let rows = [row(1)];
        assert_eq!(render_page(&rows, 0), RecentView::OutOfRange);
        assert_eq!(render_page(&rows, -3), RecentView::OutOfRange);
        assert_eq!(render_page(&rows, 2), RecentView::OutOfRange);
    }

    #[test]
    fn reasons_render_inside_a_code_span() {
        let mut hostile = row(1);
        hostile.reason = "`@everyone` <@1>\nnext".to_owned();
        let line = render_row(&hostile);
        assert!(line.ends_with("`'@everyone' <@1> next`"), "{line}");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn channel_rows_name_their_channel_and_missing_targets_say_so() {
        let mut channel = row(1);
        channel.target_id = None;
        channel.channel_id = Some("555".to_owned());
        assert!(render_row(&channel).contains("channel 555"));
        channel.channel_id = None;
        assert!(render_row(&channel).contains("target -"));
    }
}
