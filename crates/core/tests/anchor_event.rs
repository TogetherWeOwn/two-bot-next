//! Sunday Squad occurrences and scheduled-event payloads against legacy
//! `test/unit.anchorevent.test.ts`.
//!
//! Every golden below was printed by legacy `src/onboarding/anchorEvent.ts`
//! (blob `c8a3be2c`) under Node 24 with `JSON.stringify`, so the payload
//! checks are byte-for-byte against the old bot, not against this port's own
//! arithmetic. The published epochs are the Community Manager's on TOG-93.

use proptest::prelude::*;
use two_bot_core::anchor_event::{
    individual_event_payloads, live_series_start_epoch, occurrences_from, scheduled_event_payload,
    SUNDAY_SQUAD_DESCRIPTION, SUNDAY_SQUAD_EVENT,
};
use two_bot_core::funnel::{format_iso_millis, parse_iso_millis};
use two_bot_core::onboarding::SUNDAY_SQUAD;

/// Run 1: Sunday 23 August 2026, 20:00 EDT.
const RUN_1: i64 = 1_787_529_600;
const WEEK: i64 = 604_800;

/// `JSON.stringify(scheduledEventPayload())`.
const LEGACY_SERIES_PAYLOAD: &str = r#"{"name":"Sunday Squad","description":"Fall Guys, an hour, every Sunday. It runs whether there's two of us or eight — a party of two still drops into a full public show. Free on PC, PlayStation, Xbox, Switch and Android, and nothing to be rusty at.\n\nDrop in whenever. No sign-up, no need to say you're coming, and if you haven't got it installed there's something we can play in the room itself.","channel_id":"1175127344072118405","entity_type":2,"privacy_level":2,"scheduled_start_time":"2026-08-24T00:00:00.000Z","scheduled_end_time":"2026-08-24T01:00:00.000Z","recurrence_rule":{"start":"2026-08-24T00:00:00.000Z","frequency":2,"interval":1,"by_weekday":[6]}}"#;

/// `JSON.stringify(scheduledEventPayload(SUNDAY_SQUAD, 1793581200))`: a series
/// re-created on 1 November 2026, the first EST Sunday.
const LEGACY_POST_DST_PAYLOAD: &str = r#"{"name":"Sunday Squad","description":"Fall Guys, an hour, every Sunday. It runs whether there's two of us or eight — a party of two still drops into a full public show. Free on PC, PlayStation, Xbox, Switch and Android, and nothing to be rusty at.\n\nDrop in whenever. No sign-up, no need to say you're coming, and if you haven't got it installed there's something we can play in the room itself.","channel_id":"1175127344072118405","entity_type":2,"privacy_level":2,"scheduled_start_time":"2026-11-02T01:00:00.000Z","scheduled_end_time":"2026-11-02T02:00:00.000Z","recurrence_rule":{"start":"2026-11-02T01:00:00.000Z","frequency":2,"interval":1,"by_weekday":[6]}}"#;

/// `JSON.stringify(individualEventPayloads(Date.parse('2026-10-20T00:00:00Z'), 3))`.
const LEGACY_INDIVIDUAL_ACROSS_DST: &str = r#"[{"name":"Sunday Squad","description":"Fall Guys, an hour, every Sunday. It runs whether there's two of us or eight — a party of two still drops into a full public show. Free on PC, PlayStation, Xbox, Switch and Android, and nothing to be rusty at.\n\nDrop in whenever. No sign-up, no need to say you're coming, and if you haven't got it installed there's something we can play in the room itself.","channel_id":"1175127344072118405","entity_type":2,"privacy_level":2,"scheduled_start_time":"2026-10-26T00:00:00.000Z","scheduled_end_time":"2026-10-26T01:00:00.000Z"},{"name":"Sunday Squad","description":"Fall Guys, an hour, every Sunday. It runs whether there's two of us or eight — a party of two still drops into a full public show. Free on PC, PlayStation, Xbox, Switch and Android, and nothing to be rusty at.\n\nDrop in whenever. No sign-up, no need to say you're coming, and if you haven't got it installed there's something we can play in the room itself.","channel_id":"1175127344072118405","entity_type":2,"privacy_level":2,"scheduled_start_time":"2026-11-02T01:00:00.000Z","scheduled_end_time":"2026-11-02T02:00:00.000Z"},{"name":"Sunday Squad","description":"Fall Guys, an hour, every Sunday. It runs whether there's two of us or eight — a party of two still drops into a full public show. Free on PC, PlayStation, Xbox, Switch and Android, and nothing to be rusty at.\n\nDrop in whenever. No sign-up, no need to say you're coming, and if you haven't got it installed there's something we can play in the room itself.","channel_id":"1175127344072118405","entity_type":2,"privacy_level":2,"scheduled_start_time":"2026-11-09T01:00:00.000Z","scheduled_end_time":"2026-11-09T02:00:00.000Z"}]"#;

/// `JSON.stringify(occurrencesFrom(Date.parse('2026-10-01T00:00:00Z'), 30))`:
/// through the November fall-back and the March 2027 spring-forward.
const LEGACY_30_FROM_OCT_1: [i64; 30] = [
    1791158400, 1791763200, 1792368000, 1792972800, 1793581200, 1794186000, 1794790800, 1795395600,
    1796000400, 1796605200, 1797210000, 1797814800, 1798419600, 1799024400, 1799629200, 1800234000,
    1800838800, 1801443600, 1802048400, 1802653200, 1803258000, 1803862800, 1804467600, 1805068800,
    1805673600, 1806278400, 1806883200, 1807488000, 1808092800, 1808697600,
];

fn secs(iso: &str) -> i64 {
    parse_iso_millis(iso).expect("valid ISO fixture") / 1000
}

/// New York wall clock at `utc_secs` as (days since epoch, seconds into the
/// day). Test-side and deliberately dumb: the two DST changes in range are
/// written out, not computed, so it cannot share a bug with the crate.
fn ny_clock(utc_secs: i64) -> (i64, i64) {
    const FALL_2026: i64 = 1_793_512_800; // 2026-11-01T06:00Z, 02:00 EDT
    const SPRING_2027: i64 = 1_805_007_600; // 2027-03-14T07:00Z, 02:00 EST
    const SPRING_2026: i64 = 1_772_953_200; // 2026-03-08T07:00Z, 02:00 EST
    assert!(
        (SPRING_2026..1_825_000_000).contains(&utc_secs),
        "outside table"
    );
    let offset = if (FALL_2026..SPRING_2027).contains(&utc_secs) {
        -5 * 3600
    } else {
        -4 * 3600
    };
    let local = utc_secs + offset;
    (local.div_euclid(86_400), local.rem_euclid(86_400))
}

/// True when `utc_secs` reads Sunday 20:00 on a New York wall clock.
fn is_sunday_8pm_ny(utc_secs: i64) -> bool {
    let (days, of_day) = ny_clock(utc_secs);
    // 1970-01-01 was a Thursday; +4 makes Sunday = 0.
    (days + 4).rem_euclid(7) == 0 && of_day == 20 * 3600
}

// --- occurrences -------------------------------------------------------------

#[test]
fn first_six_occurrences_are_the_published_epochs() {
    let got = occurrences_from(secs("2026-08-22T12:00:00Z"), 6, SUNDAY_SQUAD);
    assert_eq!(
        got,
        [
            1_787_529_600, // Sun 23 Aug
            1_788_134_400, // Sun 30 Aug
            1_788_739_200, // Sun 6 Sep
            1_789_344_000, // Sun 13 Sep
            1_789_948_800, // Sun 20 Sep
            1_790_553_600, // Sun 27 Sep
        ]
    );
}

#[test]
fn run_one_is_23_august_not_30_august() {
    assert_eq!(SUNDAY_SQUAD.series_start_epoch, RUN_1);
    assert_eq!(format_iso_millis(RUN_1 * 1000), "2026-08-24T00:00:00.000Z");
    assert!(is_sunday_8pm_ny(RUN_1));
}

#[test]
fn thirty_sundays_match_legacy_and_read_8pm_in_new_york() {
    let got = occurrences_from(secs("2026-10-01T00:00:00Z"), 30, SUNDAY_SQUAD);
    assert_eq!(got, LEGACY_30_FROM_OCT_1);
    for start in got {
        assert!(
            is_sunday_8pm_ny(start),
            "{} is not Sunday 20:00 NY",
            format_iso_millis(start * 1000)
        );
    }
}

#[test]
fn the_week_dst_ends_is_169_hours() {
    let before = occurrences_from(secs("2026-10-20T00:00:00Z"), 1, SUNDAY_SQUAD)[0];
    let after = occurrences_from(secs("2026-10-27T00:00:00Z"), 1, SUNDAY_SQUAD)[0];
    assert_eq!(before, 1_792_972_800); // 25 Oct, 20:00 EDT
    assert_eq!(after, 1_793_581_200); // 1 Nov, 20:00 EST
    assert_eq!(after - before, 608_400);
    // A fixed week lands on 19:00 local: the bug the spec warns about.
    assert!(!is_sunday_8pm_ny(before + WEEK));
}

#[test]
fn an_occurrence_on_the_boundary_is_not_skipped_or_repeated() {
    assert_eq!(occurrences_from(RUN_1 - 1, 1, SUNDAY_SQUAD), [RUN_1]);
    assert_eq!(occurrences_from(RUN_1, 1, SUNDAY_SQUAD), [RUN_1 + WEEK]);
    assert!(occurrences_from(RUN_1, 0, SUNDAY_SQUAD).is_empty());
}

// --- live series start -------------------------------------------------------

#[test]
fn live_series_starts_on_run_one_then_advances_past_missed_runs() {
    assert_eq!(
        live_series_start_epoch(secs("2026-08-22T12:00:00Z"), SUNDAY_SQUAD),
        RUN_1
    );
    assert_eq!(live_series_start_epoch(RUN_1 - 1, SUNDAY_SQUAD), RUN_1);
    // Standing on run 1, it is no longer creatable: next week.
    assert_eq!(live_series_start_epoch(RUN_1, SUNDAY_SQUAD), 1_788_134_400);
    assert_eq!(
        live_series_start_epoch(secs("2026-09-06T00:00:00Z"), SUNDAY_SQUAD),
        1_788_739_200
    );
    // 8 November 2026, 20:00 EST.
    let after_dst = live_series_start_epoch(secs("2026-11-02T12:00:00Z"), SUNDAY_SQUAD);
    assert_eq!(after_dst, 1_794_186_000);
    assert!(is_sunday_8pm_ny(after_dst));
}

proptest! {
    /// Discord refuses a start in the past; whatever `now` is, the live start
    /// is strictly ahead of it and is an actual occurrence.
    #[test]
    fn live_series_start_is_never_in_the_past(now in RUN_1 - 400 * 86_400..RUN_1 + 30 * 365 * 86_400) {
        let start = live_series_start_epoch(now, SUNDAY_SQUAD);
        prop_assert!(start > now);
        if now < RUN_1 {
            prop_assert_eq!(start, RUN_1);
        } else {
            prop_assert_eq!(start, occurrences_from(now, 1, SUNDAY_SQUAD)[0]);
            prop_assert!(start - now <= WEEK + 3600);
        }
    }

    #[test]
    fn consecutive_occurrences_are_a_week_give_or_take_the_clock_change(now in RUN_1..RUN_1 + 30 * 365 * 86_400) {
        let run = occurrences_from(now, 8, SUNDAY_SQUAD);
        prop_assert!(run[0] > now);
        for pair in run.windows(2) {
            let gap = pair[1] - pair[0];
            prop_assert!(gap == WEEK || gap == WEEK - 3600 || gap == WEEK + 3600, "gap {gap}");
        }
    }
}

// --- payloads ----------------------------------------------------------------

#[test]
fn series_payload_is_byte_equal_to_legacy() {
    let payload = scheduled_event_payload(SUNDAY_SQUAD_EVENT, SUNDAY_SQUAD.series_start_epoch);
    assert_eq!(
        serde_json::to_string(&payload).unwrap(),
        LEGACY_SERIES_PAYLOAD
    );

    assert_eq!(payload.entity_type, 2); // VOICE
    assert_eq!(payload.privacy_level, 2); // GUILD_ONLY
    let rule = payload.recurrence_rule.as_ref().expect("series has a rule");
    assert_eq!(rule.frequency, 2); // WEEKLY
    assert_eq!(rule.interval, 1);
    assert_eq!(rule.by_weekday, [6]); // Discord's Sunday
    assert_eq!(rule.start, payload.scheduled_start_time);
}

#[test]
fn series_recreated_after_dst_is_byte_equal_to_legacy() {
    let payload = scheduled_event_payload(SUNDAY_SQUAD_EVENT, 1_793_581_200);
    assert_eq!(
        serde_json::to_string(&payload).unwrap(),
        LEGACY_POST_DST_PAYLOAD
    );
}

#[test]
fn description_is_the_spec_copy_within_discords_cap() {
    assert_eq!(SUNDAY_SQUAD_EVENT.description, SUNDAY_SQUAD_DESCRIPTION);
    assert!(SUNDAY_SQUAD_DESCRIPTION.contains("Fall Guys"));
    assert!(SUNDAY_SQUAD_DESCRIPTION.chars().count() <= 1000);
}

#[test]
fn individual_payloads_across_dst_are_byte_equal_to_legacy() {
    let payloads = individual_event_payloads(secs("2026-10-20T00:00:00Z"), 3, SUNDAY_SQUAD_EVENT);
    assert_eq!(
        serde_json::to_string(&payloads).unwrap(),
        LEGACY_INDIVIDUAL_ACROSS_DST
    );
}

#[test]
fn six_individual_cards_are_real_occurrences_without_a_rule() {
    let payloads = individual_event_payloads(secs("2026-08-22T12:00:00Z"), 6, SUNDAY_SQUAD_EVENT);
    assert_eq!(payloads.len(), 6);
    let mut starts: Vec<&str> = payloads
        .iter()
        .map(|p| p.scheduled_start_time.as_str())
        .collect();
    starts.dedup();
    assert_eq!(starts.len(), 6, "six copies of one card");
    for p in &payloads {
        assert!(p.recurrence_rule.is_none());
        let json = serde_json::to_value(p).unwrap();
        assert!(json.get("recurrence_rule").is_none(), "{json}");
        assert!(is_sunday_8pm_ny(secs(&p.scheduled_start_time)));
        assert_eq!(
            secs(&p.scheduled_end_time) - secs(&p.scheduled_start_time),
            3600
        );
    }
}

// --- purity ------------------------------------------------------------------

#[test]
fn module_reads_no_clock_env_or_network() {
    let source = include_str!("../src/anchor_event.rs");
    for banned in [
        "std::env",
        "env!(",
        "SystemTime",
        "Instant",
        "now_iso",
        "tokio",
        "hyper",
        "http::",
    ] {
        assert!(!source.contains(banned), "anchor_event.rs uses {banned}");
    }
}
