#![cfg(test)]
#![forbid(dead_code)]
//! Protected acceptance: moderation replies tell the truth without leaking
//! internals, and name who they acted on.
//!
//! Baseline bugs (the paired build owns the fix):
//! - `error_text` interpolates the raw Discord/executor rejection detail
//!   verbatim into the moderator reply (`"Discord refused the request:
//!   {detail}"`). The detail carries provider-shaped codes, build errors,
//!   hostile mentions and unbounded verbose runs. Moderators see internals;
//!   parallel moderations cannot be told apart from the copy.
//! - `outcome_text` replies bare `"Banned."`/`"Kicked."` with no target, so
//!   parallel moderations cannot be matched to replies.
//!
//! Protected file: the builder must NEVER edit this file. Return necessary
//! test corrections to the test author. This file must compile both on the
//! baseline and after the builder lands; the trait fallbacks below provide
//! the new associated-fn surface on the baseline, and an inherent item
//! shadows its fallback once added.
//!
//! Builder contract (all required for GREEN):
//! 1. Keep `fn error_text(err: MemberError) -> String` with its current
//!    signature, but render `Discord(Rejected(_))` as one FIXED actionable
//!    string: identical for every detail, naming Discord as the source,
//!    bounded (< 200 chars), carrying a next action (permissions/operator),
//!    and never containing any substring of the detail. Suggested copy:
//!    `"Discord refused the request; nothing was applied. Check the bot's
//!    permissions or ask an operator."` Any copy satisfying the properties
//!    passes. Preserve every other arm byte-for-byte (RateLimited,
//!    Timeout/Unavailable uncertain, policy/reason passthroughs, InFlight,
//!    KeyMismatch, store/bot/authority failure, TargetNotMember). Update the
//!    existing `rejected_detail_passthrough_is_pinned` test to the fixed copy.
//! 2. Add `impl MemberRuntime { pub(crate) fn error_text_with_log(err:
//!    MemberError, interaction_id: u64, request_id: &str) -> String }` that
//!    logs the FULL rejection detail at WARN (or ERROR) with structured
//!    `interaction_id` and `request_id` fields on the same event, then
//!    returns exactly `error_text(err)`. Call sites that have both ids must
//!    switch to it; the detail must never reach the reply.
//! 3. Add `impl MemberRuntime { pub(crate) fn outcome_text_with_target(result:
//!    &MemberResult, target_display: &str) -> String }` that names the
//!    sanitized target. Sanitizing grade (ephemeral moderator reply):
//!    single-line (fold `\r`/`\n` to spaces), mass-mention neutralized via
//!    the shared `message_safety::neutralize_mentions` (including zero-width
//!    obfuscations), mention pills (`<@`, `<#`, `<:`, `<a:`) split with
//!    U+200B so they render literally, and the rendered slot capped at 512
//!    chars (cut on char boundaries, applied after neutralizing).
//!    Markdown-escaping is NOT required here. The reply starts with the
//!    verb base (`Banned`, `Temporarily banned`, `Kicked`, `Timed out`,
//!    `Warned`, `Unbanned`), names the sanitized target, and when
//!    `result.replayed` ends with exactly ` Already applied.`.
//!    `handle_command` must switch to it (reading the display from the
//!    resolved interaction user with a user-id fallback). You MAY delete or
//!    change the existing private `outcome_text`; this file never calls it
//!    (its fallback is self-contained). Update the existing
//!    `*_success_confirmation_copy_is_pinned` tests to the new copy.
//!
//! Offline: pure reply rendering plus the process-wide tracing capture. No
//! database, no Discord calls, no `#[ignore]`.
//!
//! Expected results on the baseline (10 RED, 4 GREEN): every `rejected_*`,
//! `hostile_*`, `zero_width_*`, `mention_pills_*`, `newlines_*`,
//! `overlong_*`, `success_replay_*` and `replay_with_hostile_*` test RED
//! (raw detail leaks into replies, no log line, hostile names unsanitized,
//! replay marker misplaced); `rate_limited_copy_preserved_as_control`,
//! `uncertain_copy_preserved_as_control`,
//! `normal_names_and_punctuation_render_verbatim_as_control` and
//! `success_replies_name_the_sanitized_target_for_every_verb` GREEN (the
//! last pins the verb-base + plain-name shape, which already behaves).

use super::MemberRuntime;
use two_bot_core::member_moderation::{
    DiscordError as MemberDiscordError, MemberError, MemberOutcome, MemberResult,
};
use two_bot_core::message_safety::{contains_mass_mention, neutralize_mentions};

/// Hostile/verbose rejection detail: provider code, build error, hostile
/// mention, pill, newline and a long run. It must never reach a reply.
const HOSTILE_DETAIL_A: &str =
    "build: 403 missing permissions (code 50013) @everyone <@999> line1\nline2 verbose-marker-AAAA";
const HOSTILE_DETAIL_B: &str =
    "unknown channel (code 10003) @here <#456> totally different verbose-marker-BBBB\nsecond line";
const LONG_RUN: &str = "qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq";
const RAW_MARKERS: [&str; 7] = [
    "@everyone",
    "@here",
    "50013",
    "10003",
    "verbose-marker-AAAA",
    "verbose-marker-BBBB",
    "build:",
];

/// Baseline fallback for the builder's `MemberRuntime::error_text_with_log`
/// (contract item 2). Calls the legacy `error_text` directly: on the
/// baseline the raw detail leaks into the reply and nothing is logged, so
/// the log assertions RED. An inherent `error_text_with_log` shadows this
/// fallback once the builder adds it.
trait ErrorTextWithLogFallback {
    fn error_text_with_log(err: MemberError, interaction_id: u64, request_id: &str) -> String;
}

impl ErrorTextWithLogFallback for MemberRuntime {
    fn error_text_with_log(err: MemberError, _interaction_id: u64, _request_id: &str) -> String {
        super::error_text(err)
    }
}

/// Baseline fallback for the builder's `MemberRuntime::outcome_text_with_target`
/// (contract item 3). Self-contained on purpose: it hardcodes the baseline
/// verb bases and appends the target RAW, so every hostile name leaks and
/// every sanitizing assertion REDs. It never calls `super::outcome_text`,
/// so the builder may delete or reshape that helper freely. An inherent
/// `outcome_text_with_target` shadows this fallback once added.
trait OutcomeTextWithTargetFallback {
    fn outcome_text_with_target(result: &MemberResult, target_display: &str) -> String;
}

impl OutcomeTextWithTargetFallback for MemberRuntime {
    fn outcome_text_with_target(result: &MemberResult, target_display: &str) -> String {
        let base = match result.outcome {
            MemberOutcome::Banned => "Banned.",
            MemberOutcome::TemporarilyBanned => "Temporarily banned.",
            MemberOutcome::Kicked => "Kicked.",
            MemberOutcome::TimedOut => "Timed out.",
            MemberOutcome::Warned => "Warned.",
            MemberOutcome::Unbanned => "Unbanned.",
        };
        let base = if result.replayed {
            format!("{base} Already applied.")
        } else {
            base.to_owned()
        };
        format!("{base} {target_display}")
    }
}

/// One process-wide capture for this binary (see
/// `tracing_capture::global`): each test reads only the text appended after
/// its own start offset.
fn log_capture() -> &'static crate::tracing_capture::Capture {
    crate::tracing_capture::global()
}

fn rejected(detail: &str) -> MemberError {
    MemberError::Discord(MemberDiscordError::Rejected(detail.to_owned()))
}

const VERB_BASES: [(MemberOutcome, &str); 6] = [
    (MemberOutcome::Banned, "Banned"),
    (MemberOutcome::TemporarilyBanned, "Temporarily banned"),
    (MemberOutcome::Kicked, "Kicked"),
    (MemberOutcome::TimedOut, "Timed out"),
    (MemberOutcome::Warned, "Warned"),
    (MemberOutcome::Unbanned, "Unbanned"),
];

// Explicit trait calls keep the baseline adapters exercised after inherent
// items shadow them; behavioral acceptance tests below keep associated syntax.
#[test]
fn legacy_error_fallback_preserves_non_rejection_copy_control() {
    for (error, expected) in [
        (
            MemberError::Discord(MemberDiscordError::RateLimited),
            "Discord rate-limited the request; nothing was applied. Try again shortly.",
        ),
        (
            MemberError::Discord(MemberDiscordError::Timeout),
            "Discord did not confirm the action. It may still land; ask an operator to reconcile before retrying.",
        ),
    ] {
        assert_eq!(
            <MemberRuntime as ErrorTextWithLogFallback>::error_text_with_log(
                error,
                123456789,
                "req-fallback-control",
            ),
            expected
        );
    }
}

#[test]
fn legacy_outcome_fallback_preserves_raw_target_and_replay_order_control() {
    let result = MemberResult {
        outcome: MemberOutcome::Kicked,
        replayed: false,
    };
    assert_eq!(
        <MemberRuntime as OutcomeTextWithTargetFallback>::outcome_text_with_target(
            &result,
            "@everyone\nx",
        ),
        "Kicked. @everyone\nx"
    );
    let replay = MemberResult {
        replayed: true,
        ..result
    };
    assert_eq!(
        <MemberRuntime as OutcomeTextWithTargetFallback>::outcome_text_with_target(
            &replay,
            "@everyone\nx",
        ),
        "Kicked. Already applied. @everyone\nx"
    );
}

#[test]
fn rejected_details_render_one_fixed_copy_without_raw_leak() {
    let a = super::error_text(rejected(HOSTILE_DETAIL_A));
    let b = super::error_text(rejected(HOSTILE_DETAIL_B));
    assert_eq!(
        a, b,
        "rejection copy must be fixed vocabulary, not per-detail text"
    );
    assert!(a.contains("Discord"), "fixed copy names the source: {a:?}");
    assert!(
        a.len() < 200,
        "fixed copy is bounded, not verbose ({} chars): {a:?}",
        a.len()
    );
    for marker in RAW_MARKERS {
        assert!(
            !a.contains(marker),
            "fixed copy must not leak raw detail {marker:?}: {a:?}"
        );
    }
    assert!(!a.contains('\n'), "fixed copy is single-line: {a:?}");
    assert!(
        !contains_mass_mention(&a),
        "fixed copy carries no live mention: {a:?}"
    );
}

#[test]
fn rejected_verbose_run_does_not_reach_the_reply() {
    let detail = format!("403 {LONG_RUN}");
    let reply = super::error_text(rejected(&detail));
    assert!(
        !reply.contains(LONG_RUN),
        "verbose detail must not leak: {reply:?}"
    );
    assert!(reply.len() < 200, "reply stays bounded: {reply:?}");
}

#[test]
fn rejected_full_detail_reaches_the_log_with_ids() {
    let log_start = log_capture().text().len();
    let reply =
        MemberRuntime::error_text_with_log(rejected(HOSTILE_DETAIL_A), 123456789, "req-log-1");
    // The reply stays fixed even on the logging path.
    assert_eq!(reply, super::error_text(rejected(HOSTILE_DETAIL_B)));
    let fresh = log_capture().text();
    let fresh = &fresh[log_start..];
    let line = fresh
        .lines()
        .find(|line| line.contains("verbose-marker-AAAA"))
        .unwrap_or_else(|| panic!("rejection must log the full detail; fresh log: {fresh:?}"));
    assert!(
        line.contains("123456789"),
        "log carries the interaction id: {line:?}"
    );
    assert!(
        line.contains("req-log-1"),
        "log carries the request id: {line:?}"
    );
}

#[test]
fn rate_limited_copy_preserved_as_control() {
    assert_eq!(
        super::error_text(MemberError::Discord(MemberDiscordError::RateLimited)),
        "Discord rate-limited the request; nothing was applied. Try again shortly."
    );
}

#[test]
fn uncertain_copy_preserved_as_control() {
    let pinned = "Discord did not confirm the action. It may still land; ask an operator to reconcile before retrying.";
    assert_eq!(
        super::error_text(MemberError::Discord(MemberDiscordError::Timeout)),
        pinned
    );
    assert_eq!(
        super::error_text(MemberError::Discord(MemberDiscordError::Unavailable(
            "503 service unavailable".to_owned()
        ))),
        pinned
    );
}

#[test]
fn success_replies_name_the_sanitized_target_for_every_verb() {
    for (outcome, base) in VERB_BASES {
        let result = MemberResult {
            outcome,
            replayed: false,
        };
        let reply = MemberRuntime::outcome_text_with_target(&result, "dave");
        assert!(
            reply.starts_with(base),
            "{outcome:?} reply keeps its verb base: {reply:?}"
        );
        assert!(
            reply.contains("dave"),
            "{outcome:?} reply names the target: {reply:?}"
        );
        assert!(
            !reply.contains("Already applied"),
            "fresh reply carries no replay marker: {reply:?}"
        );
    }
}

#[test]
fn hostile_mass_mentions_are_neutralized_exactly() {
    for (name, neutralized) in [
        ("@everyone", "@\u{200b}everyone"),
        ("@here", "@\u{200b}here"),
    ] {
        let reply = MemberRuntime::outcome_text_with_target(
            &MemberResult {
                outcome: MemberOutcome::Banned,
                replayed: false,
            },
            name,
        );
        assert!(
            !reply.contains(name),
            "raw mass mention must not survive: {reply:?}"
        );
        assert!(
            reply.contains(neutralized),
            "shared-neutralizer output must appear (input {name:?}): {reply:?}"
        );
        assert!(
            !contains_mass_mention(&reply),
            "reply carries no live mention: {reply:?}"
        );
        assert_eq!(
            neutralize_mentions(name),
            neutralized,
            "test pins the shared contract"
        );
    }
}

#[test]
fn zero_width_obfuscated_mentions_are_neutralized() {
    for separator in ['\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{feff}'] {
        let disguised = format!("@{separator}everyone");
        let reply = MemberRuntime::outcome_text_with_target(
            &MemberResult {
                outcome: MemberOutcome::Kicked,
                replayed: false,
            },
            &disguised,
        );
        assert!(
            reply.contains("@\u{200b}everyone"),
            "obfuscated mention must canonicalize inert ({disguised:?}): {reply:?}"
        );
        assert!(
            !contains_mass_mention(&reply),
            "reply carries no live mention: {reply:?}"
        );
    }
}

#[test]
fn mention_pills_are_split_so_they_render_literally() {
    for (pill, split) in [
        ("<@44>", "<\u{200b}@44>"),
        ("<#13>", "<\u{200b}#13>"),
        ("<:wave:99>", "<\u{200b}:wave:99>"),
        ("<a:wave:99>", "<\u{200b}a:wave:99>"),
    ] {
        let reply = MemberRuntime::outcome_text_with_target(
            &MemberResult {
                outcome: MemberOutcome::Warned,
                replayed: false,
            },
            pill,
        );
        assert!(
            !reply.contains(pill),
            "raw pill must not survive: {reply:?}"
        );
        assert!(
            reply.contains(split),
            "pill must split literally ({pill:?}): {reply:?}"
        );
    }
}

#[test]
fn newlines_in_names_fold_to_spaces() {
    let reply = MemberRuntime::outcome_text_with_target(
        &MemberResult {
            outcome: MemberOutcome::TimedOut,
            replayed: false,
        },
        "line1\nline2\r\nline3",
    );
    assert!(
        !reply.contains('\n'),
        "reply must be single-line: {reply:?}"
    );
    assert!(
        !reply.contains('\r'),
        "reply must be single-line: {reply:?}"
    );
    assert!(
        reply.contains("line1 line2 line3"),
        "fold keeps the text readable: {reply:?}"
    );
}

#[test]
fn overlong_names_are_capped() {
    let long = "a".repeat(600);
    let reply = MemberRuntime::outcome_text_with_target(
        &MemberResult {
            outcome: MemberOutcome::Banned,
            replayed: false,
        },
        &long,
    );
    assert!(
        !reply.contains(&long),
        "the full 600-char run must not survive: {} chars",
        reply.len()
    );
    assert!(
        reply.contains('a'),
        "capped output keeps readable text: {reply:?}"
    );
    // The 512-char grade cap plus a short verb frame always fits Discord.
    assert!(
        reply.len() < 700,
        "reply stays bounded: {} chars",
        reply.len()
    );
}

#[test]
fn success_replay_keeps_already_applied_after_the_target() {
    for (outcome, base) in VERB_BASES {
        let result = MemberResult {
            outcome,
            replayed: true,
        };
        let reply = MemberRuntime::outcome_text_with_target(&result, "dave");
        assert!(
            reply.starts_with(base),
            "{outcome:?} replay keeps its verb base: {reply:?}"
        );
        assert!(
            reply.contains("dave"),
            "{outcome:?} replay still names the target: {reply:?}"
        );
        assert!(
            reply.ends_with("Already applied."),
            "{outcome:?} replay keeps the marker last: {reply:?}"
        );
    }
}

#[test]
fn replay_with_hostile_name_sanitizes_and_keeps_the_marker() {
    let reply = MemberRuntime::outcome_text_with_target(
        &MemberResult {
            outcome: MemberOutcome::TemporarilyBanned,
            replayed: true,
        },
        "@everyone\nx",
    );
    assert!(
        !reply.contains("@everyone"),
        "raw mention must not survive replay: {reply:?}"
    );
    assert!(!reply.contains('\n'), "replay stays single-line: {reply:?}");
    assert!(
        reply.ends_with("Already applied."),
        "replay keeps the marker last: {reply:?}"
    );
}

#[test]
fn normal_names_and_punctuation_render_verbatim_as_control() {
    for name in [
        "dave",
        "space cadet",
        "mary-jane",
        "o'brien",
        "user42",
        "界界",
    ] {
        let reply = MemberRuntime::outcome_text_with_target(
            &MemberResult {
                outcome: MemberOutcome::Banned,
                replayed: false,
            },
            name,
        );
        assert!(
            reply.contains(name),
            "normal name must survive verbatim ({name:?}): {reply:?}"
        );
    }
}
