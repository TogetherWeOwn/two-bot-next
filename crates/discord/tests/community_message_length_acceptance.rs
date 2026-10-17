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
//!
//! Run: `python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db --test community_message_length_acceptance`
#![cfg(feature = "db")]

use std::str::FromStr;

use twilight_model::{
    channel::{
        message::{MessageFlags, MessageType},
        Message,
    },
    gateway::{event::Event, payload::incoming::MessageCreate},
    id::Id,
    user::User,
    util::Timestamp,
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
    buffer.enable(pool);
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

#[test]
fn gateway_sink_buffers_a_length_carrying_fact() {
    let buffer = armed_buffer();
    buffer.record_message(gateway_message("msg-gateway-1", Some(11)));

    let writes = buffer.take_pending();
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
        buffer.take_pending().is_empty(),
        "take_pending drains the buffer"
    );
}

#[test]
fn gateway_sink_without_length_buffers_no_length_key() {
    // No length measured (e.g. the privileged message-content intent is
    // off): the buffered fact is still recorded, but with no
    // `contentLength` key and never a fabricated `0`.
    let buffer = armed_buffer();
    buffer.record_message(gateway_message("msg-gateway-2", None));

    let writes = buffer.take_pending();
    assert_eq!(writes.len(), 1, "the fact itself is still captured");
    let meta = metadata_of(&writes[0]);
    assert!(
        meta.get("contentLength").is_none(),
        "unmeasured length records no contentLength key: {meta}"
    );
}

fn ts(s: &str) -> Timestamp {
    Timestamp::from_str(&format!("2026-09-01T{s}.000+00:00")).expect("fixture stamp")
}

fn user(id: u64) -> User {
    User {
        accent_color: None,
        avatar: None,
        avatar_decoration: None,
        avatar_decoration_data: None,
        banner: None,
        bot: false,
        discriminator: 0,
        email: None,
        flags: None,
        global_name: None,
        id: Id::new(id),
        locale: None,
        mfa_enabled: None,
        name: "member".to_owned(),
        premium_type: None,
        primary_guild: None,
        public_flags: None,
        system: None,
        verified: None,
    }
}

fn message_event(message_id: u64, at: &str, content: &str) -> Event {
    Event::MessageCreate(Box::new(MessageCreate(Message {
        activity: None,
        application: None,
        application_id: None,
        attachments: vec![],
        author: user(MEMBER),
        call: None,
        channel_id: Id::new(CHANNEL),
        components: vec![],
        content: content.to_owned(),
        edited_timestamp: None,
        embeds: vec![],
        flags: Some(MessageFlags::empty()),
        guild_id: Some(Id::new(GUILD)),
        id: Id::new(message_id),
        // `interaction` is deprecated in favour of `interaction_metadata`
        // but still a required literal field on this twilight version.
        #[allow(deprecated)]
        interaction: None,
        interaction_metadata: None,
        kind: MessageType::Regular,
        member: None,
        mention_channels: vec![],
        mention_everyone: false,
        mention_roles: vec![],
        mentions: vec![],
        message_snapshots: vec![],
        pinned: false,
        poll: None,
        reactions: vec![],
        reference: None,
        referenced_message: None,
        role_subscription_data: None,
        sticker_items: vec![],
        timestamp: ts(at),
        thread: None,
        tts: false,
        webhook_id: None,
    })))
}

#[test]
fn pipeline_message_create_records_scalar_count_not_bytes() {
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

    let writes = buffer.take_pending();
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
