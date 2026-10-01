//! Offline legacy golden corpus. Deferred service/wiring expectations are data,
//! not successful parity checks: the core has no Discord service to execute them.
use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use two_bot_core::automod::{
    match_automod, normalize_content, sanction_for, AutomodConfig, AutomodMessage, RepeatTracker,
};
use two_bot_core::moderation::{moderation_target_protection, ModerationPolicy, ModerationTarget};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    version: u32,
    legacy_revision: String,
    sources: HashMap<String, Vec<u32>>,
    expanded_checks: usize,
    expanded_site_multiplicities: HashMap<String, HashMap<u32, u32>>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    assertions: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(flatten)]
    check: Check,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Check {
    Match {
        messages: Vec<Message>,
        #[serde(default)]
        raw_bad_words: Option<Vec<String>>,
    },
    Normalize {
        input: String,
        expected: String,
    },
    Config {
        enabled: bool,
        dry_run: bool,
    },
    ConfigError {
        contains: String,
    },
    Policy {
        expected: serde_json::Value,
    },
    Sanction {
        count: u64,
        action: String,
        timeout_seconds: Option<u64>,
    },
    Protection {
        target: Target,
        expected: Option<String>,
    },
    Exempt {
        bot: bool,
        channel: String,
        roles: Vec<String>,
        expected: bool,
    },
    Deferred {
        issue: String,
        reason: String,
        legacy_expected: serde_json::Value,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    #[serde(default)]
    owner: bool,
    #[serde(default)]
    bot: bool,
    #[serde(default)]
    owen: bool,
    #[serde(default)]
    staff: bool,
}

fn corpus() -> Corpus {
    serde_json::from_str(include_str!("fixtures/automod_corpus.json")).expect("valid golden corpus")
}

#[test]
fn legacy_automod_core_decisions() {
    let corpus = corpus();
    let mut executed = 0;
    let mut deferred = 0;
    for case in &corpus.cases {
        if let Check::ConfigError { contains } = &case.check {
            let error = AutomodConfig::from_map(&case.env).expect_err(&case.id);
            assert!(error.to_string().contains(contains), "{}: {error}", case.id);
            executed += 1;
            continue;
        }
        let config = AutomodConfig::from_map(&case.env).expect(&case.id);
        match &case.check {
            Check::ConfigError { .. } => unreachable!("handled above"),
            Check::Policy { expected } => {
                let actual = serde_json::json!({
                    "bad_words": config.policy.bad_words,
                    "blocked_attachment_extensions": config.policy.blocked_attachment_extensions,
                    "allowed_domains": config.policy.allowed_domains,
                    "repeated_message_count": config.policy.repeated_message_count,
                    "repeated_message_window_seconds": config.policy.repeated_message_window_seconds,
                    "mention_limit": config.policy.mention_limit,
                    "sanctions": config.policy.sanctions.iter().map(|s| serde_json::json!({
                        "violations": s.violations,
                        "action": s.action.as_str(),
                        "timeout_seconds": s.timeout_seconds,
                    })).collect::<Vec<_>>(),
                });
                let fields = expected
                    .as_object()
                    .expect("policy expectation is an object");
                assert!(!fields.is_empty(), "{}: empty policy expectation", case.id);
                for (field, value) in fields {
                    assert_eq!(&actual[field], value, "{}: {field}", case.id);
                }
            }
            Check::Match {
                messages,
                raw_bad_words,
            } => {
                assert!(!messages.is_empty(), "{}: empty sequence", case.id);
                let mut policy = config.policy.clone();
                // Direct legacy policies must reach the matcher unchanged.
                if let Some(words) = raw_bad_words {
                    policy.bad_words.clone_from(words);
                }
                let mut repeats = RepeatTracker::default();
                for (index, row) in messages.iter().enumerate() {
                    let message = AutomodMessage {
                        guild_id: row.guild.clone().unwrap_or_else(|| "fixture-guild".into()),
                        channel_id: "fixture-channel".into(),
                        message_id: row.id.clone().unwrap_or_else(|| format!("message-{index}")),
                        author_id: row
                            .author
                            .clone()
                            .unwrap_or_else(|| "fixture-author".into()),
                        author_is_bot: false,
                        role_ids: vec![],
                        content: row.content.clone(),
                        mentioned_user_ids: row.mentions.clone(),
                        attachment_names: row.attachments.clone(),
                        observed_timestamp_ms: row.at_ms.unwrap_or(1_000_000 + index as u64),
                    };
                    let actual = match_automod(&message, &policy, &mut repeats)
                        .map(|filter| filter.as_str().to_owned());
                    assert_eq!(actual, row.expected, "{}: message {index}", case.id);
                }
            }
            Check::Normalize { input, expected } => {
                assert_eq!(&normalize_content(input), expected, "{}", case.id);
            }
            Check::Config { enabled, dry_run } => {
                assert_eq!(config.enabled, *enabled, "{}: enabled", case.id);
                assert_eq!(config.dry_run, *dry_run, "{}: dry_run", case.id);
            }
            Check::Sanction {
                count,
                action,
                timeout_seconds,
            } => {
                let actual = sanction_for(*count, &config.policy.sanctions);
                assert_eq!(actual.action.as_str(), action, "{}: action", case.id);
                assert_eq!(
                    actual.timeout_seconds, *timeout_seconds,
                    "{}: duration",
                    case.id
                );
            }
            Check::Protection { target, expected } => {
                let policy = ModerationPolicy {
                    owen_user_id: "fixture-owen".into(),
                    bot_user_id: None,
                    protected_role_ids: HashSet::from(["fixture-staff".into()]),
                };
                let target = ModerationTarget {
                    user_id: if target.owen {
                        "fixture-owen"
                    } else {
                        "fixture-author"
                    }
                    .into(),
                    role_ids: if target.staff {
                        vec!["fixture-staff".into()]
                    } else {
                        vec![]
                    },
                    highest_role_position: 1,
                    is_bot: target.bot,
                    is_guild_owner: target.owner,
                };
                let actual = moderation_target_protection(&target, &policy)
                    .map(|reason| format!("{reason:?}"));
                assert_eq!(&actual, expected, "{}", case.id);
            }
            Check::Exempt {
                bot,
                channel,
                roles,
                expected,
            } => {
                let message = AutomodMessage {
                    guild_id: "fixture-guild".into(),
                    channel_id: channel.clone(),
                    message_id: "fixture-message".into(),
                    author_id: "fixture-author".into(),
                    author_is_bot: *bot,
                    role_ids: roles.clone(),
                    content: "spamword".into(),
                    mentioned_user_ids: vec![],
                    attachment_names: vec![],
                    observed_timestamp_ms: 1_000_000,
                };
                assert_eq!(config.policy.is_exempt(&message), *expected, "{}", case.id);
            }
            Check::Deferred { .. } => {
                deferred += 1;
                continue;
            }
        }
        executed += 1;
    }
    eprintln!("Automod corpus: {executed} executed core cases; {deferred} deferred service/wiring cases (NOT parity passes)");
    assert!(executed > 0);
}

#[test]
fn every_legacy_assertion_is_mapped_once_or_more() {
    let corpus = corpus();
    assert_eq!(corpus.version, 1);
    assert_eq!(corpus.legacy_revision.len(), 40);
    assert!(corpus
        .legacy_revision
        .bytes()
        .all(|b| b.is_ascii_hexdigit()));
    let mut expected = HashSet::new();
    for (file, lines) in &corpus.sources {
        for line in lines {
            let count = corpus
                .expanded_site_multiplicities
                .get(file)
                .and_then(|counts| counts.get(line))
                .copied()
                .unwrap_or(1);
            assert!(count > 0, "{file}:{line}: zero multiplicity");
            for iteration in 1..=count {
                let reference = if count == 1 {
                    format!("{file}:{line}")
                } else {
                    format!("{file}:{line}#{iteration}")
                };
                assert!(expected.insert(reference), "duplicate source assertion");
            }
        }
    }
    assert_eq!(corpus.sources.values().map(Vec::len).sum::<usize>(), 123);
    assert_eq!(corpus.expanded_checks, 212);
    assert_eq!(expected.len(), corpus.expanded_checks);
    let mut mapped = HashSet::new();
    let mut ids = HashSet::new();
    for case in &corpus.cases {
        assert!(ids.insert(&case.id), "duplicate case {}", case.id);
        // Extra edge cases may have no legacy assertion, but all references
        // must name an assertion in the pinned source inventory.
        for assertion in &case.assertions {
            assert!(
                expected.contains(assertion),
                "{}: unknown {assertion}",
                case.id
            );
            mapped.insert(assertion.clone());
        }
        if let Check::Match {
            messages,
            raw_bad_words,
        } = &case.check
        {
            let empty_word_assertions = [
                "test/unit.automodmatcher.test.ts:76",
                "test/unit.automodmatcher.test.ts:77",
                "test/unit.automodmatcher.test.ts:79",
            ];
            if case
                .assertions
                .iter()
                .any(|reference| empty_word_assertions.contains(&reference.as_str()))
            {
                let words = raw_bad_words.as_ref().expect("literal empty-word policy");
                assert!(
                    words.iter().any(String::is_empty),
                    "{}: no empty word",
                    case.id
                );
                assert!(
                    words
                        .iter()
                        .any(|word| !word.is_empty() && word.trim().is_empty()),
                    "{}: no whitespace-only word",
                    case.id
                );
            }
            if !case.assertions.is_empty() {
                assert_eq!(
                    messages.len(),
                    case.assertions.len(),
                    "{}: assertion/row count",
                    case.id
                );
            }
        }
        if let Check::Deferred {
            issue,
            reason,
            legacy_expected,
        } = &case.check
        {
            assert!(issue.starts_with("https://") || issue.starts_with("/TOG/issues/TOG-"));
            assert!(
                !reason.trim().is_empty(),
                "{}: no divergence explanation",
                case.id
            );
            assert!(
                !legacy_expected.is_null(),
                "{}: missing legacy outcome",
                case.id
            );
            if !case.assertions.is_empty() {
                let checks = legacy_expected["checks"]
                    .as_object()
                    .expect("deferred checks");
                let references: HashSet<_> = case.assertions.iter().map(String::as_str).collect();
                assert_eq!(
                    checks.keys().map(String::as_str).collect::<HashSet<_>>(),
                    references,
                    "{}: every deferred assertion needs its own expected outcome",
                    case.id
                );
            }
        }
    }
    let missing: Vec<_> = expected.difference(&mapped).collect();
    assert!(
        missing.is_empty(),
        "unmapped legacy assertions: {missing:?}"
    );
    eprintln!("Mapped {} pinned legacy assertion sites", mapped.len());
}
