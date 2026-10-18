//! Regression tests mirroring the `helix_subscription_listing` fuzz oracle.
//!
//! These are the offline, deterministic pins for the fuzz-found input shapes:
//! every malformed listing envelope the fuzzer feeds at
//! [`two_bot_core::twitch_helix::validate_subscription_listing_page`] must
//! fail with the bounded [`two_bot_core::twitch_helix::HelixError::BadResponse`]
//! static literal (never response bytes), and every valid shape must validate
//! and parse deterministically. Pure and sync: no transport, no network, no
//! credentials. The async transport-level contract (fail the whole listing,
//! zero mutations, retry and 409-confirmation paths) stays pinned by
//! `helix_subscription_listing_envelope_acceptance.rs`.

use std::collections::HashSet;

use serde_json::{json, Value};
use two_bot_core::twitch_eventsub::parse_subscriptions;
use two_bot_core::twitch_helix::{validate_subscription_listing_page, HelixError};

/// Marker embedded in hostile payloads; errors must never echo it.
const PAYLOAD_SENTINEL: &str = "PAYLOAD-SENTINEL-7f3a9c";

fn valid_entry(id: &str, broadcaster: &str) -> Value {
    json!({
        "id": id,
        "status": "enabled",
        "type": "stream.online",
        "version": "1",
        "condition": {"broadcaster_user_id": broadcaster},
        "transport": {
            "method": "webhook",
            "callback": "https://hooks.example.com/twitch",
        },
    })
}

/// The error contract for one malformed page: the bounded `BadResponse`
/// variant carrying exactly `expected`, with no payload bytes in either
/// rendering.
fn assert_bounded_envelope_error(error: &HelixError, expected: &str, shape: &str) {
    assert!(
        matches!(error, HelixError::BadResponse(message) if *message == expected),
        "{shape}: must fail with the static literal, got: {error:?}"
    );
    assert_eq!(
        format!("{error}"),
        format!("helix response was not usable: {expected}"),
        "{shape}: display must be the static literal"
    );
    for rendered in [format!("{error}"), format!("{error:?}")] {
        assert!(
            !rendered.contains(PAYLOAD_SENTINEL),
            "{shape}: error must not echo response bytes: {rendered}"
        );
    }
}

#[test]
fn fuzz_shapes_valid_pages_validate_and_parse_deterministically() {
    let pages: Vec<(&str, Value, Option<&str>)> = vec![
        ("bare empty data", json!({"data": []}), None),
        (
            "empty data with pagination",
            json!({"data": [], "pagination": {}}),
            None,
        ),
        (
            "null pagination is terminal",
            json!({"data": [], "pagination": null}),
            None,
        ),
        (
            "non-object pagination carries no cursor",
            json!({"data": [], "pagination": "not-an-object"}),
            None,
        ),
        (
            "null cursor is terminal",
            json!({"data": [], "pagination": {"cursor": null}}),
            None,
        ),
        (
            "single entry",
            json!({"data": [valid_entry("s1", "111")]}),
            None,
        ),
        (
            "cursor page",
            json!({
                "data": [valid_entry("s1", "111")],
                "pagination": {"cursor": "cursor-1"},
            }),
            Some("cursor-1"),
        ),
    ];
    for (shape, document, expected_cursor) in pages {
        let cursor = validate_subscription_listing_page(&document)
            .expect("{shape}: valid page must validate");
        assert_eq!(
            cursor.as_deref(),
            expected_cursor,
            "{shape}: cursor extraction"
        );
        let first = parse_subscriptions(&document);
        assert_eq!(
            first,
            parse_subscriptions(&document),
            "{shape}: deterministic"
        );
        assert!(
            first.len() <= document["data"].as_array().map_or(0, Vec::len),
            "{shape}: parser only filters entries"
        );
    }
}

#[test]
fn fuzz_shapes_malformed_data_envelopes_fail_bounded() {
    let shapes: Vec<(&str, Value)> = vec![
        ("empty object (missing data)", json!({})),
        ("null top level", json!(null)),
        ("array top level", json!([])),
        ("string top level", json!(PAYLOAD_SENTINEL)),
        ("number top level", json!(42)),
        ("boolean top level", json!(true)),
        ("data is null", json!({"data": null})),
        ("data is a string", json!({"data": PAYLOAD_SENTINEL})),
        ("data is a number", json!({"data": 42})),
        ("data is a boolean", json!({"data": false})),
        (
            "data is an object",
            json!({"data": {"leak": PAYLOAD_SENTINEL}}),
        ),
        (
            "pagination without data",
            json!({"pagination": {"cursor": "c1"}}),
        ),
    ];
    for (shape, document) in shapes {
        let error = validate_subscription_listing_page(&document)
            .expect_err("{shape}: malformed envelope must not validate");
        assert_bounded_envelope_error(&error, "subscription listing has no data array", shape);
    }
}

/// Deep nesting fails closed without panic: a `data` object nested far past
/// any reasonable depth is still a non-array envelope, hostile nesting inside
/// a valid `data` array only filters entries, and bytes nested past
/// `serde_json`'s recursion limit are a malformed page.
#[test]
fn fuzz_shapes_deeply_nested_envelopes_fail_closed_or_filter() {
    // A `data` object nested 200 levels deep is still not a `data` array.
    // Pre-fix this read as an empty list; now it is a bounded error.
    let mut nested: Value = json!(1);
    for _ in 0..200 {
        nested = json!({"nest": nested});
    }
    let document = json!({"data": nested});
    let error = validate_subscription_listing_page(&document)
        .expect_err("deeply nested object data must not validate");
    assert_bounded_envelope_error(
        &error,
        "subscription listing has no data array",
        "deeply nested object data",
    );

    // Hostile nesting inside a valid `data` array only filters entries: the
    // envelope stays valid and the listing accumulates nothing from it.
    let mut hostile: Value = json!("leaf");
    for _ in 0..50 {
        hostile = json!([hostile]);
    }
    let document = json!({"data": [hostile], "pagination": {}});
    let cursor = validate_subscription_listing_page(&document)
        .expect("valid envelope with nested entries must validate");
    assert_eq!(cursor, None, "no cursor in the terminal page");
    assert!(
        parse_subscriptions(&document).is_empty(),
        "nested arrays are not subscriptions and must filter out"
    );

    // Bytes nested past the JSON recursion limit are a malformed page, never a
    // panic: the multi-page driver reports `BadResponse`. Arrays nest without
    // a key so depth alone triggers the parser's recursion limit.
    let deep = format!("{{\"data\": {}1{}}}", "[".repeat(300), "]".repeat(300));
    let error = run_listing(&[deep.as_bytes()])
        .expect_err("over-nested bytes must fail as a malformed page");
    assert!(
        matches!(error, HelixError::BadResponse(_)),
        "over-nested bytes must fail closed with BadResponse, got: {error:?}"
    );
}

#[test]
fn fuzz_shapes_unusable_cursors_fail_bounded() {
    for (shape, cursor) in [
        ("empty-string cursor", json!("")),
        ("number cursor", json!(42)),
        ("boolean cursor", json!(false)),
        ("array cursor", json!([])),
        ("object cursor", json!({})),
    ] {
        let document = json!({"data": [], "pagination": {"cursor": cursor}});
        let error = validate_subscription_listing_page(&document)
            .expect_err("{shape}: unusable cursor must not validate");
        assert_bounded_envelope_error(
            &error,
            "subscription pagination cursor is not usable",
            shape,
        );
    }
}

/// Drive NUL-joined multi-page bytes through the same split the fuzz target
/// uses: two valid pages accumulate, a malformed later page rejects the whole
/// listing, and a repeated cursor is refused.
fn run_listing(pages: &[&[u8]]) -> Result<Vec<String>, HelixError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut accumulated = Vec::new();
    for page in pages {
        let document: Value = serde_json::from_slice(page)
            .map_err(|_| HelixError::BadResponse("subscription listing is not JSON"))?;
        let cursor = validate_subscription_listing_page(&document)?;
        accumulated.extend(
            parse_subscriptions(&document)
                .into_iter()
                .map(|subscription| subscription.id),
        );
        match cursor {
            None => return Ok(accumulated),
            Some(next) => {
                if !seen.insert(next) {
                    return Err(HelixError::BadResponse(
                        "subscription pagination cursor repeated",
                    ));
                }
            }
        }
    }
    Ok(accumulated)
}

#[test]
fn fuzz_shapes_multipage_bytes_accumulate_or_reject_whole() {
    let first = json!({
        "data": [valid_entry("first", "111")],
        "pagination": {"cursor": "cursor-1"},
    })
    .to_string()
    .into_bytes();
    let second = json!({
        "data": [valid_entry("second", "222")],
        "pagination": {},
    })
    .to_string()
    .into_bytes();

    // Two valid pages accumulate across the NUL split.
    let joined = [first.as_slice(), second.as_slice()].join(b"\x00".as_slice());
    let pages: Vec<&[u8]> = joined.split(|byte| *byte == 0x00).collect();
    assert_eq!(pages.len(), 2);
    assert_eq!(
        run_listing(&pages).expect("two valid pages must accumulate"),
        vec!["first".to_owned(), "second".to_owned()]
    );

    // A malformed later page rejects the whole listing, never partial success.
    let malformed = br#"{"data":"not-an-array"}"#.as_slice();
    let error = run_listing(&[first.as_slice(), malformed])
        .expect_err("a malformed later page must reject the listing, not truncate it");
    assert_bounded_envelope_error(
        &error,
        "subscription listing has no data array",
        "later page",
    );

    // A repeated cursor is refused rather than looped.
    let error = run_listing(&[first.as_slice(), first.as_slice()])
        .expect_err("a repeated cursor must fail closed");
    assert_bounded_envelope_error(
        &error,
        "subscription pagination cursor repeated",
        "repeated cursor",
    );

    // Non-JSON bytes are a malformed page, not a panic.
    let error = run_listing(&[b"\xff\xfe not json".as_slice()])
        .expect_err("non-JSON bytes must fail as a malformed page");
    assert!(matches!(error, HelixError::BadResponse(_)));
}
