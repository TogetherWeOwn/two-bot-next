//! Message-length metadata acceptance (pure, offline).
//!
//! Failing-first contract: message facts must carry a `contentLength`
//! metadata value (Unicode scalar count of the raw content) so the weekly
//! scorecard's copy/paste detector can run on production facts instead of
//! reporting `copy_paste:insufficient_data` every week. Content itself is
//! never stored or threaded: the length is computed once where the gateway
//! message is already in hand, the content is dropped at that same
//! statement, and only the integer crosses into fact metadata.
//!
//! Test files only; no runtime code is touched here. Every test below pins
//! behavior the current runtime does not implement yet, so each one FAILS
//! today (the pinned Builder API does not exist yet) and must turn green
//! under the Builder without changing the test.
//!
//! Pre-step finding (analytics opt-out): community-fact capture already
//! honors analytics opt-out categories before any row lands.
//! `community_store::record_fact` drops a fact when the `(guild, actor)`
//! pair is suppressed, and drops message/voice facts when the guild's
//! analytics family is disabled (`privacy::community_category` maps
//! `message_created` to the activity family), all inside the write
//! transaction. The length metadata therefore needs no new consent surface:
//! it rides the existing gated write path and is covered here only as fact
//! metadata shape, not as a new capture category.
//!
//! ## Pinned Builder API (names and shapes the Builder must implement)
//!
//! ```rust,ignore
//! // crates/core/src/community_store.rs
//! pub struct MessageFactInput<'a> {
//!     pub guild_id: &'a str,
//!     pub message_id: &'a str,
//!     pub channel_id: &'a str,
//!     pub channel_class: &'a str,
//!     pub actor: &'a ClassifyInput,
//!     pub occurred_at: &'a str,
//!     pub content_length: Option<usize>, // NEW
//!     pub classification: Classification,
//! }
//! pub fn message_fact(input: MessageFactInput<'_>) -> FactWrite;
//! ```
//! The input is a struct on purpose: `message_fact` already sits at the
//! clippy argument-count limit, so an eighth positional parameter would
//! force a brand-new checker waiver, which the floor guard forbids.
//! `record_message_fact` forwards the same length into `message_fact`;
//! its own shape is the Builder's (its pre-existing argument-count
//! waiver already covers one more positional parameter, or it can take
//! the same struct).
//! ```rust,ignore
//!
//! // crates/core/src/handlers.rs
//! pub struct MessageInput { /* existing fields, plus */ pub content_length: Option<usize> }
//! pub struct MessageFact<'a> { /* existing fields, plus */ pub content_length: Option<usize> }
//! // `FunnelHandlers::on_message` forwards `i.content_length` into the
//! // `MessageFact` it records. Neither struct ever carries content text.
//! ```
//!
//! Metadata rule: `content_length: Some(n)` with `n > 0` records
//! `"contentLength": n`. `content_length: None` (no length was measured,
//! e.g. the privileged message-content intent is off and no content was
//! delivered) or `Some(0)` records no `contentLength` key at all — the
//! length is never recorded as `0`, so an unknown length can never
//! masquerade as a measured zero-length message in the copy/paste detector.
//! The scalar-vs-byte distinction lives at the capture site and is pinned
//! end to end by the gateway pipeline test in the discord crate's companion
//! suite (`community_message_length_acceptance` there): the integer
//! threaded through must be `content.chars().count()` (Unicode scalar
//! values), never the UTF-8 byte length and never a grapheme-cluster count.
//!
//! Call-site updates the Builder owns (mechanical, not part of this file):
//! every existing `message_fact` / `record_message_fact` caller and every
//! `MessageInput` / `MessageFact` construction gains the new field. That
//! includes the single `Event::MessageCreate` site in the gateway pipeline
//! (`crates/discord/src/pipeline.rs`), which computes the count from the
//! message content already in hand and drops the content at the same
//! statement, and the discord crate's deferred community-facts sink, which
//! forwards `MessageFact.content_length` into `message_fact`. Note one
//! existing assertion the Builder must revise rather than delete: the
//! community-store unit test that proves message facts store no content
//! currently rejects the substring `"content"` in metadata, which the new
//! `contentLength` key necessarily contains; the check must keep proving
//! no message *text* is stored (as pinned by
//! `message_fact_metadata_has_no_content_bearing_field` below) without
//! rejecting the length key itself.
//!
//! Run: `python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test community_message_length_acceptance`
#![cfg(feature = "db")]

use std::sync::Mutex;

use two_bot_core::community::{
    build_scorecard, Classification, ClassifyInput, FactRow, ScorecardInputs, StreamCoverage,
    COMMUNITY_FACT_TYPES,
};
use two_bot_core::community_store::{message_fact, FactWrite, MessageFactInput};
use two_bot_core::{
    ChannelClass, FactsSink, FunnelHandlers, MemStore, MemberJoinFact, MessageFact, MessageInput,
    NoopLeveling, RulesAcceptedFact, VoiceEndedFact, VoiceStartedFact,
};

const GUILD: &str = "1545644954272137297";
const MEMBER: &str = "100000000000000001";
const CHANNEL: &str = "200000000000000001";
const VERSION: &str = "community-test-v1";
const WEEK_START: &str = "2026-08-31T00:00:00.000Z";
const WEEK_END: &str = "2026-09-07T00:00:00.000Z";
const GENERATED_AT: &str = "2026-09-07T06:15:00.000Z";

fn actor(id: &str) -> ClassifyInput {
    ClassifyInput {
        guild_id: GUILD.to_owned(),
        actor_id: id.to_owned(),
        is_bot: false,
        webhook_id: None,
        is_staff_automation: false,
        is_raid: false,
        is_staging: false,
        is_test: false,
    }
}

fn verdict() -> Classification {
    Classification {
        classification: "eligible_human".to_owned(),
        classifier_version: VERSION.to_owned(),
        matched_rule: "fixture".to_owned(),
    }
}

fn write_for(message_id: &str, occurred_at: &str, content_length: Option<usize>) -> FactWrite {
    let who = actor(MEMBER);
    message_fact(MessageFactInput {
        guild_id: GUILD,
        message_id,
        channel_id: CHANNEL,
        channel_class: "human",
        actor: &who,
        occurred_at,
        content_length,
        classification: verdict(),
    })
}

fn metadata_of(write: &FactWrite) -> serde_json::Value {
    serde_json::from_str(write.metadata.as_deref().expect("message metadata"))
        .expect("fact metadata is JSON")
}

#[test]
fn message_fact_records_supplied_content_length() {
    let write = write_for("msg-1", "2026-09-01T10:00:00.000Z", Some(11));
    let meta = metadata_of(&write);
    assert_eq!(
        meta["contentLength"], 11,
        "a measured length lands in metadata as contentLength: {meta}"
    );
    assert!(
        meta["contentLength"].is_number(),
        "contentLength is a JSON number, never a string: {meta}"
    );
}

#[test]
fn message_fact_without_length_has_no_length_key() {
    // No length measured (e.g. the privileged message-content intent is
    // off): the fact carries no `contentLength` key, never a fabricated `0`.
    let write = write_for("msg-none", "2026-09-01T10:00:00.000Z", None);
    let meta = metadata_of(&write);
    assert!(
        meta.get("contentLength").is_none(),
        "unmeasured length records no contentLength key: {meta}"
    );

    // A zero length is also "no measured length": never recorded as `0`.
    let write = write_for("msg-empty", "2026-09-01T10:01:00.000Z", Some(0));
    let meta = metadata_of(&write);
    assert!(
        meta.get("contentLength").is_none(),
        "zero length records no contentLength key, never 0: {meta}"
    );
}

#[test]
fn message_fact_metadata_has_no_content_bearing_field() {
    // The writer only ever receives the integer, so no message text can
    // reach the stored fact. Pin the exact metadata key set: the four
    // pre-existing fields plus the numeric length, nothing else.
    let write = write_for("msg-secret", "2026-09-01T10:00:00.000Z", Some(41));
    let meta = metadata_of(&write);
    let mut keys: Vec<&str> = meta
        .as_object()
        .expect("metadata object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "channelClass",
            "channelId",
            "contentLength",
            "discordBot",
            "webhookId"
        ],
        "metadata carries exactly the known fields plus the length: {meta}"
    );
    assert_eq!(meta["contentLength"], 41);
    // Every string value is an id/class the writer already recorded; none
    // is free text that could carry a content substring.
    assert_eq!(meta["channelId"], CHANNEL);
    assert_eq!(meta["channelClass"], "human");
    // The rest of the write shape is unchanged by the new field.
    assert_eq!(write.source, format!("channel:{CHANNEL}"));
    assert_eq!(write.idempotency_key, "discord-message:msg-secret");
}

/// Map one built fact onto the builder's row shape exactly as the store's
/// scoped load does (same columns, metadata passed through untouched).
fn row_from_write(id: i64, write: &FactWrite) -> FactRow {
    FactRow {
        id,
        guild_id: write.guild_id.clone(),
        event_type: write.event_type.clone(),
        source_event_id: write.source_event_id.clone(),
        actor_id: write.actor_id.clone(),
        occurred_at: write.occurred_at.clone(),
        source: write.source.clone(),
        classifier_version: write.classification.classifier_version.clone(),
        classification: write.classification.classification.clone(),
        matched_rule: write.classification.matched_rule.clone(),
        metadata: write.metadata.clone(),
        idempotency_key: write.idempotency_key.clone(),
    }
}

fn build_json(facts: Vec<FactRow>) -> serde_json::Value {
    let outcome = build_scorecard(ScorecardInputs {
        guild_id: GUILD.to_owned(),
        classifier_version: VERSION.to_owned(),
        week_start: WEEK_START.to_owned(),
        week_end: WEEK_END.to_owned(),
        watermark: facts.iter().map(|f| f.id).max().unwrap_or(0),
        generated_at: GENERATED_AT.to_owned(),
        recommendations_enabled: true,
        correction_cycles: 0,
        facts,
        coverage: COMMUNITY_FACT_TYPES
            .iter()
            .map(|stream| StreamCoverage {
                stream: (*stream).to_owned(),
                covered_from: WEEK_START.to_owned(),
                covered_through: WEEK_END.to_owned(),
            })
            .collect(),
        duplicate_source_ids: false,
        existing_revision_count: 0,
        existing_scorecard_json: None,
    })
    .expect("scorecard builds");
    serde_json::from_str(&outcome.scorecard_json).expect("scorecard JSON parses")
}

#[test]
fn writer_to_builder_round_trip_flags_copy_paste() {
    // Five messages from one member in one channel inside an hour, each
    // with a measured length of 12. (The detector's threshold is five
    // same-channel messages within ±2 chars in an hour.) The facts go
    // through the real writer (`message_fact`) and the real builder
    // (`build_scorecard` → `detect_copy_paste`).
    let facts: Vec<FactRow> = (0..5)
        .map(|i| {
            let at = format!("2026-09-01T10:{:02}:00.000Z", i * 6);
            let write = write_for(&format!("msg-{i}"), &at, Some(12));
            row_from_write(i64::from(i) + 1, &write)
        })
        .collect();
    let json = build_json(facts);
    let flags = json["abuse_flags"].as_array().expect("abuse_flags array");
    assert!(
        flags
            .iter()
            .any(|f| f["detector"] == "copy_paste" && f["member_id"] == MEMBER),
        "identical-length same-channel burst must flag copy_paste: {flags:?}"
    );
    let quality = json["abuse_data_quality"]
        .as_array()
        .expect("abuse_data_quality array");
    assert!(
        !quality
            .iter()
            .any(|q| q.as_str() == Some("copy_paste:insufficient_data")),
        "length-carrying facts must not report insufficient data: {quality:?}"
    );
}

#[test]
fn writer_without_lengths_stays_fail_closed() {
    // Control: the same burst shape with no measured lengths keeps the
    // fail-closed verdict — `copy_paste:insufficient_data`, never a flag.
    let facts: Vec<FactRow> = (0..5)
        .map(|i| {
            let at = format!("2026-09-01T10:{:02}:00.000Z", i * 6);
            let write = write_for(&format!("msg-{i}"), &at, None);
            row_from_write(i64::from(i) + 1, &write)
        })
        .collect();
    let json = build_json(facts);
    let flags = json["abuse_flags"].as_array().expect("abuse_flags array");
    assert!(
        !flags.iter().any(|f| f["detector"] == "copy_paste"),
        "no length data, no copy_paste verdict: {flags:?}"
    );
    let quality = json["abuse_data_quality"]
        .as_array()
        .expect("abuse_data_quality array");
    assert!(
        quality
            .iter()
            .any(|q| q.as_str() == Some("copy_paste:insufficient_data")),
        "length-less week reports insufficient data: {quality:?}"
    );
}

/// Captured message ids with their recorded integer lengths.
type CapturedMessages = std::sync::Arc<Mutex<Vec<(String, Option<usize>)>>>;

/// Facts sink that captures what the handler records, so the handler path
/// (input → sink) is observable without a database.
#[derive(Clone, Default)]
struct CaptureFacts {
    messages: CapturedMessages,
}

impl FactsSink for CaptureFacts {
    fn record_member_join(&self, _fact: MemberJoinFact<'_>) {}

    fn record_rules_accepted(&self, _fact: RulesAcceptedFact<'_>) {}

    fn record_message(&self, fact: MessageFact<'_>) {
        self.messages
            .lock()
            .expect("capture lock")
            .push((fact.message_id.to_owned(), fact.content_length));
    }

    fn record_voice_started(&self, _fact: VoiceStartedFact<'_>) -> Option<String> {
        None
    }

    fn record_voice_ended(&self, _fact: VoiceEndedFact<'_>) {}
}

#[test]
fn on_message_handler_forwards_content_length_to_the_fact_sink() {
    let sink = CaptureFacts::default();
    let handlers = FunnelHandlers::new(MemStore::new(), Some(NoopLeveling), Some(sink.clone()));
    let base = MessageInput {
        guild_id: 1_545_644_954_272_137_297,
        member_id: 100_000_000_000_000_001,
        is_bot: false,
        message_id: Some("msg-gateway-1".to_owned()),
        webhook_id: None,
        is_staff_automation: false,
        channel_id: 200_000_000_000_000_001,
        channel_class: ChannelClass::Human,
        capture_only: false,
        occurred_at: Some("2026-09-01T10:00:00.000Z".to_owned()),
        content_length: Some(11),
        has_embed_or_attachment: false,
    };
    handlers.on_message(base.clone());
    handlers.on_message(MessageInput {
        message_id: Some("msg-gateway-2".to_owned()),
        occurred_at: Some("2026-09-01T10:01:00.000Z".to_owned()),
        content_length: None,
        has_embed_or_attachment: false,
        ..base
    });
    let captured = sink.messages.lock().expect("capture lock");
    assert_eq!(
        *captured,
        vec![
            ("msg-gateway-1".to_owned(), Some(11)),
            ("msg-gateway-2".to_owned(), None),
        ],
        "the handler forwards the measured length and absence as None"
    );
}
