//! Slice 3c: strict option decode, synthetic message build, and the scope
//! gate (spec §§3.1, 5; tests T7 gate half and T11). Offline only: no Discord,
//! no database, no clock beyond explicit receipt timestamps.

use two_bot_core::automod::{AutomodConfig, AutomodFilter, AutomodPolicy, RepeatTracker};
use two_bot_core::automod_preview::{preview_automod, PreviewResult};
use two_bot_core::automod_preview_input::{
    build_preview_message, decode_preview_input, preview_message_id, DecodedPreviewInput,
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

fn decode_attachments(value: &str) -> Result<DecodedPreviewInput, PreviewInputRefusal> {
    decode_preview_input(&[text("hi"), ("attachments", PreviewOptionValue::Text(value))])
}

#[test]
fn attachments_hold_count_total_and_name_shape() {
    let ten: Vec<String> = (0..10).map(|i| format!("f{i}.txt")).collect();
    let input = decode_attachments(&ten.join(", ")).expect("10 attachments decode");
    assert_eq!(input.attachment_names, ten);
    let eleven: Vec<String> = (0..11).map(|i| format!("f{i}.txt")).collect();
    assert_eq!(
        decode_attachments(&eleven.join(", ")),
        Err(PreviewInputRefusal::TooManyAttachments)
    );
    assert_eq!(
        decode_attachments(&"a".repeat(1001)),
        Err(PreviewInputRefusal::AttachmentsBounds)
    );
    for bad in ["", ", a.txt", "a.txt, ", "a.txt, , b.png"] {
        assert_eq!(
            decode_attachments(bad),
            Err(PreviewInputRefusal::EmptyFilename),
            "empty entries must refuse the whole option"
        );
    }
    for bad in [
        "safe.exe,txt",
        "a.txt,b.png",
        "a.txt,,b.txt",
        ",a.txt",
        "a.txt,",
        "a.txt,\tb.png",
        "a.txt,\u{a0}b.png",
        "a/b.txt",
        "a\\b.txt",
        "a\x01b.txt",
        "a\nb.txt",
        "a\u{7f}b.txt",
        "a\u{85}b.txt",
        "a.txt, b,png",
        "a.txt, b.png,txt",
    ] {
        assert_eq!(
            decode_attachments(bad),
            Err(PreviewInputRefusal::BadFilename),
            "invalid delimiters and names must refuse the whole option"
        );
    }
    assert!(decode_attachments(&"b".repeat(255)).is_ok());
    assert_eq!(
        decode_attachments(&"b".repeat(256)),
        Err(PreviewInputRefusal::BadFilename)
    );
}

#[test]
fn canonical_attachment_delimiter_preserves_all_other_characters() {
    let fixtures: &[(&str, &[&str])] = &[
        ("safe.exe, txt", &["safe.exe", "txt"]),
        ("exe", &["exe"]),
        ("txt", &["txt"]),
        ("archive.tar.GZ", &["archive.tar.GZ"]),
        ("EXE", &["EXE"]),
        ("a.txt,  b.png", &["a.txt", " b.png"]),
        ("a b.txt", &["a b.txt"]),
        (" a.txt , b.png ", &[" a.txt ", "b.png "]),
        ("a.txt, \u{a0}b.png", &["a.txt", "\u{a0}b.png"]),
    ];
    for (raw, expected) in fixtures {
        let decoded = decode_attachments(raw).expect("canonical option decodes");
        assert_eq!(&decoded.attachment_names, expected);
    }
    assert!(decode_preview_input(&[text("hi")])
        .expect("omission decodes")
        .attachment_names
        .is_empty());
}

#[test]
fn attachment_utf16_bounds_include_astral_names_and_delimiters() {
    let name255 = format!("{}a", "😀".repeat(127));
    assert_eq!(name255.encode_utf16().count(), 255);
    assert_eq!(
        decode_attachments(&name255)
            .expect("255 units")
            .attachment_names
            .as_slice(),
        std::slice::from_ref(&name255)
    );
    assert_eq!(
        decode_attachments(&format!("{name255}a")),
        Err(PreviewInputRefusal::BadFilename)
    );
    let names = [name255.clone(), name255.clone(), name255, "b".repeat(229)];
    let raw1000 = names.join(", ");
    assert_eq!(raw1000.encode_utf16().count(), 1000);
    assert_eq!(
        decode_attachments(&raw1000)
            .expect("1000 raw units")
            .attachment_names,
        names
    );
    let raw1001 = format!("{raw1000}b");
    assert_eq!(raw1001.encode_utf16().count(), 1001);
    assert_eq!(
        decode_attachments(&raw1001),
        Err(PreviewInputRefusal::AttachmentsBounds)
    );
}

#[test]
fn malformed_attachments_refuse_before_boundary_dependencies() {
    // Offline handler-boundary harness; runtime wiring lands in a later slice.
    let mut calls = (0, 0, 0);
    let boundary =
        |raw: &str, calls: &mut (usize, usize, usize)| -> Result<(), PreviewInputRefusal> {
            let input = decode_attachments(raw)?;
            calls.0 += 1;
            let config = AutomodConfig {
                enabled: true,
                dry_run: true,
                policy: AutomodPolicy::default(),
            };
            calls.1 += 1;
            let message = build_preview_message(&input, "g", "c", "a", &[], 1, 1000)?;
            calls.2 += 1;
            let verdict = preview_automod(&message, 0, &config, &RepeatTracker::default());
            assert_eq!(verdict.result, PreviewResult::Matched);
            Ok(())
        };
    assert_eq!(
        boundary("safe.exe,txt", &mut calls),
        Err(PreviewInputRefusal::BadFilename)
    );
    assert_eq!(calls, (0, 0, 0));
    // Positive control proves that all three counters are connected.
    assert!(boundary("safe.exe, txt", &mut calls).is_ok());
    assert_eq!(calls, (1, 1, 1));
}

#[test]
fn decoded_debug_redacts_content_including_nested_and_alternate_output() {
    let input = decode_preview_input(&[
        text("PRIVATE_PREVIEW_SENTINEL"),
        (
            "attachments",
            PreviewOptionValue::Text("PRIVATE_ATTACHMENT_SENTINEL.exe, txt"),
        ),
    ])
    .expect("sentinels decode");
    for output in [
        format!("{input:?}"),
        format!("{input:#?}"),
        format!("{:?}", Ok::<_, PreviewInputRefusal>(&input)),
        format!("{:#?}", vec![&input]),
    ] {
        assert!(!output.contains("PRIVATE_PREVIEW_SENTINEL"));
        assert!(!output.contains("PRIVATE_ATTACHMENT_SENTINEL"));
        assert!(output.contains("[REDACTED]"));
    }
    // Exact output excludes excerpts, transformed content and content hashes.
    assert_eq!(format!("{input:?}"), "DecodedPreviewInput { text: [REDACTED], text_len_utf16: 24, mention_count: 0, attachment_names: [REDACTED] }");
    let mut same_shape = input.clone();
    same_shape.text = "DIFFERENT_SECRET_CONTENT".to_owned();
    same_shape.attachment_names = vec!["different.exe".to_owned()];
    assert_eq!(format!("{input:?}"), format!("{same_shape:?}"));
    assert_eq!(format!("{input:#?}"), format!("{same_shape:#?}"));
}

#[test]
fn raw_option_debug_redacts_text_before_decode_and_refusal() {
    for raw in ["PRIVATE_RAW_TEXT_SENTINEL", "PRIVATE_RAW_FILE.exe,txt"] {
        let options = [text(raw), ("attachments", PreviewOptionValue::Text(raw))];
        for output in [
            format!("{:?}", options[0].1),
            format!("{:#?}", options[1].1),
            format!("{options:?}"),
            format!("{options:#?}"),
        ] {
            assert!(!output.contains(raw));
            assert!(output.contains("[REDACTED]"));
        }
        assert_eq!(format!("{:?}", options[0].1), "Text([REDACTED])");
    }
    assert_eq!(
        format!("{:?}", PreviewOptionValue::Integer(7)),
        "Integer(7)"
    );
    assert_eq!(format!("{:?}", PreviewOptionValue::Other), "Other");
    assert!(decode_attachments("PRIVATE_RAW_FILE.exe,txt").is_err());
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
fn decoded_filenames_match_live_extension_semantics_unchanged() {
    for (raw, expected, blocked) in [
        ("exe", vec!["exe"], "exe"),
        ("txt", vec!["txt"], "txt"),
        ("safe.exe, txt", vec!["safe.exe", "txt"], "txt"),
        ("archive.tar.GZ", vec!["archive.tar.GZ"], "gz"),
        ("EXE", vec!["EXE"], "exe"),
    ] {
        let input = decode_attachments(raw).expect("valid names decode");
        assert_eq!(input.attachment_names, expected);
        let message =
            build_preview_message(&input, "g", "c", "a", &[], 1, 1000).expect("message builds");
        assert_eq!(message.attachment_names, expected);
        for (extension, matched) in [(blocked, true), ("unrelated", false)] {
            let config = AutomodConfig {
                enabled: true,
                dry_run: true,
                policy: AutomodPolicy {
                    blocked_attachment_extensions: vec![extension.to_owned()],
                    ..AutomodPolicy::default()
                },
            };
            let live = two_bot_core::automod::match_automod(
                &message,
                &config.policy,
                &mut RepeatTracker::default(),
            );
            let preview = preview_automod(&message, 0, &config, &RepeatTracker::default());
            assert_eq!(live, matched.then_some(AutomodFilter::AttachmentType));
            assert_eq!(preview.filter, live);
            assert_eq!(
                preview.result,
                if matched {
                    PreviewResult::Matched
                } else {
                    PreviewResult::NoMatch
                }
            );
        }
    }
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
