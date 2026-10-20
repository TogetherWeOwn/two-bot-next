#![no_main]

use libfuzzer_sys::fuzz_target;
use std::collections::HashSet;
use two_bot_core::lfg::{
    lfg_custom_id, lfg_select_options, parse_lfg_select, parse_role_spec, spec_roles,
    valid_role_key, LfgRoleSpec, LfgSelectAction, LfgStatus, RoleSpecError, LFG_LEAVE_VALUE,
    LFG_SELECT_PREFIX, MAX_LFG_ROLES, MAX_OPTION_LABEL_CHARS, MAX_ROLE_LABEL_CHARS, MAX_ROLE_SLOTS,
    MAX_ROLE_SPEC_CHARS,
};

/// Bounds keep every allocation a function of the (libFuzzer-capped) input,
/// following the `cutover_parse.rs` pattern: at most this many NUL-separated
/// fields are read (the last keeps any further NULs), each truncated to a
/// char cap before reaching a parser. NUL is the only separator so newlines,
/// mentions and markdown stay inside the field that carries them.
const MAX_FIELDS: usize = 4;
/// Post id, select value and raw `custom_id` fields. Discord caps real
/// components at 100 chars; the harness deliberately goes beyond both that
/// transport cap and the parser's 128-UTF-16-unit post-id bound.
const MAX_ID_CHARS: usize = 256;
/// The role-spec field may overshoot the published bound so `TooLong` and the
/// exact-boundary spec are both reachable.
const MAX_SPEC_CHARS: usize = MAX_ROLE_SPEC_CHARS + 64;
/// Mirrors the private `MAX_ROLE_KEY_ERROR_CHARS` in `lfg.rs`.
const MAX_ROLE_KEY_ERROR_CHARS: usize = 128;

/// Field order in the raw input: post id, select value, raw `custom_id`,
/// role spec. Missing trailing fields read as empty; extras stay in the last.
const POST_ID_IDX: usize = 0;
const VALUE_IDX: usize = 1;
const CUSTOM_ID_IDX: usize = 2;
const SPEC_IDX: usize = 3;

/// The wire format is a contract: live posts already carry these strings in
/// their components, so a rename strands every open post. Pinned as literals
/// rather than through the constants so drift fails the fuzzer.
const WIRE_PREFIX: &str = "two:lfg:";
const WIRE_LEAVE: &str = "__leave__";

/// ECMAScript WhiteSpace + LineTerminator, the legacy `trim()` set. An
/// independent copy so a label the parser fails to trim is caught here
/// instead of agreeing with itself through the canonical round trip.
fn is_ecmascript_space(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn truncate_chars(s: &str, cap: usize) -> String {
    s.chars().take(cap).collect()
}

/// Independent post-id contract, not the production validator or its constant.
fn post_id_in_bounds(post_id: &str) -> bool {
    (1..=128).contains(&post_id.encode_utf16().count())
}

/// `parse_lfg_select` against an independent oracle: foreign or forged
/// prefixes and empty/overlong post ids refuse. Valid ids after the one exact
/// prefix are carried byte-for-byte (no trim, fold, unescape or second strip).
fn check_select(custom_id: &str, value: &str) {
    let parsed = parse_lfg_select(custom_id, value);
    assert_eq!(
        parsed,
        parse_lfg_select(custom_id, value),
        "select parse is deterministic"
    );
    if !custom_id.as_bytes().starts_with(WIRE_PREFIX.as_bytes()) {
        assert_eq!(parsed, None, "foreign or forged prefixes are refused");
        return;
    }
    let rest = &custom_id[WIRE_PREFIX.len()..];
    if !post_id_in_bounds(rest) {
        assert_eq!(parsed, None, "post id must be 1..=128 UTF-16 units");
        return;
    }
    match parsed {
        Some(LfgSelectAction::Leave { post_id }) => {
            assert_eq!(value, WIRE_LEAVE, "leave only on the exact leave value");
            assert_eq!(post_id, rest, "post id is carried byte-exact");
        }
        Some(LfgSelectAction::Signup { post_id, role_key }) => {
            assert_ne!(value, WIRE_LEAVE, "the leave value never signs up");
            assert_eq!(post_id, rest, "post id is carried byte-exact");
            assert_eq!(role_key, value, "role key is carried byte-exact");
        }
        None => panic!("an exact prefix with a bounded, nonempty post id parses"),
    }
}

/// Every accepted spec must hold the invariants the store and the select menu
/// rely on, round-trip through its own canonical rendering, and mint select
/// options that parse back to the same role key.
fn check_accepted_spec(spec: &str, roles: &[LfgRoleSpec], post_id: &str) {
    assert!(
        (1..=MAX_LFG_ROLES).contains(&roles.len()),
        "role count stays in 1..=20"
    );
    assert_eq!(
        roles.len(),
        spec.split(',').count(),
        "one role per comma entry"
    );
    let mut keys = HashSet::new();
    for role in roles {
        assert!(valid_role_key(&role.key), "minted key is a valid key");
        assert_ne!(role.key, WIRE_LEAVE, "the leave value is never a role key");
        assert!(keys.insert(role.key.as_str()), "keys are unique");
        let label_units = role.label.encode_utf16().count();
        assert!(
            (1..=MAX_ROLE_LABEL_CHARS).contains(&label_units),
            "label is 1..=80 UTF-16 units"
        );
        assert!(
            !role.label.starts_with(is_ecmascript_space)
                && !role.label.ends_with(is_ecmascript_space),
            "label is trimmed"
        );
        assert!(
            !role.label.contains([',', ':']),
            "separators never reach a label"
        );
        assert!(
            (1..=MAX_ROLE_SLOTS).contains(&role.slots),
            "slots stay in 1..=99"
        );
    }

    // Canonical form is a fixed point: a label left untrimmed, a key left
    // unnormalized or a slot that does not survive decimal rendering would
    // re-parse to something else (or refuse).
    let canonical = roles
        .iter()
        .map(|role| format!("{}:{}:{}", role.key, role.label, role.slots))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        parse_role_spec(&canonical).as_deref(),
        Ok(roles),
        "canonical rendering re-parses to the same roles"
    );

    // Select menu consistency: bounded post ids preserve every minted role
    // key; invalid ids refuse even otherwise valid options. The leave entry
    // cannot collide with a role, regardless of post-id validity.
    let custom_id = lfg_custom_id(post_id);
    let rows = spec_roles(post_id, roles);
    let options = lfg_select_options(LfgStatus::Open, &rows, &[]);
    assert_eq!(
        options.len(),
        roles.len() + 1,
        "one option per role + leave"
    );
    assert!(
        lfg_select_options(LfgStatus::Closed, &rows, &[]).is_empty(),
        "closed posts render no select"
    );
    let mut values = HashSet::new();
    for option in &options {
        assert!(values.insert(option.value.as_str()), "option values unique");
    }
    for (option, role) in options.iter().zip(roles) {
        assert_eq!(option.value, role.key);
        check_select(&custom_id, &option.value);
        assert_eq!(
            parse_lfg_select(&custom_id, &option.value),
            post_id_in_bounds(post_id).then(|| LfgSelectAction::Signup {
                post_id: post_id.to_owned(),
                role_key: role.key.clone(),
            }),
            "a role option signs up for exactly that role iff the post id is valid"
        );
    }
    let leave = options.last().expect("leave option is last");
    assert_eq!(leave.value, WIRE_LEAVE);
    check_select(&custom_id, &leave.value);
    assert_eq!(
        parse_lfg_select(&custom_id, &leave.value),
        post_id_in_bounds(post_id).then(|| LfgSelectAction::Leave {
            post_id: post_id.to_owned(),
        }),
        "the leave option leaves iff the post id is valid"
    );
}

/// `check_accepted_spec` only sees labels the parser accepts (at most 80 UTF-16
/// units, so the 100-unit option cap cannot bite there). A row built directly
/// around a fuzzed label of up to `MAX_ID_CHARS` chars makes the cap real.
fn check_option_label_cap(post_id: &str, label: &str) {
    let role = LfgRoleSpec {
        key: "k".to_owned(),
        label: label.to_owned(),
        slots: MAX_ROLE_SLOTS,
    };
    let options = lfg_select_options(LfgStatus::Open, &spec_roles(post_id, &[role]), &[]);
    assert_eq!(options.len(), 2, "one role option + leave");
    assert_eq!(options[0].value, "k");
    assert!(
        options[0].label.encode_utf16().count() <= MAX_OPTION_LABEL_CHARS,
        "option label stays within the select cap"
    );
}

fn check_spec(spec: &str, post_id: &str) {
    let too_long = spec.encode_utf16().count() > MAX_ROLE_SPEC_CHARS;
    let parts = spec.split(',').count();
    let parsed = parse_role_spec(spec);
    assert_eq!(parsed, parse_role_spec(spec), "role spec is deterministic");

    // Refusal before parsing: `TooLong` if and only if the raw spec exceeds
    // the published UTF-16 bound; the count check is the next gate.
    assert_eq!(
        matches!(parsed, Err(RoleSpecError::TooLong)),
        too_long,
        "TooLong iff the spec exceeds MAX_ROLE_SPEC_CHARS"
    );
    if !too_long && parts > MAX_LFG_ROLES {
        assert_eq!(parsed, Err(RoleSpecError::BadCount));
    }
    match &parsed {
        Err(RoleSpecError::BadCount) => assert!(parts > MAX_LFG_ROLES),
        Err(RoleSpecError::BadKey(key)) => assert!(
            key.chars().count() <= MAX_ROLE_KEY_ERROR_CHARS,
            "BadKey echoes at most a bounded raw prefix"
        ),
        Err(RoleSpecError::ReservedKey(key)) => assert_eq!(key, WIRE_LEAVE),
        Err(RoleSpecError::DuplicateKey(key)) => assert!(valid_role_key(key)),
        Err(_) => {}
        Ok(roles) => {
            assert!(!too_long);
            check_accepted_spec(spec, roles, post_id);
        }
    }
}

fuzz_target!(|data: &[u8]| {
    // Input-independent base cases. Any parser behavior change must update
    // this target on its own card; silent drift fails the fuzzer instead.
    assert_eq!(LFG_SELECT_PREFIX, WIRE_PREFIX);
    assert_eq!(LFG_LEAVE_VALUE, WIRE_LEAVE);
    assert_eq!(lfg_custom_id("abc123"), "two:lfg:abc123");
    assert_eq!(
        parse_lfg_select("two:lfg:abc123", "tank"),
        Some(LfgSelectAction::Signup {
            post_id: "abc123".to_owned(),
            role_key: "tank".to_owned(),
        })
    );
    assert_eq!(
        parse_lfg_select("two:lfg:abc123", "__leave__"),
        Some(LfgSelectAction::Leave {
            post_id: "abc123".to_owned(),
        })
    );
    // Empty/unknown role keys still reach the store for a valid post id.
    for role_key in ["", "unknown-role", " __leave__", "__leave__ "] {
        assert_eq!(
            parse_lfg_select("two:lfg:abc123", role_key),
            Some(LfgSelectAction::Signup {
                post_id: "abc123".to_owned(),
                role_key: role_key.to_owned(),
            })
        );
    }
    assert_eq!(parse_lfg_select("two:lfg:", ""), None);
    // A doubled prefix strips once; the leave value is exact-match only.
    assert_eq!(
        parse_lfg_select("two:lfg:two:lfg:x", " __leave__"),
        Some(LfgSelectAction::Signup {
            post_id: "two:lfg:x".to_owned(),
            role_key: " __leave__".to_owned(),
        })
    );
    // Literal validity pins distinguish UTF-16 units from bytes/scalars and
    // count only the suffix, without trimming or stripping a second prefix.
    for (post_id, valid) in [
        (String::new(), false),
        ("a".repeat(129), false),
        ("é".repeat(129), false),
        ("😀".repeat(65), false),
        ("a".repeat(128), true),
        ("😀".repeat(64), true),
        ("é".repeat(128), true),
        ("x".to_owned(), true),
        (" \t".to_owned(), true),
        ("two:lfg:x".to_owned(), true),
    ] {
        assert_eq!(post_id_in_bounds(&post_id), valid);
        let custom_id = format!("two:lfg:{post_id}");
        assert_eq!(lfg_custom_id(&post_id), custom_id);
        for role_key in ["tank", "", "unknown-role", " __leave__", "__leave__ "] {
            check_select(&custom_id, role_key);
            assert_eq!(
                parse_lfg_select(&custom_id, role_key),
                valid.then(|| LfgSelectAction::Signup {
                    post_id: post_id.clone(),
                    role_key: role_key.to_owned(),
                })
            );
        }
        check_select(&custom_id, WIRE_LEAVE);
        assert_eq!(
            parse_lfg_select(&custom_id, WIRE_LEAVE),
            valid.then(|| LfgSelectAction::Leave {
                post_id: post_id.clone(),
            })
        );
        check_spec("tank:Tank:2,healer:Healer:2", &post_id);
    }
    for forged in [
        "",
        "two:lfg",
        "TWO:LFG:x",
        " two:lfg:x",
        "xtwo:lfg:x",
        "two-lfg:x",
        "two:rsvp:x",
        "\u{200b}two:lfg:x",
    ] {
        assert_eq!(parse_lfg_select(forged, "tank"), None, "{forged:?}");
    }
    let canonical = "tank:Tank:2,healer:Healer:2,dps:DPS:6";
    assert_eq!(parse_role_spec(canonical).map(|roles| roles.len()), Ok(3));
    // Key and label are trimmed, the key is lowercased, slots may be radix.
    assert_eq!(
        parse_role_spec(" TANK : Tank \u{3000}: 0x2 "),
        Ok(vec![LfgRoleSpec {
            key: "tank".to_owned(),
            label: "Tank".to_owned(),
            slots: 2,
        }])
    );
    assert_eq!(
        parse_role_spec("__leave__:Leave:1"),
        Err(RoleSpecError::ReservedKey("__leave__".to_owned()))
    );
    assert_eq!(
        parse_role_spec("a:A:1,A:B:2"),
        Err(RoleSpecError::DuplicateKey("a".to_owned()))
    );
    let twenty: Vec<String> = (0..MAX_LFG_ROLES).map(|i| format!("k{i}:K{i}:1")).collect();
    assert_eq!(
        parse_role_spec(&twenty.join(",")).map(|roles| roles.len()),
        Ok(MAX_LFG_ROLES)
    );
    let twenty_one: Vec<String> = (0..=MAX_LFG_ROLES)
        .map(|i| format!("k{i}:K{i}:1"))
        .collect();
    assert_eq!(
        parse_role_spec(&twenty_one.join(",")),
        Err(RoleSpecError::BadCount)
    );
    assert!(parse_role_spec("").is_err());

    // Discord strings are UTF-8. Invalid UTF-8 is discarded, matching the
    // other `&str` harnesses.
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    let fields: Vec<String> = raw
        .splitn(MAX_FIELDS, '\0')
        .enumerate()
        .map(|(idx, field)| {
            truncate_chars(
                field,
                if idx == SPEC_IDX {
                    MAX_SPEC_CHARS
                } else {
                    MAX_ID_CHARS
                },
            )
        })
        .collect();
    assert!(fields.len() <= MAX_FIELDS);
    let field = |idx: usize| fields.get(idx).map(String::as_str).unwrap_or("");
    let post_id = field(POST_ID_IDX);
    let value = field(VALUE_IDX);

    // No database, Discord client, network or secret is touched below; only
    // these pure functions run.
    //
    // A well-formed custom id: the real builder over a fuzzed post id.
    let custom_id = lfg_custom_id(post_id);
    assert_eq!(custom_id.len(), WIRE_PREFIX.len() + post_id.len());
    check_select(&custom_id, value);
    // The fuzzed post id and value reach the parser unbuilt as well, so
    // forged, doubled and foreign prefixes stay reachable without the builder.
    check_select(field(CUSTOM_ID_IDX), value);
    check_select(post_id, field(CUSTOM_ID_IDX));
    check_select(&custom_id, WIRE_LEAVE);
    check_option_label_cap(post_id, value);

    check_spec(field(SPEC_IDX), post_id);
});
