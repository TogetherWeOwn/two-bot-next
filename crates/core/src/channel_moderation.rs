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
/// Discord permission bits that let a member keep talking in a channel without
/// `SEND_MESSAGES`: reacting, opening threads and posting inside them.
pub const ADD_REACTIONS_BIT: u64 = 1 << 6;
pub const CREATE_PUBLIC_THREADS_BIT: u64 = 1 << 35;
pub const CREATE_PRIVATE_THREADS_BIT: u64 = 1 << 36;
pub const SEND_MESSAGES_IN_THREADS_BIT: u64 = 1 << 38;
/// Every bit `/lockdown` denies on the @everyone overwrite. `SEND_MESSAGES` is
/// the control bit that proves a channel is still locked; the rest close the
/// thread and reaction side doors a raider would otherwise keep using.
pub const LOCKDOWN_BITS: u64 = SEND_MESSAGES_BIT
    | ADD_REACTIONS_BIT
    | CREATE_PUBLIC_THREADS_BIT
    | CREATE_PRIVATE_THREADS_BIT
    | SEND_MESSAGES_IN_THREADS_BIT;

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

pub const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
/// Discord's bulk delete rejects the whole call (400) when any listed message
/// is older than 14 days.
pub const BULK_DELETE_MAX_AGE_MS: u64 = 14 * 24 * 60 * 60 * 1000;
/// Margin under the 14-day limit for clock skew and the time between the age
/// check and the call, so a message at the edge is deleted singly, not bulk.
pub const BULK_DELETE_MARGIN_MS: u64 = 60 * 60 * 1000;
/// Bulk delete takes between 2 and 100 messages.
const MIN_BULK_DELETE: usize = 2;

/// Creation time of a message, from the timestamp embedded in its snowflake.
#[must_use]
pub fn message_created_ms(message_id: u64) -> u64 {
    (message_id >> 22).saturating_add(DISCORD_EPOCH_MS)
}

/// How a purge deletes its messages, in call order: one bulk call for the
/// messages Discord still allows in it, then one delete per remaining message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgePlan {
    pub bulk: Vec<u64>,
    pub single: Vec<u64>,
}

/// Split listed message ids into a bulk batch and single deletes. A message too
/// old for bulk delete, or the lone message of a batch Discord would refuse,
/// goes to `single`. A future-dated id (clock skew) counts as recent.
#[must_use]
pub fn plan_purge(ids: &[u64], now_ms: u64) -> PurgePlan {
    let cutoff = now_ms.saturating_sub(BULK_DELETE_MAX_AGE_MS - BULK_DELETE_MARGIN_MS);
    let (bulk, single): (Vec<u64>, Vec<u64>) =
        ids.iter().partition(|id| message_created_ms(**id) > cutoff);
    if bulk.len() < MIN_BULK_DELETE {
        return PurgePlan {
            bulk: Vec::new(),
            single: bulk.into_iter().chain(single).collect(),
        };
    }
    PurgePlan { bulk, single }
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
    /// Opaque recovery generation assigned by the store at insert time.
    /// Repeated lockdowns preserve the generation while the live send deny
    /// remains set; a new lock cycle refreshes it alongside the seed.
    /// Cleanup must present it so a delayed unlock cannot
    /// delete a later lockdown cycle's recovery state. The runtime caller is
    /// responsible for serializing channel-scoped mutations (claim tokens
    /// fence only the request ledger, not this recovery row); this token
    /// fences only the cleanup half. Wiring follow-up: TOG-10174.
    pub recovery_generation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockdownPlan {
    pub seed: LockdownSeed,
    pub write: EveryoneOverwrite,
}

/// Live pre-lock recovery state. Preserve the first seed while the send deny
/// remains set; refresh it if an external edit has ended that lock cycle.
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
    let allow = parse_mask(&prior_allow)? & !LOCKDOWN_BITS;
    let deny = parse_mask(&prior_deny)? | LOCKDOWN_BITS;
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
    #[error("The channel overwrite changed since lockdown; no mutation attempted. Reconcile its SEND_MESSAGES permissions before unlocking.")]
    Drift,
}

/// Undo only the recorded lockdown change in a freshly read overwrite.
/// Preserve all other live bits, including denies added since lockdown. Refuse
/// if the live send bits no longer match our lock, rather than guessing who owns
/// the edit. Every other lockdown bit is restored only while it still holds the
/// locked state (allow clear, deny set); a thread or reaction bit edited since
/// is live state and kept, which also unlocks channels locked before those bits
/// were denied. Delete an originally absent entry only if no other bits remain.
pub fn plan_unlock(
    recorded: Option<&LockdownRecord>,
    current: Option<&EveryoneOverwrite>,
) -> Result<UnlockPlan, UnlockError> {
    let rec = recorded.ok_or(UnlockError::NotLocked)?;
    let prior_allow = parse_mask(&rec.prior_allow)?;
    let prior_deny = parse_mask(&rec.prior_deny)?;
    let current = current.ok_or(UnlockError::Drift)?;
    let allow = parse_mask(&current.allow)?;
    let deny = parse_mask(&current.deny)?;
    if allow & SEND_MESSAGES_BIT != 0 || deny & SEND_MESSAGES_BIT == 0 {
        return Err(UnlockError::Drift);
    }
    // Lockdown bits still in the locked state (allow clear, deny set).
    let restore = deny & !allow & LOCKDOWN_BITS;
    let allow = (allow & !restore) | (prior_allow & restore);
    let deny = (deny & !restore) | (prior_deny & restore);
    if !rec.prior_exists && allow == 0 && deny == 0 {
        return Ok(UnlockPlan::DeleteOverwrite);
    }
    Ok(UnlockPlan::Restore {
        allow: allow.to_string(),
        deny: deny.to_string(),
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
            recovery_generation: "test-generation".to_owned(),
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
    fn lockdown_changes_only_lockdown_bits_and_unlock_restores_exactly() {
        for (allow, deny) in [(0, 0), (1024, 8192), (3072, 8192), (u64::MAX, 0)] {
            let original = overwrite(&allow.to_string(), &deny.to_string());
            let plan = plan_lockdown(Some(&original)).expect("plans");
            assert_eq!(plan.seed.prior_allow, original.allow);
            assert_eq!(plan.seed.prior_deny, original.deny);
            assert!(plan.seed.prior_exists);
            assert_eq!(
                plan.write,
                overwrite(
                    &(allow & !LOCKDOWN_BITS).to_string(),
                    &(deny | LOCKDOWN_BITS).to_string(),
                )
            );
            let rec = recorded(&plan.seed.prior_allow, &plan.seed.prior_deny, true);
            assert_eq!(
                plan_unlock(Some(&rec), Some(&plan.write)),
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
        assert_eq!(plan.write, overwrite("0", &LOCKDOWN_BITS.to_string()));
        assert_eq!(
            plan_unlock(Some(&recorded("0", "0", false)), Some(&plan.write)),
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
        assert_eq!(plan_unlock(None, None), Err(UnlockError::NotLocked));
    }

    #[test]
    fn unlock_rejects_corrupt_recorded_masks() {
        assert_eq!(
            plan_unlock(
                Some(&recorded("bad", "0", true)),
                Some(&overwrite("0", "2048"))
            ),
            Err(UnlockError::InvalidMask(MaskError))
        );
        assert_eq!(
            plan_unlock(
                Some(&recorded("0", "bad", true)),
                Some(&overwrite("0", "2048"))
            ),
            Err(UnlockError::InvalidMask(MaskError))
        );
    }

    #[test]
    fn lockdown_bits_are_the_documented_discord_permission_bits() {
        assert_eq!(SEND_MESSAGES_BIT, 1 << 11);
        assert_eq!(ADD_REACTIONS_BIT, 1 << 6);
        assert_eq!(CREATE_PUBLIC_THREADS_BIT, 1 << 35);
        assert_eq!(CREATE_PRIVATE_THREADS_BIT, 1 << 36);
        assert_eq!(SEND_MESSAGES_IN_THREADS_BIT, 1 << 38);
        assert_eq!(LOCKDOWN_BITS.count_ones(), 5);
    }

    #[test]
    fn lockdown_denies_thread_and_reaction_bits_not_just_send_messages() {
        for current in [
            None,
            Some(overwrite("0", "0")),
            Some(overwrite("1024", "8192")),
        ] {
            let write = plan_lockdown(current.as_ref()).expect("plans").write;
            let deny = write.deny.parse::<u64>().unwrap();
            let allow = write.allow.parse::<u64>().unwrap();
            for bit in [
                SEND_MESSAGES_BIT,
                SEND_MESSAGES_IN_THREADS_BIT,
                CREATE_PUBLIC_THREADS_BIT,
                CREATE_PRIVATE_THREADS_BIT,
                ADD_REACTIONS_BIT,
            ] {
                assert_eq!(deny & bit, bit, "bit {bit} denied");
                assert_eq!(allow & bit, 0, "bit {bit} not allowed");
            }
        }
    }

    #[test]
    fn lockdown_removes_an_everyone_allow_of_a_thread_bit() {
        let original = overwrite(&SEND_MESSAGES_IN_THREADS_BIT.to_string(), "0");
        let plan = plan_lockdown(Some(&original)).expect("plans");
        assert_eq!(plan.write.allow, "0");
        let rec = recorded(&plan.seed.prior_allow, &plan.seed.prior_deny, true);
        assert_eq!(
            plan_unlock(Some(&rec), Some(&plan.write)),
            Ok(UnlockPlan::Restore {
                allow: original.allow,
                deny: "0".to_owned(),
            })
        );
    }

    #[test]
    fn unlock_restores_a_channel_locked_when_only_send_messages_was_denied() {
        // A lock written before thread and reaction bits were denied.
        let live = overwrite("1024", "10240");
        assert_eq!(
            plan_unlock(Some(&recorded("3072", "8192", true)), Some(&live)),
            Ok(UnlockPlan::Restore {
                allow: "3072".to_owned(),
                deny: "8192".to_owned(),
            })
        );
    }

    #[test]
    fn unlock_keeps_a_thread_bit_a_moderator_changed_during_the_lock() {
        let rec = recorded("0", "0", true);
        let locked = plan_lockdown(Some(&overwrite("0", "0")))
            .expect("plans")
            .write;
        let locked_allow = locked.allow.parse::<u64>().unwrap();
        let locked_deny = locked.deny.parse::<u64>().unwrap();
        // Threads reopened on purpose while sending stays locked.
        let edited = overwrite(
            &(locked_allow | SEND_MESSAGES_IN_THREADS_BIT).to_string(),
            &(locked_deny & !SEND_MESSAGES_IN_THREADS_BIT).to_string(),
        );
        assert_eq!(
            plan_unlock(Some(&rec), Some(&edited)),
            Ok(UnlockPlan::Restore {
                allow: SEND_MESSAGES_IN_THREADS_BIT.to_string(),
                deny: "0".to_owned(),
            })
        );
        // Reactions explicitly cleared to neutral: kept, not re-denied.
        let neutral = overwrite("0", &(locked_deny & !ADD_REACTIONS_BIT).to_string());
        assert_eq!(
            plan_unlock(Some(&rec), Some(&neutral)),
            Ok(UnlockPlan::Restore {
                allow: "0".to_owned(),
                deny: "0".to_owned(),
            })
        );
    }

    #[test]
    fn unlock_still_refuses_when_the_send_lock_drifted() {
        let rec = recorded("0", "0", true);
        let locked = plan_lockdown(Some(&overwrite("0", "0")))
            .expect("plans")
            .write;
        let deny = locked.deny.parse::<u64>().unwrap() & !SEND_MESSAGES_BIT;
        assert_eq!(
            plan_unlock(Some(&rec), Some(&overwrite("0", &deny.to_string()))),
            Err(UnlockError::Drift)
        );
    }

    fn snowflake_aged(now_ms: u64, age_ms: u64, sequence: u64) -> u64 {
        ((now_ms - age_ms - DISCORD_EPOCH_MS) << 22) | sequence
    }

    const NOW_MS: u64 = 1_800_000_000_000;
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    #[test]
    fn message_created_ms_reads_the_snowflake_timestamp() {
        let id = snowflake_aged(NOW_MS, 1_000, 7);
        assert_eq!(message_created_ms(id), NOW_MS - 1_000);
        assert_eq!(message_created_ms(0), DISCORD_EPOCH_MS);
    }

    #[test]
    fn purge_plan_sends_recent_messages_to_one_bulk_call_and_old_ones_singly() {
        let recent = [
            snowflake_aged(NOW_MS, 60_000, 1),
            snowflake_aged(NOW_MS, 3 * DAY_MS, 2),
        ];
        let old = [
            snowflake_aged(NOW_MS, 15 * DAY_MS, 3),
            snowflake_aged(NOW_MS, 400 * DAY_MS, 4),
        ];
        let ids = [recent[0], old[0], recent[1], old[1]];
        let plan = plan_purge(&ids, NOW_MS);
        assert_eq!(plan.bulk, recent);
        assert_eq!(plan.single, old);
    }

    #[test]
    fn purge_plan_never_bulk_deletes_a_message_near_the_fourteen_day_edge() {
        let edge = snowflake_aged(NOW_MS, 14 * DAY_MS - 30 * 60 * 1000, 1);
        let fresh = snowflake_aged(NOW_MS, 1_000, 2);
        let safe = snowflake_aged(NOW_MS, 14 * DAY_MS - 2 * 60 * 60 * 1000, 3);
        let plan = plan_purge(&[edge, fresh, safe], NOW_MS);
        assert_eq!(plan.bulk, [fresh, safe]);
        assert_eq!(plan.single, [edge]);
    }

    #[test]
    fn purge_plan_deletes_a_lone_recent_message_singly() {
        let recent = snowflake_aged(NOW_MS, 1_000, 1);
        let old = snowflake_aged(NOW_MS, 20 * DAY_MS, 2);
        let plan = plan_purge(&[old, recent], NOW_MS);
        assert!(plan.bulk.is_empty());
        assert_eq!(plan.single, [recent, old]);
        assert_eq!(plan_purge(&[recent], NOW_MS).single, [recent]);
        assert_eq!(
            plan_purge(&[], NOW_MS),
            PurgePlan {
                bulk: vec![],
                single: vec![]
            }
        );
    }

    #[test]
    fn purge_plan_treats_a_future_dated_id_as_recent() {
        let ahead = ((NOW_MS + 5_000 - DISCORD_EPOCH_MS) << 22) | 1;
        let fresh = snowflake_aged(NOW_MS, 1_000, 2);
        assert_eq!(plan_purge(&[ahead, fresh], NOW_MS).bulk, [ahead, fresh]);
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
