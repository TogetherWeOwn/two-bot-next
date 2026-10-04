//! Member_leave backfill gap analysis (TOG-11152).
//!
//! Port of legacy two-bot `src/analytics/memberLeaveGap.ts` (frozen main
//! `bffccf3`, TOG-8305). The question: members with a `member_join` row and
//! no `member_leave` row who are no longer in the guild. Every one of them
//! makes retention read higher than it is: the denominator keeps them as
//! "still here".
//!
//! The gap taxonomy (`pre-coverage` / `log-miss` / `raid-residue` /
//! `rejoin-gap`) is pure over caller-supplied rows. Timestamps compare as
//! instants ([`parse_iso_millis`], the `Date.parse` analogue), never as
//! strings; malformed timestamps stay skipped, never paired. Read-only by
//! construction: [`fetch_leave_gap_feeds`] only SELECTs, the roster is a
//! bounded GET, and the fill rule in the report is a proposal, executed
//! nowhere. There is no repair path (out of scope on TOG-11152).

use serde::Serialize;
use two_bot_core::funnel::parse_iso_millis;

/// One `member_join` row. The source tells log-derived apart from roster-derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapJoin {
    pub guild_id: String,
    pub member_id: Option<String>,
    pub occurred_at: String,
    pub source: String,
}

/// One `member_leave` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapLeave {
    pub guild_id: String,
    pub member_id: Option<String>,
    pub occurred_at: String,
}

/// One entry of the live Discord roster: proof the member is still here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterMember {
    pub guild_id: String,
    pub member_id: Option<String>,
}

/// The incident raid windows the classifier checks (legacy `ANOMALIES` rows
/// with `kind: raid` affecting `member_join`):
/// - `2025-07-06-raid` (confirmed): 1,015 accounts between 20:31-21:27 UTC.
/// - `2025-09-12-raid` (suspected): 15 accounts 17:42:59-17:43:04 UTC.
/// - `2025-12-15-raid` (suspected): 15 accounts 21:16:49-21:16:56 UTC.
///
/// Cleanup/prune windows never classify a join as raid residue.
/// Day-bounds are whole listed days, half-open `[day 00:00, next day 00:00)`.
///
/// Static day-bounds without heap allocation in const position: computed at
/// first use (legacy day windows `[from, to)` covering whole listed days).
fn raid_bounds() -> [(i64, i64); 3] {
    fn day_start_ms(day: &str) -> Option<i64> {
        parse_iso_millis(&format!("{day}T00:00:00.000Z"))
    }
    let days = ["2025-07-06", "2025-09-12", "2025-12-15"];
    let mut out = [(0_i64, 0_i64); 3];
    for (i, day) in days.iter().enumerate() {
        let from = day_start_ms(day).expect("raid window is a valid day");
        out[i] = (from, from + 86_400_000);
    }
    out
}

/// True when the instant falls inside a raid-kind window, half-open
/// `[start-day 00:00, end-day+1 00:00)` (legacy `inRaidWindow` via
/// `windowBounds`).
fn in_raid_window(at_ms: i64) -> bool {
    raid_bounds()
        .iter()
        .any(|(from, to)| at_ms >= *from && at_ms < *to)
}

/// Where a gap member's departure falls relative to what we can still read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GapKind {
    /// Last join predates the oldest log message scanned: unknowable.
    PreCoverage,
    /// Last join is inside scanned history but no leave row: a logger miss.
    LogMiss,
    /// Joined inside a raid window and never came back: mass-join residue.
    RaidResidue,
    /// Joined twice or more with no leave between: left at least once.
    RejoinGap,
}

/// One row a future fill card would write. Proposed here, written nowhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedFill {
    pub occurred_at: String,
    /// Always `earliest-possible`: THAT, never WHEN.
    pub bound: &'static str,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaveGap {
    pub guild_id: String,
    pub member_id: String,
    pub kind: GapKind,
    pub last_join_at: String,
    pub joins_seen: usize,
    /// The human sentence: what happened and what (if anything) to do.
    pub detail: String,
    pub fills: Vec<ProposedFill>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
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

/// Sort a gap member's joins oldest-first and propose fills, one per join
/// instant, each stamped AT the join it bounds: the member was provably
/// present then and gone sometime after. A rejoin additionally proves the
/// inter-join departure (the note says so), but the fill still stamps the
/// earlier join: stamping the later one would share its `occurred_at` with
/// the next fill and the idempotency key would merge them into one row.
fn fills_for(joins: &[String], kind: GapKind) -> Vec<ProposedFill> {
    let mut fills = Vec::with_capacity(joins.len());
    for pair in joins.windows(2) {
        fills.push(ProposedFill {
            occurred_at: pair[0].clone(),
            bound: "earliest-possible",
            note: format!(
                "provably present at {}, gone sometime after - and necessarily before the rejoin at {}",
                pair[0], pair[1],
            ),
        });
    }
    let last = joins.last().cloned().unwrap_or_default();
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
        occurred_at: last,
        bound: "earliest-possible",
        note: kind_note,
    });
    fills
}

/// Partition every joined member into present / resolved / gap / skipped.
///
/// `log_floor` is the oldest log timestamp the backfill actually read (its
/// `scannedBackTo`). None means unknown, in which case nothing is called
/// pre-coverage: without a floor that label would be a guess, so those gaps
/// read as log-miss with the floor-unknown note.
/// Joins per (guild, member): the guild plus each join instant, both the
/// epoch millis for comparison and the source spelling for reports/fills.
type JoinsByMember = std::collections::BTreeMap<(String, String), (String, Vec<(i64, String)>)>;

#[must_use]
pub fn classify_leave_gaps(
    joins: &[GapJoin],
    leaves: &[GapLeave],
    roster: &[RosterMember],
    log_floor: Option<&str>,
) -> ClassifyResult {
    let mut result = ClassifyResult::default();

    let mut joins_by_member: JoinsByMember = std::collections::BTreeMap::new();
    for j in joins {
        let Some(member_id) = j.member_id.as_deref().filter(|m| !m.is_empty()) else {
            result.skipped += 1;
            continue;
        };
        // Empty and malformed timestamps are skipped, never paired
        // somewhere (legacy `usableTime` rejects falsy and NaN alike).
        let Some(at) = parse_iso_millis(&j.occurred_at) else {
            result.skipped += 1;
            continue;
        };
        joins_by_member
            .entry((j.guild_id.clone(), member_id.to_owned()))
            .or_insert_with(|| (j.guild_id.clone(), Vec::new()))
            .1
            .push((at, j.occurred_at.clone()));
    }

    let mut left_members = std::collections::HashSet::new();
    for l in leaves {
        let Some(member_id) = l.member_id.as_deref().filter(|m| !m.is_empty()) else {
            result.skipped += 1;
            continue;
        };
        if parse_iso_millis(&l.occurred_at).is_none() {
            result.skipped += 1;
            continue;
        }
        left_members.insert((l.guild_id.clone(), member_id.to_owned()));
    }

    let mut on_roster = std::collections::HashSet::new();
    for r in roster {
        match r.member_id.as_deref().filter(|m| !m.is_empty()) {
            Some(member_id) => {
                on_roster.insert((r.guild_id.clone(), member_id.to_owned()));
            }
            None => result.skipped += 1,
        }
    }

    for ((guild_id, member_id), (guild, times)) in joins_by_member {
        let key = (guild_id.clone(), member_id.clone());
        if left_members.contains(&key) {
            // At least one leave row: the departure is recorded.
            result.resolved += 1;
            continue;
        }
        if on_roster.contains(&key) {
            // Still here with no leave row is the correct state, not a gap.
            result.present += 1;
            continue;
        }
        // Compare instants, retain the source spelling in reports and fills.
        let mut times = times;
        times.sort_by_key(|a| a.0);
        let last_join_ms = times.last().map(|(ms, _)| *ms).unwrap_or(0);
        let last_join = times.last().map(|(_, s)| s.clone()).unwrap_or_default();
        let spellings: Vec<String> = times.into_iter().map(|(_, s)| s).collect();
        let floor_ms = log_floor.and_then(parse_iso_millis);
        let (kind, detail) = if spellings.len() > 1 {
            (
                GapKind::RejoinGap,
                format!(
                    "{} joins and no leave: left at least once between them, and left again after {}. \
                     Fill one leave per inter-join gap plus the final departure.",
                    spellings.len(),
                    last_join,
                ),
            )
        } else if in_raid_window(last_join_ms) {
            (
                GapKind::RaidResidue,
                format!(
                    "joined inside a raid window ({last_join}) and never recorded leaving: likely \
                     removed in a cleanup the logs do not show. Confirm against the cleanup before \
                     counting this as churn - never auto-fill as organic."
                ),
            )
        } else if floor_ms.is_some_and(|floor| last_join_ms < floor) {
            let floor = log_floor.unwrap_or_default();
            (
                GapKind::PreCoverage,
                format!(
                    "last join {last_join} predates the oldest scanned log message ({floor}): the \
                     departure left no record we can still read. Fill establishes THAT they left, never WHEN."
                ),
            )
        } else {
            let floor_note = match log_floor {
                Some(floor) => format!("inside scanned history (floor {floor})"),
                None => "inside scanned history (log floor unknown)".to_owned(),
            };
            (
                GapKind::LogMiss,
                format!(
                    "last join {last_join} is {floor_note} but no leave row: the logger missed it \
                     (format drift, missing footer id, mixed-feed skip, or a bot-down window). \
                     Re-scan the raw logs for this id first."
                ),
            )
        };
        result.gaps.push(LeaveGap {
            guild_id: guild,
            member_id,
            kind,
            last_join_at: last_join,
            joins_seen: spellings.len(),
            detail,
            fills: fills_for(&spellings, kind),
        });
    }

    result.gaps.sort_by(|a, b| {
        let ao = parse_iso_millis(&a.last_join_at).unwrap_or(i64::MAX);
        let bo = parse_iso_millis(&b.last_join_at).unwrap_or(i64::MAX);
        ao.cmp(&bo).then(a.member_id.cmp(&b.member_id))
    });
    result
}

/// The two feeds the classifier needs. SELECT only: the sweep never writes.
/// `since` bounds both feeds (instant comparison); omit it for the
/// full-history sweep. `guild` scopes to one server.
pub struct LeaveGapFeeds {
    pub joins: Vec<GapJoin>,
    pub leaves: Vec<GapLeave>,
}

pub async fn fetch_leave_gap_feeds(
    pool: &sqlx::PgPool,
    guild: Option<&str>,
    since: Option<&str>,
) -> Result<LeaveGapFeeds, sqlx::Error> {
    let join_rows: Vec<(String, Option<String>, time::OffsetDateTime, String)> = sqlx::query_as(
        "SELECT guild_id, member_id, occurred_at, source FROM events
              WHERE event_type = 'member_join'
                AND ($1::text IS NULL OR guild_id = $1)
                AND ($2::timestamptz IS NULL OR occurred_at >= $2::timestamptz)
              ORDER BY occurred_at",
    )
    .bind(guild)
    .bind(since)
    .fetch_all(pool)
    .await?;
    let leave_rows: Vec<(String, Option<String>, time::OffsetDateTime)> = sqlx::query_as(
        "SELECT guild_id, member_id, occurred_at FROM events
          WHERE event_type = 'member_leave'
            AND ($1::text IS NULL OR guild_id = $1)
            AND ($2::timestamptz IS NULL OR occurred_at >= $2::timestamptz)
          ORDER BY occurred_at",
    )
    .bind(guild)
    .bind(since)
    .fetch_all(pool)
    .await?;

    let iso = |t: &time::OffsetDateTime| {
        two_bot_core::funnel::format_iso_millis((t.unix_timestamp_nanos() / 1_000_000) as i64)
    };
    // Memberless rows pass through here (legacy `fetchLeaveGapFeeds` maps
    // them); the classifier counts them as skipped.
    Ok(LeaveGapFeeds {
        joins: join_rows
            .into_iter()
            .map(|(g, m, at, source)| GapJoin {
                guild_id: g,
                member_id: m,
                occurred_at: iso(&at),
                source,
            })
            .collect(),
        leaves: leave_rows
            .into_iter()
            .map(|(g, m, at)| GapLeave {
                guild_id: g,
                member_id: m,
                occurred_at: iso(&at),
            })
            .collect(),
    })
}

/// Reviewer fixture: seven members covering every path (legacy
/// `buildSeedGapData`): one present, one resolved, one pre-coverage, one
/// log-miss, one raid-residue, one rejoin-gap, two malformed rows skipped.
pub fn build_seed_gap_data() -> (Vec<GapJoin>, Vec<GapLeave>, Vec<RosterMember>, String) {
    const G: &str = "seed-guild";
    let join = |member_id: Option<&str>, occurred_at: &str| GapJoin {
        guild_id: G.to_owned(),
        member_id: member_id.map(str::to_owned),
        occurred_at: occurred_at.to_owned(),
        source: "backfill:log:join-leave-log".to_owned(),
    };
    let joins = vec![
        // Still here with no leave row: correct, counted as present.
        join(Some("m-present"), "2025-06-01T10:00:00.000Z"),
        // Join plus leave, gone: resolved.
        join(Some("m-clean"), "2025-05-01T10:00:00.000Z"),
        // Last join older than the floor: pre-coverage.
        join(Some("m-pre"), "2023-01-15T10:00:00.000Z"),
        // Last join inside history with no leave: log-miss.
        join(Some("m-miss"), "2025-06-15T10:00:00.000Z"),
        // Joined mid-raid, never seen leaving: raid residue, not churn.
        join(Some("m-raid"), "2025-07-06T21:00:00.000Z"),
        // Two joins, no leaves: left between them and after the last one.
        join(Some("m-rejoin"), "2024-05-01T10:00:00.000Z"),
        join(Some("m-rejoin"), "2024-09-01T10:00:00.000Z"),
        // Malformed: skipped, never paired.
        join(None, "2025-06-01T10:00:00.000Z"),
        join(Some("m-badtime"), "not-a-timestamp"),
    ];
    let leaves = vec![
        GapLeave {
            guild_id: G.to_owned(),
            member_id: Some("m-clean".to_owned()),
            occurred_at: "2025-05-10T10:00:00.000Z".to_owned(),
        },
        // Malformed: skipped, never paired.
        GapLeave {
            guild_id: G.to_owned(),
            member_id: None,
            occurred_at: "2025-05-11T10:00:00.000Z".to_owned(),
        },
    ];
    let roster = vec![
        RosterMember {
            guild_id: G.to_owned(),
            member_id: Some("m-present".to_owned()),
        },
        RosterMember {
            guild_id: G.to_owned(),
            member_id: Some("someone-never-logged".to_owned()),
        },
    ];
    (joins, leaves, roster, "2024-01-01T00:00:00.000Z".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: &str = "g1";

    fn join(member_id: Option<&str>, at: &str) -> GapJoin {
        GapJoin {
            guild_id: G.to_owned(),
            member_id: member_id.map(str::to_owned),
            occurred_at: at.to_owned(),
            source: "backfill:log:join-leave-log".to_owned(),
        }
    }

    fn leave(member_id: Option<&str>, at: &str) -> GapLeave {
        GapLeave {
            guild_id: G.to_owned(),
            member_id: member_id.map(str::to_owned),
            occurred_at: at.to_owned(),
        }
    }

    fn on_roster(member_id: Option<&str>) -> RosterMember {
        RosterMember {
            guild_id: G.to_owned(),
            member_id: member_id.map(str::to_owned),
        }
    }

    #[test]
    fn present_members_with_no_leave_row_are_correct_not_a_gap() {
        let r = classify_leave_gaps(
            &[join(Some("m"), "2025-06-01T10:00:00.000Z")],
            &[],
            &[on_roster(Some("m"))],
            None,
        );
        assert!(r.gaps.is_empty());
        assert_eq!(r.present, 1);
        assert_eq!(r.resolved, 0);
    }

    #[test]
    fn departed_members_with_a_leave_row_are_resolved() {
        let r = classify_leave_gaps(
            &[join(Some("m"), "2025-06-01T10:00:00.000Z")],
            &[leave(Some("m"), "2025-06-10T10:00:00.000Z")],
            &[],
            None,
        );
        assert!(r.gaps.is_empty());
        assert_eq!(r.resolved, 1);
    }

    #[test]
    fn last_join_before_the_floor_is_pre_coverage() {
        let r = classify_leave_gaps(
            &[join(Some("m"), "2023-01-15T10:00:00.000Z")],
            &[],
            &[],
            Some("2024-01-01T00:00:00.000Z"),
        );
        assert_eq!(r.gaps.len(), 1);
        assert_eq!(r.gaps[0].kind, GapKind::PreCoverage);
        assert_eq!(
            r.gaps[0].fills,
            vec![ProposedFill {
                occurred_at: "2023-01-15T10:00:00.000Z".to_owned(),
                bound: "earliest-possible",
                note: "leave predates scanned log history: the instant is unknowable".to_owned(),
            }]
        );
    }

    #[test]
    fn last_join_inside_history_is_log_miss_with_or_without_floor() {
        let with_floor = classify_leave_gaps(
            &[join(Some("m"), "2025-06-15T10:00:00.000Z")],
            &[],
            &[],
            Some("2024-01-01T00:00:00.000Z"),
        );
        assert_eq!(with_floor.gaps[0].kind, GapKind::LogMiss);
        assert!(with_floor.gaps[0].detail.contains("inside scanned history"));

        let no_floor = classify_leave_gaps(
            &[join(Some("m"), "2025-06-15T10:00:00.000Z")],
            &[],
            &[],
            None,
        );
        assert_eq!(no_floor.gaps[0].kind, GapKind::LogMiss);
        assert!(no_floor.gaps[0].detail.contains("log floor unknown"));
    }

    #[test]
    fn raid_window_joins_are_residue_never_organic_churn() {
        let r = classify_leave_gaps(
            &[join(Some("m"), "2025-07-06T21:00:00.000Z")],
            &[],
            &[],
            Some("2024-01-01T00:00:00.000Z"),
        );
        assert_eq!(r.gaps[0].kind, GapKind::RaidResidue);
        assert!(r.gaps[0].detail.to_lowercase().contains("cleanup"));
    }

    #[test]
    fn rejoins_fill_one_leave_per_gap_plus_final_departure() {
        let r = classify_leave_gaps(
            &[
                join(Some("m"), "2024-05-01T10:00:00.000Z"),
                join(Some("m"), "2024-09-01T10:00:00.000Z"),
            ],
            &[],
            &[],
            None,
        );
        assert_eq!(r.gaps[0].kind, GapKind::RejoinGap);
        assert_eq!(r.gaps[0].joins_seen, 2);
        assert!(r.gaps[0]
            .fills
            .iter()
            .all(|f| f.bound == "earliest-possible"));
        // Each fill stamps the join it bounds: distinct instants, so the
        // occurred_at-keyed idempotency key keeps them as distinct rows.
        assert_eq!(
            r.gaps[0]
                .fills
                .iter()
                .map(|f| f.occurred_at.as_str())
                .collect::<Vec<_>>(),
            vec!["2024-05-01T10:00:00.000Z", "2024-09-01T10:00:00.000Z"]
        );
        assert!(r.gaps[0].fills[0]
            .note
            .contains("necessarily before the rejoin"));
    }

    #[test]
    fn malformed_rows_are_skipped_never_paired() {
        let r = classify_leave_gaps(
            &[
                join(None, "2025-06-01T10:00:00.000Z"),
                join(Some("m"), "garbage"),
            ],
            &[leave(None, "2025-06-10T10:00:00.000Z")],
            &[on_roster(None)],
            None,
        );
        assert_eq!(r.skipped, 4);
        assert!(r.gaps.is_empty());
        assert_eq!(r.present, 0);
        assert_eq!(r.resolved, 0);
    }

    #[test]
    fn coverage_uses_instants_for_equivalent_spellings() {
        // `2026-08-31T22:30Z` == `2026-09-01T00:30+02:00` == `2026-08-31T17:30-05:00`.
        for (spellings, floor, kind) in [
            (
                vec![
                    "2026-08-31T22:30:00.000Z",
                    "2026-09-01T00:30:00+02:00",
                    "2026-08-31T17:30:00-05:00",
                ],
                "2026-09-01T00:00:00.000Z",
                GapKind::PreCoverage,
            ),
            (
                vec![
                    "2026-09-01T00:00:00.000Z",
                    "2026-09-01T00:00:00Z",
                    "2026-08-31T19:00:00-05:00",
                ],
                "2026-09-01T00:00:00.000Z",
                GapKind::LogMiss,
            ),
        ] {
            for at in spellings {
                let r = classify_leave_gaps(&[join(Some("m"), at)], &[], &[], Some(floor));
                assert_eq!(r.gaps[0].kind, kind, "{at} against {floor}");
                assert_eq!(r.gaps[0].last_join_at, at);
                assert_eq!(r.gaps[0].fills[0].occurred_at, at);
                assert_eq!(r.gaps[0].fills[0].bound, "earliest-possible");
            }
        }
    }

    #[test]
    fn raid_uses_instants_for_equivalent_spellings() {
        for (spellings, kind) in [
            (
                vec!["2025-07-05T23:59:59.999Z", "2025-07-06T01:59:59.999+02:00"],
                GapKind::LogMiss,
            ),
            (
                vec!["2025-07-06T00:00:00.000Z", "2025-07-05T19:00:00-05:00"],
                GapKind::RaidResidue,
            ),
            (
                vec!["2025-07-06T23:30:00.000Z", "2025-07-07T01:30:00+02:00"],
                GapKind::RaidResidue,
            ),
            (
                vec!["2025-07-07T00:00:00.000Z", "2025-07-06T19:00:00-05:00"],
                GapKind::LogMiss,
            ),
        ] {
            for at in spellings {
                let r = classify_leave_gaps(&[join(Some("m"), at)], &[], &[], None);
                assert_eq!(r.gaps[0].kind, kind, "{at}");
            }
        }
    }

    #[test]
    fn offset_joins_with_reversed_string_order_yield_chronological_fills() {
        let earlier = "2026-09-01T00:30:00+02:00";
        let later = "2026-08-31T18:00:00-05:00";
        assert!(earlier > later, "offset spellings reverse instant order");
        for input in [[earlier, later], [later, earlier]] {
            let r = classify_leave_gaps(&input.map(|at| join(Some("m"), at)), &[], &[], None);
            let gap = &r.gaps[0];
            assert_eq!(gap.kind, GapKind::RejoinGap);
            assert_eq!(gap.joins_seen, 2);
            assert_eq!(gap.last_join_at, later);
            assert_eq!(
                gap.fills
                    .iter()
                    .map(|f| f.occurred_at.as_str())
                    .collect::<Vec<_>>(),
                vec![earlier, later]
            );
            assert!(gap.fills.iter().all(|f| f.bound == "earliest-possible"));
            assert_eq!(
                gap.fills[0].note,
                format!("provably present at {earlier}, gone sometime after - and necessarily before the rejoin at {later}")
            );
            assert!(gap.detail.contains(&format!("left again after {later}")));
        }
    }

    #[test]
    fn gap_members_sort_by_last_join_instant_with_member_tiebreak() {
        let r = classify_leave_gaps(
            &[
                join(Some("a-later"), "2026-08-31T18:00:00-05:00"),
                join(Some("z-earlier"), "2026-09-01T00:30:00+02:00"),
                join(Some("b-tied"), "2026-08-31T22:30:00.000Z"),
            ],
            &[],
            &[],
            None,
        );
        assert_eq!(
            r.gaps
                .iter()
                .map(|g| g.member_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b-tied", "z-earlier", "a-later"]
        );
    }

    #[test]
    fn empty_and_malformed_leave_timestamps_do_not_resolve_epoch_join() {
        let r = classify_leave_gaps(
            &[
                join(Some("m"), "1970-01-01T00:00:00.000Z"),
                join(Some("empty"), ""),
            ],
            &[leave(Some("m"), ""), leave(Some("m"), "not-a-time")],
            &[],
            Some("1970-01-01T01:00:00+01:00"),
        );
        assert_eq!(r.skipped, 3);
        assert_eq!(r.resolved, 0);
        assert_eq!(r.gaps.len(), 1);
        assert_eq!(r.gaps[0].kind, GapKind::LogMiss);
    }

    #[test]
    fn seeded_gaps_cover_every_path() {
        let (joins, leaves, roster, floor) = build_seed_gap_data();
        let r = classify_leave_gaps(&joins, &leaves, &roster, Some(floor.as_str()));
        let mut kinds: Vec<_> = r.gaps.iter().map(|g| g.kind).collect();
        kinds.sort_by_key(|k| *k as u8);
        assert_eq!(
            kinds,
            vec![
                GapKind::PreCoverage,
                GapKind::LogMiss,
                GapKind::RaidResidue,
                GapKind::RejoinGap,
            ]
        );
        assert_eq!(r.present, 1);
        assert_eq!(r.resolved, 1);
        assert_eq!(r.skipped, 3);
        // Every proposed fill is bounded THAT-not-WHEN, never a bare timestamp.
        for g in &r.gaps {
            assert!(!g.fills.is_empty());
            assert!(g.fills.iter().all(|f| f.bound == "earliest-possible"));
            assert!(!g.detail.is_empty());
        }
        // No two fills for one member share a timestamp: occurred_at-keyed
        // idempotency would merge them and a departure would vanish.
        for g in &r.gaps {
            let distinct: std::collections::HashSet<_> =
                g.fills.iter().map(|f| &f.occurred_at).collect();
            assert_eq!(distinct.len(), g.fills.len());
        }
        // Legacy pins 5 proposed fills across the 4 gaps (1+1+1+2).
        assert_eq!(r.gaps.iter().map(|g| g.fills.len()).sum::<usize>(), 5);
    }
}
