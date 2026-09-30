//! Pure V2 ownership decisions derived from `docs/voice-rooms.md`.
//!
//! Supply a complete, current membership snapshot for one room (after a join
//! or leave). This module performs no I/O and does not manage membership,
//! persistence, permissions, room deletion or interaction deduplication.

use std::collections::HashSet;

use crate::Snowflake;

/// The original creator can be absent and survives caretaker succession.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomOwnership {
    pub owner_id: Snowflake,
    pub original_creator_id: Snowflake,
}

/// One current occupant. Join times describe the current continuous stay, not
/// their first-ever visit. Bots never inherit or receive ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomMember {
    pub member_id: Snowflake,
    pub joined_at_ms: u64,
    pub is_bot: bool,
}

/// The caller supplies effective Manage Channels permission for this room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomActor {
    pub member_id: Snowflake,
    pub is_admin: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipRequest {
    /// Reconcile after membership changes or a restart; an absent owner is valid.
    Reconcile,
    /// Reclaim has its own eligibility rule, not an admin override.
    Reclaim { member_id: Snowflake },
    Transfer {
        actor: RoomActor,
        target_id: Snowflake,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipChange {
    Caretaker,
    Reclaimed,
    Transferred,
}

/// A plan only. Apply a change atomically against `previous`; an empty room is
/// handed back to V1 lifecycle management, never assigned to a bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipDecision {
    Unchanged(RoomOwnership),
    Changed {
        previous: RoomOwnership,
        next: RoomOwnership,
        reason: OwnershipChange,
    },
    EmptyRoom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OwnershipError {
    #[error("Member ids must be nonzero.")]
    InvalidMemberId,
    #[error("Member {0} appears more than once in the room snapshot.")]
    DuplicateMember(Snowflake),
    #[error("A bot cannot be the room owner or original creator.")]
    BotOwnership,
    #[error("The room has no human occupants.")]
    EmptyRoom,
    #[error("Only the room owner or an admin may use this command.")]
    NotOwner,
    #[error("You must be in the room to use this command.")]
    ActorNotInRoom,
    #[error("Bots cannot claim or transfer room ownership.")]
    BotActor,
    #[error("The transfer recipient must be in the room.")]
    TargetNotInRoom,
    #[error("The transfer recipient must be human.")]
    BotTarget,
    #[error("Only the original creator may reclaim while the owner is present.")]
    NotOriginalCreator,
}

/// The owner-only gate for transfer and future owner controls. Admins need
/// not be in the room; ordinary actors must be current human occupants.
pub fn require_room_owner(
    ownership: RoomOwnership,
    members: &[RoomMember],
    actor: RoomActor,
) -> Result<(), OwnershipError> {
    validate_snapshot(ownership, members)?;
    require_owner_actor(ownership, members, actor)
}

fn require_owner_actor(
    ownership: RoomOwnership,
    members: &[RoomMember],
    actor: RoomActor,
) -> Result<(), OwnershipError> {
    require_actor(members, actor)?;
    if actor.is_admin || actor.member_id == ownership.owner_id {
        Ok(())
    } else {
        Err(OwnershipError::NotOwner)
    }
}

/// Evaluate exactly one request against caller-supplied state. Refusals do
/// not change it. Reconciliation and already-applied eligible claims are
/// no-ops; replaying a transfer never bypasses the current authorization gate.
pub fn decide_ownership(
    ownership: RoomOwnership,
    members: &[RoomMember],
    request: OwnershipRequest,
) -> Result<OwnershipDecision, OwnershipError> {
    validate_snapshot(ownership, members)?;
    let earliest = members
        .iter()
        .filter(|m| !m.is_bot)
        .min_by_key(|m| (m.joined_at_ms, m.member_id));
    let Some(earliest) = earliest else {
        return match request {
            OwnershipRequest::Reconcile => Ok(OwnershipDecision::EmptyRoom),
            _ => Err(OwnershipError::EmptyRoom),
        };
    };
    let owner_present = member(members, ownership.owner_id).is_some();
    let (next, reason) = match request {
        OwnershipRequest::Reconcile => {
            if owner_present {
                return Ok(OwnershipDecision::Unchanged(ownership));
            }
            (
                RoomOwnership {
                    owner_id: earliest.member_id,
                    ..ownership
                },
                OwnershipChange::Caretaker,
            )
        }
        OwnershipRequest::Reclaim { member_id } => {
            require_actor(
                members,
                RoomActor {
                    member_id,
                    is_admin: false,
                },
            )?;
            if member_id != ownership.original_creator_id && owner_present {
                return Err(OwnershipError::NotOriginalCreator);
            }
            (
                RoomOwnership {
                    owner_id: member_id,
                    ..ownership
                },
                OwnershipChange::Reclaimed,
            )
        }
        OwnershipRequest::Transfer { actor, target_id } => {
            require_owner_actor(ownership, members, actor)?;
            require_id(target_id)?;
            let target = member(members, target_id).ok_or(OwnershipError::TargetNotInRoom)?;
            if target.is_bot {
                return Err(OwnershipError::BotTarget);
            }
            (
                RoomOwnership {
                    owner_id: target_id,
                    original_creator_id: target_id,
                },
                OwnershipChange::Transferred,
            )
        }
    };
    if next == ownership {
        Ok(OwnershipDecision::Unchanged(ownership))
    } else {
        Ok(OwnershipDecision::Changed {
            previous: ownership,
            next,
            reason,
        })
    }
}

fn member(members: &[RoomMember], member_id: Snowflake) -> Option<&RoomMember> {
    members.iter().find(|m| m.member_id == member_id)
}

fn require_id(member_id: Snowflake) -> Result<(), OwnershipError> {
    if member_id == 0 {
        Err(OwnershipError::InvalidMemberId)
    } else {
        Ok(())
    }
}

fn validate_snapshot(
    ownership: RoomOwnership,
    members: &[RoomMember],
) -> Result<(), OwnershipError> {
    require_id(ownership.owner_id)?;
    require_id(ownership.original_creator_id)?;
    let mut seen = HashSet::new();
    for occupant in members {
        require_id(occupant.member_id)?;
        if !seen.insert(occupant.member_id) {
            return Err(OwnershipError::DuplicateMember(occupant.member_id));
        }
        if occupant.is_bot
            && (occupant.member_id == ownership.owner_id
                || occupant.member_id == ownership.original_creator_id)
        {
            return Err(OwnershipError::BotOwnership);
        }
    }
    Ok(())
}

fn require_actor(members: &[RoomMember], actor: RoomActor) -> Result<(), OwnershipError> {
    require_id(actor.member_id)?;
    match member(members, actor.member_id) {
        Some(m) if m.is_bot => Err(OwnershipError::BotActor),
        None if !actor.is_admin => Err(OwnershipError::ActorNotInRoom),
        _ => Ok(()),
    }
}
