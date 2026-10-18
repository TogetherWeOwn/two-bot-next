//! Acceptance: a scheduled-message body can never become a ping or
//! link ambush when the ticker posts it.
//!
//! Hermetic half of the battery, against the public schedule API only
//! (`validate_schedule` / `ScheduleInput`, plus the shared
//! `message_safety` predicates the outbound path itself uses). No REST,
//! timer, or SQL calls are made here; the DB-backed ticker half lives in
//! the bot crate's scheduled-jobs tests and captures the channel call.
//!
//! Contract under test, stated implementation-agnostically: for every
//! hostile body, `validate_schedule` either refuses it outright, or the
//! body it hands to storage is already inert — no mass mention that the
//! shared neutralizer would still rewrite, and no contiguous
//! `discord.gg` invite host that Discord would linkify. Inertness is a
//! property, not a pinned spelling: zero-width splits, escaping,
//! rewriting, or refusal all satisfy it. Benign bodies are the opposite
//! pin: they must validate and pass through byte-identical.
//!
//! Deliberately unpinned (the paired build may choose either way):
//! non-invite URLs, and user/role mention pills (`<@123>` / `<@&123>`),
//! whose ping is governed by `allowed_mentions` at the channel boundary
//! and is asserted there, in the ticker tests.

use two_bot_core::{
    message_safety::{contains_mass_mention, has_message_text, text_len},
    validate_schedule, ScheduleInput,
};

fn validated(body: &str) -> Option<String> {
    validate_schedule(&ScheduleInput {
        body,
        in_minutes: Some(10),
        every_minutes: None,
    })
    .ok()
    .map(|schedule| schedule.body)
}

/// Inert on the wire: nothing left for the shared mention neutralizer to
/// rewrite (which covers raw and zero-width-split mass mentions), and no
/// contiguous invite host for Discord to turn into a clickable pill.
fn assert_inert(output: &str, case: &str) {
    assert!(
        !contains_mass_mention(output),
        "{case:?}: stored body still carries a mass mention: {output:?}"
    );
    assert!(
        !output.contains("@everyone") && !output.contains("@here"),
        "{case:?}: stored body still contains a pingable mass mention: {output:?}"
    );
    let lower = output.to_lowercase();
    assert!(
        !lower.contains("discord.gg"),
        "{case:?}: stored body still contains a linkable invite host: {output:?}"
    );
    assert!(
        has_message_text(output),
        "{case:?}: neutralized body must still be sendable text, got {output:?}"
    );
    assert!(
        text_len(output) <= 2000,
        "{case:?}: neutralized body exceeds the 2000-unit ceiling: {} units",
        text_len(output)
    );
}

/// Hostile payloads, each embedded in ordinary announcement text exactly
/// the way an ambush would be: a fix that blanks the whole message
/// instead of defusing the payload fails the fragment-preservation
/// check below, while a refusal at validation stays acceptable.
fn hostile_bodies() -> Vec<String> {
    let payloads = [
        "@everyone".to_owned(),
        "@here".to_owned(),
        "@everyone @here".to_owned(),
        // Zero-width-split obfuscations, one per invisible separator the
        // shared safety module names, split after `@` and inside the word.
        "@e\u{200b}veryone".to_owned(),
        "@ev\u{200c}eryone".to_owned(),
        "@he\u{200d}re".to_owned(),
        "@\u{2060}here".to_owned(),
        "@\u{feff}everyone".to_owned(),
        "@e\u{200b}v\u{200b}e\u{200b}r\u{200b}y\u{200b}o\u{200b}n\u{200b}e".to_owned(),
        // Bare and disguised invite links.
        "discord.gg/free-nitro".to_owned(),
        "discord.gg".to_owned(),
        "www.discord.gg/free-nitro".to_owned(),
        "https://discord.gg/free-nitro".to_owned(),
        "DISCORD.GG/FREE-NITRO".to_owned(),
        // Combined ambush: ping plus invite in one body.
        "@here claim at discord.gg/free-nitro".to_owned(),
    ];
    payloads
        .into_iter()
        .map(|payload| format!("Game night {payload} bring snacks"))
        .collect()
}

#[test]
fn hostile_bodies_are_refused_or_stored_inert() {
    for body in hostile_bodies() {
        let Some(stored) = validated(&body) else {
            continue; // Refused at the validate boundary: acceptable.
        };
        assert_inert(&stored, &body);
        assert!(
            stored.contains("Game night") && stored.contains("bring snacks"),
            "{body:?}: neutralization must defuse the payload, not erase \
             the announcement around it, got {stored:?}"
        );
    }
}

#[test]
fn hostile_body_at_the_length_ceiling_is_refused_or_stored_inert_within_bounds() {
    // Exactly at the stored ceiling: a neutralization that grows the
    // text must refuse or stay within the bound, never hand storage a
    // body the store or the wire would reject — and never truncate in a
    // way that leaves a live payload behind.
    for payload in ["@everyone", "discord.gg/free-nitro"] {
        let filler = "x".repeat(2000 - text_len(payload) - 1);
        let body = format!("{filler} {payload}");
        assert_eq!(text_len(&body), 2000, "fixture sits on the ceiling");
        if let Some(stored) = validated(&body) {
            assert_inert(&stored, payload);
        }
    }
}

#[test]
fn benign_bodies_validate_byte_identical() {
    for body in [
        "Game night Friday at 19:00 — bring snacks!",
        "Reminder: update to v1.2 and check notes.txt before the raid",
        "Line one\nLine two with **bold** and a `code` span",
        "Ping @username in chat if you are running late",
        "Discuss it on discord later, not in game",
        // Legitimate Unicode the neutralizer must not touch: Persian
        // ZWNJ, a ZWJ emoji sequence, and an already-defused mention.
        "می\u{200c}روم at 19:00, see you \u{1f469}\u{200d}\u{1f4bb}",
        "Reminder quoted from chat: @\u{200b}everyone (already defused)",
        "\u{1f389} Week 12 winners — café meetup, naïve plan, 3.5 pages",
    ] {
        let stored = validated(body)
            .unwrap_or_else(|| panic!("benign body must validate, got refusal: {body:?}"));
        assert_eq!(
            stored, body,
            "benign body must reach storage byte-identical: {body:?}"
        );
    }
}

#[test]
fn timing_and_length_validation_survive_the_safety_screen() {
    // The safety screen is additive: pre-existing refusals still refuse.
    assert!(validate_schedule(&ScheduleInput {
        body: "@everyone",
        in_minutes: None,
        every_minutes: None,
    })
    .is_err());
    assert!(validate_schedule(&ScheduleInput {
        body: &"x".repeat(2001),
        in_minutes: Some(10),
        every_minutes: None,
    })
    .is_err());
    // And a recurring schedule validates with its interval intact when
    // the body is benign.
    let recurring = validate_schedule(&ScheduleInput {
        body: "Weekly raid at 19:00",
        in_minutes: None,
        every_minutes: Some(60 * 24 * 7),
    })
    .expect("benign recurring schedule validates");
    assert_eq!(recurring.interval_seconds, Some(60 * 24 * 7 * 60));
    assert_eq!(recurring.body, "Weekly raid at 19:00");
}
