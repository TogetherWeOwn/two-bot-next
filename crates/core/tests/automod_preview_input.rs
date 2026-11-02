//! Slice 3c: strict option decode, synthetic message build, and the scope
//! gate (spec §§3.1, 5; tests T7 gate half and T11). Offline only: no Discord,
//! no database, no clock beyond explicit receipt timestamps.

use two_bot_core::automod::{AutomodConfig, AutomodFilter, AutomodPolicy, RepeatTracker};
use two_bot_core::automod_preview::{preview_automod, PreviewResult};
use two_bot_core::automod_preview_input::{
    build_preview_message, decode_preview_input, parse_attachment_list, preview_message_id,
    PreviewInputRefusal, PreviewOptionValue,
};
use two_bot_core::automod_runtime::{AutomodRuntime, AutomodScope, STAGING_GUILD_ID};

fn text(value: &str) -> (&str, PreviewOptionValue<'_>) {
    ("text", PreviewOptionValue::Text(value))
}

fn decode(options: &[(&str, PreviewOptionValue<'_>)]) -> Result<String, PreviewInputRefusal> {
    decode_preview_input(options).map(|input| input.text)
}

#[test]
fn strict_decode_refuses_unknown_duplicated_and_mistyped() {
    assert_eq!(
        decode(&[
            ("text", PreviewOptionValue::Text("hi")),
            ("nope", PreviewOptionValue::Text("x"))
        ]),
        Err(PreviewInputRefusal::UnknownOption)
    );
    assert_eq!(
        decode(&[text("a"), text("b")]),
        Err(PreviewInputRefusal::DuplicateOption)
    );
    assert_eq!(
        decode(&[("text", PreviewOptionValue::Integer(3))]),
        Err(PreviewInputRefusal::WrongType)
    );
    assert_eq!(
        decode(&[text("hi"), ("mentions", PreviewOptionValue::Text("3"))]),
        Err(PreviewInputRefusal::WrongType)
    );
    assert_eq!(
        decode(&[("mentions", PreviewOptionValue::Integer(1))]),
        Err(PreviewInputRefusal::MissingText)
    );
    // The unknown name is attacker-controlled vocabulary: the fixed refusal
    // must not echo it.
    let refusal = decode(&[
        ("nope <@everyone>", PreviewOptionValue::Text("x")),
        text("hi"),
    ])
    .expect_err("unknown option refuses");
    assert_eq!(refusal, PreviewInputRefusal::UnknownOption);
    assert!(!refusal.message().contains("<@everyone>"));
    assert!(!refusal.message().contains("nope"));
}

#[test]
fn text_bounds_count_utf16_units_not_chars() {
    assert!(decode(&[text(&"a".repeat(2000))]).is_ok());
    assert_eq!(
        decode(&[text(&"a".repeat(2001))]),
        Err(PreviewInputRefusal::TextBounds)
    );
    assert_eq!(decode(&[text("")]), Err(PreviewInputRefusal::TextBounds));
    // Astral-plane characters cost two units each: 1000 emoji fill the bound,
    // 1001 exceed it while still far under 2000 chars.
    assert!(decode(&[text(&"😀".repeat(1000))]).is_ok());
    assert_eq!(
        decode(&[text(&"😀".repeat(1001))]),
        Err(PreviewInputRefusal::TextBounds)
    );
}

#[test]
fn mentions_default_to_zero_and_hold_the_limit() {
    let input = decode_preview_input(&[text("hi")]).expect("minimal options decode");
    assert_eq!(input.mention_count, 0);
    let input = decode_preview_input(&[text("hi"), ("mentions", PreviewOptionValue::Integer(100))])
        .expect("mentions 100 decodes");
    assert_eq!(input.mention_count, 100);
    assert_eq!(
        decode(&[text("hi"), ("mentions", PreviewOptionValue::Integer(101))]),
        Err(PreviewInputRefusal::MentionsBounds)
    );
    assert_eq!(
        decode(&[text("hi"), ("mentions", PreviewOptionValue::Integer(-1))]),
        Err(PreviewInputRefusal::MentionsBounds)
    );
}

#[test]
fn attachments_hold_count_total_and_name_shape() {
    let ten: Vec<String> = (0..10).map(|i| format!("f{i}.txt")).collect();
    let input = decode_preview_input(&[
        text("hi"),
        ("attachments", PreviewOptionValue::Text(&ten.join(","))),
    ])
    .expect("10 attachments decode");
    assert_eq!(input.attachment_names.len(), 10);
    let eleven: Vec<String> = (0..11).map(|i| format!("f{i}.txt")).collect();
    assert_eq!(
        decode(&[
            text("hi"),
            ("attachments", PreviewOptionValue::Text(&eleven.join(",")))
        ]),
        Err(PreviewInputRefusal::TooManyAttachments)
    );
    assert_eq!(
        parse_attachment_list(&"a".repeat(1001)),
        Err(PreviewInputRefusal::AttachmentsBounds)
    );
    // Comma-boundary ambiguity refuses instead of silently dropping entries.
    for bad in ["a.txt,,b.txt", ",a.txt", "a.txt,", ""] {
        assert_eq!(
            parse_attachment_list(bad),
            Err(PreviewInputRefusal::EmptyFilename),
            "value {bad:?} must refuse on its empty entry"
        );
    }
    for bad in ["a/b.txt", "a\\b.txt", "a\x01b.txt", "a\nb.txt"] {
        assert_eq!(
            parse_attachment_list(bad),
            Err(PreviewInputRefusal::BadFilename),
            "value {bad:?} must refuse on its name shape"
        );
    }
    assert!(parse_attachment_list(&"b".repeat(255)).is_ok());
    assert_eq!(
        parse_attachment_list(&"b".repeat(256)),
        Err(PreviewInputRefusal::BadFilename)
    );
    // Commas separate entries, so `safe.exe,txt` is unambiguously two bare
    // names — never one comma-bearing name reaching the matcher.
    assert_eq!(
        parse_attachment_list("safe.exe,txt"),
        Ok(vec!["safe.exe".to_owned(), "txt".to_owned()])
    );
}

#[test]
fn filename_without_a_dot_is_its_own_extension_as_live() {
    let config = AutomodConfig {
        enabled: true,
        dry_run: true,
        policy: AutomodPolicy {
            blocked_attachment_extensions: vec!["txt".to_owned()],
            ..AutomodPolicy::default()
        },
    };
    let input = decode_preview_input(&[
        text("   "),
        ("attachments", PreviewOptionValue::Text("txt")),
    ])
    .expect("no-dot name with blank text decodes");
    let message =
        build_preview_message(&input, "g", "c", "a", &[], 1, 1_000).expect("message builds");
    let verdict = preview_automod(&message, 0, &config, &RepeatTracker::default());
    assert_eq!(verdict.result, PreviewResult::Matched);
    assert_eq!(verdict.filter, Some(AutomodFilter::AttachmentType));
}

#[test]
fn blank_text_needs_a_postable_form() {
    assert_eq!(
        decode(&[text("   \t\n  ")]),
        Err(PreviewInputRefusal::BlankText)
    );
    // Mention-only and attachment-only posts exist live, so they evaluate.
    let with_mentions =
        decode_preview_input(&[text("   "), ("mentions", PreviewOptionValue::Integer(100))])
            .expect("whitespace with mentions decodes");
    assert_eq!(with_mentions.mention_count, 100);
    let with_files = decode_preview_input(&[
        text("  "),
        ("attachments", PreviewOptionValue::Text("evil.exe")),
    ])
    .expect("whitespace with attachments decodes");
    assert_eq!(with_files.attachment_names, vec!["evil.exe".to_owned()]);
    // Zero-width-only input is not blank: fidelity requires evaluating it.
    assert!(decode(&[text("\u{200b}")]).is_ok());
    // A mention count at the limit decides the whitespace text, as live.
    let config = AutomodConfig {
        enabled: true,
        dry_run: true,
        policy: AutomodPolicy::default(),
    };
    let message = build_preview_message(&with_mentions, "g", "c", "a", &[], 2, 2_000)
        .expect("message builds");
    let verdict = preview_automod(
        &message,
        with_mentions.mention_count,
        &config,
        &RepeatTracker::default(),
    );
    assert_eq!(verdict.result, PreviewResult::Matched);
    assert_eq!(verdict.filter, Some(AutomodFilter::MentionSpam));
}

#[test]
fn decoded_input_builds_the_invokers_own_message() {
    let input = decode_preview_input(&[
        text("hello"),
        ("mentions", PreviewOptionValue::Integer(2)),
        ("attachments", PreviewOptionValue::Text("a.txt")),
    ])
    .expect("decodes");
    assert_eq!(input.text_len_utf16, 5);
    let roles = vec!["role-9".to_owned()];
    let message =
        build_preview_message(&input, "guild-1", "channel-2", "actor-3", &roles, 99, 7_000)
            .expect("message builds");
    assert_eq!(message.guild_id, "guild-1");
    assert_eq!(message.channel_id, "channel-2");
    assert_eq!(message.author_id, "actor-3");
    assert!(!message.author_is_bot);
    assert_eq!(message.role_ids, roles);
    assert_eq!(message.message_id, preview_message_id(99));
    assert_eq!(message.message_id, "preview:99");
    assert_eq!(message.content, "hello");
    assert!(message.mentioned_user_ids.is_empty());
    assert_eq!(message.attachment_names, vec!["a.txt".to_owned()]);
    assert_eq!(message.observed_timestamp_ms, 7_000);
    // A comma-bearing name is refused at the builder even if it bypassed
    // option decode (defense in depth: no separator reaches the matcher).
    let smuggled = two_bot_core::automod_preview_input::DecodedPreviewInput {
        text: "hello".to_owned(),
        text_len_utf16: 5,
        mention_count: 0,
        attachment_names: vec!["safe.exe,txt".to_owned()],
    };
    assert_eq!(
        build_preview_message(&smuggled, "g", "c", "a", &[], 1, 1_000),
        Err(PreviewInputRefusal::BadFilename)
    );
}

#[test]
fn gate_matrix_admits_only_the_scoped_guild_when_enabled() {
    let scoped = |enabled: bool| {
        AutomodRuntime::new(
            AutomodConfig {
                enabled,
                dry_run: true,
                policy: AutomodPolicy::default(),
            },
            AutomodScope {
                guild_id: STAGING_GUILD_ID.to_owned(),
                live_approved: false,
            },
        )
    };
    assert!(scoped(true).admits(Some(STAGING_GUILD_ID)));
    assert!(!scoped(false).admits(Some(STAGING_GUILD_ID)));
    assert!(!scoped(true).admits(Some("999999999999999999")));
    assert!(!scoped(true).admits(None));
}

#[test]
fn mode_changes_the_sentence_not_the_filter() {
    let verdict_for = |dry_run: bool| {
        let config = AutomodConfig {
            enabled: true,
            dry_run,
            policy: AutomodPolicy {
                bad_words: vec!["bannedword".to_owned()],
                ..AutomodPolicy::default()
            },
        };
        let message = two_bot_core::automod::AutomodMessage {
            guild_id: "g".to_owned(),
            channel_id: "c".to_owned(),
            message_id: "preview:5".to_owned(),
            author_id: "a".to_owned(),
            author_is_bot: false,
            role_ids: vec![],
            content: "has bannedword here".to_owned(),
            mentioned_user_ids: vec![],
            attachment_names: vec![],
            observed_timestamp_ms: 1,
        };
        preview_automod(&message, 0, &config, &RepeatTracker::default())
    };
    let dry = verdict_for(true);
    let enforced = verdict_for(false);
    assert_eq!(dry.filter, Some(AutomodFilter::BadWords));
    assert_eq!(dry.filter, enforced.filter);
    assert!(dry.dry_run);
    assert!(!enforced.dry_run);
}
