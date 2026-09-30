//! Offline classification/attribution corpus executed against pinned legacy code.
//! See docs/classifier-golden.md for the assertion map and excluded report surfaces.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use two_bot_core::community::{classify, ClassifierConfig, ClassifyInput};
use two_bot_core::handlers::voice_end_metadata_json;
use two_bot_core::invites::{
    attribute_joins, attribution_category, invite_growth, summarize_attribution_split,
    AttributionCategory, InviteState, InviteTracker, MemSnapshots,
};
use two_bot_core::rsvp::{
    checkin_classification, checkin_idempotency_key, checkin_metadata_json, checkin_source,
    checkin_source_event_id, AttendanceProof, ATTENDANCE_EVENT_TYPE,
};

fn fixture() -> Value {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/classifier_legacy.json")).unwrap();
    assert_eq!(fixture["version"], 1);
    assert_eq!(
        fixture["source"]["revision"],
        "96777468472f23a02a1e97a43ffab3912fe5df2a"
    );
    fixture
}

fn text(value: &Value) -> &str {
    value.as_str().expect("fixture string")
}

fn flag(value: &Value) -> bool {
    value.as_bool().unwrap_or(false)
}

fn actor(input: &Value) -> ClassifyInput {
    ClassifyInput {
        guild_id: text(&input["guildId"]).to_owned(),
        actor_id: text(&input["actorId"]).to_owned(),
        is_bot: flag(&input["isBot"]),
        webhook_id: input["webhookId"].as_str().map(str::to_owned),
        is_staff_automation: flag(&input["isStaffAutomation"]),
        is_raid: flag(&input["isRaid"]),
        is_staging: flag(&input["isStaging"]),
        is_test: flag(&input["isTest"]),
    }
}

fn counters(value: &Value) -> HashMap<String, u64> {
    value
        .as_object()
        .unwrap()
        .iter()
        .map(|(code, count)| (code.clone(), count.as_u64().unwrap()))
        .collect()
}

#[test]
fn community_exclusions_and_precedence_match_legacy() {
    let f = fixture();
    let env = f["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), text(value).to_owned()))
        .collect();
    let config = ClassifierConfig::from_map(&env);
    let cases = f["community"].as_array().unwrap();
    assert_eq!(cases.len(), 18, "do not silently delete golden coverage");
    let mut ids = HashSet::new();
    for case in cases {
        let id = text(&case["id"]);
        assert!(ids.insert(id), "duplicate case {id}");
        let result = classify(&config, &actor(&case["input"]));
        assert_eq!(
            json!({
                "classification": result.classification,
                "classifierVersion": result.classifier_version,
                "matchedRule": result.matched_rule,
            }),
            case["expect"],
            "community case {id}"
        );
    }
}

#[test]
fn window_and_live_invite_attribution_match_legacy() {
    let f = fixture();
    let cases = f["invites"].as_array().unwrap();
    assert_eq!(cases.len(), 16, "all legacy golden cases must be retained");
    let tracker = InviteTracker::new(MemSnapshots::new());
    let mut ids = HashSet::new();
    let mut buckets = HashSet::new();
    for case in cases {
        let id = text(&case["id"]);
        assert!(ids.insert(id), "duplicate case {id}");
        buckets.insert(text(&case["expectCategory"]));
        let scenario = &case["scenario"];
        match text(&case["kind"]) {
            "window" => {
                let current: Vec<_> = scenario["currentUses"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|inv| InviteState {
                        code: text(&inv["code"]).to_owned(),
                        uses: inv["uses"].as_u64().unwrap(),
                        inviter_id: None,
                        channel_id: None,
                    })
                    .collect();
                let growth = invite_growth(&counters(&scenario["prevUses"]), &current);
                assert_eq!(growth, counters(&case["growth"]), "growth case {id}");
                let count = usize::try_from(scenario["joinCount"].as_u64().unwrap()).unwrap();
                let out = attribute_joins(&growth, count, flag(&scenario["guildHasVanity"]));
                assert_eq!(out.len(), count, "join count case {id}");
                assert_eq!(
                    json!(out.iter().map(|a| &a.source).collect::<Vec<_>>()),
                    case["expect"]["sources"],
                    "sources case {id}"
                );
                assert_eq!(
                    json!(out.iter().map(|a| a.exact).collect::<Vec<_>>()),
                    case["expect"]["exact"],
                    "exactness case {id}"
                );
            }
            "legacy-attribute" => {
                let grew: Vec<_> = scenario["grew"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|code| text(code).to_owned())
                    .collect();
                assert_eq!(
                    tracker.attribute(&grew, flag(&scenario["guildHasVanity"])),
                    text(&case["expect"]["source"]),
                    "live attribution case {id}"
                );
            }
            kind => panic!("unknown golden kind {kind}"),
        }
    }
    assert_eq!(
        buckets,
        HashSet::from(["ambiguous", "unknown", "vanity", "invite-exact", "invite-placed"])
    );
}

#[test]
fn attribution_quality_does_not_merge_ambiguous_and_unknown() {
    let f = fixture();
    for case in f["categories"].as_array().unwrap() {
        let source = text(&case["source"]);
        let category = match attribution_category(source) {
            AttributionCategory::Ambiguous => "ambiguous",
            AttributionCategory::Unknown => "unknown",
            AttributionCategory::Other => "other",
        };
        assert_eq!(category, text(&case["expect"]), "source {source}");
    }
    for case in f["splits"].as_array().unwrap() {
        let rows: Vec<_> = case["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let count = row["n"]
                    .as_u64()
                    .unwrap_or_else(|| text(&row["n"]).parse().unwrap());
                (text(&row["source"]), count)
            })
            .collect();
        let split = summarize_attribution_split(&rows);
        assert_eq!(
            json!({ "ambiguous": split.ambiguous, "unknown": split.unknown }),
            case["expect"],
            "whole-population split {rows:?}"
        );
    }
}

#[test]
fn attendance_identity_classification_and_metadata_bytes_match_legacy() {
    let f = fixture();
    let input = &f["attendance"]["input"];
    let expected = &f["attendance"]["expect"];
    let occurrence = text(&input["eventOccurrenceId"]);
    let member = text(&input["actorId"]);
    assert_eq!(ATTENDANCE_EVENT_TYPE, text(&expected["eventType"]));
    assert_eq!(
        checkin_source_event_id(occurrence, member),
        text(&expected["sourceEventId"])
    );
    assert_eq!(checkin_source(occurrence), text(&expected["source"]));
    assert_eq!(
        checkin_idempotency_key(occurrence, member),
        text(&expected["idempotencyKey"])
    );
    let raw = checkin_metadata_json(occurrence, AttendanceProof::HostCheckin);
    assert_eq!(raw.as_bytes(), text(&expected["metadata"]).as_bytes());
    // Value equality alone would miss a sorted-key serialization regression.
    assert_eq!(serde_json::from_str::<Value>(&raw).unwrap().to_string(), raw);
    for case in f["community"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| text(&c["id"]).starts_with("attendance-"))
    {
        let classification = checkin_classification(flag(&case["input"]["isBot"]));
        assert_eq!(
            classification.classification,
            text(&case["expect"]["classification"])
        );
        assert_eq!(classification.matched_rule, text(&case["expect"]["matchedRule"]));
    }
}

#[test]
fn persisted_voice_metadata_preserves_legacy_byte_order() {
    let f = fixture();
    let cases = f["voiceMetadata"].as_array().unwrap();
    assert_eq!(cases.len(), 2, "known and unknown starts must be retained");
    for case in cases {
        let input = &case["input"];
        let actual = voice_end_metadata_json(
            input["startKnown"].as_bool().unwrap(),
            input["startedAt"].as_str(),
            input["durationSeconds"].as_i64(),
        );
        // Parsed Value equality ignores object key order. Compare persisted bytes.
        assert_eq!(actual.to_string().as_bytes(), text(&case["expect"]).as_bytes());
    }
}
