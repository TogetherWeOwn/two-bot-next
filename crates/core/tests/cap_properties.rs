//! Runtime-cap wire-format properties: moderation ceilings, schedule windows,
//! sticky debounce, and UTF-16-unit LFG/scheduled-event labels.
//!
//! The parser/builder properties pinned elsewhere cover shapes; this suite pins
//! the runtime caps documented in `docs/property-tests.md` at the wire level
//! (JSON numbers/strings and the published command bounds Discord enforces).
//! Synthetic fixtures only: no Discord, network, or database. Each property
//! runs 64 cases, exercises both inclusive edges plus adjacent refusals every
//! run, and leaves proptest shrinking and failure persistence at their defaults
//! so regressions are recorded.

use proptest::prelude::*;
use serde_json::{json, Map, Value};
use two_bot_core::{
    automation_commands, build_channel_keys, moderation_commands, normalize_debounce,
    parse_role_spec, validate_event_input, validate_moderation_numbers, validate_title,
    CommandDefinition, ModerationAction,
};

/// Tempban ceiling: 365 days in seconds (internal runtime validation).
const TEMPBAN_MAX: i64 = 365 * 24 * 60 * 60;
/// Timeout ceiling: 28 days in seconds (Discord's own ceiling).
const TIMEOUT_MAX: i64 = 28 * 24 * 60 * 60;
/// Floor both duration verbs share (builders and runtime agree here).
const DURATION_MIN: i64 = 60;

fn event_body(name: &str, description: Option<&str>) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert("name".to_owned(), json!(name));
    body.insert("starts_at".to_owned(), json!("2026-10-01T18:00:00Z"));
    body.insert("ends_at".to_owned(), json!("2026-10-01T20:00:00Z"));
    body.insert("channel_key".to_owned(), json!("ann"));
    if let Some(description) = description {
        body.insert("description".to_owned(), json!(description));
    }
    body
}

fn channel_keys() -> std::collections::HashMap<String, String> {
    build_channel_keys("ann:333333333333333333").expect("valid fixture keys")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn property_tempban_timeout_wire_caps_enforce_inclusive_edges(value in any::<i64>()) {
        // Builders and runtime validators share both duration bounds, so pin
        // each verb's advertised ceiling as well as its runtime edges.
        let definitions = moderation_commands();
        for (name, max) in [("tempban", TEMPBAN_MAX), ("timeout", TIMEOUT_MAX)] {
            let definition = definitions
                .iter()
                .find(|d| d.name == name)
                .expect("verb published");
            let duration = definition
                .options
                .iter()
                .find(|o| o.name == "duration_seconds")
                .expect("duration option published");
            prop_assert_eq!(
                (duration.min_value, duration.max_value),
                (Some(DURATION_MIN), Some(max))
            );
        }
        for (action, max) in [
            (ModerationAction::TempBan, TEMPBAN_MAX),
            (ModerationAction::Timeout, TIMEOUT_MAX),
        ] {
            // Every run pins both exact edges, not only the random i64 (which
            // almost never lands inside the accepted range).
            for n in [
                DURATION_MIN - 1,
                DURATION_MIN,
                DURATION_MIN + 1,
                max - 1,
                max,
                max + 1,
                value,
            ] {
                let number = json!(n);
                let accepted =
                    validate_moderation_numbers(action, Some(&number), None, None).is_ok();
                prop_assert_eq!(accepted, (DURATION_MIN..=max).contains(&n));
                // Wire strings never coerce the way legacy `Number()` did.
                let coerced = json!(n.to_string());
                prop_assert!(
                    validate_moderation_numbers(action, Some(&coerced), None, None).is_err()
                );
            }
            // Missing and non-integer wire values refuse.
            prop_assert!(validate_moderation_numbers(action, None, None, None).is_err());
            for invalid in [
                Value::Null,
                json!(true),
                json!(1.5),
                json!([1]),
                json!({"n": 1}),
            ] {
                prop_assert!(
                    validate_moderation_numbers(action, Some(&invalid), None, None).is_err()
                );
            }
        }
    }

    #[test]
    fn property_schedule_windows_publish_exact_wire_bounds(value in any::<i64>()) {
        let definitions = automation_commands();
        let schedule = definitions
            .iter()
            .find(|d| d.name == "schedule")
            .expect("schedule published");
        // The published shape is the enforcement: Discord clamps what callers
        // can send, so the wire round trip must preserve the bounds exactly.
        let wire = serde_json::to_value(&definitions).expect("commands serialize");
        let decoded: Vec<CommandDefinition> =
            serde_json::from_value(wire).expect("commands deserialize");
        prop_assert_eq!(&decoded, &definitions);
        for (option, min, max) in
            [("in-minutes", 1i64, 525_600i64), ("every-minutes", 60i64, 525_600i64)]
        {
            let published = schedule
                .options
                .iter()
                .find(|o| o.name == option)
                .expect("timing option published");
            prop_assert_eq!(
                (published.min_value, published.max_value),
                (Some(min), Some(max))
            );
            for n in [min - 1, min, min + 1, max - 1, max, max + 1, value] {
                let advertised = published.min_value.is_none_or(|lower| n >= lower)
                    && published.max_value.is_none_or(|upper| n <= upper);
                prop_assert_eq!(advertised, (min..=max).contains(&n));
            }
        }
    }

    #[test]
    fn property_sticky_debounce_wire_caps_enforce_inclusive_edges(value in any::<i64>()) {
        let definitions = automation_commands();
        let sticky = definitions
            .iter()
            .find(|d| d.name == "sticky")
            .expect("sticky published");
        let debounce = sticky
            .options
            .iter()
            .find(|o| o.name == "debounce")
            .expect("debounce option published");
        prop_assert_eq!(
            (debounce.min_value, debounce.max_value),
            (Some(1), Some(300))
        );
        // Omitted callers get the legacy default; anything outside 1–300 refuses.
        prop_assert_eq!(normalize_debounce(None), Ok(5));
        for n in [0, 1, 2, 299, 300, 301, value] {
            prop_assert_eq!(
                normalize_debounce(Some(n)).is_ok(),
                (1i64..=300).contains(&n)
            );
        }
    }

    #[test]
    fn property_lfg_caps_enforce_count_slots_and_utf16_labels(
        count in 0usize..=22,
        slots in -2i32..=102,
        label in proptest::collection::vec(prop::sample::select(vec!['a', 'é', '😀']), 0..=110),
        title in proptest::collection::vec(prop::sample::select(vec!['a', 'é', '😀']), 0..=120),
    ) {
        // Role count: 1–20 accept, 0 and 21+ refuse.
        let spec = (0..count)
            .map(|i| format!("r{i}:Role:1"))
            .collect::<Vec<_>>()
            .join(",");
        prop_assert_eq!(parse_role_spec(&spec).is_ok(), (1..=20).contains(&count));
        for n in [0usize, 1, 2, 19, 20, 21, 22] {
            let spec = (0..n)
                .map(|i| format!("r{i}:Role:1"))
                .collect::<Vec<_>>()
                .join(",");
            prop_assert_eq!(parse_role_spec(&spec).is_ok(), (1..=20).contains(&n));
        }
        // Slots: 1–99 accept, 0/negatives and 100+ refuse.
        let spec = format!("role:Label:{slots}");
        prop_assert_eq!(
            parse_role_spec(&spec).is_ok(),
            (1..=99).contains(&slots)
        );
        for n in [-2i32, -1, 0, 1, 2, 98, 99, 100, 101, 102] {
            let spec = format!("role:Label:{n}");
            prop_assert_eq!(parse_role_spec(&spec).is_ok(), (1..=99).contains(&n));
        }
        // Labels and titles count UTF-16 units like legacy `length`: an astral
        // scalar costs two units, so scalar counts alone mislead.
        let label: String = label.into_iter().collect();
        prop_assert_eq!(
            parse_role_spec(&format!("role:{label}:1")).is_ok(),
            (1..=80).contains(&label.encode_utf16().count())
        );
        let title: String = title.into_iter().collect();
        prop_assert_eq!(
            validate_title(&title).is_ok(),
            (1..=100).contains(&title.encode_utf16().count())
        );
        // Astral proof: 41 emoji are 82 units (refuse) while 80 BMP scalars
        // accept; 51 emoji are 102 title units (refuse) while 100 accept.
        for (text, accepted) in [
            ("a".repeat(79), true),
            ("a".repeat(80), true),
            ("a".repeat(81), false),
            ("😀".repeat(39), true),
            ("😀".repeat(40), true),
            ("😀".repeat(41), false),
        ] {
            prop_assert_eq!(
                parse_role_spec(&format!("role:{text}:1")).is_ok(),
                accepted
            );
        }
        for (text, accepted) in [
            ("a".repeat(99), true),
            ("a".repeat(100), true),
            ("a".repeat(101), false),
            ("😀".repeat(50), true),
            ("😀".repeat(51), false),
        ] {
            prop_assert_eq!(validate_title(&text).is_ok(), accepted);
        }
    }

    #[test]
    fn property_schedule_labels_use_utf16_units(
        name in proptest::collection::vec(prop::sample::select(vec!['a', 'é', '😀']), 0..=120),
    ) {
        let keys = channel_keys();
        let name: String = name.into_iter().collect();
        prop_assert_eq!(
            validate_event_input(&event_body(name.as_str(), None), &keys).is_ok(),
            (1..=100).contains(&name.encode_utf16().count())
        );
        // Name edges: 99/100 accept, 101 refuses; 50 emoji are exactly 100
        // units (accept) while 51 are 102 (refuse) at only 51 scalars.
        for (text, accepted) in [
            ("a".repeat(99), true),
            ("a".repeat(100), true),
            ("a".repeat(101), false),
            ("😀".repeat(50), true),
            ("😀".repeat(51), false),
        ] {
            prop_assert_eq!(
                validate_event_input(&event_body(text.as_str(), None), &keys).is_ok(),
                accepted
            );
        }
        // Description ceiling likewise counts units, not scalars or bytes.
        for (text, accepted) in [
            ("a".repeat(999), true),
            ("a".repeat(1000), true),
            ("a".repeat(1001), false),
            ("😀".repeat(500), true),
            ("😀".repeat(501), false),
        ] {
            prop_assert_eq!(
                validate_event_input(&event_body("Party", Some(text.as_str())), &keys).is_ok(),
                accepted
            );
        }
    }
}
