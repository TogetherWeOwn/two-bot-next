//! Hostile-input regression hold for the voice-assistant reply parser.
//!
//! The fuzz target (`fuzz/fuzz_targets/voice_assistant_reply.rs`) feeds
//! arbitrary bytes through [`parse_reply`]/[`parse_reply_each`]; these cases
//! pin the hostile shapes it must hold against as ordinary unit tests so the
//! standard `cargo test` lane proves the hold without a libFuzzer run. Every
//! refusal is a bounded [`ReplyError`] carrying only static field names,
//! indexes and counts, never reply text.

use serde_json::json;
use two_bot_core::voice_assistant_request::{
    parse_reply, parse_reply_each, parse_template_strict, ReplyError, TemplateIssue,
    MAX_EXPLANATION_CHARS, MAX_REPLY_BYTES, MAX_SUGGESTIONS, MAX_TEMPLATE_CHARS,
};

// Token-safe and lowercase, so `@@{MARKER}@@` parses as an unknown token whose
// name the parser keeps verbatim.
const MARKER: &str = "leak_marker_7f3a";

fn envelope(content: &str) -> Vec<u8> {
    json!({
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
    envelope(&json!({ "suggestions": items }).to_string())
}

fn assert_bounded(error: &ReplyError) {
    assert!(
        error.to_string().chars().count() <= 256,
        "error display stays bounded: {error}"
    );
    assert!(
        !error.to_string().contains(MARKER),
        "error carries no reply text: {error}"
    );
    assert!(
        !format!("{error:?}").contains(MARKER),
        "error debug carries no reply text: {error:?}"
    );
}

#[test]
fn hostile_bodies_refuse_as_bounded_errors() {
    let leak = format!("{MARKER} <@123456789012345678>");
    let cases: Vec<(Vec<u8>, ReplyError)> = vec![
        (
            vec![b' '; MAX_REPLY_BYTES + 1],
            ReplyError::Oversize {
                len: MAX_REPLY_BYTES + 1,
            },
        ),
        (format!("not json {leak}").into_bytes(), ReplyError::NotJson),
        (vec![0xff, 0xfe, b'{'], ReplyError::NotJson),
        (vec![], ReplyError::NotJson),
        (
            json!({"error": {"message": leak}}).to_string().into_bytes(),
            ReplyError::EndpointError,
        ),
        (
            json!([leak]).to_string().into_bytes(),
            ReplyError::WrongShape { at: "body" },
        ),
        (
            json!({"choices": "not-an-array"}).to_string().into_bytes(),
            ReplyError::WrongShape { at: "choices" },
        ),
        (
            json!({"result": leak}).to_string().into_bytes(),
            ReplyError::WrongShape { at: "choices" },
        ),
        (
            json!({"choices": []}).to_string().into_bytes(),
            ReplyError::WrongShape { at: "choices" },
        ),
        (
            json!({"choices": [{"text": leak}]})
                .to_string()
                .into_bytes(),
            ReplyError::WrongShape {
                at: "choices[0].message",
            },
        ),
        (
            json!({"choices": [{"message": {"content": null}}]})
                .to_string()
                .into_bytes(),
            ReplyError::WrongShape {
                at: "choices[0].message.content",
            },
        ),
        (envelope(&leak), ReplyError::ContentNotJson),
        (
            envelope("```json\n{\"suggestions\": [broken\n```"),
            ReplyError::ContentNotJson,
        ),
        (
            envelope(&json!({"suggestions": "not-an-array"}).to_string()),
            ReplyError::WrongShape { at: "suggestions" },
        ),
        (
            envelope(&json!({"names": []}).to_string()),
            ReplyError::WrongShape { at: "suggestions" },
        ),
        (
            suggestions(json!([leak])),
            ReplyError::WrongShape {
                at: "suggestions[].template",
            },
        ),
        (
            suggestions(json!([{"template": "##", "explanation": 7}])),
            ReplyError::WrongShape {
                at: "suggestions[].explanation",
            },
        ),
        (suggestions(json!([])), ReplyError::NoSuggestions),
        (
            suggestions(json!([
                {"template": "##", "explanation": "a"},
                {"template": "$#", "explanation": "b"},
                {"template": "+#", "explanation": "c"},
                {"template": "[[A/B]]", "explanation": "d"},
            ])),
            ReplyError::TooManySuggestions {
                count: MAX_SUGGESTIONS + 1,
            },
        ),
    ];
    for (body, expected) in cases {
        let each_err = parse_reply_each(&body).expect_err("hostile body refuses");
        assert_eq!(each_err, expected, "{}", String::from_utf8_lossy(&body));
        assert_eq!(parse_reply(&body), Err(expected));
        assert_bounded(&expected);
    }
}

#[test]
fn hostile_templates_fail_per_position_without_echo() {
    // Through `parse_reply` the whole reply fails; through `parse_reply_each`
    // the same index comes back as a per-position template issue.
    let cases: Vec<(String, TemplateIssue)> = vec![
        (format!("@@{MARKER}@@"), TemplateIssue::UnknownToken),
        (
            "<<open only".to_string(),
            TemplateIssue::UnbalancedDelimiter,
        ),
        ("closed>>".to_string(), TemplateIssue::UnbalancedDelimiter),
        (
            "{{PLAYING ?? ".to_string(),
            TemplateIssue::UnbalancedDelimiter,
        ),
        (
            format!("{MARKER}\nnewline"),
            TemplateIssue::ControlCharacter,
        ),
        ("tab\there".to_string(), TemplateIssue::ControlCharacter),
        ("   ".to_string(), TemplateIssue::Blank),
        ("a".repeat(MAX_TEMPLATE_CHARS + 1), TemplateIssue::TooLong),
        (
            format!("{}x{}", "{{".repeat(70), "}}".repeat(70)),
            TemplateIssue::UnbalancedDelimiter,
        ),
        ("[[@@nope@@/b]]".to_string(), TemplateIssue::UnknownToken),
    ];
    for (template, issue) in cases {
        let body = suggestions(json!([{"template": template, "explanation": ""}]));
        assert_eq!(
            parse_reply(&body),
            Err(ReplyError::InvalidTemplate { index: 0, issue })
        );
        assert_eq!(
            parse_reply_each(&body).expect("envelope parses"),
            [Err(issue)]
        );
        assert_bounded(&parse_reply(&body).expect_err("hostile template refuses"));
    }
}

#[test]
fn hostile_envelope_nesting_never_panics() {
    // Deep JSON nesting hits serde's recursion limit as `NotJson`; deep
    // template nesting stays literal until the strict check refuses it.
    let inner = json!({"suggestions": [{"template": "##", "explanation": ""}]}).to_string();
    let nested = json!({"choices": [{"message": {"content": inner}}]}).to_string();
    let _ = parse_reply(nested.as_bytes());

    let deep_json = json!({"a": {"a": {"a": {}}}}).to_string();
    let _ = parse_reply(deep_json.as_bytes());
    let deep_array = format!("[{}", "[".repeat(500));
    let _ = parse_reply(deep_array.as_bytes());

    let deep_template = format!("{}x{}", "{{".repeat(200), "}}".repeat(200));
    let body = suggestions(json!([{"template": deep_template, "explanation": ""}]));
    let result = parse_reply(&body);
    assert!(result.is_err());
    if let Err(error) = result {
        assert_bounded(&error);
    }

    // Huge numbers and long digit runs are just JSON values, never IDs here.
    let big_numbers = json!({
        "choices": [{
            "message": {"content": json!({"suggestions": [
                {"template": "##", "explanation": "e"},
            ]}).to_string()}
        }],
        "usage": {"prompt_tokens": 9999999999999999999u64, "total": 1e308}
    })
    .to_string();
    let parsed = parse_reply(big_numbers.as_bytes()).expect("numbers are envelope extras");
    assert_eq!(parsed.len(), 1);
}

#[test]
fn accepted_hostile_looking_replies_still_respect_every_bound() {
    // Multibyte text measures in characters, not bytes: the limit string in
    // `é` (2 bytes each) is accepted at exactly the bound and refused one
    // character over.
    let at_limit = "é".repeat(MAX_EXPLANATION_CHARS);
    let ok = suggestions(json!([{"template": "##", "explanation": at_limit}]));
    assert!(parse_reply(&ok).is_ok());
    let over = suggestions(json!([
        {"template": "##", "explanation": format!("{at_limit}é")},
    ]));
    assert_eq!(
        parse_reply(&over),
        Err(ReplyError::ExplanationTooLong { index: 0 })
    );

    // The fixed-matrix over-long second explanation refuses with its index.
    let long_second = "e".repeat(MAX_EXPLANATION_CHARS + 1);
    let over_second = suggestions(json!([
        {"template": "##", "explanation": "fine"},
        {"template": "$#", "explanation": long_second},
    ]));
    assert_eq!(
        parse_reply(&over_second),
        Err(ReplyError::ExplanationTooLong { index: 1 })
    );
    assert_eq!(
        parse_reply_each(&over_second),
        Err(ReplyError::ExplanationTooLong { index: 1 })
    );
    assert_bounded(&parse_reply(&over_second).expect_err("second explanation over limit refuses"));

    // A numeric explanation is a shape refusal at its documented position.
    assert_eq!(
        parse_reply(&suggestions(json!([{"template": "##", "explanation": 7}]))),
        Err(ReplyError::WrongShape {
            at: "suggestions[].explanation"
        })
    );

    // NUL bytes are control characters in templates, not silent truncation.
    let nul = "##\u{0}";
    assert_eq!(
        parse_reply(&suggestions(json!([{"template": nul, "explanation": ""}]))),
        Err(ReplyError::InvalidTemplate {
            index: 0,
            issue: TemplateIssue::ControlCharacter,
        })
    );

    // Both entry points agree on every hostile body in this file.
    for body in [
        ok,
        over,
        suggestions(json!([{"template": "##", "explanation": 7}])),
        envelope("```\nnot json\n```"),
    ] {
        let reply = parse_reply(&body);
        let each = parse_reply_each(&body);
        match &each {
            Err(error) => assert_eq!(reply.as_ref(), Err(error)),
            Ok(items) => {
                if items.iter().all(Result::is_ok) {
                    let parsed = reply.as_ref().expect("all-valid each implies reply Ok");
                    assert_eq!(parsed.len(), items.len());
                    for suggestion in parsed {
                        assert_eq!(
                            parse_template_strict(suggestion.template()),
                            Ok(suggestion.parsed().clone())
                        );
                    }
                } else {
                    assert!(matches!(reply, Err(ReplyError::InvalidTemplate { .. })));
                }
            }
        }
        if let Err(error) = reply {
            assert_bounded(&error);
        }
    }
}
