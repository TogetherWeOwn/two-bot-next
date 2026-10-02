//! Historical-raid replay through `scan_joins_for_bursts`. Scenario shapes are
//! the legacy `test/unit.raidwatch.test.ts` cases at `two-bot@d5d11793`
//! (lines 74-130). Synthetic joins only: no database, gateway or guild.

use two_bot_core::{
    format_iso_millis, parse_iso_millis, scan_joins_for_bursts, HistoricalJoin, RaidAlert,
    RaidScanOptions, RaidTuning, DEFAULT_RAID_COOLDOWN_SECONDS, DEFAULT_RAID_MAX_IDS,
    DEFAULT_RAID_THRESHOLD, DEFAULT_RAID_WINDOW_SECONDS,
};

const G: &str = "326474832151838730";

fn join(guild_id: &str, member_id: &str, occurred_at: &str) -> HistoricalJoin {
    HistoricalJoin {
        guild_id: guild_id.into(),
        member_id: member_id.into(),
        occurred_at: occurred_at.into(),
    }
}

/// `n` joins from `start_iso`, one every `gap_seconds`, formatted like legacy
/// `new Date(start + i * gap * 1000).toISOString()` (fractional ms truncate).
fn burst(start_iso: &str, n: usize, gap_seconds: f64, prefix: &str) -> Vec<HistoricalJoin> {
    let start = parse_iso_millis(start_iso).unwrap();
    (0..n)
        .map(|i| {
            let offset = (i as f64 * gap_seconds * 1000.0).trunc() as i64;
            join(
                G,
                &format!("{prefix}{i}"),
                &format_iso_millis(start + offset),
            )
        })
        .collect()
}

fn scan(joins: &[HistoricalJoin], options: RaidScanOptions) -> Vec<RaidAlert> {
    scan_joins_for_bursts(joins, options).unwrap()
}

fn with_threshold(threshold: f64, cooldown_seconds: f64) -> RaidScanOptions {
    RaidScanOptions {
        tuning: RaidTuning::new(60.0, threshold).unwrap(),
        cooldown_seconds,
        ..RaidScanOptions::default()
    }
}

#[test]
fn defaults_match_the_raid_port_contract() {
    let options = RaidScanOptions::default();
    assert_eq!(options.tuning.window_seconds(), 60.0);
    assert_eq!(options.tuning.threshold(), 5.0);
    assert_eq!(options.cooldown_seconds, 900.0);
    assert_eq!(options.max_ids, 50);
    assert_eq!(
        (
            DEFAULT_RAID_WINDOW_SECONDS,
            DEFAULT_RAID_THRESHOLD,
            DEFAULT_RAID_COOLDOWN_SECONDS,
            DEFAULT_RAID_MAX_IDS
        ),
        (60.0, 5.0, 900.0, 50)
    );
}

#[test]
fn a_long_raid_alerts_on_a_cooldown_not_once_per_account() {
    // The 2025-07-06 shape: 1,015 accounts over 56 minutes, ~18 a minute.
    let joins = burst("2025-07-06T20:31:00Z", 1015, 56.0 * 60.0 / 1015.0, "m");
    let alerts = scan(
        &joins,
        RaidScanOptions {
            cooldown_seconds: 900.0,
            ..RaidScanOptions::default()
        },
    );
    assert!(
        (3..=6).contains(&alerts.len()),
        "got {} alerts",
        alerts.len()
    );
    assert!(!alerts[0].repeat);
    assert!(alerts[1..].iter().all(|alert| alert.repeat));
}

#[test]
fn both_small_raids_are_caught_on_the_fifth_account() {
    // 2025-09-12: 15 accounts in 6 seconds. 2025-12-15: 15 in 11 seconds.
    let raids = [
        ("2025-09-12T17:42:59Z", 6_u64),
        ("2025-12-15T21:16:49Z", 11),
    ];
    for (start, span) in raids {
        let alerts = scan(
            &burst(start, 15, span as f64 / 15.0, "m"),
            RaidScanOptions::default(),
        );
        assert_eq!(alerts.len(), 1, "{start}");
        assert_eq!(alerts[0].count, 5, "fires on the 5th account, not after 15");
        assert!(alerts[0].span_seconds <= span);
        assert!(!alerts[0].repeat);
    }

    // One replay over both: the cooldown has long expired, so each raid still
    // alerts once; the second is `repeat` because this watch alerted before.
    let mut both = Vec::new();
    for (prefix, (start, span)) in ["a", "b"].into_iter().zip(raids) {
        both.extend(burst(start, 15, span as f64 / 15.0, prefix));
    }
    let alerts = scan(&both, RaidScanOptions::default());
    assert_eq!(alerts.len(), 2);
    assert!(alerts.iter().all(|alert| alert.count == 5));
    assert_eq!((alerts[0].repeat, alerts[1].repeat), (false, true));
}

#[test]
fn the_busiest_genuine_month_would_not_have_alerted() {
    // 2023-04: 7 real joins in a month; even on one evening, minutes apart.
    let joins = burst("2023-04-14T19:00:00Z", 7, 8.0 * 60.0, "m");
    assert!(scan(&joins, RaidScanOptions::default()).is_empty());
}

#[test]
fn an_alert_past_the_id_cap_counts_everyone_and_lists_some() {
    // 400 accounts, ten a second. The first alert carries five IDs; the
    // follow-up after the 10 s cooldown must truncate at the 10-ID cap.
    let alerts = scan(
        &burst("2025-07-06T20:31:00Z", 400, 0.1, "m"),
        RaidScanOptions {
            max_ids: 10,
            cooldown_seconds: 10.0,
            ..RaidScanOptions::default()
        },
    );
    assert_eq!(alerts[0].member_ids.len(), 5);
    assert!(!alerts[0].truncated);

    let later = alerts
        .get(1)
        .expect("a sustained raid alerts again after the cooldown");
    assert!(later.repeat);
    assert_eq!(later.member_ids.len(), 10);
    assert!(later.truncated);
    assert!(later.count > 10);
}

#[test]
fn each_call_uses_a_fresh_watch() {
    let joins = burst("2025-09-12T17:42:59Z", 15, 0.4, "m");
    let first = scan(&joins, RaidScanOptions::default());
    let second = scan(&joins, RaidScanOptions::default());
    assert_eq!(first, second);
    assert!(
        !second[0].repeat,
        "no cooldown or repeat state carries over"
    );
}

#[test]
fn unsorted_input_replays_in_occurrence_order() {
    let sorted = burst("2025-07-06T20:31:00Z", 400, 0.1, "m");
    let options = RaidScanOptions {
        max_ids: 10,
        cooldown_seconds: 10.0,
        ..RaidScanOptions::default()
    };
    let mut reversed = sorted.clone();
    reversed.reverse();
    let mut interleaved: Vec<_> = sorted.iter().skip(1).step_by(2).cloned().collect();
    interleaved.extend(sorted.iter().step_by(2).cloned());
    let expected = scan(&sorted, options);
    assert_eq!(scan(&reversed, options), expected);
    assert_eq!(scan(&interleaved, options), expected);
}

#[test]
fn equal_instants_keep_input_order() {
    let joins: Vec<_> = ["e", "d", "c", "b", "a"]
        .into_iter()
        .map(|id| join(G, id, "2025-07-06T20:31:00.000Z"))
        .collect();
    let alerts = scan(&joins, RaidScanOptions::default());
    assert_eq!(alerts[0].member_ids, ["e", "d", "c", "b", "a"]);
}

#[test]
fn sorting_uses_the_instant_not_the_string() {
    // By string, "...T10:00:30Z" < "...T11:00:10+01:00"; by instant the
    // +01:00 join (10:00:10Z) is earlier, so it completes the first burst.
    let joins = [
        join(G, "a", "2026-01-01T10:00:00Z"),
        join(G, "b", "2026-01-01T10:00:30Z"),
        join(G, "c", "2026-01-01T11:00:10+01:00"),
    ];
    let alerts = scan(&joins, with_threshold(2.0, 0.0));
    assert_eq!(alerts.len(), 2);
    assert_eq!(alerts[0].member_ids, ["a", "c"]);
    assert_eq!(alerts[0].last_join_at, "2026-01-01T10:00:10.000Z");
    assert_eq!(alerts[1].member_ids, ["a", "c", "b"]);
}

#[test]
fn invalid_rfc3339_timestamps_are_skipped() {
    let invalid = [
        "",
        "not-a-date",
        "2025-07-06",
        "2025-07-06T20:31:00",
        "2025-13-01T00:00:00Z",
        "July 6, 2025 20:31:00 UTC",
    ];
    let mut joins: Vec<_> = invalid
        .iter()
        .enumerate()
        .map(|(i, at)| join(G, &format!("bad{i}"), at))
        .collect();
    joins.extend(burst("2025-07-06T20:31:00Z", 4, 1.0, "ok"));
    assert!(scan(&joins, RaidScanOptions::default()).is_empty());

    joins.push(join(G, "ok4", "2025-07-06T20:31:04.000Z"));
    let alerts = scan(&joins, RaidScanOptions::default());
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].member_ids, ["ok0", "ok1", "ok2", "ok3", "ok4"]);
}

#[test]
fn guild_state_is_separate() {
    // The same member IDs and instants in two guilds: each guild alerts as a
    // first burst; dedupe and cooldown never cross guilds.
    let mut joins = Vec::new();
    for i in 0..5 {
        let at = format!("2026-01-01T10:00:0{i}Z");
        joins.push(join("g1", &format!("m{i}"), &at));
        joins.push(join("g2", &format!("m{i}"), &at));
    }
    joins.push(join("g3", "lone", "2026-01-01T10:00:04Z"));
    let alerts = scan(&joins, RaidScanOptions::default());
    let guilds: Vec<_> = alerts.iter().map(|a| a.guild_id.as_str()).collect();
    assert_eq!(guilds, ["g1", "g2"]);
    assert!(alerts.iter().all(|a| !a.repeat && a.count == 5));
}

#[test]
fn invalid_cooldown_is_refused() {
    let options = RaidScanOptions {
        cooldown_seconds: -1.0,
        ..RaidScanOptions::default()
    };
    assert!(scan_joins_for_bursts(&[], options).is_err());
}
