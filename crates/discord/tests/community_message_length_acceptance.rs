//! Message-length metadata acceptance: gateway sink half (pure, offline).
//!
//! Companion to the core crate's `community_message_length_acceptance`
//! suite. That suite pins the fact shape; this one pins the live capture
//! seam: the deferred community-facts sink that the gateway pipeline feeds
//! must forward delivered message content into the fact writer, so the
//! buffered `message_created` write carries `contentLength` metadata (the
//! Unicode scalar count) and never the content itself.
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
//! // Its `FactsSink::record_message` forwards `fact.content` into
//! // `community_store::message_fact` (see the core suite header for the
//! // pinned signature and the metadata rule).
//! ```
//!
//! The sink drops message facts while unarmed (no pool), by design. These
//! tests arm it with a *lazy* pool handle (`connect_lazy` performs no I/O
//! and no connection is ever opened: nothing here drains to a database),
//! which exercises the exact armed buffering path the gateway uses.
//!
//! Run: `python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db --test community_message_length_acceptance`
#![cfg(feature = "db")]

use two_bot_core::{ChannelClass, FactsSink, MessageFact};
use two_bot_discord::DeferredCommunityFacts;

/// "café ☕ test" with precomposed é (U+00E9) and hot beverage (U+2615):
/// 11 Unicode scalar values, 14 UTF-8 bytes.
const MULTIBYTE_CONTENT: &str = "caf\u{00E9} \u{2615} test";

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

fn gateway_message<'a>(message_id: &'a str, content: Option<&'a str>) -> MessageFact<'a> {
    MessageFact {
        guild_id: 1_545_644_954_272_137_297,
        member_id: 100_000_000_000_000_001,
        is_bot: false,
        webhook_id: None,
        is_staff_automation: false,
        message_id,
        channel_id: 200_000_000_000_000_001,
        channel_class: ChannelClass::Human,
        occurred_at: "2026-09-01T10:00:00.000Z",
        content,
    }
}

#[test]
fn gateway_sink_buffers_a_length_carrying_fact_without_the_content() {
    assert_eq!(
        MULTIBYTE_CONTENT.chars().count(),
        11,
        "fixture scalar count"
    );
    let buffer = armed_buffer();
    buffer.record_message(gateway_message("msg-gateway-1", Some(MULTIBYTE_CONTENT)));

    let writes = buffer.take_pending();
    assert_eq!(writes.len(), 1, "one armed message buffers one fact write");
    let write = &writes[0];
    assert_eq!(write.event_type, "message_created");
    assert_eq!(write.source_event_id, "msg-gateway-1");
    let metadata = write.metadata.as_deref().expect("message metadata");
    let meta: serde_json::Value = serde_json::from_str(metadata).expect("fact metadata is JSON");
    assert_eq!(
        meta["contentLength"], 11,
        "the buffered gateway fact carries the scalar count: {meta}"
    );
    assert!(
        !metadata.contains(MULTIBYTE_CONTENT) && !metadata.contains("caf"),
        "the content itself never reaches the buffered write: {metadata}"
    );
    assert!(
        buffer.take_pending().is_empty(),
        "take_pending drains the buffer"
    );
}

#[test]
fn gateway_sink_without_content_buffers_no_length_key() {
    // Content absent (e.g. the privileged message-content intent is off):
    // the buffered fact is still recorded, but with no `contentLength` key
    // and never a fabricated `0`.
    let buffer = armed_buffer();
    buffer.record_message(gateway_message("msg-gateway-2", None));

    let writes = buffer.take_pending();
    assert_eq!(writes.len(), 1, "the fact itself is still captured");
    let meta: serde_json::Value =
        serde_json::from_str(writes[0].metadata.as_deref().expect("message metadata"))
            .expect("fact metadata is JSON");
    assert!(
        meta.get("contentLength").is_none(),
        "absent content records no contentLength key: {meta}"
    );
}
