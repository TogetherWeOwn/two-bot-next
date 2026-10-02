//! Presence probe domain: hourly guild-size series plus the reopen trigger.
//!
//! Ports the schedule-relevant core of `src/jobs/presenceProbe.ts` +
//! `src/analytics/presence.ts` from legacy two-bot (frozen `main` @ `d5d11793`)
//! as framework-free data plus pure functions, in the same style as
//! `leveling.rs`/`moderation.rs`. No Discord client, no SQL here: the caller
//! supplies the REST reading and the store persists the outcome, so every rule
//! below unit-tests without Discord or Postgres.
//!
//! Timestamps reuse the shared [`crate::funnel`] ISO-8601 UTC helpers, so row
//! bytes match legacy `toISOString()` exactly.
//!
//! Three containment rules from migration 0004 travel with this port and must
//! stay true: no gateway presence intent, guild-level aggregates only (never a
//! per-member row), and the series is never published (no `web_v1` view may
//! read `presence_probe`). The only reader is an operator trend report.
//!
//! Source files (legacy `two-bot`):
//! - `src/jobs/presenceProbe.ts` (`PRESENCE_PROBE_INTERVAL_MS`,
//!   `BOT_FLOOR_MAX_AGE_MS`, `runProbeCycle`)
//! - `src/analytics/presence.ts` (`REOPEN_*`, `dailyPeaks`, `latestBotFloor`,
//!   `evaluateTrigger`)
//! - `migrations/0004_presence_probe.sql` (table, documented in 0310)

use std::collections::HashMap;

use super::funnel::format_iso_millis;
#[cfg(test)]
use super::funnel::parse_iso_millis;

/// Hourly collection cadence (legacy `PRESENCE_PROBE_INTERVAL_MS`).
///
/// A trend instrument, not a live counter: presence moves over an evening and
/// sampling every minute would buy nothing while putting 60x the Discord calls
/// behind a number nobody sees.
pub const PRESENCE_PROBE_INTERVAL_MS: u64 = 60 * 60 * 1000;

/// Bot-floor re-list ceiling (legacy `BOT_FLOOR_MAX_AGE_MS`).
///
/// The bot roster changes a few times a year; re-listing every member every
/// hour would page 100+ member objects to re-derive a number that did not
/// move. Once a day is far more often than the floor actually drifts.
pub const BOT_FLOOR_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;

/// Overlap-lease expiry for one probe cycle (TOG-12142).
///
/// A cycle slower than this is presumed dead: a later trigger takes over the
/// lease instead of queueing behind a stuck run. Set well above the 120 s job
/// supervisor timeout (no legitimate cycle lives that long) and well below
/// the hourly cadence, so a wedged holder self-heals within one tick.
pub const PRESENCE_PROBE_LEASE_MS: u64 = 30 * 60 * 1000;

/// Reopen threshold (legacy `REOPEN_PEAK_THRESHOLD`): raw
/// `approximate_presence_count` peaks at or above this argue roughly 20+
/// humans online at once against a ~23 bot floor. Compare to a raw reading,
/// never to a human estimate.
pub const REOPEN_PEAK_THRESHOLD: i64 = 45;
/// "Sustains", made decidable (legacy `REOPEN_REQUIRED_DAYS`): three separate
/// days inside a fortnight cannot be satisfied by one unusual night.
pub const REOPEN_REQUIRED_DAYS: usize = 3;
/// Trigger window (legacy `REOPEN_WINDOW_DAYS`).
pub const REOPEN_WINDOW_DAYS: i64 = 14;
/// Thin-window guard (legacy `MIN_DAYS_FOR_A_VERDICT`): below this many
/// observed days the instrument declines to answer rather than manufacturing
/// the "one reading, standing decision" problem it was built to fix.
pub const MIN_DAYS_FOR_A_VERDICT: usize = 7;

/// One stored reading (legacy `PresenceReading`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresenceReading {
    /// Epoch millis of the observation.
    pub observed_at_ms: i64,
    /// Discord's `approximate_presence_count`. Includes bots (known defect —
    /// why nothing may render this without a verified floor).
    pub presence: i64,
    /// Members with `user.bot` true, or `None` when this cycle did not
    /// rescan (the common case — most rows carry `NULL`).
    pub bot_floor: Option<i64>,
}

/// A completed bot-floor scan never exposes a partial count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotFloorScan {
    Complete(i64),
    Truncated,
}

/// A failed presence read writes NOTHING — not a row with a null count, not a
/// zero. The series must read as "the times we successfully looked", or a gap
/// in the collector becomes indistinguishable from a quiet night (legacy
/// `runProbeCycle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeDecision {
    /// Discord answered with a usable count: persist this row.
    Record {
        presence: i64,
        /// `Some` only when the bot floor was due for a re-list (or has never
        /// been observed); `None` means "not rescanned", the normal state.
        /// A failed member listing must not lose the presence reading, so a
        /// rescan failure also lands here as `None`.
        bot_floor: Option<i64>,
        /// A bounded scan exhausted its page budget; the 24 h cadence still applies.
        bot_floor_scan_truncated: bool,
    },
    /// Discord did not answer with a usable number: persist nothing.
    Skip,
}

/// Validate a raw presence count (legacy `fetchPresenceCount`): finite,
/// non-negative, truncated. `None` means "we did not read it".
#[must_use]
pub fn sanitize_presence_count(raw: Option<i64>) -> Option<i64> {
    raw.filter(|n| *n >= 0)
}

/// Decide whether the bot floor needs a re-list (legacy `runProbeCycle`):
/// rescan when no complete or truncated scan was ever recorded, or the newest
/// outcome has aged out (`now - last >= max_age`, the `>=` boundary included).
#[must_use]
pub fn bot_floor_due(last_scan_at_ms: Option<i64>, now_ms: i64, max_age_ms: u64) -> bool {
    match last_scan_at_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= max_age_ms as i64,
    }
}

/// One collection cycle as a pure decision (legacy `runProbeCycle` minus the
/// REST/DB seams): failed presence → [`ProbeDecision::Skip`]; otherwise
/// record, rescanning the floor only when [`bot_floor_due`].
///
/// `fresh_bot_floor` is the just-completed scan outcome, or `None` when
/// no rescan was attempted or the listing failed. It is consulted only when a
/// rescan is due, so a stale outcome from an earlier cycle can never leak into
/// a "not rescanned" row. Truncation records no floor but consumes the cadence.
#[must_use]
pub fn decide_probe_cycle(
    presence: Option<i64>,
    last_scan_at_ms: Option<i64>,
    fresh_bot_floor: Option<BotFloorScan>,
    now_ms: i64,
) -> ProbeDecision {
    let count = match sanitize_presence_count(presence) {
        Some(n) => n,
        None => return ProbeDecision::Skip,
    };
    let (bot_floor, bot_floor_scan_truncated) =
        if bot_floor_due(last_scan_at_ms, now_ms, BOT_FLOOR_MAX_AGE_MS) {
            match fresh_bot_floor {
                Some(BotFloorScan::Complete(n)) if n >= 0 => (Some(n), false),
                Some(BotFloorScan::Truncated) => (None, true),
                _ => (None, false),
            }
        } else {
            (None, false)
        };
    ProbeDecision::Record {
        presence: count,
        bot_floor,
        bot_floor_scan_truncated,
    }
}

/// One probe cycle's overlap lease (TOG-12142): the `started_at_ms` of the
/// cycle currently holding the probe, or `None` when no cycle is in flight.
/// The caller holds this in memory; a restart starts unleased, never inheriting
/// a dead process's claim.
pub type ProbeLease = Option<i64>;

/// Overlap verdict for one trigger: run this cycle, skip it, or take over a
/// stale holder's lease. Pure in `now_ms` so tests drive it with a fake clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeLeaseDecision {
    /// No cycle in flight: take the lease (`started_at_ms = now_ms`) and run.
    Run,
    /// A recent cycle is still in flight: skip this trigger, log and return.
    Skip,
    /// The holder started at or before `now - lease_ms` and is presumed dead:
    /// take over the lease (`started_at_ms = now_ms`) and run.
    Takeover,
}

/// Decide whether this trigger runs (TOG-12142).
///
/// `None` is always runnable — a finished cycle clears the lease, so the next
/// trigger starts clean. `Some(started)` skips while the holder is fresh
/// (`now - started < lease_ms`, the `>=` boundary takes over), which keeps a
/// slow run from duplicating the daily roster scan while letting a wedged
/// holder self-heal without operator action.
#[must_use]
pub fn decide_probe_lease(lease: ProbeLease, now_ms: i64, lease_ms: u64) -> ProbeLeaseDecision {
    match lease {
        None => ProbeLeaseDecision::Run,
        Some(started) => {
            if now_ms.saturating_sub(started) >= lease_ms as i64 {
                ProbeLeaseDecision::Takeover
            } else {
                ProbeLeaseDecision::Skip
            }
        }
    }
}

/// Outcome of one trigger evaluation (legacy `TriggerStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerStatus {
    /// Not enough of a series yet to say anything.
    InsufficientData,
    /// We looked, and presence is not close. Option C stands, now on evidence.
    Closed,
    /// The numbers qualify but `web_v1` is not live, so the other half fails.
    Armed,
    /// Both halves hold. Reopen option A.
    Fires,
}

impl TriggerStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InsufficientData => "insufficient_data",
            Self::Closed => "closed",
            Self::Armed => "armed",
            Self::Fires => "fires",
        }
    }
}

/// One UTC day's peak (legacy `DailyPeak`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyPeak {
    /// UTC date, `YYYY-MM-DD`.
    pub date: String,
    pub peak: i64,
    pub low: i64,
    pub readings: usize,
    /// True when this day's peak reached the threshold.
    pub qualifies: bool,
}

/// Group readings into UTC days, oldest first (legacy `dailyPeaks`).
///
/// Daily PEAK rather than daily mean is deliberate: the question is whether
/// the community is ever busy enough to be worth a headline, and a 24h
/// average is dominated by the small hours in every timezone at once.
#[must_use]
pub fn daily_peaks(readings: &[PresenceReading], threshold: i64) -> Vec<DailyPeak> {
    let mut by_day: HashMap<String, (i64, i64, usize)> = HashMap::new();
    for r in readings {
        let day = format_iso_millis(r.observed_at_ms)[..10].to_owned();
        by_day
            .entry(day)
            .and_modify(|v| {
                v.0 = v.0.max(r.presence);
                v.1 = v.1.min(r.presence);
                v.2 += 1;
            })
            .or_insert((r.presence, r.presence, 1));
    }
    let mut days: Vec<DailyPeak> = by_day
        .into_iter()
        .map(|(date, (peak, low, readings))| DailyPeak {
            date,
            peak,
            low,
            readings,
            qualifies: peak >= threshold,
        })
        .collect();
    days.sort_by(|a, b| a.date.cmp(&b.date));
    days
}

/// The most recent bot floor actually observed, or `None` (legacy
/// `latestBotFloor`). Taken from the WHOLE series, not the window: the floor
/// is a slow property of the server, and the newest known value is the best
/// answer even if this fortnight happened not to rescan.
#[must_use]
pub fn latest_bot_floor(readings: &[PresenceReading]) -> Option<i64> {
    readings
        .iter()
        .filter(|r| r.bot_floor.is_some())
        .max_by_key(|r| r.observed_at_ms)
        .and_then(|r| r.bot_floor)
}

/// Trigger tuning (legacy `TriggerOptions` overrides).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerOptions {
    pub threshold: i64,
    pub required_days: usize,
    pub window_days: i64,
    pub min_days: usize,
    /// Whether the website is actually serving. Nothing in the bot can
    /// observe that, so a human supplies it; defaults to false so the
    /// trigger cannot fire by accident on a number alone.
    pub web_v1_live: bool,
}

impl Default for TriggerOptions {
    fn default() -> Self {
        Self {
            threshold: REOPEN_PEAK_THRESHOLD,
            required_days: REOPEN_REQUIRED_DAYS,
            window_days: REOPEN_WINDOW_DAYS,
            min_days: MIN_DAYS_FOR_A_VERDICT,
            web_v1_live: false,
        }
    }
}

/// Trigger verdict (legacy `TriggerVerdict`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerVerdict {
    pub status: TriggerStatus,
    pub qualifying_days: usize,
    pub required_days: usize,
    pub threshold: i64,
    pub window_days: i64,
    pub days_observed: usize,
    pub readings_in_window: usize,
    pub peak: Option<i64>,
    pub peak_at_ms: Option<i64>,
    pub bot_floor: Option<i64>,
    pub web_v1_live: bool,
    /// One sentence, safe to paste into an issue comment.
    pub reason: String,
}

/// Decide whether the reopen condition is met (legacy `evaluateTrigger`).
///
/// Both halves must hold: `web_v1` live AND presence sustaining peaks at or
/// above the threshold. `Armed` when only the numbers qualify keeps those two
/// failures distinguishable — they need completely different follow-ups.
#[must_use]
pub fn evaluate_trigger(
    readings: &[PresenceReading],
    now_ms: i64,
    opts: TriggerOptions,
) -> TriggerVerdict {
    let cutoff = now_ms - opts.window_days * 86_400_000;
    let window: Vec<&PresenceReading> = readings
        .iter()
        .filter(|r| r.observed_at_ms >= cutoff && r.observed_at_ms <= now_ms)
        .collect();

    let owned: Vec<PresenceReading> = window.iter().map(|r| **r).collect();
    let days = daily_peaks(&owned, opts.threshold);
    let qualifying_days = days.iter().filter(|d| d.qualifies).count();

    let mut peak: Option<i64> = None;
    let mut peak_at_ms: Option<i64> = None;
    for r in &window {
        if peak.is_none_or(|p| r.presence > p) {
            peak = Some(r.presence);
            peak_at_ms = Some(r.observed_at_ms);
        }
    }

    let bot_floor = latest_bot_floor(readings);
    let floor_note = bot_floor.map_or(String::new(), |f| format!(", bot floor {f}"));

    let base = |status: TriggerStatus, reason: String| TriggerVerdict {
        status,
        qualifying_days,
        required_days: opts.required_days,
        threshold: opts.threshold,
        window_days: opts.window_days,
        days_observed: days.len(),
        readings_in_window: window.len(),
        peak,
        peak_at_ms,
        bot_floor,
        web_v1_live: opts.web_v1_live,
        reason,
    };

    if days.len() < opts.min_days {
        return base(
            TriggerStatus::InsufficientData,
            format!(
                "Only {} of the {} days needed for a verdict have readings in the trailing {} days. Not enough series to say anything yet.",
                days.len(),
                opts.min_days,
                opts.window_days
            ),
        );
    }
    if qualifying_days < opts.required_days {
        return base(
            TriggerStatus::Closed,
            format!(
                "{} of the required {} days peaked at >= {} in the trailing {} days (best reading {}{}). TOG-75 option C stands, now on a series rather than one reading.",
                qualifying_days,
                opts.required_days,
                opts.threshold,
                opts.window_days,
                peak.map_or("none".to_owned(), |p| p.to_string()),
                floor_note
            ),
        );
    }
    if !opts.web_v1_live {
        return base(
            TriggerStatus::Armed,
            format!(
                "Presence qualifies - {qualifying_days} days peaked at >= {} in the trailing {} days - but the trigger also requires web_v1 to be live, which was not asserted. No action yet; re-run with --web-live once it ships.",
                opts.threshold, opts.window_days
            ),
        );
    }
    base(
        TriggerStatus::Fires,
        format!(
            "Both halves hold: web_v1 is live and {qualifying_days} days peaked at >= {} in the trailing {} days (best {}{}). Reopen TOG-75 option A against the CISO's controls 1-5 - do not re-derive them.",
            opts.threshold,
            opts.window_days,
            peak.map_or("none".to_owned(), |p| p.to_string()),
            floor_note
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;

    fn ms(iso: &str) -> i64 {
        parse_iso_millis(iso).expect("fixture timestamp parses")
    }

    #[test]
    fn cadence_constants_match_legacy() {
        assert_eq!(PRESENCE_PROBE_INTERVAL_MS, 3_600_000);
        assert_eq!(BOT_FLOOR_MAX_AGE_MS, 86_400_000);
    }

    #[test]
    fn shared_iso_helpers_round_trip() {
        // The shared funnel helpers own the format; the probe only slices it.
        assert_eq!(
            format_iso_millis(ms("2026-09-07T06:15:00.000Z")),
            "2026-09-07T06:15:00.000Z"
        );
        assert_eq!(
            format_iso_millis(ms("2026-09-07T06:15:00.000Z"))[..10],
            *"2026-09-07"
        );
    }

    #[test]
    fn bot_floor_due_matches_legacy_boundary() {
        let now = ms("2026-09-07T06:15:00.000Z");
        // Never observed: rescan.
        assert!(bot_floor_due(None, now, BOT_FLOOR_MAX_AGE_MS));
        // Exactly 24h old: aged out (>= boundary rescans).
        assert!(bot_floor_due(
            Some(now - 86_400_000),
            now,
            BOT_FLOOR_MAX_AGE_MS
        ));
        // One millisecond under: keep the cached floor.
        assert!(!bot_floor_due(
            Some(now - 86_400_000 + 1),
            now,
            BOT_FLOOR_MAX_AGE_MS
        ));
    }

    #[test]
    fn failed_presence_read_writes_nothing() {
        // Null count: skip, no row, no floor side effects.
        assert_eq!(
            decide_probe_cycle(None, None, Some(BotFloorScan::Complete(23)), 1_000),
            ProbeDecision::Skip
        );
        // Negative count: same as unreadable.
        assert_eq!(
            decide_probe_cycle(Some(-1), None, Some(BotFloorScan::Complete(23)), 1_000),
            ProbeDecision::Skip
        );
    }

    #[test]
    fn successful_cycle_rescans_only_when_due() {
        let now = ms("2026-09-07T06:15:00.000Z");
        // Due: fresh floor lands on the row.
        assert_eq!(
            decide_probe_cycle(Some(42), None, Some(BotFloorScan::Complete(23)), now),
            ProbeDecision::Record {
                presence: 42,
                bot_floor: Some(23),
                bot_floor_scan_truncated: false,
            }
        );
        // Due but the listing failed: presence is kept, floor stays NULL
        // ("not rescanned", the normal state of most rows).
        assert_eq!(
            decide_probe_cycle(Some(42), None, None, now),
            ProbeDecision::Record {
                presence: 42,
                bot_floor: None,
                bot_floor_scan_truncated: false,
            }
        );
        // Not due: a stale fresh count must never leak into the row.
        assert_eq!(
            decide_probe_cycle(
                Some(42),
                Some(now - H),
                Some(BotFloorScan::Complete(99)),
                now
            ),
            ProbeDecision::Record {
                presence: 42,
                bot_floor: None,
                bot_floor_scan_truncated: false,
            }
        );
    }

    #[test]
    fn truncated_cycle_records_no_floor_and_obeys_scan_cadence() {
        let now = ms("2026-09-07T06:15:00.000Z");
        assert_eq!(
            decide_probe_cycle(Some(42), None, Some(BotFloorScan::Truncated), now),
            ProbeDecision::Record {
                presence: 42,
                bot_floor: None,
                bot_floor_scan_truncated: true,
            }
        );
        // An unattempted hourly tick cannot repeat the truncation evidence.
        assert_eq!(
            decide_probe_cycle(Some(43), Some(now), Some(BotFloorScan::Truncated), now + H),
            ProbeDecision::Record {
                presence: 43,
                bot_floor: None,
                bot_floor_scan_truncated: false,
            }
        );
        assert!(!bot_floor_due(
            Some(now),
            now + 24 * H - 1,
            BOT_FLOOR_MAX_AGE_MS
        ));
        assert!(bot_floor_due(Some(now), now + 24 * H, BOT_FLOOR_MAX_AGE_MS));
        assert_eq!(
            decide_probe_cycle(None, None, Some(BotFloorScan::Truncated), now),
            ProbeDecision::Skip
        );
        assert_eq!(
            decide_probe_cycle(Some(42), None, Some(BotFloorScan::Complete(-1)), now),
            ProbeDecision::Record {
                presence: 42,
                bot_floor: None,
                bot_floor_scan_truncated: false,
            }
        );
    }

    fn reading(day_offset: i64, presence: i64, floor: Option<i64>) -> PresenceReading {
        PresenceReading {
            observed_at_ms: ms("2026-09-01T12:00:00.000Z") + day_offset * 86_400_000,
            presence,
            bot_floor: floor,
        }
    }

    #[test]
    fn trigger_declines_thin_windows() {
        let readings = vec![reading(0, 60, Some(23)), reading(1, 61, None)];
        let verdict = evaluate_trigger(
            &readings,
            ms("2026-09-03T00:00:00.000Z"),
            TriggerOptions::default(),
        );
        assert_eq!(verdict.status, TriggerStatus::InsufficientData);
        assert_eq!(verdict.days_observed, 2);
    }

    #[test]
    fn trigger_closed_when_peaks_do_not_sustain() {
        let readings: Vec<_> = (0..10)
            .map(|d| reading(d, if d == 3 { 50 } else { 30 }, (d == 0).then_some(23)))
            .collect();
        let verdict = evaluate_trigger(
            &readings,
            ms("2026-09-12T00:00:00.000Z"),
            TriggerOptions::default(),
        );
        assert_eq!(verdict.status, TriggerStatus::Closed);
        assert_eq!(verdict.qualifying_days, 1);
        // Floor comes from the whole series, not the window.
        assert_eq!(verdict.bot_floor, Some(23));
        assert!(verdict.reason.contains("option C stands"));
    }

    #[test]
    fn trigger_armed_without_web_v1_and_fires_with_it() {
        let readings: Vec<_> = (0..10)
            .map(|d| reading(d, if d % 3 == 0 { 45 } else { 30 }, None))
            .collect();
        let now = ms("2026-09-12T00:00:00.000Z");
        let armed = evaluate_trigger(&readings, now, TriggerOptions::default());
        assert_eq!(armed.status, TriggerStatus::Armed);
        assert_eq!(armed.qualifying_days, 4);
        let fires = evaluate_trigger(
            &readings,
            now,
            TriggerOptions {
                web_v1_live: true,
                ..TriggerOptions::default()
            },
        );
        assert_eq!(fires.status, TriggerStatus::Fires);
        assert!(fires.reason.contains("Both halves hold"));
    }

    #[test]
    fn overlap_lease_skips_concurrent_cycle_and_heals_stale_holder() {
        let now = ms("2026-09-07T06:15:00.000Z");
        let lease_ms = PRESENCE_PROBE_LEASE_MS;
        // No cycle in flight: take the lease and run.
        assert_eq!(
            decide_probe_lease(None, now, lease_ms),
            ProbeLeaseDecision::Run
        );
        // A fresh holder blocks the next trigger: skip, no roster rescan.
        assert_eq!(
            decide_probe_lease(Some(now), now, lease_ms),
            ProbeLeaseDecision::Skip
        );
        assert_eq!(
            decide_probe_lease(Some(now), now + lease_ms as i64 - 1, lease_ms),
            ProbeLeaseDecision::Skip
        );
        // `>=` boundary takes over: a cycle slower than the lease is dead.
        assert_eq!(
            decide_probe_lease(Some(now), now + lease_ms as i64, lease_ms),
            ProbeLeaseDecision::Takeover
        );
        assert_eq!(
            decide_probe_lease(Some(now), now + lease_ms as i64 + 1, lease_ms),
            ProbeLeaseDecision::Takeover
        );
        // A stale holder's lease never leaks into the next decision: after
        // takeover the new holder's own start governs the following trigger.
        let taken_over = now + lease_ms as i64;
        assert_eq!(
            decide_probe_lease(Some(taken_over), taken_over + H, lease_ms),
            ProbeLeaseDecision::Skip
        );
    }

    #[test]
    fn daily_peaks_use_max_not_mean() {
        let base = ms("2026-09-01T00:00:00.000Z");
        let readings = vec![
            PresenceReading {
                observed_at_ms: base,
                presence: 10,
                bot_floor: None,
            },
            PresenceReading {
                observed_at_ms: base + 12 * H,
                presence: 50,
                bot_floor: None,
            },
        ];
        let days = daily_peaks(&readings, REOPEN_PEAK_THRESHOLD);
        assert_eq!(days.len(), 1);
        assert_eq!((days[0].peak, days[0].low, days[0].readings), (50, 10, 2));
        assert!(days[0].qualifies);
    }
}
