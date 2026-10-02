//! Parity §1 #23: pure `!<trigger>` prefix parser.
//!
//! Table tests for
//! [`two_bot_core::feature_commands::parse_prefix_trigger`]: first token
//! only, legacy trigger shape, builtin-name exclusion, gate-off rejection,
//! and unicode/whitespace edges. No gateway, store or Discord surface.

use std::collections::HashSet;

use two_bot_core::commands::core_commands;
use two_bot_core::feature_commands::{
    announcement_commands, automation_commands, parse_prefix_trigger, scorecard_attendance_command,
};
use two_bot_core::moderation::moderation_commands;

/// Every retained builtin slash name, mirroring the router's reserved set
/// (core + scorecard + automation + announcement + moderation).
fn builtin_names() -> HashSet<String> {
    core_commands()
        .into_iter()
        .chain(std::iter::once(scorecard_attendance_command()))
        .chain(automation_commands())
        .chain(announcement_commands())
        .chain(moderation_commands())
        .map(|def| def.name)
        .collect()
}

#[test]
fn parity_row_23_parse_and_reject_table() {
    let builtins = builtin_names();
    // (input, expected bare name)
    let cases: &[(&str, Option<&str>)] = &[
        // Happy path: first token only, `!` stripped.
        ("!faq", Some("faq")),
        ("!faq extra words stay out", Some("faq")),
        ("!has space", Some("has")),
        ("!0", Some("0")),
        ("!-a_9", Some("-a_9")),
        ("!a-b_c9", Some("a-b_c9")),
        // Leading/trailing ASCII whitespace is ignored; only first token.
        ("  !faq  ", Some("faq")),
        ("\t!faq\nmore", Some("faq")),
        ("!faq\nmore", Some("faq")),
        // Unicode spaces (NBSP, narrow NBSP, ideographic space) split the
        // same way (`char::is_whitespace`).
        ("\u{a0}!faq\u{202f}", Some("faq")),
        ("\u{3000}!faq\u{3000}", Some("faq")),
        // Not a trigger.
        ("", None),
        ("   ", None),
        ("faq", None),
        ("hi !faq", None),
        ("!", None),
        ("! ", None),
        ("!!faq", None),
        ("!faq!", None),
        ("!FAQ", None),
        ("!café", None),
        // Full-width exclamation (U+FF01) is not the ASCII `!` prefix.
        ("\u{ff01}faq", None),
        ("!faq\u{200b}more", None),
        ("!\u{a0}faq", None),
        // Builtin slash names are excluded even with the gate on.
        ("!rank", None),
        ("!ban", None),
        ("!command", None),
        ("!rsvp-attendance", None),
        ("!feed-add", None),
        ("!slowmode", None),
        // Near-misses of builtins still parse.
        ("!rank2", Some("rank2")),
        ("!bans", Some("bans")),
    ];
    for (text, expected) in cases {
        assert_eq!(
            parse_prefix_trigger(text, true, &builtins).as_deref(),
            *expected,
            "input {text:?}"
        );
    }
}

#[test]
fn gate_off_rejects_everything() {
    let builtins = builtin_names();
    for text in ["!faq", "!rank", "!a-b_c9", "  !faq  extra"] {
        assert_eq!(
            parse_prefix_trigger(text, false, &builtins),
            None,
            "gate off rejects {text:?}"
        );
    }
    // ... including with an empty builtin set.
    assert_eq!(parse_prefix_trigger("!faq", false, &HashSet::new()), None);
}

#[test]
fn every_builtin_name_is_excluded() {
    let builtins = builtin_names();
    assert!(!builtins.is_empty());
    for name in &builtins {
        let text = format!("!{name}");
        assert_eq!(
            parse_prefix_trigger(&text, true, &builtins),
            None,
            "builtin {name:?} excluded"
        );
    }
}

#[test]
fn name_length_bound_is_32_chars() {
    let builtins = HashSet::new();
    let max = format!("!{}", "a".repeat(32));
    assert_eq!(
        parse_prefix_trigger(&max, true, &builtins),
        Some("a".repeat(32))
    );
    let over = format!("!{}", "a".repeat(33));
    assert_eq!(parse_prefix_trigger(&over, true, &builtins), None);
}
