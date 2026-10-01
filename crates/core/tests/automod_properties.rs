//! Bounded automod normalization and word-boundary properties.
//!
//! Pins legacy `hasBadWord` semantics ported in `automod.rs`: NFKC +
//! lowercase normalization is idempotent, zero-width/whitespace gaps between
//! bad-word characters still match (evasion resistance), affixed/embedded
//! words refuse (boundary precision), and empty-normalization never matches.
//! Synthetic fixtures only: no Discord, network, database, or subprocess.

use proptest::prelude::*;
use two_bot_core::automod::{
    match_automod, normalize_content, AutomodFilter, AutomodMessage, AutomodPolicy,
    RepeatTracker,
};

fn message(content: String) -> AutomodMessage {
    AutomodMessage {
        guild_id: "111111111111111111".to_owned(),
        channel_id: "222222222222222222".to_owned(),
        message_id: "333333333333333333".to_owned(),
        author_id: "444444444444444444".to_owned(),
        author_is_bot: false,
        role_ids: vec![],
        content,
        mentioned_user_ids: vec![],
        attachment_names: vec![],
        observed_timestamp_ms: 1_000_000,
    }
}

fn policy_with(words: &[&str]) -> AutomodPolicy {
    AutomodPolicy {
        bad_words: words.iter().map(ToString::to_string).collect(),
        ..AutomodPolicy::default()
    }
}

fn check(content: String, policy: &AutomodPolicy) -> Option<AutomodFilter> {
    match_automod(&message(content), policy, &mut RepeatTracker::default())
}

/// Fullwidth ASCII letters (compatibility forms NFKC folds to ASCII).
fn fullwidth(word: &str) -> String {
    word.chars()
        .map(|c| {
            if c.is_ascii_lowercase() {
                char::from_u32(c as u32 - 0x61 + 0xFF41).unwrap_or(c)
            } else if c.is_ascii_uppercase() {
                char::from_u32(c as u32 - 0x41 + 0xFF21).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

fn gap_strategy() -> impl Strategy<Value = char> {
    prop_oneof![
        Just(' '),
        Just('\t'),
        Just('\n'),
        Just('\u{200B}'),
        Just('\u{200C}'),
        Just('\u{200D}'),
        Just('\u{2060}'),
        Just('\u{FEFF}'),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn property_normalize_idempotent(
        s in proptest::collection::vec(any::<char>(), 0..64)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
    ) {
        let once = normalize_content(&s);
        // NFKC + lowercase + whitespace collapse is a fixed point.
        prop_assert_eq!(normalize_content(&once), once);
        // Normalized output is trimmed and single-spaced.
        prop_assert!(once == once.trim());
        prop_assert!(!once.contains("  "));
    }

    #[test]
    fn property_fullwidth_nfkc_matches_ascii(
        word in proptest::collection::vec(proptest::char::range('a', 'z'), 1..12)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
    ) {
        prop_assert_eq!(normalize_content(&fullwidth(&word)), word);
        prop_assert_eq!(normalize_content(&fullwidth(&word.to_uppercase())), word);
    }

    #[test]
    fn property_gap_evasion_still_matches(
        gaps in proptest::collection::vec(gap_strategy(), 0..16),
    ) {
        let policy = policy_with(&["spamword"]);
        let letters: Vec<char> = "spamword".chars().collect();
        let mut content = String::from("hey ");
        content.push(letters[0]);
        for (i, letter) in letters.iter().skip(1).enumerate() {
            // Distribute up to 16 generated gaps between the 7 slots.
            if i < gaps.len() {
                content.push(gaps[i]);
                if gaps.len() > 8 && i + 8 < gaps.len() {
                    content.push(gaps[i + 8]);
                }
            }
            content.push(*letter);
        }
        content.push_str(" bye");
        prop_assert_eq!(check(content, &policy), Some(AutomodFilter::BadWords));
    }

    #[test]
    fn property_word_boundaries_refuse_affixed(
        prefix in proptest::collection::vec(
            prop_oneof![
                proptest::char::range('a', 'z'),
                proptest::char::range('0', '9'),
                Just('_'),
            ],
            1..8,
        ).prop_map(|chars| chars.into_iter().collect::<String>()),
        suffix in proptest::collection::vec(
            prop_oneof![
                proptest::char::range('a', 'z'),
                proptest::char::range('0', '9'),
                Just('_'),
            ],
            1..8,
        ).prop_map(|chars| chars.into_iter().collect::<String>()),
    ) {
        let policy = policy_with(&["spamword"]);
        // `badwordx`, `xbadword`, and embedded forms must not match.
        for content in [
            format!("hi spamword{suffix} bye"),
            format!("hi {prefix}spamword bye"),
            format!("hi {prefix}spamword{suffix} bye"),
        ] {
            prop_assert_eq!(check(content, &policy), None);
        }
        // The bare word in the same framing still matches (guard).
        prop_assert_eq!(
            check("hi spamword bye".to_owned(), &policy),
            Some(AutomodFilter::BadWords)
        );
    }

    #[test]
    fn property_empty_words_never_match(
        word in proptest::collection::vec(gap_strategy(), 1..8)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
        tail in proptest::collection::vec(proptest::char::range('a', 'z'), 0..24)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
    ) {
        // Words that normalize to empty (whitespace/zero-width only) never match.
        let policy = policy_with(&[word.as_str()]);
        let content = format!("hello world {tail}");
        prop_assert_ne!(check(content, &policy), Some(AutomodFilter::BadWords));
        // Empty content never matches either.
        let clean = policy_with(&["spamword"]);
        prop_assert_eq!(check("   ".to_owned(), &clean), None);
        prop_assert_eq!(check(String::new(), &clean), None);
    }
}

#[test]
fn regression_gap_and_boundary_vectors() {
    let policy = policy_with(&["spamword"]);
    // Zero-width joiners and spaces between letters still match.
    assert_eq!(
        check("s\u{200B}p\u{200C}am\u{200D}word".to_owned(), &policy),
        Some(AutomodFilter::BadWords)
    );
    assert_eq!(
        check("s p a m w o r d".to_owned(), &policy),
        Some(AutomodFilter::BadWords)
    );
    // Full-width input normalizes before matching.
    assert_eq!(
        check(fullwidth("SPAMWORD"), &policy),
        Some(AutomodFilter::BadWords)
    );
    // Affixed/embedded forms refuse.
    for content in [
        "this is spamwordish",
        "this is xspamword",
        "this is spamwordx",
        "xxspamwordyy",
        "foo_spamword_bar",
    ] {
        assert_eq!(check(content.to_owned(), &policy), None, "{content}");
    }
    // Empty-normalization never matches.
    for word in ["", "   ", "\u{200B}\u{200C}", " \u{200B} \u{FEFF} "] {
        let policy = policy_with(&[word]);
        assert_eq!(
            check("hello world".to_owned(), &policy),
            None,
            "word {word:?}"
        );
    }
}
