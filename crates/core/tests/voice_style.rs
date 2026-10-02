//! Hermetic V6a acceptance cases against the public styling core.
//!
//! The golden table pins one independently computed expectation per mode; the
//! properties pin determinism and shape invariants across arbitrary input.

use proptest::prelude::*;
use two_bot_core::voice_style::{
    apply_chain, apply_mode, parse_modes, StyleMode, REMSHORT_STOP_WORDS,
};

/// (mode chain, seed, input, expected). The seed only affects `rand`.
const GOLDENS: &[(&str, u64, &str, &str)] = &[
    // Case modes.
    ("upper", 0, "hello world", "HELLO WORLD"),
    ("caps", 0, "hello", "HELLO"),
    ("UPPER", 0, "hi", "HI"),
    ("lower", 0, "HeLLo WoRLD", "hello world"),
    ("title", 0, "hello WORLD", "Hello World"),
    ("swap", 0, "HeLLo", "hEllO"),
    ("scaps", 0, "hello xyz ABC", "ʜᴇʟʟᴏ xʏᴢ ABC"),
    ("rand", 7, "hello", "HelLo"),
    ("rand", 0, "hello", "HeLlO"),
    // Words and spacing.
    ("spaces", 0, "abc", "a b c"),
    ("spaces", 0, "", ""),
    ("acro", 0, "hello world", "hw"),
    ("acro", 0, "  hello   world ", "hw"),
    (
        "remshort",
        0,
        "the quick brown fox and the lazy dog",
        "quick brown fox lazy dog",
    ),
    ("remshort", 0, "A Tale Of Two Cities", "Tale Two Cities"),
    ("2w", 0, "one two three four", "one two"),
    ("0w", 0, "one two", ""),
    ("3w", 0, "one two", "one two"),
    // Novelty modes.
    ("uwu", 0, "hello lovely rover", "hewwo wuvwy wuvw"),
    ("uwu", 0, "Love Cover OVER", "Wuv Cuvw UVW"),
    ("usd", 0, "hello", "ollǝɥ"),
    ("usd", 0, "Hi!", "¡ᴉH"),
    ("usd", 0, "2026", "9ᄅ0ᄅ"),
    ("usd", 0, "4 5 7", "7 5 4"),
    // Unicode fonts on the shared demo line.
    ("bold", 0, "Hello World 123", "𝐇𝐞𝐥𝐥𝐨 𝐖𝐨𝐫𝐥𝐝 𝟏𝟐𝟑"),
    ("italic", 0, "Hello World 123", "𝐻𝑒𝑙𝑙𝑜 𝑊𝑜𝑟𝑙𝑑 123"),
    ("bolditalic", 0, "Hello World 123", "𝑯𝒆𝒍𝒍𝒐 𝑾𝒐𝒓𝒍𝒅 123"),
    ("script", 0, "Hello World 123", "ℋℯ𝓁𝓁ℴ 𝒲ℴ𝓇𝓁𝒹 123"),
    ("boldscript", 0, "Hello World 123", "𝓗𝓮𝓵𝓵𝓸 𝓦𝓸𝓻𝓵𝓭 123"),
    ("fraktur", 0, "Hello World 123", "ℌ𝔢𝔩𝔩𝔬 𝔚𝔬𝔯𝔩𝔡 123"),
    ("boldfraktur", 0, "Hello World 123", "𝕳𝖊𝖑𝖑𝖔 𝖂𝖔𝖗𝖑𝖉 123"),
    ("double", 0, "Hello World 123", "ℍ𝕖𝕝𝕝𝕠 𝕎𝕠𝕣𝕝𝕕 𝟙𝟚𝟛"),
    ("sans", 0, "Hello World 123", "𝖧𝖾𝗅𝗅𝗈 𝖶𝗈𝗋𝗅𝖽 𝟣𝟤𝟥"),
    ("boldsans", 0, "Hello World 123", "𝗛𝗲𝗹𝗹𝗼 𝗪𝗼𝗿𝗹𝗱 𝟭𝟮𝟯"),
    ("italicsans", 0, "Hello World 123", "𝘏𝘦𝘭𝘭𝘰 𝘞𝘰𝘳𝘭𝘥 123"),
    ("bolditalicsans", 0, "Hello World 123", "𝙃𝙚𝙡𝙡𝙤 𝙒𝙤𝙧𝙡𝙙 123"),
    ("mono", 0, "Hello World 123", "𝙷𝚎𝚕𝚕𝚘 𝚆𝚘𝚛𝚕𝚍 𝟷𝟸𝟹"),
    // Reserved-gap capitals in the Letterlike Symbols block.
    ("script", 0, "BEF HIL MR ego", "ℬℰℱ ℋℐℒ ℳℛ ℯℊℴ"),
    ("fraktur", 0, "CHIRZ", "ℭℌℑℜℨ"),
    ("double", 0, "CHNPQRZ abc 789", "ℂℍℕℙℚℝℤ 𝕒𝕓𝕔 𝟟𝟠𝟡"),
    ("italic", 0, "hi", "ℎ𝑖"),
    // Characters outside the font alphabets pass through.
    ("italic", 0, "a1", "𝑎1"),
    ("bold", 0, "a-b_c!", "𝐚-𝐛_𝐜!"),
    // Chains and unknown modes.
    ("upper+bold", 0, "hi", "𝐇𝐈"),
    ("upper + bold", 0, "hi", "𝐇𝐈"),
    ("upper+bogus", 0, "hi", "HI"),
    ("bogus", 0, "hello", "hello"),
];

#[test]
fn golden_table_pins_every_mode() {
    for (chain, seed, input, expected) in GOLDENS {
        let modes = parse_modes(chain);
        assert_eq!(
            apply_chain(&modes, input, *seed).as_str(),
            *expected,
            "chain={chain:?} input={input:?}"
        );
    }
}

#[test]
fn parse_modes_chains_aliases_trims_and_numbers_words() {
    assert_eq!(
        parse_modes("upper+bold"),
        vec![StyleMode::Upper, StyleMode::Bold]
    );
    assert_eq!(parse_modes("caps"), vec![StyleMode::Upper]);
    assert_eq!(
        parse_modes(" upper + bold "),
        vec![StyleMode::Upper, StyleMode::Bold]
    );
    assert_eq!(parse_modes("2w"), vec![StyleMode::FirstWords(2)]);
    assert_eq!(parse_modes("10w"), vec![StyleMode::FirstWords(10)]);
    assert_eq!(parse_modes("w"), vec![StyleMode::Unknown("w".to_string())]);
    assert_eq!(
        parse_modes("bogus"),
        vec![StyleMode::Unknown("bogus".to_string())]
    );
    assert_eq!(
        parse_modes("upper+bogus"),
        vec![StyleMode::Upper, StyleMode::Unknown("bogus".to_string())]
    );
    assert_eq!(parse_modes(""), vec![StyleMode::Unknown(String::new())]);
}

#[test]
fn remshort_uses_exactly_the_spec_stop_word_list() {
    assert_eq!(
        REMSHORT_STOP_WORDS,
        &["a", "an", "and", "at", "by", "from", "in", "is", "of", "on", "or", "the", "to",]
    );
}

#[test]
fn chains_apply_left_to_right() {
    // `title` then `swap` inverts the title case; the reverse order retitles.
    assert_eq!(apply_chain(&parse_modes("title+swap"), "hello", 0), "hELLO");
    assert_eq!(apply_chain(&parse_modes("swap+title"), "hello", 0), "Hello");
}

fn font_modes() -> Vec<StyleMode> {
    vec![
        StyleMode::Bold,
        StyleMode::Italic,
        StyleMode::BoldItalic,
        StyleMode::Script,
        StyleMode::BoldScript,
        StyleMode::Fraktur,
        StyleMode::BoldFraktur,
        StyleMode::Double,
        StyleMode::Sans,
        StyleMode::BoldSans,
        StyleMode::ItalicSans,
        StyleMode::BoldItalicSans,
        StyleMode::Mono,
    ]
}

fn non_rand_modes() -> Vec<StyleMode> {
    let mut modes = vec![
        StyleMode::Upper,
        StyleMode::Lower,
        StyleMode::Title,
        StyleMode::Swap,
        StyleMode::SmallCaps,
        StyleMode::Spaces,
        StyleMode::Acro,
        StyleMode::RemShort,
        StyleMode::FirstWords(2),
        StyleMode::Uwu,
        StyleMode::UpsideDown,
        StyleMode::Unknown("bogus".to_string()),
    ];
    modes.extend(font_modes());
    modes
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn property_rand_is_deterministic_per_seed(
        text in any::<String>(),
        seed in any::<u64>(),
    ) {
        prop_assert_eq!(
            apply_mode(&StyleMode::Rand, &text, seed),
            apply_mode(&StyleMode::Rand, &text, seed)
        );
    }

    #[test]
    fn property_rand_preserves_character_count(
        // ASCII only: Unicode case folding (e.g. `ß` -> `SS`) can expand
        // characters, while ASCII case mapping stays one-to-one.
        text in any::<String>()
            .prop_map(|s| s.chars().filter(|c| c.is_ascii()).collect::<String>()),
        seed in any::<u64>(),
    ) {
        prop_assert_eq!(
            apply_mode(&StyleMode::Rand, &text, seed).chars().count(),
            text.chars().count()
        );
    }

    #[test]
    fn property_font_modes_preserve_character_count(
        text in any::<String>(),
        index in 0usize..13,
    ) {
        let modes = font_modes();
        prop_assert_eq!(
            apply_mode(&modes[index], &text, 0).chars().count(),
            text.chars().count()
        );
    }

    #[test]
    fn property_single_char_font_maps_preserve_character_count(
        text in any::<String>(),
    ) {
        for mode in [StyleMode::SmallCaps, StyleMode::UpsideDown] {
            prop_assert_eq!(
                apply_mode(&mode, &text, 0).chars().count(),
                text.chars().count()
            );
        }
    }

    #[test]
    fn property_non_rand_modes_ignore_seed(
        text in any::<String>(),
        first in any::<u64>(),
        second in any::<u64>(),
    ) {
        for mode in non_rand_modes() {
            prop_assert_eq!(
                apply_mode(&mode, &text, first),
                apply_mode(&mode, &text, second)
            );
        }
    }

    #[test]
    fn property_unknown_leaves_text_unchanged(
        text in any::<String>(),
        raw in any::<String>(),
        seed in any::<u64>(),
    ) {
        prop_assert_eq!(
            apply_mode(&StyleMode::Unknown(raw), &text, seed),
            text
        );
    }

    #[test]
    fn property_parse_covers_every_segment(chain in any::<String>()) {
        prop_assert_eq!(parse_modes(&chain).len(), chain.split('+').count());
    }
}
