//! Pure V6a styling-mode library derived from `docs/voice-rooms.md`.
//!
//! `""mode:text""` transforms are total `&str -> String` functions with no
//! Discord types, no store access, no I/O and no dependency on any other
//! voice slice. Modes chain with `+` and apply left to right; unknown modes
//! leave the text unchanged. The case, word, novelty and Unicode-font
//! behaviour below is an original implementation written from the
//! specification's described behaviour only. Font code points follow the
//! Unicode code charts, including the reserved-gap capitals that live in the
//! Letterlike Symbols block; characters outside `A-Z`, `a-z` and `0-9` pass
//! through the font maps unchanged.

/// The stop words dropped by [`StyleMode::RemShort`], exactly the
/// specification's list.
pub const REMSHORT_STOP_WORDS: &[&str] = &[
    "a", "an", "and", "at", "by", "from", "in", "is", "of", "on", "or", "the", "to",
];

/// One styling mode from a `""mode:text""` chain.
///
/// `Unknown` carries the raw trimmed segment so callers can surface it; it
/// always leaves the text unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StyleMode {
    Upper,
    Lower,
    Title,
    Swap,
    SmallCaps,
    Rand,
    Spaces,
    Acro,
    RemShort,
    FirstWords(u32),
    Uwu,
    UpsideDown,
    Bold,
    Italic,
    BoldItalic,
    Script,
    BoldScript,
    Fraktur,
    BoldFraktur,
    Double,
    Sans,
    BoldSans,
    ItalicSans,
    BoldItalicSans,
    Mono,
    Unknown(String),
}

/// Parse a `+`-chained mode list such as `"upper+bold"`.
///
/// Matching is ASCII case-insensitive and segments are trimmed; `caps` is an
/// alias for `upper`, and `<N>w` (ASCII digits followed by `w`) keeps the
/// first `N` words. Anything else becomes [`StyleMode::Unknown`] and leaves
/// the text unchanged. Every `+`-separated segment yields exactly one mode,
/// so an empty chain parses to a single empty `Unknown`.
pub fn parse_modes(chain: &str) -> Vec<StyleMode> {
    chain
        .split('+')
        .map(|raw| {
            let token = raw.trim();
            match token.to_ascii_lowercase().as_str() {
                "upper" | "caps" => StyleMode::Upper,
                "lower" => StyleMode::Lower,
                "title" => StyleMode::Title,
                "swap" => StyleMode::Swap,
                "scaps" => StyleMode::SmallCaps,
                "rand" => StyleMode::Rand,
                "spaces" => StyleMode::Spaces,
                "acro" => StyleMode::Acro,
                "remshort" => StyleMode::RemShort,
                "uwu" => StyleMode::Uwu,
                "usd" => StyleMode::UpsideDown,
                "bold" => StyleMode::Bold,
                "italic" => StyleMode::Italic,
                "bolditalic" => StyleMode::BoldItalic,
                "script" => StyleMode::Script,
                "boldscript" => StyleMode::BoldScript,
                "fraktur" => StyleMode::Fraktur,
                "boldfraktur" => StyleMode::BoldFraktur,
                "double" => StyleMode::Double,
                "sans" => StyleMode::Sans,
                "boldsans" => StyleMode::BoldSans,
                "italicsans" => StyleMode::ItalicSans,
                "bolditalicsans" => StyleMode::BoldItalicSans,
                "mono" => StyleMode::Mono,
                other => match other
                    .strip_suffix('w')
                    .filter(|digits| !digits.is_empty())
                    .and_then(|digits| digits.parse::<u32>().ok())
                {
                    Some(n) => StyleMode::FirstWords(n),
                    None => StyleMode::Unknown(token.to_string()),
                },
            }
        })
        .collect()
}

/// Apply one mode. `seed` only affects [`StyleMode::Rand`]; every other mode
/// ignores it so chains stay deterministic.
pub fn apply_mode(mode: &StyleMode, text: &str, seed: u64) -> String {
    match mode {
        StyleMode::Upper => text.chars().flat_map(|c| c.to_uppercase()).collect(),
        StyleMode::Lower => text.chars().flat_map(|c| c.to_lowercase()).collect(),
        StyleMode::Title => {
            let mut out = String::with_capacity(text.len());
            let mut word_start = true;
            for c in text.chars() {
                if c.is_whitespace() {
                    out.push(c);
                    word_start = true;
                } else if word_start {
                    out.extend(c.to_uppercase());
                    word_start = false;
                } else {
                    out.extend(c.to_lowercase());
                    word_start = false;
                }
            }
            out
        }
        StyleMode::Swap => text
            .chars()
            .flat_map(|c| {
                if c.is_lowercase() {
                    c.to_uppercase().collect::<Vec<_>>()
                } else if c.is_uppercase() {
                    c.to_lowercase().collect::<Vec<_>>()
                } else {
                    vec![c]
                }
            })
            .collect(),
        StyleMode::SmallCaps => text.chars().map(map_small_caps).collect(),
        StyleMode::Rand => random_case(text, seed),
        StyleMode::Spaces => {
            let chars: Vec<String> = text.chars().map(|c| c.to_string()).collect();
            chars.join(" ")
        }
        StyleMode::Acro => text
            .split_whitespace()
            .filter_map(|word| word.chars().next())
            .collect(),
        StyleMode::RemShort => text
            .split_whitespace()
            .filter(|word| {
                !REMSHORT_STOP_WORDS
                    .iter()
                    .any(|stop| word.eq_ignore_ascii_case(stop))
            })
            .collect::<Vec<_>>()
            .join(" "),
        StyleMode::FirstWords(n) => text
            .split_whitespace()
            .take(*n as usize)
            .collect::<Vec<_>>()
            .join(" "),
        StyleMode::Uwu => uwu(text),
        StyleMode::UpsideDown => text.chars().rev().map(map_upside_down).collect(),
        StyleMode::Bold => apply_font(text, &FontMap::BOLD),
        StyleMode::Italic => apply_font(text, &FontMap::ITALIC),
        StyleMode::BoldItalic => apply_font(text, &FontMap::BOLD_ITALIC),
        StyleMode::Script => apply_font(text, &FontMap::SCRIPT),
        StyleMode::BoldScript => apply_font(text, &FontMap::BOLD_SCRIPT),
        StyleMode::Fraktur => apply_font(text, &FontMap::FRAKTUR),
        StyleMode::BoldFraktur => apply_font(text, &FontMap::BOLD_FRAKTUR),
        StyleMode::Double => apply_font(text, &FontMap::DOUBLE),
        StyleMode::Sans => apply_font(text, &FontMap::SANS),
        StyleMode::BoldSans => apply_font(text, &FontMap::BOLD_SANS),
        StyleMode::ItalicSans => apply_font(text, &FontMap::ITALIC_SANS),
        StyleMode::BoldItalicSans => apply_font(text, &FontMap::BOLD_ITALIC_SANS),
        StyleMode::Mono => apply_font(text, &FontMap::MONO),
        StyleMode::Unknown(_) => text.to_string(),
    }
}

/// Apply modes left to right; `seed` is threaded to every mode but only
/// [`StyleMode::Rand`] reads it.
pub fn apply_chain(modes: &[StyleMode], text: &str, seed: u64) -> String {
    modes
        .iter()
        .fold(text.to_string(), |acc, mode| apply_mode(mode, &acc, seed))
}

/// Small capitals convert lowercase letters only. There is no small-capital
/// `x` in Unicode, so `x` passes through unchanged, as does everything that
/// is not a lowercase ASCII letter.
fn map_small_caps(c: char) -> char {
    match c {
        'a' => 'ᴀ',
        'b' => 'ʙ',
        'c' => 'ᴄ',
        'd' => 'ᴅ',
        'e' => 'ᴇ',
        'f' => 'ꜰ',
        'g' => 'ɢ',
        'h' => 'ʜ',
        'i' => 'ɪ',
        'j' => 'ᴊ',
        'k' => 'ᴋ',
        'l' => 'ʟ',
        'm' => 'ᴍ',
        'n' => 'ɴ',
        'o' => 'ᴏ',
        'p' => 'ᴘ',
        'q' => 'ꞯ',
        'r' => 'ʀ',
        's' => 'ꜱ',
        't' => 'ᴛ',
        'u' => 'ᴜ',
        'v' => 'ᴠ',
        'w' => 'ᴡ',
        'y' => 'ʏ',
        'z' => 'ᴢ',
        _ => c,
    }
}

/// One step of splitmix64. The state update is bijective, so every seed —
/// including zero — yields a distinct stream, and the same seed always yields
/// the same stream.
fn next_splitmix_bit(state: &mut u64) -> bool {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) & 1 == 1
}

/// Random case, deterministic per caller-supplied seed: each cased character
/// independently becomes upper or lower case. Uncased characters pass through
/// without consuming the stream, so the decision for a letter depends only on
/// the seed and the cased letters before it.
fn random_case(text: &str, seed: u64) -> String {
    let mut state = seed;
    text.chars()
        .flat_map(|c| {
            if c.is_lowercase() || c.is_uppercase() {
                if next_splitmix_bit(&mut state) {
                    c.to_uppercase().collect::<Vec<_>>()
                } else {
                    c.to_lowercase().collect::<Vec<_>>()
                }
            } else {
                vec![c]
            }
        })
        .collect()
}

/// `r`/`l` become `w` (preserving case) and `ove` becomes `uv` (`OVE` to
/// `UV`, `Ove` to `Uve`); everything else passes through.
fn uwu(text: &str) -> String {
    text.replace("ove", "uv")
        .replace("OVE", "UV")
        .replace("Ove", "Uve")
        .chars()
        .map(|c| match c {
            'r' | 'l' => 'w',
            'R' | 'L' => 'W',
            _ => c,
        })
        .collect()
}

/// Flip table for [`StyleMode::UpsideDown`]. Unlisted characters (including
/// most uppercase letters and the digits 4, 5 and 7, which have no stable
/// single-code-point flip) pass through unchanged.
fn map_upside_down(c: char) -> char {
    match c {
        'a' => 'ɐ',
        'b' => 'q',
        'c' => 'ɔ',
        'd' => 'p',
        'e' => 'ǝ',
        'f' => 'ɟ',
        'g' => 'ƃ',
        'h' => 'ɥ',
        'i' => 'ᴉ',
        'j' => 'ɾ',
        'k' => 'ʞ',
        'l' => 'l',
        'm' => 'ɯ',
        'n' => 'u',
        'o' => 'o',
        'p' => 'd',
        'q' => 'b',
        'r' => 'ɹ',
        's' => 's',
        't' => 'ʇ',
        'u' => 'n',
        'v' => 'ʌ',
        'w' => 'ʍ',
        'x' => 'x',
        'y' => 'ʎ',
        'z' => 'z',
        'A' => '∀',
        'E' => 'Ǝ',
        'H' => 'H',
        'I' => 'I',
        'M' => 'W',
        'N' => 'N',
        'O' => 'O',
        'S' => 'S',
        'W' => 'M',
        'X' => 'X',
        'Z' => 'Z',
        '0' => '0',
        '1' => 'Ɩ',
        '2' => 'ᄅ',
        '3' => 'Ɛ',
        '6' => '9',
        '8' => '8',
        '9' => '6',
        '.' => '˙',
        '?' => '¿',
        '!' => '¡',
        '\'' => ',',
        ',' => '\'',
        '_' => '‾',
        '&' => '⅋',
        '(' => ')',
        ')' => '(',
        _ => c,
    }
}

/// One Unicode font: contiguous Mathematical Alphanumeric Symbols bases plus
/// the reserved-gap capitals that live in the Letterlike Symbols block.
/// `digit` is zero when the font has no digit block (italic, bold-italic,
/// script, bold-script, fraktur, bold-fraktur, italic-sans and
/// bold-italic-sans); those digits pass through unchanged.
struct FontMap {
    upper: u32,
    lower: u32,
    digit: u32,
    exceptions: &'static [(char, char)],
}

impl FontMap {
    const BOLD: FontMap = FontMap {
        upper: 0x1D400,
        lower: 0x1D41A,
        digit: 0x1D7CE,
        exceptions: &[],
    };
    const ITALIC: FontMap = FontMap {
        upper: 0x1D434,
        lower: 0x1D44E,
        digit: 0,
        exceptions: &[('h', 'ℎ')],
    };
    const BOLD_ITALIC: FontMap = FontMap {
        upper: 0x1D468,
        lower: 0x1D482,
        digit: 0,
        exceptions: &[],
    };
    const SCRIPT: FontMap = FontMap {
        upper: 0x1D49C,
        lower: 0x1D4B6,
        digit: 0,
        exceptions: &[
            ('B', 'ℬ'),
            ('E', 'ℰ'),
            ('F', 'ℱ'),
            ('H', 'ℋ'),
            ('I', 'ℐ'),
            ('L', 'ℒ'),
            ('M', 'ℳ'),
            ('R', 'ℛ'),
            ('e', 'ℯ'),
            ('g', 'ℊ'),
            ('o', 'ℴ'),
        ],
    };
    const BOLD_SCRIPT: FontMap = FontMap {
        upper: 0x1D4D0,
        lower: 0x1D4EA,
        digit: 0,
        exceptions: &[],
    };
    const FRAKTUR: FontMap = FontMap {
        upper: 0x1D504,
        lower: 0x1D51E,
        digit: 0,
        exceptions: &[('C', 'ℭ'), ('H', 'ℌ'), ('I', 'ℑ'), ('R', 'ℜ'), ('Z', 'ℨ')],
    };
    const BOLD_FRAKTUR: FontMap = FontMap {
        upper: 0x1D56C,
        lower: 0x1D586,
        digit: 0,
        exceptions: &[],
    };
    const DOUBLE: FontMap = FontMap {
        upper: 0x1D538,
        lower: 0x1D552,
        digit: 0x1D7D8,
        exceptions: &[
            ('C', 'ℂ'),
            ('H', 'ℍ'),
            ('N', 'ℕ'),
            ('P', 'ℙ'),
            ('Q', 'ℚ'),
            ('R', 'ℝ'),
            ('Z', 'ℤ'),
        ],
    };
    const SANS: FontMap = FontMap {
        upper: 0x1D5A0,
        lower: 0x1D5BA,
        digit: 0x1D7E2,
        exceptions: &[],
    };
    const BOLD_SANS: FontMap = FontMap {
        upper: 0x1D5D4,
        lower: 0x1D5EE,
        digit: 0x1D7EC,
        exceptions: &[],
    };
    const ITALIC_SANS: FontMap = FontMap {
        upper: 0x1D608,
        lower: 0x1D622,
        digit: 0,
        exceptions: &[],
    };
    const BOLD_ITALIC_SANS: FontMap = FontMap {
        upper: 0x1D63C,
        lower: 0x1D656,
        digit: 0,
        exceptions: &[],
    };
    const MONO: FontMap = FontMap {
        upper: 0x1D670,
        lower: 0x1D68A,
        digit: 0x1D7F6,
        exceptions: &[],
    };
}

fn apply_font(text: &str, map: &FontMap) -> String {
    text.chars()
        .map(|c| {
            if let Some((_, mapped)) = map.exceptions.iter().find(|(from, _)| *from == c) {
                return *mapped;
            }
            match c {
                'A'..='Z' => shift(map.upper, c, 'A'),
                'a'..='z' => shift(map.lower, c, 'a'),
                '0'..='9' if map.digit != 0 => shift(map.digit, c, '0'),
                _ => c,
            }
        })
        .collect()
}

/// Add the letter's zero-based index to a contiguous block base. Every block
/// slot is a valid scalar value, so this cannot fail; the fallback is
/// unreachable defence.
fn shift(base: u32, c: char, zero: char) -> char {
    char::from_u32(base + (c as u32 - zero as u32)).unwrap_or(c)
}
