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
    },
    Normalize {
        input: String,
        expected: String,
    },
    Config {
        enabled: bool,
        dry_run: bool,
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
        let config = AutomodConfig::from_map(&case.env).expect(&case.id);
        match &case.check {
            Check::Match { messages } => {
                assert!(!messages.is_empty(), "{}: empty sequence", case.id);
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
                    let actual = match_automod(&message, &config.policy, &mut repeats)
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
    let expected: HashSet<String> = corpus
        .sources
        .iter()
        .flat_map(|(file, lines)| lines.iter().map(move |line| format!("{file}:{line}")))
        .collect();
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
        }
    }
    let missing: Vec<_> = expected.difference(&mapped).collect();
    assert!(
        missing.is_empty(),
        "unmapped legacy assertions: {missing:?}"
    );
    eprintln!("Mapped {} pinned legacy assertion sites", mapped.len());
}
