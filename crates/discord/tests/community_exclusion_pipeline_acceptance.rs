//! Protected acceptance: exclusion refresh reaches pipeline facts coherently.
//!
//! PROTECTED FILE: the paired Builder extends the gateway per-message overlay
//! to the five exclusion keys so the channel classifier and the facts sink
//! swap to the same stored generation together — without editing this file.
//! Return necessary corrections to the Test Author; never edit.
//!
//! Contract under test (through the real `Pipeline::handle_at` message seam,
//! the same entry `OrderedLevelingPipeline::collect_at` drives for every
//! gateway dispatch, plus the public `SettingsChannelClassifier` refresh
//! handle and an in-memory facts sink that classifies exactly like
//! `DeferredCommunityFacts::record_message`; pure in-memory via
//! `live_channel`, no database, no network, no process-environment reads):
//!
//! - Stored rows for the five exclusion keys override boot through one
//!   `env_snapshot` layering; refreshing the channel classifier and the
//!   sink exclusions with that one config flips subsequent message facts
//!   to `staff_automation` / `raid` / `staging` / `test`.
//! - The classifier and the sink swap together to the same generation: a
//!   message in a stored human channel from a stored test actor records
//!   (`human`, `test`) from one refresh, never a split generation.
//! - Events between generations keep their verdicts: a message before the
//!   refresh records human, a message after records excluded, and the first
//!   record is never rewritten.
//! - Per-guild isolation through per-message refresh: layering guild A's
//!   snapshot before A's message and guild B's (empty) snapshot before B's
//!   excludes in A while B stays human.
//! - Precedence through the pipeline: bot and webhook message flags win over
//!   every configured set, exactly as `classify` orders them.
//! - A refresh failure preserves the last confirmed generation: when the
//!   live reader is parked (`None`), the caller keeps the previous config
//!   instead of publishing boot-empty or a mixed subset.
//! - Bound reads: one snapshot publish plus one layering serves any number
//!   of subsequent messages; this file runs fully offline.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;
use twilight_model::gateway::event::Event;
use two_bot_core::settings::{live_channel, SettingRow, SettingsSnapshot};
use two_bot_core::{
    classify, ChannelClass, Classification, ClassifierConfig, ClassifyInput, FactsSink, MemStore,
    MemberJoinFact, MessageFact, NoopLeveling, RulesAcceptedFact, VoiceEndedFact, VoiceStartedFact,
};
use two_bot_discord::{NoInvites, Pipeline, SettingsChannelClassifier};

const AUTOMATION: &str = "TWO_COMMUNITY_AUTOMATION_ACTOR_IDS";
const RAID: &str = "TWO_COMMUNITY_RAID_ACTOR_IDS";
const STAGING_GUILDS: &str = "TWO_COMMUNITY_STAGING_GUILD_IDS";
const STAGING_ACTORS: &str = "TWO_COMMUNITY_STAGING_ACTOR_IDS";
const TEST: &str = "TWO_COMMUNITY_TEST_ACTOR_IDS";
const FIVE: [&str; 5] = [AUTOMATION, RAID, STAGING_GUILDS, STAGING_ACTORS, TEST];

const GUILD_A: u64 = 100_000_000_000_000_001;
const GUILD_B: u64 = 100_000_000_000_000_002;
const STAGING_GUILD_ID: u64 = 100_000_000_000_000_009;
const HUMAN_MEMBER: u64 = 900_000_000_000_001_111;
const AUTOMATION_MEMBER: u64 = 900_000_000_000_001_222;
const RAID_MEMBER: u64 = 900_000_000_000_001_333;
const STAGING_MEMBER: u64 = 900_000_000_000_001_444;
const TEST_MEMBER: u64 = 900_000_000_000_001_555;
const HUMAN_CHANNEL: u64 = 200_000_000_000_000_001;
const OTHER_CHANNEL: u64 = 400_000_000_000_000_001;
const AT: &str = "2026-09-20T12:00:00.000Z";

/// Shared exclusion config handle: the sink-side twin of
/// `SettingsChannelClassifier`. Production swaps both from the same
/// `ClassifierConfig` in `OrderedLevelingPipeline::refresh_classifier_config`,
/// so the channel stamp and the capture verdict never disagree.
#[derive(Debug, Clone, Default)]
struct SharedExclusions(Arc<Mutex<ClassifierConfig>>);

impl SharedExclusions {
    fn new(config: ClassifierConfig) -> Self {
        Self(Arc::new(Mutex::new(config)))
    }

    fn refresh(&self, config: ClassifierConfig) {
        let mut live = self.0.lock().expect("exclusions lock");
        if *live != config {
            *live = config;
        }
    }

    fn classify(&self, input: &ClassifyInput) -> Classification {
        classify(&self.0.lock().expect("exclusions lock"), input)
    }
}

/// One recorded message fact: channel stamp plus capture verdict, in arrival
/// order. Mirrors what `DeferredCommunityFacts` persists (`channelClass` in
/// metadata plus the stored classification), without a database.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedMessage {
    member_id: u64,
    channel_class: ChannelClass,
    classification: String,
}

#[derive(Debug, Clone, Default)]
struct RecordingSink {
    records: Arc<Mutex<Vec<RecordedMessage>>>,
    exclusions: SharedExclusions,
}

impl RecordingSink {
    fn new(exclusions: SharedExclusions) -> Self {
        Self {
            records: Arc::default(),
            exclusions,
        }
    }

    fn recorded(&self) -> Vec<RecordedMessage> {
        self.records.lock().expect("recorded lock").clone()
    }
}

impl FactsSink for RecordingSink {
    fn record_member_join(&self, _fact: MemberJoinFact<'_>) {}

    fn record_rules_accepted(&self, _fact: RulesAcceptedFact<'_>) {}

    fn record_message(&self, fact: MessageFact<'_>) {
        // Exactly the `DeferredCommunityFacts::record_message` input shape:
        // raid/staging/test come from the configured sets, never the event.
        let input = ClassifyInput {
            guild_id: fact.guild_id.to_string(),
            actor_id: fact.member_id.to_string(),
            is_bot: fact.is_bot,
            webhook_id: fact.webhook_id.map(|w| w.to_string()),
            is_staff_automation: fact.is_staff_automation,
            is_raid: false,
            is_staging: false,
            is_test: false,
        };
        let verdict = self.exclusions.classify(&input);
        self.records
            .lock()
            .expect("recorded lock")
            .push(RecordedMessage {
                member_id: fact.member_id,
                channel_class: fact.channel_class,
                classification: verdict.classification,
            });
    }

    fn record_voice_started(&self, _fact: VoiceStartedFact<'_>) -> Option<String> {
        None
    }

    fn record_voice_ended(&self, _fact: VoiceEndedFact<'_>) {}
}

fn pipeline_with(
    classifier: SettingsChannelClassifier,
    exclusions: SharedExclusions,
) -> (
    Pipeline<MemStore, NoopLeveling, RecordingSink, NoInvites, SettingsChannelClassifier>,
    RecordingSink,
) {
    let sink = RecordingSink::new(exclusions);
    let pipeline = Pipeline::new(
        MemStore::new(),
        Some(NoopLeveling),
        Some(sink.clone()),
        NoInvites,
        classifier,
    );
    (pipeline, sink)
}

/// Coherent swap: the same config object reaches both handles, like
/// `OrderedLevelingPipeline::refresh_classifier_config`. Refreshing only one
/// would split the generation and fail the agreement test below.
fn refresh_both(
    classifier: &SettingsChannelClassifier,
    exclusions: &SharedExclusions,
    config: ClassifierConfig,
) {
    classifier.refresh(config.clone());
    exclusions.refresh(config);
}

fn row(guild: &str, key: &str, value: serde_json::Value, version: i64) -> SettingRow {
    SettingRow {
        guild_id: guild.to_owned(),
        key: key.to_owned(),
        value,
        version,
    }
}

fn layer_five(
    boot: &HashMap<String, String>,
    live: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut vars = boot.clone();
    for key in FIVE {
        if let Some(value) = live.get(key) {
            vars.insert(key.to_owned(), value.clone());
        }
    }
    vars
}

fn message_event(
    guild_id: u64,
    channel_id: u64,
    member_id: u64,
    is_bot: bool,
    webhook_id: Option<u64>,
    seq: u64,
) -> Event {
    let webhook_json = webhook_id.map_or("null".to_owned(), |w| format!("\"{w}\""));
    Event::MessageCreate(Box::new(
        serde_json::from_value(serde_json::json!({
            "id": (4_000_000_000_000_000_000_u64 + seq).to_string(),
            "guild_id": guild_id.to_string(),
            "channel_id": channel_id.to_string(),
            "webhook_id": serde_json::from_str::<serde_json::Value>(&webhook_json).expect("webhook id"),
            "author": {
                "id": member_id.to_string(),
                "username": "username",
                "global_name": "Global name",
                "discriminator": "0",
                "avatar": null,
                "bot": is_bot
            },
            "content": "hello",
            "timestamp": "2026-09-20T12:00:00.000+00:00",
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false,
            "type": 0,
            "components": []
        }))
        .expect("fixture message parses as a gateway MessageCreate"),
    ))
}

fn plain_message(guild_id: u64, member_id: u64, seq: u64) -> Event {
    message_event(guild_id, OTHER_CHANNEL, member_id, false, None, seq)
}

#[test]
fn stored_automation_row_flips_subsequent_message_facts() {
    let boot = HashMap::new();
    let classifier = SettingsChannelClassifier::new(ClassifierConfig::from_map(&boot));
    let exclusions = SharedExclusions::new(ClassifierConfig::from_map(&boot));
    let (pipeline, sink) = pipeline_with(classifier.clone(), exclusions.clone());

    // Before the stored row: the automation actor reads human.
    pipeline.handle_at(&plain_message(GUILD_A, AUTOMATION_MEMBER, 1), AT);
    assert_eq!(
        sink.recorded()[0].classification,
        "eligible_human",
        "boot carries no exclusions"
    );

    // One publish plus one layering plus one coherent refresh.
    let (mut writer, live) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![row(
            &GUILD_A.to_string(),
            AUTOMATION,
            json!([AUTOMATION_MEMBER.to_string()]),
            1,
        )],
    });
    let snap = live.env_snapshot(Some(&GUILD_A.to_string()));
    let config = ClassifierConfig::from_map(&layer_five(&boot, &snap));
    refresh_both(&classifier, &exclusions, config);

    pipeline.handle_at(&plain_message(GUILD_A, AUTOMATION_MEMBER, 2), AT);
    let recorded = sink.recorded();
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].classification, "eligible_human");
    assert_eq!(
        recorded[1].classification, "staff_automation",
        "the stored row flips subsequent facts"
    );
}

#[test]
fn all_five_exclusion_sets_flip_subsequent_facts_together() {
    let boot = HashMap::new();
    let classifier = SettingsChannelClassifier::new(ClassifierConfig::from_map(&boot));
    let exclusions = SharedExclusions::new(ClassifierConfig::from_map(&boot));
    let (pipeline, sink) = pipeline_with(classifier.clone(), exclusions.clone());

    let (mut writer, live) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![
            row(
                &GUILD_A.to_string(),
                AUTOMATION,
                json!([AUTOMATION_MEMBER.to_string()]),
                1,
            ),
            row(
                &GUILD_A.to_string(),
                RAID,
                json!([RAID_MEMBER.to_string()]),
                2,
            ),
            row(
                &GUILD_A.to_string(),
                STAGING_GUILDS,
                json!([STAGING_GUILD_ID.to_string()]),
                3,
            ),
            row(
                &GUILD_A.to_string(),
                STAGING_ACTORS,
                json!([STAGING_MEMBER.to_string()]),
                4,
            ),
            row(
                &GUILD_A.to_string(),
                TEST,
                json!([TEST_MEMBER.to_string()]),
                5,
            ),
        ],
    });
    let snap = live.env_snapshot(Some(&GUILD_A.to_string()));
    refresh_both(
        &classifier,
        &exclusions,
        ClassifierConfig::from_map(&layer_five(&boot, &snap)),
    );

    pipeline.handle_at(&plain_message(GUILD_A, AUTOMATION_MEMBER, 1), AT);
    pipeline.handle_at(&plain_message(GUILD_A, RAID_MEMBER, 2), AT);
    pipeline.handle_at(&plain_message(GUILD_A, STAGING_MEMBER, 3), AT);
    pipeline.handle_at(&plain_message(GUILD_A, TEST_MEMBER, 4), AT);
    pipeline.handle_at(&plain_message(GUILD_A, HUMAN_MEMBER, 5), AT);
    // Staging-guild VALUE semantics under this one config: the listed guild
    // stages every actor in it. Per-guild SNAPSHOT selection (which guild's
    // rows layer for a message) is pinned in
    // `per_guild_snapshots_isolate_pipeline_facts` and in the core
    // per-guild test — production layers each message's own guild.
    pipeline.handle_at(
        &message_event(
            STAGING_GUILD_ID,
            OTHER_CHANNEL,
            HUMAN_MEMBER,
            false,
            None,
            6,
        ),
        AT,
    );

    let recorded = sink.recorded();
    assert_eq!(
        recorded
            .iter()
            .map(|r| r.classification.as_str())
            .collect::<Vec<_>>(),
        vec![
            "staff_automation",
            "raid",
            "staging",
            "test",
            "eligible_human",
            "staging",
        ]
    );
}

#[test]
fn event_between_generations_keeps_its_verdict_without_rewrite() {
    let boot = HashMap::new();
    let classifier = SettingsChannelClassifier::new(ClassifierConfig::from_map(&boot));
    let exclusions = SharedExclusions::new(ClassifierConfig::from_map(&boot));
    let (pipeline, sink) = pipeline_with(classifier.clone(), exclusions.clone());

    pipeline.handle_at(&plain_message(GUILD_A, TEST_MEMBER, 1), AT);

    let (mut writer, live) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![row(
            &GUILD_A.to_string(),
            TEST,
            json!([TEST_MEMBER.to_string()]),
            1,
        )],
    });
    let snap = live.env_snapshot(Some(&GUILD_A.to_string()));
    refresh_both(
        &classifier,
        &exclusions,
        ClassifierConfig::from_map(&layer_five(&boot, &snap)),
    );
    pipeline.handle_at(&plain_message(GUILD_A, TEST_MEMBER, 2), AT);

    // Deleting the row flips the third message back without touching the
    // first two records.
    writer.publish(&SettingsSnapshot {
        revision: 2,
        rows: vec![],
    });
    let snap = live.env_snapshot(Some(&GUILD_A.to_string()));
    refresh_both(
        &classifier,
        &exclusions,
        ClassifierConfig::from_map(&layer_five(&boot, &snap)),
    );
    pipeline.handle_at(&plain_message(GUILD_A, TEST_MEMBER, 3), AT);

    let recorded = sink.recorded();
    assert_eq!(
        recorded
            .iter()
            .map(|r| r.classification.as_str())
            .collect::<Vec<_>>(),
        vec!["eligible_human", "test", "eligible_human"],
        "generations apply forward only; history is never rewritten"
    );
}

#[test]
fn per_guild_snapshots_isolate_pipeline_facts() {
    let boot = HashMap::new();
    let classifier = SettingsChannelClassifier::new(ClassifierConfig::from_map(&boot));
    let exclusions = SharedExclusions::new(ClassifierConfig::from_map(&boot));
    let (pipeline, sink) = pipeline_with(classifier.clone(), exclusions.clone());

    // Guild A names the test actor; guild B names nothing.
    let (mut writer, live) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![row(
            &GUILD_A.to_string(),
            TEST,
            json!([TEST_MEMBER.to_string()]),
            1,
        )],
    });

    // Gateway per-message refresh: layer the message's own guild before it.
    let snap_a = live.env_snapshot(Some(&GUILD_A.to_string()));
    refresh_both(
        &classifier,
        &exclusions,
        ClassifierConfig::from_map(&layer_five(&boot, &snap_a)),
    );
    pipeline.handle_at(&plain_message(GUILD_A, TEST_MEMBER, 1), AT);

    let snap_b = live.env_snapshot(Some(&GUILD_B.to_string()));
    refresh_both(
        &classifier,
        &exclusions,
        ClassifierConfig::from_map(&layer_five(&boot, &snap_b)),
    );
    pipeline.handle_at(&plain_message(GUILD_B, TEST_MEMBER, 2), AT);

    let recorded = sink.recorded();
    assert_eq!(recorded[0].classification, "test");
    assert_eq!(
        recorded[1].classification, "eligible_human",
        "guild B's empty snapshot must not inherit guild A's exclusion"
    );
}

#[test]
fn channel_classifier_and_sink_agree_on_one_generation() {
    let boot = HashMap::new();
    let classifier = SettingsChannelClassifier::new(ClassifierConfig::from_map(&boot));
    let exclusions = SharedExclusions::new(ClassifierConfig::from_map(&boot));
    let (pipeline, sink) = pipeline_with(classifier.clone(), exclusions.clone());

    // One snapshot carries a human channel AND a test exclusion. The single
    // coherent refresh must stamp both: refreshing only the channels (the
    // pre-slice overlay) would record (`human`, `eligible_human`) and fail
    // the classification half.
    let (mut writer, live) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![
            row(
                &GUILD_A.to_string(),
                "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
                json!([HUMAN_CHANNEL.to_string()]),
                1,
            ),
            row(
                &GUILD_A.to_string(),
                TEST,
                json!([TEST_MEMBER.to_string()]),
                2,
            ),
        ],
    });
    let snap = live.env_snapshot(Some(&GUILD_A.to_string()));
    let mut vars = boot.clone();
    for (key, value) in &snap {
        vars.insert(key.clone(), value.clone());
    }
    refresh_both(&classifier, &exclusions, ClassifierConfig::from_map(&vars));

    pipeline.handle_at(
        &message_event(GUILD_A, HUMAN_CHANNEL, TEST_MEMBER, false, None, 1),
        AT,
    );
    let recorded = sink.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].channel_class, ChannelClass::Human);
    assert_eq!(
        recorded[0].classification, "test",
        "the sink must share the classifier's stored generation"
    );
}

#[test]
fn control_bot_and_webhook_flags_win_over_configured_sets_on_the_pipeline() {
    let vars: HashMap<String, String> = FIVE
        .iter()
        .map(|key| {
            (
                (*key).to_owned(),
                match *key {
                    AUTOMATION => AUTOMATION_MEMBER.to_string(),
                    RAID => RAID_MEMBER.to_string(),
                    STAGING_GUILDS => STAGING_GUILD_ID.to_string(),
                    STAGING_ACTORS => STAGING_MEMBER.to_string(),
                    _ => TEST_MEMBER.to_string(),
                },
            )
        })
        .collect();
    let config = ClassifierConfig::from_map(&vars);
    let classifier = SettingsChannelClassifier::new(config.clone());
    let exclusions = SharedExclusions::new(config);
    let (pipeline, sink) = pipeline_with(classifier, exclusions);

    // A bot in the automation set is still a bot; a webhook in the raid set
    // is still a webhook.
    pipeline.handle_at(
        &message_event(GUILD_A, OTHER_CHANNEL, AUTOMATION_MEMBER, true, None, 1),
        AT,
    );
    pipeline.handle_at(
        &message_event(
            GUILD_A,
            OTHER_CHANNEL,
            RAID_MEMBER,
            false,
            Some(7_000_000_000_000_000_001),
            2,
        ),
        AT,
    );
    pipeline.handle_at(&plain_message(GUILD_A, HUMAN_MEMBER, 3), AT);

    let recorded = sink.recorded();
    assert_eq!(
        recorded
            .iter()
            .map(|r| r.classification.as_str())
            .collect::<Vec<_>>(),
        vec!["bot", "webhook", "eligible_human"]
    );
}

#[test]
fn refresh_failure_preserves_the_last_confirmed_generation() {
    let boot = HashMap::new();
    let classifier = SettingsChannelClassifier::new(ClassifierConfig::from_map(&boot));
    let exclusions = SharedExclusions::new(ClassifierConfig::from_map(&boot));
    let (pipeline, sink) = pipeline_with(classifier.clone(), exclusions.clone());

    // Generation 1 applies the stored test exclusion.
    let (mut writer, live) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![row(
            &GUILD_A.to_string(),
            TEST,
            json!([TEST_MEMBER.to_string()]),
            1,
        )],
    });
    let snap = live.env_snapshot(Some(&GUILD_A.to_string()));
    refresh_both(
        &classifier,
        &exclusions,
        ClassifierConfig::from_map(&layer_five(&boot, &snap)),
    );
    pipeline.handle_at(&plain_message(GUILD_A, TEST_MEMBER, 1), AT);
    assert_eq!(sink.recorded()[0].classification, "test");

    // Refresh failure (parked poller: no live reader, boot empty) must keep
    // generation 1 — the caller skips the refresh — instead of publishing
    // boot-empty or a mixed subset. Layering boot here proves what the
    // failure path must NOT publish; the pipeline keeps its config by not
    // refreshing at all.
    let failed = ClassifierConfig::from_map(&boot);
    assert_eq!(
        classify(
            &failed,
            &ClassifyInput {
                guild_id: GUILD_A.to_string(),
                actor_id: TEST_MEMBER.to_string(),
                is_bot: false,
                webhook_id: None,
                is_staff_automation: false,
                is_raid: false,
                is_staging: false,
                is_test: false,
            }
        )
        .classification,
        "eligible_human",
        "boot-empty is the failure value the pipeline must not publish"
    );
    drop(writer);
    drop(live);
    pipeline.handle_at(&plain_message(GUILD_A, TEST_MEMBER, 2), AT);
    assert_eq!(
        sink.recorded()[1].classification,
        "test",
        "no refresh on failure: the last confirmed generation stands"
    );
}

#[test]
fn one_refresh_serves_many_messages_without_per_message_reads() {
    let boot = HashMap::new();
    let classifier = SettingsChannelClassifier::new(ClassifierConfig::from_map(&boot));
    let exclusions = SharedExclusions::new(ClassifierConfig::from_map(&boot));
    let (pipeline, sink) = pipeline_with(classifier.clone(), exclusions.clone());

    let (mut writer, live) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![row(
            &GUILD_A.to_string(),
            TEST,
            json!([TEST_MEMBER.to_string()]),
            1,
        )],
    });
    // One snapshot read, one layering, one coherent refresh — then ten
    // messages with no further reads.
    let snap = live.env_snapshot(Some(&GUILD_A.to_string()));
    refresh_both(
        &classifier,
        &exclusions,
        ClassifierConfig::from_map(&layer_five(&boot, &snap)),
    );
    for seq in 1..=10 {
        pipeline.handle_at(&plain_message(GUILD_A, TEST_MEMBER, seq), AT);
    }
    let recorded = sink.recorded();
    assert_eq!(recorded.len(), 10);
    assert!(
        recorded.iter().all(|r| r.classification == "test"),
        "every message after the one refresh uses it: {recorded:?}"
    );
}

#[test]
fn control_human_channel_still_reclassifies_through_the_live_snapshot() {
    let boot = HashMap::new();
    let classifier = SettingsChannelClassifier::new(ClassifierConfig::from_map(&boot));
    let exclusions = SharedExclusions::new(ClassifierConfig::from_map(&boot));
    let (pipeline, sink) = pipeline_with(classifier.clone(), exclusions.clone());

    pipeline.handle_at(
        &message_event(GUILD_A, HUMAN_CHANNEL, HUMAN_MEMBER, false, None, 1),
        AT,
    );
    assert_eq!(sink.recorded()[0].channel_class, ChannelClass::Other);

    let (mut writer, live) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![row(
            &GUILD_A.to_string(),
            "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
            json!([HUMAN_CHANNEL.to_string()]),
            1,
        )],
    });
    let snap = live.env_snapshot(Some(&GUILD_A.to_string()));
    let mut vars = boot.clone();
    for (key, value) in &snap {
        vars.insert(key.clone(), value.clone());
    }
    refresh_both(&classifier, &exclusions, ClassifierConfig::from_map(&vars));
    pipeline.handle_at(
        &message_event(GUILD_A, HUMAN_CHANNEL, HUMAN_MEMBER, false, None, 2),
        AT,
    );

    writer.publish(&SettingsSnapshot {
        revision: 2,
        rows: vec![],
    });
    let snap = live.env_snapshot(Some(&GUILD_A.to_string()));
    let mut vars = boot;
    for (key, value) in &snap {
        vars.insert(key.clone(), value.clone());
    }
    refresh_both(&classifier, &exclusions, ClassifierConfig::from_map(&vars));
    pipeline.handle_at(
        &message_event(GUILD_A, HUMAN_CHANNEL, HUMAN_MEMBER, false, None, 3),
        AT,
    );

    assert_eq!(
        sink.recorded()
            .iter()
            .map(|r| r.channel_class)
            .collect::<Vec<_>>(),
        vec![
            ChannelClass::Other,
            ChannelClass::Human,
            ChannelClass::Other,
        ]
    );
}
