//! Ghost-channel cleanup planning for the voice cutover (TOG-13564).
//!
//! The sibling read-only count (TOG-13548) takes stock of tracked rooms vs
//! live channels: tracked-present, tracked-gone/forgotten, untracked-present.
//! This module turns that diff into a safe cleanup plan. It is pure: no I/O,
//! no Discord writes, no database writes — the operator CLI in
//! `two-bot-cutover` executes the plan, dry-run first.
//!
//! Safety contract (cutover staging rehearsal; no live guild changes without
//! an owner-approved rollout):
//!
//! * Tracked but gone (deleted by hand): forget the row. No Discord write.
//! * Tracked, present, manageable, empty: delete the channel, then forget.
//!   This mirrors the runtime [`reconcile`] empty rule exactly.
//! * Tracked but access lost, or still occupied: never touched — listed for
//!   the operator.
//! * Untracked live channels are NEVER deleted by this plan, even when empty.
//!   A hand-made lounge looks identical to an interim-bot leftover from a
//!   channel listing, so the plan lists them for manual triage instead of
//!   guessing. Claiming one (tracking it, or deleting it by hand) is an
//!   explicit operator act outside this tool.

use std::collections::HashSet;

use crate::voice_rooms::{reconcile, SeenChannel, VoiceRoom};
use crate::Snowflake;

/// The executable ghost-cleanup plan: [`reconcile`] output plus the
/// never-delete lists. `forget` + `delete` are the only mutating actions;
/// everything else is triage output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GhostCleanupPlan {
    /// Tracked but gone: drop the rows.
    pub forget: Vec<VoiceRoom>,
    /// Tracked, present, manageable, empty: delete these channels.
    pub delete: Vec<VoiceRoom>,
    /// Tracked, present, manageable, still occupied.
    pub occupied: Vec<VoiceRoom>,
    /// Tracked and present but access lost.
    pub suspended: Vec<VoiceRoom>,
    /// Live channels the bot never tracked. Never deleted; operator triage.
    pub untracked_present: Vec<Snowflake>,
}

impl GhostCleanupPlan {
    /// Mutating actions (Discord deletes + row forgets). The `--expect`
    /// count the operator confirms before `--execute`.
    #[must_use]
    pub fn action_count(&self) -> usize {
        self.forget.len() + self.delete.len()
    }

    /// Whether the plan changes anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.action_count() == 0
            && self.occupied.is_empty()
            && self.suspended.is_empty()
            && self.untracked_present.is_empty()
    }
}

/// Plan ghost cleanup for `tracked` rooms against the `seen` live snapshot.
/// Untracked channels are listed, never deleted; see the module contract.
#[must_use]
pub fn plan_ghost_cleanup(tracked: &[VoiceRoom], seen: &[SeenChannel]) -> GhostCleanupPlan {
    let base = reconcile(tracked, seen);
    let known: HashSet<Snowflake> = tracked.iter().map(|room| room.channel_id).collect();
    let live: HashSet<Snowflake> = seen.iter().map(|s| s.channel_id).collect();

    let mut occupied: Vec<VoiceRoom> = tracked
        .iter()
        .filter(|room| {
            !base.forget.iter().any(|r| r.channel_id == room.channel_id)
                && !base
                    .delete_empty
                    .iter()
                    .any(|r| r.channel_id == room.channel_id)
                && !base.suspend.iter().any(|r| r.channel_id == room.channel_id)
                && live.contains(&room.channel_id)
        })
        .cloned()
        .collect();
    occupied.sort_by_key(|room| room.channel_id);

    let mut untracked: Vec<Snowflake> = seen
        .iter()
        .map(|s| s.channel_id)
        .filter(|id| !known.contains(id))
        .collect();
    untracked.sort_unstable();

    let mut plan = GhostCleanupPlan {
        forget: base.forget,
        delete: base.delete_empty,
        occupied,
        suspended: base.suspend,
        untracked_present: untracked,
    };
    plan.forget.sort_by_key(|room| room.channel_id);
    plan.delete.sort_by_key(|room| room.channel_id);
    plan.suspended.sort_by_key(|room| room.channel_id);
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: Snowflake = 9;
    const MEMBER: Snowflake = 41;

    fn room(channel: Snowflake) -> VoiceRoom {
        VoiceRoom {
            guild_id: GUILD,
            channel_id: channel,
            creator_channel_id: 7,
            owner_id: MEMBER,
            original_creator_id: MEMBER,
            name_seed: 1,
            created_at: "2026-09-20T12:00:00.000Z".to_owned(),
        }
    }

    fn seen(channel: Snowflake, humans: usize, manageable: bool) -> SeenChannel {
        SeenChannel {
            channel_id: channel,
            human_occupants: humans,
            manageable,
        }
    }

    #[test]
    fn empty_inputs_plan_nothing() {
        let plan = plan_ghost_cleanup(&[], &[]);
        assert_eq!(plan.action_count(), 0);
        assert!(plan.is_empty());
    }

    #[test]
    fn tracked_gone_is_forget_never_delete() {
        let plan = plan_ghost_cleanup(&[room(101)], &[]);
        assert_eq!(plan.forget, vec![room(101)]);
        assert!(plan.delete.is_empty());
        assert_eq!(plan.action_count(), 1);
    }

    #[test]
    fn empty_manageable_tracked_is_delete() {
        let plan = plan_ghost_cleanup(&[room(101)], &[seen(101, 0, true)]);
        assert_eq!(plan.delete, vec![room(101)]);
        assert!(plan.forget.is_empty());
        assert_eq!(plan.action_count(), 1);
    }

    #[test]
    fn occupied_tracked_is_listed_never_deleted() {
        let plan = plan_ghost_cleanup(&[room(101)], &[seen(101, 2, true)]);
        assert!(plan.forget.is_empty() && plan.delete.is_empty());
        assert_eq!(plan.occupied, vec![room(101)]);
        assert_eq!(plan.action_count(), 0);
    }

    #[test]
    fn unmanageable_tracked_is_suspended_never_deleted() {
        let plan = plan_ghost_cleanup(&[room(101)], &[seen(101, 0, false)]);
        assert!(plan.forget.is_empty() && plan.delete.is_empty());
        assert_eq!(plan.suspended, vec![room(101)]);
        assert_eq!(plan.action_count(), 0);
    }

    #[test]
    fn untracked_channels_are_listed_never_deleted_even_when_empty() {
        let plan = plan_ghost_cleanup(&[], &[seen(201, 0, true), seen(202, 3, true)]);
        assert_eq!(plan.action_count(), 0);
        assert_eq!(plan.untracked_present, vec![201, 202]);
        assert!(!plan.is_empty());
    }

    #[test]
    fn mixed_guild_splits_every_class() {
        let tracked = vec![room(101), room(102), room(103), room(104), room(105)];
        let live = vec![
            seen(101, 0, true),
            seen(103, 1, true),
            seen(104, 0, false),
            seen(201, 0, true),
        ];
        // 102 and 105 are tracked but gone; 201 is untracked.
        let plan = plan_ghost_cleanup(&tracked, &live);
        assert_eq!(plan.delete, vec![room(101)]);
        assert_eq!(plan.forget, vec![room(102), room(105)]);
        assert_eq!(plan.occupied, vec![room(103)]);
        assert_eq!(plan.suspended, vec![room(104)]);
        assert_eq!(plan.untracked_present, vec![201]);
        assert_eq!(plan.action_count(), 3);
    }
}
