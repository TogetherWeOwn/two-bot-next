//! Message-length metadata acceptance: gateway half (pure, offline).
//!
//! Companion to the core crate's `community_message_length_acceptance`
//! suite. That suite pins the fact shape and handler forwarding; this one
//! pins the live capture seam end to end. The single
//! `Event::MessageCreate` site in the gateway pipeline computes the
//! length from content already in hand — the Unicode scalar count
//! (`chars().count()`), never the UTF-8 byte length — and drops the
//! content at the same statement. Only the integer is threaded through
//! `MessageInput` to `MessageFact` to the deferred sink's buffered
//! `message_created` write, whose metadata carries `contentLength` and
//! never the content itself.
//!
//! Test files only; no runtime code is touched here. Every test below pins
//! behavior the current runtime does not implement yet, so each one FAILS
//! today (the pinned Builder API does not exist yet) and must turn green
//! under the Builder without changing the test.
//!
//! ## Pinned Builder API (in addition to the core suite's pins)
//!
//! ```rust,ignore
//! // crates/discord/src/leveling_runtime.rs
//! impl DeferredCommunityFacts {
//!     /// Take every buffered fact write (`message_created`,
//!     /// `voice_session_started` / `voice_session_ended`), leaving that
//!     /// buffer empty. Mirrors `take` (rules) and `take_joins` (joins);
//!     /// order is arrival order.
//!     pub fn take_pending(&self) -> Vec<FactWrite>;
//! }
//! // Its `FactsSink::record_message` forwards `fact.content_length` into
//! // `community_store::message_fact` (see the core suite header for the
//! // pinned signature and the metadata rule).
//!
//! // crates/discord/src/pipeline.rs, the `Event::MessageCreate` arm
//! MessageInput { /* existing fields, plus */
//!     content_length: Some(msg.content.chars().count())
//!         .filter(|n| *n > 0), // shape is the Builder's; behavior is pinned
//! }
//! ```
//!
//! The pipeline behavior pinned below: a delivered non-empty content
//! records its scalar count; an empty content (nothing delivered, e.g.
//! the privileged message-content intent is off, or an attachment-only
//! message) records no `contentLength` key, never `0`.
//!
//! The sink drops message facts while unarmed (no pool), by design. These
//! tests arm it with a *lazy* pool handle (`connect_lazy` performs no I/O
//! and no connection is ever opened: nothing here drains to a database),
//! which exercises the exact armed buffering path the gateway uses.
//! Creating even a lazy pool needs a Tokio context, so the tests run
//! under `#[tokio::test]`; nothing in them awaits.
//!
//! Run: `python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db --test community_message_length_acceptance`
#![cfg(feature = "db")]

#[path = "support/deferred_capture_compat.rs"]
mod deferred_capture_compat;
use deferred_capture_compat::Healthy;

use serde_json::json;
use twilight_model::{
    channel::Message,
    gateway::{event::Event, payload::incoming::MessageCreate},
};
use two_bot_core::{ChannelClass, FactsSink, MemStore, MessageFact, NoopLeveling};
use two_bot_discord::{DeferredCommunityFacts, NoClassification, Pipeline, ScriptedInvites};

/// "café ☕ test" with precomposed é (U+00E9) and hot beverage (U+2615):
/// 11 Unicode scalar values, 14 UTF-8 bytes.
const MULTIBYTE_CONTENT: &str = "caf\u{00E9} \u{2615} test";
const GUILD: u64 = 1_545_644_954_272_137_297;
const MEMBER: u64 = 100_000_000_000_000_001;
const CHANNEL: u64 = 200_000_000_000_000_001;

fn armed_buffer() -> DeferredCommunityFacts {
    let buffer = DeferredCommunityFacts::new();
    // Lazy handle only: parsing the URL is the whole cost. No connection is
    // opened because these tests never drain.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb/agent_test")
        .expect("lazy pool handle parses");
    buffer.enable(pool).healthy();
    buffer
}

fn gateway_message<'a>(message_id: &'a str, content_length: Option<usize>) -> MessageFact<'a> {
    MessageFact {
        guild_id: GUILD,
        member_id: MEMBER,
        is_bot: false,
        webhook_id: None,
        is_staff_automation: false,
        message_id,
        channel_id: CHANNEL,
        channel_class: ChannelClass::Human,
        occurred_at: "2026-09-01T10:00:00.000Z",
        content_length,
    }
}

fn metadata_of(write: &two_bot_core::community_store::FactWrite) -> serde_json::Value {
    serde_json::from_str(write.metadata.as_deref().expect("message metadata"))
        .expect("fact metadata is JSON")
}

#[tokio::test]
async fn gateway_sink_buffers_a_length_carrying_fact() {
    let buffer = armed_buffer();
    buffer.record_message(gateway_message("msg-gateway-1", Some(11)));

    let writes = buffer.take_pending().healthy();
    assert_eq!(writes.len(), 1, "one armed message buffers one fact write");
    let write = &writes[0];
    assert_eq!(write.event_type, "message_created");
    assert_eq!(write.source_event_id, "msg-gateway-1");
    let meta = metadata_of(write);
    assert_eq!(
        meta["contentLength"], 11,
        "the buffered gateway fact carries the measured length: {meta}"
    );
    assert!(
        buffer.take_pending().healthy().is_empty(),
        "take_pending drains the buffer"
    );
}

#[tokio::test]
async fn gateway_sink_without_length_buffers_no_length_key() {
    // No length measured (e.g. the privileged message-content intent is
    // off): the buffered fact is still recorded, but with no
    // `contentLength` key and never a fabricated `0`.
    let buffer = armed_buffer();
    buffer.record_message(gateway_message("msg-gateway-2", None));

    let writes = buffer.take_pending().healthy();
    assert_eq!(writes.len(), 1, "the fact itself is still captured");
    let meta = metadata_of(&writes[0]);
    assert!(
        meta.get("contentLength").is_none(),
        "unmeasured length records no contentLength key: {meta}"
    );
}

fn message_event(message_id: u64, at: &str, content: &str) -> Event {
    // Deserialized like a gateway payload (the same fixture shape as the
    // automation-admission suite), so no struct literal has to name
    // twilight's deprecated `interaction` field.
    let message: Message = serde_json::from_value(json!({
        "id": message_id.to_string(),
        "guild_id": GUILD.to_string(),
        "channel_id": CHANNEL.to_string(),
        "type": 0,
        "author": {"id": MEMBER.to_string(), "username": "member", "discriminator": "0", "avatar": null},
        "content": content,
        "timestamp": format!("2026-09-01T{at}.000000+00:00"),
        "edited_timestamp": null,
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false
    }))
    .expect("fixture message deserializes");
    Event::MessageCreate(Box::new(MessageCreate(message)))
}

#[tokio::test]
async fn pipeline_message_create_records_scalar_count_not_bytes() {
    assert_eq!(
        MULTIBYTE_CONTENT.chars().count(),
        11,
        "fixture scalar count"
    );
    assert_eq!(MULTIBYTE_CONTENT.len(), 14, "fixture byte count differs");

    let buffer = armed_buffer();
    let pipeline = Pipeline::new(
        MemStore::new(),
        Some(NoopLeveling),
        Some(buffer.clone()),
        ScriptedInvites::new(),
        NoClassification,
    );

    pipeline.handle_at(
        &message_event(4_000_000_000_000_000_001, "10:00:00", MULTIBYTE_CONTENT),
        "2026-09-01T10:00:01.000Z",
    );
    // Scalar values, not grapheme clusters: "e" + combining acute (U+0301)
    // twice is 4 scalars (6 bytes, 2 grapheme clusters).
    pipeline.handle_at(
        &message_event(4_000_000_000_000_000_002, "10:01:00", "e\u{0301}e\u{0301}"),
        "2026-09-01T10:01:01.000Z",
    );
    // Empty content (nothing delivered): fact still captured, no key.
    pipeline.handle_at(
        &message_event(4_000_000_000_000_000_003, "10:02:00", ""),
        "2026-09-01T10:02:01.000Z",
    );

    let writes = buffer.take_pending().healthy();
    assert_eq!(writes.len(), 3, "three gateway messages buffer three facts");

    let first = &writes[0];
    assert_eq!(first.event_type, "message_created");
    assert_eq!(first.source_event_id, "4000000000000000001");
    let metadata = first.metadata.as_deref().expect("message metadata");
    let meta = metadata_of(first);
    assert_eq!(
        meta["contentLength"], 11,
        "contentLength is the Unicode scalar count of the raw content: {meta}"
    );
    assert_ne!(
        meta["contentLength"], 14,
        "contentLength must not be the UTF-8 byte length: {meta}"
    );
    assert!(
        !metadata.contains(MULTIBYTE_CONTENT) && !metadata.contains("caf"),
        "the content itself never reaches the buffered write: {metadata}"
    );

    let meta = metadata_of(&writes[1]);
    assert_eq!(
        meta["contentLength"], 4,
        "combining sequences count per scalar value: {meta}"
    );

    let meta = metadata_of(&writes[2]);
    assert!(
        meta.get("contentLength").is_none(),
        "empty content records no contentLength key, never 0: {meta}"
    );
}
