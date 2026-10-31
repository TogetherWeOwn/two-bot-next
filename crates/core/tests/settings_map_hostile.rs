//! Acceptance corpus: hostile dashboard settings maps against the pure
//! `from_map` parsers (`AssistantConfig`, `ModerationGates`, `DisableGates`).
//!
//! Written from the documented contracts only (module docs on each parser,
//! `docs/command-wiring-security.md` TA-04, and the fuzz target's stated
//! properties). Every parser must fail closed — refused-or-ignored per its
//! existing contract — with no panic, hang or allocation beyond the input,
//! and no rendering of supplied values in errors or `Debug` output. Valid
//! maps must parse byte-identically; unknown keys must be ignored.
//!
//! Run: `python3 scripts/cargo_cache.py run -- test -p two-bot-core --test settings_map_hostile`

use std::collections::HashMap;

use two_bot_core::disable_preflight::DisableGates;
use two_bot_core::moderation::{ModerationGateError, ModerationGates};
use two_bot_core::voice_assistant::{AssistantConfig, MAX_ENDPOINT_CHARS};

const MARKER: &str = "hostilemarker7f3a";
const VALID_ENDPOINT: &str = "https://assistant.example.test/v1";
const VALID_OWEN: &str = "123456789012345678";
const VALID_ROLE: &str = "234567890123456789";

fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn huge() -> String {
    "a".repeat(1_000_000)
}

// --- AssistantConfig -------------------------------------------------------

#[test]
fn assistant_valid_map_byte_identical() {
    let config = AssistantConfig::from_map(&map(&[
        ("TWO_ASSISTANT_ENDPOINT", VALID_ENDPOINT),
        ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
        ("TWO_ASSISTANT_MODEL", "model-x"),
    ]));
    let Some(config) = config else {
        panic!("valid map must enable the assistant");
    };
    assert_eq!(config.endpoint(), VALID_ENDPOINT);
    assert_eq!(config.model(), "model-x");
}

#[test]
fn assistant_valid_map_trims_surrounding_whitespace() {
    let config = AssistantConfig::from_map(&map(&[
        (
            "TWO_ASSISTANT_ENDPOINT",
            "  https://assistant.example.test  ",
        ),
        ("TWO_ASSISTANT_MODEL", "  model-y\t"),
        ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
    ]));
    let Some(config) = config else {
        panic!("trimmed valid map must enable the assistant");
    };
    assert_eq!(config.endpoint(), "https://assistant.example.test");
    assert_eq!(config.model(), "model-y");
}

#[test]
fn assistant_missing_or_blank_endpoint_disabled() {
    for vars in [
        map(&[]),
        map(&[("TWO_ASSISTANT_MODEL", "model-x")]),
        map(&[("TWO_ASSISTANT_ENDPOINT", "")]),
        map(&[("TWO_ASSISTANT_ENDPOINT", "   ")]),
        map(&[("TWO_ASSISTANT_ENDPOINT", "\t\n ")]),
    ] {
        assert_eq!(AssistantConfig::from_map(&vars), None);
    }
}

#[test]
fn assistant_non_http_or_scheme_only_endpoints_disabled() {
    for endpoint in [
        "ftp://assistant.example.test",
        "javascript:alert",
        "assistant.example.test",
        "//assistant.example.test",
        "http:/\\assistant.example.test",
        "http://",
        "https://",
        "https://   ",
        "https:assistant.example.test",
        "null",
        "true",
        "123",
        "{}",
        "[]",
    ] {
        assert_eq!(
            AssistantConfig::from_map(&map(&[
                ("TWO_ASSISTANT_ENDPOINT", endpoint),
                ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
            ])),
            None,
            "endpoint {endpoint:?} must stay disabled"
        );
    }
}

#[test]
fn assistant_control_characters_in_endpoint_disabled() {
    for endpoint in [
        "https://assistant.example.test/\u{0}",
        "https://assistant.\u{1}example.test",
        "https://assistant.example.test/\u{7f}",
        "https://assistant.example.test/a\nb",
        "https://assistant.example.test/a\rb",
        "\u{2}https://assistant.example.test",
        "https://assistant.example.test/\u{1b}[0m",
    ] {
        assert_eq!(
            AssistantConfig::from_map(&map(&[
                ("TWO_ASSISTANT_ENDPOINT", endpoint),
                ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
            ])),
            None,
            "control characters must disable, never redirect"
        );
    }
}

#[test]
fn assistant_endpoint_length_boundary_in_chars() {
    // Pad a path, not a hostname: a huge Unicode label hits the IDNA
    // parser's Punycode bound before it can exercise the endpoint char bound.
    let prefix = format!("{VALID_ENDPOINT}/");
    let pad = MAX_ENDPOINT_CHARS - prefix.chars().count();
    let at_bound = format!("{prefix}{}", "a".repeat(pad));
    assert_eq!(at_bound.chars().count(), MAX_ENDPOINT_CHARS);
    assert!(url::Url::parse(&at_bound).is_ok());
    assert!(
        AssistantConfig::from_map(&map(&[
            ("TWO_ASSISTANT_ENDPOINT", &at_bound),
            ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
        ]))
        .is_some(),
        "endpoint exactly at MAX_ENDPOINT_CHARS must be accepted"
    );
    assert_eq!(
        AssistantConfig::from_map(&map(&[
            ("TWO_ASSISTANT_ENDPOINT", &at_bound),
            ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
        ]))
        .expect("ASCII endpoint at the char bound enables")
        .endpoint(),
        at_bound
    );
    let over_bound = format!("{at_bound}a");
    assert_eq!(over_bound.chars().count(), MAX_ENDPOINT_CHARS + 1);
    assert!(url::Url::parse(&over_bound).is_ok());
    assert_eq!(
        AssistantConfig::from_map(&map(&[
            ("TWO_ASSISTANT_ENDPOINT", &over_bound),
            ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
        ])),
        None,
        "endpoint one char over the bound must be refused, not truncated"
    );
    // The bound is in characters, not bytes.
    let multibyte = format!("{prefix}{}", "é".repeat(pad));
    assert_eq!(multibyte.chars().count(), MAX_ENDPOINT_CHARS);
    assert!(multibyte.len() > MAX_ENDPOINT_CHARS);
    assert!(url::Url::parse(&multibyte).is_ok());
    assert!(
        AssistantConfig::from_map(&map(&[
            ("TWO_ASSISTANT_ENDPOINT", &multibyte),
            ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
        ]))
        .is_some(),
        "multibyte endpoint at the char bound must be accepted"
    );
    assert_eq!(
        AssistantConfig::from_map(&map(&[
            ("TWO_ASSISTANT_ENDPOINT", &multibyte),
            ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
        ]))
        .expect("multibyte endpoint at the char bound enables")
        .endpoint(),
        multibyte
    );
    let multibyte_over_bound = format!("{multibyte}é");
    assert_eq!(multibyte_over_bound.chars().count(), MAX_ENDPOINT_CHARS + 1);
    assert!(url::Url::parse(&multibyte_over_bound).is_ok());
    assert_eq!(
        AssistantConfig::from_map(&map(&[
            ("TWO_ASSISTANT_ENDPOINT", &multibyte_over_bound),
            ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
        ])),
        None,
        "multibyte endpoint one char over the bound must be refused, not truncated"
    );
}

#[test]
fn assistant_huge_endpoint_disabled() {
    let big = format!("https://{}", huge());
    assert_eq!(
        AssistantConfig::from_map(&map(&[
            ("TWO_ASSISTANT_ENDPOINT", &big),
            ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
        ])),
        None
    );
}

#[test]
fn assistant_huge_model_never_gates_and_stays_bounded_by_input() {
    let big_model = huge();
    let config = AssistantConfig::from_map(&map(&[
        ("TWO_ASSISTANT_ENDPOINT", VALID_ENDPOINT),
        ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
        ("TWO_ASSISTANT_MODEL", &big_model),
    ]));
    let Some(config) = config else {
        panic!("the model never gates the assistant (documented contract)");
    };
    assert_eq!(config.endpoint(), VALID_ENDPOINT);
    assert!(config.model().len() <= big_model.len());
}

#[test]
fn assistant_unknown_keys_ignored() {
    let base = map(&[
        ("TWO_ASSISTANT_ENDPOINT", VALID_ENDPOINT),
        ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
        ("TWO_ASSISTANT_MODEL", "model-x"),
    ]);
    let mut noisy = base.clone();
    for (k, v) in [
        ("TWO_ASSISTANT_ENDPOINTS", "https://evil.example.test"),
        ("two_assistant_endpoint", "https://evil.example.test"),
        ("TWO_ASSISTANT_ENDPOINT ", "https://evil.example.test"),
        (" TWO_ASSISTANT_ENDPOINT", "https://evil.example.test"),
        ("", VALID_ENDPOINT),
        ("DISCORD_TOKEN", MARKER),
    ] {
        noisy.insert(k.to_owned(), v.to_owned());
    }
    assert_eq!(
        AssistantConfig::from_map(&noisy),
        AssistantConfig::from_map(&base)
    );
    // Unknown keys alone never enable anything.
    let only_unknown = map(&[("two_assistant_endpoint", VALID_ENDPOINT)]);
    assert_eq!(AssistantConfig::from_map(&only_unknown), None);
}

#[test]
fn assistant_debug_never_renders_supplied_values() {
    // TA-04: even an endpoint path may carry credentials. Userinfo is now
    // refused by TA-02; use an admitted path/model to exercise Debug itself.
    let config = AssistantConfig::from_map(&map(&[
        (
            "TWO_ASSISTANT_ENDPOINT",
            &format!("https://assistant.example.test/{MARKER}/v1"),
        ),
        ("TWO_ASSISTANT_MODEL", MARKER),
        ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
    ]));
    let Some(config) = config else {
        panic!("path sentinel with a bound credential must enable the assistant");
    };
    for rendered in [format!("{config:?}"), format!("{config:#?}")] {
        assert!(
            !rendered.contains(MARKER),
            "Debug output must not render supplied endpoint/model values"
        );
    }
}

// --- ModerationGates -------------------------------------------------------

#[test]
fn moderation_valid_maps_byte_identical() {
    let Ok(gates) = ModerationGates::from_map(&map(&[])) else {
        panic!("empty map parses to defaults");
    };
    assert!(!gates.enabled);
    assert_eq!(gates.owen_user_id, "");
    assert!(gates.protected_role_ids.is_empty());

    let Ok(gates) = ModerationGates::from_map(&map(&[
        ("TWO_MODERATION", "1"),
        ("TWO_OWEN_USER_ID", VALID_OWEN),
        (
            "TWO_MODERATION_PROTECTED_ROLE_IDS",
            &format!("{VALID_ROLE},345678901234567890"),
        ),
    ])) else {
        panic!("valid enabled map must parse");
    };
    assert!(gates.enabled);
    assert_eq!(gates.owen_user_id, VALID_OWEN);
    assert_eq!(gates.protected_role_ids.len(), 2);
    assert!(gates.protected_role_ids.contains(VALID_ROLE));
}

#[test]
fn moderation_flag_strictly_one() {
    for value in ["true", "yes", "0", "01", " 1", "1 ", "1\n", "-1", "１", ""] {
        let Ok(gates) = ModerationGates::from_map(&map(&[
            ("TWO_MODERATION", value),
            ("TWO_OWEN_USER_ID", "not-a-snowflake"),
        ])) else {
            panic!("non-\"1\" flag means disabled; the owner id is validated only while enabled");
        };
        assert!(!gates.enabled, "flag value {value:?} must not enable");
    }
}

#[test]
fn moderation_enabled_requires_snowflake_owner() {
    let big_digits = "1".repeat(1_000_000);
    for owner in [
        "",
        "123",
        "1234567890123456",
        "123456789012345678901",
        "12345678901234567a",
        "+123456789012345678",
        " 123456789012345678",
        "123456789012345678 ",
        "12345678 9012345678",
        "１２３４５６７８９０１２３４５６７８",
        MARKER,
        &big_digits,
    ] {
        assert_eq!(
            ModerationGates::from_map(&map(&[
                ("TWO_MODERATION", "1"),
                ("TWO_OWEN_USER_ID", owner),
            ])),
            Err(ModerationGateError::InvalidOwenUserId),
            "owner of length {} must be refused",
            owner.len()
        );
    }
    // Snowflake length window: 17 and 20 digits both accepted.
    for owner in ["12345678901234567", "12345678901234567890"] {
        assert!(
            ModerationGates::from_map(&map(&[
                ("TWO_MODERATION", "1"),
                ("TWO_OWEN_USER_ID", owner),
            ]))
            .is_ok(),
            "owner length {} is in the snowflake window",
            owner.len()
        );
    }
}

#[test]
fn moderation_protected_roles_validated_even_when_disabled() {
    assert_eq!(
        ModerationGates::from_map(&map(&[(
            "TWO_MODERATION_PROTECTED_ROLE_IDS",
            "not-a-role-id"
        )])),
        Err(ModerationGateError::InvalidProtectedRoleId)
    );
}

#[test]
fn moderation_protected_roles_hostile_shapes_refused() {
    let big_token = "7".repeat(1_000_000);
    for roles in [
        format!("{VALID_ROLE},,345678901234567890"),
        format!("{VALID_ROLE},"),
        format!(",{VALID_ROLE}"),
        ",".to_owned(),
        " ".to_owned(),
        format!(" {VALID_ROLE}"),
        format!("{VALID_ROLE} "),
        format!("{VALID_ROLE}, 345678901234567890"),
        format!("{VALID_ROLE};345678901234567890"),
        format!("{VALID_ROLE}\n345678901234567890"),
        big_token,
    ] {
        assert_eq!(
            ModerationGates::from_map(&map(&[(
                "TWO_MODERATION_PROTECTED_ROLE_IDS",
                roles.as_str()
            )])),
            Err(ModerationGateError::InvalidProtectedRoleId),
            "protected-role list starting {:?} must be refused",
            &roles[..roles.len().min(24)]
        );
    }
}

#[test]
fn moderation_protected_roles_duplicates_deduplicate() {
    let repeated = format!("{VALID_ROLE},{VALID_ROLE},{VALID_ROLE}");
    let Ok(gates) = ModerationGates::from_map(&map(&[(
        "TWO_MODERATION_PROTECTED_ROLE_IDS",
        repeated.as_str(),
    )])) else {
        panic!("duplicate ids are not hostile");
    };
    assert_eq!(gates.protected_role_ids.len(), 1);
}

#[test]
fn moderation_huge_repeated_role_list_bounded_by_dedup() {
    // ~1 MB of the same valid id: parsing must complete without retaining
    // more than the distinct set.
    let repeated = format!("{VALID_ROLE},").repeat(50_000);
    let roles = repeated.trim_end_matches(',');
    let Ok(gates) =
        ModerationGates::from_map(&map(&[("TWO_MODERATION_PROTECTED_ROLE_IDS", roles)]))
    else {
        panic!("repeated valid ids must parse");
    };
    assert_eq!(gates.protected_role_ids.len(), 1);
}

#[test]
fn moderation_errors_never_render_supplied_values() {
    let marker_owner = format!("{MARKER}12345");
    let Err(err) = ModerationGates::from_map(&map(&[
        ("TWO_MODERATION", "1"),
        ("TWO_OWEN_USER_ID", marker_owner.as_str()),
    ])) else {
        panic!("hostile owner id must error");
    };
    for rendered in [format!("{err}"), format!("{err:?}")] {
        assert!(
            !rendered.contains(MARKER),
            "error output must not echo the supplied value"
        );
    }
    let Err(err) =
        ModerationGates::from_map(&map(&[("TWO_MODERATION_PROTECTED_ROLE_IDS", MARKER)]))
    else {
        panic!("hostile role id must error");
    };
    for rendered in [format!("{err}"), format!("{err:?}")] {
        assert!(
            !rendered.contains(MARKER),
            "error output must not echo the supplied value"
        );
    }
}

#[test]
fn moderation_unknown_keys_ignored() {
    let base = map(&[("TWO_MODERATION", "0")]);
    let mut noisy = base.clone();
    for (k, v) in [
        ("two_moderation", "1"),
        ("TWO_MODERATION ", "1"),
        ("TWO_OWEN_USER_IDS", MARKER),
        ("", "1"),
    ] {
        noisy.insert(k.to_owned(), v.to_owned());
    }
    assert_eq!(
        ModerationGates::from_map(&noisy),
        ModerationGates::from_map(&base)
    );
}

// --- DisableGates ----------------------------------------------------------

#[test]
fn disable_valid_maps_byte_identical() {
    assert_eq!(
        DisableGates::from_map(&map(&[])),
        DisableGates {
            moderation: false,
            automations: false
        }
    );
    assert_eq!(
        DisableGates::from_map(&map(&[("TWO_MODERATION", "1"), ("TWO_AUTOMATIONS", "1")])),
        DisableGates {
            moderation: true,
            automations: true
        }
    );
}

#[test]
fn disable_flags_strictly_one_and_independent() {
    let big = huge();
    for value in [
        "true",
        "yes",
        "0",
        "01",
        " 1",
        "1 ",
        "1\n",
        "-1",
        "１",
        "",
        big.as_str(),
    ] {
        let gates = DisableGates::from_map(&map(&[("TWO_MODERATION", value)]));
        assert!(
            !gates.moderation,
            "value of length {} must not enable",
            value.len()
        );
        assert!(!gates.automations);
        let gates = DisableGates::from_map(&map(&[("TWO_AUTOMATIONS", value)]));
        assert!(!gates.automations);
        assert!(!gates.moderation);
    }
    // Each flag answers only to its own key.
    let gates = DisableGates::from_map(&map(&[("TWO_MODERATION", "1")]));
    assert!(gates.moderation);
    assert!(!gates.automations);
}

#[test]
fn disable_malformed_companion_values_do_not_change_semantics() {
    // Documented: raw flag reads only; a malformed companion value must not
    // change disable semantics (typed loaders reject those on their own).
    let gates = DisableGates::from_map(&map(&[
        ("TWO_MODERATION", "1"),
        ("TWO_AUTOMATIONS", "1"),
        ("TWO_FEED_POLL_SECONDS", "not-a-number"),
        ("TWO_OWEN_USER_ID", MARKER),
        ("TWO_MODERATION_PROTECTED_ROLE_IDS", "@@@"),
    ]));
    assert_eq!(
        gates,
        DisableGates {
            moderation: true,
            automations: true
        }
    );
}

#[test]
fn disable_unknown_keys_ignored() {
    let gates = DisableGates::from_map(&map(&[
        ("two_moderation", "1"),
        ("TWO_MODERATION ", "1"),
        ("", "1"),
    ]));
    assert_eq!(
        gates,
        DisableGates {
            moderation: false,
            automations: false
        }
    );
}

// --- Cross-cutting ---------------------------------------------------------

#[test]
fn large_map_of_unknown_keys_changes_no_outcome() {
    // ~1 MB of unknown keys around the valid known keys: every parser's
    // outcome must equal the small-map outcome (no half-application, no
    // blow-up beyond the input).
    let mut big = map(&[
        ("TWO_ASSISTANT_ENDPOINT", VALID_ENDPOINT),
        ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
        ("TWO_MODERATION", "1"),
        ("TWO_OWEN_USER_ID", VALID_OWEN),
        ("TWO_AUTOMATIONS", "1"),
    ]);
    let filler = "x".repeat(100);
    for i in 0..10_000 {
        big.insert(format!("UNKNOWN_KEY_{i}"), filler.clone());
    }
    let small = map(&[
        ("TWO_ASSISTANT_ENDPOINT", VALID_ENDPOINT),
        ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
        ("TWO_MODERATION", "1"),
        ("TWO_OWEN_USER_ID", VALID_OWEN),
        ("TWO_AUTOMATIONS", "1"),
    ]);
    assert_eq!(
        AssistantConfig::from_map(&big),
        AssistantConfig::from_map(&small)
    );
    assert_eq!(
        ModerationGates::from_map(&big),
        ModerationGates::from_map(&small)
    );
    assert_eq!(DisableGates::from_map(&big), DisableGates::from_map(&small));
}
