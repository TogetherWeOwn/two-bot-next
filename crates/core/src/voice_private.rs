//! Pure V3 privacy decisions (`/private`, `/public` and the Join-channel
//! request flow), written from `docs/voice-rooms.md` §V3 only.
//!
//! The caller supplies one room's privacy state, its current occupants and the
//! owner's display name, serializes events per room, and persists the returned
//! state atomically against the state it passed in. This module performs no
//! I/O, holds no Discord, store or clock types, and depends on no V1 lifecycle
//! state. It does not authorize `/private` or `/public`: gate those owner
//! commands with `voice_ownership::require_room_owner` first.

use std::collections::{BTreeMap, BTreeSet};

use crate::Snowflake;

/// Every Join channel name starts with this, followed by the owner's name.
pub const JOIN_CHANNEL_PREFIX: &str = "⇩ Join";

/// Discord's channel-name length limit, in characters.
pub const MAX_CHANNEL_NAME_CHARS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemberId(pub Snowflake);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(pub Snowflake);

/// Allocated by this core from `PrivateRoom::next_request_id` and never reused
/// within a room, so a button bound to an old request can never match a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinChannel {
    /// Creation was planned; the runtime has not reported the new channel yet.
    Requested,
    /// `name` is the last name the bot created or renamed the channel to.
    Created { id: ChannelId, name: String },
}

/// One pending request, bound to the owner it was raised to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinRequest {
    pub id: RequestId,
    pub member_id: MemberId,
    pub owner_id: MemberId,
}

/// Privacy state for one room. A public room has no Join channel, grants or
/// pending requests. The block list belongs to the room: it survives ownership
/// changes and privacy toggles and dies with the room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateRoom {
    pub room_id: ChannelId,
    pub owner_id: MemberId,
    /// Display name (or `/nick` name) the Join channel is named after.
    pub owner_display: String,
    pub private: bool,
    pub join_channel: Option<JoinChannel>,
    pub blocked: BTreeSet<MemberId>,
    /// Members holding a Connect allow on the room from an approved request.
    pub granted: BTreeSet<MemberId>,
    /// At most one pending request per member.
    pub pending: BTreeMap<MemberId, JoinRequest>,
    pub next_request_id: u64,
}

/// The durable part of one room's privacy state, as `voice_rooms` and
/// `voice_room_blocks` hold it: the private flag, the Join channel once it
/// exists, and the block list. Grants, pending requests and request ids stay
/// runtime-only: a restart forgets requests (the runtime keeps a pre-restart
/// button from matching a new request by binding buttons to a fresh worker
/// epoch) and which members were approved, but never the blocks or the Join
/// channel, and Discord keeps the approved members' Connect allow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrivacyRecord {
    pub private: bool,
    pub join_channel_id: Option<Snowflake>,
    pub blocked: BTreeSet<Snowflake>,
}

/// Discord work for the runtime, in order. Every effect names its target, so
/// the runtime never has to infer one from the state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivacyEffect {
    /// Deny Connect to @everyone on the room. View Channel is not touched, so
    /// the room stays visible.
    DenyEveryoneConnect {
        room_id: ChannelId,
    },
    /// Remove the @everyone Connect deny, restoring the room's prior access.
    RestoreEveryoneConnect {
        room_id: ChannelId,
    },
    /// Create a voice channel directly next to the room, then report it with
    /// `join_channel_created` (or `join_channel_creation_failed`).
    CreateJoinChannel {
        room_id: ChannelId,
        name: String,
    },
    RenameJoinChannel {
        channel_id: ChannelId,
        name: String,
    },
    DeleteJoinChannel {
        channel_id: ChannelId,
    },
    /// Allow Connect for this member on the room only.
    GrantConnect {
        room_id: ChannelId,
        member_id: MemberId,
    },
    /// Remove that member's Connect allow from the room.
    RevokeConnect {
        room_id: ChannelId,
        member_id: MemberId,
    },
    MoveMember {
        member_id: MemberId,
        room_id: ChannelId,
    },
    /// Show the owner Approve / Deny / Block buttons bound to this request.
    AskOwner {
        request: JoinRequest,
    },
    /// Retire the buttons of a request that can no longer be decided.
    WithdrawRequest {
        request: JoinRequest,
    },
}

/// A plan only: persist `room` against the state it was computed from, then
/// carry out `effects`. Refusals return an error and plan nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct PrivacyPlan {
    pub room: PrivateRoom,
    pub effects: Vec<PrivacyEffect>,
}

impl PrivacyPlan {
    #[must_use]
    pub fn is_noop(&self, previous: &PrivateRoom) -> bool {
        self.effects.is_empty() && self.room == *previous
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryOutcome {
    /// Not this room's current Join channel (deleted, replaced, or the room is
    /// public now). Ignored.
    StaleChannel,
    /// The owner, a current occupant or an approved member: no request.
    HasAccess,
    /// Silently ignored, with no message to anyone.
    Blocked,
    /// Deduplicated: the member's existing request stays the only one.
    AlreadyPending(JoinRequest),
    Raised(JoinRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinEntry {
    pub outcome: EntryOutcome,
    pub plan: PrivacyPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinDecision {
    Approve,
    Deny,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PrivacyError {
    #[error("Member and channel ids must be nonzero.")]
    InvalidId,
    #[error("The Join channel cannot be the room itself.")]
    JoinChannelIsRoom,
    #[error("A public room cannot have a Join channel, grants or pending requests.")]
    PublicRoomHasPrivateState,
    #[error("A blocked member cannot hold a Connect grant or a pending request.")]
    BlockedMemberHasAccess,
    #[error("A pending request does not match this room.")]
    InconsistentRequest,
    #[error("This room has run out of request ids.")]
    RequestIdsExhausted,
    #[error("Only the room owner can answer join requests.")]
    NotOwner,
    #[error("The room is public, so there is no request to answer.")]
    NotPrivate,
    #[error("This join request is no longer pending.")]
    RequestNotPending,
}

/// "⇩ Join ‹owner›", trimmed and cut to Discord's 100-character limit.
#[must_use]
pub fn join_channel_name(owner_display: &str) -> String {
    let display = owner_display.trim();
    if display.is_empty() {
        return JOIN_CHANNEL_PREFIX.to_owned();
    }
    let name: String = format!("{JOIN_CHANNEL_PREFIX} {display}")
        .chars()
        .take(MAX_CHANNEL_NAME_CHARS)
        .collect();
    name.trim_end().to_owned()
}

impl PrivateRoom {
    /// A new room starts public with an empty block list.
    #[must_use]
    pub fn new(room_id: ChannelId, owner_id: MemberId, owner_display: impl Into<String>) -> Self {
        Self {
            room_id,
            owner_id,
            owner_display: owner_display.into(),
            private: false,
            join_channel: None,
            blocked: BTreeSet::new(),
            granted: BTreeSet::new(),
            pending: BTreeMap::new(),
            next_request_id: 1,
        }
    }

    /// The name the Join channel should have for the current owner.
    #[must_use]
    pub fn join_channel_name(&self) -> String {
        join_channel_name(&self.owner_display)
    }

    /// Every transition validates first and never repairs a corrupt state.
    pub fn validate(&self) -> Result<(), PrivacyError> {
        require_channel(self.room_id)?;
        require_member(self.owner_id)?;
        if self.next_request_id == 0 {
            return Err(PrivacyError::InconsistentRequest);
        }
        if !self.private
            && (self.join_channel.is_some() || !self.granted.is_empty() || !self.pending.is_empty())
        {
            return Err(PrivacyError::PublicRoomHasPrivateState);
        }
        if let Some(JoinChannel::Created { id, .. }) = &self.join_channel {
            require_channel(*id)?;
            if *id == self.room_id {
                return Err(PrivacyError::JoinChannelIsRoom);
            }
        }
        for member in self.blocked.iter().chain(&self.granted) {
            require_member(*member)?;
        }
        if self.granted.iter().any(|m| self.blocked.contains(m)) {
            return Err(PrivacyError::BlockedMemberHasAccess);
        }
        let mut ids = BTreeSet::new();
        for (member, request) in &self.pending {
            require_member(*member)?;
            if self.blocked.contains(member) {
                return Err(PrivacyError::BlockedMemberHasAccess);
            }
            if request.member_id != *member
                || request.owner_id != self.owner_id
                || *member == self.owner_id
                || self.granted.contains(member)
                || request.id.0 == 0
                || request.id.0 >= self.next_request_id
                || !ids.insert(request.id)
            {
                return Err(PrivacyError::InconsistentRequest);
            }
        }
        Ok(())
    }

    /// The durable subset of this state. A Join channel still being created
    /// is not durable: the runtime persists it once it reports the channel.
    #[must_use]
    pub fn to_record(&self) -> PrivacyRecord {
        PrivacyRecord {
            private: self.private,
            join_channel_id: match &self.join_channel {
                Some(JoinChannel::Created { id, .. }) => Some(id.0),
                _ => None,
            },
            blocked: self.blocked.iter().map(|member| member.0).collect(),
        }
    }

    /// Rebuild a room's state from its stored record. The stored Join
    /// channel's name is unknown, so it is recorded empty and a later
    /// `set_owner` renames it once. A corrupt record is refused, never
    /// repaired.
    pub fn from_record(
        room_id: ChannelId,
        owner_id: MemberId,
        owner_display: impl Into<String>,
        record: &PrivacyRecord,
    ) -> Result<Self, PrivacyError> {
        let mut room = Self::new(room_id, owner_id, owner_display);
        room.private = record.private;
        room.join_channel = record.join_channel_id.map(|id| JoinChannel::Created {
            id: ChannelId(id),
            name: String::new(),
        });
        room.blocked = record
            .blocked
            .iter()
            .map(|member| MemberId(*member))
            .collect();
        room.validate()?;
        Ok(room)
    }

    /// `/private`: deny Connect to @everyone and plan the Join channel.
    /// Repeating it is a no-op; on a private room whose Join channel was lost
    /// it plans only a new Join channel.
    pub fn make_private(&self) -> Result<PrivacyPlan, PrivacyError> {
        self.validate()?;
        let mut next = self.clone();
        let mut effects = Vec::new();
        if !next.private {
            next.private = true;
            effects.push(PrivacyEffect::DenyEveryoneConnect {
                room_id: self.room_id,
            });
        }
        if next.join_channel.is_none() {
            next.join_channel = Some(JoinChannel::Requested);
            effects.push(PrivacyEffect::CreateJoinChannel {
                room_id: self.room_id,
                name: self.join_channel_name(),
            });
        }
        Ok(PrivacyPlan {
            room: next,
            effects,
        })
    }

    /// `/public`: restore access, revoke grants, delete the Join channel and
    /// withdraw pending requests. The block list is kept. Repeating it is a
    /// no-op. A Join channel still being created is deleted when reported.
    pub fn make_public(&self) -> Result<PrivacyPlan, PrivacyError> {
        self.validate()?;
        let mut next = self.clone();
        let mut effects = Vec::new();
        if next.private {
            next.private = false;
            effects.push(PrivacyEffect::RestoreEveryoneConnect {
                room_id: self.room_id,
            });
            effects.extend(
                std::mem::take(&mut next.granted)
                    .into_iter()
                    .map(|member_id| PrivacyEffect::RevokeConnect {
                        room_id: self.room_id,
                        member_id,
                    }),
            );
            if let Some(JoinChannel::Created { id, .. }) = next.join_channel.take() {
                effects.push(PrivacyEffect::DeleteJoinChannel { channel_id: id });
            }
            effects.extend(withdraw_all(&mut next.pending));
        }
        Ok(PrivacyPlan {
            room: next,
            effects,
        })
    }

    /// The runtime created a Join channel named `name`. Only the channel this
    /// room is waiting for is kept; any other (the room went public meanwhile,
    /// or a duplicate) is deleted. A name gone stale while it was being created
    /// is renamed. Replaying the current channel is a no-op.
    pub fn join_channel_created(
        &self,
        channel_id: ChannelId,
        name: &str,
    ) -> Result<PrivacyPlan, PrivacyError> {
        self.validate()?;
        require_channel(channel_id)?;
        if channel_id == self.room_id {
            return Err(PrivacyError::JoinChannelIsRoom);
        }
        let mut next = self.clone();
        let mut effects = Vec::new();
        match &self.join_channel {
            Some(JoinChannel::Created { id, .. }) if *id == channel_id => {}
            Some(JoinChannel::Requested) => {
                let desired = self.join_channel_name();
                if name != desired {
                    effects.push(PrivacyEffect::RenameJoinChannel {
                        channel_id,
                        name: desired.clone(),
                    });
                }
                next.join_channel = Some(JoinChannel::Created {
                    id: channel_id,
                    name: desired,
                });
            }
            _ => effects.push(PrivacyEffect::DeleteJoinChannel { channel_id }),
        }
        Ok(PrivacyPlan {
            room: next,
            effects,
        })
    }

    /// The planned creation failed: forget it so `/private` can plan it again.
    pub fn join_channel_creation_failed(&self) -> Result<PrivacyPlan, PrivacyError> {
        self.validate()?;
        let mut next = self.clone();
        if next.join_channel == Some(JoinChannel::Requested) {
            next.join_channel = None;
        }
        Ok(PrivacyPlan {
            room: next,
            effects: Vec::new(),
        })
    }

    /// Someone deleted the Join channel: quietly forget it. The room stays
    /// private; `/private` plans a new Join channel. Other ids are no-ops.
    pub fn join_channel_deleted(&self, channel_id: ChannelId) -> Result<PrivacyPlan, PrivacyError> {
        self.validate()?;
        let mut next = self.clone();
        if matches!(&next.join_channel, Some(JoinChannel::Created { id, .. }) if *id == channel_id)
        {
            next.join_channel = None;
        }
        Ok(PrivacyPlan {
            room: next,
            effects: Vec::new(),
        })
    }

    /// `member_id` entered `channel_id`. `occupants` are the room's current
    /// members; the runtime filters out bots before calling.
    pub fn enter_join_channel(
        &self,
        channel_id: ChannelId,
        member_id: MemberId,
        occupants: &[MemberId],
    ) -> Result<JoinEntry, PrivacyError> {
        self.validate()?;
        require_member(member_id)?;
        let mut next = self.clone();
        let mut effects = Vec::new();
        let is_current = matches!(&self.join_channel, Some(JoinChannel::Created { id, .. }) if *id == channel_id);
        let outcome = if !is_current {
            EntryOutcome::StaleChannel
        } else if member_id == self.owner_id {
            EntryOutcome::HasAccess
        } else if self.blocked.contains(&member_id) {
            EntryOutcome::Blocked
        } else if occupants.contains(&member_id) || self.granted.contains(&member_id) {
            EntryOutcome::HasAccess
        } else if let Some(request) = self.pending.get(&member_id) {
            EntryOutcome::AlreadyPending(*request)
        } else {
            let request = JoinRequest {
                id: RequestId(self.next_request_id),
                member_id,
                owner_id: self.owner_id,
            };
            next.next_request_id = self
                .next_request_id
                .checked_add(1)
                .ok_or(PrivacyError::RequestIdsExhausted)?;
            next.pending.insert(member_id, request);
            effects.push(PrivacyEffect::AskOwner { request });
            EntryOutcome::Raised(request)
        };
        Ok(JoinEntry {
            outcome,
            plan: PrivacyPlan {
                room: next,
                effects,
            },
        })
    }

    /// Answer a request. Only the current owner decides; admin status is not
    /// an input. A request raised to a previous owner or in an earlier private
    /// period is no longer pending and is refused.
    pub fn decide(
        &self,
        actor_id: MemberId,
        request_id: RequestId,
        decision: JoinDecision,
    ) -> Result<PrivacyPlan, PrivacyError> {
        self.validate()?;
        require_member(actor_id)?;
        if actor_id != self.owner_id {
            return Err(PrivacyError::NotOwner);
        }
        if !self.private {
            return Err(PrivacyError::NotPrivate);
        }
        let request = *self
            .pending
            .values()
            .find(|r| r.id == request_id)
            .ok_or(PrivacyError::RequestNotPending)?;
        let member_id = request.member_id;
        let mut next = self.clone();
        next.pending.remove(&member_id);
        let mut effects = Vec::new();
        match decision {
            JoinDecision::Approve => {
                next.granted.insert(member_id);
                effects.push(PrivacyEffect::GrantConnect {
                    room_id: self.room_id,
                    member_id,
                });
                effects.push(PrivacyEffect::MoveMember {
                    member_id,
                    room_id: self.room_id,
                });
            }
            JoinDecision::Deny => {}
            JoinDecision::Block => {
                next.blocked.insert(member_id);
            }
        }
        Ok(PrivacyPlan {
            room: next,
            effects,
        })
    }

    /// Record the current owner and display name after any ownership change
    /// (V2) or `/nick` update. Privacy, grants and the block list are kept;
    /// requests raised to the previous owner are withdrawn. The Join channel
    /// is renamed only when its name actually changes.
    pub fn set_owner(
        &self,
        owner_id: MemberId,
        owner_display: &str,
    ) -> Result<PrivacyPlan, PrivacyError> {
        self.validate()?;
        require_member(owner_id)?;
        let mut next = self.clone();
        let mut effects = Vec::new();
        next.owner_display = owner_display.to_owned();
        if owner_id != self.owner_id {
            next.owner_id = owner_id;
            effects.extend(withdraw_all(&mut next.pending));
        }
        let desired = next.join_channel_name();
        if let Some(JoinChannel::Created { id, name }) = &mut next.join_channel {
            if *name != desired {
                name.clone_from(&desired);
                effects.push(PrivacyEffect::RenameJoinChannel {
                    channel_id: *id,
                    name: desired,
                });
            }
        }
        Ok(PrivacyPlan {
            room: next,
            effects,
        })
    }

    /// The room is being deleted (or was deleted by hand): withdraw pending
    /// requests and delete the Join channel. The runtime then forgets this
    /// state; a Join channel still being created must be deleted on arrival.
    pub fn delete_room(&self) -> Result<Vec<PrivacyEffect>, PrivacyError> {
        self.validate()?;
        let mut pending = self.pending.clone();
        let mut effects = withdraw_all(&mut pending);
        if let Some(JoinChannel::Created { id, .. }) = &self.join_channel {
            effects.push(PrivacyEffect::DeleteJoinChannel { channel_id: *id });
        }
        Ok(effects)
    }
}

fn withdraw_all(pending: &mut BTreeMap<MemberId, JoinRequest>) -> Vec<PrivacyEffect> {
    std::mem::take(pending)
        .into_values()
        .map(|request| PrivacyEffect::WithdrawRequest { request })
        .collect()
}

fn require_member(member_id: MemberId) -> Result<(), PrivacyError> {
    if member_id.0 == 0 {
        Err(PrivacyError::InvalidId)
    } else {
        Ok(())
    }
}

fn require_channel(channel_id: ChannelId) -> Result<(), PrivacyError> {
    if channel_id.0 == 0 {
        Err(PrivacyError::InvalidId)
    } else {
        Ok(())
    }
}
