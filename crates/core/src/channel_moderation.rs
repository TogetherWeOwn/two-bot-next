//! Framework-free channel moderation plans for `/purge`, `/slowmode`,
//! `/lockdown`, and `/unlock` (parity §1 #8–#11).
//!
//! Callers use the shared moderation gates and audit-reason rule before
//! planning a mutation. Persist the lockdown seed before applying the overwrite;
//! clear recovery state only after Discord accepts the unlock. The interaction
//! router and REST executor own side effects, not this module.

use super::moderation::{require_moderation_reason, ModerationAction, ReasonError};

pub const MIN_PURGE_COUNT: u64 = 1;
pub const MAX_PURGE_COUNT: u64 = 100;
pub const MAX_SLOWMODE_SECONDS: u64 = 21_600;
pub const SEND_MESSAGES_BIT: u64 = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelModerationVerb {
    Purge,
    Slowmode,
    Lockdown,
    Unlock,
}

impl ChannelModerationVerb {
    #[must_use]
    pub fn action(self) -> ModerationAction {
        match self {
            Self::Purge => ModerationAction::Purge,
            Self::Slowmode => ModerationAction::Slowmode,
            Self::Lockdown => ModerationAction::Lockdown,
            Self::Unlock => ModerationAction::Unlock,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelOutcome {
    Purged { affected: u64 },
    SlowmodeUpdated,
    LockedDown,
    Unlocked,
}

impl ChannelOutcome {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Purged { .. } => "purged",
            Self::SlowmodeUpdated => "slowmode_updated",
            Self::LockedDown => "locked_down",
            Self::Unlocked => "unlocked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("\"{field}\" must be an integer between {min} and {max}")]
pub struct BoundsError {
    pub field: &'static str,
    pub min: u64,
    pub max: u64,
}

pub fn validate_purge_count(count: Option<u64>) -> Result<u64, BoundsError> {
    match count {
        Some(n) if (MIN_PURGE_COUNT..=MAX_PURGE_COUNT).contains(&n) => Ok(n),
        _ => Err(BoundsError {
            field: "count",
            min: MIN_PURGE_COUNT,
            max: MAX_PURGE_COUNT,
        }),
    }
}

/// Zero disables slowmode; both ends of the range are inclusive.
pub fn validate_slowmode_seconds(seconds: Option<u64>) -> Result<u64, BoundsError> {
    match seconds {
        Some(n) if n <= MAX_SLOWMODE_SECONDS => Ok(n),
        _ => Err(BoundsError {
            field: "seconds",
            min: 0,
            max: MAX_SLOWMODE_SECONDS,
        }),
    }
}

pub fn require_channel_reason(value: &str) -> Result<String, ReasonError> {
    require_moderation_reason(value)
}

/// Full @everyone masks, stored as decimal strings as in the legacy schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EveryoneOverwrite {
    pub allow: String,
    pub deny: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unparseable permission mask in @everyone overwrite")]
pub struct MaskError;

fn parse_mask(value: &str) -> Result<u64, MaskError> {
    value.parse::<u64>().map_err(|_| MaskError)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockdownRecord {
    pub channel_id: String,
    pub guild_id: String,
    pub prior_allow: String,
    pub prior_deny: String,
    pub prior_exists: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockdownPlan {
    pub seed: LockdownSeed,
    pub write: EveryoneOverwrite,
}

/// Insert-only recovery state: repeated lockdowns must preserve the first seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockdownSeed {
    pub prior_allow: String,
    pub prior_deny: String,
    pub prior_exists: bool,
}

pub fn plan_lockdown(current: Option<&EveryoneOverwrite>) -> Result<LockdownPlan, MaskError> {
    let (prior_allow, prior_deny, prior_exists) = match current {
        Some(ow) => (ow.allow.clone(), ow.deny.clone(), true),
        None => ("0".to_owned(), "0".to_owned(), false),
    };
    let allow = parse_mask(&prior_allow)? & !SEND_MESSAGES_BIT;
    let deny = parse_mask(&prior_deny)? | SEND_MESSAGES_BIT;
    Ok(LockdownPlan {
        seed: LockdownSeed {
            prior_allow,
            prior_deny,
            prior_exists,
        },
        write: EveryoneOverwrite {
            allow: allow.to_string(),
            deny: deny.to_string(),
        },
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnlockPlan {
    Restore { allow: String, deny: String },
    DeleteOverwrite,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnlockError {
    #[error("channel is not locked down")]
    NotLocked,
    #[error(transparent)]
    InvalidMask(#[from] MaskError),
}

/// Restore only recorded state. An unlocked/untracked channel refuses without
/// changing permissions; do not guess at or clear an unrelated moderator deny.
pub fn plan_unlock(recorded: Option<&LockdownRecord>) -> Result<UnlockPlan, UnlockError> {
    let rec = recorded.ok_or(UnlockError::NotLocked)?;
    if !rec.prior_exists {
        return Ok(UnlockPlan::DeleteOverwrite);
    }
    parse_mask(&rec.prior_allow)?;
    parse_mask(&rec.prior_deny)?;
    Ok(UnlockPlan::Restore {
        allow: rec.prior_allow.clone(),
        deny: rec.prior_deny.clone(),
    })
}

#[must_use]
pub fn moderation_result_text(outcome: ChannelOutcome) -> String {
    match outcome {
        ChannelOutcome::Purged { affected } => {
            format!("Moderation action completed: purged ({affected}).")
        }
        other => format!("Moderation action completed: {}.", other.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn verbs_map_to_shared_moderation_actions() {
        for (verb, action) in [
            (ChannelModerationVerb::Purge, ModerationAction::Purge),
            (ChannelModerationVerb::Slowmode, ModerationAction::Slowmode),
            (ChannelModerationVerb::Lockdown, ModerationAction::Lockdown),
            (ChannelModerationVerb::Unlock, ModerationAction::Unlock),
        ] {
            assert_eq!(verb.action(), action);
        }
    }

    #[test]
    fn outcome_names_match_audit_mapping() {
        assert_eq!(ChannelOutcome::Purged { affected: 5 }.name(), "purged");
        assert_eq!(ChannelOutcome::SlowmodeUpdated.name(), "slowmode_updated");
        assert_eq!(ChannelOutcome::LockedDown.name(), "locked_down");
        assert_eq!(ChannelOutcome::Unlocked.name(), "unlocked");
    }

    #[test]
    fn purge_bounds_reject_before_any_discord_call() {
        for good in [1, 50, 100] {
            assert_eq!(validate_purge_count(Some(good)), Ok(good));
        }
        for bad in [None, Some(0), Some(101), Some(u64::MAX)] {
            assert_eq!(
                validate_purge_count(bad),
                Err(BoundsError {
                    field: "count",
                    min: 1,
                    max: 100
                })
            );
        }
    }

    #[test]
    fn slowmode_bounds_allow_zero_and_cap_at_6h() {
        for good in [0, 1, 21600] {
            assert_eq!(validate_slowmode_seconds(Some(good)), Ok(good));
        }
        for bad in [None, Some(21601), Some(u64::MAX)] {
            assert_eq!(
                validate_slowmode_seconds(bad),
                Err(BoundsError {
                    field: "seconds",
                    min: 0,
                    max: 21600
                })
            );
        }
    }

    #[test]
    fn reason_rule_is_the_shared_moderation_rule() {
        assert_eq!(require_channel_reason("  spam  "), Ok("spam".to_owned()));
        assert!(require_channel_reason("   ").is_err());
        assert!(require_channel_reason(&"x".repeat(513)).is_err());
    }

    #[test]
    fn lockdown_changes_only_send_messages_and_unlock_restores_exactly() {
        for (allow, deny) in [(0, 0), (1024, 8192), (3072, 8192), (u64::MAX, 0)] {
            let original = overwrite(&allow.to_string(), &deny.to_string());
            let plan = plan_lockdown(Some(&original)).expect("plans");
            assert_eq!(plan.seed.prior_allow, original.allow);
            assert_eq!(plan.seed.prior_deny, original.deny);
            assert!(plan.seed.prior_exists);
            assert_eq!(
                plan.write,
                overwrite(
                    &(allow & !SEND_MESSAGES_BIT).to_string(),
                    &(deny | SEND_MESSAGES_BIT).to_string(),
                )
            );
            let rec = recorded(&plan.seed.prior_allow, &plan.seed.prior_deny, true);
            assert_eq!(
                plan_unlock(Some(&rec)),
                Ok(UnlockPlan::Restore {
                    allow: original.allow,
                    deny: original.deny,
                })
            );
        }
    }

    #[test]
    fn lockdown_without_overwrite_unlocks_by_deleting_it() {
        let plan = plan_lockdown(None).expect("plans");
        assert_eq!(
            plan.seed,
            LockdownSeed {
                prior_allow: "0".to_owned(),
                prior_deny: "0".to_owned(),
                prior_exists: false,
            }
        );
        assert_eq!(plan.write, overwrite("0", "2048"));
        assert_eq!(
            plan_unlock(Some(&recorded("0", "0", false))),
            Ok(UnlockPlan::DeleteOverwrite)
        );
    }

    #[test]
    fn lockdown_rejects_corrupt_masks_before_any_write() {
        for bad in ["not-a-mask", "0x10", "-1", "18446744073709551616"] {
            assert_eq!(plan_lockdown(Some(&overwrite(bad, "0"))), Err(MaskError));
            assert_eq!(plan_lockdown(Some(&overwrite("0", bad))), Err(MaskError));
        }
    }

    #[test]
    fn unlock_of_unlocked_channel_refuses_without_permission_changes() {
        assert_eq!(plan_unlock(None), Err(UnlockError::NotLocked));
    }

    #[test]
    fn unlock_rejects_corrupt_recorded_masks() {
        assert_eq!(
            plan_unlock(Some(&recorded("bad", "0", true))),
            Err(UnlockError::InvalidMask(MaskError))
        );
        assert_eq!(
            plan_unlock(Some(&recorded("0", "bad", true))),
            Err(UnlockError::InvalidMask(MaskError))
        );
    }

    #[test]
    fn result_text_matches_legacy_format() {
        assert_eq!(
            moderation_result_text(ChannelOutcome::Purged { affected: 5 }),
            "Moderation action completed: purged (5)."
        );
        assert_eq!(
            moderation_result_text(ChannelOutcome::SlowmodeUpdated),
            "Moderation action completed: slowmode_updated."
        );
        assert_eq!(
            moderation_result_text(ChannelOutcome::LockedDown),
            "Moderation action completed: locked_down."
        );
        assert_eq!(
            moderation_result_text(ChannelOutcome::Unlocked),
            "Moderation action completed: unlocked."
        );
    }
}
