#![no_main]

//! Fuzz the Helix subscription-listing envelope validator: the fail-closed
//! gate [`validate_subscription_listing_page`] plus the lenient per-entry
//! [`parse_subscriptions`] it guards, over arbitrary bytes.
//!
//! Input structure: the raw fuzzer bytes are split on `0x00` (NUL) into pages.
//! Raw JSON never contains a literal NUL byte (it is escaped as `\u0000`), so
//! the split preserves every page's bytes while letting unguided mutations
//! discover multi-page accumulation by inserting NULs. An input without NULs
//! is a single page. Each page is decoded as JSON, run through the production
//! validator, and — when envelope-valid — through the production per-entry
//! parser, mirroring the accumulation loop in `list_subscriptions`
//! (cursor chaining, repeated-cursor refusal, terminal pages).
//!
//! Ground truth: an independent oracle recomputes the expected envelope
//! verdict from the decoded JSON with a different access pattern, so a mutant
//! that widens or narrows the production check fails here. Malformed
//! envelopes must fail with the bounded `HelixError::BadResponse` carrying
//! one of the two static literals — never response bytes. Valid pages must
//! parse deterministically into at most their `data` length.
//!
//! Bounds: inputs are capped at twice [`HELIX_MAX_BODY_BYTES`] (so an
//! oversized-response seed still exercises the parser), pages per iteration
//! at 8 (the 100-page production limit is pinned by the offline acceptance
//! test, not by fuzz iterations), entries parsed per page at 1024 and total
//! accumulation at 4096. No network, database, credentials or secrets.

use std::collections::HashSet;
use std::sync::Once;

use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use two_bot_core::twitch_eventsub::parse_subscriptions;
use two_bot_core::twitch_helix::{validate_subscription_listing_page, HELIX_MAX_BODY_BYTES};

/// Keep each decode bounded while allowing a body above the transport cap so
/// an oversized-response corpus seed exercises the parser itself.
const MAX_INPUT_BYTES: usize = HELIX_MAX_BODY_BYTES * 2;
/// Pages simulated per iteration; the production 100-page limit is pinned by
/// the offline acceptance test.
const MAX_FUZZ_PAGES: usize = 8;
/// Per-page entry-parse cap; larger `data` arrays still validate the envelope
/// but skip full per-entry parsing to bound iteration time.
const MAX_ENTRIES_PER_PAGE: usize = 1024;
/// Total accumulation cap across the pages of one iteration.
const MAX_TOTAL_ENTRIES: usize = 4096;
/// Page separator: never appears raw in JSON text.
const PAGE_SEPARATOR: u8 = 0x00;
/// Static envelope verdicts; the only messages a malformed page may produce.
const NO_DATA_ARRAY: &str = "subscription listing has no data array";
const CURSOR_NOT_USABLE: &str = "subscription pagination cursor is not usable";

/// Independent envelope oracle: same contract as
/// [`validate_subscription_listing_page`], recomputed with explicit matching
/// rather than the production combinator chain so a one-sided mutant fails.
fn expected_envelope(document: &Value) -> Result<Option<String>, &'static str> {
    let data_is_array = match document {
        Value::Object(map) => match map.get("data") {
            Some(Value::Array(_)) => true,
            _ => false,
        },
        _ => false,
    };
    if !data_is_array {
        return Err(NO_DATA_ARRAY);
    }
    let pagination = match document {
        Value::Object(map) => map.get("pagination"),
        _ => None,
    };
    let Some(pagination) = pagination else {
        return Ok(None);
    };
    let cursor = match pagination {
        Value::Object(map) => map.get("cursor"),
        _ => None,
    };
    match cursor {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(CURSOR_NOT_USABLE),
    }
}

/// Assert a malformed page fails with exactly the oracle's static literal and
/// that neither rendering echoes response bytes.
fn assert_bounded_envelope_error(
    error: &two_bot_core::twitch_helix::HelixError,
    expected: &'static str,
    page: &[u8],
) {
    let rendered = format!("{error}");
    let debug = format!("{error:?}");
    assert_eq!(
        rendered,
        format!("helix response was not usable: {expected}"),
        "envelope error must be the static literal"
    );
    assert!(
        debug.contains(expected),
        "debug rendering must carry the static literal"
    );
    // The error carries no page bytes: the only variable content it could
    // echo is excluded by construction, pinned here against long page-unique
    // substrings (short pages may coincide with the literal by chance).
    // Skip probes that coincide with either static rendering: Debug wraps the
    // literal in quotes, so a JSON string page starting with `"subscription`
    // would otherwise read as echoed bytes for correct behavior.
    if page.len() > 64 {
        let probe = &page[..32];
        if let Ok(probe) = std::str::from_utf8(probe) {
            if probe.len() >= 8
                && !expected.contains(probe)
                && !rendered.contains(probe)
                && !debug.contains(probe)
            {
                assert!(
                    !rendered.contains(probe) && !debug.contains(probe),
                    "envelope error must not echo response bytes"
                );
            }
        }
    }
}

fn valid_entry(id: &str, broadcaster: &str) -> Value {
    serde_json::json!({
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

/// Input-independent pins: every envelope shape the contract names validates
/// or refuses exactly as documented, multi-page accumulation matches the list
/// path, and error renderings never echo a payload sentinel. Any
/// envelope-validation change must update this target on its own card; silent
/// drift fails the fuzzer instead. Runs once per campaign.
fn check_fixed_matrix() {
    // Valid terminal pages.
    for document in [
        serde_json::json!({"data": []}),
        serde_json::json!({"data": [], "pagination": {}}),
        serde_json::json!({"data": [], "pagination": null}),
        serde_json::json!({"data": [], "pagination": "not-an-object"}),
        serde_json::json!({"data": [], "pagination": {"cursor": null}}),
        serde_json::json!({"data": [valid_entry("s1", "111")]}),
    ] {
        assert_eq!(
            expected_envelope(&document).expect("oracle accepts valid page"),
            validate_subscription_listing_page(&document).expect("validator accepts valid page"),
            "valid page {document}"
        );
        let first = parse_subscriptions(&document);
        assert_eq!(first, parse_subscriptions(&document));
        assert!(first.len() <= document["data"].as_array().map_or(0, Vec::len));
    }

    // Valid cursor page.
    let next = serde_json::json!({
        "data": [valid_entry("s1", "111")],
        "pagination": {"cursor": "cursor-1"},
    });
    assert_eq!(
        validate_subscription_listing_page(&next).expect("cursor page validates"),
        Some("cursor-1".to_owned())
    );

    // Malformed envelopes: every non-array `data` shape and every unusable
    // cursor shape the acceptance contract names.
    for (document, expected) in [
        (serde_json::json!({}), NO_DATA_ARRAY),
        (serde_json::json!(null), NO_DATA_ARRAY),
        (serde_json::json!([]), NO_DATA_ARRAY),
        (serde_json::json!("PAYLOAD-SENTINEL-7f3a9c"), NO_DATA_ARRAY),
        (serde_json::json!(42), NO_DATA_ARRAY),
        (serde_json::json!(true), NO_DATA_ARRAY),
        (serde_json::json!({"data": null}), NO_DATA_ARRAY),
        (
            serde_json::json!({"data": "PAYLOAD-SENTINEL-7f3a9c"}),
            NO_DATA_ARRAY,
        ),
        (serde_json::json!({"data": 42}), NO_DATA_ARRAY),
        (serde_json::json!({"data": false}), NO_DATA_ARRAY),
        (
            serde_json::json!({"data": {"leak": "PAYLOAD-SENTINEL-7f3a9c"}}),
            NO_DATA_ARRAY,
        ),
        (
            serde_json::json!({"pagination": {"cursor": "c1"}}),
            NO_DATA_ARRAY,
        ),
        (
            serde_json::json!({"data": [], "pagination": {"cursor": ""}}),
            CURSOR_NOT_USABLE,
        ),
        (
            serde_json::json!({"data": [], "pagination": {"cursor": 42}}),
            CURSOR_NOT_USABLE,
        ),
        (
            serde_json::json!({"data": [], "pagination": {"cursor": false}}),
            CURSOR_NOT_USABLE,
        ),
        (
            serde_json::json!({"data": [], "pagination": {"cursor": []}}),
            CURSOR_NOT_USABLE,
        ),
        (
            serde_json::json!({"data": [], "pagination": {"cursor": {}}}),
            CURSOR_NOT_USABLE,
        ),
    ] {
        assert_eq!(expected_envelope(&document), Err(expected), "{document}");
        let error =
            validate_subscription_listing_page(&document).expect_err("validator rejects malformed");
        assert!(matches!(
            error,
            two_bot_core::twitch_helix::HelixError::BadResponse(message) if message == expected
        ));
        for rendered in [format!("{error}"), format!("{error:?}")] {
            assert!(
                !rendered.contains("PAYLOAD-SENTINEL-7f3a9c"),
                "error must not echo payload bytes: {rendered}"
            );
        }
    }

    // Multi-page accumulation: valid pages chain through cursors, a malformed
    // later page rejects the whole listing (never partial success), and a
    // repeated cursor is refused.
    let first = serde_json::json!({
        "data": [valid_entry("first", "111")],
        "pagination": {"cursor": "cursor-1"},
    });
    let second = serde_json::json!({
        "data": [valid_entry("second", "222")],
        "pagination": {},
    });
    let mut seen = HashSet::new();
    let mut accumulated = Vec::new();
    let cursor = validate_subscription_listing_page(&first).expect("first page valid");
    accumulated.extend(parse_subscriptions(&first));
    let cursor = cursor.expect("first page carries a cursor");
    assert!(seen.insert(cursor));
    let terminal = validate_subscription_listing_page(&second).expect("second page valid");
    accumulated.extend(parse_subscriptions(&second));
    assert_eq!(terminal, None);
    assert_eq!(accumulated.len(), 2);
    assert!(validate_subscription_listing_page(&serde_json::json!({})).is_err());
    assert!(
        !seen.insert("cursor-1".to_owned()),
        "repeat must be detected"
    );
}

fuzz_target!(|data: &[u8]| {
    static MATRIX: Once = Once::new();
    MATRIX.call_once(check_fixed_matrix);

    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    // Split into pages on NUL; cap the page count to bound iterations.
    let mut pages: Vec<&[u8]> = data.split(|byte| *byte == PAGE_SEPARATOR).collect();
    if pages.len() > MAX_FUZZ_PAGES {
        pages.truncate(MAX_FUZZ_PAGES);
    }

    // One pass mirroring `list_subscriptions`: each page decodes as JSON,
    // validates through the production envelope gate (checked against the
    // oracle), accumulates through the production per-entry parser, and
    // chains through cursors. Any malformed page rejects the whole listing,
    // never truncating to partial success.
    let mut seen_cursors: HashSet<String> = HashSet::new();
    let mut accumulated: usize = 0;

    for page in pages {
        // Non-JSON bytes are a malformed page in production terms
        // (`BadResponse("subscription listing is not JSON")`): no panic, no
        // hang, and the listing fails rather than returning partial pages.
        let Ok(document) = serde_json::from_slice::<Value>(page) else {
            break;
        };

        // The validator and the oracle must agree exactly.
        let expected = expected_envelope(&document);
        let actual = validate_subscription_listing_page(&document);
        let cursor = match (&expected, &actual) {
            (Ok(expected_cursor), Ok(actual_cursor)) => {
                assert_eq!(expected_cursor, actual_cursor, "page {document}");
                actual_cursor.clone()
            }
            (Err(expected_message), Err(error)) => {
                assert!(
                    matches!(
                        error,
                        two_bot_core::twitch_helix::HelixError::BadResponse(message)
                            if message == expected_message
                    ),
                    "page {document}"
                );
                assert_bounded_envelope_error(error, expected_message, page);
                // A malformed page rejects the whole listing: stop
                // accumulating, mirroring the list path's early return.
                break;
            }
            (expected, actual) => {
                panic!("validator/oracle disagreement on {document}: {expected:?} vs {actual:?}");
            }
        };

        // Envelope-valid pages accumulate through the real per-entry parser,
        // bounded so adversarial `data` arrays cannot OOM or hang the
        // iteration. Oversized arrays still validated the envelope above;
        // only the per-entry walk is skipped.
        let data_len = document
            .get("data")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if data_len <= MAX_ENTRIES_PER_PAGE && accumulated <= MAX_TOTAL_ENTRIES {
            let first = parse_subscriptions(&document);
            assert_eq!(
                first,
                parse_subscriptions(&document),
                "parser deterministic"
            );
            assert!(
                first.len() <= data_len,
                "parser only filters entries, never invents them"
            );
            accumulated += first.len();
        }

        match cursor {
            None => break,
            Some(next) => {
                if !seen_cursors.insert(next) {
                    // Repeated cursor: production refuses rather than looping.
                    break;
                }
            }
        }
    }
    assert!(accumulated <= MAX_TOTAL_ENTRIES + MAX_ENTRIES_PER_PAGE);
});
