//! Slice-1 acceptance for the read-only automod preview core
//! (docs/automod-preview-spec.md §8): T1–T6 and the T19 struct shape.
//! Audit, rate limiting, and reply mapping land in later slices.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use two_bot_core::automod::{
    match_automod, AutomodConfig, AutomodMessage, AutomodPolicy, RepeatTracker,
};
use two_bot_core::automod_preview::{
    deciding_knobs, exemption_knob, preview_automod, PreviewExemption, PreviewResult,
    PreviewVerdict,
};

fn policy() -> AutomodPolicy {
    AutomodPolicy {
        bad_words: vec!["bannedword".to_owned()],
        allowed_domains: vec!["allowed.example".to_owned()],
        repeated_message_count: 3,
        repeated_message_window_seconds: 30,
        mention_limit: 5,
        ..AutomodPolicy::default()
    }
}

fn config(policy: AutomodPolicy) -> AutomodConfig {
    AutomodConfig {
        enabled: true,
        dry_run: true,
        policy,
    }
}

fn synthetic(content: &str, at_ms: u64) -> AutomodMessage {
    AutomodMessage {
        guild_id: "preview-guild".to_owned(),
        channel_id: "preview-channel".to_owned(),
        message_id: "preview:interaction-1".to_owned(),
        author_id: "preview-author".to_owned(),
        author_is_bot: false,
        role_ids: vec![],
        content: content.to_owned(),
        mentioned_user_ids: vec![],
        attachment_names: vec![],
        observed_timestamp_ms: at_ms,
    }
}

fn live_message(id: &str, content: &str, at_ms: u64) -> AutomodMessage {
    AutomodMessage {
        message_id: id.to_owned(),
        ..synthetic(content, at_ms)
    }
}

#[derive(Deserialize)]
struct Corpus {
    cases: Vec<CorpusCase>,
}

#[derive(Deserialize)]
struct CorpusCase {
    id: String,
    #[serde(default)]
    env: HashMap<String, String>,
    kind: String,
    #[serde(default)]
    messages: Vec<CorpusMessage>,
    #[serde(default)]
    raw_bad_words: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct CorpusMessage {
    content: String,
    expected: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    at_ms: Option<u64>,
    #[serde(default)]
    guild: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    mentions: Vec<String>,
    #[serde(default)]
    attachments: Vec<String>,
}

/// T1: every golden-corpus CREATE verdict replays through the preview.
/// The three edit cases reuse message ids across revisions (not fresh
/// posts); the preview only models "as a new post", so they are out of
/// scope here and covered by T2's duplicate-id histories instead.
#[test]
fn t1_corpus_replay_matches_live() {
    const EDIT_CASES: [&str; 3] = [
        "legacy-M-repeat-edit",
        "legacy-A-repeat-edit",
        "staging-edit-classification",
    ];
    let corpus: Corpus =
        serde_json::from_str(include_str!("fixtures/automod_corpus.json")).expect("corpus");
    let mut checked = 0;
    for case in &corpus.cases {
        if case.kind != "match" || EDIT_CASES.contains(&case.id.as_str()) {
            continue;
        }
        let config = AutomodConfig::from_map(&case.env).expect(&case.id);
        let mut policy = config.policy.clone();
        if let Some(words) = &case.raw_bad_words {
            policy.bad_words.clone_from(words);
        }
        let config = AutomodConfig { policy, ..config };
        let mut live = RepeatTracker::default();
        for (index, row) in case.messages.iter().enumerate() {
            let mut message = live_message(
                &row.id.clone().unwrap_or_else(|| format!("message-{index}")),
                &row.content,
                row.at_ms.unwrap_or(1_000_000 + index as u64),
            );
            if let Some(guild) = &row.guild {
                message.guild_id = guild.clone();
            }
            if let Some(author) = &row.author {
                message.author_id = author.clone();
            }
            message.mentioned_user_ids = row.mentions.clone();
            message.attachment_names = row.attachments.clone();
            let preview_id = format!("preview:{}:{index}", case.id);
            let preview = AutomodMessage {
                message_id: preview_id,
                ..message.clone()
            };
            let preview_mentions = row.mentions.len();
            let verdict = preview_automod(&preview, preview_mentions, &config, &live);
            let actual = match_automod(&message, &config.policy, &mut live)
                .map(|filter| filter.as_str().to_owned());
            assert_eq!(
                verdict.filter.map(|filter| filter.as_str().to_owned()),
                actual,
                "{}: message {index}",
                case.id
            );
            assert_eq!(actual, row.expected, "{}: golden {index}", case.id);
            checked += 1;
        }
    }
    assert!(checked > 100, "corpus replay covered {checked} messages");
}

/// T2: preview == CREATE-on-clone over generated histories, and the live
/// tracker is byte-identical afterwards.
#[test]
fn t2_repeat_equivalence_and_immutability() {
    // Spec §8 T2: every configured repeat count from 2 through 20. Each
    // history seeds `count - 1` identical posts one second apart, so the
    // probe is exactly the streak-completing post; the preview verdict must
    // equal a CREATE evaluated on a cloned tracker, and the live tracker
    // must read identical afterwards.
    for count in 2..=20_u32 {
        let mut count_policy = policy();
        count_policy.repeated_message_count = count;
        let count_config = config(count_policy);
        let mut live = RepeatTracker::default();
        for i in 0..count - 1 {
            let seeded = live_message(
                &format!("range-{count}-{i}"),
                "repeat me",
                1_000_000 + u64::from(i) * 1_000,
            );
            match_automod(&seeded, &count_config.policy, &mut live);
        }
        let at_ms = 1_000_000 + u64::from(count - 1) * 1_000;
        let before = format!("{live:?}");
        let preview = AutomodMessage {
            message_id: format!("preview:range-{count}"),
            ..synthetic("repeat me", at_ms)
        };
        let mut cloned = live.clone();
        let reference = live_message(&format!("range-{count}-live"), "repeat me", at_ms);
        let reference = match_automod(&reference, &count_config.policy, &mut cloned);
        let verdict = preview_automod(&preview, 0, &count_config, &live);
        assert!(reference.is_some(), "count {count}: streak completes");
        assert_eq!(
            verdict.filter, reference,
            "count {count}: preview/live disagree"
        );
        assert_eq!(
            format!("{live:?}"),
            before,
            "count {count}: tracker mutated"
        );
    }
    let config = config(policy());
    let mut live = RepeatTracker::default();
    // Seed history: 20 identical posts, one per second.
    for i in 0..20_u64 {
        let seeded = live_message(&format!("seed-{i}"), "repeat me", 1_000_000 + i * 1_000);
        match_automod(&seeded, &config.policy, &mut live);
    }
    // Window edge: count=3, window=30s, tracker truncated to the 3 newest
    // seeds. Candidate at exactly `window` after the second-newest seed
    // keeps two rows in-window (2 + 1 >= 3); 1 ms later one falls out.
    let probes = [
        (1_000_000 + 18_000 + 30_000, true, "inclusive window edge"),
        (1_000_000 + 18_000 + 30_001, false, "1 ms past the window"),
    ];
    for (at_ms, expected, why) in probes {
        let before = format!("{live:?}");
        let preview = AutomodMessage {
            message_id: "preview:edge".to_owned(),
            ..synthetic("repeat me", at_ms)
        };
        let mut cloned = live.clone();
        let reference = live_message("reference-post", "repeat me", at_ms);
        let reference = match_automod(&reference, &config.policy, &mut cloned);
        let verdict = preview_automod(&preview, 0, &config, &live);
        assert_eq!(
            verdict.filter.is_some(),
            reference.is_some(),
            "{why}: preview/live disagree"
        );
        assert_eq!(verdict.filter.is_some(), expected, "{why}");
        assert_eq!(format!("{live:?}"), before, "{why}: tracker mutated");
    }
    // Out-of-order future rows never leak into an earlier probe, and a
    // duplicate id never double-counts: with two seeded posts and count 3,
    // a fresh id completes the streak but the retained id `dup-1` excludes
    // itself and falls short. Reusing an evicted id (like `seed-5` above,
    // long since truncated to the 3 newest rows) would prove nothing, so
    // the duplicate probe targets a retained row. The future row stays
    // inside the 30 s window (8 ms after the probe) so seeding it does not
    // expire the dup history; it is still excluded from the earlier probe
    // by the at_ms <= now filter.
    let mut live = RepeatTracker::default();
    for i in 0..2_u64 {
        let seeded = live_message(&format!("dup-{i}"), "repeat me", 6_000_000 + i);
        match_automod(&seeded, &config.policy, &mut live);
    }
    let future = live_message("future-post", "repeat me", 6_000_010);
    match_automod(&future, &config.policy, &mut live);
    for (id, expected, why) in [
        ("fresh-dup-probe", true, "fresh id completes the streak"),
        ("dup-1", false, "duplicate id excludes its retained row"),
    ] {
        let before = format!("{live:?}");
        let preview = AutomodMessage {
            message_id: id.to_owned(),
            ..synthetic("repeat me", 6_000_002)
        };
        let mut cloned = live.clone();
        let reference = live_message(id, "repeat me", 6_000_002);
        let reference = match_automod(&reference, &config.policy, &mut cloned);
        let verdict = preview_automod(&preview, 0, &config, &live);
        assert_eq!(verdict.filter, reference, "{why}: preview/live disagree");
        assert_eq!(verdict.filter.is_some(), expected, "{why}");
        assert_eq!(format!("{live:?}"), before, "{why}: tracker mutated");
    }
}

/// T3: N previews of identical text, then real CREATEs — the streak counts
/// only the real posts.
#[test]
fn t3_preview_storm_leaves_no_streak() {
    let config = config(policy());
    let live = RepeatTracker::default();
    for i in 0..5 {
        let preview = AutomodMessage {
            message_id: format!("preview:storm-{i}"),
            ..synthetic("storm text", 2_000_000)
        };
        let verdict = preview_automod(&preview, 0, &config, &live);
        assert_eq!(verdict.result, PreviewResult::NoMatch, "storm preview {i}");
    }
    let mut live = live;
    for i in 0..2 {
        let post = live_message(&format!("real-{i}"), "storm text", 2_000_001 + i);
        assert_eq!(
            match_automod(&post, &config.policy, &mut live),
            None,
            "only {i} real posts so far"
        );
    }
    let third = live_message("real-2", "storm text", 2_000_003);
    assert!(
        match_automod(&third, &config.policy, &mut live).is_some(),
        "third real post completes the streak of 3"
    );
}

/// T4: first match only — bad_words hides repeats/invites, mention_spam
/// hides invites.
#[test]
fn t4_order_masking() {
    let full_config = config(policy());
    // `match_automod` reports `bad_words` before recording a repeat row,
    // so seeding through the full policy would build no streak and the
    // masking claim below would be vacuous. Seed with bad-word matching
    // disabled to lay down a real repeat streak instead.
    let mut seed_policy = policy();
    seed_policy.bad_words.clear();
    let mut live = RepeatTracker::default();
    for i in 0..5 {
        let seeded = live_message(
            &format!("mask-{i}"),
            "bannedword plus discord.gg/abc",
            3_000_000 + i,
        );
        match_automod(&seeded, &seed_policy, &mut live);
    }
    let probe = AutomodMessage {
        message_id: "preview:mask".to_owned(),
        ..synthetic("bannedword plus discord.gg/abc", 3_000_010)
    };
    // Control: without bad-word matching the same probe completes the
    // seeded streak, so there is a real repeat (plus the invite) to mask.
    let seed_config = config(seed_policy);
    let control = preview_automod(&probe, 0, &seed_config, &live);
    assert_eq!(
        control.filter.map(|f| f.as_str()),
        Some("repeated_message"),
        "control: seeded streak is a live repeat"
    );
    let verdict = preview_automod(&probe, 0, &full_config, &live);
    assert_eq!(
        verdict.filter.map(|f| f.as_str()),
        Some("bad_words"),
        "bad_words masks repeat and invite"
    );
    let verdict = preview_automod(
        &synthetic("join discord.gg/abc", 3_000_011),
        5,
        &full_config,
        &live,
    );
    assert_eq!(
        verdict.filter.map(|f| f.as_str()),
        Some("mention_spam"),
        "mention_spam at the limit masks invite"
    );
}

/// T5: another author's real streak is invisible to the invoker's preview.
#[test]
fn t5_cross_author_isolation() {
    let config = config(policy());
    let mut live = RepeatTracker::default();
    for i in 0..5 {
        let other = AutomodMessage {
            author_id: "other-author".to_owned(),
            ..live_message(&format!("other-{i}"), "shared text", 4_000_000 + i)
        };
        match_automod(&other, &config.policy, &mut live);
    }
    let verdict = preview_automod(&synthetic("shared text", 4_000_010), 0, &config, &live);
    assert_eq!(verdict.result, PreviewResult::NoMatch);
}

/// T6: exemptions short-circuit the matcher; channel wins over role.
#[test]
fn t6_exemptions() {
    let mut base = policy();
    base.exempt_channel_ids = HashSet::from(["exempt-channel".to_owned()]);
    base.bypass_role_ids = HashSet::from(["bypass-role".to_owned()]);
    let config = config(base);
    let live = RepeatTracker::default();
    // Bypass role alone.
    let verdict = preview_automod(
        &AutomodMessage {
            role_ids: vec!["bypass-role".to_owned()],
            ..synthetic("bannedword", 5_000_000)
        },
        0,
        &config,
        &live,
    );
    assert_eq!(verdict.result, PreviewResult::Exempt);
    assert_eq!(verdict.exemption, Some(PreviewExemption::BypassRole));
    assert_eq!(verdict.filter, None);
    assert_eq!(verdict.knobs, ["TWO_AUTOMOD_BYPASS_ROLE_IDS"]);
    // Exempt channel alone, and channel wins when both apply.
    for (roles, why) in [
        (vec![], "channel alone"),
        (vec!["bypass-role".to_owned()], "channel wins over role"),
    ] {
        let verdict = preview_automod(
            &AutomodMessage {
                channel_id: "exempt-channel".to_owned(),
                role_ids: roles,
                ..synthetic("bannedword", 5_000_001)
            },
            0,
            &config,
            &live,
        );
        assert_eq!(verdict.result, PreviewResult::Exempt, "{why:?}");
        assert_eq!(verdict.exemption, Some(PreviewExemption::ExemptChannel));
        assert_eq!(verdict.knobs, ["TWO_AUTOMOD_EXEMPT_CHANNEL_IDS"]);
    }
    assert_eq!(
        exemption_knob(PreviewExemption::ExemptChannel),
        "TWO_AUTOMOD_EXEMPT_CHANNEL_IDS"
    );
    assert_eq!(
        exemption_knob(PreviewExemption::BypassRole),
        "TWO_AUTOMOD_BYPASS_ROLE_IDS"
    );
}

/// T9/T19: verdict field inventory — names only, and the partial fallback
/// carries no filter or knob.
#[test]
fn t9_verdict_and_partial_shapes() {
    let config = config(policy());
    let live = RepeatTracker::default();
    let verdict = preview_automod(&synthetic("hello world", 6_000_000), 0, &config, &live);
    assert_eq!(verdict.result, PreviewResult::NoMatch);
    assert_eq!(verdict.filter, None);
    assert!(verdict.knobs.is_empty());
    let partial = PreviewVerdict::partial(true);
    assert_eq!(partial.filter, None);
    assert!(partial.knobs.is_empty());
    assert_eq!(partial.exemption, None);
    for filter in [
        two_bot_core::automod::AutomodFilter::BadWords,
        two_bot_core::automod::AutomodFilter::RepeatedMessage,
        two_bot_core::automod::AutomodFilter::MentionSpam,
        two_bot_core::automod::AutomodFilter::InviteLink,
        two_bot_core::automod::AutomodFilter::ExternalLink,
        two_bot_core::automod::AutomodFilter::AttachmentType,
    ] {
        let rendered = format!("{filter:?} => {:?}", deciding_knobs(filter));
        assert!(!rendered.contains("bannedword"), "{rendered}");
    }
}
