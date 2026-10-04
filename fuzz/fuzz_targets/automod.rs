#![no_main]

use libfuzzer_sys::fuzz_target;
use two_bot_core::automod::{match_automod, AutomodMessage, AutomodPolicy, RepeatTracker};

fuzz_target!(|data: &[u8]| {
    // Discord supplies UTF-8 strings. Lossy conversion keeps arbitrary bytes
    // useful for normalization, URL scanning and attachment-name inspection.
    let text = String::from_utf8_lossy(data).into_owned();
    let mut policy = AutomodPolicy {
        bad_words: vec!["spamword".to_owned()],
        allowed_domains: vec!["example.org".to_owned()],
        ..AutomodPolicy::default()
    };
    let mut message = AutomodMessage {
        guild_id: "guild".to_owned(),
        channel_id: "channel".to_owned(),
        message_id: "0".to_owned(),
        author_id: "author".to_owned(),
        author_is_bot: false,
        role_ids: Vec::new(),
        content: text.clone(),
        mentioned_user_ids: Vec::new(),
        attachment_names: vec![text.clone()],
        observed_timestamp_ms: 1_000_000,
    };
    let mut repeats = RepeatTracker::default();
    for id in 0..3 {
        message.message_id = id.to_string();
        if !policy.is_exempt(&message) {
            let _ = match_automod(&message, &policy, &mut repeats);
        }
    }

    // Mutate policy text independently of the fixed bad-word seed, and cover
    // explicit mention counts without starving the earlier link scans above.
    policy.bad_words.push(text.chars().take(32).collect());
    message.mentioned_user_ids = (0..data.first().copied().unwrap_or(0) % 8)
        .map(|id| id.to_string())
        .collect();
    let _ = match_automod(&message, &policy, &mut RepeatTracker::default());
    message.author_is_bot = true;
    assert!(policy.is_exempt(&message));
});
