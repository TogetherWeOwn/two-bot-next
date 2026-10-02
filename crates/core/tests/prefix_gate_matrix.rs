//! Parity §1 #23: `!<trigger>` text-trigger gate matrix.
//!
//! Acceptance layer over
//! [`two_bot_core::feature_commands::parse_prefix_trigger`], pinning the gate
//! (`TWO_TEXT_COMMANDS=1`, plumbed as the `text_commands_enabled` flag) and
//! builtin-exclusion matrix. The parser shape rows (unicode/whitespace edges,
//! case fold, length bound) live in `prefix_trigger.rs` and are not repeated
//! here; every input below is plain ASCII.
//!
//! Matrix:
//! 1. Gate OFF refuses every trigger, including valid non-builtin ones.
//! 2. Gate ON accepts valid non-builtin triggers.
//! 3. Every retained builtin slash name (core + scorecard + automation +
//!    announcement + moderation) is excluded as a trigger with the gate on.
//! 4. `!` anywhere but the first position refuses.
//! 5. Multi-word input resolves on the first token only.
//!
//! No gateway, store or Discord surface.

use std::collections::HashSet;

use two_bot_core::commands::core_commands;
use two_bot_core::feature_commands::{
    announcement_commands, automation_commands, parse_prefix_trigger, scorecard_attendance_command,
};
use two_bot_core::moderation::moderation_commands;

/// Every retained builtin slash name, mirroring the router's reserved set
/// (core + scorecard + automation + announcement + moderation). Same
/// composition as `prefix_trigger.rs`; the exclusion loop below is the
/// acceptance row (3), not a parser-shape repeat.
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

/// Row 1: gate OFF refuses every trigger, including valid non-builtin ones.
#[test]
fn gate_off_refuses_all_triggers() {
    let builtins = builtin_names();
    for text in [
        "!faq",
        "!rules-day_2",
        "!welcome",
        "!0",
        "!rank",
        "!ban",
        "!command",
        "!",
        "faq",
        "",
    ] {
        assert_eq!(
            parse_prefix_trigger(text, false, &builtins),
            None,
            "gate off refuses {text:?}"
        );
    }
}

/// Row 2: gate ON accepts valid non-builtin triggers.
#[test]
fn gate_on_accepts_valid_non_builtin_triggers() {
    let builtins = builtin_names();
    for (text, expected) in [
        ("!faq", "faq"),
        ("!rules-day_2", "rules-day_2"),
        ("!welcome", "welcome"),
        ("!0", "0"),
        ("!a-b_c9", "a-b_c9"),
    ] {
        assert_eq!(
            parse_prefix_trigger(text, true, &builtins).as_deref(),
            Some(expected),
            "gate on accepts {text:?}"
        );
    }
}

/// Row 3: every retained builtin slash name is excluded with the gate on.
#[test]
fn gate_on_excludes_every_builtin_name() {
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

/// Row 4: `!` anywhere but the first position refuses. (Inputs differ from
/// the leading-whitespace rows pinned in `prefix_trigger.rs`.)
#[test]
fn bang_not_in_first_position_refuses() {
    let builtins = builtin_names();
    for text in [" !faq", "x!faq", "say !faq please", "!!faq", "a !"] {
        assert_eq!(
            parse_prefix_trigger(text, true, &builtins),
            None,
            "off-first `!` refuses {text:?}"
        );
    }
}

/// Row 5: multi-word input resolves on the first token only. (Inputs differ
/// from the first-token rows pinned in `prefix_trigger.rs`.)
#[test]
fn multi_word_input_uses_first_token_only() {
    let builtins = builtin_names();
    for (text, expected) in [
        ("!welcome to the server", "welcome"),
        ("!rules  read them all", "rules"),
        ("!faq please", "faq"),
    ] {
        assert_eq!(
            parse_prefix_trigger(text, true, &builtins).as_deref(),
            Some(expected),
            "first token wins for {text:?}"
        );
    }
    // First token a builtin stays excluded even with trailing words.
    assert_eq!(parse_prefix_trigger("!rank please", true, &builtins), None);
}
