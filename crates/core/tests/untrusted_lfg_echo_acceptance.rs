//! Protected reply contract: presentation must not turn a rejected key into markup.
//! Expected strings are hand-derived, not computed by an escaping helper.

use two_bot_core::lfg::{parse_role_spec, RoleSpecError};

fn bad_key(key: &str) -> RoleSpecError {
    let error = parse_role_spec(&format!("{key}:Tank:2")).expect_err("invalid key must refuse");
    assert!(matches!(error, RoleSpecError::BadKey(_)), "{error:?}");
    error
}

#[test]
fn rejected_key_markdown_and_backticks_are_inert_but_readable() {
    for (key, escaped, stripped) in [
        ("**forged**", "\\*\\*forged\\*\\*", "forged"),
        ("`forged`", "\\`forged\\`", "forged"),
        (
            "[forged](example.org)",
            "\\[forged\\](example.org)",
            "forgedexample.org",
        ),
        ("back\\slash", "back\\\\slash", "backslash"),
    ] {
        let text = bad_key(key).to_string();
        let accepted = [
            format!("Invalid LFG role key \"{escaped}\"."),
            format!("Invalid LFG role key \"{stripped}\"."),
        ];
        assert!(
            accepted.contains(&text),
            "active or unreadable key echo: {text:?}"
        );
    }
}

#[test]
fn rejected_key_cannot_forge_a_new_reply_line() {
    let text = bad_key("bad\nforged\rline").to_string();
    assert_eq!(text, "Invalid LFG role key \"bad forged line\".");
}

#[test]
fn mention_shaped_key_keeps_its_identity_without_changing_the_refusal_class() {
    let text = bad_key("<@&8888>").to_string();
    // Inspect escaping preserves pills; inert text strips their brackets.
    // Actual Discord replies still require their existing empty allowed_mentions.
    assert!(
        [
            "Invalid LFG role key \"<@&8888>\".",
            "Invalid LFG role key \"@&8888\".",
        ]
        .contains(&text.as_str()),
        "{text:?}"
    );
}

#[test]
fn bounded_hostile_key_never_displays_the_unbounded_tail() {
    let key = format!("{}TAIL_SENTINEL", "`".repeat(160));
    let text = bad_key(&key).to_string();
    assert!(!text.contains("TAIL_SENTINEL"));
    assert!(text.encode_utf16().count() <= 2000);
    let escaped = format!("Invalid LFG role key \"{}\".", "\\`".repeat(128));
    assert!(
        text == escaped || text == "Invalid LFG role key \"\".",
        "bounded raw punctuation is still active: {text:?}"
    );
}

#[test]
fn ordinary_key_errors_and_refusal_variants_remain_byte_identical() {
    assert_eq!(
        bad_key("Tank!").to_string(),
        "Invalid LFG role key \"Tank!\"."
    );
    let key = "k".repeat(192);
    let error = bad_key(&key);
    assert_eq!(error, RoleSpecError::BadKey("k".repeat(128)));
    assert_eq!(
        error.to_string(),
        format!("Invalid LFG role key \"{}\".", "k".repeat(128))
    );
    assert_eq!(parse_role_spec("tank:Tank:0"), Err(RoleSpecError::BadSlots));
    assert_eq!(
        parse_role_spec("tank:Tank:2,tank:Other:1"),
        Err(RoleSpecError::DuplicateKey("tank".into()))
    );
    assert_eq!(
        parse_role_spec("__leave__:Leave:1"),
        Err(RoleSpecError::ReservedKey("__leave__".into()))
    );
    assert_eq!(
        parse_role_spec("tank:Tank:2").expect("ordinary valid key")[0].key,
        "tank"
    );
}
