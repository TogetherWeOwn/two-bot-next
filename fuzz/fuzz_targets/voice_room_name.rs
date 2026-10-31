#![no_main]

//! Arbitrary member/game text through the V3 `/name` decision parsers.
//!
//! [`decide_custom_name`], [`decide_template_name`] and [`is_literal_name`]
//! are pure functions over untrusted member/game text
//! (`crates/core/src/voice_room_name.rs`). This target drives arbitrary
//! names/templates through them with member-controlled render facts
//! (owner/game names) and asserts the name-policy invariants on every decided
//! name: at most [`MAX_CHANNEL_NAME_CHARS`] characters, never empty, and no
//! C0 controls, DEL, `@` or backticks surviving the sanitizer. No database,
//! Discord client, network or secret is touched here; only these pure
//! functions run.

use libfuzzer_sys::fuzz_target;
use two_bot_core::automod::AutomodPolicy;
use two_bot_core::voice_conditions::ConditionFacts;
use two_bot_core::voice_name_filter::{NameFilterContext, MAX_CHANNEL_NAME_CHARS};
use two_bot_core::voice_naming::RoomContext;
use two_bot_core::voice_room_name::{
    decide_custom_name, decide_template_name, is_literal_name, NameChecks, NameRefusal,
    RenderFacts, MAX_CUSTOM_NAME_CHARS,
};

/// Render-fact strings stay short so token expansion stays bounded: the
/// template engine caps templates at 4096 bytes but expands member-controlled
/// tokens per occurrence, so an unbounded owner/game name multiplied by
/// hundreds of token occurrences could blow the intermediate render before
/// the 100-character truncation.
const CONTEXT_CHARS: usize = 32;
/// The fuzz-derived bad word stays short for the same reason: the automod
/// matcher scans the name per word.
const BAD_WORD_CHARS: usize = 16;
/// Refusal replies are short ephemeral sentences; the unit tests pin < 300.
const MAX_REFUSAL_CHARS: usize = 300;

fn assert_decided_name(name: &str) {
    assert!(!name.is_empty(), "decided names are never empty");
    assert!(
        name.chars().count() <= MAX_CHANNEL_NAME_CHARS,
        "decided names fit the channel bound"
    );
    assert!(
        !name
            .chars()
            .any(|c| matches!(c, '\u{0}'..='\u{1F}' | '\u{7F}')),
        "no C0 control or DEL leaks into decided names"
    );
    assert!(!name.contains('@'), "the sanitizer strips @");
    assert!(!name.contains('`'), "the sanitizer strips backticks");
}

fn assert_refusal_text(refusal: &NameRefusal) {
    let text = refusal.to_string();
    assert!(!text.is_empty(), "refusals explain what to do");
    assert!(
        text.chars().count() < MAX_REFUSAL_CHARS,
        "refusal replies stay short"
    );
}

fn base_pins() {
    // Empty/overlong base cases, independent of the fuzzer input.
    let context = RoomContext {
        room_number: 3,
        owner_name: "Ava".to_owned(),
        original_creator_name: "Ava".to_owned(),
        member_count: 2,
        owner_present: true,
        user_limit: 8,
        game_name: "General".to_owned(),
        seed: 7,
        ..RoomContext::default()
    };
    let conditions = ConditionFacts::default();
    let policy = AutomodPolicy::default();
    let filter = NameFilterContext {
        guild_id: "100".to_owned(),
        channel_id: "500".to_owned(),
        user_id: "300".to_owned(),
    };
    let others: Vec<String> = Vec::new();
    let render = RenderFacts {
        context: &context,
        conditions: &conditions,
        fallback_name: "Ava's room",
    };
    let checks = NameChecks {
        policy: &policy,
        filter: &filter,
        unique_names: false,
        other_voice_names: &others,
    };
    assert_eq!(
        decide_custom_name("", &render, &checks),
        Err(NameRefusal::Empty)
    );
    assert!(decide_custom_name(&"a".repeat(MAX_CUSTOM_NAME_CHARS), &render, &checks).is_ok());
    assert_eq!(
        decide_custom_name(&"a".repeat(MAX_CUSTOM_NAME_CHARS + 1), &render, &checks),
        Err(NameRefusal::TooLong)
    );
    assert!(is_literal_name("Plain name"));
    assert!(!is_literal_name("@@owner@@"));
    assert_eq!(
        decide_template_name("", &render, &checks),
        Ok("Ava's room".to_owned())
    );
}

fn drive(input: &str, data: &[u8]) {
    let flag = data.first().copied().unwrap_or(0);
    // Member-controlled render facts: bounded prefixes of the fuzzer input so
    // tokens such as `@@owner@@` and `@@game_name@@` pull hostile text into
    // the render, exercising the filter-after-render check.
    let owner: String = input.chars().take(CONTEXT_CHARS).collect();
    let game: String = input.chars().rev().take(CONTEXT_CHARS).collect();
    let context = RoomContext {
        room_number: data.len() as u32,
        owner_name: owner.clone(),
        original_creator_name: owner.clone(),
        member_count: (data.len() % 9) as u32,
        owner_present: flag & 0x40 != 0,
        user_limit: (data.len() % 11) as u32,
        game_name: game.clone(),
        timestamp: 1_700_000_000,
        seed: data.len() as u64,
        ..RoomContext::default()
    };
    let conditions = ConditionFacts {
        owner_playing: flag & 0x01 != 0,
        owner_live_discord: flag & 0x02 != 0,
        owner_live_external: flag & 0x04 != 0,
        private: flag & 0x08 != 0,
        games: if flag & 0x10 != 0 && !game.is_empty() {
            vec![game.clone()]
        } else {
            Vec::new()
        },
        live_discord_count: u32::from(flag & 0x20 != 0),
        ..ConditionFacts::default()
    };
    let mut policy = AutomodPolicy {
        bad_words: vec!["blorp".to_owned()],
        ..AutomodPolicy::default()
    };
    let fuzz_word: String = input.chars().take(BAD_WORD_CHARS).collect();
    if !fuzz_word.is_empty() {
        policy.bad_words.push(fuzz_word);
    }
    let filter = NameFilterContext {
        guild_id: "100".to_owned(),
        channel_id: "500".to_owned(),
        user_id: "300".to_owned(),
    };
    // The fuzz-derived others make the "unique names" Taken refusal reachable
    // whenever the render folds onto the owner/game text.
    let others = vec![owner, game, "Lounge".to_owned()];
    let render = RenderFacts {
        context: &context,
        conditions: &conditions,
        fallback_name: "Ava's room",
    };
    let checks = NameChecks {
        policy: &policy,
        filter: &filter,
        unique_names: data.len() % 2 == 0,
        other_voice_names: &others,
    };

    // The literal check must never panic; the Taken refusal below re-checks it
    // on the trimmed text the decision actually uses.
    let _ = is_literal_name(input);
    match decide_custom_name(input, &render, &checks) {
        Ok(custom) => {
            assert_eq!(
                custom.stored,
                input.trim(),
                "the stored override is the trimmed raw text"
            );
            assert!(
                custom.stored.chars().count() <= MAX_CUSTOM_NAME_CHARS,
                "the stored override fits the modal bound"
            );
            assert_decided_name(&custom.channel_name);
        }
        Err(refusal) => {
            let trimmed = input.trim();
            if trimmed.is_empty() {
                assert_eq!(refusal, NameRefusal::Empty, "blank input refuses as empty");
            }
            if trimmed.chars().count() > MAX_CUSTOM_NAME_CHARS {
                assert_eq!(
                    refusal,
                    NameRefusal::TooLong,
                    "overlong input refuses before filtering"
                );
            }
            if refusal == NameRefusal::Taken {
                assert!(
                    is_literal_name(trimmed),
                    "only literal names compare for uniqueness"
                );
            }
            assert_refusal_text(&refusal);
        }
    }
    match decide_template_name(input, &render, &checks) {
        Ok(name) => assert_decided_name(&name),
        Err(refusal) => {
            // Restores never compare for uniqueness; every filter refusal maps
            // to `TemplateBlocked`.
            assert_eq!(
                refusal,
                NameRefusal::TemplateBlocked,
                "template refusals never leak the filter kind"
            );
            assert_refusal_text(&refusal);
        }
    }
}

fuzz_target!(|data: &[u8]| {
    // Names arrive as UTF-8 text. Invalid UTF-8 is discarded, matching the
    // other `&str` harnesses.
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    base_pins();
    drive(input, data);
});
