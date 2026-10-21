//! Acceptance tests: assistant reply parsing refuses duplicate JSON keys.
//!
//! PROTECTED: the paired builder must satisfy these tests without editing
//! this file. Do not merge this file on its own while it is red.
//!
//! Contract for `parse_reply` / `parse_reply_each`: both the LLM
//! chat-completions envelope (`voice_assistant_request.rs:434`) and the
//! suggestion content it carries (`:459`) are decoded with serde into a
//! `Value`, so repeated object keys silently last-win today and the winning
//! suggestion is proposed. After the fix, repeated keys at any depth of
//! EITHER document — conflicting values in both orders, identical values,
//! nested objects, escaped spellings of the same key, and unknown keys — are
//! refused with ONE fixed error shared by both documents and both entry
//! points, whose `Display` contains `duplicate JSON object key` (the Python
//! refusal precedent). A refused reply yields no suggestion. The refusal
//! renders no body bytes: neither `Display` nor `Debug` may contain hostile
//! keys, values, or bodies, and both stay bounded. Unique-key replies parse
//! byte-identically, and pre-existing errors (`NotJson`, `ContentNotJson`,
//! `WrongShape`) keep their own variants (refusal never swallows them).
//!
//! Offline only: pure byte inputs, no network, no model calls, no env.
//! RED on the baseline: every refusal test fails with `Ok(..)` where `Err`
//! is expected (silent last-wins); the controls pass.

use two_bot_core::voice_assistant_request::{parse_reply, parse_reply_each, ReplyError};

/// Fixed refusal vocabulary (Python dup-key precedent). The singular
/// substring also matches a plural message.
const DUP_TOKEN: &str = "duplicate JSON object key";
/// Marker embedded in hostile keys/values; refusals must never echo it.
const SENTINEL: &str = "DUPKEY-SENTINEL-9c4e1a";
/// Refusal renderings stay bounded: a static description, never body bytes.
const MAX_RENDERED: usize = 1024;

/// Embed JSON `content_json` as the assistant message content of an
/// otherwise-valid chat-completions envelope.
fn envelope(content_json: &str) -> Vec<u8> {
    let content = serde_json::to_string(content_json).expect("content embeds as a string");
    format!(
        r#"{{"id":"reply-1","object":"chat.completion","choices":[{{"index":0,"message":{{"role":"assistant","content":{content}}},"finish_reason":"stop"}}]}}"#
    )
    .into_bytes()
}

fn suggestions_json(items_json: &str) -> String {
    format!(r#"{{"suggestions":{items_json}}}"#)
}

fn valid_content() -> String {
    suggestions_json(r###"[{"template":"##","explanation":"why"}]"###)
}

/// Assert the reply is refused with the fixed error; return the refusal
/// `Display` so callers can pin one fixed error across fixtures.
fn assert_refused(result: Result<Vec<()>, ReplyError>, label: &str) -> String {
    match result {
        Ok(suggestions) => panic!(
            "{label}: duplicate keys must be refused, got {} suggestion(s)",
            suggestions.len()
        ),
        Err(error) => {
            let display = format!("{error}");
            let debug = format!("{error:?}");
            assert!(
                display.contains(DUP_TOKEN),
                "{label}: refusal must carry the fixed vocabulary, got: {display}"
            );
            for rendered in [&display, &debug] {
                assert!(
                    !rendered.contains(SENTINEL),
                    "{label}: refusal echoes hostile bytes: {rendered}"
                );
                assert!(
                    rendered.len() < MAX_RENDERED,
                    "{label}: refusal is not bounded: {rendered}"
                );
            }
            display
        }
    }
}

fn refused_each(body: &[u8], label: &str) -> String {
    // `parse_reply_each` fails the whole reply on envelope/content errors
    // exactly as `parse_reply` does; erase the item type for the shared check.
    let result = parse_reply_each(body).map(|items| vec![(); items.len()]);
    assert_refused(result, label)
}

fn refused(body: &[u8], label: &str) -> String {
    let result = parse_reply(body).map(|items| vec![(); items.len()]);
    assert_refused(result, label)
}

fn choice(content_json: &str) -> String {
    let content = serde_json::to_string(content_json).expect("content embeds as a string");
    format!(
        r#"{{"index":0,"message":{{"role":"assistant","content":{content}}},"finish_reason":"stop"}}"#
    )
}

#[test]
fn envelope_duplicate_choices_is_refused_in_both_orders() {
    // Both winners are valid choice lists with different suggestions, so
    // last-wins proposes one of them today — a different suggestion per order.
    let content_a = suggestions_json(r###"[{"template":"##","explanation":"why-a"}]"###);
    let content_b = suggestions_json(r###"[{"template":"##","explanation":"why-b"}]"###);
    let a_first = format!(
        r#"{{"id":"reply-1","choices":[{a}],"choices":[{b}]}}"#,
        a = choice(&content_a),
        b = choice(&content_b),
    );
    let b_first = format!(
        r#"{{"id":"reply-1","choices":[{b}],"choices":[{a}]}}"#,
        a = choice(&content_a),
        b = choice(&content_b),
    );
    let a = refused_each(a_first.as_bytes(), "choices duplicate, A first");
    let b = refused_each(b_first.as_bytes(), "choices duplicate, B first");
    assert_eq!(a, b, "both orders share one fixed refusal");
}

#[test]
fn envelope_identical_duplicate_is_refused() {
    let content = valid_content();
    let escaped = serde_json::to_string(&content).expect("content embeds as a string");
    let body = format!(
        r#"{{"id":"reply-1","id":"reply-1","choices":[{{"index":0,"message":{{"role":"assistant","content":{escaped}}},"finish_reason":"stop"}}]}}"#
    );
    refused_each(body.as_bytes(), "id repeated with the identical value");
}

#[test]
fn envelope_nested_duplicate_is_refused() {
    // `index` is unread, so the nested duplicate parses today.
    let content = valid_content();
    let escaped = serde_json::to_string(&content).expect("content embeds as a string");
    let body = format!(
        r#"{{"id":"reply-1","choices":[{{"index":0,"index":1,"message":{{"role":"assistant","content":{escaped}}},"finish_reason":"stop"}}]}}"#
    );
    refused_each(body.as_bytes(), "nested conflicting index");
}

#[test]
fn content_duplicate_suggestions_is_refused_in_both_orders() {
    // Both winners are valid suggestion lists, so last-wins proposes one of
    // them today — a different suggestion per order.
    let a_first = r###"{"suggestions":[{"template":"##","explanation":"why-a"}],"suggestions":[{"template":"##","explanation":"why-b"}]}"###;
    let b_first = r###"{"suggestions":[{"template":"##","explanation":"why-b"}],"suggestions":[{"template":"##","explanation":"why-a"}]}"###;
    let a = refused(
        envelope(a_first).as_slice(),
        "suggestions duplicate, A first",
    );
    let b = refused(
        envelope(b_first).as_slice(),
        "suggestions duplicate, B first",
    );
    assert_eq!(a, b, "both orders share one fixed refusal");
}

#[test]
fn content_duplicate_item_field_is_refused_in_both_orders() {
    let valid_first = suggestions_json(&format!(
        r###"[{{"template":"##","explanation":"why","explanation":"{SENTINEL}"}}]"###
    ));
    let sentinel_first = suggestions_json(&format!(
        r###"[{{"template":"##","explanation":"{SENTINEL}","explanation":"why"}}]"###
    ));
    let a = refused(
        envelope(&valid_first).as_slice(),
        "explanation duplicate, valid first",
    );
    let b = refused(
        envelope(&sentinel_first).as_slice(),
        "explanation duplicate, sentinel first",
    );
    assert_eq!(a, b, "both orders share one fixed refusal");
}

#[test]
fn content_identical_duplicate_is_refused() {
    let content =
        suggestions_json(r###"[{"template":"##","template":"##","explanation":"why"}]"###);
    refused(
        envelope(&content).as_slice(),
        "template repeated identically",
    );
}

#[test]
fn escaped_key_spelling_counts_as_duplicate_in_both_documents() {
    // `\u0069` is `i`, `\u0074` is `t`: same key twice after decoding. Both
    // parse points need decoder-level detection, not a string pre-scan.
    let content = valid_content();
    let escaped = serde_json::to_string(&content).expect("content embeds as a string");
    let envelope_body =
        format!("{{\"id\":\"a\",\"\\u0069d\":\"b\",\"choices\":[{{
            \"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":{escaped}}},\"finish_reason\":\"stop\"}}]}}");
    let a = refused_each(envelope_body.as_bytes(), "escaped spelling of envelope id");
    let item = "{\"template\":\"##\",\"\\u0074emplate\":\"##\",\"explanation\":\"why\"}";
    let b = refused(
        envelope(&suggestions_json(&format!("[{item}]"))).as_slice(),
        "escaped spelling of suggestion template",
    );
    assert_eq!(a, b, "both documents share one fixed refusal");
}

#[test]
fn unknown_duplicate_key_is_refused_without_echo_in_both_documents() {
    let content = valid_content();
    let escaped = serde_json::to_string(&content).expect("content embeds as a string");
    let envelope_body = format!(
        r#"{{"{SENTINEL}":1,"{SENTINEL}":2,"id":"reply-1","choices":[{{"index":0,"message":{{"role":"assistant","content":{escaped}}},"finish_reason":"stop"}}]}}"#
    );
    let a = refused_each(envelope_body.as_bytes(), "unknown envelope key repeated");
    // The unknown pair sits beside `suggestions` at content top.
    let content_body = format!(
        r###"{{"suggestions":[{{"template":"##","explanation":"why"}}],"{SENTINEL}":1,"{SENTINEL}":2}}"###
    );
    let b = refused(
        envelope(&content_body).as_slice(),
        "unknown content key repeated",
    );
    assert_eq!(a, b, "both documents share one fixed refusal");
}

#[test]
fn both_entry_points_share_one_fixed_error() {
    // Envelope shape through `parse_reply_each`, content shape through
    // `parse_reply`, then crossed: all four refuse identically.
    let content = valid_content();
    let escaped = serde_json::to_string(&content).expect("content embeds as a string");
    let envelope_body = format!(
        r#"{{"id":"a","id":"b","choices":[{{"index":0,"message":{{"role":"assistant","content":{escaped}}},"finish_reason":"stop"}}]}}"#
    );
    let content_body =
        suggestions_json(r###"[{"template":"##","explanation":"why","explanation":"why-2"}]"###);
    let content_body = envelope(&content_body);
    let each_envelope = refused_each(envelope_body.as_bytes(), "each over envelope dup");
    let each_content = refused_each(content_body.as_slice(), "each over content dup");
    let reply_envelope = refused(envelope_body.as_bytes(), "reply over envelope dup");
    let reply_content = refused(content_body.as_slice(), "reply over content dup");
    for (shape, display) in [
        ("each/content", each_content),
        ("reply/envelope", reply_envelope),
        ("reply/content", reply_content),
    ] {
        assert_eq!(
            each_envelope, display,
            "{shape} refusal must equal the fixed error"
        );
    }
}

#[test]
fn refusal_is_deterministic() {
    let content =
        suggestions_json(r###"[{"template":"##","template":"##","explanation":"why"}]"###);
    let body = envelope(&content);
    let first = refused(body.as_slice(), "repeat call one");
    let second = refused(body.as_slice(), "repeat call two");
    assert_eq!(first, second, "the same body refuses identically");
}

#[test]
fn unique_replies_parse_byte_identically() {
    let body = envelope(&valid_content());
    let suggestions = parse_reply(&body).expect("valid reply parses");
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].template(), "##");
    assert_eq!(suggestions[0].explanation(), "why");

    let each = parse_reply_each(&body).expect("valid reply parses per-item");
    assert_eq!(each.len(), 1);
    let suggestion = each[0].as_ref().expect("valid item parses");
    assert_eq!(suggestion.template(), "##");
    assert_eq!(suggestion.explanation(), "why");

    // Pre-existing error paths keep their own variants: refusal never
    // swallows a syntax or shape error.
    match parse_reply(b"not json") {
        Ok(_) => panic!("invalid envelope JSON must fail"),
        Err(error @ ReplyError::NotJson) => {
            assert!(
                !format!("{error}").contains(DUP_TOKEN),
                "syntax errors are not duplicate refusals: {error}"
            );
        }
        Err(error) => panic!("invalid envelope JSON must fail typed, got: {error:?}"),
    }
    let bad_content = envelope("not json");
    match parse_reply(&bad_content) {
        Ok(_) => panic!("invalid content JSON must fail"),
        Err(error @ ReplyError::ContentNotJson) => {
            assert!(
                !format!("{error}").contains(DUP_TOKEN),
                "content syntax errors are not duplicate refusals: {error}"
            );
        }
        Err(error) => panic!("invalid content JSON must fail typed, got: {error:?}"),
    }
    let no_choices = br#"{"id":"reply-1","object":"chat.completion"}"#.to_vec();
    match parse_reply_each(&no_choices) {
        Ok(_) => panic!("missing choices must fail"),
        Err(error @ ReplyError::WrongShape { .. }) => {
            assert!(
                !format!("{error}").contains(DUP_TOKEN),
                "shape errors are not duplicate refusals: {error}"
            );
        }
        Err(error) => panic!("missing choices must fail typed, got: {error:?}"),
    }
}
