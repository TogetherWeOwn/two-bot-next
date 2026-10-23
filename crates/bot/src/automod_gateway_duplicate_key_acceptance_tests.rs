#![cfg(test)]
//! Acceptance tests: automod `partial_edit` refuses duplicate JSON keys.
//!
//! PROTECTED: the paired builder must satisfy these tests without editing
//! this file. Do not merge this file on its own while it is red.
//!
//! Contract for `partial_edit(text, receipt_ms)` (`automod_gateway.rs:252`):
//! the raw gateway `MESSAGE_UPDATE` text is decoded with
//! `serde_json::from_str` into a `Value`, so repeated object keys silently
//! last-win today and the winning IDs route the automod verdict pipeline.
//! After the fix, repeated keys at any depth — conflicting values in both
//! orders, identical values, nested dispatch objects, escaped spellings of
//! the same key, and unknown keys — are refused as `None`: no
//! `MessageDelivery` is produced, so no automod verdict can rest on the
//! body. `None` is the single fixed refusal and is inherently non-echoing
//! (it carries no key, value, or body bytes). Unique-key dispatches decode
//! byte-identically.
//!
//! Out of scope by construction: a duplicated `t` whose winner is not
//! `MESSAGE_UPDATE` already yields `None` today for a different reason (not
//! an edit at all), so the refusal is indistinguishable there and untestable
//! through this `Option` seam; the fix still refuses it.
//!
//! Offline only: pure string inputs, no network, no env, no database.
//! RED on the baseline: every refusal test fails with `Some(delivery)`
//! where `None` is expected (silent last-wins); the controls pass.

use super::*;
use two_bot_core::automod_runtime::MessageDeliveryKind;

/// Marker embedded in hostile keys/values. `None` carries nothing, so the
/// no-echo pin is the refusal itself; the marker proves the hostile bytes
/// were present in the refused input.
const SENTINEL: &str = "DUPKEY-SENTINEL-9c4e1a";

fn dispatch(d: &str) -> String {
    format!(r#"{{"op":0,"s":7,"t":"MESSAGE_UPDATE","d":{d}}}"#)
}

const EDIT_A: &str = r#"{"id":"9","channel_id":"8","guild_id":"7","edited_timestamp":"2026-10-02T00:00:01.000000+00:00","content":"edited"}"#;
const EDIT_B: &str = r#"{"id":"10","channel_id":"11","content":"other"}"#;

/// Assert the dispatch is refused (`None`); the label names the shape.
fn assert_refused(text: &str, label: &str) {
    if let Some(delivery) = partial_edit(text, 0) {
        panic!(
            "{label}: duplicate keys must be refused, got delivery for message {}",
            delivery.message_id
        );
    }
}

#[test]
fn duplicate_dispatch_object_is_refused_in_both_orders() {
    // Both winners are valid edits, so last-wins delivers one of them
    // today — a different verdict input per order.
    let a_first = format!(r#"{{"op":0,"s":7,"t":"MESSAGE_UPDATE","d":{EDIT_A},"d":{EDIT_B}}}"#);
    let b_first = format!(r#"{{"op":0,"s":7,"t":"MESSAGE_UPDATE","d":{EDIT_B},"d":{EDIT_A}}}"#);
    assert_refused(&a_first, "d duplicate, edit A first");
    assert_refused(&b_first, "d duplicate, edit B first");
}

#[test]
fn duplicate_message_id_is_refused_in_both_orders() {
    let sentinel_first =
        format!(r#"{{"id":"{SENTINEL}","id":"9","channel_id":"8","content":"edited"}}"#);
    let valid_first =
        format!(r#"{{"id":"9","id":"{SENTINEL}","channel_id":"8","content":"edited"}}"#);
    assert_refused(&dispatch(&sentinel_first), "id duplicate, sentinel first");
    assert_refused(&dispatch(&valid_first), "id duplicate, sentinel second");
}

#[test]
fn duplicate_content_is_refused_in_both_orders() {
    // Content is unread by the decoder, so either order delivers today.
    let edited_first =
        r#"{"id":"9","channel_id":"8","content":"edited","content":"other"}"#.to_owned();
    let other_first =
        r#"{"id":"9","channel_id":"8","content":"other","content":"edited"}"#.to_owned();
    assert_refused(&dispatch(&edited_first), "content duplicate, edited first");
    assert_refused(&dispatch(&other_first), "content duplicate, other first");
}

#[test]
fn identical_duplicate_is_refused() {
    let body = r#"{"id":"9","channel_id":"8","channel_id":"8","content":"edited"}"#;
    assert_refused(
        &dispatch(body),
        "channel_id repeated with the identical value",
    );
}

#[test]
fn escaped_key_spelling_counts_as_duplicate() {
    // `\u0063` decodes to `c`: same key twice after decoding. The winner is
    // unread content, so the dispatch delivers today.
    let body = "{\"id\":\"9\",\"channel_id\":\"8\",\"\\u0063ontent\":\"a\",\"content\":\"b\"}";
    assert_refused(&dispatch(body), "escaped spelling of content");
}

#[test]
fn unknown_duplicate_key_is_refused() {
    // The unknown pair sits beside `d` at the dispatch top level.
    let text = format!(
        r#"{{"op":0,"s":7,"t":"MESSAGE_UPDATE","{SENTINEL}":1,"{SENTINEL}":2,"d":{EDIT_A}}}"#
    );
    assert_refused(&text, "unknown key repeated");
}

#[test]
fn unique_dispatch_decodes_byte_identically() {
    let delivery = partial_edit(&dispatch(EDIT_A), 1_790_726_400_000).expect("partial edit");
    assert_eq!(delivery.kind, MessageDeliveryKind::Update);
    assert_eq!(delivery.message_id, "9");
    assert_eq!(delivery.channel_id, "8");
    assert_eq!(delivery.guild_id.as_deref(), Some("7"));
    assert!(delivery.snapshot.is_none() && delivery.create_pending_roles.is_none());
    assert!(delivery.edited_timestamp_ms.is_some());
    assert_eq!(delivery.observed_timestamp_ms, 1_790_726_400_000);

    // Pre-existing `None` paths stay `None`: refusal adds no delivery.
    let create = r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"id":"9","channel_id":"8"}}"#;
    assert!(partial_edit(create, 0).is_none());
    let missing_ids = r#"{"t":"MESSAGE_UPDATE","d":{"content":"no ids"}}"#;
    assert!(partial_edit(missing_ids, 0).is_none());
}
