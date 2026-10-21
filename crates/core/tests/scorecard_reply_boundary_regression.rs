//! Builder regressions for the inclusive first-own clock boundary.

use two_bot_core::community::{
    build_scorecard, FactRow, FirstHumanReply, ScorecardInputs, StreamCoverage,
    COMMUNITY_FACT_TYPES,
};

fn fact(id: i64, kind: &str, actor: &str, at: &str) -> FactRow {
    FactRow {
        id,
        guild_id: "reply-boundary-guild".into(),
        event_type: kind.into(),
        source_event_id: format!("event-{id}"),
        actor_id: Some(actor.into()),
        occurred_at: at.into(),
        source: "fixture".into(),
        classifier_version: "boundary-v1".into(),
        classification: "eligible_human".into(),
        matched_rule: "fixture".into(),
        metadata: Some(r#"{"channelClass":"welcome","contentLength":80}"#.into()),
        idempotency_key: format!("fact-{id}"),
    }
}

#[test]
fn first_own_at_join_or_rules_clock_resolves_with_the_later_reply() {
    for rules_clock in [false, true] {
        let clock = if rules_clock {
            "2026-09-01T10:05:00.000Z"
        } else {
            "2026-09-01T10:00:00.000Z"
        };
        let reply = if rules_clock {
            "2026-09-01T10:06:00.000Z"
        } else {
            "2026-09-01T10:01:00.000Z"
        };
        let mut facts = vec![
            fact(1, "member_joined", "joiner", "2026-09-01T10:00:00.000Z"),
            fact(2, "message_created", "joiner", clock),
            // Same-millisecond replies do not resolve the join; the reply
            // must be strictly later than the first own message.
            fact(3, "message_created", "welcomer", clock),
            fact(4, "message_created", "welcomer", reply),
        ];
        if rules_clock {
            facts.push(fact(5, "rules_accepted", "joiner", clock));
        }
        let outcome = build_scorecard(ScorecardInputs {
            guild_id: "reply-boundary-guild".into(),
            classifier_version: "boundary-v1".into(),
            week_start: "2026-08-31T00:00:00.000Z".into(),
            week_end: "2026-09-07T00:00:00.000Z".into(),
            watermark: 5,
            generated_at: "2026-09-07T06:15:00.000Z".into(),
            recommendations_enabled: true,
            correction_cycles: 0,
            facts,
            coverage: COMMUNITY_FACT_TYPES
                .iter()
                .map(|stream| StreamCoverage {
                    stream: (*stream).into(),
                    covered_from: "2026-08-31T00:00:00.000Z".into(),
                    covered_through: "2026-09-07T00:00:00.000Z".into(),
                })
                .collect(),
            duplicate_source_ids: false,
            existing_revision_count: 0,
            existing_scorecard_json: None,
        })
        .expect("valid boundary fixture");
        let card = outcome.scorecard.expect("fresh scorecard");
        assert_eq!(
            card.first_human_reply,
            Some(FirstHumanReply {
                median_seconds: Some(60.0),
                resolved_count: 1,
                eligible_join_count: 1,
                no_reply_within_24h_count: 0,
                pending_count: 0,
            }),
            "first own at the clock is inclusive; rules_clock={rules_clock}"
        );
        assert!(!card
            .abuse_data_quality
            .iter()
            .any(|note| note.contains("invalid_timestamp")));
    }
}
