//! Hostile-input regression for `parse_self_role_custom_id`.
//!
//! Member-controlled interaction payloads reach the parser unwrapped, so
//! arbitrary strings must never panic or hang, anything outside the
//! `two:self-role:` namespace must return `None`, and over-long or
//! multi-colon inputs stay rejected per `valid_key`. Pure offline cases
//! only: no Discord, network, database, or handler wiring.

use two_bot_core::self_roles::{parse_self_role_custom_id, self_role_custom_id, ParsedCustomId};

#[test]
fn builder_round_trips_through_parser() {
    assert_eq!(
        parse_self_role_custom_id(&self_role_custom_id("games", Some("chess"))),
        Some(ParsedCustomId {
            panel_id: "games".to_owned(),
            option_key: Some("chess".to_owned()),
        })
    );
    assert_eq!(
        parse_self_role_custom_id(&self_role_custom_id("games", None)),
        Some(ParsedCustomId {
            panel_id: "games".to_owned(),
            option_key: None,
        })
    );
}

#[test]
fn outside_namespace_returns_none() {
    for bad in [
        "",
        "other:games:chess",
        "two:self-role",
        "TWO:SELF-ROLE:games",
        " two:self-role:games",
        "two:lfg:abc123",
        "two:voice:kick-yes:99",
        "two:self-roleXgames",
        "two:self-role :games",
        "\u{200b}two:self-role:games",
    ] {
        assert_eq!(parse_self_role_custom_id(bad), None, "{bad:?}");
    }
}

#[test]
fn over_long_keys_rejected_per_valid_key() {
    let ok60 = "k".repeat(60);
    assert_eq!(
        parse_self_role_custom_id(&format!("two:self-role:{ok60}")),
        Some(ParsedCustomId {
            panel_id: ok60.clone(),
            option_key: None,
        })
    );
    assert_eq!(
        parse_self_role_custom_id(&format!("two:self-role:{ok60}:{ok60}")),
        Some(ParsedCustomId {
            panel_id: ok60.clone(),
            option_key: Some(ok60.clone()),
        })
    );
    // 61 bytes on either side is rejected (byte length, not chars).
    let long61 = "k".repeat(61);
    for bad in [
        format!("two:self-role:{long61}"),
        format!("two:self-role:{long61}:ok"),
        format!("two:self-role:ok:{long61}"),
        format!("two:self-role:{}:{}", ok60, long61),
    ] {
        assert_eq!(parse_self_role_custom_id(&bad), None, "{bad:?}");
    }
    // Multibyte UTF-8 only grows the byte count: 30 é (60 bytes) parses,
    // 31 é (62 bytes) is rejected even though both fail the charset first.
    assert_eq!(parse_self_role_custom_id("two:self-role:caf\u{e9}"), None);
    assert_eq!(
        parse_self_role_custom_id(&format!("two:self-role:{}", "é".repeat(31))),
        None
    );
    // A 512-char hostile run is still just a rejection, never a hang.
    assert_eq!(
        parse_self_role_custom_id(&format!("two:self-role:{}", "a".repeat(512))),
        None
    );
}

#[test]
fn multi_colon_inputs_rejected() {
    for bad in [
        "two:self-role:a:b:c",
        "two:self-role:games:chess:",
        "two:self-role:games::chess",
        "two:self-role:::",
        "two:self-role:games:chess:extra:more",
    ] {
        assert_eq!(parse_self_role_custom_id(bad), None, "{bad:?}");
    }
}

#[test]
fn key_shape_edges_rejected() {
    // Empty sides, bad first byte, and charset violations.
    for bad in [
        "two:self-role:",
        "two:self-role::chess",
        "two:self-role:games:",
        "two:self-role:Games",
        "two:self-role:games:CHESS!",
        "two:self-role:_panel",
        "two:self-role:-panel",
        "two:self-role:games:ch ess",
        "two:self-role:games:chess\n",
        "two:self-role:games\t:chess",
        "two:self-role:café",
        "two:self-role:games:\u{1f600}",
        "two:self-role:games:chess\u{0}",
        "two:self-role:games:CHESS",
        "two:self-role:GAMES:chess",
    ] {
        assert_eq!(parse_self_role_custom_id(bad), None, "{bad:?}");
    }
    // Allowed alphabet holds: digits, `_` and `-` after a lower/digit lead.
    for good in ["a", "0", "a0", "a_b-c9", "abc123", "0-_-0"] {
        assert_eq!(
            parse_self_role_custom_id(&format!("two:self-role:{good}")),
            Some(ParsedCustomId {
                panel_id: good.to_owned(),
                option_key: None,
            }),
            "{good:?}"
        );
    }
}

#[test]
fn hostile_battery_never_panics_and_only_valid_parses() {
    let hostile = [
        "",
        "\u{0}",
        "\n",
        ":",
        "::",
        ":::",
        "two:self-role:\u{0}",
        "two:self-role:games\u{0}:chess",
        "two:self-role:games:chess\u{0}",
        "two:self-role:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "two:self-role:a:b:c:d:e:f:g",
        "two:self-role:games::",
        "TWO:SELF-ROLE:GAMES:CHESS",
        "two:self-role:games:chess ",
        " two:self-role:games:chess",
        "two:self-role:games:chess\nmore",
        "two:self-role:\u{1f600}",
        "two:self-role:games:\u{1f600}",
        "<:chess:666666666666666666>",
        "two:self-role:games:chess\x7f",
        "two:self-role:UPPER:lower",
    ];
    for input in hostile {
        // Every hostile fixture is outside the panel namespace or violates
        // `valid_key`: rejection (`None`) is the only acceptable outcome.
        // The call itself is the no-panic proof.
        assert_eq!(parse_self_role_custom_id(input), None, "{input:?}");
    }
    // Determinism: hostile inputs parse the same way twice.
    for input in [
        "two:self-role:games:chess",
        "two:self-role:a:b:c",
        "\u{0}\u{0}\u{0}",
    ] {
        assert_eq!(
            parse_self_role_custom_id(input),
            parse_self_role_custom_id(input),
            "{input:?}"
        );
    }
}
