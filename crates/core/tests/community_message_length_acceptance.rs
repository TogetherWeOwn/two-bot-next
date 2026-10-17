//! Message-length metadata acceptance (pure, offline).
//!
//! Failing-first contract: message facts must carry a `contentLength`
//! metadata value (Unicode scalar count of the raw content) so the weekly
//! scorecard's copy/paste detector can run on production facts instead of
//! reporting `copy_paste:insufficient_data` every week. Content itself is
//! never stored: only its length survives into fact metadata.
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
//! pub fn message_fact(
//!     guild_id: &str,
//!     message_id: &str,
//!     channel_id: &str,
//!     channel_class: &str,
//!     actor: &ClassifyInput,
//!     occurred_at: &str,
//!     content: Option<&str>, // NEW, before `classification`
//!     classification: Classification,
//! ) -> FactWrite;
//! pub async fn record_message_fact(
//!     pool: &Pool<Postgres>, /* same order, plus */ content: Option<&str>,
//!     classification: Classification,
//! ) -> Result<bool, CommunityStoreError>;
//!
//! // crates/core/src/handlers.rs
//! pub struct MessageInput { /* existing fields, plus */ pub content: Option<String> }
//! pub struct MessageFact<'a> { /* existing fields, plus */ pub content: Option<&'a str> }
//! // `FunnelHandlers::on_message` forwards `i.content.as_deref()` into the
//! // `MessageFact` it records.
//! ```
//!
//! Metadata rule: `content: Some(c)` with `!c.is_empty()` records
//! `"contentLength": <c.chars().count()>` (Unicode scalar values, never the
//! UTF-8 byte length and never a grapheme-cluster count). `content: None`
//! (content absent, e.g. the privileged message-content intent is off) or
//! an empty `Some("")` records no `contentLength` key at all — the length
//! is never recorded as `0`, so an unknown length can never masquerade as a
//! measured zero-length message in the copy/paste detector.
//!
//! Call-site updates the Builder owns (mechanical, not part of this file):
//! every existing `message_fact` / `record_message_fact` caller and every
//! `MessageInput` / `MessageFact` construction gains the new field. That
//! includes the gateway pipeline that builds `MessageInput` from a gateway
//! message-create event (it supplies `Some(content)` only when message
//! content was actually delivered, otherwise `None`) and the discord
//! crate's deferred community-facts sink, which forwards
//! `MessageFact.content` into `message_fact`. Note one existing assertion
//! the Builder must revise rather than delete: the community-store unit
//! test that proves message facts store no content currently rejects the
//! substring `"content"` in metadata, which the new `contentLength` key
//! necessarily contains; the check must keep proving no message *text* is
//! stored (as pinned by `message_fact_metadata_never_contains_content`
//! below) without rejecting the length key itself.
//!
//! Run: `python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test community_message_length_acceptance`
#![cfg(feature = "db")]

use std::sync::Mutex;

use two_bot_core::community::{
    build_scorecard, Classification, ClassifyInput, FactRow, ScorecardInputs, StreamCoverage,
    COMMUNITY_FACT_TYPES,
};
use two_bot_core::community_store::{message_fact, FactWrite};
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
/// "café ☕ test" with precomposed é (U+00E9) and hot beverage (U+2615):
/// 11 Unicode scalar values, 14 UTF-8 bytes.
const MULTIBYTE_CONTENT: &str = "caf\u{00E9} \u{2615} test";

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

fn write_for(message_id: &str, occurred_at: &str, content: Option<&str>) -> FactWrite {
    message_fact(
        GUILD,
        message_id,
        CHANNEL,
        "human",
        &actor(MEMBER),
        occurred_at,
        content,
        verdict(),
    )
}

fn metadata_of(write: &FactWrite) -> serde_json::Value {
    serde_json::from_str(write.metadata.as_deref().expect("message metadata"))
        .expect("fact metadata is JSON")
}

#[test]
fn message_fact_records_unicode_scalar_count_not_bytes() {
    assert_eq!(
        MULTIBYTE_CONTENT.chars().count(),
        11,
        "fixture scalar count"
    );
    assert_eq!(MULTIBYTE_CONTENT.len(), 14, "fixture byte count differs");

    let write = write_for("msg-1", "2026-09-01T10:00:00.000Z", Some(MULTIBYTE_CONTENT));
    let meta = metadata_of(&write);
    assert_eq!(
        meta["contentLength"], 11,
        "contentLength is the Unicode scalar count of the raw content: {meta}"
    );
    assert_ne!(
        meta["contentLength"], 14,
        "contentLength must not be the UTF-8 byte length: {meta}"
    );

    // Scalar values, not grapheme clusters: "e" + combining acute (U+0301)
    // twice is 4 scalars (6 bytes, 2 grapheme clusters).
    let combining = "e\u{0301}e\u{0301}";
    assert_eq!(combining.chars().count(), 4, "fixture scalar count");
    let write = write_for("msg-2", "2026-09-01T10:01:00.000Z", Some(combining));
    let meta = metadata_of(&write);
    assert_eq!(
        meta["contentLength"], 4,
        "combining sequences count per scalar value: {meta}"
    );
}

#[test]
fn message_fact_without_content_has_no_length_key() {
    // Content absent (e.g. the privileged message-content intent is off):
    // the fact carries no `contentLength` key, never a fabricated `0`.
    let write = write_for("msg-none", "2026-09-01T10:00:00.000Z", None);
    let meta = metadata_of(&write);
    assert!(
        meta.get("contentLength").is_none(),
        "absent content records no contentLength key: {meta}"
    );

    // An empty string is also "no measured length": never recorded as `0`.
    let write = write_for("msg-empty", "2026-09-01T10:01:00.000Z", Some(""));
    let meta = metadata_of(&write);
    assert!(
        meta.get("contentLength").is_none(),
        "empty content records no contentLength key, never 0: {meta}"
    );
}

#[test]
fn message_fact_metadata_never_contains_content() {
    let content = "zebra-quantifier-9173 zebra-quantifier-9173 caf\u{00E9}";
    let write = write_for("msg-secret", "2026-09-01T10:00:00.000Z", Some(content));
    let metadata = write.metadata.clone().expect("message metadata");
    assert!(
        !metadata.contains("zebra-quantifier-9173"),
        "no message content substring in serialized metadata: {metadata}"
    );
    assert!(
        !metadata.contains(content),
        "the raw content itself is never stored: {metadata}"
    );
    // Only the length survives: every string value in the metadata object is
    // free of the content token, and the scalar count is present.
    let meta = metadata_of(&write);
    assert_eq!(meta["contentLength"], content.chars().count() as u64);
    for (key, value) in meta.as_object().expect("metadata object") {
        if let Some(text) = value.as_str() {
            assert!(
                !text.contains("zebra"),
                "metadata value {key} carries no content substring: {text}"
            );
        }
    }
    // The rest of the write shape stays content-free too.
    assert!(!write.source.contains("zebra"), "source: {}", write.source);
    assert!(
        !write.idempotency_key.contains("zebra"),
        "idempotency key: {}",
        write.idempotency_key
    );
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
    // Five messages from one member in one channel inside an hour, each 12
    // scalar values long but with *different* UTF-8 byte lengths. Only a
    // scalar-count length makes them identical-length for the detector, so
    // a byte-length implementation cannot pass this test. (The detector's
    // threshold is five same-channel messages within ±2 chars in an hour.)
    let contents = [
        "hello world!",               // 12 scalars, 12 bytes
        "caf\u{00E9} \u{2615} tests", // 12 scalars, 15 bytes
        "na\u{00EF}ve fa\u{00E7}ade", // 12 scalars, 14 bytes
        "\u{00FC}nicode r\u{00F6}ck", // 12 scalars, 14 bytes
        "plain text!!",               // 12 scalars, 12 bytes
    ];
    for content in contents {
        assert_eq!(content.chars().count(), 12, "fixture: {content}");
    }
    let facts: Vec<FactRow> = contents
        .iter()
        .enumerate()
        .map(|(i, content)| {
            let at = format!("2026-09-01T10:{:02}:00.000Z", i * 6);
            let write = write_for(&format!("msg-{i}"), &at, Some(content));
            row_from_write((i + 1) as i64, &write)
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
fn writer_without_content_stays_fail_closed() {
    // Control: the same burst shape with no measured lengths keeps the
    // fail-closed verdict — `copy_paste:insufficient_data`, never a flag.
    let facts: Vec<FactRow> = (0..5)
        .map(|i| {
            let at = format!("2026-09-01T10:{:02}:00.000Z", i * 6);
            let write = write_for(&format!("msg-{i}"), &at, None);
            row_from_write((i + 1) as i64, &write)
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

/// Facts sink that captures what the handler records, so the gateway path
/// (handler → sink) is observable without a database.
#[derive(Clone, Default)]
struct CaptureFacts {
    messages: std::sync::Arc<Mutex<Vec<(String, Option<String>)>>>,
}

impl FactsSink for CaptureFacts {
    fn record_member_join(&self, _fact: MemberJoinFact<'_>) {}

    fn record_rules_accepted(&self, _fact: RulesAcceptedFact<'_>) {}

    fn record_message(&self, fact: MessageFact<'_>) {
        self.messages
            .lock()
            .expect("capture lock")
            .push((fact.message_id.to_owned(), fact.content.map(str::to_owned)));
    }

    fn record_voice_started(&self, _fact: VoiceStartedFact<'_>) -> Option<String> {
        None
    }

    fn record_voice_ended(&self, _fact: VoiceEndedFact<'_>) {}
}

#[test]
fn on_message_handler_forwards_content_to_the_fact_sink() {
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
        content: Some(MULTIBYTE_CONTENT.to_owned()),
    };
    handlers.on_message(base.clone());
    handlers.on_message(MessageInput {
        message_id: Some("msg-gateway-2".to_owned()),
        occurred_at: Some("2026-09-01T10:01:00.000Z".to_owned()),
        content: None,
        ..base
    });
    let captured = sink.messages.lock().expect("capture lock");
    assert_eq!(
        *captured,
        vec![
            (
                "msg-gateway-1".to_owned(),
                Some(MULTIBYTE_CONTENT.to_owned())
            ),
            ("msg-gateway-2".to_owned(), None),
        ],
        "the handler forwards delivered content verbatim and absence as None"
    );
}
