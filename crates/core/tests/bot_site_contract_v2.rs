//! Bot-site contract v2 conformance (`docs/bot-site-contract.md` §2).
//!
//! Hermetic: `tests/fixtures/bot-site-contract-v2.json` holds golden
//! request/result pairs per verb; this file only drives the production
//! validators in `two_bot_core::internal_actions` and checks shapes.
//! `voice.snapshot` validates against the allowlist, the flag gate and the
//! field validators. The other five verbs have no implementation yet, so the
//! test pins `is_implemented == false` and checks shapes only — landing an
//! implementation must flip the pin and tighten these checks.

use std::collections::HashMap;

use serde_json::{Map, Value};
use two_bot_core::internal_actions::{
    assert_allowed, is_implemented, is_snowflake, needs_idempotency_key, require_snowflake,
    require_timestamp, utf16_len, valid_event_key, valid_idempotency_key, validate_idempotency_key,
    ErrorCode, InternalFlags, MAX_EVENT_NAME_CHARS,
};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/bot-site-contract-v2.json"))
        .expect("contract v2 fixture parses")
}

fn verb<'a>(fixture: &'a Value, name: &str) -> &'a Value {
    &fixture["verbs"][name]
}

fn body(value: &Value) -> Map<String, Value> {
    value.as_object().expect("request object").clone()
}

fn flags_with(flag: &str) -> InternalFlags {
    InternalFlags::from_map(&HashMap::from([(flag.to_owned(), "1".to_owned())]))
}

fn flags_empty() -> InternalFlags {
    InternalFlags::from_map(&HashMap::new())
}

fn assert_timestamp_field(body: &Map<String, Value>, field: &str) {
    let normalised =
        require_timestamp(body, field).unwrap_or_else(|_| panic!("{field} is a valid timestamp"));
    assert!(normalised.ends_with('Z'), "{field} normalises to UTC Zulu");
}

#[test]
fn snapshot_request_is_allowlisted_and_flag_gated() {
    let fx = fixture();
    let v = verb(&fx, "voice.snapshot");
    let request = body(&v["request"]);
    assert_eq!(request["action"], "voice.snapshot");
    assert!(is_implemented("voice.snapshot"));
    assert!(!needs_idempotency_key("voice.snapshot"));
    assert_allowed(
        "voice.snapshot",
        &flags_with("TWO_INTERNAL_ALLOW_VOICE_SNAPSHOT"),
        true,
        true,
    )
    .expect("flagged-on snapshot is allowed");
    let err = assert_allowed("voice.snapshot", &flags_empty(), true, true)
        .expect_err("flag-off snapshot is refused");
    assert_eq!(err.code, ErrorCode::ActionNotAllowed);
}

#[test]
fn snapshot_results_share_one_additive_tolerant_shape() {
    let fx = fixture();
    let results = &verb(&fx, "voice.snapshot")["results"];
    // The legacy golden omits the v2 additive fields; both must pass the
    // same checker so older bot builds stay readable.
    for key in ["full", "legacy"] {
        let result = &results[key];
        assert_eq!(result["v"], 1);
        let mut observed = result.as_object().expect("result").clone();
        // `observed_at` is whole-second Zulu; reuse the timestamp validator
        // through an owned copy.
        observed.insert("at".to_owned(), result["observed_at"].clone());
        assert_timestamp_field(&observed, "at");
        assert!(result["total_in_voice"].as_u64().is_some());
        for room in result["rooms"].as_array().expect("rooms array") {
            assert!(is_snowflake(
                room["channel_id"].as_str().expect("channel_id str")
            ));
            for id in room["member_ids"].as_array().expect("member_ids array") {
                assert!(is_snowflake(id.as_str().expect("member id str")));
            }
            match &room["occupied_since"] {
                Value::Null => {}
                Value::String(_) => {
                    let mut owned = Map::new();
                    owned.insert("since".to_owned(), room["occupied_since"].clone());
                    assert_timestamp_field(&owned, "since");
                }
                other => panic!("occupied_since is a timestamp or null, got {other}"),
            }
            if let Some(key) = room.get("event_key") {
                assert!(
                    valid_event_key(key.as_str().expect("event_key str")),
                    "event_key keeps the website key shape"
                );
            }
        }
    }
    let refusal = &results["refusal"];
    assert_eq!(refusal["ok"], false);
    assert_eq!(refusal["error"]["code"], "discord_unavailable");
}

#[test]
fn unimplemented_verbs_stay_closed_until_they_land() {
    let fx = fixture();
    for name in [
        "voice.move_member",
        "reminder.send",
        "place_offer.send",
        "event.attendance.read",
        "botlog.read",
    ] {
        let v = verb(&fx, name);
        // Flip this pin the moment the verb lands, and tighten that verb's
        // shape test to drive the real validators.
        assert_eq!(v["implemented"], false, "{name} pins unimplemented");
        assert!(!is_implemented(name), "{name} is not on the allowlist yet");
        assert_eq!(
            needs_idempotency_key(name),
            v["needs_idempotency_key"].as_bool().expect("pin"),
            "{name} idempotency pin"
        );
        let err = assert_allowed(
            name,
            &flags_with("TWO_INTERNAL_ALLOW_VOICE_SNAPSHOT"),
            true,
            true,
        )
        .expect_err("unknown verb is refused even with flags on");
        assert_eq!(err.code, ErrorCode::ActionNotAllowed);
    }
}

#[test]
fn move_member_request_and_outcomes_match_the_closed_vocab() {
    let fx = fixture();
    let v = verb(&fx, "voice.move_member");
    let request = body(&v["request"]);
    assert_eq!(request["action"], "voice.move_member");
    require_snowflake(&request, "discord_id").expect("discord_id snowflake");
    require_snowflake(&request, "channel_id").expect("channel_id snowflake");
    let outcomes: Vec<&str> = v["outcomes"]
        .as_array()
        .expect("outcomes array")
        .iter()
        .map(|o| o.as_str().expect("outcome str"))
        .collect();
    for (name, result) in v["results"].as_object().expect("results object") {
        let outcome = result["outcome"].as_str().expect("outcome str");
        assert_eq!(outcome, name, "golden key names its outcome");
        assert!(outcomes.contains(&outcome), "{outcome} is a closed outcome");
        if outcome == "moved" {
            assert!(is_snowflake(
                result["from_channel_id"]
                    .as_str()
                    .expect("moved carries from_channel_id")
            ));
        }
    }
}

fn assert_notify_request(v: &Value, time_field: &str) {
    let request = body(&v["request"]);
    require_snowflake(&request, "discord_id").expect("discord_id snowflake");
    let event_key = request["event_key"].as_str().expect("event_key str");
    assert!(valid_event_key(event_key));
    assert!(
        event_key.len() <= 64,
        "notify event keys stay within the 64-char notify bound"
    );
    assert_timestamp_field(&request, time_field);
    let name = request["event_name"].as_str().expect("event_name str");
    assert!(!name.is_empty());
    assert!(
        utf16_len(name) <= MAX_EVENT_NAME_CHARS,
        "event_name fits the website name ceiling"
    );
    let key = v["idempotency_key"].as_str().expect("idempotency key");
    assert!(valid_idempotency_key(key));
    let action = request["action"].as_str().expect("action str");
    assert_eq!(
        validate_idempotency_key(Some(key), action).expect("key validates"),
        key
    );
    let outcomes: Vec<&str> = v["outcomes"]
        .as_array()
        .expect("outcomes array")
        .iter()
        .map(|o| o.as_str().expect("outcome str"))
        .collect();
    for result in v["results"].as_object().expect("results object").values() {
        let outcome = result["outcome"].as_str().expect("outcome str");
        assert!(outcomes.contains(&outcome), "{outcome} is a closed outcome");
    }
}

#[test]
fn reminder_send_shapes_hold() {
    let fx = fixture();
    let v = verb(&fx, "reminder.send");
    let request = body(&v["request"]);
    assert_eq!(request["action"], "reminder.send");
    let kinds: Vec<&str> = v["kinds"]
        .as_array()
        .expect("kinds array")
        .iter()
        .map(|k| k.as_str().expect("kind str"))
        .collect();
    let kind = request["kind"].as_str().expect("kind str");
    assert!(kinds.contains(&kind), "{kind} is a closed DM kind");
    assert!(
        request.get("body").is_none(),
        "notify requests carry no free-text body"
    );
    assert_notify_request(v, "starts_at");
}

#[test]
fn place_offer_send_shapes_hold() {
    let fx = fixture();
    let v = verb(&fx, "place_offer.send");
    assert_eq!(v["request"]["action"], "place_offer.send");
    assert!(
        v["request"].get("body").is_none(),
        "notify requests carry no free-text body"
    );
    assert_notify_request(v, "expires_at");
}

#[test]
fn attendance_read_shapes_hold() {
    let fx = fixture();
    let v = verb(&fx, "event.attendance.read");
    let request = body(&v["request"]);
    assert_eq!(request["action"], "event.attendance.read");
    assert!(valid_event_key(
        request["event_key"].as_str().expect("event_key str")
    ));
    let read = &v["results"]["read"];
    assert_eq!(read["v"], 1);
    assert_eq!(read["event_key"], request["event_key"]);
    assert_eq!(read["threshold_minutes"], 10);
    assert!(read["peak_concurrent"].as_u64().is_some());
    let window = read["window"].as_object().expect("window").clone();
    assert_timestamp_field(&window, "starts_at");
    assert_timestamp_field(&window, "ends_at");
    for room in read["rooms"].as_array().expect("rooms array") {
        assert!(is_snowflake(
            room["channel_id"].as_str().expect("channel_id str")
        ));
        assert!(!room["name_at_close"]
            .as_str()
            .expect("name_at_close str")
            .is_empty());
    }
    for member in read["members"].as_array().expect("members array") {
        assert!(is_snowflake(
            member["discord_id"].as_str().expect("discord_id str")
        ));
        assert!(member["minutes"].as_u64().is_some());
        assert!(member["sessions"].as_u64().is_some());
        let mut owned = Map::new();
        owned.insert("first".to_owned(), member["first_joined_at"].clone());
        assert_timestamp_field(&owned, "first");
    }
    assert_eq!(read["final"], true);
    assert_eq!(v["results"]["not_found"]["outcome"], "not_found");
}

#[test]
fn botlog_read_shapes_hold() {
    let fx = fixture();
    let v = verb(&fx, "botlog.read");
    let request = body(&v["request"]);
    assert_eq!(request["action"], "botlog.read");
    assert!(!request["after"].as_str().expect("after str").is_empty());
    let limit = request["limit"].as_u64().expect("limit number");
    assert!((1..=200).contains(&limit), "limit fits the 200-entry cap");
    let kinds: Vec<&str> = v["kinds"]
        .as_array()
        .expect("kinds array")
        .iter()
        .map(|k| k.as_str().expect("kind str"))
        .collect();
    let page = &v["results"]["page"];
    assert_eq!(page["v"], 1);
    let entries = page["entries"].as_array().expect("entries array");
    assert!(!entries.is_empty());
    assert!((entries.len() as u64) <= limit);
    for entry in entries {
        assert!(!entry["seq"].as_str().expect("seq str").is_empty());
        let mut owned = Map::new();
        owned.insert("at".to_owned(), entry["at"].clone());
        assert_timestamp_field(&owned, "at");
        let kind = entry["kind"].as_str().expect("kind str");
        assert!(kinds.contains(&kind), "{kind} is a publishable kind");
        if let Some(subject) = entry.get("subject_id") {
            assert!(is_snowflake(subject.as_str().expect("subject_id str")));
        }
    }
    assert!(!page["next"].as_str().expect("next str").is_empty());
}
