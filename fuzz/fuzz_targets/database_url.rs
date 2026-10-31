#![no_main]

//! Hostile-input hold for database-URL validation.
//!
//! `validate` runs before SQLx ever sees the URL: SQLx 0.9 WARN-logs both
//! key and value for unrecognized query parameters, so an allowlist miss
//! must refuse before the driver parses. `connect_options` keeps that order
//! (`validate` first, then the passfile-silenced `FromStr`) and maps every
//! failure to a fixed `InvalidArgument` message, so connection secrets can
//! never ride error text into logs.
//!
//! Properties (no panic/hang, refusal before SQLx, fixed vocabulary):
//! - Arbitrary UTF-8 strings go through both `validate` and
//!   `connect_options`. Invalid UTF-8 is discarded, matching the other
//!   `&str` harnesses.
//! - Input over 64 KiB is bounded by the documented `-max_len=65536`
//!   campaign flag; each iteration does exactly one `validate` plus one
//!   `connect_options` parse, so allocation stays O(input).
//! - `validate` errors are exactly the two fixed `&'static str` values;
//!   exact equality (not a substring check) proves failing input is never
//!   echoed.
//! - `connect_options` only ever fails as `InvalidArgument` carrying one of
//!   those same two fixed strings (compared through its `Debug` rendering so
//!   this target needs no direct SQLx dependency): an `unsupported` refusal
//!   propagates the pre-parse refusal (never a driver parse), and any
//!   driver-parse failure maps to the fixed invalid-URL message. No other
//!   error variant may escape, and both seams are deterministic across
//!   repeated calls.
//! - No database, network, Discord client or real secret is touched; only
//!   the two synchronous pure-parse seams run. Seeds are synthetic.

use libfuzzer_sys::fuzz_target;
use two_bot_core::database_url::{connect_options, validate};

/// Campaign bound (`-max_len=65536`): one bounded parse per seam per
/// iteration keeps hostile inputs from becoming a hang.
const MAX_INPUT_BYTES: usize = 65536;

/// Fixed vocabulary from `crates/core/src/database_url.rs`. Exact-match
/// assertions below pin this set; any new error string must update this
/// target on its own card.
const INVALID_URL: &str = "invalid database URL";
const UNSUPPORTED_PARAM: &str = "unsupported database URL parameter";

/// Exact `InvalidArgument` rendering for a fixed message. Comparing through
/// `Debug` pins both the variant and the message without a direct SQLx
/// dependency; any new variant or interpolated message fails the fuzzer.
fn expected_debug(message: &str) -> String {
    format!("InvalidArgument({message:?})")
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    // Database URLs arrive as UTF-8 configuration strings. Invalid UTF-8 is
    // discarded, matching the other `&str` harnesses.
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };

    // Input-independent base cases: the happy path still validates, the
    // allowlist miss still refuses with the fixed string, and garbage still
    // refuses with the fixed invalid-URL string. Any seam behavior change
    // must update this target on its own card; silent drift fails the
    // fuzzer instead.
    assert_eq!(
        validate("postgres://agent-testdb/db?sslmode=require"),
        Ok(())
    );
    assert_eq!(
        validate("postgres://agent-testdb/db?api_key=fixture-query-secret"),
        Err(UNSUPPORTED_PARAM)
    );
    assert_eq!(validate("not a database url"), Err(INVALID_URL));
    assert_eq!(validate(""), Err(INVALID_URL));
    assert!(connect_options("postgres://agent-testdb/db?sslmode=require").is_ok());

    // Seam 1: fixed vocabulary. Exact equality proves no input bytes escape
    // into the error; a future interpolated message fails here first.
    let validated = validate(raw);
    match validated {
        Ok(()) => {}
        Err(message) => assert!(
            message == INVALID_URL || message == UNSUPPORTED_PARAM,
            "validate escaped a non-fixed error string"
        ),
    }

    // Seam 2: refusal before SQLx, still fixed vocabulary. `connect_options`
    // validates before the driver parses, so an unsupported key can never
    // reach the driver's WARN log; the propagated message proves the order.
    let connected = connect_options(raw);
    let connected_ok = connected.is_ok();
    match validated {
        Err(expected) => {
            let error = connected.expect_err("validate refusal must refuse connect_options");
            assert_eq!(
                format!("{error:?}"),
                expected_debug(expected),
                "validate refusal must propagate before the driver parses"
            );
        }
        Ok(()) => match connected {
            Ok(_) => {}
            Err(error) => assert_eq!(
                format!("{error:?}"),
                expected_debug(INVALID_URL),
                "driver-parse failure must stay the fixed invalid-URL message"
            ),
        },
    }

    // Determinism: repeated parses agree, so no hidden state or
    // input-dependent branching escapes the fixed contract.
    assert_eq!(validate(raw).map_err(|message| message), validated);
    assert_eq!(
        connect_options(raw).is_ok(),
        connected_ok,
        "connect_options must parse deterministically"
    );
});
