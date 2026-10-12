//! Participation-rewards anti-gaming acceptance (pure, offline).
//!
//! Failing-first contract for the anti-gaming design: diminishing returns,
//! short-message and burst rules, daily caps, seasonal decay math, abuse
//! detection with fail-closed paths, and Monday season-boundary alignment.
//!
//! Test files only; no runtime code is touched here. Every test below pins a
//! behavior the current runtime does not implement yet, so each one FAILS
//! today and must turn green under the Builder without changing the test.
//!
//! Design reference: AC2 (caps + decay + diminishing returns), AC3 (seasonal
//! reset aligned with leaderboards), AC4 (observe-only abuse monitoring).

use two_bot_core::community::{
    build_scorecard, FactRow, ScorecardInputs, StreamCoverage, COMMUNITY_FACT_TYPES,
};
use two_bot_core::leveling::{
    adjudicate_award, leaderboard_reply, voice_xp_for_duration, LeaderboardEntry, MAX_STORED_XP,
    MESSAGE_XP,
};
use two_bot_core::{format_iso_millis, parse_iso_millis, week_start_ms};

const GUILD: &str = "1545644954272137297";
const MEMBER: &str = "100000000000000001";
const VERSION: &str = "community-v1";
const WEEK_START: &str = "2026-08-31T00:00:00.000Z";
const WEEK_END: &str = "2026-09-07T00:00:00.000Z";
const GENERATED_AT: &str = "2026-09-07T06:15:00.000Z";

/// SPEC AC2: awards 1-5/day earn full 15 XP, 6-10/day earn 5 XP, beyond
/// 10/day earn 0 (daily cap reached).
fn spec_message_xp_for_daily_award_index(index: usize) -> u64 {
    if index < 5 {
        15
    } else if index < 10 {
        5
    } else {
        0
    }
}

#[test]
fn diminishing_return_ladder_15_5_0() {
    let mut prev = 0u64;
    for index in 0..11usize {
        let since = if index == 0 { None } else { Some(61) };
        let award = adjudicate_award(prev, MESSAGE_XP, since, MAX_STORED_XP - prev);
        assert_eq!(
            award.awarded,
            spec_message_xp_for_daily_award_index(index),
            "daily award #{index} must follow the 15/5/0 ladder"
        );
        prev = award.total_xp;
    }
}

#[test]
fn short_message_zero_consumes_cooldown() {
    // SPEC AC2: messages shorter than 8 chars (after trim, no embed or
    // attachment) earn 0 XP but still consume the 60 s cooldown, so "+1"
    // farming cannot spin the award clock. The award path takes no content
    // length today, so a short-message attempt still adjudicates to full XP.
    let short_attempt = adjudicate_award(0, MESSAGE_XP, None, MAX_STORED_XP);
    assert_eq!(
        short_attempt.awarded, 0,
        "short message (<8 chars) must earn 0 XP while consuming cooldown"
    );
    // Cooldown consumed: the next attempt 30 s later stays blocked.
    let follow_up = adjudicate_award(
        short_attempt.total_xp,
        MESSAGE_XP,
        Some(30),
        MAX_STORED_XP - short_attempt.total_xp,
    );
    assert_eq!(follow_up.awarded, 0, "short-message cooldown must hold");
}

#[test]
fn same_channel_burst_halving() {
    // SPEC AC2: the 3rd+ award from one channel within 10 minutes earns half
    // (floor). Three awards 3 minutes apart in one channel must read
    // [15, 15, 7]; the runtime has no channel history, so the 3rd still pays
    // full price.
    let mut prev = 0u64;
    let expected = [15u64, 15, 7];
    for (step, want) in expected.iter().enumerate() {
        let since = if step == 0 { None } else { Some(180) };
        let award = adjudicate_award(prev, MESSAGE_XP, since, MAX_STORED_XP - prev);
        assert_eq!(
            award.awarded,
            *want,
            "same-channel award #{} within 10 min must halve",
            step + 1
        );
        prev = award.total_xp;
    }
}

#[test]
fn daily_caps_message_150_voice_120_utc() {
    // SPEC AC2: per guild+member+source UTC day, message XP caps at 150 and
    // voice XP at 120 (24 min). The 11th message award in one UTC day must be
    // 0 and the day total must stay within cap; the runtime caps nothing, so
    // the 11th still pays 15 and the total reaches 165.
    let mut prev = 0u64;
    for index in 0..11usize {
        let since = if index == 0 { None } else { Some(61) };
        let award = adjudicate_award(prev, MESSAGE_XP, since, MAX_STORED_XP - prev);
        if index < 10 {
            prev = award.total_xp;
        } else {
            assert_eq!(
                award.awarded, 0,
                "11th message award in one UTC day must cap to 0"
            );
        }
    }
    assert!(
        prev <= 150,
        "message day total must stay within the 150 XP cap, got {prev}"
    );
    // Voice leg: a full-rate day tops out at 24 min x 5 XP = 120 XP.
    assert_eq!(
        voice_xp_for_duration(24 * 60),
        120,
        "24 voice minutes must credit exactly the 120 XP daily cap"
    );
    assert!(
        voice_xp_for_duration(30 * 60) <= 120,
        "30 voice minutes in one UTC day must cap at 120 XP"
    );
}

#[test]
fn seasonal_decay_28d_10pct_weekly_lifetime_untouched() {
    // SPEC AC2: lifetime XP is never decayed. Seasonal XP decays 10 % per week
    // past 28 days without an award (multiplicative, floor 0) via a batch job
    // writing `source='decay'` audit rows. A 1000 XP base idle 35 days (one
    // week overdue) must decay to 900 before the next 15 XP award lands
    // (total 915); the runtime never decays, so the award lands on 1000
    // (total 1015).
    let idle_35d_seconds = 35 * 86_400;
    let award = adjudicate_award(
        1000,
        MESSAGE_XP,
        Some(idle_35d_seconds),
        MAX_STORED_XP - 1000,
    );
    assert_eq!(
        award.total_xp,
        900 + MESSAGE_XP,
        "seasonal base idle 35d must decay 10 % for one overdue week before awarding"
    );
}

fn full_coverage() -> Vec<StreamCoverage> {
    COMMUNITY_FACT_TYPES
        .iter()
        .map(|stream| StreamCoverage {
            stream: (*stream).to_owned(),
            covered_from: WEEK_START.to_owned(),
            covered_through: WEEK_END.to_owned(),
        })
        .collect()
}

fn fact(
    id: i64,
    event_type: &str,
    actor: &str,
    occurred_at: &str,
    metadata: Option<String>,
) -> FactRow {
    FactRow {
        id,
        guild_id: GUILD.to_owned(),
        event_type: event_type.to_owned(),
        source_event_id: format!("evt-{id}"),
        actor_id: Some(actor.to_owned()),
        occurred_at: occurred_at.to_owned(),
        source: "gateway".to_owned(),
        classifier_version: VERSION.to_owned(),
        classification: "eligible_human".to_owned(),
        matched_rule: "no_exclusion_matched".to_owned(),
        metadata,
        idempotency_key: format!("key-{id}"),
    }
}

fn scorecard_inputs(facts: Vec<FactRow>) -> ScorecardInputs {
    ScorecardInputs {
        guild_id: GUILD.to_owned(),
        classifier_version: VERSION.to_owned(),
        week_start: WEEK_START.to_owned(),
        week_end: WEEK_END.to_owned(),
        watermark: facts.iter().map(|f| f.id).max().unwrap_or(0),
        generated_at: GENERATED_AT.to_owned(),
        recommendations_enabled: true,
        correction_cycles: 0,
        facts,
        coverage: full_coverage(),
        duplicate_source_ids: false,
        existing_revision_count: 0,
        existing_scorecard_json: None,
    }
}

fn build_json(facts: Vec<FactRow>) -> serde_json::Value {
    let outcome = build_scorecard(scorecard_inputs(facts)).expect("scorecard builds");
    serde_json::from_str(&outcome.scorecard_json).expect("scorecard JSON parses")
}

fn at_offset(seconds: i64) -> String {
    let base = parse_iso_millis(WEEK_START).expect("week start parses");
    format_iso_millis(base + seconds * 1000)
}

#[test]
fn detector_burst_flags_rolling_10min_and_daily() {
    // SPEC AC4: `burst` fires above 30 messages in any rolling 10 minutes or
    // above 60/day. 31 messages 15 s apart (465 s span) from one member must
    // surface `abuse_flags` with a `burst` entry plus the week-level
    // `SPAM_TO_EARN_WATCH` candidate; the builder emits neither today.
    let facts: Vec<FactRow> = (0..31)
        .map(|i| {
            fact(
                i + 1,
                "message_created",
                MEMBER,
                &at_offset(i * 15),
                Some(r#"{"channelClass":"human","channelId":"200000000000000001"}"#.to_owned()),
            )
        })
        .collect();
    let json = build_json(facts);
    let text = json.to_string();
    assert!(
        text.contains("abuse_flags"),
        "burst week must carry per-member abuse_flags"
    );
    assert!(
        text.contains("SPAM_TO_EARN_WATCH"),
        "burst week must raise the SPAM_TO_EARN_WATCH candidate"
    );
}

#[test]
fn detector_copy_paste_flags_identical_length_same_channel() {
    // SPEC AC4: `copy_paste` fires on >=5 messages with identical length +/-2
    // in the identical channel within 1 hour. Five 40-char messages in one
    // channel inside 30 minutes must flag; the builder has no detector.
    let facts: Vec<FactRow> = (0..5)
        .map(|i| {
            fact(
                i + 1,
                "message_created",
                MEMBER,
                &at_offset(i * 360),
                Some(
                    r#"{"channelClass":"human","channelId":"200000000000000001","contentLength":40}"#
                        .to_owned(),
                ),
            )
        })
        .collect();
    let text = build_json(facts).to_string();
    assert!(
        text.contains("copy_paste"),
        "copy-paste week must flag the identical-length channel burst"
    );
}

#[test]
fn detector_voice_afk_needs_occupancy_else_insufficient_data() {
    // SPEC AC4: `voice_afk` needs >=3 sessions with duration >=600 s while
    // alone; single-occupant overlap comes from started/ended pairs, and when
    // occupancy is unavailable the detector must report `insufficient_data`,
    // never a verdict. Three 600 s sessions with no occupancy signal must
    // therefore carry `insufficient_data` and no `voice_afk` flag.
    let facts: Vec<FactRow> = (0..3)
        .map(|i| {
            let end = at_offset(i * 3600);
            let start_ms = parse_iso_millis(&end).expect("end parses") - 600_000;
            fact(
                i + 1,
                "voice_session_ended",
                MEMBER,
                &end,
                Some(format!(
                    r#"{{"durationSeconds":600,"startedAt":"{}"}}"#,
                    format_iso_millis(start_ms)
                )),
            )
        })
        .collect();
    let text = build_json(facts).to_string();
    assert!(
        text.contains("insufficient_data"),
        "voice-afk without occupancy must report insufficient_data, never a verdict"
    );
    assert!(
        !text.contains("voice_afk"),
        "missing occupancy must never emit a voice_afk verdict"
    );
}

#[test]
fn season_boundary_monday_alignment_reports_both_watermarks() {
    // SPEC AC3: seasons are calendar quarters starting Monday 00:00 UTC
    // (`season_id='2026-Q4'`); the boundary coincides with a Monday scorecard
    // week boundary, so the boundary week reports both `season_close` and
    // `season_open` watermarks. The builder reports neither today.
    let season_monday = "2026-10-05T00:00:00.000Z";
    let season_ms = parse_iso_millis(season_monday).expect("season start parses");
    assert_eq!(
        week_start_ms(season_ms),
        season_ms,
        "season start must itself be a Monday 00:00 UTC boundary"
    );
    let json = build_json(vec![fact(
        1,
        "message_created",
        MEMBER,
        &at_offset(60),
        Some(r#"{"channelClass":"human"}"#.to_owned()),
    )]);
    let text = json.to_string();
    assert!(
        text.contains("2026-Q4"),
        "boundary week must carry the current season_id"
    );
    assert!(
        text.contains("season_close") && text.contains("season_open"),
        "boundary week must report both season_close and season_open watermarks"
    );
}

#[test]
fn seasonal_leaderboard_defaults_to_current_season() {
    // SPEC AC3: `/leaderboard` defaults to current-season XP (same 1-25 clamp,
    // same top-10 public shape); lifetime stays behind an explicit flag owned
    // by a later slice. The reply carries no season scope today.
    let entries = vec![LeaderboardEntry {
        member_id: MEMBER.to_owned(),
        xp: 1200,
        level: 5,
        rank: 1,
    }];
    let reply = leaderboard_reply(&entries);
    assert!(
        reply.content.contains("2026-Q4"),
        "seasonal leaderboard must scope to the current season by default"
    );
}
