//! Read-only member-leave backfill gap analysis (port of two-bot
//! `src/analytics/memberLeaveGap.ts` at `bffccf3`, obligation TOG-11152).
//!
//! The question: members with a `member_join` row and no `member_leave` row
//! who are no longer in the guild. Every one of them makes retention read
//! higher than it is — the denominator keeps them as "still here".
//!
//! Read-only by design, same split as [`crate::voice_reconcile`]: the caller
//! reads the two event feeds (SELECT only) plus the roster and this module
//! decides what they mean. The pure classifier takes rows, so the taxonomy
//! is pinned without a database. The proposed fills below are a proposal
//! for a future card, flagged row by row — written nowhere.
//!
//! Timestamps are compared by instant ([`parse_iso_millis`], never string
//! ordering — the TOG-5684/#411 fix) but retained in their source spelling
//! in reports and proposed fills. Unknown (unparseable) timestamps are
//! retained and counted, never synthesized.

use std::collections::{HashMap, HashSet};

use crate::community_snapshots::{window_bounds, RaidAnomaly};
use crate::funnel::parse_iso_millis;
use crate::handlers::StoredRow;
use crate::{EventType, Snowflake};

/// One `member_join` row. The source tells log-derived apart from
/// roster-derived; classification does not read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapJoin {
    pub guild_id: Snowflake,
    pub member_id: Option<Snowflake>,
    /// ISO-8601 UTC of the join, retained verbatim.
    pub occurred_at: String,
    pub source: String,
}

/// One `member_leave` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapLeave {
    pub guild_id: Snowflake,
    pub member_id: Option<Snowflake>,
    /// ISO-8601 UTC of the leave, retained verbatim.
    pub occurred_at: String,
}

/// One entry of the roster: proof the member is still here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GapRosterMember {
    pub guild_id: Snowflake,
    pub member_id: Option<Snowflake>,
}

/// Where a gap member's departure falls relative to what can still be read.
/// Wire strings match legacy exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapKind {
    /// Last join predates the oldest log message scanned: the leave instant
    /// is unknowable.
    PreCoverage,
    /// Last join is inside scanned history but no leave row: a logger miss
    /// worth re-scanning.
    LogMiss,
    /// Joined inside a raid window and never came back: mass-join residue,
    /// not organic churn.
    RaidResidue,
    /// Joined twice or more with no leave between: necessarily left at least
    /// once.
    RejoinGap,
}

impl GapKind {
    /// Legacy wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreCoverage => "pre-coverage",
            Self::LogMiss => "log-miss",
            Self::RaidResidue => "raid-residue",
            Self::RejoinGap => "rejoin-gap",
        }
    }
}

/// One row a future fill card would write. Proposed here, written nowhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedFill {
    pub occurred_at: String,
    /// Always earliest-possible: the member was provably present at this
    /// join instant and gone sometime after. Stamping a later instant would
    /// share its `occurred_at` with the next fill, and `member_leave`
    /// idempotency keys on `occurred_at`, so the two would dedupe into one
    /// row and the inter-join departure would vanish. Same doctrine as
    /// backfilled `gate_cleared` rows, which say THAT and never WHEN.
    pub bound: FillBound,
    pub note: String,
}

/// The only fill bound the classifier ever proposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FillBound {
    #[default]
    EarliestPossible,
}

impl FillBound {
    /// Legacy wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EarliestPossible => "earliest-possible",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveGap {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub kind: GapKind,
    pub last_join_at: String,
    pub joins_seen: usize,
    /// The human sentence: what happened and what (if anything) to do.
    pub detail: String,
    pub fills: Vec<ProposedFill>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClassifyResult {
    /// Join-without-leave members confirmed gone, one entry each with fills.
    pub gaps: Vec<LeaveGap>,
    /// Join-without-leave members still on the roster: correct, not a gap.
    pub present: usize,
    /// Off-roster members with at least one leave row: already resolved.
    pub resolved: usize,
    /// Rows with an unparseable timestamp or no member. Counted, never paired.
    pub skipped: usize,
}

/// True when the instant falls inside a raid window (half-open `[from, to)`,
/// compared by instant).
fn in_raid_window(at: &str, windows: &[RaidAnomaly]) -> bool {
    let Some(ms) = parse_iso_millis(at) else {
        return false;
    };
    windows.iter().any(|w| {
        let Some((from, to)) = window_bounds(w.start, w.end) else {
            return false;
        };
        match (parse_iso_millis(&from), parse_iso_millis(&to)) {
            (Some(f), Some(t)) => ms >= f && ms < t,
            _ => false,
        }
    })
}

/// The raw spelling when it parses to an instant, else `None`: empty and
/// garbage timestamps are unusable, never guessed.
fn usable_time(raw: &str) -> Option<&str> {
    if raw.is_empty() || parse_iso_millis(raw).is_none() {
        return None;
    }
    Some(raw)
}

/// One fill per join instant, each stamped AT the join it bounds: the member
/// was provably present then and gone sometime after. A rejoin additionally
/// proves the inter-join departure by its own instant (the note says so), but
/// the fill still stamps the earlier join.
fn fills_for(joins: &[String], kind: GapKind) -> Vec<ProposedFill> {
    let mut fills = Vec::with_capacity(joins.len());
    for pair in joins.windows(2) {
        fills.push(ProposedFill {
            occurred_at: pair[0].clone(),
            bound: FillBound::EarliestPossible,
            note: format!(
                "provably present at {}, gone sometime after — and necessarily before the rejoin at {}",
                pair[0], pair[1],
            ),
        });
    }
    let Some(last) = joins.last() else {
        return fills;
    };
    let kind_note = match kind {
        GapKind::RaidResidue => {
            "raid-window join: confirm the cleanup before counting this as churn".to_owned()
        }
        GapKind::PreCoverage => {
            "leave predates scanned log history: the instant is unknowable".to_owned()
        }
        GapKind::LogMiss => "re-scan the raw logs for this member id before filling".to_owned(),
        GapKind::RejoinGap => "final departure after the last join".to_owned(),
    };
    fills.push(ProposedFill {
        occurred_at: last.clone(),
        bound: FillBound::EarliestPossible,
        note: kind_note,
    });
    fills
}

/// Partition every joined member into present / resolved / gap / skipped.
///
/// `log_floor` is the oldest log timestamp the backfill actually read (its
/// scanned-back-to). `None` means unknown, in which case nothing is called
/// pre-coverage — without a floor that label would be a guess, so those gaps
/// read as log-miss with the floor-unknown note.
#[must_use]
pub fn classify_leave_gaps(
    joins: &[GapJoin],
    leaves: &[GapLeave],
    roster: &[GapRosterMember],
    log_floor: Option<&str>,
    raid_windows: &[RaidAnomaly],
) -> ClassifyResult {
    let mut result = ClassifyResult::default();

    let mut joins_by_member: HashMap<(Snowflake, Snowflake), (Snowflake, Vec<String>)> =
        HashMap::new();
    for j in joins {
        let (Some(member_id), Some(at)) = (j.member_id, usable_time(&j.occurred_at)) else {
            result.skipped += 1;
            continue;
        };
        joins_by_member
            .entry((j.guild_id, member_id))
            .or_insert_with(|| (j.guild_id, Vec::new()))
            .1
            .push(at.to_owned());
    }

    let mut left_members: HashSet<(Snowflake, Snowflake)> = HashSet::new();
    for l in leaves {
        match (l.member_id, usable_time(&l.occurred_at)) {
            (Some(member_id), Some(_)) => {
                left_members.insert((l.guild_id, member_id));
            }
            _ => result.skipped += 1,
        }
    }

    let mut on_roster: HashSet<(Snowflake, Snowflake)> = HashSet::new();
    for r in roster {
        match r.member_id {
            Some(member_id) => {
                on_roster.insert((r.guild_id, member_id));
            }
            None => result.skipped += 1,
        }
    }

    for ((guild_id, member_id), (_, at)) in joins_by_member {
        let key = (guild_id, member_id);
        if left_members.contains(&key) {
            // At least one leave row: the departure is recorded, whatever
            // else is missing.
            result.resolved += 1;
            continue;
        }
        if on_roster.contains(&key) {
            // Still here with no leave row is the correct state, not a gap.
            result.present += 1;
            continue;
        }
        // Compare instants, but retain the source spelling in reports and
        // proposed fills.
        let mut times = at;
        times.sort_by_key(|t| parse_iso_millis(t).unwrap_or(i64::MIN));
        let last_join = times.last().expect("member has a join").clone();
        let (kind, detail) = if times.len() > 1 {
            (
                GapKind::RejoinGap,
                format!(
                    "{} joins and no leave: left at least once between them, and left again after {}. Fill one leave per inter-join gap plus the final departure.",
                    times.len(),
                    last_join,
                ),
            )
        } else if in_raid_window(&last_join, raid_windows) {
            (
                GapKind::RaidResidue,
                format!(
                    "joined inside a raid window ({}) and never recorded leaving: likely removed in a cleanup the logs do not show. Confirm against the cleanup before counting this as churn — never auto-fill as organic.",
                    last_join,
                ),
            )
        } else if matches!(log_floor.and_then(parse_iso_millis), Some(floor) if parse_iso_millis(&last_join).is_some_and(|t| t < floor))
        {
            let floor = log_floor.expect("floor parsed");
            (
                GapKind::PreCoverage,
                format!(
                    "last join {} predates the oldest scanned log message ({}): the departure left no record we can still read. Fill establishes THAT they left, never WHEN.",
                    last_join, floor,
                ),
            )
        } else {
            (
                GapKind::LogMiss,
                match log_floor {
                    Some(floor) => format!(
                        "last join {} is inside scanned history (floor {}) but no leave row: the logger missed it (format drift, missing footer id, mixed-feed skip, or a bot-down window). Re-scan the raw logs for this id first.",
                        last_join, floor,
                    ),
                    None => format!(
                        "last join {} is inside scanned history (log floor unknown) but no leave row: the logger missed it (format drift, missing footer id, mixed-feed skip, or a bot-down window). Re-scan the raw logs for this id first.",
                        last_join,
                    ),
                },
            )
        };
        result.gaps.push(LeaveGap {
            guild_id,
            member_id,
            kind,
            last_join_at: last_join.clone(),
            joins_seen: times.len(),
            detail,
            fills: fills_for(&times, kind),
        });
    }

    result.gaps.sort_by(|a, b| {
        parse_iso_millis(&a.last_join_at)
            .cmp(&parse_iso_millis(&b.last_join_at))
            .then(a.member_id.cmp(&b.member_id))
    });
    result
}

/// Project the two event feeds out of persisted rows. Pure and
/// non-destructive: timestamp spellings pass through unmodified. Memberless
/// rows are filtered out here and counted in `skipped`, never paired (the
/// classifier also skips memberless rows defensively, so callers must not
/// sum both counters); unknown (unparseable) timestamps pass through to
/// [`classify_leave_gaps`], which counts them.
#[must_use]
pub fn leave_gap_feeds_from_rows(rows: &[StoredRow]) -> (Vec<GapJoin>, Vec<GapLeave>, usize) {
    let mut joins = Vec::new();
    let mut leaves = Vec::new();
    let mut skipped = 0_usize;
    for row in rows {
        match row.event_type {
            EventType::MemberJoin => {
                if row.member_id.is_none() {
                    skipped += 1;
                    continue;
                }
                joins.push(GapJoin {
                    guild_id: row.guild_id,
                    member_id: row.member_id,
                    occurred_at: row.occurred_at.clone(),
                    source: row.source.clone(),
                });
            }
            EventType::MemberLeave => {
                if row.member_id.is_none() {
                    skipped += 1;
                    continue;
                }
                leaves.push(GapLeave {
                    guild_id: row.guild_id,
                    member_id: row.member_id,
                    occurred_at: row.occurred_at.clone(),
                });
            }
            _ => {}
        }
    }
    (joins, leaves, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(member: Option<Snowflake>, at: &str) -> GapJoin {
        GapJoin {
            guild_id: 1,
            member_id: member,
            occurred_at: at.to_owned(),
            source: "gateway".to_owned(),
        }
    }

    fn leave(member: Option<Snowflake>, at: &str) -> GapLeave {
        GapLeave {
            guild_id: 1,
            member_id: member,
            occurred_at: at.to_owned(),
        }
    }

    fn on_roster(member: Option<Snowflake>) -> GapRosterMember {
        GapRosterMember {
            guild_id: 1,
            member_id: member,
        }
    }

    #[test]
    fn malformed_rows_are_skipped_never_paired() {
        let joins = vec![
            join(None, "2025-06-01T10:00:00.000Z"),
            join(Some(9), "not-a-timestamp"),
        ];
        let leaves = vec![leave(None, "2025-05-11T10:00:00.000Z")];
        let roster = vec![on_roster(None)];
        let r = classify_leave_gaps(&joins, &leaves, &roster, None, &[]);
        assert_eq!(r.skipped, 4);
        assert!(r.gaps.is_empty());
        assert_eq!((r.present, r.resolved), (0, 0));
    }

    #[test]
    fn present_and_resolved_are_not_gaps() {
        let joins = vec![
            join(Some(1), "2025-06-01T10:00:00.000Z"),
            join(Some(2), "2025-05-01T10:00:00.000Z"),
        ];
        let leaves = vec![leave(Some(2), "2025-05-10T10:00:00.000Z")];
        let roster = vec![on_roster(Some(1))];
        let r = classify_leave_gaps(&joins, &leaves, &roster, None, &[]);
        assert!(r.gaps.is_empty());
        assert_eq!(r.present, 1);
        assert_eq!(r.resolved, 1);
    }

    #[test]
    fn offset_timestamps_compare_by_instant_not_string() {
        // Same instant, different spellings: the +02:00 join sorts equal to
        // the Z join, and a floor between the two spellings still applies by
        // instant (the #411 fix).
        let joins = vec![join(Some(7), "2025-06-15T12:00:00.000+02:00")];
        let r = classify_leave_gaps(&joins, &[], &[], Some("2025-06-15T10:00:00.000Z"), &[]);
        assert_eq!(r.gaps.len(), 1);
        // Equal instants: not before the floor → log-miss, not pre-coverage.
        assert_eq!(r.gaps[0].kind, GapKind::LogMiss);
        // ...but a floor one millisecond later makes it pre-coverage.
        let r = classify_leave_gaps(&joins, &[], &[], Some("2025-06-15T10:00:00.001Z"), &[]);
        assert_eq!(r.gaps[0].kind, GapKind::PreCoverage);
    }

    #[test]
    fn fills_stamp_the_earlier_join_never_the_later() {
        let fills = fills_for(
            &[
                "2024-05-01T10:00:00.000Z".to_owned(),
                "2024-09-01T10:00:00.000Z".to_owned(),
            ],
            GapKind::RejoinGap,
        );
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0].occurred_at, "2024-05-01T10:00:00.000Z");
        assert_eq!(fills[0].bound, FillBound::EarliestPossible);
        assert_eq!(fills[1].occurred_at, "2024-09-01T10:00:00.000Z");
    }
}
