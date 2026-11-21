//! Collect members and scheduled events at the same gateway receive position.

use std::collections::BTreeMap;

use twilight_model::gateway::payload::outgoing::RequestGuildMembers;

use super::{
    baseline::events_from_dispatch, decode, project_dispatch, wire, Change, Deserialize, Dispatch,
    GuildMarker, Id, Kind, Operation, ProjectionError,
};
use crate::realtime_member_baseline::{MemberBaseline, MemberBaselineError};

pub struct BaselineRows {
    pub members: Vec<wire::Member>,
    pub events: Vec<wire::Event>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IntakeError {
    Projection(ProjectionError),
    Chunk(MemberBaselineError),
    MemberChanged,
    InvalidEvents,
}

/// Owned by one guild's gateway receiver, not by spawned handlers. Feed original
/// dispatches strictly in receive order; deliver the completed rows in that same
/// turn, alongside independently reduced topology. This is not a snapshot or a
/// publication gate: full topic validation and generation/cursor fencing follow.
pub struct BaselineIntake {
    guild_id: Id<GuildMarker>,
    pending: Option<Pending>,
}

struct Pending {
    members: MemberBaseline,
    events: BTreeMap<wire::Id, wire::Event>,
}

#[derive(Deserialize)]
struct MemberCount {
    member_count: u64,
}

impl BaselineIntake {
    pub fn new(guild_id: Id<GuildMarker>) -> Self {
        Self {
            guild_id,
            pending: None,
        }
    }

    /// Replace the attempt on the configured guild's fresh `GUILD_CREATE`.
    /// Both the exact member count and the event list must be explicit on that
    /// dispatch. Supply a nonce unique across attempts and send only the returned
    /// full roster request. A failed start leaves no pending baseline.
    pub fn begin(
        &mut self,
        raw: &str,
        nonce: String,
    ) -> Result<Option<RequestGuildMembers>, IntakeError> {
        self.retire();
        let dispatch: Dispatch<'_> = decode(raw, '{').map_err(IntakeError::Projection)?;
        let Some(events) =
            events_from_dispatch(self.guild_id, &dispatch).map_err(IntakeError::Projection)?
        else {
            return Ok(None);
        };
        let body = dispatch.body().map_err(IntakeError::Projection)?;
        let count: MemberCount = decode(body, '{').map_err(IntakeError::Projection)?;
        if events.len() > wire::MAX_SNAPSHOT_EVENTS || !events.iter().all(wire::Event::is_valid) {
            return Err(IntakeError::InvalidEvents);
        }
        let Some((members, request)) =
            MemberBaseline::start(self.guild_id, count.member_count, nonce)
        else {
            return Ok(None);
        };
        self.pending = Some(Pending {
            members,
            events: events.into_iter().map(|event| (event.id, event)).collect(),
        });
        Ok(Some(request))
    }

    /// Discard on guild unavailability, gateway generation change, missed
    /// dispatch, timeout, request failure, or ambiguous topology change.
    pub fn retire(&mut self) {
        self.pending = None;
    }

    /// A projection or collection error consumes the attempt. Nonmatching chunks
    /// never finish a replacement attempt. Only its final chunk can emit rows.
    pub fn observe(&mut self, raw: &str) -> Result<Option<BaselineRows>, IntakeError> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(None);
        };
        let dispatch: Dispatch<'_> = decode(raw, '{').map_err(IntakeError::Projection)?;
        let kind = dispatch.kind().map_err(IntakeError::Projection)?;
        if matches!(kind, Some(Kind::GuildMembersChunk)) {
            let body = dispatch.body().map_err(IntakeError::Projection)?;
            pending
                .members
                .ingest_chunk(body)
                .map_err(IntakeError::Chunk)?;
            if pending.members.is_complete() {
                return Ok(pending.members.finish().map(|members| BaselineRows {
                    members,
                    events: pending.events.into_values().collect(),
                }));
            }
        } else {
            match project_dispatch(self.guild_id, kind, &dispatch)
                .map_err(IntakeError::Projection)?
            {
                Some(Operation::Members(_)) => return Err(IntakeError::MemberChanged),
                Some(Operation::Events(Change::Upsert { id, value })) => {
                    if !value.is_valid()
                        || (pending.events.len() == wire::MAX_SNAPSHOT_EVENTS
                            && !pending.events.contains_key(&id))
                    {
                        return Err(IntakeError::InvalidEvents);
                    }
                    pending.events.insert(id, value);
                }
                Some(Operation::Events(Change::Remove { id })) => {
                    pending.events.remove(&id);
                }
                _ => {}
            }
        }
        self.pending = Some(pending);
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
