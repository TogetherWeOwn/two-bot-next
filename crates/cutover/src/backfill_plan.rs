//! History-backfill merge planning.
//!
//! Ports the pure reconciliation half of `scripts/backfill.ts`: collapse the
//! log channels against each other (cross-logger copies), then the member
//! list against the logs (the log is the richer record, so it wins within a
//! 5-minute tolerance — Discord's stamp vs when the logger bot posted).
//!
//! Gate-clearing rule (TOG-76): `pending` is Discord's live answer. Only an
//! explicit `true` is stuck; anything else (false or screening-off absent)
//! is a `gate_cleared` row stamped at the JOIN time with `timestampIsJoinTime`
//! set, because Discord keeps no history of the transition.

use std::collections::HashMap;

use crate::dedupe::{collapse_cross_source_duplicates, DedupableEvent, DEFAULT_TOLERANCE_MS};

/// Member-list vs log tolerance: Discord's stamp vs the logger post
/// (legacy `TOLERANCE_MS = 5 * 60 * 1000`).
pub const MEMBER_LOG_TOLERANCE_MS: i64 = 5 * 60 * 1000;

/// One recovered event awaiting write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedEvent {
    pub member_id: Option<String>,
    pub event_type: String,
    pub occurred_at: String,
    pub source: String,
}

/// Row metadata the backfill writes for a planned event. A recovered
/// `gate_cleared` carries `timestampIsJoinTime` because its `occurred_at` is
/// the JOIN time, a placeholder (Discord keeps no history of the transition);
/// together with the `backfill:` source prefix it keeps the row out of
/// time-to-clear arithmetic via `two_bot_core::is_measurable_gate_clearing`,
/// while counts and conversion still include it (legacy 6928f11).
#[must_use]
pub fn event_metadata_json(event_type: &str) -> &'static str {
    if event_type == "gate_cleared" {
        r#"{"backfill":true,"timestampIsJoinTime":true}"#
    } else {
        r#"{"backfill":true}"#
    }
}

/// One current member as Discord reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedMember {
    pub id: String,
    pub joined_at: Option<String>,
    /// Membership-screening flag: `Some(true)` = still behind the rules
    /// gate; `Some(false)` = through; `None` = screening off, trivially
    /// through (legacy `pending`).
    pub pending: Option<bool>,
    pub is_bot: bool,
}

/// Merge outcome: the ordered write set plus the reconciliation counts the
/// operator report prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillMerge {
    /// Ascending by `occurred_at`: `member_join` clears `left_at` in the
    /// projection, so replay order decides the final state.
    pub ordered: Vec<PlannedEvent>,
    pub log_joins: usize,
    pub log_leaves: usize,
    pub member_list_joins: usize,
    pub gate_clearings: usize,
    pub gate_stuck: usize,
    pub join_dupes_collapsed: usize,
    pub leave_dupes_collapsed: usize,
    pub member_list_superseded_by_log: usize,
}

fn parse_ms(s: &str) -> Option<i64> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// Merge log-derived joins/leaves with the member list into one ordered
/// write set. `bots` output holds bot ids the caller marks via `markBot`;
/// gate rows and counts follow the TOG-76 rule.
pub fn plan_backfill_merge(
    log_joins: &[DedupableEvent],
    log_leaves: &[DedupableEvent],
    members: &[ListedMember],
) -> BackfillMerge {
    let join_dedupe = collapse_cross_source_duplicates(log_joins, DEFAULT_TOLERANCE_MS);
    let leave_dedupe = collapse_cross_source_duplicates(log_leaves, DEFAULT_TOLERANCE_MS);

    let deduped_joins: Vec<&DedupableEvent> =
        join_dedupe.kept.iter().map(|&i| &log_joins[i]).collect();
    let deduped_leaves: Vec<&DedupableEvent> =
        leave_dedupe.kept.iter().map(|&i| &log_leaves[i]).collect();

    let mut log_times: HashMap<&str, Vec<i64>> = HashMap::new();
    for e in &deduped_joins {
        if let (Some(m), Some(ms)) = (e.member_id.as_deref(), parse_ms(&e.occurred_at)) {
            log_times.entry(m).or_default().push(ms);
        }
    }

    let mut member_list_joins: Vec<PlannedEvent> = Vec::new();
    let mut gate_clearings: Vec<PlannedEvent> = Vec::new();
    let mut gate_stuck = 0usize;
    let mut superseded = 0usize;

    let mut sorted_members: Vec<&ListedMember> = members.iter().collect();
    sorted_members.sort_by(|a, b| a.id.cmp(&b.id));
    for m in sorted_members {
        if m.is_bot || m.id.is_empty() {
            continue;
        }
        let Some(joined_at) = m.joined_at.clone() else {
            continue;
        };
        // The log wins within tolerance.
        let dup = match parse_ms(&joined_at) {
            Some(at) => log_times
                .get(m.id.as_str())
                .is_some_and(|ts| ts.iter().any(|t| (t - at).abs() <= MEMBER_LOG_TOLERANCE_MS)),
            None => false,
        };
        if dup {
            superseded += 1;
        } else {
            member_list_joins.push(PlannedEvent {
                member_id: Some(m.id.clone()),
                event_type: "member_join".to_owned(),
                occurred_at: joined_at.clone(),
                source: "backfill:member_list".to_owned(),
            });
        }
        // Every clearing found, not just ones whose join survived: the gate
        // row is keyed on the member, not on which source told us they
        // arrived.
        if m.pending == Some(true) {
            gate_stuck += 1;
        } else {
            gate_clearings.push(PlannedEvent {
                member_id: Some(m.id.clone()),
                event_type: "gate_cleared".to_owned(),
                occurred_at: joined_at,
                source: "backfill:member_list".to_owned(),
            });
        }
    }

    let mut ordered: Vec<PlannedEvent> = Vec::new();
    for e in deduped_joins {
        ordered.push(PlannedEvent {
            member_id: e.member_id.clone(),
            event_type: "member_join".to_owned(),
            occurred_at: e.occurred_at.clone(),
            source: e.source.clone(),
        });
    }
    for e in deduped_leaves {
        ordered.push(PlannedEvent {
            member_id: e.member_id.clone(),
            event_type: "member_leave".to_owned(),
            occurred_at: e.occurred_at.clone(),
            source: e.source.clone(),
        });
    }
    let log_joins_n = ordered
        .iter()
        .filter(|e| e.event_type == "member_join")
        .count();
    let log_leaves_n = ordered
        .iter()
        .filter(|e| e.event_type == "member_leave")
        .count();
    ordered.extend(member_list_joins.iter().cloned());
    let member_list_n = member_list_joins.len();
    let gate_n = gate_clearings.len();
    ordered.extend(gate_clearings);
    ordered.sort_by(|a, b| {
        a.occurred_at
            .cmp(&b.occurred_at)
            .then_with(|| a.event_type.cmp(&b.event_type))
            .then_with(|| a.member_id.cmp(&b.member_id))
    });

    BackfillMerge {
        ordered,
        log_joins: log_joins_n,
        log_leaves: log_leaves_n,
        member_list_joins: member_list_n,
        gate_clearings: gate_n,
        gate_stuck,
        join_dupes_collapsed: join_dedupe.collapsed,
        leave_dupes_collapsed: leave_dedupe.collapsed,
        member_list_superseded_by_log: superseded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(member: &str, at: &str, source: &str, event_type: &str) -> DedupableEvent {
        DedupableEvent {
            event_type: event_type.to_owned(),
            member_id: Some(member.to_owned()),
            occurred_at: at.to_owned(),
            source: source.to_owned(),
        }
    }

    #[test]
    fn log_wins_over_member_list_within_tolerance() {
        let joins = vec![log(
            "1",
            "2025-01-01T10:00:30.000Z",
            "backfill:log:member-join",
            "member_join",
        )];
        let members = vec![ListedMember {
            id: "1".to_owned(),
            joined_at: Some("2025-01-01T10:00:00.000Z".to_owned()),
            pending: Some(false),
            is_bot: false,
        }];
        let m = plan_backfill_merge(&joins, &[], &members);
        assert_eq!(m.member_list_superseded_by_log, 1);
        assert_eq!(m.member_list_joins, 0);
        // The gate row survives: keyed on the member, not the join source.
        assert_eq!(m.gate_clearings, 1);
    }

    #[test]
    fn distant_member_list_join_survives() {
        let joins = vec![log(
            "1",
            "2025-01-01T10:00:00.000Z",
            "backfill:log:a",
            "member_join",
        )];
        let members = vec![ListedMember {
            id: "2".to_owned(),
            joined_at: Some("2025-06-01T10:00:00.000Z".to_owned()),
            pending: None,
            is_bot: false,
        }];
        let m = plan_backfill_merge(&joins, &[], &members);
        assert_eq!((m.log_joins, m.member_list_joins), (1, 1));
    }

    #[test]
    fn pending_true_is_stuck_otherwise_cleared() {
        let members = vec![
            ListedMember {
                id: "1".to_owned(),
                joined_at: Some("2025-01-01T10:00:00.000Z".to_owned()),
                pending: Some(true),
                is_bot: false,
            },
            ListedMember {
                id: "2".to_owned(),
                joined_at: Some("2025-01-02T10:00:00.000Z".to_owned()),
                pending: Some(false),
                is_bot: false,
            },
            ListedMember {
                id: "3".to_owned(),
                joined_at: Some("2025-01-03T10:00:00.000Z".to_owned()),
                pending: None,
                is_bot: false,
            },
        ];
        let m = plan_backfill_merge(&[], &[], &members);
        assert_eq!((m.gate_stuck, m.gate_clearings), (1, 2));
    }

    #[test]
    fn backfilled_gate_clearings_never_feed_time_to_clear_but_still_count() {
        use two_bot_core::is_measurable_gate_clearing;
        let members = vec![
            ListedMember {
                id: "1".to_owned(),
                joined_at: Some("2025-01-01T10:00:00.000Z".to_owned()),
                pending: Some(false),
                is_bot: false,
            },
            ListedMember {
                id: "2".to_owned(),
                joined_at: Some("2025-01-02T10:00:00.000Z".to_owned()),
                pending: None,
                is_bot: false,
            },
        ];
        let m = plan_backfill_merge(&[], &[], &members);
        // Counting is unchanged: every through-the-gate member is a clearing.
        assert_eq!(m.gate_clearings, 2);
        let clearings: Vec<_> = m
            .ordered
            .iter()
            .filter(|e| e.event_type == "gate_cleared")
            .collect();
        assert_eq!(clearings.len(), 2);
        for e in &clearings {
            // The placeholder instant is the member's join time.
            assert!(m.ordered.iter().any(|j| j.event_type == "member_join"
                && j.member_id == e.member_id
                && j.occurred_at == e.occurred_at));
            let metadata: serde_json::Value =
                serde_json::from_str(event_metadata_json(&e.event_type)).expect("valid JSON");
            assert_eq!(metadata["timestampIsJoinTime"], true);
            // Either signal alone disqualifies the row from timing...
            assert!(!is_measurable_gate_clearing(&e.source, None));
            assert!(!is_measurable_gate_clearing("gateway", Some(&metadata)));
            // ...and together, as written, certainly.
            assert!(!is_measurable_gate_clearing(&e.source, Some(&metadata)));
        }
    }

    #[test]
    fn only_gate_clearings_carry_the_join_time_placeholder_flag() {
        for event_type in ["member_join", "member_leave", "first_voice_session"] {
            let metadata: serde_json::Value =
                serde_json::from_str(event_metadata_json(event_type)).expect("valid JSON");
            assert_eq!(metadata["backfill"], true, "{event_type}");
            assert!(
                metadata.get("timestampIsJoinTime").is_none(),
                "{event_type}"
            );
        }
    }

    #[test]
    fn bots_and_missing_joined_at_excluded() {
        let members = vec![
            ListedMember {
                id: "9".to_owned(),
                joined_at: Some("2025-01-01T10:00:00.000Z".to_owned()),
                pending: None,
                is_bot: true,
            },
            ListedMember {
                id: "8".to_owned(),
                joined_at: None,
                pending: None,
                is_bot: false,
            },
        ];
        let m = plan_backfill_merge(&[], &[], &members);
        assert!(m.ordered.is_empty());
    }

    #[test]
    fn output_is_ascending() {
        let joins = vec![
            log(
                "1",
                "2025-03-01T10:00:00.000Z",
                "backfill:log:a",
                "member_join",
            ),
            log(
                "2",
                "2025-01-01T10:00:00.000Z",
                "backfill:log:a",
                "member_join",
            ),
        ];
        let m = plan_backfill_merge(&joins, &[], &[]);
        assert_eq!(m.ordered[0].member_id.as_deref(), Some("2"));
    }
}
