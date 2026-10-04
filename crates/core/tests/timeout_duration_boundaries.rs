//! Timeout `duration_seconds` boundary pins (offline, test-only).
//!
//! The wire validator (`validate_timeout_duration`) and the service validator
//! (`validate_member_request` with `ModerationAction::Timeout`) both refuse
//! out-of-range input — they never clamp. This suite pins the exact Discord
//! ceiling literals, the interior edges, the integer extremes, and
//! duration-shaped malformed strings, plus the no-echo error-text rule.
//! Synthetic fixtures only: no Discord, network, or database.

use serde_json::json;
use two_bot_core::commands::{TIMEOUT_DURATION_MAX_SECONDS, TIMEOUT_DURATION_MIN_SECONDS};
use two_bot_core::member_moderation::{validate_member_request, MemberError};
use two_bot_core::moderation::validate_timeout_duration;
use two_bot_core::{ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget};

const MIN: i64 = 60;
const MAX: i64 = 2_419_200; // 28 days, Discord's timeout ceiling.

fn actor() -> ModerationActor {
    ModerationActor {
        user_id: "111111111111111111".to_owned(),
        role_ids: vec![],
        highest_role_position: 50,
        permissions: u64::MAX,
    }
}

fn target() -> ModerationTarget {
    ModerationTarget {
        user_id: "333333333333333333".to_owned(),
        role_ids: vec![],
        highest_role_position: 10,
        is_bot: false,
        is_guild_owner: false,
    }
}

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: "123456789012345678".to_owned(),
        bot_user_id: Some("555555555555555555".to_owned()),
        protected_role_ids: std::collections::HashSet::new(),
    }
}

fn validate_timeout(duration_seconds: Option<i64>) -> Result<u64, MemberError> {
    validate_member_request(
        ModerationAction::Timeout,
        &policy(),
        &actor(),
        Some(&target()),
        Some(100),
        "spam",
        duration_seconds,
    )
    .map(|req| {
        req.duration_seconds
            .expect("timeout always bounds a duration")
    })
}

fn assert_malformed(err: MemberError, raw: Option<i64>) {
    let MemberError::Malformed { field, message } = err else {
        panic!("timeout {raw:?} must refuse as malformed, got {err:?}");
    };
    assert_eq!(field, "duration_seconds");
    // The message is a static format string that names both bounds and never
    // interpolates the caller-supplied value, so exact equality pins no-echo
    // for every input at once.
    assert_eq!(
        message,
        "\"duration_seconds\" must be an integer between 60 and 2419200"
    );
}

#[test]
fn timeout_bounds_match_the_advertised_constants() {
    assert_eq!(TIMEOUT_DURATION_MIN_SECONDS, MIN);
    assert_eq!(TIMEOUT_DURATION_MAX_SECONDS, MAX);
    assert_eq!(TIMEOUT_DURATION_MAX_SECONDS, 28 * 24 * 60 * 60);
    assert_eq!(
        (
            two_bot_core::member_moderation::MIN_DURATION_SECONDS,
            two_bot_core::member_moderation::MAX_TIMEOUT_SECONDS
        ),
        (MIN, MAX),
        "wire and service layers must agree on the timeout window"
    );
}

#[test]
fn timeout_accepts_every_edge_of_the_window() {
    // Exact literals: the floor, the Discord ceiling, and the interior edges.
    for n in [MIN, MIN + 1, MAX - 1, MAX] {
        assert_eq!(
            validate_timeout(Some(n)),
            Ok(n as u64),
            "timeout {n} must accept with its value intact"
        );
        let wire = json!(n);
        assert_eq!(
            validate_timeout_duration(Some(&wire)),
            Ok(n),
            "wire timeout {n} must accept"
        );
    }
}

#[test]
fn timeout_refuses_outside_the_window_without_clamping() {
    // Adjacent outsiders AND far extremes refuse: the validators narrow to
    // the window by refusing, never by silently clamping to an edge.
    for bad in [
        Some(MIN - 1),
        Some(0),
        Some(-1),
        Some(-60),
        Some(MAX + 1),
        Some(i64::MAX),
        Some(i64::MIN),
    ] {
        assert_malformed(
            validate_timeout(bad).expect_err("out-of-window timeout must refuse"),
            bad,
        );
        let wire = json!(bad.expect("some"));
        assert!(
            validate_timeout_duration(Some(&wire)).is_err(),
            "wire timeout {bad:?} must refuse"
        );
    }
    // A missing duration refuses on both layers.
    assert_malformed(
        validate_timeout(None).expect_err("missing timeout duration must refuse"),
        None,
    );
    assert!(validate_timeout_duration(None).is_err());
    // max+1 refuses rather than clamping down to max; min-1 refuses rather
    // than clamping up to min.
    assert!(validate_timeout(Some(MAX + 1)).is_err());
    assert!(validate_timeout(Some(MIN - 1)).is_err());
}

#[test]
fn timeout_refuses_duration_shaped_strings() {
    // Human duration spellings and padded/embellished numbers never coerce
    // the way a loose `Number()` cast would; the wire validator takes
    // integers only.
    for raw in [
        "1h",
        "10m",
        "1d",
        "60s",
        "1h30m",
        "",
        "   ",
        " 60 ",
        "+60",
        "-60",
        "0x3C",
        "60.0",
        "2_419_200",
        "2419200",
        "9999999999999999999999",
    ] {
        let value = json!(raw);
        assert!(
            validate_timeout_duration(Some(&value)).is_err(),
            "duration-shaped string {raw:?} must refuse"
        );
    }
    // Whole-number floats refuse too: JSON integers only, no float path.
    assert!(validate_timeout_duration(Some(&json!(60.0))).is_err());
    // Positive integers above i64 range are not integers the validator can
    // hold, so they refuse rather than wrapping.
    let overflow = serde_json::from_str::<serde_json::Value>("9223372036854775808")
        .expect("u64-range literal parses");
    assert!(validate_timeout_duration(Some(&overflow)).is_err());
}

#[test]
fn timeout_bounds_do_not_leak_onto_other_verbs() {
    // A timeout-sized duration on a verb that takes none is ignored, not
    // validated: bounds belong to the timeout path only.
    let req = validate_member_request(
        ModerationAction::Ban,
        &policy(),
        &actor(),
        Some(&target()),
        Some(100),
        "spam",
        Some(MAX),
    )
    .expect("ban ignores duration_seconds");
    assert_eq!(req.duration_seconds, None);
}
