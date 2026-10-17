//! Detector input coverage in the weekly scorecard (pure, offline).
//!
//! Protected acceptance file: the builder must make these tests pass
//! without editing them. Test code only; no runtime code belongs here.
//! Every test below fails on `main` today because the scorecard JSON has
//! no `detector_inputs` field: the detectors run on partial input, but a
//! clean week and a mostly blind week serialize identically apart from
//! flags.
//!
//! ## Pinned contract (what the builder must emit)
//!
//! A new top-level scorecard key, spelled literally `detector_inputs`
//! (snake_case, like `abuse_flags`/`season_id`), whose value is an object
//! with exactly the keys `burst` and `copy_paste`. Each maps to
//! `{"eligible": <uint>, "with_input": <uint>}` (snake_case inner keys):
//!
//! - `eligible` counts `message_created` facts classified
//!   `eligible_human` — the denominator both message detectors draw from.
//!   Facts with any other classification, and non-message facts, never
//!   count. Every eligible fixture message carries an actor, a
//!   `channelId` and a parseable timestamp, so detector-internal drops
//!   for a missing actor/channel/timestamp are deliberately not pinned
//!   here.
//! - `burst.with_input` counts the eligible messages the burst detector
//!   consumes. Burst consumes eligible messages themselves, so in these
//!   fixtures `with_input == eligible`.
//! - `copy_paste.with_input` counts eligible messages actually usable by
//!   the copy/paste detector: metadata carrying `contentLength` as an
//!   unsigned integer (the detector's `as_u64` gate). A string-valued
//!   `contentLength` is present-but-unusable and must not count.
//!
//! The field is populated under the same complete-coverage gate that
//! runs the detectors. Under incomplete coverage it is present with
//! zeros (stable JSON shape) and the detectors stay fail-closed: no
//! flags, no candidates, no data-quality notes.
//!
//! Zero-input behavior is unchanged: a complete week whose eligible
//! messages carry no usable `contentLength` still emits
//! `copy_paste:insufficient_data`, now with `with_input: 0` saying how
//! blind the detector was. An empty week emits zero counts and no
//! fabricated quality note.
//!
//! Out of scope: `voice_afk` and voice occupancy denominators. The exact
//! object equality below fails if a `voice_afk` key (or any other extra
//! key) is smuggled into `detector_inputs` by this slice.

use serde_json::{json, Value};
use two_bot_core::community::{
    build_scorecard, FactRow, ScorecardInputs, StreamCoverage, COMMUNITY_FACT_TYPES,
};
use two_bot_core::{format_iso_millis, parse_iso_millis};

const GUILD: &str = "1545644954272137297";
const MEMBER: &str = "100000000000000001";
const CHANNEL: &str = "200000000000000001";
const VERSION: &str = "community-v1";
const WEEK_START: &str = "2026-08-31T00:00:00.000Z";
const WEEK_END: &str = "2026-09-07T00:00:00.000Z";
const GENERATED_AT: &str = "2026-09-07T06:15:00.000Z";

fn fact(
    id: i64,
    event_type: &str,
    actor: &str,
    classification: &str,
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
        classification: classification.to_owned(),
        matched_rule: "no_exclusion_matched".to_owned(),
        metadata,
        idempotency_key: format!("key-{id}"),
    }
}

fn message_meta(content_length: Option<&str>) -> Option<String> {
    Some(match content_length {
        Some(raw) => {
            format!(r#"{{"channelClass":"human","channelId":"{CHANNEL}","contentLength":{raw}}}"#)
        }
        None => format!(r#"{{"channelClass":"human","channelId":"{CHANNEL}"}}"#),
    })
}

fn at_offset(seconds: i64) -> String {
    let base = parse_iso_millis(WEEK_START).expect("week start parses");
    format_iso_millis(base + seconds * 1000)
}

/// Eligible human message at `seconds` past week start; `content_length`
/// is the raw JSON value for the `contentLength` key, or `None` to omit it.
fn eligible_message(id: i64, seconds: i64, content_length: Option<&str>) -> FactRow {
    fact(
        id,
        "message_created",
        MEMBER,
        "eligible_human",
        &at_offset(seconds),
        message_meta(content_length),
    )
}

fn complete_coverage() -> Vec<StreamCoverage> {
    COMMUNITY_FACT_TYPES
        .iter()
        .map(|stream| StreamCoverage {
            stream: (*stream).to_owned(),
            covered_from: WEEK_START.to_owned(),
            covered_through: WEEK_END.to_owned(),
        })
        .collect()
}

fn build_with_coverage(facts: Vec<FactRow>, coverage: Vec<StreamCoverage>) -> (Value, String) {
    let outcome = build_scorecard(ScorecardInputs {
        guild_id: GUILD.to_owned(),
        classifier_version: VERSION.to_owned(),
        week_start: WEEK_START.to_owned(),
        week_end: WEEK_END.to_owned(),
        watermark: facts.iter().map(|f| f.id).max().unwrap_or(0),
        generated_at: GENERATED_AT.to_owned(),
        recommendations_enabled: true,
        correction_cycles: 0,
        facts,
        coverage,
        duplicate_source_ids: false,
        existing_revision_count: 0,
        existing_scorecard_json: None,
    })
    .expect("scorecard builds");
    let json: Value = serde_json::from_str(&outcome.scorecard_json).expect("scorecard JSON parses");
    (json, outcome.scorecard_json)
}

fn build(facts: Vec<FactRow>) -> (Value, String) {
    build_with_coverage(facts, complete_coverage())
}

fn detector_inputs(json: &Value) -> Value {
    json.get("detector_inputs")
        .unwrap_or_else(|| {
            panic!("scorecard must carry a literal `detector_inputs` key; got keys: {json}")
        })
        .clone()
}

fn zero_detector_inputs() -> Value {
    json!({
        "burst": {"eligible": 0, "with_input": 0},
        "copy_paste": {"eligible": 0, "with_input": 0},
    })
}

#[test]
fn partial_input_week_reports_exact_detector_inputs() {
    // Known partial input, counted by hand:
    // - 7 eligible human messages an hour apart (no detector fires):
    //   2 carry an integer contentLength, 1 carries a string-valued
    //   contentLength (present but unusable), 4 carry none.
    // - 1 bot message carrying an integer contentLength: not eligible.
    // - 1 eligible member_joined fact: not a message.
    // Expected: burst {7, 7}, copy_paste {7, 2}.
    let mut facts: Vec<FactRow> = vec![
        eligible_message(1, 0, Some("40")),
        eligible_message(2, 3_600, Some("120")),
        eligible_message(3, 7_200, Some("\"40\"")),
        eligible_message(4, 10_800, None),
        eligible_message(5, 14_400, None),
        eligible_message(6, 18_000, None),
        eligible_message(7, 21_600, None),
    ];
    facts.push(fact(
        8,
        "message_created",
        "100000000000000099",
        "bot",
        &at_offset(25_200),
        message_meta(Some("40")),
    ));
    facts.push(fact(
        9,
        "member_joined",
        MEMBER,
        "eligible_human",
        &at_offset(28_800),
        None,
    ));

    let (json, raw) = build(facts);
    assert_eq!(json["coverageState"], "complete");
    // Exact object equality: pins the key set (no voice_afk, no extras),
    // the snake_case inner keys, integer values, and the exact counts.
    assert_eq!(
        detector_inputs(&json),
        json!({
            "burst": {"eligible": 7, "with_input": 7},
            "copy_paste": {"eligible": 7, "with_input": 2},
        }),
        "partial week must report exact per-detector eligible/with_input counts"
    );
    // Literal serialization: snake_case on the wire, never camelized by
    // the scorecard's camelCase default.
    assert!(
        raw.contains("\"detector_inputs\"") && raw.contains("\"with_input\""),
        "serialized scorecard must spell detector_inputs/with_input literally: {raw}"
    );
    assert!(
        !raw.contains("detectorInputs") && !raw.contains("withInput"),
        "detector input keys must stay snake_case, not camelCase: {raw}"
    );
    assert_eq!(json["abuse_flags"], json!([]), "no detector fires");
    assert_eq!(json["abuse_candidates"], json!([]));
    assert_eq!(
        json["abuse_data_quality"],
        json!(["copy_paste:insufficient_data"]),
        "2 of 7 usable lengths is sparse: fail-closed note, no verdict"
    );
}

#[test]
fn zero_copy_paste_input_week_keeps_insufficient_data_and_zero_with_input() {
    // 3 eligible messages, none carrying contentLength: the existing
    // fail-closed note stays, and the counts now quantify the blindness
    // (copy_paste saw 0 of 3) while burst saw all 3.
    let facts: Vec<FactRow> = (0..3)
        .map(|i| eligible_message(i + 1, i * 3_600, None))
        .collect();

    let (json, _raw) = build(facts);
    assert_eq!(json["coverageState"], "complete");
    assert_eq!(
        detector_inputs(&json),
        json!({
            "burst": {"eligible": 3, "with_input": 3},
            "copy_paste": {"eligible": 3, "with_input": 0},
        })
    );
    assert_eq!(
        json["abuse_data_quality"],
        json!(["copy_paste:insufficient_data"]),
        "zero usable lengths must keep the existing insufficient_data note"
    );
    assert_eq!(json["abuse_flags"], json!([]));
    assert_eq!(json["abuse_candidates"], json!([]));
}

#[test]
fn empty_week_emits_zero_detector_inputs() {
    // No facts at all under complete coverage: zero counts, no flags,
    // and no fabricated insufficient_data note (existing behavior: the
    // note fires only when eligible messages exist but none is usable).
    let (json, _raw) = build(Vec::new());
    assert_eq!(json["coverageState"], "complete");
    assert_eq!(detector_inputs(&json), zero_detector_inputs());
    assert_eq!(json["abuse_flags"], json!([]));
    assert_eq!(json["abuse_candidates"], json!([]));
    assert_eq!(json["abuse_data_quality"], json!([]));
}

#[test]
fn incomplete_coverage_week_emits_zero_shape_and_no_flags() {
    // Facts that would fire both burst (31 messages inside 10 minutes)
    // and copy_paste (31 same-length messages in one channel inside an
    // hour) if the detectors ran. One stream's coverage is missing, so
    // the week is incomplete: the pinned shape is present with zeros
    // and fail-closed behavior is unchanged (no flags at all).
    let facts: Vec<FactRow> = (0..31)
        .map(|i| eligible_message(i + 1, i * 15, Some("40")))
        .collect();
    let mut coverage = complete_coverage();
    coverage.pop();

    let (json, _raw) = build_with_coverage(facts, coverage);
    assert_eq!(json["coverageState"], "incomplete");
    assert_eq!(
        json["intervention"]["code"], "INGESTION_INCOMPLETE",
        "incomplete coverage stays fail-closed"
    );
    assert_eq!(
        detector_inputs(&json),
        zero_detector_inputs(),
        "incomplete coverage emits the pinned shape with zeros"
    );
    assert_eq!(
        json["abuse_flags"],
        json!([]),
        "no flags under incomplete coverage, however flagrant the facts"
    );
    assert_eq!(json["abuse_candidates"], json!([]));
    assert_eq!(json["abuse_data_quality"], json!([]));
}

#[test]
fn flag_week_still_reports_detector_inputs() {
    // Counts ride alongside flags, so a flagged week also says how much
    // each detector saw: 31 eligible messages inside 10 minutes fire
    // burst; only the first 5 carry lengths (spread 20 chars apart, so
    // no copy_paste cluster). Expected: burst {31, 31},
    // copy_paste {31, 5}, a burst flag, the watch candidate, and no
    // copy_paste flag.
    let lengths = ["10", "30", "50", "70", "90"];
    let facts: Vec<FactRow> = (0..31)
        .map(|i| {
            let len = if (i as usize) < lengths.len() {
                Some(lengths[i as usize])
            } else {
                None
            };
            eligible_message(i + 1, i * 15, len)
        })
        .collect();

    let (json, _raw) = build(facts);
    assert_eq!(json["coverageState"], "complete");
    assert_eq!(
        detector_inputs(&json),
        json!({
            "burst": {"eligible": 31, "with_input": 31},
            "copy_paste": {"eligible": 31, "with_input": 5},
        })
    );
    let flags = json["abuse_flags"]
        .as_array()
        .expect("abuse_flags is an array");
    assert!(
        flags
            .iter()
            .any(|f| f["detector"] == "burst" && f["member_id"] == MEMBER),
        "burst must fire on 31 messages in 10 minutes: {flags:?}"
    );
    assert!(
        flags.iter().all(|f| f["detector"] != "copy_paste"),
        "spread lengths must not flag copy_paste: {flags:?}"
    );
    assert_eq!(json["abuse_candidates"], json!(["SPAM_TO_EARN_WATCH"]));
    assert_eq!(
        json["abuse_data_quality"],
        json!(["copy_paste:insufficient_data"]),
        "5 of 31 usable lengths is sparse: copy_paste stays fail-closed while burst still fires"
    );
}
