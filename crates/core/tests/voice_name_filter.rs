//! Hermetic acceptance cases for the legacy temp-voice room-name filter core.

use proptest::prelude::*;
use two_bot_core::automod::{AutomodFilter, AutomodPolicy};
use two_bot_core::voice_name_filter::{
    filter_channel_name, render_name_template, resolve_create_name, sanitize_channel_name,
    NameError, NameFilterContext, FALLBACK_TEMPLATE_NAME, MAX_CHANNEL_NAME_CHARS,
    MIN_CHANNEL_NAME_CHARS, NAME_BLOCKED_AUDIT_REASON,
};

fn context() -> NameFilterContext {
    NameFilterContext {
        guild_id: "111111111111111111".to_owned(),
        channel_id: "222222222222222222".to_owned(),
        user_id: "444444444444444444".to_owned(),
    }
}

fn policy_with_words(words: &[&str]) -> AutomodPolicy {
    AutomodPolicy {
        bad_words: words.iter().map(ToString::to_string).collect(),
        ..AutomodPolicy::default()
    }
}

fn policy_allowing(domain: &str) -> AutomodPolicy {
    AutomodPolicy {
        allowed_domains: vec![domain.to_owned()],
        ..AutomodPolicy::default()
    }
}

fn reject(raw: &str, policy: &AutomodPolicy) -> NameError {
    filter_channel_name(raw, policy, &context()).expect_err("must reject")
}

#[test]
fn sanitize_table() {
    for (raw, expected) in [
        // NFKC first: full-width folds (then `@` strips), ligatures expand.
        ("Ｈｅｌｌｏ", "Hello"),
        ("ﬁsh", "fish"),
        ("＠everyone", "everyone"),
        // C0 controls and DEL become spaces, then collapse.
        ("a\u{0000}b\u{0007}c\u{007F}d", "a b c d"),
        ("a\nb\tc\rd", "a b c d"),
        ("a\u{0085}b", "a\u{0085}b"),
        ("a\u{FEFF}b", "a b"),
        // `@` and backtick strip; whitespace runs collapse and trim.
        ("@everyone `code`", "everyone code"),
        ("@@@```", ""),
        ("  a \t\n b   c  ", "a b c"),
        ("plain name", "plain name"),
    ] {
        assert_eq!(sanitize_channel_name(raw), expected, "{raw:?}");
    }
}

proptest! {
    #[test]
    fn sanitize_is_a_fixed_point(raw in ".*") {
        let once = sanitize_channel_name(&raw);
        prop_assert_eq!(sanitize_channel_name(&once), once);
        prop_assert!(!once.contains(['@', '`']));
        // Legacy strips only C0 + DEL (C1 controls survive, as in legacy).
        prop_assert!(!once.chars().any(|c| matches!(
            c,
            '\u{0000}'..='\u{001F}' | '\u{007F}'
        )));
    }
}

#[test]
fn length_bounds_apply_to_the_sanitized_name() {
    assert_eq!(MIN_CHANNEL_NAME_CHARS, 1);
    assert_eq!(MAX_CHANNEL_NAME_CHARS, 100);
    let policy = AutomodPolicy::default();
    assert_eq!(reject("", &policy), NameError::Empty);
    assert_eq!(reject("@@@", &policy), NameError::Empty);
    assert_eq!(
        reject("", &policy).to_string(),
        "That name is empty once formatting is removed."
    );
    // Exactly 100 scalars passes; 101 refuses with its length.
    assert_eq!(
        filter_channel_name(&"x".repeat(100), &policy, &context()),
        Ok("x".repeat(100))
    );
    assert_eq!(
        reject(&"x".repeat(101), &policy),
        NameError::TooLong { len: 101 }
    );
    assert_eq!(
        reject(&"x".repeat(101), &policy).to_string(),
        "Channel names are at most 100 characters."
    );
    // Bounds apply after sanitize: stripped sigils do not count.
    let padded = format!("{}@", "x".repeat(100));
    assert_eq!(
        filter_channel_name(&padded, &policy, &context()),
        Ok("x".repeat(100))
    );
    // Astral scalars count one each (legacy UTF-16 units would count two).
    assert_eq!(
        filter_channel_name(&"🎮".repeat(100), &policy, &context()),
        Ok("🎮".repeat(100))
    );
    assert!(matches!(
        reject(&"🎮".repeat(101), &policy),
        NameError::TooLong { len: 101 }
    ));
    assert_eq!(NameError::Empty.filter(), None);
    assert_eq!(NameError::TooLong { len: 101 }.filter(), None);
}

#[test]
fn bad_words_reject_with_the_legacy_sentence() {
    let policy = policy_with_words(&["spamword"]);
    assert_eq!(
        reject("this is spamword here", &policy),
        NameError::Blocked {
            filter: AutomodFilter::BadWords
        }
    );
    assert_eq!(
        reject("this is spamword here", &policy).to_string(),
        "That name is not allowed here (bad words)."
    );
    // Boundaries hold: a longer word containing the term passes.
    assert!(filter_channel_name("this is spamwordish", &policy, &context()).is_ok());
    // Evasion still matches: zero-width gaps, inner spaces, full-width input.
    for raw in [
        "s\u{200B}p\u{200C}am\u{200D}word",
        "s p a m w o r d",
        "ＳＰＡＭＷＯＲＤ",
        "as spamword!",
    ] {
        assert_eq!(
            reject(raw, &policy),
            NameError::Blocked {
                filter: AutomodFilter::BadWords
            },
            "{raw:?}"
        );
    }
    assert!(filter_channel_name("hello world, good game tonight", &policy, &context()).is_ok());
    assert_eq!(
        reject("spamword here", &policy).filter(),
        Some(AutomodFilter::BadWords)
    );
}

#[test]
fn invite_links_reject_with_the_legacy_sentence() {
    let policy = AutomodPolicy::default();
    for raw in [
        "join https://discord.gg/abc123 now",
        "see discord.com/invite/xyz789 ok",
    ] {
        assert_eq!(
            reject(raw, &policy),
            NameError::Blocked {
                filter: AutomodFilter::InviteLink
            },
            "{raw:?}"
        );
    }
    assert_eq!(
        reject("join https://discord.gg/abc123 now", &policy).to_string(),
        "That name is not allowed here (invite link)."
    );
    // A bare `discord.gg` with no code is an external link, not an invite.
    assert_eq!(
        reject("plain discord.gg with no code", &policy),
        NameError::Blocked {
            filter: AutomodFilter::ExternalLink
        }
    );
    assert!(filter_channel_name("plain discord with no code", &policy, &context()).is_ok());
}

#[test]
fn external_links_reject_unless_allowlisted() {
    let policy = policy_allowing("two.gg");
    assert_eq!(
        reject("visit https://evil.example.com now", &policy),
        NameError::Blocked {
            filter: AutomodFilter::ExternalLink
        }
    );
    assert_eq!(
        reject("visit https://evil.example.com now", &policy).to_string(),
        "That name is not allowed here (external link)."
    );
    assert_eq!(
        reject("see evil.example.com ok", &policy),
        NameError::Blocked {
            filter: AutomodFilter::ExternalLink
        }
    );
    assert!(filter_channel_name("read https://two.gg/news today", &policy, &context()).is_ok());
    assert!(filter_channel_name("read https://sub.two.gg/news today", &policy, &context()).is_ok());
}

#[test]
fn inapplicable_filters_are_neutralised_not_ignored() {
    // A guild running mention limit 0 would short-circuit on `mention_spam`
    // before the invite check if the policy were passed through unchanged.
    let mention_zero = AutomodPolicy {
        mention_limit: 0,
        ..AutomodPolicy::default()
    };
    assert!(filter_channel_name("plain name", &mention_zero, &context()).is_ok());
    assert_eq!(
        reject("join https://discord.gg/abc123 now", &mention_zero),
        NameError::Blocked {
            filter: AutomodFilter::InviteLink
        }
    );
    // A repeat count of 1 would fire on every non-empty name; attachment text
    // is content, not an attachment.
    let repeat_one = AutomodPolicy {
        repeated_message_count: 1,
        ..AutomodPolicy::default()
    };
    assert!(filter_channel_name("plain name", &repeat_one, &context()).is_ok());
    let policy = AutomodPolicy::default();
    assert!(filter_channel_name("Setup.EXE", &policy, &context()).is_ok());
    // Bad words still win under a neutralised policy.
    let hostile = AutomodPolicy {
        mention_limit: 0,
        repeated_message_count: 1,
        bad_words: vec!["spamword".to_owned()],
        ..AutomodPolicy::default()
    };
    assert_eq!(
        reject("spamword here", &hostile),
        NameError::Blocked {
            filter: AutomodFilter::BadWords
        }
    );
}

#[test]
fn accepted_names_come_back_sanitized() {
    let policy = AutomodPolicy::default();
    assert_eq!(
        filter_channel_name("  Hi @there  `buddy` ", &policy, &context()),
        Ok("Hi there buddy".to_owned())
    );
}

#[test]
fn render_substitutes_sanitizes_falls_back_and_truncates() {
    assert_eq!(FALLBACK_TEMPLATE_NAME, "voice channel");
    assert_eq!(
        render_name_template("{username}'s room #{seq}", "Ava", 1, 3),
        "Ava's room #3"
    );
    // `{username}` replaces first, so a username holding `{count}` expands.
    assert_eq!(
        render_name_template("{username} #{count}", "{count}", 5, 9),
        "5 #5"
    );
    // Sanitized, not rejected: sigils strip, edges trim.
    assert_eq!(render_name_template("{username}", "  Ava  ", 1, 1), "Ava");
    assert_eq!(
        render_name_template("@{username} `hi`", "Ava", 1, 1),
        "Ava hi"
    );
    // Empty renders fall back; long renders truncate to 100 scalars.
    assert_eq!(
        render_name_template("{username}", "@@@", 1, 1),
        "voice channel"
    );
    assert_eq!(render_name_template("", "Ava", 1, 1), "voice channel");
    assert_eq!(
        render_name_template(&"x".repeat(500), "Ava", 1, 1),
        "x".repeat(100)
    );
}

#[test]
fn create_path_uses_the_rendered_name_directly() {
    let policy = AutomodPolicy::default();
    let resolved = resolve_create_name("{username}'s room", "Ava", 1, 3, &policy, &context())
        .expect("clean name");
    assert_eq!(resolved.name, "Ava's room");
    assert!(!resolved.username_stripped);
}

#[test]
fn create_path_retries_without_the_username() {
    let policy = policy_with_words(&["spamword"]);
    let resolved = resolve_create_name("{username} hangout", "spamword", 1, 2, &policy, &context())
        .expect("bare template passes");
    assert_eq!(resolved.name, "hangout");
    assert!(resolved.username_stripped);
    // Invites in the username retry the same way.
    let policy = AutomodPolicy::default();
    let resolved = resolve_create_name(
        "{username}'s room",
        "discord.gg/abc123",
        1,
        2,
        &policy,
        &context(),
    )
    .expect("bare template passes");
    assert_eq!(resolved.name, "'s room");
    assert!(resolved.username_stripped);
}

#[test]
fn create_path_refuses_only_when_the_bare_template_is_blocked() {
    let policy = policy_with_words(&["spamword"]);
    let blocked = resolve_create_name("spamword lounge", "Ava", 1, 2, &policy, &context())
        .expect_err("template itself blocked");
    assert_eq!(blocked.audit_reason, NAME_BLOCKED_AUDIT_REASON);
    assert_eq!(
        blocked.reason,
        "That channel name is not allowed here. That name is not allowed here (bad words)."
    );
    assert_eq!(
        blocked.error,
        NameError::Blocked {
            filter: AutomodFilter::BadWords
        }
    );
    assert_eq!(blocked.filter(), Some(AutomodFilter::BadWords));
    // A template with no username placeholder refuses the same way.
    let blocked = resolve_create_name("spamword", "whoever", 1, 2, &policy, &context())
        .expect_err("no placeholder to strip");
    assert_eq!(blocked.audit_reason, NAME_BLOCKED_AUDIT_REASON);
    // Render-level empty can never refuse: it falls back to "voice channel".
    let policy = AutomodPolicy::default();
    let resolved = resolve_create_name("{username}", "@@@", 1, 1, &policy, &context())
        .expect("fallback name passes");
    assert_eq!(resolved.name, "voice channel");
    assert!(!resolved.username_stripped);
}
