//! MEE6 intake bounds: hostile exports fail closed with bounded errors
//! (failing first, tests only).
//!
//! Base: `164343832156c3172dbbcfddd53892353cae23a5` (main at acceptance
//! authoring). At that base the MEE6 parsers accept any player count and
//! accumulate one problem per malformed row with no bound: a 180 000-row
//! export parses to a 180 000-row vec and a 200 000-row hostile export fails
//! with a 200 000-line error. The hostile tests are RED on that base for
//! exactly that reason, while the valid-import controls stay green. The paired
//! builder implements production code only and must NOT edit this file; the
//! tests pass unmodified once parsing enforces the contract below. No failing
//! test merges: this file is handed off unpublished for the builder's final
//! green implementation pull request. (CLI byte caps are covered by
//! `untrusted_file_intake_cap_acceptance.rs`, the staging manifest by
//! `staging_manifest_cap_acceptance.rs`.)
//!
//! ## Builder contract
//!
//! - Every hostile fixture below is under the 8 MiB file cap (each test
//!   self-checks its byte size), so a correct implementation refuses them via
//!   a player/problem bound, not the byte cap. Any player bound above ~200 000
//!   would be meaningless — the byte cap admits at most ~220 000 minimal
//!   rows — so the fixtures exceed every meaningful bound.
//! - Bound vocabulary (case-insensitive; one required): `exceeds` /
//!   `too many` / `too large` / `oversized` / `limit` / `maximum` / `cap` /
//!   `bound` / `truncated` / `more than`.
//! - Role vocabulary per parser (case-insensitive; one required): XP parser
//!   `player` / `export` / `row` / `mee6`; rewards parser `reward` / `role` /
//!   `export` / `mee6`.
//! - Truncated problem lists keep at most `MAX_KEPT_PROBLEMS` entries and
//!   carry a receipt: one entry matching `truncat*` / `remain*` / `omitt*` /
//!   `additional` / `further` / `suppress*` / `not shown`, or `N more` with a
//!   count. Counts are preserved in the receipt; content is truncated.
//! - Controls: valid exports parse byte-identical (exact rows pinned below),
//!   and extreme nesting keeps failing closed. Any over-blocking or rewording
//!   of the valid path fails the controls.

use two_bot_cutover::mee6_rewards::SnapshotRoleTags;
use two_bot_cutover::{
    parse_mee6_export, parse_mee6_role_rewards, parse_roles_snapshot, Mee6ImportRow,
    Mee6RoleReward, SnapshotRole,
};

/// The file byte cap from the companion CLI suite. Every hostile fixture here
/// self-checks it stays under this size, so the refusal under test can only
/// come from a player/problem bound.
const INTAKE_CAP_BYTES: usize = 8 * 1024 * 1024;

/// Maximum accumulated problems any correct implementation keeps: generous
/// headroom over any sane truncation bound, tight enough to prove hostile
/// input cannot grow the error 1:1 with rows.
const MAX_KEPT_PROBLEMS: usize = 5000;

const EXPORT: &str = include_str!("fixtures/mee6-export-role-rewards.json");
const ROLES: &str = include_str!("fixtures/mee6-guild-roles.json");

/// True when `message` carries a bound refusal. See the module docs for the
/// accepted words.
fn is_bound_refusal(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "exceed",
        "too many",
        "too large",
        "oversiz",
        "too big",
        "larger than",
        "limit",
        "maximum",
        "capped",
        "cap ",
        "cap:",
        "(cap",
        "bound",
        "truncat",
        "more than",
    ]
    .into_iter()
    .any(|w| lower.contains(w))
}

/// True when `message` names one of the role words for the parser under test.
fn names_role(message: &str, roles: &[&str]) -> bool {
    let lower = message.to_lowercase();
    roles.iter().any(|r| lower.contains(&r.to_lowercase()))
}

/// True when a problem line is a truncation receipt: it says bounded content
/// was cut and (via a count) preserves the total. Accepted shapes are listed
/// in the module contract.
fn is_truncation_receipt(problem: &str) -> bool {
    let lower = problem.to_lowercase();
    lower.contains("truncat")
        || lower.contains("remain")
        || lower.contains("omitt")
        || lower.contains("additional")
        || lower.contains("further")
        || lower.contains("suppress")
        || lower.contains("not shown")
        || (lower.contains("more") && lower.bytes().any(|b| b.is_ascii_digit()))
}

/// Assert a hostile parse failure is bounded: few problems plus a receipt.
fn assert_truncated(problems: &[String], submitted: usize) {
    assert!(
        problems.len() <= MAX_KEPT_PROBLEMS,
        "must truncate {submitted} hostile rows to at most {MAX_KEPT_PROBLEMS} problems, kept {}",
        problems.len()
    );
    let sample: Vec<&String> = problems.iter().take(3).collect();
    assert!(
        problems.iter().any(|p| is_truncation_receipt(p)),
        "must carry a truncation receipt naming the cut; first of {}: {sample:?}",
        problems.len()
    );
}

/// `count` valid XP rows with distinct member ids, so the fixture trips a
/// row bound and a unique-member bound alike.
fn many_valid_rows(count: usize) -> String {
    let mut export = String::from("[");
    for index in 0..count {
        if index > 0 {
            export.push(',');
        }
        export.push_str(&format!(
            "{{\"id\":\"{}\",\"xp\":1}}",
            100_000_000_000_000_000u64 + index as u64
        ));
    }
    export.push(']');
    export
}

#[test]
fn xp_parse_refuses_many_row_export() {
    let export = many_valid_rows(180_000);
    assert!(
        export.len() < INTAKE_CAP_BYTES,
        "fixture must isolate the player bound under the file cap"
    );
    let result = parse_mee6_export(&export);
    assert!(result.is_err(), "180 000 valid rows must be refused");
    let message = result.unwrap_err().to_string();
    assert!(
        is_bound_refusal(&message),
        "refusal must name the player bound, got: {message}"
    );
    assert!(
        names_role(&message, &["player", "export", "row", "mee6"]),
        "refusal must name the file role, got: {message}"
    );
}

#[test]
fn xp_parse_truncates_many_malformed_rows() {
    let mut export = String::from("[0");
    export.push_str(&",0".repeat(199_999));
    export.push(']');
    assert!(
        export.len() < INTAKE_CAP_BYTES,
        "fixture must isolate problem truncation under the file cap"
    );
    let err = parse_mee6_export(&export).expect_err("malformed rows must fail");
    assert_truncated(&err.problems, 200_000);
}

#[test]
fn xp_parse_refuses_deeply_nested_many_row_export() {
    // Hostile in both dimensions: 60 000 rows (over any meaningful player
    // bound) each carrying 20-deep junk (under the JSON nesting limit, so
    // depth alone would not refuse it).
    let junk = format!("{}{}", "[".repeat(20), "]".repeat(20));
    let mut export = String::from("[");
    for index in 0..60_000 {
        if index > 0 {
            export.push(',');
        }
        export.push_str(&format!(
            "{{\"id\":\"{}\",\"xp\":1,\"junk\":{junk}}}",
            100_000_000_000_000_000u64 + index as u64
        ));
    }
    export.push(']');
    assert!(
        export.len() < INTAKE_CAP_BYTES,
        "fixture must isolate the player bound under the file cap"
    );
    let result = parse_mee6_export(&export);
    assert!(result.is_err(), "nested many-row export must be refused");
    let message = result.unwrap_err().to_string();
    assert!(
        is_bound_refusal(&message),
        "refusal must name the player bound, got: {message}"
    );
    assert!(
        names_role(&message, &["player", "export", "row", "mee6"]),
        "refusal must name the file role, got: {message}"
    );
}

#[test]
fn xp_parse_fails_closed_on_extreme_nesting() {
    // Depth alone already fails closed at the JSON recursion limit; this pins
    // that behavior so the new bounds cannot regress it into a panic or hang.
    let nested = format!("{}{}", "[".repeat(300), "]".repeat(300));
    let err = parse_mee6_export(&nested).expect_err("extreme nesting must fail closed");
    assert!(
        err.problems.len() <= 10,
        "nesting failure must stay a short error, got {} problems",
        err.problems.len()
    );
}

#[test]
fn rewards_parse_refuses_many_reward_export() {
    let mut export = String::from("{\"role_rewards\":[");
    for rank in 1..=150_000 {
        if rank > 1 {
            export.push(',');
        }
        export.push_str(&format!(
            "{{\"rank\":{rank},\"role_id\":\"900000000000000020\"}}"
        ));
    }
    export.push_str("]}");
    assert!(
        export.len() < INTAKE_CAP_BYTES,
        "fixture must isolate the reward bound under the file cap"
    );
    let result = parse_mee6_role_rewards(&export);
    assert!(result.is_err(), "150 000 rewards must be refused");
    let message = result.unwrap_err().to_string();
    assert!(
        is_bound_refusal(&message),
        "refusal must name the reward bound, got: {message}"
    );
    assert!(
        names_role(&message, &["reward", "role", "export", "mee6"]),
        "refusal must name the file role, got: {message}"
    );
}

#[test]
fn rewards_parse_truncates_many_malformed_rewards() {
    let mut export = String::from("{\"role_rewards\":[0");
    export.push_str(&",0".repeat(199_999));
    export.push_str("]}");
    assert!(
        export.len() < INTAKE_CAP_BYTES,
        "fixture must isolate problem truncation under the file cap"
    );
    let err = parse_mee6_role_rewards(&export).expect_err("malformed rewards must fail");
    assert_truncated(&err.problems, 200_000);
}

#[test]
fn xp_parse_of_valid_export_is_byte_identical() {
    assert_eq!(
        parse_mee6_export(EXPORT).expect("valid export parses"),
        [
            Mee6ImportRow {
                member_id: "900000000000000030".to_owned(),
                xp: 1000,
                level: None,
            },
            Mee6ImportRow {
                member_id: "900000000000000031".to_owned(),
                xp: 2500,
                level: None,
            },
            Mee6ImportRow {
                member_id: "900000000000000032".to_owned(),
                xp: 90,
                level: None,
            },
        ]
    );
}

#[test]
fn rewards_parse_of_valid_export_is_byte_identical() {
    let expected = [
        (5, "900000000000000020", "Level 5"),
        (10, "900000000000000021", "Level 10"),
        (15, "900000000000000099", "Level 15"),
        (20, "900000000000000022", "Server Booster"),
        (25, "900000000000000023", "Staff"),
        (30, "900000000000000020", "Level 5"),
        (5, "900000000000000024", "Level 5 Alt"),
    ]
    .map(|(level, role_id, role_name)| Mee6RoleReward {
        level,
        role_id: role_id.to_owned(),
        role_name: Some(role_name.to_owned()),
    });
    assert_eq!(
        parse_mee6_role_rewards(EXPORT).expect("valid export parses"),
        expected
    );
}

#[test]
fn roles_snapshot_parse_is_byte_identical() {
    let expected = [
        ("900000000000000000", "@everyone", 0, false, None),
        (
            "900000000000000010",
            "Owen QA Test",
            50,
            true,
            Some("900000000000000001"),
        ),
        ("900000000000000020", "Level 5", 10, false, None),
        ("900000000000000024", "Level 5 Alt", 12, false, None),
        ("900000000000000021", "Level Ten", 20, false, None),
        ("900000000000000022", "Server Booster", 30, true, None),
        ("900000000000000023", "Staff", 80, false, None),
    ]
    .map(|(id, name, position, managed, bot_id)| SnapshotRole {
        id: id.to_owned(),
        name: name.to_owned(),
        position,
        managed,
        tags: bot_id.map(|bot| SnapshotRoleTags {
            bot_id: Some(bot.to_owned()),
        }),
    });
    assert_eq!(
        parse_roles_snapshot(ROLES).expect("valid snapshot parses"),
        expected
    );
}
