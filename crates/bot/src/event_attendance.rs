//! Pure planning for mapped event attendance windows. No runtime caller.

use std::ops::Range;

use two_bot_core::Snowflake;

const ATTENDANCE_GRACE_MS: i64 = 15 * 60 * 1_000;

/// One mapped event window, in epoch milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttendanceWindowMs {
    pub event_key: String,
    pub starts_ms: i64,
    pub ends_ms: i64,
}

impl AttendanceWindowMs {
    /// The half-open window with 15 minutes of grace on either side.
    pub fn bounds(&self) -> Range<i64> {
        self.starts_ms.saturating_sub(ATTENDANCE_GRACE_MS)
            ..self.ends_ms.saturating_add(ATTENDANCE_GRACE_MS)
    }
}

/// A previously opened session. A missing channel cannot prove continuity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRow {
    pub id: i64,
    pub channel_id: Option<Snowflake>,
    pub joined_ms: i64,
    pub event_keys: Vec<String>,
}

/// One row to finalize, retaining existing tags even when their windows are omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosePlan {
    pub id: i64,
    pub left_ms: i64,
    pub tag_union: Vec<String>,
}

/// A new session, tagged only with mapped windows containing its join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPlan {
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TransitionPlan {
    pub closes: Vec<ClosePlan>,
    pub open: Option<OpenPlan>,
}

fn matching_keys<'a>(
    windows: &'a [AttendanceWindowMs],
    matches: impl Fn(Range<i64>) -> bool + 'a,
) -> impl Iterator<Item = String> + 'a {
    windows
        .iter()
        .filter(move |window| matches(window.bounds()))
        .map(|window| window.event_key.clone())
}

fn plan_close(
    windows: &[AttendanceWindowMs],
    row: &OpenRow,
    previous: Option<Snowflake>,
    now_ms: i64,
) -> ClosePlan {
    // A shared event key does not prove continuity after a missed leave.
    let continuous = row.channel_id.is_some() && row.channel_id == previous;
    let mut tag_union = row.event_keys.clone();
    let left_ms = if continuous {
        // A closed session must intersect the window for positive time. Unlike
        // a join, merely touching the opening boundary does not earn a tag.
        tag_union.extend(matching_keys(windows, |bounds| {
            bounds.start.max(row.joined_ms) < bounds.end.min(now_ms)
        }));
        now_ms
    } else {
        row.joined_ms
    };
    tag_union.sort_unstable();
    tag_union.dedup();
    ClosePlan {
        id: row.id,
        left_ms,
        tag_union,
    }
}

/// Plan one transition from mapped windows and the member's open rows.
/// Same-channel frames do nothing. A move or leave closes known rows even
/// after grace, but only the observed previous channel can extend a session.
/// The plan is not capture authorization.
pub fn plan_transition(
    windows: &[AttendanceWindowMs],
    open_rows: &[OpenRow],
    previous: Option<Snowflake>,
    current: Option<Snowflake>,
    now_ms: i64,
) -> TransitionPlan {
    if previous == current {
        return TransitionPlan::default();
    }
    let closes = open_rows
        .iter()
        .map(|row| plan_close(windows, row, previous, now_ms))
        .collect();
    let open = current.and_then(|_| {
        let mut tags: Vec<_> = matching_keys(windows, |bounds| bounds.contains(&now_ms)).collect();
        tags.sort_unstable();
        tags.dedup();
        (!tags.is_empty()).then_some(OpenPlan { tags })
    });
    TransitionPlan { closes, open }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    fn window(key: &str, starts_ms: i64, ends_ms: i64) -> AttendanceWindowMs {
        AttendanceWindowMs {
            event_key: key.to_owned(),
            starts_ms,
            ends_ms,
        }
    }

    fn row(channel_id: Option<Snowflake>, joined_ms: i64, key: &str) -> OpenRow {
        OpenRow {
            id: 1,
            channel_id,
            joined_ms,
            event_keys: vec![key.to_owned()],
        }
    }

    #[test]
    fn join_requires_a_mapped_window_and_uses_half_open_grace() {
        let windows = [window("show", 0, 60 * MIN)];
        for now_ms in [-15 * MIN - 1, 75 * MIN, 75 * MIN + 1] {
            assert_eq!(
                plan_transition(&windows, &[], None, Some(7), now_ms),
                TransitionPlan::default()
            );
        }
        for now_ms in [-15 * MIN, 75 * MIN - 1] {
            let plan = plan_transition(&windows, &[], None, Some(7), now_ms);
            assert!(plan.closes.is_empty());
            assert_eq!(plan.open.unwrap().tags, ["show"]);
        }
        assert_eq!(
            plan_transition(&[], &[], None, Some(7), 30 * MIN),
            TransitionPlan::default()
        );
    }

    #[test]
    fn overlapping_windows_tag_both_join_and_continuous_close_once() {
        let windows = [
            window("late", 30 * MIN, 120 * MIN),
            window("early", 0, 60 * MIN),
            window("late", 30 * MIN, 120 * MIN),
        ];
        let rows = [row(Some(7), 10 * MIN, "early")];
        let plan = plan_transition(&windows, &rows, Some(7), Some(9), 45 * MIN);
        assert_eq!(
            plan.closes,
            [ClosePlan {
                id: 1,
                left_ms: 45 * MIN,
                tag_union: vec!["early".to_owned(), "late".to_owned()],
            }]
        );
        assert_eq!(plan.open.unwrap().tags, ["early", "late"]);
    }

    #[test]
    fn close_keeps_omitted_tag_but_does_not_add_touching_or_disjoint_keys() {
        let windows = [
            window("ends-at-join", -60 * MIN, -15 * MIN),
            window("starts-at-leave", 75 * MIN, 100 * MIN),
            window("later", 90 * MIN, 120 * MIN),
        ];
        let rows = [row(Some(7), 0, "omitted")];
        let plan = plan_transition(&windows, &rows, Some(7), None, 60 * MIN);
        assert_eq!(
            plan,
            TransitionPlan {
                closes: vec![ClosePlan {
                    id: 1,
                    left_ms: 60 * MIN,
                    tag_union: vec!["omitted".to_owned()],
                }],
                open: None,
            }
        );
    }

    #[test]
    fn move_after_grace_closes_without_opening() {
        let windows = [window("show", 0, 60 * MIN)];
        let rows = [row(Some(7), 10 * MIN, "show")];
        let plan = plan_transition(&windows, &rows, Some(7), Some(9), 75 * MIN);
        assert_eq!(plan.closes[0].left_ms, 75 * MIN);
        assert_eq!(plan.closes[0].tag_union, ["show"]);
        assert_eq!(plan.open, None);
    }

    #[test]
    fn grace_bounds_saturate() {
        assert_eq!(
            window("show", i64::MIN, i64::MAX).bounds(),
            i64::MIN..i64::MAX
        );
    }

    #[test]
    fn unproven_continuity_never_widens_a_close() {
        let windows = [window("later", 30 * MIN, 120 * MIN)];
        for (stored, previous) in [(Some(7), None), (Some(7), Some(9)), (None, None)] {
            let rows = [row(stored, 0, "original")];
            let plan = plan_transition(&windows, &rows, previous, Some(7), 45 * MIN);
            assert_eq!(plan.closes[0].left_ms, 0);
            assert_eq!(plan.closes[0].tag_union, ["original"]);
            assert_eq!(plan.open.unwrap().tags, ["later"]);
        }
    }

    #[test]
    fn same_channel_does_not_close_or_open() {
        let windows = [window("show", 0, 60 * MIN)];
        let rows = [row(Some(7), 0, "show")];
        assert_eq!(
            plan_transition(&windows, &rows, Some(7), Some(7), 45 * MIN),
            TransitionPlan::default()
        );
    }
}
