//! Invite attribution (port of two-bot `src/core/inviteTracker.ts`).
//!
//! Discord never says which invite a member used. The trick is a snapshot of
//! every invite's use count: on a join, the code whose count grew gets the
//! credit. One code grew → `invite:CODE`; several → `ambiguous:a+b`;
//! nothing (vanity URL or Discovery) → `vanity` / `unknown`. `ambiguous` and
//! `unknown` are different facts with different fixes — never round one into
//! the other (TOG-5681).
//!
//! Storage stays behind [`InviteSnapshotStore`] (S6 implements it with sqlx;
//! tests use [`MemSnapshots`]). Everything else here is pure.

use std::collections::{HashMap, HashSet};

use crate::Snowflake;

/// One invite counter reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteState {
    pub code: String,
    pub uses: u64,
    pub inviter_id: Option<Snowflake>,
    pub channel_id: Option<Snowflake>,
}

/// How much each invite code was used between two counter readings.
///
/// A code absent from `prev` was created inside the window, so ALL its uses
/// are new. A code whose count went DOWN (deleted and recreated) is clamped
/// out — never let it cancel a real increase elsewhere.
#[must_use]
pub fn invite_growth(prev: &HashMap<String, u64>, current: &[InviteState]) -> HashMap<String, u64> {
    let mut growth = HashMap::new();
    for inv in current {
        let delta = match prev.get(&inv.code) {
            None => inv.uses,
            Some(before) => inv.uses.saturating_sub(*before),
        };
        if delta > 0 {
            growth.insert(inv.code.clone(), delta);
        }
    }
    growth
}

/// One join's attribution for a whole capture window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinAttribution {
    /// `invite:CODE` / `ambiguous:a+b` / `vanity` / `unknown`.
    pub source: String,
    /// True only when THIS member provably came through THIS code. Per-code
    /// join COUNTS may be quoted on `false`; per-member rates (AM7/AM30) may not.
    pub exact: bool,
}

/// Decide a source for every join in one capture window.
///
/// * Nothing moved → vanity or unknown. Nobody's source is proven.
/// * Counters and member list AGREE → each code gets as many joins as it
///   gained (exact per-code counts; member↔code pairing still unobservable,
///   so `exact` stays false in the multi-code branch).
/// * They DISAGREE → honest `ambiguous:a+b` (someone joined and left inside
///   the window, or came via vanity).
///
/// `exact` is true only when one code moved AND the arithmetic closes.
#[must_use]
pub fn attribute_joins(
    growth: &HashMap<String, u64>,
    join_count: usize,
    guild_has_vanity: bool,
) -> Vec<JoinAttribution> {
    if join_count == 0 {
        return Vec::new();
    }
    let fill = |source: &str, exact: bool| {
        (0..join_count)
            .map(|_| JoinAttribution {
                source: source.to_owned(),
                exact,
            })
            .collect()
    };
    let mut codes: Vec<&String> = growth.keys().collect();
    codes.sort();
    let total: u64 = growth.values().sum();
    let closes = total == join_count as u64;

    if codes.is_empty() {
        return fill(
            if guild_has_vanity {
                "vanity"
            } else {
                "unknown"
            },
            false,
        );
    }
    if codes.len() == 1 {
        return fill(&format!("invite:{}", codes[0]), closes);
    }
    if !closes {
        let joined = codes
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("+");
        return fill(&format!("ambiguous:{joined}"), false);
    }
    let mut out = Vec::new();
    for code in codes {
        for _ in 0..growth.get(code).copied().unwrap_or(0) {
            out.push(JoinAttribution {
                source: format!("invite:{code}"),
                exact: false,
            });
        }
    }
    out
}

/// Attribution-quality bucket for the funnel report (TOG-5681).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributionCategory {
    Ambiguous,
    Unknown,
    Other,
}

/// Which bucket one join `source` string belongs in.
#[must_use]
pub fn attribution_category(source: &str) -> AttributionCategory {
    if source == "unknown" {
        AttributionCategory::Unknown
    } else if source == "ambiguous" || source.starts_with("ambiguous:") {
        AttributionCategory::Ambiguous
    } else {
        AttributionCategory::Other
    }
}

/// Counts of the two honest-failure buckets over grouped join rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AttributionSplit {
    pub ambiguous: u64,
    pub unknown: u64,
}

#[must_use]
pub fn summarize_attribution_split(rows: &[(&str, u64)]) -> AttributionSplit {
    let mut out = AttributionSplit::default();
    for (source, n) in rows {
        match attribution_category(source) {
            AttributionCategory::Ambiguous => out.ambiguous += n,
            AttributionCategory::Unknown => out.unknown += n,
            AttributionCategory::Other => {}
        }
    }
    out
}

// --- join-downtime unknown attribution (TOG-5719) ---------------------------
//
// Joins while the bot is down file `unknown` with no accounting of how much
// of `unknown` that explains. These functions name each bot-down window and
// count the `unknown` joins inside it — a count, never a re-attribution.
// Only `unknown` counts; the result is an UPPER BOUND, not a proof.

/// One interval in which the bot was not writing. Shape-matches `BlindWindow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DowntimeWindow {
    pub start: String,
    pub end: String,
    pub gap_ms: i64,
}

/// A down window plus the unknown joins in it (half-open `[start, end)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DowntimeWindowCount {
    pub window: DowntimeWindow,
    pub downtime_unknown: usize,
}

#[must_use]
pub fn count_downtime_unknown_joins(
    windows: &[DowntimeWindow],
    joins: &[(&str, &str)],
) -> Vec<DowntimeWindowCount> {
    use crate::funnel::parse_iso_millis;
    let bounds: Vec<(Option<i64>, Option<i64>)> = windows
        .iter()
        .map(|w| (parse_iso_millis(&w.start), parse_iso_millis(&w.end)))
        .collect();
    let mut out: Vec<DowntimeWindowCount> = windows
        .iter()
        .cloned()
        .map(|window| DowntimeWindowCount {
            window,
            downtime_unknown: 0,
        })
        .collect();
    for (occurred_at, source) in joins {
        if *source != "unknown" {
            continue;
        }
        let Some(t) = parse_iso_millis(occurred_at) else {
            continue;
        };
        for (i, (start, end)) in bounds.iter().enumerate() {
            match (start, end) {
                (Some(s), Some(e)) if *s <= t && t < *e => {
                    out[i].downtime_unknown += 1;
                    break;
                }
                _ => {}
            }
        }
    }
    out
}

// --- live tracker ------------------------------------------------------------
//
// Snapshot freshness (TOG-11716): counters only move on joins, creates and
// deletes, so a recent baseline diffs honestly — but after a long quiet
// stretch (bot down, missed `InviteDelete`, deleted-and-recreated codes)
// the next diff cannot tell genuine growth from drift. A baseline older
// than `INVITE_SNAPSHOT_STALENESS_BOUND_MS` is therefore re-seeded, not
// diffed: the read stores the fresh counters, credits NOTHING, and the
// join in that window files `vanity`/`unknown`. The NEXT join measures
// against the fresh baseline.
//
// Freshness lives in the tracker, not the store: the `InviteSnapshotStore`
// seam stays timeless so the S6 sqlx implementation and the transactional
// commit path (`gateway_session`) need no changes.

/// Maximum age of a successful full invite snapshot before its counters stop
/// being trusted for attribution.
///
/// One hour: presence probes on the same cadence, well under the 2h voice
/// blind-window bound, and long enough that an active guild (snapshotted on
/// every join) never trips it — only genuinely quiet/stale baselines do.
pub const INVITE_SNAPSHOT_STALENESS_BOUND_MS: i64 = 3_600_000;

/// True when the baseline observed at `last_observed_ms` is too old to trust
/// for attribution at `now_ms`.
///
/// * No baseline (`None`) is NOT stale: the window diff already treats every
///   code as new (no credit) except codes witnessed at creation via
///   [`InviteTracker::seed`], which is exactly the seeding path.
/// * A backwards clock (negative age) is a glitch, not evidence of drift.
/// * An overflowing subtraction is unmeasurable — call it stale.
#[must_use]
pub fn is_snapshot_stale(last_observed_ms: Option<i64>, now_ms: i64) -> bool {
    match last_observed_ms {
        None => false,
        Some(last) => match now_ms.checked_sub(last) {
            Some(age) => age > INVITE_SNAPSHOT_STALENESS_BOUND_MS,
            None => true,
        },
    }
}

/// Wall clock in epoch millis (mirrors `funnel::now_millis` without pulling
/// that module in; this file stays dependency-free).
fn wall_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Snapshot persistence seam. S6 implements this with sqlx over
/// `invite_snapshots`; the tracker logic above it is storage-free.
pub trait InviteSnapshotStore {
    fn load(&self, guild_id: Snowflake) -> Vec<InviteState>;
    fn store_all(&self, guild_id: Snowflake, states: &[InviteState]);
    fn delete_missing(&self, guild_id: Snowflake, live_codes: &HashSet<String>);
}

/// Live invite tracker: snapshot diffing + attribution strings.
///
/// Owns the per-guild freshness clock behind
/// [`INVITE_SNAPSHOT_STALENESS_BOUND_MS`]. Only successful FULL reads refresh
/// it: single-code [`InviteTracker::seed`] upserts say nothing about the
/// other codes' counters, and failed reads never reach the tracker.
pub struct InviteTracker<S> {
    store: S,
    last_observed_ms: std::sync::Mutex<HashMap<Snowflake, i64>>,
}

impl<S: InviteSnapshotStore> InviteTracker<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            last_observed_ms: std::sync::Mutex::default(),
        }
    }

    /// Seed a created invite without discarding unrelated live codes.
    ///
    /// A witnessed creation-time baseline: it never refreshes the freshness
    /// clock, so a guild whose only state came from `InviteCreate` still
    /// diffs normally (creation-time codes are trusted baselines, and a
    /// guild with no full read yet has nothing stale to distrust).
    pub fn seed(&self, guild_id: Snowflake, state: InviteState) {
        self.store.store_all(guild_id, &[state]);
    }

    /// Replace the stored snapshot; return the codes that grew. New codes are
    /// stored but never count as growth live (only the window path,
    /// [`invite_growth`], credits those).
    ///
    /// Wall-clock shorthand for [`InviteTracker::diff_and_store_at`].
    pub fn diff_and_store(&self, guild_id: Snowflake, current: &[InviteState]) -> Vec<String> {
        self.diff_and_store_at(guild_id, current, None)
    }

    /// Replace the stored snapshot as observed at `now_ms` (epoch millis;
    /// `None` falls back to the wall clock); return the codes that grew.
    ///
    /// When the guild's last successful full read is older than
    /// [`INVITE_SNAPSHOT_STALENESS_BOUND_MS`], the baseline is untrusted:
    /// the fresh counters are STORED (re-seed) but nothing is credited, so
    /// this window's join files `vanity`/`unknown` and the next join diffs
    /// against the fresh baseline.
    pub fn diff_and_store_at(
        &self,
        guild_id: Snowflake,
        current: &[InviteState],
        now_ms: Option<i64>,
    ) -> Vec<String> {
        let now = now_ms.unwrap_or_else(wall_millis);
        let last = self
            .last_observed_ms
            .lock()
            .expect("freshness lock")
            .get(&guild_id)
            .copied();
        let stale = is_snapshot_stale(last, now);
        let prev: HashMap<String, u64> = self
            .store
            .load(guild_id)
            .into_iter()
            .map(|s| (s.code, s.uses))
            .collect();
        let mut grew = Vec::new();
        if !stale {
            for inv in current {
                if let Some(before) = prev.get(&inv.code) {
                    if inv.uses > *before {
                        grew.push(inv.code.clone());
                    }
                }
            }
        }
        self.store.store_all(guild_id, current);
        let live: HashSet<String> = current.iter().map(|s| s.code.clone()).collect();
        self.store.delete_missing(guild_id, &live);
        // The re-seed IS the new baseline — including on the stale path.
        self.last_observed_ms
            .lock()
            .expect("freshness lock")
            .insert(guild_id, now);
        grew
    }

    /// Attribution string for a join, given the codes that grew.
    #[must_use]
    pub fn attribute(&self, grew: &[String], guild_has_vanity: bool) -> String {
        match grew {
            [single] => format!("invite:{single}"),
            [first, rest @ ..] => {
                let mut codes = vec![first.as_str()];
                codes.extend(rest.iter().map(String::as_str));
                format!("ambiguous:{}", codes.join("+"))
            }
            [] => {
                if guild_has_vanity {
                    "vanity".to_owned()
                } else {
                    "unknown".to_owned()
                }
            }
        }
    }

    /// Inviter for a singly-grown code (only called when `grew.len() == 1`).
    #[must_use]
    pub fn inviter_for(&self, guild_id: Snowflake, code: &str) -> Option<Snowflake> {
        self.store
            .load(guild_id)
            .into_iter()
            .find(|s| s.code == code)
            .and_then(|s| s.inviter_id)
    }
}

/// In-memory snapshots for tests.
#[derive(Debug, Default)]
pub struct MemSnapshots {
    tables: std::sync::Mutex<HashMap<Snowflake, HashMap<String, InviteState>>>,
}

impl MemSnapshots {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl InviteSnapshotStore for MemSnapshots {
    fn load(&self, guild_id: Snowflake) -> Vec<InviteState> {
        self.tables
            .lock()
            .expect("snapshot lock")
            .get(&guild_id)
            .map_or_else(Vec::new, |t| t.values().cloned().collect())
    }

    fn store_all(&self, guild_id: Snowflake, states: &[InviteState]) {
        let mut tables = self.tables.lock().expect("snapshot lock");
        let table = tables.entry(guild_id).or_default();
        for s in states {
            table.insert(s.code.clone(), s.clone());
        }
    }

    fn delete_missing(&self, guild_id: Snowflake, live_codes: &HashSet<String>) {
        if let Some(table) = self
            .tables
            .lock()
            .expect("snapshot lock")
            .get_mut(&guild_id)
        {
            table.retain(|code, _| live_codes.contains(code));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(code: &str, uses: u64) -> InviteState {
        InviteState {
            code: code.to_owned(),
            uses,
            inviter_id: Some(7),
            channel_id: None,
        }
    }

    #[test]
    fn growth_credits_new_codes_clamps_drops() {
        let prev = HashMap::from([("a".to_owned(), 5), ("b".to_owned(), 9)]);
        let cur = vec![state("a", 7), state("b", 4), state("c", 3)];
        let g = invite_growth(&prev, &cur);
        assert_eq!(g.get("a"), Some(&2));
        assert!(!g.contains_key("b"));
        assert_eq!(g.get("c"), Some(&3));
    }

    #[test]
    fn live_diff_ignores_new_codes() {
        let t = InviteTracker::new(MemSnapshots::new());
        // Seed snapshot.
        assert!(t.diff_and_store(1, &[state("a", 5)]).is_empty());
        // Existing code grew → credited; brand-new code stored, not credited.
        let grew = t.diff_and_store(1, &[state("a", 6), state("new", 4)]);
        assert_eq!(grew, vec!["a".to_owned()]);
        assert_eq!(t.attribute(&grew, false), "invite:a");
        assert_eq!(t.inviter_for(1, "a"), Some(7));
        // Vanished code dropped: recreating it later counts as new, not growth.
        let grew = t.diff_and_store(1, &[state("a", 6)]);
        assert!(grew.is_empty());
        let grew = t.diff_and_store(1, &[state("a", 6), state("new", 9)]);
        assert!(grew.is_empty());
    }

    #[test]
    fn attribute_strings() {
        let t = InviteTracker::new(MemSnapshots::new());
        assert_eq!(t.attribute(&[], false), "unknown");
        assert_eq!(t.attribute(&[], true), "vanity");
        assert_eq!(
            t.attribute(&["a".to_owned(), "b".to_owned()], false),
            "ambiguous:a+b"
        );
    }

    #[test]
    fn window_attribution_rules() {
        let g = HashMap::from([("b".to_owned(), 1), ("a".to_owned(), 2)]);
        // Arithmetic closes (3 joins): per-code counts in sorted order.
        let out = attribute_joins(&g, 3, false);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|a| !a.exact));
        assert_eq!(out[0].source, "invite:a");
        assert_eq!(out[2].source, "invite:b");
        // Mismatch: honest ambiguous.
        let out = attribute_joins(&g, 5, false);
        assert!(out.iter().all(|a| a.source == "ambiguous:a+b" && !a.exact));
        // One code, closes: exact proof.
        let one = HashMap::from([("a".to_owned(), 2)]);
        let out = attribute_joins(&one, 2, false);
        assert!(out.iter().all(|a| a.source == "invite:a" && a.exact));
        // One code, mismatch: best guess, not proof.
        let out = attribute_joins(&one, 3, false);
        assert!(out.iter().all(|a| a.source == "invite:a" && !a.exact));
        // Nothing moved.
        assert_eq!(
            attribute_joins(&HashMap::new(), 2, true)[0].source,
            "vanity"
        );
        assert_eq!(
            attribute_joins(&HashMap::new(), 2, false)[0].source,
            "unknown"
        );
    }

    #[test]
    fn category_split() {
        let s =
            summarize_attribution_split(&[("invite:x", 3), ("ambiguous:a+b", 2), ("unknown", 4)]);
        assert_eq!(
            s,
            AttributionSplit {
                ambiguous: 2,
                unknown: 4
            }
        );
        assert_eq!(
            attribution_category("ambiguous"),
            AttributionCategory::Ambiguous
        );
        assert_eq!(attribution_category("vanity"), AttributionCategory::Other);
    }

    #[test]
    fn staleness_bound_is_one_hour() {
        assert_eq!(INVITE_SNAPSHOT_STALENESS_BOUND_MS, 3_600_000);
    }

    #[test]
    fn stale_predicate_rules() {
        // No baseline yet: the window diff already treats every code as new
        // (no live credit), so there is nothing stale to distrust.
        assert!(!is_snapshot_stale(None, 1_000_000));
        // Fresh: within the bound.
        assert!(!is_snapshot_stale(Some(1_000_000), 1_000_000));
        assert!(!is_snapshot_stale(
            Some(1_000_000),
            1_000_000 + INVITE_SNAPSHOT_STALENESS_BOUND_MS
        ));
        // Beyond the bound: re-seed, do not diff.
        assert!(is_snapshot_stale(
            Some(1_000_000),
            1_000_000 + INVITE_SNAPSHOT_STALENESS_BOUND_MS + 1
        ));
        // Backwards clock: a glitch, not evidence of drift.
        assert!(!is_snapshot_stale(Some(2_000_000), 1_000_000));
    }

    #[test]
    fn stale_snapshot_reseeds_without_credit_and_next_join_attributes() {
        let t = InviteTracker::new(MemSnapshots::new());
        let t0 = 1_700_000_000_000_i64;
        // Baseline at t0.
        assert!(t
            .diff_and_store_at(1, &[state("a", 5)], Some(t0))
            .is_empty());
        // Fresh read shortly after: growth credits normally.
        let grew = t.diff_and_store_at(1, &[state("a", 6)], Some(t0 + 60_000));
        assert_eq!(grew, vec!["a".to_owned()]);
        // Stale read past the bound: counters stored, nothing credited —
        // this window files vanity/unknown, never drift.
        let grew = t.diff_and_store_at(
            1,
            &[state("a", 9)],
            Some(t0 + 60_000 + INVITE_SNAPSHOT_STALENESS_BOUND_MS + 1),
        );
        assert!(grew.is_empty());
        assert_eq!(t.attribute(&grew, false), "unknown");
        // Next fresh read measures against the re-seeded baseline (9 → 10).
        let grew = t.diff_and_store_at(
            1,
            &[state("a", 10)],
            Some(t0 + 120_000 + INVITE_SNAPSHOT_STALENESS_BOUND_MS + 1),
        );
        assert_eq!(grew, vec!["a".to_owned()]);
    }

    #[test]
    fn seed_baseline_attributes_next_join_without_refreshing_clock() {
        let t = InviteTracker::new(MemSnapshots::new());
        // InviteCreate witness: code seeded at its live uses (0).
        t.seed(1, state("fresh", 0));
        // The next full read shows uses=1: growth of 1, not of the whole
        // counter — even far in the future, because a witnessed
        // creation-time baseline is trusted (nothing stale to distrust).
        let grew = t.diff_and_store_at(
            1,
            &[state("fresh", 1)],
            Some(1_700_000_000_000 + 10 * INVITE_SNAPSHOT_STALENESS_BOUND_MS),
        );
        assert_eq!(grew, vec!["fresh".to_owned()]);
        assert_eq!(t.attribute(&grew, false), "invite:fresh");
    }

    #[test]
    fn downtime_counts_unknown_in_half_open_window() {
        let w = vec![DowntimeWindow {
            start: "2026-09-20T10:00:00.000Z".to_owned(),
            end: "2026-09-20T12:00:00.000Z".to_owned(),
            gap_ms: 7_200_000,
        }];
        let joins = vec![
            ("2026-09-20T11:00:00.000Z", "unknown"),
            ("2026-09-20T12:00:00.000Z", "unknown"), // at end: observed, excluded
            ("2026-09-20T11:30:00.000Z", "invite:x"), // attributed despite gap
        ];
        let c = count_downtime_unknown_joins(&w, &joins);
        assert_eq!(c[0].downtime_unknown, 1);
    }
}
