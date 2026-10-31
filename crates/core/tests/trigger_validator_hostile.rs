//! Hostile corpus for the trigger-name validators.
//!
//! Pins the registration-gate boundary for `valid_command_name` and
//! `valid_text_trigger`: every previously-valid shape stays accepted (no
//! accept-set shrink), every hostile shape is refused, refused inputs never
//! pass `validate_put_input`, and refusal errors never echo the submitted
//! content. The intended grammar is the legacy one: a name is
//! `[a-z0-9_-]{1,32}` and a text trigger is `!` followed by a name.

use std::collections::HashSet;

use two_bot_core::custom_commands::{
    error_code, validate_put_input, CommandError, PutCommandInput,
};
use two_bot_core::leveling::{valid_command_name, valid_text_trigger};

#[test]
fn previously_valid_names_stay_accepted() {
    let ceiling = "a".repeat(32);
    let mixed_ceiling = "z9_-".repeat(8);
    for name in [
        "faq",
        "a-b_c9",
        "a",
        "0",
        "9lives",
        "-",
        "_",
        "-x-",
        // Underscore runs are inside the accepted charset even though they
        // look like markdown emphasis; rendering is a downstream concern.
        "__faq__",
        ceiling.as_str(),
        mixed_ceiling.as_str(),
    ] {
        assert!(
            valid_command_name(name),
            "name must stay accepted: {name:?}"
        );
    }
}

#[test]
fn previously_valid_triggers_stay_accepted() {
    let ceiling = format!("!{}", "a".repeat(32));
    for trigger in ["!faq", "!a", "!9lives", "!__faq__", ceiling.as_str()] {
        assert!(
            valid_text_trigger(trigger),
            "trigger must stay accepted: {trigger:?}"
        );
    }
}

#[test]
fn empty_and_whitespace_only_names_are_refused() {
    for name in [
        "", " ", "  ", "\t", "\n", "\r\n", "\u{00A0}", // no-break space
        "\u{2003}", // em space
        "\u{FEFF}", // zero-width no-break space / BOM
    ] {
        assert!(!valid_command_name(name), "name must be refused: {name:?}");
    }
}

#[test]
fn overlong_names_are_refused() {
    for len in [33_usize, 64, 100_000] {
        let name = "a".repeat(len);
        assert!(!valid_command_name(&name), "overlong name must be refused");
        let trigger = format!("!{name}");
        assert!(
            !valid_text_trigger(&trigger),
            "overlong trigger must be refused"
        );
    }
}

#[test]
fn uppercase_names_and_triggers_are_refused() {
    for name in ["FAQ", "Faq", "faQ"] {
        assert!(!valid_command_name(name), "name must be refused: {name:?}");
        let trigger = format!("!{name}");
        assert!(
            !valid_text_trigger(&trigger),
            "trigger must be refused: {trigger:?}"
        );
    }
}

#[test]
fn mixed_script_confusable_names_are_refused() {
    for name in [
        "f\u{0430}q",               // Cyrillic small a
        "f\u{03BF}q",               // Greek small omicron
        "\u{FF46}\u{FF41}\u{FF51}", // fullwidth faq
        "\u{212A}ick",              // Kelvin sign folds to k
        "\u{0130}ck",               // Latin capital I with dot above
        "f\u{0251}q",               // Latin alpha
        "b\u{0430}n",               // ban with Cyrillic a
        "f\u{0301}aq",              // combining acute accent
    ] {
        assert!(!valid_command_name(name), "name must be refused: {name:?}");
        let trigger = format!("!{name}");
        assert!(
            !valid_text_trigger(&trigger),
            "trigger must be refused: {trigger:?}"
        );
    }
}

#[test]
fn mention_payload_names_are_refused() {
    for name in [
        "<@123>",
        "<@!123>",
        "<@&123>",
        "<#123>",
        "@everyone",
        "@here",
    ] {
        assert!(!valid_command_name(name), "name must be refused: {name:?}");
        let trigger = format!("!{name}");
        assert!(
            !valid_text_trigger(&trigger),
            "trigger must be refused: {trigger:?}"
        );
    }
}

#[test]
fn control_character_names_are_refused() {
    for payload in [
        "fa\u{0000}q", // NUL
        "fa\u{0007}q", // BEL
        "fa\u{001B}q", // ESC
        "fa\u{007F}q", // DEL
        "fa\u{0085}q", // NEL
        "fa\u{200B}q", // zero-width space
        "fa\u{202E}q", // right-to-left override
        "fa\u{2066}q", // left-to-right isolate
        "fa\u{FEFF}q", // BOM mid-name
        "faq\n",
        "faq\r",
        "faq\t",
    ] {
        assert!(
            !valid_command_name(payload),
            "name must be refused: {payload:?}"
        );
        let trigger = format!("!{payload}");
        assert!(
            !valid_text_trigger(&trigger),
            "trigger must be refused: {trigger:?}"
        );
    }
}

#[test]
fn markdown_injection_names_are_refused() {
    for name in [
        "*faq*", "**faq**", "`faq`", "||faq||", "#faq", ">faq", "[faq](x)", "~faq~", "\\faq",
        "faq!", "!faq", "f aq", " faq", "faq ",
    ] {
        assert!(!valid_command_name(name), "name must be refused: {name:?}");
    }
}

#[test]
fn punctuation_and_symbol_names_are_refused() {
    for name in [
        ".",
        "faq.",
        "f.aq",
        "a/b",
        "a\\b",
        "a:b",
        "a;b",
        "a,b",
        "a+b",
        "a=b",
        "a?b",
        "a&b",
        "a%b",
        "a$b",
        "a^b",
        "a(b",
        "a)b",
        "a\u{1F600}b",
    ] {
        assert!(!valid_command_name(name), "name must be refused: {name:?}");
    }
}

#[test]
fn malformed_triggers_are_refused() {
    for trigger in [
        "",
        "!",
        "faq",
        "!!faq",
        "x!faq",
        "\u{FF01}faq", // fullwidth exclamation mark
        "?faq",
        "/faq",
        " !faq",
        "! faq",
        "!faq ",
        "!\nfaq",
        "!\u{202E}faq", // bidi override right after the bang
    ] {
        assert!(
            !valid_text_trigger(trigger),
            "trigger must be refused: {trigger:?}"
        );
    }
}

#[test]
fn newline_smuggled_mentions_are_refused() {
    for trigger in ["!faq\n@everyone", "!faq\r@here", "!faq\u{000B}@everyone"] {
        assert!(
            !valid_text_trigger(trigger),
            "trigger must be refused: {trigger:?}"
        );
    }
}

#[test]
fn validators_do_not_panic_on_boundary_inputs() {
    // Inputs whose second byte is not a char boundary, replacement chars,
    // and other shapes that would panic a naive byte-slicing validator.
    // Completing these calls at all is half the assertion.
    for text in [
        "\u{00E9}",
        "\u{00E9}faq",
        "!\u{00E9}",
        "\u{FFFD}",
        "!\u{FFFD}",
        "!\u{1F600}",
        "\u{1F600}faq",
    ] {
        assert!(!valid_command_name(text), "name must be refused: {text:?}");
        assert!(
            !valid_text_trigger(text),
            "trigger must be refused: {text:?}"
        );
    }
}

fn input(name: &str, text_trigger: Option<&str>) -> PutCommandInput {
    PutCommandInput {
        name: name.to_owned(),
        description: "A helpful command.".to_owned(),
        template: "hello".to_owned(),
        text_trigger: text_trigger.map(str::to_owned),
    }
}

#[test]
fn refused_names_never_reach_registration() {
    let builtins = HashSet::new();
    for name in [
        "",
        " ",
        "FAQ",
        "f\u{0430}q",
        "<@123>",
        "@everyone",
        "fa\u{0000}q",
        "*faq*",
        "faq\n",
    ] {
        assert_eq!(
            validate_put_input(&input(name, None), &builtins),
            Err(CommandError::InvalidName),
            "hostile name must be refused at the gate: {name:?}"
        );
    }
    let overlong = "a".repeat(33);
    assert_eq!(
        validate_put_input(&input(&overlong, None), &builtins),
        Err(CommandError::InvalidName),
        "overlong name must be refused at the gate"
    );
}

#[test]
fn refused_triggers_never_reach_registration() {
    let builtins = HashSet::new();
    for trigger in [
        "",
        "!",
        "faq",
        "!!faq",
        "!\nfaq",
        "!f\u{0430}q",
        "!<@123>",
        "!*faq*",
        "!faq\n@everyone",
    ] {
        assert_eq!(
            validate_put_input(&input("faq", Some(trigger)), &builtins),
            Err(CommandError::InvalidTextTrigger),
            "hostile trigger must be refused at the gate: {trigger:?}"
        );
    }
    let overlong = format!("!{}", "a".repeat(33));
    assert_eq!(
        validate_put_input(&input("faq", Some(&overlong)), &builtins),
        Err(CommandError::InvalidTextTrigger),
        "overlong trigger must be refused at the gate"
    );
}

#[test]
fn refusal_errors_do_not_echo_submitted_content() {
    let builtins = HashSet::new();
    let marker = "qxz9marker";

    let hostile_name = format!("{marker}\n@everyone");
    let name_err = validate_put_input(&input(&hostile_name, None), &builtins);
    assert_eq!(name_err, Err(CommandError::InvalidName));
    if let Err(err) = &name_err {
        assert!(
            !err.to_string().contains(marker),
            "name refusal must not echo the submission"
        );
        assert!(
            !error_code(err).contains(marker),
            "name refusal code must not echo the submission"
        );
    }

    let hostile_trigger = format!("!{marker}\n@everyone");
    let trigger_err = validate_put_input(&input("faq", Some(&hostile_trigger)), &builtins);
    assert_eq!(trigger_err, Err(CommandError::InvalidTextTrigger));
    if let Err(err) = &trigger_err {
        assert!(
            !err.to_string().contains(marker),
            "trigger refusal must not echo the submission"
        );
        assert!(
            !error_code(err).contains(marker),
            "trigger refusal code must not echo the submission"
        );
    }
}

#[test]
fn valid_input_still_registers() {
    let builtins = HashSet::new();
    assert_eq!(
        validate_put_input(&input("faq", Some("!faq")), &builtins),
        Ok(())
    );
    assert_eq!(validate_put_input(&input("faq", None), &builtins), Ok(()));
}
