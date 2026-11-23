//! Ordered, dormant gateway intake for realtime deltas.

use std::{collections::HashMap, time::Duration};

use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::{sync::watch, time::Instant};
use twilight_model::{
    gateway::payload::outgoing::RequestGuildMembers,
    id::{marker::GuildMarker, Id},
};
use two_bot_core::{duplicate_keys::reject_duplicate_keys, realtime_wire as wire};
use two_bot_discord::{realtime_baseline_join::BaselineJoin, realtime_gateway, realtime_roster};
use wire::{Change, ChannelKind, Operation, OverwriteKind, WireTime};

use crate::live_delivery::{self, Delivery};

const BASELINE_TIMEOUT: Duration = Duration::from_secs(30);
type GuildId = Id<GuildMarker>;

/// The default receive path has no collector, queue or member requests.
#[derive(Default)]
pub struct LiveGateway(Option<Intake>);

pub struct LiveReceiver(live_delivery::Receiver<QueuedDelta>);

struct QueuedDelta {
    body: Vec<u8>,
    generation: watch::Receiver<()>,
}

struct Intake {
    guild: GuildId,
    identified: bool,
    join: BaselineJoin,
    pending_until: Option<Instant>,
    active: Option<BaselineFence>,
    seq: u64,
    sender: live_delivery::Sender<QueuedDelta>,
}

/// Keep the references the hub will validate after each delta. The hub also
/// applies the channel, member and role removal cascades below.
struct BaselineFence {
    guild: wire::Guild,
    channels: Rows<wire::Channel>,
    roles: Rows<wire::Role>,
    members: Rows<wire::Member>,
    events: Rows<Option<wire::Id>>,
    lifetime: watch::Sender<()>,
}

type Rows<T> = HashMap<wire::Id, T>;

#[derive(Deserialize)]
struct Dispatch<'a> {
    op: u8,
    t: String,
    #[serde(borrow)]
    d: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct GuildRow {
    id: GuildId,
}

#[derive(Deserialize)]
struct GuildScope {
    guild_id: GuildId,
}

impl LiveGateway {
    /// Construct an isolated handoff. The cursor is in memory until durable
    /// reservation and snapshot publication are wired at the runtime boundary.
    pub fn for_guild(guild: GuildId, seq: u64) -> (Self, LiveReceiver) {
        let (sender, receiver) = live_delivery::channel();
        let intake = Intake {
            guild,
            identified: false,
            join: BaselineJoin::new(guild),
            pending_until: None,
            active: None,
            seq,
            sender,
        };
        (Self(Some(intake)), LiveReceiver(receiver))
    }

    pub fn enabled(&self) -> bool {
        self.0.is_some()
    }

    /// Retire before processing any further dispatch after a transport loss,
    /// session change or sequence gap. A resumed stream cannot prove a rebuild.
    pub fn disconnect(&mut self) {
        if let Some(intake) = &mut self.0 {
            intake.disconnect();
        }
    }

    /// Called by the shard owner for each accepted dispatch in receive order.
    pub fn receive(
        &mut self,
        raw: &str,
        at: WireTime,
        request_members: impl FnOnce(&RequestGuildMembers) -> bool,
    ) {
        let Some(intake) = &mut self.0 else { return };
        match intake.receive(raw, at) {
            Ok(Some(request)) if !request_members(&request) => intake.retire(),
            Err(()) => intake.retire(),
            _ => {}
        }
    }
}

impl LiveReceiver {
    /// Drive this borrowing worker alongside its receive owner. Every retry
    /// uses the same bytes, and retirement cancels pending attempts.
    pub async fn run(self, mut attempt: impl AsyncFnMut(&[u8]) -> Delivery) {
        self.0
            .run(async |delta| {
                let mut generation = delta.generation.clone();
                tokio::select! {
                    biased;
                    _ = generation.changed() => Delivery::Rejected,
                    result = attempt(&delta.body) => result,
                }
            })
            .await;
    }
}

impl Intake {
    fn disconnect(&mut self) {
        self.identified = false;
        self.retire();
    }

    fn retire(&mut self) {
        self.join.retire();
        self.pending_until = None;
        self.active = None;
    }

    fn receive(&mut self, raw: &str, at: WireTime) -> Result<Option<RequestGuildMembers>, ()> {
        let Ok(dispatch @ Dispatch { op: 0, .. }) = object::<Dispatch<'_>>(raw) else {
            self.disconnect();
            return Err(());
        };
        if self
            .pending_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.retire();
        }
        let kind = dispatch.t.as_str();
        match kind {
            "READY" | "RESUMED" => {
                self.disconnect();
                self.identified = kind == "READY";
            }
            _ if !self.identified => {}
            "GUILD_CREATE" | "GUILD_DELETE" => {
                if object::<GuildRow>(dispatch.d.ok_or(())?.get())?.id != self.guild {
                    return Ok(None);
                }
                self.retire();
                if kind == "GUILD_CREATE" {
                    let nonce = hex::encode(rand::random::<[u8; 16]>());
                    let request = self.join.begin(raw, nonce).map_err(|_| ())?;
                    self.pending_until =
                        request.as_ref().map(|_| Instant::now() + BASELINE_TIMEOUT);
                    return Ok(request);
                }
            }
            _ if self.pending_until.is_some() => {
                if let Some(topics) = self.join.observe(raw, at).map_err(|_| ())? {
                    self.active = Some(BaselineFence {
                        guild: topics.guild,
                        channels: index(topics.channels, |row| row.id),
                        roles: index(topics.roles, |row| row.id),
                        members: index(topics.members, |row| row.user_id),
                        events: topics
                            .events
                            .into_iter()
                            .map(|row| (row.id, row.channel_id))
                            .collect(),
                        lifetime: watch::channel(()).0,
                    });
                    self.pending_until = None;
                }
            }
            _ => self.forward(raw, &dispatch, at)?,
        }
        Ok(None)
    }

    fn forward(&mut self, raw: &str, dispatch: &Dispatch<'_>, at: WireTime) -> Result<(), ()> {
        let Some(active) = &mut self.active else {
            return Ok(());
        };
        let kind = dispatch.t.as_str();
        let body = dispatch.d;
        let scope = match kind.rsplit_once('_') {
            Some(("GUILD", "UPDATE")) => object::<GuildRow>(body.ok_or(())?.get())?.id,
            Some(
                ("VOICE_STATE", "UPDATE")
                | (
                    "CHANNEL" | "GUILD_ROLE" | "GUILD_MEMBER" | "GUILD_SCHEDULED_EVENT",
                    "CREATE" | "UPDATE" | "DELETE" | "ADD" | "REMOVE",
                ),
            ) => object::<GuildScope>(body.ok_or(())?.get())?.guild_id,
            _ => return Ok(()),
        };
        if scope != self.guild {
            return Ok(());
        }
        let payload = body.ok_or(())?.get();
        reject_duplicate_keys(payload.as_bytes()).map_err(|_| ())?;
        let operation = match kind {
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "CHANNEL_DELETE" => {
                realtime_gateway::project_live_channel(
                    kind,
                    &object(payload)?,
                    Some(&active.channels),
                )
                .map_err(|_| ())?
            }
            "GUILD_MEMBER_UPDATE" => Some(
                realtime_roster::project_member_update(&object(payload)?, |id| {
                    active.members.get(&id)
                })
                .map_err(|_| ())?,
            ),
            "GUILD_MEMBER_ADD" | "GUILD_MEMBER_REMOVE" => {
                realtime_roster::project(self.guild, raw).map_err(|_| ())?
            }
            "GUILD_SCHEDULED_EVENT_CREATE"
            | "GUILD_SCHEDULED_EVENT_UPDATE"
            | "GUILD_SCHEDULED_EVENT_DELETE" => {
                realtime_roster::project_scheduled_event(self.guild, raw).map_err(|_| ())?
            }
            _ => realtime_gateway::project(self.guild, kind, &object(payload)?).map_err(|_| ())?,
        };
        let Some(operation) = operation else {
            return Ok(());
        };
        let guild = wire::Id(self.guild.into_nonzero());
        let seq = self.seq.checked_add(1).ok_or(())?;
        let body = wire::encode_delta(guild, seq, at, std::slice::from_ref(&operation)).ok_or(())?;
        active.apply(guild, operation)?;
        self.seq = seq;
        let generation = active.lifetime.subscribe();
        self.sender.offer(QueuedDelta { body, generation });
        Ok(())
    }
}

impl BaselineFence {
    fn apply(&mut self, guild: wire::Id, operation: Operation) -> Result<(), ()> {
        match operation {
            Operation::Guild(Change::Upsert { value, .. })
                if self.members.contains_key(&value.owner_id)
                    && value
                        .afk_channel_id
                        .is_none_or(|id| self.kind(id) == Some(ChannelKind::Voice)) =>
            {
                self.guild = value;
            }
            Operation::Channels(Change::Upsert { id, value }) => self.upsert_channel(id, value)?,
            Operation::Channels(Change::Remove { id }) => {
                if self.guild.afk_channel_id == Some(id)
                    || self.channels.values().any(|row| row.parent_id == Some(id))
                {
                    return Err(());
                }
                self.channels.remove(&id);
                self.events.retain(|_, room| *room != Some(id));
            }
            Operation::Roles(Change::Upsert { id, value }) => {
                insert(&mut self.roles, id, value, wire::MAX_SNAPSHOT_ROLES)?;
            }
            Operation::Roles(Change::Remove { id }) => self.remove_role(guild, id)?,
            Operation::Members(Change::Upsert { id, value }) => {
                if !value.role_ids.iter().all(|id| self.roles.contains_key(id)) {
                    return Err(());
                }
                insert(&mut self.members, id, value, wire::MAX_SNAPSHOT_MEMBERS)?;
            }
            Operation::Members(Change::Remove { id }) if id != self.guild.owner_id => {
                self.members.remove(&id);
            }
            Operation::Voice(Change::Upsert { id, value })
                if self.members.contains_key(&id) && is_voice_room(self.kind(value.channel_id)) => {
            }
            Operation::Voice(Change::Remove { id }) if self.members.contains_key(&id) => {}
            Operation::Events(Change::Upsert { id, value }) => {
                if value
                    .channel_id
                    .is_some_and(|id| !is_voice_room(self.kind(id)))
                {
                    return Err(());
                }
                insert(
                    &mut self.events,
                    id,
                    value.channel_id,
                    wire::MAX_SNAPSHOT_EVENTS,
                )?;
            }
            Operation::Events(Change::Remove { id }) => {
                self.events.remove(&id);
            }
            _ => return Err(()),
        }
        Ok(())
    }

    fn kind(&self, id: wire::Id) -> Option<ChannelKind> {
        self.channels.get(&id).map(|row| row.kind)
    }

    fn upsert_channel(&mut self, id: wire::Id, row: wire::Channel) -> Result<(), ()> {
        // A change between tracked kinds can invalidate existing references.
        if self.kind(id).is_some_and(|previous| previous != row.kind)
            || row.parent_id.is_some_and(|parent| {
                matches!(row.kind, ChannelKind::Category)
                    || !matches!(self.kind(parent), Some(ChannelKind::Category))
            })
            || row.overwrites.iter().any(|entry| {
                entry.kind == OverwriteKind::Role && !self.roles.contains_key(&entry.id)
            })
        {
            return Err(());
        }
        insert(&mut self.channels, id, row, wire::MAX_SNAPSHOT_CHANNELS)
    }

    fn remove_role(&mut self, guild: wire::Id, id: wire::Id) -> Result<(), ()> {
        // Guessing how to remove a role overwrite can widen access.
        if id == guild
            || self.channels.values().any(|row| {
                row.overwrites
                    .iter()
                    .any(|entry| entry.kind == OverwriteKind::Role && entry.id == id)
            })
        {
            return Err(());
        }
        if self.roles.remove(&id).is_some() {
            for member in self.members.values_mut() {
                member.role_ids.retain(|role| *role != id);
            }
        }
        Ok(())
    }
}

fn is_voice_room(kind: Option<ChannelKind>) -> bool {
    matches!(kind, Some(ChannelKind::Voice | ChannelKind::Stage))
}

fn index<T>(rows: Vec<T>, key: impl Fn(&T) -> wire::Id) -> Rows<T> {
    rows.into_iter().map(|row| (key(&row), row)).collect()
}

fn insert<T>(rows: &mut Rows<T>, id: wire::Id, row: T, limit: usize) -> Result<(), ()> {
    if rows.len() == limit && !rows.contains_key(&id) {
        return Err(());
    }
    rows.insert(id, row);
    Ok(())
}

fn object<'a, T: Deserialize<'a>>(raw: &'a str) -> Result<T, ()> {
    if !raw.trim_start().starts_with('{') {
        return Err(());
    }
    serde_json::from_str(raw).map_err(|_| ())
}

#[cfg(test)]
mod tests;
