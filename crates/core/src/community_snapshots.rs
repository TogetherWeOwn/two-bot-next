//! Community snapshots: live counter tick (60 s) and rank tick (10 m).
//!
//! Ports `src/jobs/communitySnapshots.ts` from legacy two-bot (frozen `main`
//! @ `d5d11793`) as framework-free domain logic: inputs are plain data, outcomes
//! are plain data, and every refusal is unit-testable without Discord or
//! Postgres. Style follows `leveling.rs` / `moderation.rs`.
//!
//! Source files (legacy `two-bot`):
//! - `src/jobs/communitySnapshots.ts` — `buildCommunitySnapshot`,
//!   `runLiveCounterCycle`, `runRankSnapshotCycle`, `startCommunitySnapshots`
//! - `src/analytics/anomalies.ts` — `ANOMALIES` (raid windows) + `windowBounds`
//!
//! What this module owns:
//! - tick cadences ([`LIVE_COUNTER_INTERVAL_MS`], [`RANK_SNAPSHOT_INTERVAL_MS`])
//! - the Prospect → Legend ladder ([`RankKey`])
//! - raid-window grounding data ([`RAID_ANOMALIES`], [`window_bounds`])
//! - rank-role matching ([`match_rank_roles`])
//! - one-pass snapshot arithmetic ([`build_community_snapshot`])
//! - the counter-only reading ([`build_counter_reading`])
//! - skip outcomes ([`CounterSkip`], [`RankSkip`])
//! - the single-flight primitive ([`JobGate`])
//!
//! Deliberately out of scope: fetching the roster / roles from Discord (the S4
//! REST executor slice owns that when it lands), running the timers, and SQL.
//! The sqlx writes live in [`crate::website_store`]; the tick recipe is:
//! acquire [`JobGate`] → [`crate::website_store::read_raid_windows`] (None =
//! [`CounterSkip::RaidHistoryNotGrounded`]) → fetch roster via REST → build →
//! write via the store → drop the guard. A failed or ungrounded read writes
//! nothing, and the stale numbers age out in `web_v1` on their own.
//!
//! Legacy deviations, both documented at the use site:
//! - The counter path does not build a placeholder rank ladder. Legacy
//!   `liveSnapshot` calls `buildCommunitySnapshot` with fake `unused:{key}`
//!   role ids and then reads only `humanMemberCount`; [`build_counter_reading`]
//!   computes the same count directly, so counter ticks can never touch the
//!   rank tables.
//! - [`build_community_snapshot`] requires exactly the five ladder roles in
//!   ladder order and returns `None` otherwise; legacy relied on
//!   `fetchRankRoles` always returning five or null.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

/// Counter tick cadence (legacy `LIVE_COUNTER_INTERVAL_MS`).
pub const LIVE_COUNTER_INTERVAL_MS: u64 = 60 * 1000;
/// Rank tick cadence (legacy `RANK_SNAPSHOT_INTERVAL_MS`).
pub const RANK_SNAPSHOT_INTERVAL_MS: u64 = 10 * 60 * 1000;

/// One rung of the Prospect → Legend ladder (legacy `RANKS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RankKey {
    Prospect,
    Member,
    Soldier,
    Veteran,
    Legend,
}

impl RankKey {
    /// All five rungs in ladder order (lowest first).
    pub const ALL: [Self; 5] = [
        Self::Prospect,
        Self::Member,
        Self::Soldier,
        Self::Veteran,
        Self::Legend,
    ];

    /// DB / contract key (`prospect`, …).
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Prospect => "prospect",
            Self::Member => "member",
            Self::Soldier => "soldier",
            Self::Veteran => "veteran",
            Self::Legend => "legend",
        }
    }

    /// Display label (legacy `label`; also the Discord role name).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Prospect => "Prospect",
            Self::Member => "Member",
            Self::Soldier => "Soldier",
            Self::Veteran => "Veteran",
            Self::Legend => "Legend",
        }
    }

    /// Ladder position, 1-based (legacy `order`).
    #[must_use]
    pub fn order(self) -> u8 {
        match self {
            Self::Prospect => 1,
            Self::Member => 2,
            Self::Soldier => 3,
            Self::Veteran => 4,
            Self::Legend => 5,
        }
    }

    /// Parse a DB / contract key.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.key() == key)
    }
}

/// One known raid window (legacy `ANOMALIES` filtered to `kind === 'raid'`).
///
/// Only `raid` windows contain accounts that are candidates for exclusion —
/// `cleanup` and `prune` windows swallow event types for reports but never
/// remove anyone from a count (legacy `readRaidWindows` + `scripts/raid-list.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaidAnomaly {
    pub id: &'static str,
    /// First affected UTC day, inclusive (`YYYY-MM-DD`).
    pub start: &'static str,
    /// Last affected UTC day, inclusive (`YYYY-MM-DD`).
    pub end: &'static str,
}

/// Raid windows in start order (legacy sorts by `start` before reading).
pub const RAID_ANOMALIES: [RaidAnomaly; 3] = [
    RaidAnomaly {
        id: "2025-07-06-raid",
        start: "2025-07-06",
        end: "2025-07-06",
    },
    RaidAnomaly {
        id: "2025-09-12-raid",
        start: "2025-09-12",
        end: "2025-09-12",
    },
    RaidAnomaly {
        id: "2025-12-15-raid",
        start: "2025-12-15",
        end: "2025-12-15",
    },
];

/// Half-open ISO instants `[from, to)` covering every listed day (legacy
/// `windowBounds`: `from` is midnight of `start`, `to` is midnight of the day
/// after `end`). Returns `None` on malformed static data.
#[must_use]
pub fn window_bounds(start: &str, end: &str) -> Option<(String, String)> {
    use time::{Date, Duration};

    let parse_day = |s: &str| {
        let mut parts = s.split('-');
        let (y, m, d) = (
            parts.next()?.parse::<i32>().ok()?,
            parts.next()?.parse::<u8>().ok()?,
            parts.next()?.parse::<u8>().ok()?,
        );
        if parts.next().is_some() {
            return None;
        }
        Date::from_calendar_date(y, time::Month::try_from(m).ok()?, d).ok()
    };
    let fmt = |d: Date| {
        format!(
            "{:04}-{:02}-{:02}T00:00:00.000Z",
            d.year(),
            u8::from(d.month()),
            d.day()
        )
    };
    let from_day = parse_day(start)?;
    let to_day = parse_day(end)?.checked_add(Duration::days(1))?;
    Some((fmt(from_day), fmt(to_day)))
}

/// One roster entry the snapshot reads (legacy `RawMember` subset: id, bot
/// flag, role snowflakes). Observed-at timestamps use [`crate::now_iso`]
/// (from [`crate::funnel`]: the S3 slice landed the identical helper first,
// same contract shape, std-only — so this module reuses it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterMember {
    pub user_id: String,
    pub is_bot: bool,
    pub roles: Vec<String>,
}

/// One matched rank role (legacy `RankRole`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankRole {
    pub key: RankKey,
    pub role_id: String,
}

/// One grounded raid window (legacy `RaidWindow`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaidWindow {
    pub id: String,
    pub excluded_member_ids: HashSet<String>,
}

/// Match the five rank roles by name, case-insensitive (legacy
/// `fetchRankRoles`): each ladder label must match exactly one guild role, or
/// the whole ladder is unusable (`None` → [`RankSkip::RankRoleMissing`]).
/// Returns the roles in ladder order.
#[must_use]
pub fn match_rank_roles(roles: &[(String, String)]) -> Option<Vec<RankRole>> {
    let mut out = Vec::with_capacity(RankKey::ALL.len());
    for key in RankKey::ALL {
        let mut matches = roles
            .iter()
            .filter(|(_, name)| name.trim().eq_ignore_ascii_case(key.label()))
            .map(|(id, _)| id.clone());
        let role_id = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        out.push(RankRole { key, role_id });
    }
    Some(out)
}

/// One rank aggregate row (legacy `rankRows`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankRow {
    pub key: RankKey,
    pub role_id: String,
    /// Members whose HIGHEST rank is this one (mutually exclusive).
    pub member_count: usize,
    /// Members holding the role at all (cumulative).
    pub holders_count: usize,
}

/// Highest rank per member (legacy `memberRanks`; `None` = no rank role).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRank {
    pub member_id: String,
    pub rank_key: Option<RankKey>,
}

/// The one-pass snapshot (legacy `CommunitySnapshot`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunitySnapshot {
    /// Humans minus raid accounts (bots excluded too).
    pub human_member_count: usize,
    /// Sum of `member_count` over the ladder (≤ human count when nested).
    pub ranked_member_count: usize,
    pub rank_rows: Vec<RankRow>,
    pub member_ranks: Vec<MemberRank>,
    /// Exact `scripts/raid-list.ts` removal set, for `member_exclusions`.
    pub excluded_member_ids: Vec<String>,
    /// False when someone holds a higher rank without every lower one.
    pub nested: bool,
    /// Roster entries (bots included) sitting in a raid window.
    pub raid_accounts_excluded: usize,
}

/// Counter-only reading (legacy `liveSnapshot` outputs, without the
/// placeholder ladder — see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterReading {
    pub human_member_count: usize,
    pub raid_accounts_excluded: usize,
}

/// Why a counter tick recorded nothing (legacy `CollectionResult.reason`
/// subset for the 60 s path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterSkip {
    DiscordReadFailed,
    RaidHistoryNotGrounded,
}

/// Why a rank tick recorded nothing (legacy `CollectionResult.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankSkip {
    DiscordReadFailed,
    RaidHistoryNotGrounded,
    RankRoleMissing,
    RanksNotNested,
}

/// Build the full snapshot in one pass (legacy `buildCommunitySnapshot`):
/// bots and raid-window accounts leave the denominator, the ladder aggregates
/// and the per-member highest rank by the same decision. `None` on an empty or
/// malformed roster, or a ladder that is not exactly the five rungs in order.
#[must_use]
pub fn build_community_snapshot(
    members: &[RosterMember],
    rank_roles: &[RankRole],
    raid_windows: &[RaidWindow],
) -> Option<CommunitySnapshot> {
    if members.is_empty() || members.iter().any(|m| m.user_id.is_empty()) {
        return None;
    }
    if rank_roles.len() != RankKey::ALL.len() || !rank_roles.iter().map(|r| r.key).eq(RankKey::ALL)
    {
        return None;
    }

    let raid_accounts: HashSet<&str> = raid_windows
        .iter()
        .flat_map(|w| w.excluded_member_ids.iter().map(String::as_str))
        .collect();
    let included: Vec<&RosterMember> = members
        .iter()
        .filter(|m| !m.is_bot && !raid_accounts.contains(m.user_id.as_str()))
        .collect();

    let role_ids: Vec<&str> = rank_roles.iter().map(|r| r.role_id.as_str()).collect();
    let mut holders: HashMap<RankKey, usize> = HashMap::new();
    let mut highest: HashMap<RankKey, usize> = HashMap::new();
    let mut member_ranks = Vec::with_capacity(included.len());
    let mut nested = true;

    for member in &included {
        let held: HashSet<&str> = member.roles.iter().map(String::as_str).collect();
        let held_indexes: Vec<usize> = role_ids
            .iter()
            .enumerate()
            .filter(|(_, id)| held.contains(*id))
            .map(|(i, _)| i)
            .collect();
        for index in &held_indexes {
            let key = rank_roles[*index].key;
            *holders.entry(key).or_insert(0) += 1;
        }
        let Some(highest_index) = held_indexes.iter().max().copied() else {
            member_ranks.push(MemberRank {
                member_id: member.user_id.clone(),
                rank_key: None,
            });
            continue;
        };
        for role_id in role_ids.iter().take(highest_index + 1) {
            if !held.contains(role_id) {
                nested = false;
            }
        }
        let key = rank_roles[highest_index].key;
        *highest.entry(key).or_insert(0) += 1;
        member_ranks.push(MemberRank {
            member_id: member.user_id.clone(),
            rank_key: Some(key),
        });
    }

    let rank_rows = rank_roles
        .iter()
        .map(|r| RankRow {
            key: r.key,
            role_id: r.role_id.clone(),
            member_count: highest.get(&r.key).copied().unwrap_or(0),
            holders_count: holders.get(&r.key).copied().unwrap_or(0),
        })
        .collect::<Vec<_>>();
    let ranked_member_count = rank_rows.iter().map(|r| r.member_count).sum();
    let raid_accounts_excluded = members
        .iter()
        .filter(|m| raid_accounts.contains(m.user_id.as_str()))
        .count();
    Some(CommunitySnapshot {
        human_member_count: included.len(),
        ranked_member_count,
        rank_rows,
        member_ranks,
        excluded_member_ids: raid_accounts.into_iter().map(str::to_owned).collect(),
        nested,
        raid_accounts_excluded,
    })
}

/// Build the counter-only reading (legacy `liveSnapshot` minus the fake
/// ladder): same guards, same exclusion, no rank arithmetic. `None` on an
/// empty or malformed roster.
#[must_use]
pub fn build_counter_reading(
    members: &[RosterMember],
    raid_windows: &[RaidWindow],
) -> Option<CounterReading> {
    if members.is_empty() || members.iter().any(|m| m.user_id.is_empty()) {
        return None;
    }
    let raid_accounts: HashSet<&str> = raid_windows
        .iter()
        .flat_map(|w| w.excluded_member_ids.iter().map(String::as_str))
        .collect();
    let human_member_count = members
        .iter()
        .filter(|m| !m.is_bot && !raid_accounts.contains(m.user_id.as_str()))
        .count();
    Some(CounterReading {
        human_member_count,
        raid_accounts_excluded: members
            .iter()
            .filter(|m| raid_accounts.contains(m.user_id.as_str()))
            .count(),
    })
}

/// Single-flight guard for one job (the Rust half of legacy's promise queue:
/// `startCommunitySnapshots` chains ticks so an older, slower read can never
/// finish after a newer one and overwrite its timestamp).
///
/// The scheduler holds the guard across a whole tick — fetch, build, write —
/// and a tick that cannot acquire it skips instead of overlapping. `std`-only
/// on purpose: no executor, no dispatcher.
#[derive(Debug, Default)]
pub struct JobGate {
    running: AtomicBool,
}

/// Held across one tick; releases the gate on drop.
#[derive(Debug)]
pub struct JobGuard<'a> {
    gate: &'a JobGate,
}

impl Drop for JobGuard<'_> {
    fn drop(&mut self) {
        self.gate.running.store(false, Ordering::Release);
    }
}

impl JobGate {
    /// Try to start a tick. `None` means a previous tick is still running —
    /// skip this tick, do not queue.
    #[must_use]
    pub fn try_acquire(&self) -> Option<JobGuard<'_>> {
        self.running
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| JobGuard { gate: self })
    }

    /// True while a tick holds the gate (introspection for `/readyz`-style
    /// health reporting; not a lock).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role_ids() -> HashMap<RankKey, String> {
        RankKey::ALL
            .into_iter()
            .map(|k| (k, format!("role-{}", k.key())))
            .collect()
    }

    fn ladder() -> Vec<RankRole> {
        RankKey::ALL
            .into_iter()
            .map(|key| RankRole {
                key,
                role_id: format!("role-{}", key.key()),
            })
            .collect()
    }

    fn member(id: &str, held: &[RankKey], bot: bool) -> RosterMember {
        let ids = role_ids();
        RosterMember {
            user_id: id.to_owned(),
            is_bot: bot,
            roles: held.iter().map(|k| ids[k].clone()).collect(),
        }
    }

    fn raid_window() -> RaidWindow {
        RaidWindow {
            id: "raid".to_owned(),
            excluded_member_ids: HashSet::from(["raid".to_owned()]),
        }
    }

    #[test]
    fn intervals_are_the_contract_values() {
        assert_eq!(LIVE_COUNTER_INTERVAL_MS, 60_000);
        assert_eq!(RANK_SNAPSHOT_INTERVAL_MS, 600_000);
    }

    #[test]
    fn ladder_keys_labels_orders() {
        assert_eq!(
            RankKey::ALL.map(|k| k.key()),
            ["prospect", "member", "soldier", "veteran", "legend"]
        );
        assert_eq!(RankKey::ALL.map(|k| k.order()), [1, 2, 3, 4, 5]);
        assert_eq!(RankKey::parse("legend"), Some(RankKey::Legend));
        assert_eq!(RankKey::parse("nope"), None);
    }

    #[test]
    fn window_bounds_cover_whole_days() {
        assert_eq!(
            window_bounds("2025-07-06", "2025-07-06"),
            Some((
                "2025-07-06T00:00:00.000Z".to_owned(),
                "2025-07-07T00:00:00.000Z".to_owned()
            ))
        );
        assert_eq!(
            window_bounds("2025-12-15", "2025-12-15"),
            Some((
                "2025-12-15T00:00:00.000Z".to_owned(),
                "2025-12-16T00:00:00.000Z".to_owned()
            ))
        );
        // Month and year boundaries roll over.
        assert_eq!(
            window_bounds("2025-09-12", "2025-09-12").map(|(_, to)| to),
            Some("2025-09-13T00:00:00.000Z".to_owned())
        );
        assert_eq!(
            window_bounds("2024-12-31", "2024-12-31").map(|(_, to)| to),
            Some("2025-01-01T00:00:00.000Z".to_owned())
        );
        assert_eq!(window_bounds("not-a-date", "2025-07-06"), None);
        // All three static windows ground to real bounds.
        for anomaly in RAID_ANOMALIES {
            assert!(
                window_bounds(anomaly.start, anomaly.end).is_some(),
                "{}",
                anomaly.id
            );
        }
    }

    #[test]
    fn rank_roles_need_exactly_one_match_per_rung() {
        let roles: Vec<(String, String)> = RankKey::ALL
            .into_iter()
            .map(|k| (format!("role-{}", k.key()), k.label().to_owned()))
            .collect();
        let matched = match_rank_roles(&roles).expect("matches");
        assert_eq!(
            matched.iter().map(|r| r.key).collect::<Vec<_>>(),
            RankKey::ALL
        );
        // Case-insensitive names still match (Discord names vary).
        let upper: Vec<(String, String)> = roles
            .iter()
            .map(|(id, name)| (id.clone(), name.to_uppercase()))
            .collect();
        assert!(match_rank_roles(&upper).is_some());
        // Missing rung → whole ladder unusable.
        assert!(match_rank_roles(&roles[..4]).is_none());
        // Duplicate name → ambiguous, unusable.
        let mut dup = roles.clone();
        dup.push((
            "role-duplicate".to_owned(),
            RankKey::Prospect.label().to_owned(),
        ));
        assert!(match_rank_roles(&dup).is_none());
        // Empty guild → unusable.
        assert!(match_rank_roles(&[]).is_none());
    }

    #[test]
    fn one_pass_excludes_bots_and_raids_from_counter_and_ranks() {
        let all: Vec<RankKey> = RankKey::ALL.into();
        let snapshot = build_community_snapshot(
            &[
                member("prospect", &[RankKey::Prospect], false),
                member("legend", &all, false),
                member("none", &[], false),
                member("raid", &all, false),
                member("bot", &all, true),
            ],
            &ladder(),
            &[raid_window()],
        )
        .expect("snapshot");

        assert_eq!(snapshot.human_member_count, 3);
        assert_eq!(snapshot.raid_accounts_excluded, 1);
        let prospect = snapshot
            .rank_rows
            .iter()
            .find(|r| r.key == RankKey::Prospect)
            .expect("prospect row");
        assert_eq!(prospect.holders_count, 2);
        assert_eq!(prospect.member_count, 1);
        assert_eq!(
            snapshot
                .rank_rows
                .iter()
                .find(|r| r.key == RankKey::Legend)
                .expect("legend row")
                .member_count,
            1
        );
        assert_eq!(snapshot.ranked_member_count, 2);
        assert!(snapshot.nested);
    }

    #[test]
    fn higher_rank_without_lower_ranks_is_not_nested() {
        let snapshot = build_community_snapshot(
            &[member(
                "broken",
                &[RankKey::Prospect, RankKey::Soldier],
                false,
            )],
            &ladder(),
            &[],
        )
        .expect("snapshot");
        assert!(!snapshot.nested);
    }

    #[test]
    fn empty_or_malformed_roster_builds_nothing() {
        assert!(build_community_snapshot(&[], &ladder(), &[]).is_none());
        assert!(build_community_snapshot(
            &[RosterMember {
                user_id: String::new(),
                is_bot: false,
                roles: vec![],
            }],
            &ladder(),
            &[],
        )
        .is_none());
        // Wrong ladder shape builds nothing.
        assert!(
            build_community_snapshot(&[member("human", &[], false)], &ladder()[..4], &[],)
                .is_none()
        );
    }

    #[test]
    fn counter_reading_matches_snapshot_denominator() {
        let all: Vec<RankKey> = RankKey::ALL.into();
        let members = vec![
            member("human", &[RankKey::Prospect], false),
            member("bot", &[], true),
            member("raid", &all, false),
        ];
        let reading = build_counter_reading(&members, &[raid_window()]).expect("reading");
        assert_eq!(
            reading,
            CounterReading {
                human_member_count: 1,
                raid_accounts_excluded: 1,
            }
        );
        let snapshot =
            build_community_snapshot(&members, &ladder(), &[raid_window()]).expect("snapshot");
        assert_eq!(snapshot.human_member_count, reading.human_member_count);
        assert_eq!(
            snapshot.raid_accounts_excluded,
            reading.raid_accounts_excluded
        );
        assert!(build_counter_reading(&[], &[]).is_none());
    }

    #[test]
    fn nested_ladder_never_overcounts_humans() {
        // The rank-tick invariant (legacy `ranked > human` refusal): a nested
        // ladder assigns each member one highest rank, so the sum cannot
        // exceed the denominator.
        let snapshot = build_community_snapshot(
            &[
                // Each holder carries every rung up to their highest (nested).
                member("a", &[RankKey::Prospect, RankKey::Member], false),
                member(
                    "b",
                    &[RankKey::Prospect, RankKey::Member, RankKey::Soldier],
                    false,
                ),
                member("c", &[], false),
            ],
            &ladder(),
            &[],
        )
        .expect("snapshot");
        assert!(snapshot.nested);
        assert!(snapshot.ranked_member_count <= snapshot.human_member_count);
    }

    #[test]
    fn job_gate_is_single_flight() {
        let gate = JobGate::default();
        assert!(!gate.is_running());
        let guard = gate.try_acquire().expect("first acquire");
        assert!(gate.is_running());
        assert!(gate.try_acquire().is_none(), "overlap must skip");
        drop(guard);
        assert!(!gate.is_running());
        assert!(gate.try_acquire().is_some(), "released after tick");
    }

    #[test]
    fn now_iso_has_contract_shape() {
        // `crate::now_iso` (funnel's) is the tick clock; spot-check the shape.
        let now = crate::funnel::now_iso();
        assert!(now.ends_with('Z'), "{now}");
        assert_eq!(now.len(), "2026-09-06T17:30:00.000Z".len(), "{now}");
        assert!(now.contains('T'), "{now}");
    }
}
