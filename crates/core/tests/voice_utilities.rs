//! Hermetic V10 acceptance cases against the public utility core.

use proptest::prelude::*;
use two_bot_core::voice_utilities::{
    invite_render, is_valid_invite_code, ping_render, INVALID_INVITE_CODE, MAX_INVITE_CODE_CHARS,
    MAX_PING_DISPLAY_MS, MIN_INVITE_CODE_CHARS, NO_INVITE_CONFIGURED,
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
