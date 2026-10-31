//! Protected acceptance tests for capture/backfill invite differencing.
//! Compare both readings in the persisted INTEGER domain, not raw Discord u64.

use std::collections::BTreeMap;

use two_bot_cutover::invite::{invite_growth, InviteState};

const MAX: u64 = i32::MAX as u64;

fn baseline(rows: &[(&str, u64)]) -> BTreeMap<String, u64> {
    rows.iter()
        .map(|(code, uses)| ((*code).to_owned(), *uses))
        .collect()
}

fn invite(code: &str, uses: u64) -> InviteState {
    InviteState {
        code: code.to_owned(),
        uses,
    }
}

#[test]
fn crossing_the_persisted_ceiling_counts_one_join_once() {
    // Catches raw-current minus clamped-baseline subtraction: the last
    // representable increase is one, not the entire out-of-range excess.
    for raw in [MAX + 1, i64::MAX as u64 + 1, u64::MAX] {
        assert_eq!(
            invite_growth(
                &baseline(&[("crossing", MAX - 1)]),
                &[invite("crossing", raw)]
            ),
            baseline(&[("crossing", 1)]),
            "crossing the ceiling with raw uses={raw} contributes only one"
        );
        // This is the next capture after store_counters persisted MAX.
        assert!(
            invite_growth(&baseline(&[("crossing", MAX)]), &[invite("crossing", raw)]).is_empty(),
            "the persisted ceiling must prevent repeated growth for raw uses={raw}"
        );
    }
}

#[test]
fn a_new_code_contributes_at_most_the_persisted_ceiling() {
    for raw in [MAX, MAX + 1, i64::MAX as u64 + 1, u64::MAX] {
        assert_eq!(
            invite_growth(&BTreeMap::new(), &[invite("new", raw)]),
            baseline(&[("new", MAX)]),
            "new-code growth must not exceed INTEGER for raw uses={raw}"
        );
    }
    assert!(invite_growth(&BTreeMap::new(), &[invite("new-zero", 0)]).is_empty());
}

#[test]
fn raw_previous_and_current_readings_use_the_same_domain() {
    // Clamping only the delta reports false growth for these raw pairs.
    // Raw previous readings are valid for an
    // in-memory capture baseline even though the database stores only MAX.
    for (previous, current) in [
        (MAX + 1, MAX + 2),
        (MAX + 1, u64::MAX),
        (i64::MAX as u64 + 1, u64::MAX),
        (u64::MAX, u64::MAX),
        (u64::MAX, MAX),
        (u64::MAX, 7),
    ] {
        assert!(
            invite_growth(&baseline(&[("raw", previous)]), &[invite("raw", current)]).is_empty(),
            "both readings saturate before differencing: previous={previous}, current={current}"
        );
    }
}

#[test]
fn ordinary_growth_decreases_missing_codes_and_exact_max_keep_their_semantics() {
    let previous = baseline(&[
        ("normal", 7),
        ("decrease", 9),
        ("unchanged", 4),
        ("gone", 12),
        ("exact-max", MAX - 1),
    ]);
    assert_eq!(
        invite_growth(
            &previous,
            &[
                invite("normal", 10),
                invite("decrease", 0),
                invite("unchanged", 4),
                invite("new-small", 5),
                invite("new-zero", 0),
                invite("exact-max", MAX),
            ],
        ),
        baseline(&[("normal", 3), ("new-small", 5), ("exact-max", 1)]),
        "a decreasing or disappeared code cannot cancel another code's real increase"
    );
}
