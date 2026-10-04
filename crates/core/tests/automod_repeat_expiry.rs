//! Automod repeat-window and expiry acceptance (TOG-12544).
//!
//! Pins the existing public `AutomodConfig` / `RepeatTracker` API against
//! legacy `MemoryRepeatTracker.observe` and the `matchAutomod` filter order
//! (parity §8): the configured window trips the repeat sanction, `expire`
//! leaves an idle guild with no stale history, authors never pool, empty
//! normalized content never counts, and every message that passes the
//! bad-word check is observed. `RepeatTracker` exposes no rows, so expiry is
//! proven by a delayed probe that would trip if a stale row survived, next
//! to a control where it does. Synthetic fixtures only: no Discord, network,
//! database, process environment or wall clock. See
//! `docs/automod-repeat-expiry.md`.

use std::collections::HashMap;

use two_bot_core::automod::{
    match_automod, normalize_content, sanction_for, AutomodConfig, AutomodFilter, AutomodMessage,
    AutomodPolicy, RepeatTracker, SanctionAction,
};

const GUILD: &str = "111111111111111111";
const OTHER_GUILD: &str = "121212121212121212";
const AUTHOR: &str = "444444444444444444";
const OTHER_AUTHOR: &str = "555555555555555555";
const THIRD_AUTHOR: &str = "666666666666666666";
const T0: u64 = 1_000_000;

fn automod_config(vars: &[(&str, &str)]) -> AutomodConfig {
    let vars: HashMap<String, String> = vars
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    AutomodConfig::from_map(&vars).expect("valid automod fixture config")
}

fn message(guild: &str, author: &str, id: u64, content: &str, at_ms: u64) -> AutomodMessage {
    AutomodMessage {
        guild_id: guild.to_owned(),
        channel_id: "222222222222222222".to_owned(),
        message_id: id.to_string(),
        author_id: author.to_owned(),
        author_is_bot: false,
        role_ids: vec![],
        content: content.to_owned(),
        mentioned_user_ids: vec![],
        attachment_names: vec![],
        observed_timestamp_ms: at_ms,
    }
}

/// Direct tracker call with the normalization `match_automod` applies.
fn observe(repeats: &mut RepeatTracker, message: &AutomodMessage, policy: &AutomodPolicy) -> bool {
    repeats.observe(message, &normalize_content(&message.content), policy)
}

/// Two copies per author at `T0` and `T0 + 1s`, an optional sweep, then a
/// delayed CREATE per author stamped `T0 + 2s`, well inside the original
/// window. With the default count of three, a surviving row trips the probe.
fn seed_idle_guild_then_probe(sweep_at_ms: Option<u64>) -> Vec<bool> {
    let config = automod_config(&[]);
    let policy = &config.policy;
    let mut repeats = RepeatTracker::default();
    let mut id = 0;
    for author in [AUTHOR, OTHER_AUTHOR] {
        for at in [T0, T0 + 1_000] {
            id += 1;
            let seed = message(GUILD, author, id, "same fixture", at);
            assert!(!observe(&mut repeats, &seed, policy));
        }
    }
    if let Some(now_ms) = sweep_at_ms {
        repeats.expire(now_ms, policy.repeated_message_window_seconds);
    }
    [AUTHOR, OTHER_AUTHOR]
        .into_iter()
        .map(|author| {
            id += 1;
            let probe = message(GUILD, author, id, "same fixture", T0 + 2_000);
            observe(&mut repeats, &probe, policy)
        })
        .collect()
}

#[test]
fn configured_count_inside_window_trips_repeat_sanction() {
    let config = automod_config(&[
        ("TWO_AUTOMOD_REPEAT_COUNT", "4"),
        ("TWO_AUTOMOD_REPEAT_WINDOW_SECONDS", "10"),
    ]);
    let policy = &config.policy;
    assert_eq!(policy.repeated_message_count, 4);
    assert_eq!(policy.repeated_message_window_seconds, 10);

    let mut repeats = RepeatTracker::default();
    // N - 1 copies stay clean, including past the default count of three.
    for (id, at) in [(1, T0), (2, T0 + 3_000), (3, T0 + 6_000)] {
        let copy = message(GUILD, AUTHOR, id, "buy cheap gold", at);
        assert_eq!(match_automod(&copy, policy, &mut repeats), None);
    }
    // The Nth copy lands exactly one window after the first: inclusive cutoff.
    let nth = message(GUILD, AUTHOR, 4, "buy cheap gold", T0 + 10_000);
    assert_eq!(
        match_automod(&nth, policy, &mut repeats),
        Some(AutomodFilter::RepeatedMessage)
    );
    // A repeat hit is an ordinary violation on the configured ladder.
    assert_eq!(
        sanction_for(1, &policy.sanctions).action,
        SanctionAction::Delete
    );

    let mut late = RepeatTracker::default();
    for (id, at) in [(1, T0), (2, T0 + 3_000), (3, T0 + 6_000)] {
        let copy = message(GUILD, AUTHOR, id, "buy cheap gold", at);
        assert_eq!(match_automod(&copy, policy, &mut late), None);
    }
    // One millisecond later the first copy is outside the window: three remain.
    let outside = message(GUILD, AUTHOR, 4, "buy cheap gold", T0 + 10_001);
    assert_eq!(match_automod(&outside, policy, &mut late), None);
    // The streak continues from the copies still inside the window.
    let next = message(GUILD, AUTHOR, 5, "buy cheap gold", T0 + 10_002);
    assert_eq!(
        match_automod(&next, policy, &mut late),
        Some(AutomodFilter::RepeatedMessage)
    );
}

#[test]
fn expire_sweeps_every_idle_author_without_another_message() {
    let window_ms = automod_config(&[]).policy.repeated_message_window_seconds * 1_000;
    // Control: without a sweep the delayed probe pools with the seeded rows.
    assert_eq!(seed_idle_guild_then_probe(None), [true, true]);
    // A sweep one millisecond past the newest row's window empties the guild;
    // neither author sent anything after the seed.
    assert_eq!(
        seed_idle_guild_then_probe(Some(T0 + 1_000 + window_ms + 1)),
        [false, false]
    );
}

#[test]
fn expire_drops_only_rows_older_than_the_window() {
    // Count two: one surviving row plus the probe is a repeat.
    let config = automod_config(&[("TWO_AUTOMOD_REPEAT_COUNT", "2")]);
    let policy = &config.policy;
    let window = policy.repeated_message_window_seconds;
    let seed = message(GUILD, AUTHOR, 1, "boundary fixture", T0);
    let probe = message(GUILD, AUTHOR, 2, "boundary fixture", T0 + 500);

    // A row exactly one window old survives the sweep (inclusive cutoff).
    let mut kept = RepeatTracker::default();
    assert!(!observe(&mut kept, &seed, policy));
    kept.expire(T0 + window * 1_000, window);
    assert!(observe(&mut kept, &probe, policy));

    // One millisecond older and it is gone.
    let mut dropped = RepeatTracker::default();
    assert!(!observe(&mut dropped, &seed, policy));
    dropped.expire(T0 + window * 1_000 + 1, window);
    assert!(!observe(&mut dropped, &probe, policy));

    // A sweep removes stale rows only; an author's fresher rows remain.
    let config = automod_config(&[]);
    let policy = &config.policy;
    let mut partial = RepeatTracker::default();
    for (id, at) in [(1, T0), (2, T0 + 20_000)] {
        let copy = message(GUILD, AUTHOR, id, "partial fixture", at);
        assert!(!observe(&mut partial, &copy, policy));
    }
    partial.expire(T0 + window * 1_000 + 1, window);
    // Had the T0 row survived, this would already be the third copy.
    let third = message(GUILD, AUTHOR, 3, "partial fixture", T0 + 21_000);
    assert!(!observe(&mut partial, &third, policy));
    // The retained T0 + 20s row still counts.
    let fourth = message(GUILD, AUTHOR, 4, "partial fixture", T0 + 22_000);
    assert!(observe(&mut partial, &fourth, policy));
}

#[test]
fn repeats_from_different_authors_never_combine() {
    let config = automod_config(&[]);
    let policy = &config.policy;
    let authors = [AUTHOR, OTHER_AUTHOR, THIRD_AUTHOR];
    let mut repeats = RepeatTracker::default();
    let mut id = 0;
    // Six identical copies inside one window, two per author: no hit.
    for _ in 0..2 {
        for author in authors {
            id += 1;
            let copy = message(GUILD, author, id, "same fixture", T0 + id * 1_000);
            assert_eq!(match_automod(&copy, policy, &mut repeats), None);
        }
    }
    // The same author id in another guild is a separate history.
    id += 1;
    let elsewhere = message(OTHER_GUILD, AUTHOR, id, "same fixture", T0 + id * 1_000);
    assert_eq!(match_automod(&elsewhere, policy, &mut repeats), None);
    // Each author's own third copy trips: history was kept, never pooled.
    for author in authors {
        id += 1;
        let third = message(GUILD, author, id, "same fixture", T0 + id * 1_000);
        assert_eq!(
            match_automod(&third, policy, &mut repeats),
            Some(AutomodFilter::RepeatedMessage)
        );
    }
}

#[test]
fn empty_normalized_content_never_counts() {
    // Count two: a single recorded blank would make the next one a repeat.
    let config = automod_config(&[("TWO_AUTOMOD_REPEAT_COUNT", "2")]);
    let policy = &config.policy;
    // Whitespace that legacy `\s` and Rust `char::is_whitespace` both
    // collapse. U+FEFF and U+0085 diverge; see the doc.
    let blanks = [
        "",
        " ",
        "\t\n\r",
        "\u{00A0}\u{3000}",
        "\u{2003} \u{2028}\u{2029}",
    ];
    let mut repeats = RepeatTracker::default();
    let mut id = 0;
    for blank in blanks {
        assert_eq!(normalize_content(blank), "", "{blank:?}");
        for _ in 0..3 {
            id += 1;
            let copy = message(GUILD, AUTHOR, id, blank, T0 + id);
            assert!(!observe(&mut repeats, &copy, policy), "{blank:?}");
        }
    }

    // Blanks are not recorded, so they cannot displace a real streak either:
    // count two retains only two rows per author.
    let mut streak = RepeatTracker::default();
    let first = message(GUILD, AUTHOR, 100, "streak fixture", T0);
    assert_eq!(match_automod(&first, policy, &mut streak), None);
    for (offset, blank) in (1..).zip(blanks) {
        let copy = message(GUILD, AUTHOR, 100 + offset, blank, T0 + offset);
        assert_eq!(match_automod(&copy, policy, &mut streak), None);
    }
    let second = message(GUILD, AUTHOR, 200, "streak fixture", T0 + 1_000);
    assert_eq!(
        match_automod(&second, policy, &mut streak),
        Some(AutomodFilter::RepeatedMessage)
    );
}

#[test]
fn messages_past_the_bad_word_check_count_even_when_a_later_filter_matches() {
    let config = automod_config(&[
        ("TWO_AUTOMOD_BAD_WORDS", "spamword"),
        ("TWO_AUTOMOD_MENTION_LIMIT", "3"),
    ]);
    let policy = &config.policy;
    let mentions: Vec<String> = (1..=3).map(|n| format!("90000000000000000{n}")).collect();
    let cases: [(&str, &[&str], &[String], AutomodFilter); 3] = [
        (
            "join discord.gg/abc123",
            &[],
            &[],
            AutomodFilter::InviteLink,
        ),
        ("hey all", &[], &mentions, AutomodFilter::MentionSpam),
        ("see file", &["run.exe"], &[], AutomodFilter::AttachmentType),
    ];
    for (content, attachments, mentioned, later) in cases {
        let mut repeats = RepeatTracker::default();
        for id in 1..=2 {
            let mut copy = message(GUILD, AUTHOR, id, content, T0 + id * 1_000);
            copy.attachment_names = attachments.iter().map(ToString::to_string).collect();
            copy.mentioned_user_ids = mentioned.to_vec();
            assert_eq!(
                match_automod(&copy, policy, &mut repeats),
                Some(later),
                "{content}"
            );
        }
        // `repeated_message` precedes the later filter, and both earlier
        // copies were observed even though they reported that filter.
        let mut third = message(GUILD, AUTHOR, 3, content, T0 + 3_000);
        third.attachment_names = attachments.iter().map(ToString::to_string).collect();
        third.mentioned_user_ids = mentioned.to_vec();
        assert_eq!(
            match_automod(&third, policy, &mut repeats),
            Some(AutomodFilter::RepeatedMessage),
            "{content}"
        );
    }
}

#[test]
fn bad_word_hits_return_before_the_repeat_observation() {
    let config = automod_config(&[("TWO_AUTOMOD_BAD_WORDS", "spamword")]);
    let policy = &config.policy;

    // Interleaved bad-word hits are not recorded, so they neither count nor
    // displace the clean streak (count three keeps three rows per author).
    let mut repeats = RepeatTracker::default();
    let sequence = [
        ("streak fixture", None),
        ("spamword", Some(AutomodFilter::BadWords)),
        ("spamword", Some(AutomodFilter::BadWords)),
        // A bad word that is also a third copy still reports bad_words first.
        ("spamword", Some(AutomodFilter::BadWords)),
        ("streak fixture", None),
        ("streak fixture", Some(AutomodFilter::RepeatedMessage)),
    ];
    for (id, (content, expected)) in (1..).zip(sequence) {
        let copy = message(GUILD, AUTHOR, id, content, T0 + id * 1_000);
        assert_eq!(
            match_automod(&copy, policy, &mut repeats),
            expected,
            "#{id}"
        );
    }

    // Two hits leave no rows: once the word is no longer blocked, the same
    // text starts a fresh count instead of tripping as the third copy.
    let mut fresh = RepeatTracker::default();
    for id in 1..=2 {
        let hit = message(GUILD, AUTHOR, id, "spamword", T0 + id * 1_000);
        assert_eq!(
            match_automod(&hit, policy, &mut fresh),
            Some(AutomodFilter::BadWords)
        );
    }
    let relaxed = AutomodPolicy {
        bad_words: vec![],
        ..policy.clone()
    };
    let expected = [None, None, Some(AutomodFilter::RepeatedMessage)];
    for (id, expected) in (3..).zip(expected) {
        let copy = message(GUILD, AUTHOR, id, "spamword", T0 + id * 1_000);
        assert_eq!(
            match_automod(&copy, &relaxed, &mut fresh),
            expected,
            "relaxed #{id}"
        );
    }
}

#[test]
fn repeat_counting_folds_case_and_whitespace_variants() {
    // The repeat digest hashes the normalized text (NFKC + lowercase +
    // whitespace collapse), so visual copies with different casing or spacing
    // are the same message for counting purposes.
    let config = automod_config(&[]);
    let policy = &config.policy;
    let mut repeats = RepeatTracker::default();
    for (id, content) in [(1, "Buy Cheap Gold"), (2, "buy  cheap   gold")] {
        let copy = message(GUILD, AUTHOR, id, content, T0 + id * 1_000);
        assert_eq!(normalize_content(content), "buy cheap gold", "{content:?}");
        assert_eq!(match_automod(&copy, policy, &mut repeats), None);
    }
    let third = message(GUILD, AUTHOR, 3, "BUY CHEAP GOLD", T0 + 3_000);
    assert_eq!(
        match_automod(&third, policy, &mut repeats),
        Some(AutomodFilter::RepeatedMessage)
    );
}
