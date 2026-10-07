//! Admission policy for gateway interactions: who may hold a worker slot.
//!
//! The interaction lanes are shared and bounded, and most commands are open to
//! every member. Without a policy one member can occupy every slot and the next
//! moderator command is dropped with no callback. Three rules keep that from
//! happening:
//!
//! - **Per-member cap.** One member holds at most [`PER_USER_IN_FLIGHT`] slots.
//! - **Reserved capacity.** A command whose permission row demands guild
//!   permissions, invoked by a member who holds them, may use a reserved lane
//!   that open commands and component selects never touch.
//! - **Busy reply.** An event that cannot be admitted is answered with an
//!   ephemeral [`BUSY_REPLY`] on its own small lane instead of being dropped.
//!
//! Privilege is read from the invoker's resolved permission bits in the payload
//! (the same bits the router checks), so a member who lacks them cannot reach
//! the reserved lane by invoking a moderation command.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use two_bot_core::command_permissions::command_permission;

/// Interactions one member may have admitted at once. Covers a quick double
/// click without letting a single account hold the lane.
pub(crate) const PER_USER_IN_FLIGHT: usize = 3;

/// Ephemeral reply for an interaction that could not be admitted.
pub(crate) const BUSY_REPLY: &str = "The bot is busy right now. Try again in a few seconds.";

/// Per-member count of admitted interactions.
#[derive(Debug, Clone, Default)]
pub(crate) struct UserSlots(Arc<Mutex<HashMap<u64, usize>>>);

impl UserSlots {
    /// Claim a slot for `user`, or `None` when they already hold the cap.
    pub(crate) fn acquire(&self, user: u64) -> Option<UserSlot> {
        let mut counts = self.0.lock().expect("user slot counts");
        let held = counts.entry(user).or_default();
        if *held >= PER_USER_IN_FLIGHT {
            return None;
        }
        *held += 1;
        Some(UserSlot {
            counts: Arc::clone(&self.0),
            user,
        })
    }

    #[cfg(test)]
    pub(crate) fn tracked_users(&self) -> usize {
        self.0.lock().expect("user slot counts").len()
    }
}

/// A claimed slot; released on drop, including when the task is aborted.
#[derive(Debug)]
pub(crate) struct UserSlot {
    counts: Arc<Mutex<HashMap<u64, usize>>>,
    user: u64,
}

impl Drop for UserSlot {
    fn drop(&mut self) {
        let mut counts = self.counts.lock().expect("user slot counts");
        if let Some(held) = counts.get_mut(&self.user) {
            *held -= 1;
            if *held == 0 {
                counts.remove(&self.user);
            }
        }
    }
}

/// True for a builtin slash command that needs guild permissions and was
/// invoked by a member who holds them. Open commands, custom commands, selects
/// and anything without resolved permissions are not privileged.
pub(crate) fn is_privileged(interaction: &Interaction) -> bool {
    if interaction.kind != InteractionType::ApplicationCommand {
        return false;
    }
    let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref() else {
        return false;
    };
    let Some(row) = command_permission(&data.name) else {
        return false;
    };
    let actor = interaction
        .member
        .as_ref()
        .and_then(|member| member.permissions)
        .map(|permissions| permissions.bits());
    row.required_permissions != 0 && row.allows(actor)
}

/// Kinds that accept an ephemeral channel-message callback. Autocomplete and
/// ping interactions take a different response type, so they are never answered
/// with the busy text.
pub(crate) fn accepts_busy_reply(interaction: &Interaction) -> bool {
    matches!(
        interaction.kind,
        InteractionType::ApplicationCommand
            | InteractionType::MessageComponent
            | InteractionType::ModalSubmit
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_cap_holds_until_a_slot_is_released() {
        let slots = UserSlots::default();
        let held: Vec<_> = (0..PER_USER_IN_FLIGHT)
            .map(|_| slots.acquire(7).expect("below the cap"))
            .collect();
        assert!(slots.acquire(7).is_none(), "cap reached for this member");
        assert!(slots.acquire(8).is_some(), "other members are unaffected");
        drop(held);
        assert!(slots.acquire(7).is_some(), "released slots are reusable");
    }

    #[test]
    fn released_members_leave_no_residue() {
        let slots = UserSlots::default();
        let first = slots.acquire(1).expect("slot");
        let second = slots.acquire(2).expect("slot");
        assert_eq!(slots.tracked_users(), 2);
        drop((first, second));
        assert_eq!(slots.tracked_users(), 0, "counts never grow unbounded");
    }
}
