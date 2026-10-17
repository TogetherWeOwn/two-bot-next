//! Acceptance tests: settings-backed human/welcome channel classification.
//!
//! Protected test file: the build card implements against these tests and
//! must not edit them. If a pinned name below cannot be implemented as
//! written, stop and escalate instead of changing the tests.
//!
//! Today every message fact records `channelClass: "other"` because the
//! production pipeline hardcodes a no-op classifier, even though the
//! settings catalogue already carries `TWO_COMMUNITY_HUMAN_CHANNEL_IDS`
//! and `TWO_COMMUNITY_WELCOME_CHANNEL_IDS` (both `Hot`, stored unwired).
//! These tests fail on `main` before the build card starts.
//!
//! Contract pinned here (build card implements exactly this):
//! - `ClassifierConfig::from_map` (already infallible `-> Self`) also reads
//!   the two keys above into new public fields
//!   `human_channel_ids: HashSet<String>` and
//!   `welcome_channel_ids: HashSet<String>`, in the same env-string shape
//!   the existing `id_set` parser consumes (comma-separated, trimmed).
//! - Only Discord snowflakes survive: 17-20 ASCII digits, the rule the
//!   codebase already applies (`is_snowflake` in `internal_actions.rs` and
//!   `automod.rs`). Empty, malformed or non-snowflake entries are dropped
//!   silently; a bad entry never rejects its valid siblings and never
//!   makes `from_map` fail. Do not reuse the automod `snowflakes()` parser
//!   as-is: it returns `Err` for the whole value, which is the fatal
//!   behaviour this contract forbids.
//! - New method `ClassifierConfig::channel_class(&self, channel_id:
//!   Snowflake) -> ChannelClass`: `Welcome` when the channel is in the
//!   welcome set, else `Human` when in the human set, else `Other`.
//!   Welcome wins over human for a channel present in both sets.
//! - No new settings keys, no schema change, no classes beyond
//!   human/welcome/other. The class strings are the existing
//!   `ChannelClass::as_str` values, which the facts sink stamps into fact
//!   metadata as `channelClass`.

use std::collections::HashMap;

use two_bot_core::{ChannelClass, ClassifierConfig};

const HUMAN_CHANNEL: u64 = 200_000_000_000_000_001;
const HUMAN_CHANNEL_2: u64 = 200_000_000_000_000_002;
const WELCOME_CHANNEL: u64 = 300_000_000_000_000_001;
const UNLISTED_CHANNEL: u64 = 400_000_000_000_000_001;

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn configured() -> ClassifierConfig {
    ClassifierConfig::from_map(&vars(&[
        (
            "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
            "200000000000000001,200000000000000002",
        ),
        ("TWO_COMMUNITY_WELCOME_CHANNEL_IDS", "300000000000000001"),
    ]))
}

#[test]
fn human_and_welcome_sets_classify_and_unlisted_is_other() {
    let config = configured();

    assert!(config
        .human_channel_ids
        .contains(&HUMAN_CHANNEL.to_string()));
    assert!(config
        .human_channel_ids
        .contains(&HUMAN_CHANNEL_2.to_string()));
    assert!(config
        .welcome_channel_ids
        .contains(&WELCOME_CHANNEL.to_string()));

    assert_eq!(config.channel_class(HUMAN_CHANNEL), ChannelClass::Human);
    assert_eq!(config.channel_class(HUMAN_CHANNEL_2), ChannelClass::Human);
    assert_eq!(config.channel_class(WELCOME_CHANNEL), ChannelClass::Welcome);
    assert_eq!(
        config.channel_class(UNLISTED_CHANNEL),
        ChannelClass::Other,
        "a channel in neither set stays other"
    );
}

#[test]
fn channel_in_both_sets_classifies_as_welcome() {
    let config = ClassifierConfig::from_map(&vars(&[
        ("TWO_COMMUNITY_HUMAN_CHANNEL_IDS", "200000000000000001"),
        ("TWO_COMMUNITY_WELCOME_CHANNEL_IDS", "200000000000000001"),
    ]));

    assert_eq!(
        config.channel_class(HUMAN_CHANNEL),
        ChannelClass::Welcome,
        "welcome wins when a channel is listed as both human and welcome"
    );
}

#[test]
fn missing_and_empty_values_fall_back_to_other() {
    for map in [
        vars(&[]),
        vars(&[("TWO_COMMUNITY_HUMAN_CHANNEL_IDS", "")]),
        vars(&[("TWO_COMMUNITY_WELCOME_CHANNEL_IDS", "")]),
        vars(&[
            ("TWO_COMMUNITY_HUMAN_CHANNEL_IDS", ","),
            ("TWO_COMMUNITY_WELCOME_CHANNEL_IDS", " , , "),
        ]),
    ] {
        let config = ClassifierConfig::from_map(&map);
        assert!(config.human_channel_ids.is_empty(), "no human ids parsed");
        assert!(
            config.welcome_channel_ids.is_empty(),
            "no welcome ids parsed"
        );
        assert_eq!(config.channel_class(HUMAN_CHANNEL), ChannelClass::Other);
        assert_eq!(config.channel_class(WELCOME_CHANNEL), ChannelClass::Other);
    }
}

#[test]
fn malformed_and_non_snowflake_values_are_ignored_never_fatal() {
    // Each of these is present in the map, so `from_map` sees it; none may
    // be kept, and none may stop the valid sibling entry from classifying.
    for bad in [
        "abc",
        "general",
        "123",                     // digits, far too short
        "2000000000000000a",       // trailing letter
        "-200000000000000001",     // signed
        "+200000000000000001",     // signed
        "2.0e17",                  // float notation
        "<#200000000000000001>",   // channel-mention form is not a raw id
        "200 000 000 000 000 001", // grouped digits are not one id
        "200000000000000001\n300", // embedded newline junk
    ] {
        let value = format!("{bad},200000000000000002");
        let config =
            ClassifierConfig::from_map(&vars(&[("TWO_COMMUNITY_HUMAN_CHANNEL_IDS", &value)]));
        assert_eq!(
            config.human_channel_ids.len(),
            1,
            "only the valid sibling survives {bad:?}"
        );
        assert_eq!(
            config.channel_class(HUMAN_CHANNEL_2),
            ChannelClass::Human,
            "the valid sibling still classifies after {bad:?}"
        );
        assert_eq!(
            config.channel_class(HUMAN_CHANNEL),
            ChannelClass::Other,
            "the malformed entry itself never classifies: {bad:?}"
        );
    }
}

#[test]
fn snowflake_length_boundaries_match_the_codebase_rule() {
    // 17 and 20 digits are snowflakes; 16 and 21 are not.
    let config = ClassifierConfig::from_map(&vars(&[(
        "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
        "1000000000000001,10000000000000001,10000000000000000001,100000000000000000001",
    )]));

    assert_eq!(
        config.channel_class(1_000_000_000_000_001),
        ChannelClass::Other,
        "16 digits is not a snowflake"
    );
    assert_eq!(
        config.channel_class(10_000_000_000_000_001),
        ChannelClass::Human,
        "17 digits is the smallest snowflake"
    );
    assert_eq!(
        config.channel_class(10_000_000_000_000_000_001),
        ChannelClass::Human,
        "20 digits is the largest snowflake"
    );
    assert_eq!(
        config.human_channel_ids.len(),
        2,
        "the 21-digit entry is dropped, not kept"
    );
}

#[test]
fn whitespace_and_duplicates_are_normalised() {
    let config = ClassifierConfig::from_map(&vars(&[(
        "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
        " 200000000000000001 , 200000000000000001,200000000000000002 ",
    )]));

    assert_eq!(config.human_channel_ids.len(), 2, "duplicates collapse");
    assert_eq!(config.channel_class(HUMAN_CHANNEL), ChannelClass::Human);
    assert_eq!(config.channel_class(HUMAN_CHANNEL_2), ChannelClass::Human);
}

#[test]
fn default_config_classifies_everything_other() {
    let config = ClassifierConfig::default();

    assert!(config.human_channel_ids.is_empty());
    assert!(config.welcome_channel_ids.is_empty());
    assert_eq!(config.channel_class(HUMAN_CHANNEL), ChannelClass::Other);
    assert_eq!(config.channel_class(WELCOME_CHANNEL), ChannelClass::Other);
}

#[test]
fn class_strings_are_the_fact_metadata_strings() {
    // The deferred facts sink stamps `fact.channel_class.as_str()` into the
    // `channelClass` metadata field, so these exact strings are the
    // persisted contract, not display text.
    let config = configured();
    assert_eq!(config.channel_class(HUMAN_CHANNEL).as_str(), "human");
    assert_eq!(config.channel_class(WELCOME_CHANNEL).as_str(), "welcome");
    assert_eq!(config.channel_class(UNLISTED_CHANNEL).as_str(), "other");
}

#[test]
fn existing_classifier_fields_still_parse_unchanged() {
    // Guard: adding the channel sets must not disturb the fields the
    // scorecard classifier already reads from the same map.
    let config = ClassifierConfig::from_map(&vars(&[
        ("TWO_COMMUNITY_CLASSIFIER_VERSION", "community-v9"),
        ("TWO_COMMUNITY_AUTOMATION_ACTOR_IDS", "111,222"),
        ("TWO_COMMUNITY_HUMAN_CHANNEL_IDS", "200000000000000001"),
    ]));

    assert_eq!(config.version, "community-v9");
    assert!(config.automation_actor_ids.contains("111"));
    assert!(config.automation_actor_ids.contains("222"));
    assert_eq!(config.channel_class(HUMAN_CHANNEL), ChannelClass::Human);
}
