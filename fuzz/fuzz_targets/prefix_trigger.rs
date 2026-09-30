#![no_main]

use libfuzzer_sys::fuzz_target;
use two_bot_core::leveling::{valid_command_name, valid_text_trigger};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // The current bot has trigger validators, not a prefix-message dispatcher.
    // Independent byte grammar also checks Unicode and length boundaries.
    let valid_name = |bytes: &[u8]| {
        (1..=32).contains(&bytes.len())
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
    };
    assert_eq!(valid_command_name(text), valid_name(data));
    assert_eq!(
        valid_text_trigger(text),
        data.strip_prefix(b"!").is_some_and(valid_name)
    );
});
