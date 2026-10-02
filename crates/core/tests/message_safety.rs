use two_bot_core::{
    audit::{format_audit_event, has_audit_event_identity, AuditEvent, AuditKind},
    lfg::{lfg_content, LfgPost, LfgRole, LfgStatus},
    message_safety::{contains_mass_mention, content, text_len, CONTENT_LIMIT},
};

#[test]
fn lfg_title_and_role_labels_are_safe_and_bounded_after_rendering() {
    let post = LfgPost {
        id: "fixture".to_owned(),
        guild_id: "1".to_owned(),
        channel_id: "2".to_owned(),
        message_id: None,
        title: "@everyone @here <@&123>".to_owned(),
        starts_at: "2026-10-01T18:00:00Z".to_owned(),
        status: LfgStatus::Open,
        created_by: "3".to_owned(),
        created_at: "2026-09-30T00:00:00Z".to_owned(),
        closed_at: None,
    };
    let roles = vec![LfgRole {
        lfg_id: post.id.clone(),
        role_key: "dps".to_owned(),
        label: "@everyone @here <@&123>".to_owned(),
        slots: 1,
        position: 0,
    }];
    let safe = lfg_content(&post, &roles, &[]);
    assert!(!safe.contains("@everyone"));
    assert!(!safe.contains("@here"));
    assert!(safe.contains("<@&123>"));
    let long = LfgPost {
        title: "😀".repeat(1001),
        ..post
    };
    assert!(text_len(&lfg_content(&long, &roles, &[])) <= CONTENT_LIMIT);
}

#[test]
fn zero_width_split_mentions_refuse_while_multilingual_text_is_kept() {
    // Refused: raw mass mentions and every zero-width-split obfuscation,
    // including the U+2060 word-joiner automod also treats as a gap.
    for separator in ['\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{feff}'] {
        for mention in ["everyone", "here"] {
            let disguised = format!(
                "@{separator}{}",
                mention
                    .chars()
                    .map(|ch| format!("{ch}{separator}"))
                    .collect::<String>()
            );
            assert!(
                contains_mass_mention(&disguised),
                "{disguised:?} must refuse"
            );
            let safe = content(&disguised);
            assert!(!safe.contains("@everyone"));
            assert!(!safe.contains("@here"));
            assert!(!contains_mass_mention(&safe));
        }
    }
    // Preserved: Persian ZWNJ, ZWJ emoji, ZWSP line-breaks and word joiners
    // are legitimate text, never a refusal.
    for text in [
        "می\u{200c}روم",
        "👩\u{200d}💻",
        "line\u{200b}break",
        "\u{feff}text",
        "word\u{2060}joiner",
        "@user\u{200c}name",
    ] {
        assert_eq!(content(text), text);
        assert!(!contains_mass_mention(text), "{text:?} must be kept");
    }
}

#[test]
fn audit_metadata_bounds_include_neutralization_and_keep_identity() {
    let mut event = AuditEvent::new(
        "fixture".to_owned(),
        AuditKind::MessageEdit,
        "1".to_owned(),
        "2026-09-30T00:00:00Z".to_owned(),
    );
    event.metadata_json = serde_json::json!({
        "value": "@everyone".repeat(40),
        "roles": ["@here", "<@&123>"],
    })
    .to_string();
    let safe = format_audit_event(&event);
    assert!(has_audit_event_identity(&safe, "fixture"));
    assert!(!safe.contains("@everyone"));
    assert!(!safe.contains("@here"));
    let scalar = safe
        .split("value=`")
        .nth(1)
        .unwrap()
        .split('`')
        .next()
        .unwrap();
    assert_eq!(text_len(scalar), 300);
    event.action = Some("😀".repeat(1500));
    let safe = format_audit_event(&event);
    assert!(has_audit_event_identity(&safe, "fixture"));
    assert!(text_len(&safe) <= CONTENT_LIMIT);
    assert!(safe.ends_with("..."));
}
