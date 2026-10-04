//! Channel lockdown/unlock planner acceptance (TOG-12701).
//!
//! Pins the framework-free planner in `two_bot_core::channel_moderation`
//! (spec: `docs/internal-channel-moderation.md`): purge `1..=100`, slowmode
//! `0..=21600`, channel-reason gate, lockdown seed / unlock restore, and
//! fixed result text.
//!
//! All assertions go through the existing public `channel_moderation` API
//! only. Synthetic fixtures only: no Discord, network, database, or
//! handler/store/gateway wiring (wiring stays TOG-10174).

use two_bot_core::channel_moderation::{
    moderation_result_text, plan_lockdown, plan_unlock, require_channel_reason,
    validate_purge_count, validate_slowmode_seconds, ChannelOutcome, EveryoneOverwrite,
    LockdownRecord, UnlockError, UnlockPlan,
};

fn overwrite(allow: &str, deny: &str) -> EveryoneOverwrite {
    EveryoneOverwrite {
        allow: allow.to_owned(),
        deny: deny.to_owned(),
    }
}

fn recorded(prior_allow: &str, prior_deny: &str, prior_exists: bool) -> LockdownRecord {
    LockdownRecord {
        channel_id: "111111111111111111".to_owned(),
        guild_id: "222222222222222222".to_owned(),
        prior_allow: prior_allow.to_owned(),
        prior_deny: prior_deny.to_owned(),
        prior_exists,
        reason: "raid".to_owned(),
        recovery_generation: "test-generation".to_owned(),
    }
}

#[test]
fn purge_enforces_inclusive_edges() {
    assert_eq!(validate_purge_count(Some(1)), Ok(1));
    assert_eq!(validate_purge_count(Some(100)), Ok(100));
    assert_eq!(validate_purge_count(Some(50)), Ok(50));
}

#[test]
fn purge_rejects_out_of_range_and_missing() {
    for bad in [None, Some(0), Some(101), Some(u64::MAX)] {
        let err = validate_purge_count(bad).expect_err("must refuse");
        assert_eq!(err.field, "count");
        assert_eq!(err.min, 1);
        assert_eq!(err.max, 100);
    }
}

#[test]
fn slowmode_enforces_inclusive_edges() {
    // Zero disables slowmode; both ends are inclusive.
    assert_eq!(validate_slowmode_seconds(Some(0)), Ok(0));
    assert_eq!(validate_slowmode_seconds(Some(21_600)), Ok(21_600));
    assert_eq!(validate_slowmode_seconds(Some(7)), Ok(7));
}

#[test]
fn slowmode_rejects_out_of_range_and_missing() {
    for bad in [None, Some(21_601), Some(u64::MAX)] {
        let err = validate_slowmode_seconds(bad).expect_err("must refuse");
        assert_eq!(err.field, "seconds");
        assert_eq!(err.min, 0);
        assert_eq!(err.max, 21_600);
    }
}

#[test]
fn channel_reason_refuses_empty_and_blank() {
    for bad in ["", "   ", "\t\n  ", " \t "] {
        assert!(
            require_channel_reason(bad).is_err(),
            "blank reason {bad:?} must be refused"
        );
    }
}

#[test]
fn channel_reason_trims_valid_input() {
    assert_eq!(require_channel_reason("  spam  "), Ok("spam".to_owned()));
    assert!(require_channel_reason("raid").is_ok());
}

#[test]
fn lockdown_preserves_unrelated_bits_and_records_seed() {
    // (allow, deny) pairs with unrelated permission bits around SEND_MESSAGES.
    for (allow, deny) in [(0_u64, 0_u64), (1024, 8192), (3072, 8192), (u64::MAX, 0)] {
        let original = overwrite(&allow.to_string(), &deny.to_string());
        let plan = plan_lockdown(Some(&original)).expect("plans");
        // Seed records the exact prior masks for unlock.
        assert_eq!(plan.seed.prior_allow, original.allow);
        assert_eq!(plan.seed.prior_deny, original.deny);
        assert!(plan.seed.prior_exists);
        // Only SEND_MESSAGES (2048) moves: cleared from allow, set in deny.
        assert_eq!(
            plan.write,
            overwrite(&(allow & !2048).to_string(), &(deny | 2048).to_string(),)
        );
        // Unrelated bits survive verbatim.
        let write_allow: u64 = plan.write.allow.parse().expect("decimal allow");
        let write_deny: u64 = plan.write.deny.parse().expect("decimal deny");
        assert_eq!(write_allow & !2048, allow & !2048);
        assert_eq!(write_deny & !2048, deny & !2048);
        assert_eq!(write_allow & 2048, 0);
        assert_ne!(write_deny & 2048, 0);
    }
}

#[test]
fn lockdown_without_overwrite_seeds_absent_record() {
    let plan = plan_lockdown(None).expect("plans");
    assert_eq!(plan.seed.prior_allow, "0");
    assert_eq!(plan.seed.prior_deny, "0");
    assert!(!plan.seed.prior_exists);
    assert_eq!(plan.write, overwrite("0", "2048"));
}

#[test]
fn unlock_restores_recorded_seed() {
    for (allow, deny) in [
        ("1024", "8192"),
        ("0", "0"),
        ("18446744073709549567", "2048"),
    ] {
        let rec = recorded(allow, deny, true);
        assert_eq!(
            plan_unlock(
                Some(&rec),
                Some(&plan_lockdown(Some(&overwrite(allow, deny))).unwrap().write),
            ),
            Ok(UnlockPlan::Restore {
                allow: allow.to_owned(),
                deny: deny.to_owned(),
            })
        );
    }
}

#[test]
fn unlock_without_prior_overwrite_deletes_it() {
    assert_eq!(
        plan_unlock(
            Some(&recorded("0", "0", false)),
            Some(&overwrite("0", "2048"))
        ),
        Ok(UnlockPlan::DeleteOverwrite)
    );
}

#[test]
fn unlock_refuses_unknown_or_absent_record() {
    assert_eq!(plan_unlock(None, None), Err(UnlockError::NotLocked));
}

#[test]
fn unlock_preserves_live_access_edits_and_only_restores_the_send_bits() {
    for prior_exists in [false, true] {
        for (prior_allow, prior_deny) in [(0_u64, 0_u64), (2048, 0), (0, 2048)] {
            if !prior_exists && (prior_allow != 0 || prior_deny != 0) {
                continue;
            }
            let rec = recorded(
                &prior_allow.to_string(),
                &prior_deny.to_string(),
                prior_exists,
            );
            // VIEW_CHANNEL was denied after lockdown; a new unrelated allow
            // and a high permission bit were added too. None may be reverted.
            let live_allow = 8192_u64 | (1 << 48);
            let live_deny = 1024_u64 | 2048;
            let plan = plan_unlock(
                Some(&rec),
                Some(&overwrite(&live_allow.to_string(), &live_deny.to_string())),
            );
            assert_eq!(
                plan,
                Ok(UnlockPlan::Restore {
                    allow: (live_allow | prior_allow).to_string(),
                    deny: (1024 | prior_deny).to_string(),
                })
            );
        }
    }
}

#[test]
fn unlock_refuses_missing_overwrite_and_send_bit_drift() {
    let rec = recorded("2048", "0", true);
    assert_eq!(plan_unlock(Some(&rec), None), Err(UnlockError::Drift));
    for (allow, deny) in [("0", "1024"), ("2048", "3072"), ("2048", "0")] {
        assert_eq!(
            plan_unlock(Some(&rec), Some(&overwrite(allow, deny))),
            Err(UnlockError::Drift)
        );
    }
    assert!(UnlockError::Drift
        .to_string()
        .contains("no mutation attempted"));
}

#[test]
fn unlock_refuses_corrupt_live_masks() {
    let rec = recorded("0", "0", false);
    for (allow, deny) in [("bad", "2048"), ("0", "bad"), ("0", "18446744073709551616")] {
        assert!(matches!(
            plan_unlock(Some(&rec), Some(&overwrite(allow, deny))),
            Err(UnlockError::InvalidMask(_))
        ));
    }
}

#[test]
fn result_text_names_outcome_without_echoing_input() {
    let hostile = "<@999999999999999999> evil-ping";
    let cases = [
        (
            ChannelOutcome::Purged { affected: 5 },
            "Moderation action completed: purged (5).",
        ),
        (
            ChannelOutcome::SlowmodeUpdated,
            "Moderation action completed: slowmode_updated.",
        ),
        (
            ChannelOutcome::LockedDown,
            "Moderation action completed: locked_down.",
        ),
        (
            ChannelOutcome::Unlocked,
            "Moderation action completed: unlocked.",
        ),
    ];
    for (outcome, expected) in cases {
        let text = moderation_result_text(outcome);
        assert_eq!(text, expected);
        assert!(
            !text.contains(hostile),
            "result text must not echo user input"
        );
        assert!(!text.contains("evil-ping"), "result text must be fixed");
    }
}
