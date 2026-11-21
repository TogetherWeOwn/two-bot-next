//! Join the six realtime topics in gateway receive order without publishing them.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;
use serde_json::{value::RawValue, Value};
use twilight_model::{
    gateway::payload::outgoing::RequestGuildMembers,
    id::{marker::GuildMarker, Id},
};
use two_bot_core::{duplicate_keys::reject_duplicate_keys, realtime_wire as wire};
use wire::{Change, Operation, Topics, WireTime};

use crate::{
    realtime_gateway::{self, baseline::topology, ProjectionError},
    realtime_roster::intake::{BaselineIntake, BaselineRows, IntakeError},
};

#[derive(Debug, PartialEq, Eq)]
pub enum JoinError {
    Topology(ProjectionError),
    Roster(IntakeError),
    AmbiguousDispatch,
    InvalidTopics,
}
use JoinError::{AmbiguousDispatch, InvalidTopics, Roster, Topology};

/// Call `begin` on the configured guild's fresh GUILD_CREATE. One owner feeds
/// every subsequent frame in receive order, uses a fresh nonce per request and
/// retires on frame loss, request failure, timeout or session change.
pub struct BaselineJoin {
    guild_id: Id<GuildMarker>,
    pending: Option<Pending>,
}

struct Pending {
    intake: BaselineIntake,
    nonce: String,
    guild: wire::Guild,
    channels: Rows<wire::Channel>,
    roles: Rows<wire::Role>,
    voice: Rows<wire::Voice>,
}

#[derive(Deserialize)]
struct Dispatch<'a> {
    op: u8,
    t: Option<String>,
    #[serde(borrow)]
    d: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct GuildRowScope {
    id: GuildId,
}
#[derive(Deserialize)]
struct GuildScope {
    guild_id: GuildId,
}
#[derive(Deserialize)]
struct ChunkNonce {
    nonce: Option<String>,
}

type GuildId = Id<GuildMarker>;
type Rows<T> = BTreeMap<wire::Id, T>;
type JoinResult<T> = Result<T, JoinError>;

enum Step {
    Keep,
    Retire,
    Intake,
}
use Step::{Intake, Keep, Retire};

impl BaselineJoin {
    pub fn new(guild_id: Id<GuildMarker>) -> Self {
        Self {
            guild_id,
            pending: None,
        }
    }

    pub fn begin(&mut self, raw: &str, nonce: String) -> JoinResult<Option<RequestGuildMembers>> {
        self.retire();
        let Some(topology) = topology(self.guild_id, raw).map_err(Topology)? else {
            return Ok(None);
        };
        reject_duplicate_keys(raw.as_bytes()).map_err(|_| AmbiguousDispatch)?;
        if !topology.channels.iter().all(valid_channel) {
            return Err(InvalidTopics);
        }
        let mut intake = BaselineIntake::new(self.guild_id);
        let request = intake.begin(raw, nonce.clone()).map_err(Roster)?;
        Ok(request.inspect(|_| {
            self.pending = Some(Pending {
                intake,
                nonce,
                guild: topology.guild,
                channels: keyed(topology.channels, |row| row.id),
                roles: keyed(topology.roles, |row| row.id),
                voice: keyed(topology.voice, |row| row.user_id),
            });
        }))
    }

    pub fn retire(&mut self) {
        self.pending = None;
    }

    /// Emits a candidate once at the final matching chunk, without publishing.
    pub fn observe(&mut self, raw: &str, at: WireTime) -> JoinResult<Option<Topics>> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(None);
        };
        let rows = match pending.fold(self.guild_id, raw)? {
            Retire => return Ok(None),
            Intake => pending.intake.observe(raw).map_err(Roster)?,
            Keep => None,
        };
        let Some(BaselineRows { members, events }) = rows else {
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
        // The codec checks references and size at the longest cursor.
        let guild = wire::Id(self.guild_id.into_nonzero());
        wire::encode_snapshot(guild, wire::MAX_SEQ, at, &topics)
            .map(|_| Some(topics))
            .ok_or(InvalidTopics)
    }
}

impl Pending {
    fn fold(&mut self, guild: GuildId, raw: &str) -> JoinResult<Step> {
        let dispatch: Dispatch<'_> = object(raw)?;
        let kind = match (dispatch.op, dispatch.t.as_deref()) {
            (7 | 9, _) | (0, Some("READY" | "RESUMED")) => return Ok(Retire),
            (0, Some(kind)) => kind,
            (0, None) => return Err(AmbiguousDispatch),
            _ => return Ok(Keep),
        };
        let (guild_row, step) = match kind {
            "GUILD_CREATE" | "GUILD_DELETE" => (true, Retire),
            "GUILD_UPDATE" => (true, Keep),
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "CHANNEL_DELETE" => (false, Keep),
            "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" | "GUILD_ROLE_DELETE" => (false, Keep),
            "VOICE_STATE_UPDATE" => (false, Keep),
            "GUILD_MEMBERS_CHUNK"
            | "GUILD_MEMBER_ADD"
            | "GUILD_MEMBER_UPDATE"
            | "GUILD_MEMBER_REMOVE" => (false, Intake),
            "GUILD_SCHEDULED_EVENT_CREATE"
            | "GUILD_SCHEDULED_EVENT_UPDATE"
            | "GUILD_SCHEDULED_EVENT_DELETE" => (false, Intake),
            _ => return Ok(Keep),
        };
        // Typed headers reject duplicate scope keys before inspecting payloads.
        let raw = dispatch.d.ok_or(AmbiguousDispatch)?.get();
        let scope = if guild_row {
            object::<GuildRowScope>(raw)?.id
        } else {
            object::<GuildScope>(raw)?.guild_id
        };
        if scope != guild || kind == "GUILD_MEMBERS_CHUNK" && !self.matches_nonce(raw)? {
            return Ok(Keep);
        }
        reject_duplicate_keys(raw.as_bytes()).map_err(|_| AmbiguousDispatch)?;
        if matches!(step, Keep) {
            self.apply(guild, kind, &object(raw)?)?;
        }
        Ok(step)
    }

    fn matches_nonce(&self, raw: &str) -> JoinResult<bool> {
        Ok(object::<ChunkNonce>(raw)?.nonce.as_deref() == Some(self.nonce.as_str()))
    }

    fn apply(&mut self, guild: GuildId, kind: &str, body: &Value) -> JoinResult<()> {
        let overwrites = body["permission_overwrites"].as_array();
        let role_upsert = matches!(kind, "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE");
        if role_upsert && !body["role"].is_object()
            || overwrites.is_some_and(|rows| rows.iter().any(|row| !row.is_object()))
        {
            return Err(AmbiguousDispatch);
        }
        match realtime_gateway::project(guild, kind, body).map_err(Topology)? {
            Some(Operation::Guild(Change::Upsert { value, .. })) => self.guild = value,
            Some(Operation::Channels(mut change)) => {
                if let Change::Upsert { id, value } = &mut change {
                    if !valid_channel(value) {
                        return Err(InvalidTopics);
                    }
                    let update = kind == "CHANNEL_UPDATE";
                    let category = matches!(value.kind, wire::ChannelKind::Category);
                    if update && !category && body.get("parent_id").is_none() {
                        return Err(AmbiguousDispatch);
                    }
                    if update && body.get("position").is_none() {
                        value.position = self.channels.get(id).and_then(|row| row.position);
                    }
                }
                apply_change(&mut self.channels, change, 500)?;
            }
            Some(Operation::Roles(change)) => apply_change(&mut self.roles, change, 250)?,
            Some(Operation::Voice(change)) => apply_change(&mut self.voice, change, 10_000)?,
            _ => return Err(AmbiguousDispatch),
        }
        Ok(())
    }
}

fn object<'a, T: Deserialize<'a>>(raw: &'a str) -> JoinResult<T> {
    if !raw.trim_start().starts_with('{') {
        return Err(AmbiguousDispatch);
    }
    serde_json::from_str(raw).map_err(|_| AmbiguousDispatch)
}

fn valid_channel(row: &wire::Channel) -> bool {
    let mut targets = HashSet::new();
    let unique = |entry: &wire::Overwrite| targets.insert((entry.kind, entry.id));
    !row.name.is_empty()
        && row.name.encode_utf16().take(101).count() <= 100
        && row.overwrites.len() <= 100
        && row.overwrites.iter().all(unique)
        && (row.parent_id.is_none() || !matches!(row.kind, wire::ChannelKind::Category))
}

fn keyed<T>(rows: Vec<T>, id: impl Fn(&T) -> wire::Id) -> Rows<T> {
    rows.into_iter().map(|row| (id(&row), row)).collect()
}

fn apply_change<T>(rows: &mut Rows<T>, change: Change<T>, limit: usize) -> JoinResult<()> {
    let _ = match change {
        Change::Upsert { id, value } if rows.len() < limit || rows.contains_key(&id) => {
            rows.insert(id, value)
        }
        Change::Upsert { .. } => return Err(InvalidTopics),
        Change::Remove { id } => rows.remove(&id),
    };
    Ok(())
}

#[cfg(test)]
mod tests;
