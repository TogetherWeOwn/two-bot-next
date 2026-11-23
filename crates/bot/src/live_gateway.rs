//! Ordered, dormant gateway intake for realtime voice and scheduled-event deltas.

use std::{collections::HashSet, time::Duration};

use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::{sync::watch, time::Instant};
use twilight_model::{
    gateway::payload::outgoing::RequestGuildMembers,
    id::{marker::GuildMarker, Id},
};
use two_bot_core::{duplicate_keys::reject_duplicate_keys, realtime_wire as wire};
use two_bot_discord::{realtime_baseline_join::BaselineJoin, realtime_gateway, realtime_roster};
use wire::{Change, ChannelKind, Operation, WireTime};

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

struct BaselineFence {
    members: HashSet<wire::Id>,
    rooms: HashSet<wire::Id>,
    events: HashSet<wire::Id>,
    lifetime: watch::Sender<()>,
}

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
                        members: topics.members.into_iter().map(|row| row.user_id).collect(),
                        rooms: topics
                            .channels
                            .into_iter()
                            .filter(|row| {
                                matches!(row.kind, ChannelKind::Voice | ChannelKind::Stage)
                            })
                            .map(|row| row.id)
                            .collect(),
                        events: topics.events.into_iter().map(|row| row.id).collect(),
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
            "VOICE_STATE_UPDATE" => {
                realtime_gateway::project(self.guild, kind, &object(payload)?).map_err(|_| ())?
            }
            "GUILD_SCHEDULED_EVENT_CREATE"
            | "GUILD_SCHEDULED_EVENT_UPDATE"
            | "GUILD_SCHEDULED_EVENT_DELETE" => {
                realtime_roster::project_scheduled_event(self.guild, raw).map_err(|_| ())?
            }
            _ => return Err(()),
        };
        let operation = operation.ok_or(())?;
        // Any later encoding error retires the entire generation.
        active.apply(&operation)?;
        let generation = active.lifetime.subscribe();
        self.seq = self.seq.checked_add(1).ok_or(())?;
        let body = wire::encode_delta(
            wire::Id(self.guild.into_nonzero()),
            self.seq,
            at,
            &[operation],
        )
        .ok_or(())?;
        self.sender.offer(QueuedDelta { body, generation });
        Ok(())
    }
}

impl BaselineFence {
    fn apply(&mut self, operation: &Operation) -> Result<(), ()> {
        match operation {
            Operation::Voice(Change::Upsert { id, value })
                if self.members.contains(id) && self.rooms.contains(&value.channel_id) => {}
            Operation::Voice(Change::Remove { id }) if self.members.contains(id) => {}
            Operation::Events(Change::Upsert { id, value }) => {
                if value.channel_id.is_some_and(|id| !self.rooms.contains(&id))
                    || (self.events.len() == wire::MAX_SNAPSHOT_EVENTS && !self.events.contains(id))
                {
                    return Err(());
                }
                self.events.insert(*id);
            }
            Operation::Events(Change::Remove { id }) => {
                self.events.remove(id);
            }
            _ => return Err(()),
        }
        Ok(())
    }
}

fn object<'a, T: Deserialize<'a>>(raw: &'a str) -> Result<T, ()> {
    if !raw.trim_start().starts_with('{') {
        return Err(());
    }
    serde_json::from_str(raw).map_err(|_| ())
}

#[cfg(test)]
mod tests;
