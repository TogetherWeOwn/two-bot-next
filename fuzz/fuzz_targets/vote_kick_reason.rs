#![no_main]

use libfuzzer_sys::fuzz_target;
use two_bot::voice_rooms::{sanitize_vote_reason, VOTE_KICK_PUBLIC_REASON_LIMIT};
use two_bot_core::message_safety::neutralize_mentions;

fuzz_target!(|data: &[u8]| {
    // The ballot reason is initiator-supplied UTF-8 text. Invalid UTF-8 is
    // discarded, matching the other `&str` harnesses.
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };

    // Empty-input base cases, independent of the fuzzer input.
    assert_eq!(sanitize_vote_reason(""), "");
    assert_eq!(neutralize_mentions(""), "");

    // Gate VK-04 invariants on every sanitizer output: the ballot goes out
    // under the bot's name, so a hostile reason must never become a ping,
    // clickable link, embed or formatted endorsement.
    let safe = sanitize_vote_reason(raw);
    // At most the documented bound, counted in chars: the sanitizer cuts
    // with `.chars().take(VOTE_KICK_PUBLIC_REASON_LIMIT)`.
    assert!(safe.chars().count() <= VOTE_KICK_PUBLIC_REASON_LIMIT);
    // Single line: carriage returns and newlines are folded to spaces, so no
    // reason text can start line-leading Discord markup.
    assert!(!safe.contains('\r'));
    assert!(!safe.contains('\n'));
    // No mass pings, including zero-width-split obfuscation (broken by the
    // shared mention neutralizer).
    assert!(!safe.contains("@everyone"));
    assert!(!safe.contains("@here"));
    // No user/role, channel or custom-emoji pills (split with a zero-width
    // space so they render literally); the animated `<a:name:id>` form is
    // covered by the `<a:` pin.
    assert!(!safe.contains("<@"));
    assert!(!safe.contains("<#"));
    assert!(!safe.contains("<:"));
    assert!(!safe.contains("<a:"));
    // No URL scheme survives clickable.
    assert!(!safe.contains("://"));

    // The shared neutralizer half of the same boundary: arbitrary input never
    // leaves a raw mass mention behind, and neutralizing twice is stable.
    // No database, Discord client, network or secret is touched here; only
    // these two pure string functions run.
    let neutralized = neutralize_mentions(raw);
    assert!(!neutralized.contains("@everyone"));
    assert!(!neutralized.contains("@here"));
    assert_eq!(neutralize_mentions(&neutralized), neutralized);
});
