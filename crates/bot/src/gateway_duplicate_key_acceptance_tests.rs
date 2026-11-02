#![cfg(test)]
//! Acceptance tests: gateway frame parsing refuses duplicate JSON keys.
//!
//! PROTECTED: the paired builder must satisfy these tests without editing
//! this file. Do not merge this file on its own while it is red.
//!
//! Contract for the frame parses (`gateway.rs:1321-1328`):
//!
//! * `Header` / `HelloPacket` are derived structs, and serde already refuses
//!   duplicate struct fields: the struct tests below pin that premise GREEN
//!   on the baseline. The builder changes nothing there unless a guard goes
//!   red.
//! * The op-9 session flag (`packet["d"]`, `:1328-1329`) decodes into a
//!   `Value`, so a repeated `d` silently last-wins today and the winner
//!   decides resume-vs-`Clear`. That parse has no callable seam on the
//!   baseline, so this file carries a test-local [`BaselineSessionFlag`]
//!   adapter implemented for [`Header`]: its method mirrors the current two
//!   lines exactly (last-wins). The builder adds an INHERENT
//!   `Header::parse_session_resumable(&str) -> Result<bool, E>` (any error
//!   type whose `Display` contains `duplicate JSON object key` on refusal,
//!   renders no frame bytes, and stays bounded) and calls it from the op-9
//!   branch; inherent methods win over the trait, so the session-flag tests
//!   flip GREEN without editing this file. The loop MUST call the inherent
//!   method (review-verified): the tests pin the helper's behavior, and only
//!   the call site wires the refusal into the resume decision. The legacy
//!   control keeps one explicit qualified trait call so the adapter stays
//!   live after GREEN (clippy `-D warnings` would otherwise fail the build
//!   on `never used`, and the builder may not edit this file).
//!
//! Boundary: duplicates nested inside an op-0 dispatch body are parsed by
//! `twilight_gateway` (library scope) and are NOT refused here; the boundary
//! control pins that `Header` keeps ignoring them.
//!
//! Offline only: pure string inputs, no network, no env, no database.
//! RED on the baseline: every session-flag refusal test fails with `Ok(..)`
//! where `Err` is expected (silent last-wins); the struct guards, the legacy
//! control, and the valid controls pass.

use super::{Header, HelloPacket};
use serde_json::Value;

/// Fixed refusal vocabulary (Python dup-key precedent). The singular
/// substring also matches a plural message.
const DUP_TOKEN: &str = "duplicate JSON object key";
/// Marker embedded in hostile keys; refusals must never echo it.
const SENTINEL: &str = "DUPKEY-SENTINEL-9c4e1a";
/// Refusal renderings stay bounded: a static description, never frame bytes.
const MAX_RENDERED: usize = 1024;

/// BASELINE adapter for the op-9 session-flag parse. Mirrors
/// `gateway.rs:1328-1329` exactly: last-wins `Value` decode, then `d` as a
/// bool. See the module header for the GREEN contract.
trait BaselineSessionFlag {
    fn parse_session_resumable(text: &str) -> Result<bool, String>;
}

impl BaselineSessionFlag for Header {
    fn parse_session_resumable(text: &str) -> Result<bool, String> {
        let packet: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
        packet["d"]
            .as_bool()
            .ok_or_else(|| "invalid gateway session flag".to_owned())
    }
}

/// Assert the session flag is refused with the fixed error; return the
/// refusal `Display` so callers can pin one fixed error across fixtures.
/// Generic over the error type: `String` on the baseline, the builder's
/// inherent error after GREEN.
fn assert_refused<E: std::fmt::Display + std::fmt::Debug>(
    result: Result<bool, E>,
    label: &str,
) -> String {
    match result {
        Ok(resumable) => {
            panic!("{label}: duplicate session flag must be refused, got resumable={resumable}")
        }
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

/// Assert `error` is not a duplicate refusal: pre-existing failures keep
/// their own vocabulary. Generic over the error type like `assert_refused`:
/// `String` on the baseline, the builder's inherent error after GREEN.
fn assert_not_duplicate_refusal<E: std::fmt::Display>(error: &E, label: &str) {
    let display = format!("{error}");
    assert!(
        !display.contains(DUP_TOKEN),
        "{label} is not a duplicate refusal: {display}"
    );
}

#[test]
fn session_flag_duplicate_is_refused_in_both_orders() {
    // Today each order resumes differently: the refusal removes an
    // order-dependent resume-vs-Clear decision.
    let true_first = r#"{"op":9,"d":true,"d":false}"#;
    let false_first = r#"{"op":9,"d":false,"d":true}"#;
    let a = assert_refused(
        Header::parse_session_resumable(true_first),
        "session flag duplicate, true first",
    );
    let b = assert_refused(
        Header::parse_session_resumable(false_first),
        "session flag duplicate, false first",
    );
    assert_eq!(a, b, "both orders share one fixed refusal");
}

#[test]
fn session_flag_identical_duplicate_is_refused() {
    let a = assert_refused(
        Header::parse_session_resumable(r#"{"op":9,"d":true,"d":true}"#),
        "session flag repeated true",
    );
    let b = assert_refused(
        Header::parse_session_resumable(r#"{"op":9,"d":false,"d":false}"#),
        "session flag repeated false",
    );
    assert_eq!(a, b, "identical shapes share one fixed refusal");
}

#[test]
fn session_flag_escaped_key_spelling_counts_as_duplicate() {
    // `\u0064` decodes to `d`: same key twice after decoding.
    assert_refused(
        Header::parse_session_resumable("{\"op\":9,\"d\":true,\"\\u0064\":false}"),
        "escaped spelling of d",
    );
}

#[test]
fn session_flag_unknown_duplicate_key_is_refused_without_echo() {
    assert_refused(
        Header::parse_session_resumable(&format!(
            r#"{{"op":9,"d":true,"{SENTINEL}":1,"{SENTINEL}":2}}"#
        )),
        "unknown key repeated beside d",
    );
}

#[test]
fn every_session_flag_shape_shares_one_fixed_error() {
    let shapes = [
        ("conflicting", r#"{"op":9,"d":true,"d":false}"#.to_owned()),
        ("identical", r#"{"op":9,"d":false,"d":false}"#.to_owned()),
        (
            "escaped",
            "{\"op\":9,\"d\":true,\"\\u0064\":false}".to_owned(),
        ),
        (
            "unknown-key",
            format!(r#"{{"op":9,"d":true,"{SENTINEL}":1,"{SENTINEL}":2}}"#),
        ),
    ];
    let mut fixed: Option<String> = None;
    for (shape, frame) in &shapes {
        let display = assert_refused(Header::parse_session_resumable(frame), shape);
        match &fixed {
            None => fixed = Some(display),
            Some(first) => assert_eq!(
                *first, display,
                "{shape} refusal must equal the fixed error"
            ),
        }
    }
}

#[test]
fn session_flag_refusal_is_deterministic() {
    let frame = r#"{"op":9,"d":true,"d":false}"#;
    let first = assert_refused(Header::parse_session_resumable(frame), "repeat call one");
    let second = assert_refused(Header::parse_session_resumable(frame), "repeat call two");
    assert_eq!(first, second, "the same frame refuses identically");
}

#[test]
fn baseline_adapter_documents_current_last_wins() {
    // Legacy control: passes before AND after GREEN by construction (the
    // qualified call always hits the trait mirror), and keeps the adapter
    // live so clippy never reports it unused. Documents the baseline's
    // order-dependent resume input that the refusal removes.
    assert_eq!(
        <Header as BaselineSessionFlag>::parse_session_resumable(r#"{"op":9,"d":true,"d":false}"#),
        Ok(false),
        "baseline last-wins: true first resumes false"
    );
    assert_eq!(
        <Header as BaselineSessionFlag>::parse_session_resumable(r#"{"op":9,"d":false,"d":true}"#),
        Ok(true),
        "baseline last-wins: false first resumes true"
    );
}

#[test]
fn valid_session_flag_parses_byte_identically() {
    assert_eq!(
        Header::parse_session_resumable(r#"{"op":9,"d":true}"#),
        Ok(true)
    );
    assert_eq!(
        Header::parse_session_resumable(r#"{"op":9,"d":false}"#),
        Ok(false)
    );

    // Pre-existing error paths keep non-duplicate errors: refusal never
    // swallows a missing flag, a mistyped flag, or malformed JSON.
    for (frame, label) in [
        (r#"{"op":9}"#.to_owned(), "missing d"),
        (r#"{"op":9,"d":"yes"}"#.to_owned(), "non-bool d"),
        ("not json".to_owned(), "malformed frame"),
    ] {
        match Header::parse_session_resumable(&frame) {
            Ok(resumable) => panic!("{label} must fail, got resumable={resumable}"),
            Err(error) => assert_not_duplicate_refusal(&error, label),
        }
    }
}

#[test]
fn header_duplicate_fields_are_refused() {
    // Premise pin (GREEN on the baseline): serde's derived struct decoding
    // already refuses duplicate fields, so the builder leaves `:1321` alone.
    for (frame, label) in [
        (r#"{"op":9,"op":0,"s":5}"#, "op conflicting, 9 first"),
        (r#"{"op":0,"op":9,"s":5}"#, "op conflicting, 0 first"),
        (r#"{"op":0,"op":0,"s":5}"#, "op identical"),
        (r#"{"op":0,"s":1,"s":2}"#, "s conflicting"),
        (r#"{"op":0,"s":1,"s":1}"#, "s identical"),
    ] {
        assert!(
            serde_json::from_str::<Header>(frame).is_err(),
            "{label}: duplicate header field must be refused"
        );
    }
    let header: Header = serde_json::from_str(r#"{"op":0,"s":7}"#).expect("valid header parses");
    assert_eq!(header.op, 0);
    assert_eq!(header.s, Some(7));
}

#[test]
fn hello_duplicate_fields_are_refused() {
    // Premise pin (GREEN on the baseline): serde's derived struct decoding
    // already refuses duplicate fields, so the builder leaves `:1323` alone.
    for (frame, label) in [
        (
            r#"{"d":{"heartbeat_interval":100},"d":{"heartbeat_interval":200}}"#,
            "d conflicting",
        ),
        (
            r#"{"d":{"heartbeat_interval":100,"heartbeat_interval":200}}"#,
            "heartbeat_interval conflicting",
        ),
        (
            r#"{"d":{"heartbeat_interval":100,"heartbeat_interval":100}}"#,
            "heartbeat_interval identical",
        ),
    ] {
        assert!(
            serde_json::from_str::<HelloPacket>(frame).is_err(),
            "{label}: duplicate hello field must be refused"
        );
    }
    let hello: HelloPacket =
        serde_json::from_str(r#"{"d":{"heartbeat_interval":45000}}"#).expect("valid hello parses");
    assert_eq!(hello.d.heartbeat_interval, 45000);
}

#[test]
fn header_ignores_nested_duplicate_inside_dispatch_body() {
    // Boundary pin: duplicates nested inside an op-0 dispatch body belong to
    // `twilight_gateway` (library scope), not to the header parse. The header
    // keeps ignoring them before and after the fix.
    let header: Header = serde_json::from_str(r#"{"op":0,"s":1,"d":{"x":1,"x":2}}"#)
        .expect("header ignores dispatch-body keys");
    assert_eq!(header.op, 0);
    assert_eq!(header.s, Some(1));
}
