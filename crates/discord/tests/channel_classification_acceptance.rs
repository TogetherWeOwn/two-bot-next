//! Acceptance tests: settings-backed human/welcome channel classification
//! on the gateway path.
//!
//! Protected test file: the build card implements against these tests and
//! must not edit them. If a pinned name below cannot be implemented as
//! written, stop and escalate instead of changing the tests.
//!
//! The pure settings parsing contract lives in
//! `crates/core/tests/channel_classification_acceptance.rs`. This file
//! pins the gateway half: a message driven through the pipeline (the same
//! `Pipeline::handle_at` entry `OrderedLevelingPipeline::collect_at`
//! drives for every gateway dispatch; there is no other message-received
//! entry point in this crate) records the configured channel class on its
//! message fact, and a later settings refresh reclassifies without
//! rebuilding the pipeline.
//!
//! Contract pinned here (build card implements exactly this):
//! - New public type `two_bot_discord::SettingsChannelClassifier`,
//!   re-exported at the crate root like `NoClassification`, implementing
//!   `ChannelClassifier` on top of `ClassifierConfig::channel_class`.
//! - `SettingsChannelClassifier::new(config: ClassifierConfig) -> Self`.
//! - The classifier is a shared handle: it is `Clone`, every clone reads
//!   the same live config, and `refresh(&self, config: ClassifierConfig)`
//!   replaces that config for all clones (interior mutability, the
//!   `DeferredCommunityFacts` `Arc<Mutex<..>>` pattern). Refreshing must
//!   not require reconstructing the pipeline that already holds a clone:
//!   that is what "reclassifies without a restart" means at this seam.
//!   Production refreshes it wherever `ClassifierConfig` is (re)loaded for
//!   fact capture today, so the classifier and the sink never disagree.
//! - The production pipeline (`OrderedLevelingPipeline` in
//!   `crates/discord/src/leveling_runtime.rs`) constructs its `Pipeline`
//!   with a `SettingsChannelClassifier` seeded from `ClassifierConfig`
//!   instead of `NoClassification`. The name `NoClassification` disappears
//!   from that file entirely (imports, generics, constructors, comments):
//!   nothing in the production path may keep stamping `other`.
//! - `NoClassification` itself stays public for other callers and tests;
//!   only the production wiring changes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use twilight_model::gateway::event::Event;
use two_bot_core::{
    ChannelClass, ClassifierConfig, FactsSink, MemStore, MemberJoinFact, MessageFact, NoopLeveling,
    RulesAcceptedFact, VoiceEndedFact, VoiceStartedFact,
};
use two_bot_discord::{ChannelClassifier, NoInvites, Pipeline, SettingsChannelClassifier};

const GUILD: u64 = 100_000_000_000_000_001;
const MEMBER: u64 = 900_000_000_000_001_111;
const HUMAN_CHANNEL: u64 = 200_000_000_000_000_001;
const WELCOME_CHANNEL: u64 = 300_000_000_000_000_001;
const UNLISTED_CHANNEL: u64 = 400_000_000_000_000_001;
const AT: &str = "2026-09-20T12:00:00.000Z";

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn config(pairs: &[(&str, &str)]) -> ClassifierConfig {
    ClassifierConfig::from_map(&vars(pairs))
}

fn configured_classifier() -> SettingsChannelClassifier {
    SettingsChannelClassifier::new(config(&[
        ("TWO_COMMUNITY_HUMAN_CHANNEL_IDS", "200000000000000001"),
        ("TWO_COMMUNITY_WELCOME_CHANNEL_IDS", "300000000000000001"),
    ]))
}

/// Facts sink that keeps every message fact's channel and class, in
/// arrival order. Everything else is ignored: classification is the only
/// behaviour under test here.
#[derive(Debug, Clone, Default)]
struct RecordingSink(Arc<Mutex<Vec<(u64, ChannelClass)>>>);

impl RecordingSink {
    fn recorded(&self) -> Vec<(u64, ChannelClass)> {
        self.0.lock().expect("recorded facts lock").clone()
    }
}

impl FactsSink for RecordingSink {
    fn record_member_join(&self, _fact: MemberJoinFact<'_>) {}

    fn record_rules_accepted(&self, _fact: RulesAcceptedFact<'_>) {}

    fn record_message(&self, fact: MessageFact<'_>) {
        self.0
            .lock()
            .expect("recorded facts lock")
            .push((fact.channel_id, fact.channel_class));
    }

    fn record_voice_started(&self, _fact: VoiceStartedFact<'_>) -> Option<String> {
        None
    }

    fn record_voice_ended(&self, _fact: VoiceEndedFact<'_>) {}
}

fn pipeline_with(
    classifier: SettingsChannelClassifier,
) -> (
    Pipeline<MemStore, NoopLeveling, RecordingSink, NoInvites, SettingsChannelClassifier>,
    RecordingSink,
) {
    let sink = RecordingSink::default();
    let pipeline = Pipeline::new(
        MemStore::new(),
        Some(NoopLeveling),
        Some(sink.clone()),
        NoInvites,
        classifier,
    );
    (pipeline, sink)
}

fn message_event(channel_id: u64, seq: u64) -> Event {
    Event::MessageCreate(Box::new(
        serde_json::from_value(serde_json::json!({
            "id": (4_000_000_000_000_000_000_u64 + seq).to_string(),
            "guild_id": GUILD.to_string(),
            "channel_id": channel_id.to_string(),
            "author": {
                "id": MEMBER.to_string(),
                "username": "username",
                "global_name": "Global name",
                "discriminator": "0",
                "avatar": null,
                "bot": false
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

#[test]
fn settings_classifier_classifies_from_the_config_sets() {
    let classifier = configured_classifier();

    assert_eq!(classifier.classify(HUMAN_CHANNEL), ChannelClass::Human);
    assert_eq!(classifier.classify(WELCOME_CHANNEL), ChannelClass::Welcome);
    assert_eq!(classifier.classify(UNLISTED_CHANNEL), ChannelClass::Other);
}

#[test]
fn gateway_message_in_a_human_channel_records_a_human_fact() {
    let (pipeline, sink) = pipeline_with(configured_classifier());

    pipeline.handle_at(&message_event(HUMAN_CHANNEL, 1), AT);

    let recorded = sink.recorded();
    assert_eq!(
        recorded,
        vec![(HUMAN_CHANNEL, ChannelClass::Human)],
        "one gateway message, one message fact, classified human"
    );
    // This exact string is what the deferred sink stamps into fact
    // metadata (`channelClass`), so the persisted bytes read
    // `"channelClass":"human"` for this message.
    assert_eq!(recorded[0].1.as_str(), "human");
}

#[test]
fn gateway_messages_cover_welcome_wins_and_unlisted_other() {
    // The welcome channel is listed in both sets here: welcome must win
    // on the gateway path too, not only in the pure function.
    let classifier = SettingsChannelClassifier::new(config(&[
        (
            "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
            "200000000000000001,300000000000000001",
        ),
        ("TWO_COMMUNITY_WELCOME_CHANNEL_IDS", "300000000000000001"),
    ]));
    let (pipeline, sink) = pipeline_with(classifier);

    pipeline.handle_at(&message_event(WELCOME_CHANNEL, 1), AT);
    pipeline.handle_at(&message_event(UNLISTED_CHANNEL, 2), AT);

    assert_eq!(
        sink.recorded(),
        vec![
            (WELCOME_CHANNEL, ChannelClass::Welcome),
            (UNLISTED_CHANNEL, ChannelClass::Other),
        ]
    );
}

#[test]
fn malformed_settings_never_reach_gateway_classification() {
    // The human value is junk around one valid id (the welcome channel);
    // the welcome value is junk only. Neither may be fatal, and no
    // malformed entry may classify.
    let classifier = SettingsChannelClassifier::new(config(&[
        (
            "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
            "not-an-id,300000000000000001,123",
        ),
        ("TWO_COMMUNITY_WELCOME_CHANNEL_IDS", "<#300000000000000001>"),
    ]));
    let (pipeline, sink) = pipeline_with(classifier);

    pipeline.handle_at(&message_event(WELCOME_CHANNEL, 1), AT);
    pipeline.handle_at(&message_event(HUMAN_CHANNEL, 2), AT);

    assert_eq!(
        sink.recorded(),
        vec![
            (WELCOME_CHANNEL, ChannelClass::Human),
            (HUMAN_CHANNEL, ChannelClass::Other),
        ],
        "the one valid human id classifies; the mention-form welcome id does not"
    );
}

#[test]
fn settings_change_reclassifies_without_rebuilding_the_pipeline() {
    // Start with empty settings: the human channel records `other`.
    let classifier = SettingsChannelClassifier::new(config(&[]));
    let (pipeline, sink) = pipeline_with(classifier.clone());

    pipeline.handle_at(&message_event(HUMAN_CHANNEL, 1), AT);

    // A settings change lands through the handle the caller kept. The
    // pipeline already holding its own clone must see it immediately:
    // no new pipeline, no restart.
    classifier.refresh(config(&[(
        "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
        "200000000000000001",
    )]));
    pipeline.handle_at(&message_event(HUMAN_CHANNEL, 2), AT);

    // Removing the setting again reclassifies back to `other` the same way.
    classifier.refresh(config(&[]));
    pipeline.handle_at(&message_event(HUMAN_CHANNEL, 3), AT);

    assert_eq!(
        sink.recorded(),
        vec![
            (HUMAN_CHANNEL, ChannelClass::Other),
            (HUMAN_CHANNEL, ChannelClass::Human),
            (HUMAN_CHANNEL, ChannelClass::Other),
        ]
    );
}

#[test]
fn production_pipeline_no_longer_hardcodes_the_noop_classifier() {
    // Wiring guard: the behavioural tests above would also pass if the
    // new classifier existed but production kept the no-op. The
    // production constructor lives in `leveling_runtime.rs`; observing it
    // behaviourally needs the database-gated facts sink, so this pins the
    // wiring at the source instead (docs-guard tests in this workspace
    // read sources the same way).
    let production = include_str!("../src/leveling_runtime.rs");

    assert!(
        production.contains("SettingsChannelClassifier"),
        "OrderedLevelingPipeline must be built with the settings-backed classifier"
    );
    assert!(
        !production.contains("NoClassification"),
        "no production constructor in leveling_runtime.rs may keep the no-op classifier"
    );
}
