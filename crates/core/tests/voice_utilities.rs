//! Hermetic V10 acceptance cases against the public utility core.

use proptest::prelude::*;
use two_bot_core::voice_utilities::{
    invite_render, is_valid_invite_code, myroom_render, ping_render, INVALID_INVITE_CODE,
    MAX_INVITE_CODE_CHARS, MAX_PING_DISPLAY_MS, MIN_INVITE_CODE_CHARS, MYROOM_CREATION_GUIDANCE,
    MYROOM_EMPTY, NO_INVITE_CONFIGURED,
};

#[test]
fn ping_zero_renders_milliseconds() {
    assert_eq!(ping_render(0), "Pong! 0ms");
}

#[test]
fn ping_second_boundary() {
    assert_eq!(ping_render(999), "Pong! 999ms");
    assert_eq!(ping_render(1_000), "Pong! 1.0s");
    assert_eq!(ping_render(1_500), "Pong! 1.5s");
}

#[test]
fn ping_saturation_boundary() {
    assert_eq!(ping_render(MAX_PING_DISPLAY_MS), "Pong! 30.0s");
    assert_eq!(ping_render(MAX_PING_DISPLAY_MS + 1), "Pong! 30.0s+");
}

#[test]
fn ping_absurd_value_saturates_without_panicking() {
    assert_eq!(ping_render(u64::MAX), "Pong! 30.0s+");
}

#[test]
fn invite_none_is_unconfigured() {
    assert_eq!(invite_render(None), NO_INVITE_CONFIGURED);
}

#[test]
fn invite_empty_is_unconfigured() {
    assert_eq!(invite_render(Some("")), NO_INVITE_CONFIGURED);
}

#[test]
fn invite_valid_code_renders_link() {
    let out = invite_render(Some("aBc123-_"));
    assert_eq!(out, "Join the server: https://discord.gg/aBc123-_");
}

#[test]
fn invite_length_bounds() {
    let min = "a".repeat(MIN_INVITE_CODE_CHARS);
    let max = "a".repeat(MAX_INVITE_CODE_CHARS);
    assert!(is_valid_invite_code(&min));
    assert!(is_valid_invite_code(&max));
    assert!(!is_valid_invite_code(
        &"a".repeat(MIN_INVITE_CODE_CHARS - 1)
    ));
    assert!(!is_valid_invite_code(
        &"a".repeat(MAX_INVITE_CODE_CHARS + 1)
    ));
}

#[test]
fn invite_overlong_code_refused_without_echo() {
    let code = "a".repeat(MAX_INVITE_CODE_CHARS + 8);
    let out = invite_render(Some(&code));
    assert_eq!(out, INVALID_INVITE_CODE);
    assert!(!out.contains(&code));
}

#[test]
fn invite_url_and_malformed_refused_without_echo() {
    for code in [
        "https://discord.gg/abc123",
        "discord.gg/abc123",
        "abc 123",
        "abc.123",
        "abc/123",
        "abc?x=1",
        "ünïcode1",
        "abc!123",
    ] {
        assert!(!is_valid_invite_code(code), "{code} must be invalid");
        let out = invite_render(Some(code));
        assert_eq!(out, INVALID_INVITE_CODE, "{code} must be refused");
        assert!(!out.contains(code), "{code} must not be echoed");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Every round-trip time renders a short line; only inputs above the cap
    /// carry the saturation marker.
    #[test]
    fn ping_always_bounded(rtt in any::<u64>()) {
        let out = ping_render(rtt);
        prop_assert!(out.len() <= 32, "unbounded ping line: {out}");
        prop_assert_eq!(out.ends_with('+'), rtt > MAX_PING_DISPLAY_MS);
    }

    /// Invalid codes are never echoed: the reply is exactly one of the fixed
    /// notices, and only valid codes appear in the output.
    #[test]
    fn invite_never_echoes_invalid(code in ".*") {
        let out = invite_render(Some(&code));
        if is_valid_invite_code(&code) {
            prop_assert!(out.contains(&code));
        } else if code.is_empty() {
            prop_assert_eq!(out, NO_INVITE_CONFIGURED);
        } else {
            prop_assert_eq!(out, INVALID_INVITE_CODE);
        }
    }
}

#[test]
fn myroom_empty_names_no_room_and_the_next_action() {
    let out = myroom_render(&[], false);
    assert_eq!(out, MYROOM_EMPTY);
    let lower = out.to_ascii_lowercase();
    assert!(lower.contains("no") && lower.contains("own"));
    assert!(lower.contains("room"));
    assert!(!lower.contains("<#"), "no channel may be invented: {out}");
    // Creation guidance is conditional: no lobby promise when creation is
    // not actually open.
    assert!(
        !lower.contains("join"),
        "no join guidance without an open creator path: {out}"
    );
    let guided = myroom_render(&[], true);
    assert_eq!(guided, format!("{MYROOM_EMPTY}{MYROOM_CREATION_GUIDANCE}"));
    let guided = guided.to_ascii_lowercase();
    assert!(guided.contains("creator") && guided.contains("join"));
}

#[test]
fn myroom_lists_each_room_as_a_channel_reference_only() {
    // Channel references only: no privacy label or ACL classification.
    let out = myroom_render(&[500, 501], false);
    assert_eq!(out, "You own 2 live voice rooms:\n<#500>\n<#501>");
    assert!(!out.contains("private") && !out.contains("public"));
}

#[test]
fn myroom_singular_counts_one_room() {
    let out = myroom_render(&[502], true);
    assert_eq!(out, "You own 1 live voice room:\n<#502>");
}
