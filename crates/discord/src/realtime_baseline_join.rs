//! Join the six realtime topics in gateway receive order without publishing them.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{value::RawValue, Value};
use twilight_model::{
    gateway::payload::outgoing::RequestGuildMembers,
    id::{marker::GuildMarker, Id},
};
use two_bot_core::{duplicate_keys::reject_duplicate_keys, realtime_wire as wire};
use wire::{Change, Operation, Topics, WireTime};

use crate::{
    realtime_gateway::{self, ProjectionError},
    realtime_roster::intake::{BaselineIntake, BaselineRows, IntakeError},
};

#[derive(Debug, PartialEq, Eq)]
pub enum JoinError {
    Topology(ProjectionError),
    Roster(IntakeError),
    AmbiguousDispatch,
    InvalidTopics,
}

/// One configured guild on a single ordered gateway owner. Call `begin` only
/// on that guild's fresh GUILD_CREATE and feed all subsequent frames in order.
/// The owner must retire on a missed frame, request failure, timeout or session
/// change and use a different nonce for every request.
pub struct BaselineJoin {
    guild_id: Id<GuildMarker>,
    pending: Option<Pending>,
}

struct Pending {
    intake: BaselineIntake,
    guild: wire::Guild,
    channels: BTreeMap<wire::Id, wire::Channel>,
    roles: BTreeMap<wire::Id, wire::Role>,
    voice: BTreeMap<wire::Id, wire::Voice>,
}

#[derive(Deserialize)]
struct Dispatch<'a> {
    op: u8,
    t: Option<String>,
    #[serde(borrow)]
    d: Option<&'a RawValue>,
}

impl Dispatch<'_> {
    fn body(&self) -> Result<Value, JoinError> {
        let raw = self.d.ok_or(JoinError::AmbiguousDispatch)?.get();
        let value: Value = serde_json::from_str(raw).map_err(|_| JoinError::AmbiguousDispatch)?;
        if !value.is_object() || reject_duplicate_keys(raw.as_bytes()).is_err() {
            return Err(JoinError::AmbiguousDispatch);
        }
        Ok(value)
    }
}

impl BaselineJoin {
    pub fn new(guild_id: Id<GuildMarker>) -> Self {
        Self {
            guild_id,
            pending: None,
        }
    }

    pub fn begin(
        &mut self,
        raw: &str,
        nonce: String,
    ) -> Result<Option<RequestGuildMembers>, JoinError> {
        self.retire();
        let Some(topology) = realtime_gateway::baseline::topology(self.guild_id, raw)
            .map_err(JoinError::Topology)?
        else {
            return Ok(None);
        };
        if !topology.channels.iter().all(bounded_channel) {
            return Err(JoinError::InvalidTopics);
        }
        let mut intake = BaselineIntake::new(self.guild_id);
        let request = intake.begin(raw, nonce).map_err(JoinError::Roster)?;
        if request.is_some() {
            self.pending = Some(Pending {
                intake,
                guild: topology.guild,
                channels: keyed(topology.channels, |row| row.id),
                roles: keyed(topology.roles, |row| row.id),
                voice: keyed(topology.voice, |row| row.user_id),
            });
        }
        Ok(request)
    }

    pub fn retire(&mut self) {
        self.pending = None;
    }

    /// Emits a candidate once, at the final matching chunk's receive position.
    /// Consume it in that turn: this does not allocate a cursor or publish.
    pub fn observe(
        &mut self,
        raw: &str,
        observed_at: WireTime,
    ) -> Result<Option<Topics>, JoinError> {
        // Only an incomplete, valid attempt is restored. Errors, generation
        // changes and completion all discard both collectors together.
        let Some(mut pending) = self.pending.take() else {
            return Ok(None);
        };
        if pending.fold_topology(self.guild_id, raw)? {
            return Ok(None);
        }
        let Some(BaselineRows { members, events }) =
            pending.intake.observe(raw).map_err(JoinError::Roster)?
        else {
            self.pending = Some(pending);
            return Ok(None);
        };
        let topics = Topics {
            guild: pending.guild,
            channels: pending.channels.into_values().collect(),
            roles: pending.roles.into_values().collect(),
            members,
            voice: pending.voice.into_values().collect(),
            events,
        };
        // Reuse the codec to check references and size at the longest cursor.
        // Never expose the validation bytes or guess a cascade on deletion.
        wire::encode_snapshot(
            wire::Id(self.guild_id.into_nonzero()),
            wire::MAX_SEQ,
            observed_at,
            &topics,
        )
        .map(|_| Some(topics))
        .ok_or(JoinError::InvalidTopics)
    }
}

impl Pending {
    /// Returns true if the frame invalidates this gateway baseline.
    fn fold_topology(&mut self, guild_id: Id<GuildMarker>, raw: &str) -> Result<bool, JoinError> {
        if !raw.trim_start().starts_with('{') {
            return Err(JoinError::AmbiguousDispatch);
        }
        let dispatch: Dispatch<'_> =
            serde_json::from_str(raw).map_err(|_| JoinError::AmbiguousDispatch)?;
        match dispatch.op {
            0 => {}
            7 | 9 => return Ok(true), // reconnect or invalid session
            _ => return Ok(false),
        }
        match dispatch.t.as_deref().ok_or(JoinError::AmbiguousDispatch)? {
            "READY" | "RESUMED" => return Ok(true),
            "GUILD_CREATE" | "GUILD_DELETE" => {
                return Ok(guild_scope(&dispatch.body()?, "id")? == guild_id)
            }
            kind @ ("GUILD_UPDATE" | "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "CHANNEL_DELETE"
            | "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" | "GUILD_ROLE_DELETE"
            | "VOICE_STATE_UPDATE") => {
                self.apply(guild_id, kind, &dispatch.body()?)?;
            }
            _ => {}
        }
        Ok(false)
    }

    fn apply(
        &mut self,
        guild_id: Id<GuildMarker>,
        kind: &str,
        body: &Value,
    ) -> Result<(), JoinError> {
        let scope = if kind == "GUILD_UPDATE" {
            "id"
        } else {
            "guild_id"
        };
        if guild_scope(body, scope)? != guild_id {
            return Ok(());
        }
        if matches!(kind, "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE") && !body["role"].is_object()
            || kind == "CHANNEL_UPDATE" && body.get("parent_id").is_none()
            || body["permission_overwrites"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| !row.is_object()))
        {
            return Err(JoinError::AmbiguousDispatch);
        }
        match realtime_gateway::project(guild_id, kind, body).map_err(JoinError::Topology)? {
            Some(Operation::Guild(Change::Upsert { value, .. })) => self.guild = value,
            Some(Operation::Channels(change)) => {
                if matches!(&change, Change::Upsert { value, .. } if !bounded_channel(value)) {
                    return Err(JoinError::InvalidTopics);
                }
                apply_change(&mut self.channels, change, 500)?;
            }
            Some(Operation::Roles(change)) => apply_change(&mut self.roles, change, 250)?,
            Some(Operation::Voice(change)) => apply_change(&mut self.voice, change, 10_000)?,
            // Do not mistake an unprojectable same-guild change for no change.
            _ => return Err(JoinError::AmbiguousDispatch),
        }
        Ok(())
    }
}

fn guild_scope(body: &Value, field: &str) -> Result<Id<GuildMarker>, JoinError> {
    Id::deserialize(&body[field]).map_err(|_| JoinError::AmbiguousDispatch)
}

fn bounded_channel(row: &wire::Channel) -> bool {
    row.name.encode_utf16().take(101).count() <= 100
        && row.overwrites.len() <= 100
        && (row.parent_id.is_none() || !matches!(row.kind, wire::ChannelKind::Category))
}

fn keyed<T>(rows: Vec<T>, id: impl Fn(&T) -> wire::Id) -> BTreeMap<wire::Id, T> {
    rows.into_iter().map(|row| (id(&row), row)).collect()
}

fn apply_change<T>(
    rows: &mut BTreeMap<wire::Id, T>,
    change: Change<T>,
    limit: usize,
) -> Result<(), JoinError> {
    match change {
        Change::Upsert { id, value } => {
            if rows.len() == limit && !rows.contains_key(&id) {
                return Err(JoinError::InvalidTopics);
            }
            rows.insert(id, value);
        }
        Change::Remove { id } => {
            rows.remove(&id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
