//! Presence probe-cycle and reopen-trigger acceptance.
//!
//! Pins the public `two_bot_core::presence` contract through the public API
//! only: hourly cadence, 24 h bot-floor max age, probe lease guard,
//! `daily_peaks` / `latest_bot_floor` derivation, and the
//! 45-peak / 3-day / 14-day-window / 7-day-verdict reopen contract.
//!
//! Pure and offline: no Discord client, no SQL, no network. Guild aggregates
//! only (never per-member rows); the series is never published.

use two_bot_core::presence::{
    bot_floor_due, daily_peaks, decide_probe_cycle, decide_probe_lease, evaluate_trigger,
    latest_bot_floor, release_probe_lease, sanitize_presence_count, BotFloorScan, PresenceReading,
    ProbeDecision, ProbeLeaseDecision, TriggerOptions, TriggerStatus, BOT_FLOOR_MAX_AGE_MS,
    MIN_DAYS_FOR_A_VERDICT, PRESENCE_PROBE_INTERVAL_MS, PRESENCE_PROBE_LEASE_MS,
    REOPEN_PEAK_THRESHOLD, REOPEN_REQUIRED_DAYS, REOPEN_WINDOW_DAYS,
};

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
// Fixed clock so UTC-day grouping is deterministic across runs.
const NOW: i64 = 1_750_000_000_000;

fn reading(observed_at_ms: i64, presence: i64, bot_floor: Option<i64>) -> PresenceReading {
    PresenceReading {
        observed_at_ms,
        presence,
        bot_floor,
    }
}

fn days_ago(days: i64, presence: i64, floor: Option<i64>) -> PresenceReading {
    reading(NOW - days * DAY_MS, presence, floor)
}

#[test]
fn probe_cadence_constants_match_spec() {
    assert_eq!(
        PRESENCE_PROBE_INTERVAL_MS, 3_600_000,
        "hourly probe cadence"
    );
    assert_eq!(BOT_FLOOR_MAX_AGE_MS, 86_400_000, "24 h bot-floor max age");
    assert_eq!(
        PRESENCE_PROBE_LEASE_MS,
        30 * 60 * 1000,
        "overlap-lease expiry"
    );
    assert_eq!(REOPEN_PEAK_THRESHOLD, 45, "raw presence peak threshold");
    assert_eq!(REOPEN_REQUIRED_DAYS, 3, "sustained days required");
    assert_eq!(REOPEN_WINDOW_DAYS, 14, "trigger window days");
    assert_eq!(MIN_DAYS_FOR_A_VERDICT, 7, "thin-window guard days observed");
    let defaults = TriggerOptions::default();
    assert_eq!(defaults.threshold, 45);
    assert_eq!(defaults.required_days, 3);
    assert_eq!(defaults.window_days, 14);
    assert_eq!(defaults.min_days, 7);
    assert!(
        !defaults.web_v1_live,
        "trigger never fires on numbers alone"
    );
}

#[test]
fn bot_floor_due_honors_24h_max_age() {
    // Never observed: rescan.
    assert!(bot_floor_due(None, NOW, BOT_FLOOR_MAX_AGE_MS));
    // Exactly 24 h old: aged out (>= boundary rescans).
    assert!(bot_floor_due(
        Some(NOW - 86_400_000),
        NOW,
        BOT_FLOOR_MAX_AGE_MS
    ));
    // One millisecond under: keep the cached floor.
    assert!(!bot_floor_due(
        Some(NOW - 86_400_000 + 1),
        NOW,
        BOT_FLOOR_MAX_AGE_MS
    ));
    // Fresh hourly tick: not due.
    assert!(!bot_floor_due(
        Some(NOW - HOUR_MS),
        NOW,
        BOT_FLOOR_MAX_AGE_MS
    ));
}

#[test]
fn decide_probe_cycle_honors_hourly_cadence() {
    // Failed presence reads write nothing.
    assert_eq!(
        decide_probe_cycle(None, None, Some(BotFloorScan::Complete(23)), NOW),
        ProbeDecision::Skip
    );
    assert_eq!(
        decide_probe_cycle(Some(-1), None, Some(BotFloorScan::Complete(23)), NOW),
        ProbeDecision::Skip
    );

    // Due (never scanned): fresh floor lands on the row.
    assert_eq!(
        decide_probe_cycle(Some(42), None, Some(BotFloorScan::Complete(23)), NOW),
        ProbeDecision::Record {
            presence: 42,
            bot_floor: Some(23),
            bot_floor_scan_truncated: false,
        }
    );
    // Due but the member listing failed: presence is kept, floor stays NULL
    // ("not rescanned", the normal state of most rows).
    assert_eq!(
        decide_probe_cycle(Some(42), None, None, NOW),
        ProbeDecision::Record {
            presence: 42,
            bot_floor: None,
            bot_floor_scan_truncated: false,
        }
    );
    // Not due (one hourly tick ago): a stale fresh count must never leak in.
    assert_eq!(
        decide_probe_cycle(
            Some(42),
            Some(NOW - HOUR_MS),
            Some(BotFloorScan::Complete(99)),
            NOW
        ),
        ProbeDecision::Record {
            presence: 42,
            bot_floor: None,
            bot_floor_scan_truncated: false,
        }
    );
    // A partial scan never exposes a partial count, but consumes the cadence.
    assert_eq!(
        decide_probe_cycle(Some(42), None, Some(BotFloorScan::Truncated), NOW),
        ProbeDecision::Record {
            presence: 42,
            bot_floor: None,
            bot_floor_scan_truncated: true,
        }
    );
    // An unattempted hourly tick cannot repeat the truncation evidence.
    assert_eq!(
        decide_probe_cycle(
            Some(43),
            Some(NOW),
            Some(BotFloorScan::Truncated),
            NOW + HOUR_MS
        ),
        ProbeDecision::Record {
            presence: 43,
            bot_floor: None,
            bot_floor_scan_truncated: false,
        }
    );
    // A negative floor scan is not a complete scan.
    assert_eq!(
        decide_probe_cycle(Some(42), None, Some(BotFloorScan::Complete(-1)), NOW),
        ProbeDecision::Record {
            presence: 42,
            bot_floor: None,
            bot_floor_scan_truncated: false,
        }
    );
}

#[test]
fn decide_probe_lease_refuses_overlapping_cycles() {
    let lease_ms = PRESENCE_PROBE_LEASE_MS;
    // No cycle in flight: take the lease and run.
    assert_eq!(
        decide_probe_lease(None, NOW, lease_ms),
        ProbeLeaseDecision::Run
    );
    // A fresh holder blocks the next trigger: skip, no roster rescan.
    assert_eq!(
        decide_probe_lease(Some(NOW), NOW, lease_ms),
        ProbeLeaseDecision::Skip
    );
    assert_eq!(
        decide_probe_lease(Some(NOW), NOW + lease_ms as i64 - 1, lease_ms),
        ProbeLeaseDecision::Skip
    );
    // `>=` boundary takes over: a cycle slower than the lease is presumed dead.
    assert_eq!(
        decide_probe_lease(Some(NOW), NOW + lease_ms as i64, lease_ms),
        ProbeLeaseDecision::Takeover
    );
    assert_eq!(
        decide_probe_lease(Some(NOW), NOW + lease_ms as i64 + 1, lease_ms),
        ProbeLeaseDecision::Takeover
    );
    // After takeover the new holder's own start governs the next trigger.
    let taken_over = NOW + lease_ms as i64;
    assert_eq!(
        decide_probe_lease(Some(taken_over), taken_over + 60_000, lease_ms),
        ProbeLeaseDecision::Skip
    );
    assert_eq!(
        decide_probe_lease(Some(taken_over), taken_over + lease_ms as i64, lease_ms),
        ProbeLeaseDecision::Takeover
    );
}

#[test]
fn release_probe_lease_frees_only_own_claim() {
    let successor = NOW + PRESENCE_PROBE_LEASE_MS as i64;
    // The finishing holder clears its own claim.
    assert_eq!(release_probe_lease(Some(NOW), NOW), None);
    // A taken-over holder finishing late keeps its successor's lease.
    assert_eq!(release_probe_lease(Some(successor), NOW), Some(successor));
    // Releasing an already-free lease stays free.
    assert_eq!(release_probe_lease(None, NOW), None);
}

#[test]
fn sanitize_presence_count_refuses_negative_and_missing() {
    assert_eq!(sanitize_presence_count(None), None);
    assert_eq!(sanitize_presence_count(Some(-1)), None);
    assert_eq!(sanitize_presence_count(Some(-999_999)), None);
    assert_eq!(sanitize_presence_count(Some(i64::MIN)), None);
    // Zero and ordinary guild aggregates pass through unchanged.
    assert_eq!(sanitize_presence_count(Some(0)), Some(0));
    assert_eq!(sanitize_presence_count(Some(1)), Some(1));
    assert_eq!(sanitize_presence_count(Some(45)), Some(45));
    assert_eq!(sanitize_presence_count(Some(10_000)), Some(10_000));
    // No upper-bound clamp in the port: absurd-large passes through and the
    // 45-peak trigger threshold is the guard, not the sanitizer.
    assert_eq!(sanitize_presence_count(Some(i64::MAX)), Some(i64::MAX));
}

#[test]
fn daily_peaks_derive_peaks_lows_and_qualifying_flag() {
    // Two readings on the same UTC day: peak is the max, never the mean.
    let day_start = NOW.div_euclid(DAY_MS) * DAY_MS;
    let same_day = vec![
        reading(day_start + HOUR_MS, 10, None),
        reading(day_start + 13 * HOUR_MS, 50, None),
    ];
    let days = daily_peaks(&same_day, REOPEN_PEAK_THRESHOLD);
    assert_eq!(days.len(), 1);
    assert_eq!((days[0].peak, days[0].low, days[0].readings), (50, 10, 2));
    assert!(days[0].qualifies, "peak 50 >= threshold 45");

    // Threshold boundary: exactly 45 qualifies, 44 does not.
    let boundary = vec![days_ago(0, 45, None), days_ago(1, 44, None)];
    let peaks = daily_peaks(&boundary, REOPEN_PEAK_THRESHOLD);
    assert_eq!(peaks.len(), 2);
    // Oldest first.
    assert!(peaks[0].date < peaks[1].date);
    let low_day = peaks.iter().find(|d| d.peak == 44).expect("44-peak day");
    let high_day = peaks.iter().find(|d| d.peak == 45).expect("45-peak day");
    assert!(!low_day.qualifies);
    assert!(high_day.qualifies);
}

#[test]
fn latest_bot_floor_uses_newest_observed_floor() {
    // Newest complete scan wins; rows without a rescan are skipped.
    let readings = vec![
        reading(NOW - 3 * DAY_MS, 40, Some(23)),
        reading(NOW - 2 * DAY_MS, 41, None),
        reading(NOW - DAY_MS, 42, Some(24)),
    ];
    assert_eq!(latest_bot_floor(&readings), Some(24));

    // Only the newest floor matters even when an older row has one.
    let only_old = vec![
        reading(NOW - 3 * DAY_MS, 40, Some(23)),
        reading(NOW - DAY_MS, 42, None),
    ];
    assert_eq!(latest_bot_floor(&only_old), Some(23));

    // Never rescanned: no floor.
    let never: Vec<PresenceReading> = vec![reading(NOW - DAY_MS, 40, None), reading(NOW, 41, None)];
    assert_eq!(latest_bot_floor(&never), None);
    assert_eq!(latest_bot_floor(&[]), None);
}

#[test]
fn evaluate_trigger_requires_seven_day_verdict_window() {
    // Two observed days: declines to answer.
    let thin: Vec<PresenceReading> = (0..2).map(|d| days_ago(d, 60, None)).collect();
    let verdict = evaluate_trigger(&thin, NOW, TriggerOptions::default());
    assert_eq!(verdict.status, TriggerStatus::InsufficientData);
    assert_eq!(verdict.days_observed, 2);

    // Six observed days: still too thin, even with qualifying peaks.
    let six: Vec<PresenceReading> = (0..6).map(|d| days_ago(d, 60, None)).collect();
    let verdict = evaluate_trigger(&six, NOW, TriggerOptions::default());
    assert_eq!(verdict.status, TriggerStatus::InsufficientData);
    assert_eq!(verdict.days_observed, 6);

    // Seven observed days with no qualifying peak: a verdict (Closed), not a
    // refusal. The 7-day gate is about series length, not loudness.
    let seven_quiet: Vec<PresenceReading> = (0..7).map(|d| days_ago(d, 30, None)).collect();
    let verdict = evaluate_trigger(&seven_quiet, NOW, TriggerOptions::default());
    assert_eq!(verdict.status, TriggerStatus::Closed);
    assert_eq!(verdict.days_observed, 7);
    assert_eq!(verdict.qualifying_days, 0);
}

#[test]
fn evaluate_trigger_requires_three_sustained_peaks() {
    // Ten observed days, one loud night: Closed, option C stands.
    let one_loud: Vec<PresenceReading> = (0..10)
        .map(|d| days_ago(d, if d == 3 { 50 } else { 30 }, None))
        .collect();
    let verdict = evaluate_trigger(&one_loud, NOW, TriggerOptions::default());
    assert_eq!(verdict.status, TriggerStatus::Closed);
    assert_eq!(verdict.qualifying_days, 1);
    assert_eq!(verdict.required_days, 3);
    assert_eq!(verdict.threshold, 45);

    // Two qualifying days: still Closed.
    let two_loud: Vec<PresenceReading> = (0..10)
        .map(|d| days_ago(d, if d < 2 { 45 } else { 30 }, None))
        .collect();
    let verdict = evaluate_trigger(&two_loud, NOW, TriggerOptions::default());
    assert_eq!(verdict.status, TriggerStatus::Closed);
    assert_eq!(verdict.qualifying_days, 2);

    // Exactly the 45 boundary counts: three days at 45 qualify.
    let boundary: Vec<PresenceReading> = (0..10)
        .map(|d| {
            let presence = if d < 3 {
                45
            } else if d == 9 {
                44
            } else {
                30
            };
            days_ago(d, presence, None)
        })
        .collect();
    let verdict = evaluate_trigger(&boundary, NOW, TriggerOptions::default());
    assert_eq!(verdict.qualifying_days, 3, "44 must not qualify");
    assert_eq!(verdict.status, TriggerStatus::Armed);
}

#[test]
fn evaluate_trigger_requires_14_day_window() {
    // Three loud nights, but two are older than the 14-day window: Closed.
    let mut stale: Vec<PresenceReading> = (0..10).map(|d| days_ago(d, 30, None)).collect();
    stale.push(days_ago(0, 60, None));
    stale.push(days_ago(15, 60, None));
    stale.push(days_ago(20, 60, None));
    let verdict = evaluate_trigger(&stale, NOW, TriggerOptions::default());
    assert_eq!(verdict.status, TriggerStatus::Closed);
    assert_eq!(verdict.qualifying_days, 1);
    assert_eq!(verdict.window_days, 14);
    assert_eq!(verdict.readings_in_window, 11);

    // Future readings never count toward the verdict.
    let mut future: Vec<PresenceReading> = (0..10).map(|d| days_ago(d, 30, None)).collect();
    future.push(reading(NOW + HOUR_MS, 99, None));
    future.push(reading(NOW + DAY_MS, 99, None));
    let verdict = evaluate_trigger(&future, NOW, TriggerOptions::default());
    assert_eq!(verdict.status, TriggerStatus::Closed);
    assert_eq!(verdict.qualifying_days, 0);

    // The cutoff day itself is inclusive: a loud reading exactly 14 days ago
    // counts when the series is otherwise long enough.
    let mut cutoff: Vec<PresenceReading> = (0..7).map(|d| days_ago(d, 30, None)).collect();
    cutoff.push(days_ago(14, 60, None));
    cutoff.push(days_ago(1, 60, None));
    cutoff.push(days_ago(2, 60, None));
    let verdict = evaluate_trigger(&cutoff, NOW, TriggerOptions::default());
    assert_eq!(verdict.qualifying_days, 3);
    assert_eq!(verdict.status, TriggerStatus::Armed);
}

#[test]
fn evaluate_trigger_stays_silent_without_web_v1_and_fires_with_it() {
    // Four qualifying days in a ten-day series: numbers hold.
    let readings: Vec<PresenceReading> = (0..10)
        .map(|d| days_ago(d, if d % 3 == 0 { 45 } else { 30 }, None))
        .collect();
    let armed = evaluate_trigger(&readings, NOW, TriggerOptions::default());
    assert_eq!(armed.status, TriggerStatus::Armed);
    assert_eq!(armed.qualifying_days, 4);
    assert!(!armed.web_v1_live);
    assert!(!armed.reason.is_empty());

    // Same numbers with web_v1 live: both halves hold, reopen.
    let fires = evaluate_trigger(
        &readings,
        NOW,
        TriggerOptions {
            web_v1_live: true,
            ..TriggerOptions::default()
        },
    );
    assert_eq!(fires.status, TriggerStatus::Fires);
    assert_eq!(fires.qualifying_days, 4);
    assert!(fires.web_v1_live);
    assert!(!fires.reason.is_empty());

    // Floor comes from the whole series, not the window, and rides along.
    let mut with_floor: Vec<PresenceReading> = (0..10)
        .map(|d| {
            days_ago(
                d,
                if d % 3 == 0 { 45 } else { 30 },
                if d == 9 { Some(23) } else { None },
            )
        })
        .collect();
    with_floor.push(days_ago(20, 50, Some(99)));
    let verdict = evaluate_trigger(
        &with_floor,
        NOW,
        TriggerOptions {
            web_v1_live: true,
            ..TriggerOptions::default()
        },
    );
    assert_eq!(verdict.status, TriggerStatus::Fires);
    // Newest observed floor wins even though the 20-day-old row is outside
    // the 14-day trigger window.
    assert_eq!(verdict.bot_floor, Some(23));
}
