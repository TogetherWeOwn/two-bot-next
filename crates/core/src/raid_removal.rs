//! Operator-approved raid removal decisions. No transport or database access.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalMode {
    #[default]
    DryRun,
    Execute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalDecision {
    WouldKick,
    Kick,
    SkippedDone,
    AlreadyGone,
    Protected,
}

/// Membership and protection must come from a fresh read, never the input file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetState {
    Missing,
    Protected,
    Eligible,
}

pub fn decide_removal(
    mode: RemovalMode,
    terminal: bool,
    state: Option<TargetState>,
) -> RemovalDecision {
    if terminal {
        return RemovalDecision::SkippedDone;
    }
    if mode == RemovalMode::DryRun {
        return RemovalDecision::WouldKick;
    }
    match state {
        Some(TargetState::Missing) => RemovalDecision::AlreadyGone,
        Some(TargetState::Eligible) => RemovalDecision::Kick,
        // Unknown protection is not permission to kick.
        Some(TargetState::Protected) | None => RemovalDecision::Protected,
    }
}

/// Accept exact string IDs only: JSON numbers lose precision in other tools.
pub fn validate_targets(ids: Vec<String>) -> Result<Vec<String>, &'static str> {
    if ids.is_empty() {
        return Err("target list contains no IDs");
    }
    let mut seen = BTreeSet::new();
    let mut targets = Vec::new();
    for id in ids {
        if !(17..=20).contains(&id.len())
            || id.starts_with('0')
            || !id.bytes().all(|b| b.is_ascii_digit())
            || id.parse::<u64>().map_or(true, |n| n == 0)
        {
            return Err("target list contains an invalid Discord snowflake");
        }
        if seen.insert(id.clone()) {
            targets.push(id);
        }
    }
    Ok(targets)
}

pub fn validate_execute_count(
    mode: RemovalMode,
    expected: Option<usize>,
    actual: usize,
) -> Result<(), &'static str> {
    if mode == RemovalMode::Execute && expected != Some(actual) {
        return Err("execute requires --expect matching the unique target count");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_is_default_and_needs_no_live_state() {
        assert_eq!(RemovalMode::default(), RemovalMode::DryRun);
        assert_eq!(
            decide_removal(RemovalMode::default(), false, None),
            RemovalDecision::WouldKick
        );
    }

    #[test]
    fn target_list_is_input_deduplicated_in_order_and_never_partially_accepted() {
        let a = "100000000000000001".to_owned();
        let b = "100000000000000002".to_owned();
        assert_eq!(
            validate_targets(vec![b.clone(), a.clone(), b.clone()]).unwrap(),
            vec![b, a]
        );
        assert!(validate_targets(vec![]).is_err());
        assert!(validate_targets(vec!["bad".into()]).is_err());
        assert!(validate_targets(vec!["0100000000000000050".into()]).is_err());
        assert!(validate_targets(vec!["99999999999999999999".into()]).is_err());
        assert!(validate_execute_count(RemovalMode::Execute, None, 2).is_err());
        assert!(validate_execute_count(RemovalMode::Execute, Some(3), 2).is_err());
        assert!(validate_execute_count(RemovalMode::Execute, Some(2), 2).is_ok());
    }

    #[test]
    fn protection_and_unknown_membership_fail_closed() {
        for state in [None, Some(TargetState::Protected)] {
            assert_eq!(
                decide_removal(RemovalMode::Execute, false, state),
                RemovalDecision::Protected
            );
        }
        assert_eq!(
            decide_removal(RemovalMode::Execute, false, Some(TargetState::Missing)),
            RemovalDecision::AlreadyGone
        );
        assert_eq!(
            decide_removal(RemovalMode::Execute, false, Some(TargetState::Eligible)),
            RemovalDecision::Kick
        );
    }

    #[test]
    fn shared_kick_policy_preserves_350ms_floor_and_four_retries() {
        use crate::{backoff_ms, pace_wait_ms, MAX_HTTP_TRIES};
        assert_eq!(pace_wait_ms(1000, 350, 1000), 350);
        assert_eq!(pace_wait_ms(1000, 350, 1349), 1);
        assert_eq!(pace_wait_ms(1000, 350, 1350), 0);
        assert_eq!(MAX_HTTP_TRIES, 5);
        assert_eq!(
            (0..4).map(backoff_ms).collect::<Vec<_>>(),
            vec![500, 1000, 2000, 4000]
        );
    }

    #[test]
    fn terminal_outcome_stays_settled_even_in_a_later_dry_run() {
        for mode in [RemovalMode::DryRun, RemovalMode::Execute] {
            assert_eq!(
                decide_removal(mode, true, None),
                RemovalDecision::SkippedDone
            );
        }
    }
}
