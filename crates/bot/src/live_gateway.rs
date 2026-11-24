//! Ordered, dormant gateway intake for realtime deltas.

use std::{collections::HashMap, future::Future, time::Duration};

use serde::Deserialize;
use serde_json::value::RawValue;
use sqlx::PgPool;
use tokio::{
    sync::watch,
    time::{timeout, timeout_at, Instant},
};
use twilight_model::{
    gateway::payload::outgoing::RequestGuildMembers,
    id::{marker::GuildMarker, Id},
};
use two_bot_core::{
    duplicate_keys::reject_duplicate_keys, internal_actions::AuthorizedLiveSnapshotRead,
    realtime_wire as wire,
};
use two_bot_discord::{realtime_baseline_join::BaselineJoin, realtime_gateway, realtime_roster};
use wire::recovery::{Generation, Recovery};
use wire::watermark::{self, ReservationError};
use wire::{Change, ChannelKind, Operation, OverwriteKind, WireTime};

use crate::live_delivery::{self, Delivery};
use crate::live_transport::LiveTransport;

const BASELINE_TIMEOUT: Duration = Duration::from_secs(30);
const RESERVATION_TIMEOUT: Duration = Duration::from_secs(1);
type GuildId = Id<GuildMarker>;

/// The default receive path has no collector, queue or member requests.
pub struct LiveGateway<W = PgPool>(Option<Intake<W>>);

impl Default for LiveGateway {
    fn default() -> Self {
        Self(None)
    }
}

/// Storage for a single ordered owner. `reserve` must report success only for
/// a fresh, committed compare-and-advance in the same store as `read`.
pub trait SequenceWatermark: Send + Sync {
    fn read(&self, guild: wire::Id) -> impl Future<Output = Result<u64, ReservationError>> + Send;
    fn reserve(
        &self,
        guild: wire::Id,
        expected: u64,
        next: u64,
    ) -> impl Future<Output = Result<(), ReservationError>> + Send;
}

impl SequenceWatermark for PgPool {
    async fn read(&self, guild: wire::Id) -> Result<u64, ReservationError> {
        watermark::read(self, guild).await
    }

    async fn reserve(
        &self,
        guild: wire::Id,
        expected: u64,
        next: u64,
    ) -> Result<(), ReservationError> {
        watermark::reserve(self, guild, expected, next).await
    }
}

pub struct LiveReceiver(live_delivery::Receiver<QueuedDelta>);

struct QueuedDelta {
    body: Vec<u8>,
    generation: watch::Receiver<()>,
}

struct Intake<W> {
    guild: GuildId,
    identified: bool,
    join: BaselineJoin,
    pending_until: Option<Instant>,
    active: Option<BaselineFence>,
    watermark: W,
    sender: live_delivery::Sender<QueuedDelta>,
}

/// Keep the references the hub will validate after each delta. The hub also
/// applies the channel, member and role removal cascades below.
struct BaselineFence {
    seq: u64,
    observed_at: WireTime,
    recovery: Recovery,
    generation: Generation,
    guild: wire::Guild,
    channels: Rows<wire::Channel>,
    roles: Rows<wire::Role>,
    members: Rows<wire::Member>,
    voice: Rows<wire::Voice>,
    events: Rows<wire::Event>,
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

impl<W: SequenceWatermark> LiveGateway<W> {
    /// Construct a dormant handoff for the sole ordered owner of this guild.
    /// The first complete baseline and every rebuild read the durable cursor.
    pub fn for_guild(guild: GuildId, watermark: W) -> (Self, LiveReceiver) {
        let (sender, receiver) = live_delivery::channel();
        let intake = Intake {
            guild,
            identified: false,
            join: BaselineJoin::new(guild),
            pending_until: None,
            active: None,
            watermark,
            sender,
        };
        (Self(Some(intake)), LiveReceiver(receiver))
    }

    pub fn enabled(&self) -> bool {
        self.0.is_some()
    }

    /// Serve a post-replay read from this ordered owner. The shared borrow
    /// binds encoding to one committed cursor, observation time and guild.
    pub fn read_snapshot(
        &self,
        grant: AuthorizedLiveSnapshotRead,
        now: impl FnOnce() -> WireTime,
    ) -> Option<Vec<u8>> {
        let active = self.0.as_ref()?.active.as_ref()?;
        grant.read_current(
            &active.recovery,
            &active.generation,
            (active.seq, active.observed_at),
            now(),
            || active.topics(),
        )
    }

    /// Retire before processing any further dispatch after a transport loss,
    /// session change or sequence gap. A resumed stream cannot prove a rebuild.
    pub fn disconnect(&mut self) {
        if let Some(intake) = &mut self.0 {
            intake.disconnect();
        }
    }

    /// Called by the shard owner for each accepted dispatch in receive order.
    pub async fn receive(
        &mut self,
        raw: &str,
        at: WireTime,
        request_members: impl FnOnce(&RequestGuildMembers) -> bool,
    ) {
        let Some(intake) = &mut self.0 else { return };
        match intake.receive(raw, at).await {
            Ok(Some(request)) if !request_members(&request) => {
                intake.retire("member_request_refused")
            }
            Err(()) => intake.retire("dispatch_refused"),
            _ => {}
        }
    }
}

impl LiveReceiver {
    /// Drive signed Web delivery with the same bounds and retirement as the
    /// underlying receiver. The owner supplies the worker's shutdown future.
    pub async fn run_signed_until(self, shutdown: impl Future<Output = ()>, sink: &LiveTransport) {
        self.run_until(shutdown, async |body| sink.attempt(body).await)
            .await;
    }

    /// Drive this borrowing worker alongside its receive owner. Every retry
    /// uses the same bytes. Retirement skips queued deltas and cancels both
    /// pending attempts and retry delays for that generation.
    pub async fn run(self, mut attempt: impl AsyncFnMut(&[u8]) -> Delivery) {
        self.0
            .run_with_retirement(
                async |delta| {
                    let _ = delta.generation.clone().changed().await;
                },
                async |delta| attempt(&delta.body).await,
            )
            .await;
    }

    /// Run until the owning scope requests shutdown or the intake is dropped.
    /// Shutdown closes the queue and drops buffered work and any in-flight
    /// attempt. Drive the returned future directly so no task outlives it.
    pub async fn run_until(
        self,
        shutdown: impl Future<Output = ()>,
        attempt: impl AsyncFnMut(&[u8]) -> Delivery,
    ) {
        tokio::select! {
            biased;
            () = shutdown => {}
            () = self.run(attempt) => {}
        }
    }
}

impl<W: SequenceWatermark> Intake<W> {
    fn disconnect(&mut self) {
        self.identified = false;
        self.retire("disconnect");
    }

    fn retire(&mut self, reason: &'static str) {
        self.join.retire();
        let pending = self.pending_until.take().is_some();
        let active = self.active.take().is_some();
        if pending || active {
            tracing::info!(msg = "live_generation_retired", reason);
        }
    }

    async fn receive(
        &mut self,
        raw: &str,
        at: WireTime,
    ) -> Result<Option<RequestGuildMembers>, ()> {
        let Ok(dispatch @ Dispatch { op: 0, .. }) = object::<Dispatch<'_>>(raw) else {
            self.identified = false;
            return Err(());
        };
        if self
            .pending_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.retire("baseline_timeout");
        }
        let kind = dispatch.t.as_str();
        match kind {
            "READY" | "RESUMED" => {
                self.retire("session_changed");
                self.identified = kind == "READY";
            }
            _ if !self.identified => {}
            "GUILD_CREATE" | "GUILD_DELETE" => {
                if object::<GuildRow>(dispatch.d.ok_or(())?.get())?.id != self.guild {
                    return Ok(None);
                }
                self.retire("guild_baseline_changed");
                if kind == "GUILD_CREATE" {
                    let nonce = hex::encode(rand::random::<[u8; 16]>());
                    let request = self.join.begin(raw, nonce).map_err(|_| ())?;
                    self.pending_until =
                        request.as_ref().map(|_| Instant::now() + BASELINE_TIMEOUT);
                    return Ok(request);
                }
            }
            _ => {
                if let Some(until) = self.pending_until {
                    if let Some(topics) = self.join.observe(raw, at).map_err(|_| ())? {
                        let deadline = until.min(Instant::now() + RESERVATION_TIMEOUT);
                        timeout_at(deadline, self.publish_baseline(at, topics))
                            .await
                            .map_err(|_| ())??;
                    }
                } else {
                    self.forward(raw, &dispatch, at).await?;
                }
            }
        }
        Ok(None)
    }

    async fn publish_baseline(&mut self, at: WireTime, topics: wire::Topics) -> Result<(), ()> {
        let guild = wire::Id(self.guild.into_nonzero());
        let mut recovery = Recovery::new(guild);
        let generation = recovery.begin();
        let watermark = &self.watermark;
        let expected = watermark.read(guild).await.map_err(|_| ())?;
        let body = recovery
            .publish(
                &generation,
                expected,
                at,
                topics,
                async |guild, from, to| watermark.reserve(guild, from, to).await,
            )
            .await
            .map_err(|_| ())?;
        let baseline = recovery.baseline().ok_or(())?;
        let topics = &baseline.topics;
        let active = BaselineFence {
            seq: baseline.seq,
            observed_at: at,
            guild: topics.guild.clone(),
            channels: index(&topics.channels, |row| row.id),
            roles: index(&topics.roles, |row| row.id),
            members: index(&topics.members, |row| row.user_id),
            voice: index(&topics.voice, |row| row.user_id),
            events: index(&topics.events, |row| row.id),
            recovery,
            generation,
            lifetime: watch::channel(()).0,
        };
        self.pending_until = None;
        self.offer(active, body);
        Ok(())
    }

    async fn forward(
        &mut self,
        raw: &str,
        dispatch: &Dispatch<'_>,
        at: WireTime,
    ) -> Result<(), ()> {
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
        let expected = active.seq;
        let seq = expected + 1; // Active cursors are bounded by the wire codec.
        let body = wire::encode_delta(guild, seq, at, std::slice::from_ref(&operation)).ok_or(())?;
        active.apply(guild, operation)?;
        // Own the delivery fence across the await so dropping a possibly
        // committed reservation also drops the generation and snapshot.
        let mut active = self.active.take().ok_or(())?;
        let result = timeout(
            RESERVATION_TIMEOUT,
            self.watermark.reserve(guild, expected, seq),
        )
        .await;
        if !matches!(result, Ok(Ok(()))) {
            self.active = Some(active);
            return Err(());
        }
        active.seq = seq;
        active.observed_at = at;
        self.offer(active, body);
        Ok(())
    }

    fn offer(&mut self, active: BaselineFence, body: Vec<u8>) {
        let generation = active.lifetime.subscribe();
        self.active = Some(active);
        self.sender.offer(QueuedDelta { body, generation });
    }
}

impl BaselineFence {
    fn topics(&self) -> wire::Topics {
        wire::Topics {
            guild: self.guild.clone(),
            channels: self.channels.values().cloned().collect(),
            roles: self.roles.values().cloned().collect(),
            members: self.members.values().cloned().collect(),
            voice: self.voice.values().cloned().collect(),
            events: self.events.values().cloned().collect(),
        }
    }

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
                self.voice.retain(|_, row| row.channel_id != id);
                self.events.retain(|_, row| row.channel_id != Some(id));
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
                self.voice.remove(&id);
            }
            Operation::Voice(Change::Upsert { id, value })
                if self.members.contains_key(&id) && is_voice_room(self.kind(value.channel_id)) =>
            {
                self.voice.insert(id, value);
            }
            Operation::Voice(Change::Remove { id }) if self.members.contains_key(&id) => {
                self.voice.remove(&id);
            }
            Operation::Events(Change::Upsert { id, value }) => {
                if value
                    .channel_id
                    .is_some_and(|id| !is_voice_room(self.kind(id)))
                {
                    return Err(());
                }
                insert(&mut self.events, id, value, wire::MAX_SNAPSHOT_EVENTS)?;
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

fn index<T: Clone>(rows: &[T], key: impl Fn(&T) -> wire::Id) -> Rows<T> {
    rows.iter().map(|row| (key(row), row.clone())).collect()
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
