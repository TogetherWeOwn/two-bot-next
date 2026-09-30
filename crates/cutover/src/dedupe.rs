//! Cross-logger join/leave de-duplication.
//!
//! Ports `src/backfill/dedupe.ts`: TWO ran several logging bots at once, each
//! writing to its own channel, so one join arrived twice seconds apart from
//! different channels. The rule is "same person, same event, different
//! logger, minutes apart" — a repeat from the SAME logger is always kept,
//! because that is one bot telling us something happened twice. Clusters are
//! anchored, not chained, and the kept row is always the earliest.

/// Generous next to the 3.5s observed median, still far short of any genuine
/// rejoin (legacy `DEFAULT_TOLERANCE_MS`).
pub const DEFAULT_TOLERANCE_MS: i64 = 15 * 60 * 1000;

/// One event the dedupe can judge. `member_id` is `None` for anonymous events
/// (e.g. invite clicks), which pass through untouched — no "same person" to
/// merge against. `occurred_at` is ISO-8601 UTC, as stored in `events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupableEvent {
    pub event_type: String,
    pub member_id: Option<String>,
    pub occurred_at: String,
    pub source: String,
}

/// Outcome: indices into the input slice that survive, in time order, plus
/// how many copies collapsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapseResult {
    pub kept: Vec<usize>,
    pub collapsed: usize,
}

/// Parse an ISO-8601 timestamp to epoch millis. Real rows are always
/// well-formed; an unparsable row keeps its input position order against
/// unparsable peers via the index tiebreak below.
fn parse_ms(s: &str) -> Option<i64> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// Keep the earliest record of each real event; drop the other loggers'
/// copies. Input order does not matter; kept indices come back in time order
/// (ties break by input position, so the result is deterministic).
pub fn collapse_cross_source_duplicates(
    events: &[DedupableEvent],
    tolerance_ms: i64,
) -> CollapseResult {
    use std::collections::HashMap;

    let mut groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, e) in events.iter().enumerate() {
        groups
            .entry((
                e.member_id.clone().unwrap_or_default(),
                e.event_type.clone(),
            ))
            .or_default()
            .push(i);
    }

    let mut kept: Vec<usize> = Vec::new();
    let mut collapsed = 0usize;

    let mut keys: Vec<_> = groups.keys().cloned().collect();
    keys.sort();
    for key in keys {
        let group = &groups[&key];
        if key.0.is_empty() {
            // Anonymous events: no "same person" to merge against.
            kept.extend(group.iter().copied());
            continue;
        }
        let mut ordered = group.clone();
        ordered.sort_by(|&a, &b| {
            parse_ms(&events[a].occurred_at)
                .cmp(&parse_ms(&events[b].occurred_at))
                .then_with(|| events[a].occurred_at.cmp(&events[b].occurred_at))
                .then_with(|| a.cmp(&b))
        });

        // Anchored clustering: every candidate is measured against the kept
        // event, never against the previous duplicate. Chaining would let a
        // long run of near-misses swallow an event well outside the tolerance.
        let mut anchor_at: Option<i64> = None;
        let mut anchor_sources: Vec<&str> = Vec::new();
        for &i in &ordered {
            let e = &events[i];
            let at = parse_ms(&e.occurred_at);
            let is_copy = match (anchor_at, at) {
                (Some(anchor), Some(now)) => {
                    now - anchor <= tolerance_ms && !anchor_sources.iter().any(|s| *s == e.source)
                }
                _ => false,
            };
            if is_copy {
                anchor_sources.push(e.source.as_str());
                collapsed += 1;
                continue;
            }
            kept.push(i);
            anchor_at = at;
            anchor_sources = vec![e.source.as_str()];
        }
    }

    kept.sort_by(|&a, &b| {
        events[a]
            .occurred_at
            .cmp(&events[b].occurred_at)
            .then_with(|| a.cmp(&b))
    });
    CollapseResult { kept, collapsed }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(member: &str, at: &str, source: &str) -> DedupableEvent {
        ev_typed(member, at, source, "member_join")
    }

    fn ev_typed(member: &str, at: &str, source: &str, event_type: &str) -> DedupableEvent {
        DedupableEvent {
            event_type: event_type.to_owned(),
            member_id: Some(member.to_owned()),
            occurred_at: at.to_owned(),
            source: source.to_owned(),
        }
    }

    fn kept_at(events: &[DedupableEvent], result: &CollapseResult) -> Vec<String> {
        result
            .kept
            .iter()
            .map(|&i| events[i].occurred_at.clone())
            .collect()
    }

    #[test]
    fn two_loggers_collapse_to_earliest() {
        let events = vec![
            ev(
                "1015384495525986346",
                "2025-07-06T21:23:10.604Z",
                "backfill:log:member-join",
            ),
            ev(
                "1015384495525986346",
                "2025-07-06T21:20:57.662Z",
                "backfill:log:join-leave-log",
            ),
        ];
        let r = collapse_cross_source_duplicates(&events, DEFAULT_TOLERANCE_MS);
        assert_eq!(r.collapsed, 1);
        assert_eq!(kept_at(&events, &r), vec!["2025-07-06T21:20:57.662Z"]);
    }

    #[test]
    fn genuine_rejoin_months_later_is_kept() {
        let events = vec![
            ev("42", "2024-01-01T10:00:00.000Z", "backfill:log:member-join"),
            ev("42", "2024-06-01T10:00:00.000Z", "backfill:log:member-join"),
        ];
        let r = collapse_cross_source_duplicates(&events, DEFAULT_TOLERANCE_MS);
        assert_eq!((r.collapsed, r.kept.len()), (0, 2));
    }

    #[test]
    fn same_logger_twice_is_always_kept() {
        let events = vec![
            ev("42", "2024-01-01T10:00:00.000Z", "backfill:log:member-join"),
            ev("42", "2024-01-01T10:00:04.000Z", "backfill:log:member-join"),
        ];
        let r = collapse_cross_source_duplicates(&events, DEFAULT_TOLERANCE_MS);
        assert_eq!((r.collapsed, r.kept.len()), (0, 2));
    }

    #[test]
    fn different_people_never_merge() {
        let events = vec![
            ev("1", "2025-07-06T21:20:57.000Z", "a"),
            ev("2", "2025-07-06T21:20:57.000Z", "b"),
        ];
        let r = collapse_cross_source_duplicates(&events, DEFAULT_TOLERANCE_MS);
        assert_eq!((r.collapsed, r.kept.len()), (0, 2));
    }

    #[test]
    fn join_and_leave_at_same_instant_are_different_events() {
        let events = vec![
            ev_typed("1", "2025-07-06T21:20:57.000Z", "a", "member_join"),
            ev_typed("1", "2025-07-06T21:20:57.000Z", "b", "member_leave"),
        ];
        let r = collapse_cross_source_duplicates(&events, DEFAULT_TOLERANCE_MS);
        assert_eq!((r.collapsed, r.kept.len()), (0, 2));
    }

    #[test]
    fn clusters_are_anchored_not_chained() {
        // 0, +10min, +20min from three loggers: the third is 20min from the
        // kept event, so it stands even though it is 10min from the second.
        let events = vec![
            ev("1", "2025-01-01T00:00:00.000Z", "a"),
            ev("1", "2025-01-01T00:10:00.000Z", "b"),
            ev("1", "2025-01-01T00:20:00.000Z", "c"),
        ];
        let r = collapse_cross_source_duplicates(&events, DEFAULT_TOLERANCE_MS);
        assert_eq!(
            kept_at(&events, &r),
            vec!["2025-01-01T00:00:00.000Z", "2025-01-01T00:20:00.000Z"]
        );
    }

    #[test]
    fn three_loggers_collapse_to_one() {
        let events = vec![
            ev("1", "2025-01-01T00:00:01.000Z", "a"),
            ev("1", "2025-01-01T00:00:03.000Z", "b"),
            ev("1", "2025-01-01T00:00:06.000Z", "c"),
        ];
        let r = collapse_cross_source_duplicates(&events, DEFAULT_TOLERANCE_MS);
        assert_eq!((r.collapsed, r.kept.len()), (2, 1));
    }

    #[test]
    fn anonymous_events_pass_through() {
        let events = vec![
            DedupableEvent {
                event_type: "invite_click".to_owned(),
                member_id: None,
                occurred_at: "2025-01-01T00:00:00.000Z".to_owned(),
                source: "a".to_owned(),
            },
            DedupableEvent {
                event_type: "invite_click".to_owned(),
                member_id: None,
                occurred_at: "2025-01-01T00:00:02.000Z".to_owned(),
                source: "b".to_owned(),
            },
        ];
        let r = collapse_cross_source_duplicates(&events, DEFAULT_TOLERANCE_MS);
        assert_eq!((r.collapsed, r.kept.len()), (0, 2));
    }

    #[test]
    fn output_is_time_ordered_regardless_of_input() {
        let events = vec![
            ev("2", "2025-03-01T00:00:00.000Z", "a"),
            ev("1", "2025-01-01T00:00:00.000Z", "a"),
            ev("3", "2025-02-01T00:00:00.000Z", "a"),
        ];
        let r = collapse_cross_source_duplicates(&events, DEFAULT_TOLERANCE_MS);
        let members: Vec<_> = r
            .kept
            .iter()
            .map(|&i| events[i].member_id.clone().unwrap())
            .collect();
        assert_eq!(members, vec!["1", "3", "2"]);
    }

    #[test]
    fn empty_input_is_not_a_crash() {
        let r = collapse_cross_source_duplicates(&[], DEFAULT_TOLERANCE_MS);
        assert_eq!((r.collapsed, r.kept.len()), (0, 0));
    }
}
