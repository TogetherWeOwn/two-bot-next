#![no_main]

//! Fuzz the voice-assistant reply front door: [`parse_reply`] and
//! [`parse_reply_each`] over arbitrary model-output bytes.
//!
//! Input structure: the raw fuzzer bytes are the chat-completions response
//! body, exactly as the caller would hand it to the parser. No shaping,
//! keyword routing or UTF-8 filtering: invalid UTF-8, truncation and hostile
//! nesting all reach the production seam, which must refuse them as a bounded
//! [`ReplyError`] (static field names, indexes and counts only, never reply
//! text) rather than panic, allocate without bound or hang.
//!
//! Bounds: bodies over [`MAX_REPLY_BYTES`] refuse before parsing; at most
//! [`MAX_SUGGESTIONS`] suggestions survive; each template is at most
//! [`MAX_TEMPLATE_CHARS`] characters and each explanation at most
//! [`MAX_EXPLANATION_CHARS`] characters. The per-iteration oracle pins the
//! [`parse_reply`]/[`parse_reply_each`] contract: envelope failures agree
//! exactly, per-suggestion template failures surface as `Ok(Err(..))` positions
//! in `parse_reply_each` and as `Err(InvalidTemplate)` in `parse_reply`, and
//! every accepted suggestion respects every bound with a strict parse that
//! agrees with its stored AST. Error displays stay short, proving boundedness.
//! No network, database, Discord client, model call or secret is used.

use std::sync::Once;

use libfuzzer_sys::fuzz_target;
use two_bot_core::voice_assistant_request::{
    parse_reply, parse_reply_each, parse_template_strict, ReplyError, TemplateIssue,
    MAX_EXPLANATION_CHARS, MAX_REPLY_BYTES, MAX_SUGGESTIONS, MAX_TEMPLATE_CHARS,
};

/// Marker that must never appear in an error display: [`ReplyError`] carries
/// only static field names, indexes and counts, never reply text.
const MARKER: &str = "leak_marker_7f3a";

/// Upper bound on any [`ReplyError`] display. The longest rendering is the
/// oversize refusal (`"reply body is 18446744073709551615 bytes, over the
/// 65536-byte limit"`, well under 100 chars); 256 leaves headroom without
/// letting an echo hide inside.
const MAX_ERROR_DISPLAY_CHARS: usize = 256;

fn envelope(content: &str) -> Vec<u8> {
    serde_json::json!({
        "id": "reply-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }]
    })
    .to_string()
    .into_bytes()
}

fn suggestions(items: serde_json::Value) -> Vec<u8> {
    envelope(&serde_json::json!({ "suggestions": items }).to_string())
}

fn assert_bounded(error: &ReplyError) {
    let display = error.to_string();
    let debug = format!("{error:?}");
    assert!(
        display.chars().count() <= MAX_ERROR_DISPLAY_CHARS,
        "error display is bounded: {display:?}"
    );
    assert!(
        debug.chars().count() <= MAX_ERROR_DISPLAY_CHARS + 64,
        "error debug is bounded: {debug:?}"
    );
    assert!(
        !display.contains(MARKER) && !debug.contains(MARKER),
        "error carries no reply text: {debug:?}"
    );
    // Exhaustive: a new variant must extend this oracle on its own card.
    match *error {
        ReplyError::Oversize { .. }
        | ReplyError::NotJson
        | ReplyError::EndpointError
        | ReplyError::WrongShape { .. }
        | ReplyError::ContentNotJson
        | ReplyError::NoSuggestions
        | ReplyError::TooManySuggestions { .. }
        | ReplyError::ExplanationTooLong { .. }
        | ReplyError::InvalidTemplate { .. }
        | ReplyError::DuplicateKeys => {}
    }
}

fn assert_accepted(parsed: &[two_bot_core::voice_assistant_request::Suggestion]) {
    assert!(
        (1..=MAX_SUGGESTIONS).contains(&parsed.len()),
        "accepted list is non-empty and bounded"
    );
    for suggestion in parsed {
        assert!(
            suggestion.template().chars().count() <= MAX_TEMPLATE_CHARS,
            "accepted template is bounded"
        );
        assert!(
            suggestion.explanation().chars().count() <= MAX_EXPLANATION_CHARS,
            "accepted explanation is bounded"
        );
        assert_eq!(
            parse_template_strict(suggestion.template()),
            Ok(suggestion.parsed().clone()),
            "stored AST agrees with a strict re-parse"
        );
    }
}

/// Input-independent pins: every hostile shape from the issue's acceptance
/// list refuses with its documented bounded error, and the one happy shape
/// parses. Runs once per campaign; the oversize allocation stays here so
/// per-iteration inputs stay small and fast.
fn check_fixed_matrix() {
    // Happy path: one minimal suggestion parses through both entry points.
    let valid = suggestions(serde_json::json!([
        {"template": "##", "explanation": "ok"},
    ]));
    let parsed = parse_reply(&valid).expect("valid reply parses");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].template(), "##");
    let each = parse_reply_each(&valid).expect("valid reply parses each");
    assert_eq!(each.len(), 1);
    assert!(each[0].is_ok());

    // Oversize refuses before parsing.
    let big = vec![b'x'; MAX_REPLY_BYTES + 1];
    assert_eq!(
        parse_reply(&big),
        Err(ReplyError::Oversize {
            len: MAX_REPLY_BYTES + 1
        })
    );
    assert_eq!(
        parse_reply_each(&big),
        Err(ReplyError::Oversize {
            len: MAX_REPLY_BYTES + 1
        })
    );

    // Non-JSON refuses.
    let not_json = format!("not json {MARKER}").into_bytes();
    assert_eq!(parse_reply(&not_json), Err(ReplyError::NotJson));
    assert_eq!(parse_reply_each(&not_json), Err(ReplyError::NotJson));

    // Invalid UTF-8 refuses as non-JSON, not a panic.
    let invalid_utf8 = vec![0xff, 0xfe, b'{'];
    assert_eq!(parse_reply(&invalid_utf8), Err(ReplyError::NotJson));

    // Error object refuses without echoing its message.
    let endpoint = serde_json::json!({"error": {"message": MARKER}})
        .to_string()
        .into_bytes();
    assert_eq!(parse_reply(&endpoint), Err(ReplyError::EndpointError));

    // Wrong shapes refuse at their documented position.
    for (body, at) in [
        (serde_json::json!([MARKER]).to_string().into_bytes(), "body"),
        (
            serde_json::json!({"result": MARKER})
                .to_string()
                .into_bytes(),
            "choices",
        ),
        (
            serde_json::json!({"choices": []}).to_string().into_bytes(),
            "choices",
        ),
        (
            serde_json::json!({"choices": [{"text": MARKER}]})
                .to_string()
                .into_bytes(),
            "choices[0].message",
        ),
        (
            serde_json::json!({"choices": [{"message": {"content": null}}]})
                .to_string()
                .into_bytes(),
            "choices[0].message.content",
        ),
        (
            envelope(&serde_json::json!({"names": []}).to_string()),
            "suggestions",
        ),
        (
            suggestions(serde_json::json!([MARKER])),
            "suggestions[].template",
        ),
        (
            suggestions(serde_json::json!([{"template": "##", "explanation": 7}])),
            "suggestions[].explanation",
        ),
    ] {
        assert_eq!(
            parse_reply(&body),
            Err(ReplyError::WrongShape { at }),
            "wrong shape at {at}"
        );
    }

    // Content that is not JSON refuses.
    assert_eq!(
        parse_reply(&envelope(MARKER)),
        Err(ReplyError::ContentNotJson)
    );

    // Repeated keys refuse at either document with the one fixed error,
    // through both entry points.
    let dup_envelope = br#"{"choices": [], "choices": []}"#.to_vec();
    assert_eq!(parse_reply(&dup_envelope), Err(ReplyError::DuplicateKeys));
    assert_eq!(
        parse_reply_each(&dup_envelope),
        Err(ReplyError::DuplicateKeys)
    );
    let dup_content = envelope(r#"{"suggestions": [], "suggestions": []}"#);
    assert_eq!(parse_reply(&dup_content), Err(ReplyError::DuplicateKeys));
    assert_eq!(
        parse_reply_each(&dup_content),
        Err(ReplyError::DuplicateKeys)
    );
    assert_bounded(&ReplyError::DuplicateKeys);

    // Empty suggestion list refuses.
    assert_eq!(
        parse_reply(&suggestions(serde_json::json!([]))),
        Err(ReplyError::NoSuggestions)
    );

    // Over-long suggestion list refuses with its count.
    let over: Vec<serde_json::Value> =
        vec![serde_json::json!({"template": "##", "explanation": "e"}); MAX_SUGGESTIONS + 1];
    assert_eq!(
        parse_reply(&suggestions(serde_json::json!(over))),
        Err(ReplyError::TooManySuggestions {
            count: MAX_SUGGESTIONS + 1
        })
    );

    // Over-long explanation refuses with its index.
    let long_explanation = "e".repeat(MAX_EXPLANATION_CHARS + 1);
    assert_eq!(
        parse_reply(&suggestions(serde_json::json!([
            {"template": "##", "explanation": "fine"},
            {"template": "$#", "explanation": long_explanation},
        ]))),
        Err(ReplyError::ExplanationTooLong { index: 1 })
    );

    // Bad templates fail the whole reply through `parse_reply` but come back
    // per-position through `parse_reply_each`.
    let bad = suggestions(serde_json::json!([
        {"template": format!("@@{MARKER}@@"), "explanation": ""},
    ]));
    assert_eq!(
        parse_reply(&bad),
        Err(ReplyError::InvalidTemplate {
            index: 0,
            issue: TemplateIssue::UnknownToken,
        })
    );
    assert_eq!(
        parse_reply_each(&bad).expect("envelope parses"),
        [Err(TemplateIssue::UnknownToken)]
    );

    // Every refusal above is bounded and echo-free.
    for body in [
        big,
        not_json,
        invalid_utf8,
        endpoint,
        envelope(MARKER),
        suggestions(serde_json::json!([])),
    ] {
        assert_bounded(&parse_reply(&body).expect_err("matrix refusal"));
    }
}

fuzz_target!(|data: &[u8]| {
    static MATRIX: Once = Once::new();
    MATRIX.call_once(check_fixed_matrix);

    // Arbitrary bytes through both entry points: no panic, OOM or hang.
    // The 64 KiB envelope cap plus the 400/600-char per-suggestion caps keep
    // each call bounded; libFuzzer's timeout/RSS catch a hang or blowup.
    let first = parse_reply(data);
    assert_eq!(first, parse_reply(data), "parse_reply is deterministic");
    let first_each = parse_reply_each(data);
    assert_eq!(
        first_each,
        parse_reply_each(data),
        "parse_reply_each is deterministic"
    );

    match &first_each {
        Err(envelope_error) => {
            // Envelope failures agree exactly across both entry points.
            assert_bounded(envelope_error);
            assert_eq!(first, Err(*envelope_error));
        }
        Ok(items) => {
            assert!(
                (1..=MAX_SUGGESTIONS).contains(&items.len()),
                "parse_reply_each only succeeds non-empty and bounded"
            );
            for item in items {
                if let Err(issue) = item {
                    // Template failures are the only per-position errors;
                    // the issue itself is a small enum, never reply text.
                    let _ = format!("{issue}");
                }
            }
            match &first {
                Ok(parsed) => {
                    // All positions valid: the vec lengths agree and every
                    // suggestion respects every bound.
                    assert_eq!(parsed.len(), items.len());
                    assert!(items.iter().all(Result::is_ok));
                    assert_accepted(parsed);
                }
                Err(ReplyError::InvalidTemplate { index, issue }) => {
                    // First bad position surfaces with its index and issue.
                    assert!(*index < items.len());
                    assert_eq!(items[*index], Err(*issue));
                    assert!(items[..*index].iter().all(Result::is_ok));
                    assert_bounded(&ReplyError::InvalidTemplate {
                        index: *index,
                        issue: *issue,
                    });
                }
                Err(other) => {
                    // Shape failures (missing fields, over-long explanation)
                    // fail the whole reply through both entry points.
                    assert_bounded(other);
                    panic!("parse_reply_each succeeded but parse_reply failed: {other:?}");
                }
            }
        }
    }

    if let Err(error) = &first {
        assert_bounded(error);
    }
});
