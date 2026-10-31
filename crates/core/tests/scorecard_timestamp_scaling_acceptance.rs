//! Protected acceptance: timestamp garbage must not manufacture reply clocks,
//! and distinct actors must not turn either grouping into a nested scan.

use std::time::{Duration, Instant};

use two_bot_core::community::{
    build_scorecard, FactRow, FirstHumanReply, Scorecard, ScorecardInputs, StreamCoverage,
    COMMUNITY_FACT_TYPES,
};

const START: &str = "2026-08-31T00:00:00.000Z";
const END: &str = "2026-09-07T00:00:00.000Z";

fn fact(id: i64, kind: &str, actor: &str, at: &str, metadata: Option<String>) -> FactRow {
    FactRow {
        id,
        guild_id: "acceptance-guild".into(),
        event_type: kind.into(),
        source_event_id: format!("event-{id}"),
        actor_id: Some(actor.into()),
        occurred_at: at.into(),
        source: "known".into(),
        classifier_version: "acceptance-v1".into(),
        classification: "eligible_human".into(),
        matched_rule: "fixture".into(),
        metadata,
        idempotency_key: format!("fact-{id}"),
    }
}

fn message(id: i64, actor: &str, at: &str) -> FactRow {
    fact(
        id,
        "message_created",
        actor,
        at,
        Some(r#"{"channelId":"welcome","channelClass":"welcome","contentLength":80}"#.into()),
    )
}

fn inputs(facts: Vec<FactRow>) -> ScorecardInputs {
    ScorecardInputs {
        guild_id: "acceptance-guild".into(),
        classifier_version: "acceptance-v1".into(),
        week_start: START.into(),
        week_end: END.into(),
        watermark: facts.iter().map(|f| f.id).max().unwrap_or(0),
        generated_at: "2026-09-07T06:15:00.000Z".into(),
        recommendations_enabled: true,
        correction_cycles: 0,
        facts,
        coverage: COMMUNITY_FACT_TYPES
            .iter()
            .map(|stream| StreamCoverage {
                stream: (*stream).into(),
                covered_from: START.into(),
                covered_through: END.into(),
            })
            .collect(),
        duplicate_source_ids: false,
        existing_revision_count: 0,
        existing_scorecard_json: None,
    }
}

fn clean_week() -> Vec<FactRow> {
    vec![
        fact(1, "member_joined", "a", "2026-09-01T10:00:00.000Z", None),
        message(2, "a", "2026-09-01T10:01:00.000Z"),
        message(3, "welcomer", "2026-09-01T10:10:00.000Z"),
        fact(4, "member_joined", "b", "2026-09-02T10:00:00.000Z", None),
        message(5, "b", "2026-09-02T10:02:00.000Z"),
        message(6, "welcomer", "2026-09-02T10:30:00.000Z"),
        fact(
            7,
            "member_joined",
            "breached",
            "2026-09-05T00:00:00.000Z",
            None,
        ),
        fact(
            8,
            "member_joined",
            "pending",
            "2026-09-06T12:00:00.000Z",
            None,
        ),
    ]
}

fn build(facts: Vec<FactRow>) -> Scorecard {
    build_scorecard(inputs(facts))
        .expect("valid configuration must build even when individual timestamps are bad")
        .scorecard
        .expect("fresh scorecard")
}

fn assert_honest_reply(scorecard: &Scorecard) {
    // Hand-derived: 10 and 30 minute replies, one 48h breach, one 12h wait.
    assert_eq!(
        scorecard.coverage_state, "complete",
        "do not null every valid sample"
    );
    assert_eq!(
        scorecard.first_human_reply,
        Some(FirstHumanReply {
            median_seconds: Some(1200.0),
            resolved_count: 2,
            eligible_join_count: 4,
            no_reply_within_24h_count: 1,
            pending_count: 1,
        }),
        "exclude only invalid clocks/messages; never substitute epoch, zero, or an infinite age"
    );
}

fn assert_invalid_timestamp_note(scorecard: &Scorecard) {
    assert!(
        scorecard
            .abuse_data_quality
            .iter()
            .any(|note| note.split(':').any(|part| part == "invalid_timestamp")),
        "dropped timestamp samples must be visible as an invalid_timestamp quality note: {:?}",
        scorecard.abuse_data_quality
    );
}

#[test]
fn normal_week_keeps_the_reply_definition_and_has_no_timestamp_warning() {
    let card = build(clean_week());
    assert_honest_reply(&card);
    assert!(!card
        .abuse_data_quality
        .iter()
        .any(|note| note.contains("invalid_timestamp")));
}

#[test]
fn malformed_join_clocks_are_not_resolved_breached_or_pending_samples() {
    let mut facts = clean_week();
    facts.extend([
        fact(
            9,
            "member_joined",
            "bad-resolved",
            "2026-09-03T10:00:xx.000Z",
            None,
        ),
        message(10, "bad-resolved", "2026-09-03T10:01:00.000Z"),
        message(11, "welcomer", "2026-09-03T10:10:00.000Z"),
        fact(
            12,
            "member_joined",
            "bad-unresolved",
            "2026-09-06T23:xx:00.000Z",
            None,
        ),
    ]);
    let card = build(facts);
    assert_honest_reply(&card);
    assert_invalid_timestamp_note(&card);
}

#[test]
fn malformed_rules_clock_is_excluded_instead_of_falling_back_to_join_or_epoch() {
    let mut facts = clean_week();
    facts.extend([
        fact(
            9,
            "member_joined",
            "bad-rules",
            "2026-09-03T10:00:00.000Z",
            None,
        ),
        fact(
            10,
            "rules_accepted",
            "bad-rules",
            "2026-09-03T10:00:xx.000Z",
            None,
        ),
        message(11, "bad-rules", "2026-09-03T10:01:00.000Z"),
        message(12, "welcomer", "2026-09-03T10:10:00.000Z"),
    ]);
    let card = build(facts);
    assert_honest_reply(&card);
    assert_invalid_timestamp_note(&card);
}

#[test]
fn malformed_reply_is_skipped_for_the_next_real_reply_not_a_zero_second_reply() {
    let mut facts = clean_week();
    facts.push(message(9, "welcomer", "2026-09-01T10:05:xx.000Z"));
    let card = build(facts);
    assert_honest_reply(&card);
    assert_invalid_timestamp_note(&card);
}

#[test]
fn malformed_own_message_cannot_start_the_first_reply_search() {
    let mut facts = clean_week();
    facts.extend([
        message(9, "a", "2026-09-01T10:00:30.invalidZ"),
        message(10, "welcomer", "2026-09-01T10:00:59.000Z"),
    ]);
    let card = build(facts);
    assert_honest_reply(&card);
    assert_invalid_timestamp_note(&card);
}

#[test]
fn invalid_reply_does_not_erase_a_real_pending_or_breach() {
    for (at, want_breaches, want_pending) in [
        ("2026-09-03T10:00:00.000Z", 1, 0),
        ("2026-09-06T10:00:00.000Z", 0, 1),
    ] {
        let day = &at[..10];
        let card = build(vec![
            fact(1, "member_joined", "joiner", at, None),
            message(2, "joiner", &format!("{day}T10:01:00.000Z")),
            message(3, "welcomer", &format!("{day}T10:02:xx.000Z")),
        ]);
        assert_eq!(
            card.first_human_reply,
            Some(FirstHumanReply {
                median_seconds: None,
                resolved_count: 0,
                eligible_join_count: 1,
                no_reply_within_24h_count: want_breaches,
                pending_count: want_pending,
            })
        );
        assert_invalid_timestamp_note(&card);
    }
}

fn reply_grouping_input(n: usize) -> ScorecardInputs {
    let mut facts = Vec::with_capacity(2 * n);
    for i in 0..n {
        let actor = format!("actor-{i:08}");
        facts.push(fact(
            i as i64 + 1,
            "member_joined",
            &actor,
            "2026-09-06T12:00:00.000Z",
            None,
        ));
    }
    // All own messages have the same timestamp: no later different-actor
    // reply exists. Reversing actor order also exercises the own-message lookup.
    for i in (0..n).rev() {
        facts.push(message(
            (2 * n - i) as i64,
            &format!("actor-{i:08}"),
            "2026-09-06T12:01:00.000Z",
        ));
    }
    inputs(facts)
}

fn voice_grouping_input(n: usize) -> ScorecardInputs {
    inputs((0..n).map(|i| fact(
        i as i64 + 1,
        "voice_session_ended",
        &format!("actor-{i:08}"),
        "2026-09-06T12:10:00.000Z",
        Some(r#"{"startedAt":"2026-09-06T12:00:00.000Z","durationSeconds":600,"alone":false}"#.into()),
    )).collect())
}

fn timed_build(input: &ScorecardInputs, voice: bool) -> Duration {
    let owned = input.clone(); // Fixture generation and cloning are not timed.
    let started = Instant::now();
    let outcome = std::hint::black_box(build_scorecard(owned).expect("scaling fixture builds"));
    let elapsed = started.elapsed();
    let card = outcome.scorecard.expect("fresh scaling result");
    let n = if voice {
        input.facts.len()
    } else {
        input.facts.len() / 2
    };
    assert_eq!(card.weekly_active_humans, Some(n));
    if !voice {
        assert_eq!(
            card.first_human_reply,
            Some(FirstHumanReply {
                median_seconds: None,
                resolved_count: 0,
                eligible_join_count: n,
                no_reply_within_24h_count: 0,
                pending_count: n,
            })
        );
    }
    elapsed
}

fn assert_linearish(voice: bool) {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let make = if voice {
        voice_grouping_input
    } else {
        reply_grouping_input
    };
    let small = make(4096);
    let large = make(32768);
    // Median of three, with interleaved sizes and a 20ms scheduling allowance.
    // 8x input allows 24x runtime (O(n log n) sorts and noisy CI included),
    // but not the 64x curve of a distinct-actor nested scan.
    let mut small_times = Vec::new();
    let mut large_times = Vec::new();
    timed_build(&small, voice);
    for _ in 0..3 {
        small_times.push(timed_build(&small, voice));
        large_times.push(timed_build(&large, voice));
    }
    small_times.sort();
    large_times.sort();
    let (small, large) = (small_times[1], large_times[1]);
    eprintln!(
        "{} grouping: 4096={small:?}, 32768={large:?}",
        if voice { "voice" } else { "reply" }
    );
    assert!(
        large <= small * 24 + Duration::from_millis(20),
        "{} grouping is quadratic-shaped: 8x input took {large:?} versus {small:?}",
        if voice { "voice" } else { "reply" }
    );
}

#[test]
fn first_reply_grouping_scales_linearish_with_distinct_joiners() {
    assert_linearish(false);
}

#[test]
fn voice_grouping_scales_linearish_with_distinct_actors() {
    assert_linearish(true);
}
