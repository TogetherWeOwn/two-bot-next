//! Invite-counter differencing for the host-less capture path.
//!
//! Ports the pure half of `src/core/inviteTracker.ts`. Attribution works by
//! noticing which invite's use count went up between two readings; a code
//! created inside the window contributes ALL of its uses, and a counter that
//! went backwards (deleted/recreated code) is clamped out so it can never
//! cancel a real increase elsewhere.
//!
//! `exact` is deliberately narrower than "one code moved": only a single
//! moving code whose arithmetic closes is proof. Everything else is an
//! aggregate count (`exact: false`) or an honest `ambiguous:` fallback.

use std::collections::BTreeMap;

/// One invite's standing counter reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteState {
    pub code: String,
    pub uses: u64,
}

/// One join's attribution for a capture window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinAttribution {
    /// `invite:CODE` / `ambiguous:a+b` / `vanity` / `unknown`.
    pub source: String,
    /// True only when THIS member provably came through THIS code.
    pub exact: bool,
}

/// How much each invite code was used between two counter readings.
///
/// `prev` maps code → uses at the last capture; `current` is this capture's
/// reading. Codes absent from `prev` were created inside the window, so all
/// of their uses are new. Negative deltas clamp to zero.
pub fn invite_growth(
    prev: &BTreeMap<String, u64>,
    current: &[InviteState],
) -> BTreeMap<String, u64> {
    let mut growth = BTreeMap::new();
    for inv in current {
        let delta = match prev.get(&inv.code) {
            None => inv.uses,
            Some(before) => inv.uses.saturating_sub(*before),
        };
        if delta > 0 {
            growth.insert(inv.code.clone(), delta);
        }
    }
    growth
}

/// Decide a source for every join in one capture window.
///
/// Rules, in order: nothing moved → vanity/unknown; counters and member list
/// agree → each code gets as many joins as it gained (per-code counts exact,
/// member↔code pairing not observed); they disagree → `ambiguous:a+b`; a
/// single moving code whose arithmetic closes → proven (`exact: true`).
pub fn attribute_joins(
    growth: &BTreeMap<String, u64>,
    join_count: usize,
    guild_has_vanity: bool,
) -> Vec<JoinAttribution> {
    if join_count == 0 {
        return Vec::new();
    }
    let fill = |source: &str, exact: bool| {
        vec![
            JoinAttribution {
                source: source.to_owned(),
                exact,
            };
            join_count
        ]
    };

    let total: u64 = growth.values().sum();
    let closes = total == join_count as u64;

    if growth.is_empty() {
        return fill(
            if guild_has_vanity {
                "vanity"
            } else {
                "unknown"
            },
            false,
        );
    }
    if growth.len() == 1 {
        let code = growth.keys().next().expect("len checked");
        return fill(&format!("invite:{code}"), closes);
    }
    if !closes {
        let codes: Vec<_> = growth.keys().cloned().collect();
        return fill(&format!("ambiguous:{}", codes.join("+")), false);
    }

    let mut out = Vec::with_capacity(join_count);
    for (code, gain) in growth {
        for _ in 0..*gain {
            out.push(JoinAttribution {
                source: format!("invite:{code}"),
                exact: false,
            });
        }
    }
    out
}

/// Single-string attribution for the live path: one code, several, or none.
#[must_use]
pub fn attribute_single(grew: &[String], guild_has_vanity: bool) -> String {
    if grew.len() == 1 {
        return format!("invite:{}", grew[0]);
    }
    if grew.len() > 1 {
        return format!("ambiguous:{}", grew.join("+"));
    }
    if guild_has_vanity {
        "vanity".to_owned()
    } else {
        "unknown".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv(code: &str, uses: u64) -> InviteState {
        InviteState {
            code: code.to_owned(),
            uses,
        }
    }

    fn prev(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(c, u)| ((*c).to_owned(), *u)).collect()
    }

    #[test]
    fn one_code_moving_yields_clean_delta() {
        assert_eq!(
            invite_growth(&prev(&[("aB3xY9", 4)]), &[inv("aB3xY9", 5)]),
            prev(&[("aB3xY9", 1)])
        );
    }

    #[test]
    fn code_created_inside_window_contributes_all_uses() {
        assert_eq!(
            invite_growth(&prev(&[("old", 10)]), &[inv("old", 10), inv("brandnew", 3)]),
            prev(&[("brandnew", 3)])
        );
    }

    #[test]
    fn nothing_moving_yields_no_growth() {
        assert!(
            invite_growth(&prev(&[("a", 2), ("b", 7)]), &[inv("a", 2), inv("b", 7)]).is_empty()
        );
    }

    #[test]
    fn backwards_counter_clamps_and_cannot_cancel_real_rise() {
        // Deleted-and-recreated codes reset to zero; unclamped that would
        // subtract from the total and disguise a genuine join as vanity.
        assert_eq!(
            invite_growth(
                &prev(&[("reset", 9), ("real", 1)]),
                &[inv("reset", 0), inv("real", 2)]
            ),
            prev(&[("real", 1)])
        );
    }

    #[test]
    fn disappeared_code_is_not_growth() {
        assert!(invite_growth(&prev(&[("gone", 5), ("here", 1)]), &[inv("here", 1)]).is_empty());
    }

    #[test]
    fn single_attribution_shapes() {
        assert_eq!(
            attribute_single(&["aB3xY9".to_owned()], false),
            "invite:aB3xY9"
        );
        assert_eq!(
            attribute_single(&["a".to_owned(), "b".to_owned()], false),
            "ambiguous:a+b"
        );
        assert_eq!(attribute_single(&[], true), "vanity");
        assert_eq!(attribute_single(&[], false), "unknown");
    }

    #[test]
    fn closing_multicode_window_splits_by_magnitude() {
        let g = invite_growth(
            &prev(&[("aaa", 5), ("bbb", 1)]),
            &[inv("aaa", 7), inv("bbb", 2)],
        );
        let out = attribute_joins(&g, 3, false);
        assert_eq!(
            out.iter().map(|a| a.source.as_str()).collect::<Vec<_>>(),
            vec!["invite:aaa", "invite:aaa", "invite:bbb"]
        );
        assert!(out.iter().all(|a| !a.exact));
    }

    #[test]
    fn new_code_in_multicode_window_claims_all_uses() {
        let g = invite_growth(&prev(&[("old", 4)]), &[inv("old", 5), inv("fresh", 3)]);
        let out = attribute_joins(&g, 4, false);
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for a in &out {
            *counts.entry(a.source.as_str()).or_default() += 1;
        }
        assert_eq!(counts["invite:fresh"], 3);
        assert_eq!(counts["invite:old"], 1);
    }

    #[test]
    fn non_closing_multicode_window_stays_ambiguous() {
        let g = invite_growth(
            &prev(&[("aaa", 5), ("bbb", 1)]),
            &[inv("aaa", 7), inv("bbb", 2)],
        );
        let out = attribute_joins(&g, 2, false);
        assert_eq!(
            out.iter().map(|a| a.source.as_str()).collect::<Vec<_>>(),
            vec!["ambiguous:aaa+bbb", "ambiguous:aaa+bbb"]
        );
        assert!(out.iter().all(|a| !a.exact));
    }

    #[test]
    fn single_moving_code_closing_is_the_only_exact_case() {
        let g = invite_growth(&prev(&[("aB3xY9", 4)]), &[inv("aB3xY9", 5)]);
        assert_eq!(
            attribute_joins(&g, 1, false),
            vec![JoinAttribution {
                source: "invite:aB3xY9".to_owned(),
                exact: true,
            }]
        );
    }

    #[test]
    fn single_code_short_of_member_list_is_best_guess_not_proof() {
        let g = invite_growth(&prev(&[("aB3xY9", 4)]), &[inv("aB3xY9", 5)]);
        let out = attribute_joins(&g, 2, false);
        assert_eq!(
            out.iter().map(|a| a.source.as_str()).collect::<Vec<_>>(),
            vec!["invite:aB3xY9", "invite:aB3xY9"]
        );
        assert!(out.iter().all(|a| !a.exact));
    }

    #[test]
    fn no_movement_is_vanity_or_unknown_never_exact() {
        let g = invite_growth(&prev(&[("a", 2)]), &[inv("a", 2)]);
        assert_eq!(
            attribute_joins(&g, 1, true),
            vec![JoinAttribution {
                source: "vanity".to_owned(),
                exact: false,
            }]
        );
        assert_eq!(
            attribute_joins(&g, 1, false),
            vec![JoinAttribution {
                source: "unknown".to_owned(),
                exact: false,
            }]
        );
    }

    #[test]
    fn empty_window_emits_nothing() {
        let g = invite_growth(&prev(&[("a", 2)]), &[inv("a", 9)]);
        assert!(attribute_joins(&g, 0, false).is_empty());
    }

    #[test]
    fn seven_code_campaign_keeps_every_count() {
        let codes = ["c1", "c2", "c3", "c4", "c5", "c6", "c7"];
        let gains = [3u64, 0, 1, 0, 2, 0, 4];
        let p: BTreeMap<String, u64> = codes.iter().map(|c| ((*c).to_owned(), 0)).collect();
        let cur: Vec<_> = codes.iter().zip(gains).map(|(c, g)| inv(c, g)).collect();
        let g = invite_growth(&p, &cur);
        let out = attribute_joins(&g, 10, false);
        assert_eq!(out.len(), 10);
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for a in &out {
            *counts.entry(a.source.as_str()).or_default() += 1;
        }
        assert_eq!(counts.len(), 4);
        assert_eq!(counts["invite:c1"], 3);
        assert_eq!(counts["invite:c3"], 1);
        assert_eq!(counts["invite:c5"], 2);
        assert_eq!(counts["invite:c7"], 4);
    }
}
