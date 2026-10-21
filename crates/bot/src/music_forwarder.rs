//! Staging-only Lavalink voice-update forwarder: the bot's first gateway sends.
//!
//! For the P0 external-voice-node spike (see
//! `docs/music-voice-node-spike.md`): it owns an independent [`VoicePairing`]
//! over the same dispatches the capture tap logs, `PATCH`es completed
//! handshakes to the node, `DELETE`s the player on leave, and sends
//! `UpdateVoiceState` (join/move/leave) through a gateway sender slot the
//! shard supervisor refreshes at boot and after every shard recreate.
//!
//! The pairing is deliberately independent from the capture tap's: the tap is
//! a dry-run log, this is the send path, and sharing one cache across the
//! two slices would couple their lifecycles. Both observe identical event
//! streams, so both complete identically; when this slice is proven a
//! follow-up may merge them.
//!
//! Staging fences stack three deep: exact `TWO_MUSIC=1` (else this decorator
//! is never built), a validated node config (else the inner sink passes
//! through unchanged), and the staging-guild check on every dispatch and
//! every send.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

use tokio::sync::mpsc;
use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_gateway::{Event, MessageSender};
use twilight_model::application::interaction::Interaction;
use twilight_model::gateway::payload::outgoing::UpdateVoiceState;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker},
    Id,
};
use two_bot::voice_rooms::VoiceEventSink;
use two_bot_core::music_node::{NodeClient, NodeConfig, NodeSession, PatchOutcome};
use two_bot_core::music_voice::{is_staging_guild, MusicGates, PlayerVoiceUpdate, VoicePairing};
use two_bot_core::Snowflake;

/// Staging-only auto-join target: the voice channel the staging bot joins on
/// `READY` while the spike runs. Flag-forwarded; unset disables auto-join
/// (the `PATCH` path still serves operator drag-ins).
pub const MUSIC_CHANNEL_VAR: &str = "TWO_MUSIC_CHANNEL_ID";

/// Worker queue bound. Voice dispatches are rare (joins, leaves, server
/// refreshes); a full queue means the node worker is wedged, and dropping
/// with a warning beats growing without bound.
const WORKER_QUEUE: usize = 64;

/// Parse an auto-join channel id: ASCII digits parsing to a nonzero `u64`,
/// mirroring the Worker's `isSnowflake` (leading zeros accepted, surrounding
/// whitespace trimmed). Anything else is `None` (auto-join off).
#[must_use]
pub fn parse_channel_id(raw: &str) -> Option<Snowflake> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<Snowflake>().ok().filter(|id| *id != 0)
}

/// Join `channel_id` in `guild_id`: unmuted (the node plays through us) and
/// deafened (we never listen), the discord.js bot convention.
#[must_use]
pub fn join_command(
    guild_id: Id<GuildMarker>,
    channel_id: Id<ChannelMarker>,
) -> UpdateVoiceState {
    UpdateVoiceState::new(guild_id, Some(channel_id), true, false)
}

/// Leave voice in `guild_id` (`None` channel).
#[must_use]
pub fn leave_command(guild_id: Id<GuildMarker>) -> UpdateVoiceState {
    UpdateVoiceState::new(guild_id, None::<Id<ChannelMarker>>, true, false)
}

/// True only when music is enabled and `guild_id` is the staging guild: the
/// fence every send passes, mirroring the per-dispatch capture fence.
#[must_use]
pub fn join_allowed(
    guild_id: Snowflake,
    staging_guild: Option<Snowflake>,
    music_enabled: bool,
) -> bool {
    music_enabled && is_staging_guild(guild_id, staging_guild)
}

/// Why a voice-command send was refused. Carries ids and fixed rules, never
/// tokens or passwords.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MusicJoinError {
    #[error("music voice send refused outside the staging guild")]
    NotStagingGuild,
    #[error("music voice send has no gateway sender yet")]
    NoSender,
    #[error("music voice send failed: the shard is gone")]
    SendFailed,
    #[error("music voice send refused a zero snowflake")]
    InvalidSnowflake,
}

/// One unit of node work, in gateway order.
#[derive(Debug)]
enum WorkerJob {
    /// `PATCH` a newly completed handshake.
    Patch {
        bot_id: Snowflake,
        update: PlayerVoiceUpdate,
    },
    /// `DELETE` the player after an observed self-leave.
    Delete {
        guild_id: Snowflake,
    },
    /// Re-`PATCH` every patched handshake (gateway `READY`/`RESUMED`: the
    /// pipeline drops its voice tracking on reconnect, so refresh the node).
    RePatch {
        bot_id: Snowflake,
    },
}

/// Voice-sink decorator driving the Lavalink node (staging only).
///
/// Built only when `TWO_MUSIC=1` with a validated node config (see
/// [`MusicForwarder::wrap`]); every dispatch and every send additionally
/// passes the staging-guild fence.
pub struct MusicForwarder {
    inner: Option<Arc<dyn VoiceEventSink>>,
    staging_guild: Option<Snowflake>,
    auto_join_channel: Option<Snowflake>,
    pairing: Mutex<VoicePairing>,
    /// Last handshake `PATCH`ed per guild: duplicate dispatches of the same
    /// handshake skip the node, and reconnects replay from here.
    patched: Arc<Mutex<HashMap<Snowflake, PlayerVoiceUpdate>>>,
    /// Set by [`MusicForwarder::leave`]: an observed leave then stays out
    /// instead of auto-rejoining. Only the follow-up `/stop` sets it today.
    manual_leave: AtomicBool,
    jobs: mpsc::Sender<WorkerJob>,
    /// The live gateway sender, refreshed at boot and after every shard
    /// recreate (a recreate mints a fresh channel, so a held sender would
    /// go stale). Empty until the first refresh: sends fail `NoSender`.
    sender: Arc<Mutex<Option<MessageSender>>>,
    /// Voice commands sent (observability, mirroring the tap's counters).
    commands_sent: AtomicU64,
    /// Last voice command sent (ids only; staging introspection).
    last_command: Mutex<Option<UpdateVoiceState>>,
}

impl MusicForwarder {
    /// Forwarder over `client` without consulting gates or config: tests
    /// build clients against mock nodes, and
    /// [`MusicForwarder::wrap_with_vars`] builds one after it validates.
    /// Spawns the node worker on the current runtime; without one (a sync
    /// caller) jobs queue-drop with a warning instead of panicking.
    pub fn with_client(
        inner: Option<Arc<dyn VoiceEventSink>>,
        staging_guild: Option<Snowflake>,
        auto_join_channel: Option<Snowflake>,
        client: NodeClient,
    ) -> Self {
        let (jobs, rx) = mpsc::channel(WORKER_QUEUE);
        let patched = Arc::new(Mutex::new(HashMap::new()));
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(worker_loop(rx, client, Arc::clone(&patched)));
            }
            Err(_) => {
                tracing::warn!(
                    msg = "music_worker_without_runtime",
                    "music forwarder started outside a tokio runtime; node jobs will drop"
                );
            }
        }
        Self {
            inner,
            staging_guild,
            auto_join_channel,
            pairing: Mutex::new(VoicePairing::new()),
            patched,
            manual_leave: AtomicBool::new(false),
            jobs,
            sender: Arc::new(Mutex::new(None)),
            commands_sent: AtomicU64::new(0),
            last_command: Mutex::new(None),
        }
    }

    /// Voice commands sent through the gateway sender so far.
    #[must_use]
    pub fn commands_sent(&self) -> u64 {
        self.commands_sent.load(Ordering::Relaxed)
    }

    /// The last voice command sent, if any (staging introspection).
    #[must_use]
    pub fn last_command(&self) -> Option<UpdateVoiceState> {
        self.last_command.lock().expect("music command lock").clone()
    }

    /// The configured auto-join channel, if the binding parsed.
    #[must_use]
    pub fn auto_join_channel(&self) -> Option<Snowflake> {
        self.auto_join_channel
    }

    /// Guilds with a recorded `PATCH` (staging introspection: players up).
    #[must_use]
    pub fn patched_count(&self) -> usize {
        self.patched.lock().expect("music patched lock").len()
    }

    /// True only for the staging guild (the per-dispatch and per-send fence).
    fn staging(&self, guild_id: Snowflake) -> bool {
        is_staging_guild(guild_id, self.staging_guild)
    }

    /// Queue node work, dropping with a warning when the worker is wedged
    /// (full) or gone (closed): a slow node must never stall the dispatch.
    fn enqueue(&self, job: WorkerJob) {
        if let Err(error) = self.jobs.try_send(job) {
            let cause = if error.is_full() { "queue_full" } else { "worker_gone" };
            tracing::warn!(msg = "music_worker_job_dropped", cause = cause);
        }
    }

    /// Queue a `PATCH` unless this exact handshake already went out: Discord
    /// redelivers halves (mute/deafen frames, region refreshes), and the
    /// node needs each distinct handshake once.
    fn enqueue_patch(&self, bot_id: Snowflake, update: PlayerVoiceUpdate) {
        let fresh = self
            .patched
            .lock()
            .expect("music patched lock")
            .get(&update.guild_id)
            != Some(&update);
        if fresh {
            self.enqueue(WorkerJob::Patch { bot_id, update });
        }
    }

    /// Join `channel_id` in `guild_id`: reset the pairing first (our own join
    /// always brings fresh halves, so a rejoin can never emit a stale mix),
    /// clear any manual leave, and send the gateway command.
    pub fn join(&self, guild_id: Snowflake, channel_id: Snowflake) -> Result<(), MusicJoinError> {
        if !self.staging(guild_id) {
            return Err(MusicJoinError::NotStagingGuild);
        }
        let (Some(guild), Some(channel)) =
            (Id::new_checked(guild_id), Id::new_checked(channel_id))
        else {
            return Err(MusicJoinError::InvalidSnowflake);
        };
        self.pairing.lock().expect("music pairing lock").reset(guild_id);
        self.manual_leave.store(false, Ordering::Relaxed);
        self.send_voice_command(join_command(guild, channel))
    }

    /// Leave voice in `guild_id` and stay out until the next [`MusicForwarder::join`]:
    /// the observed leave clears the pairing and `DELETE`s the player, and
    /// the manual flag suppresses the auto-rejoin.
    pub fn leave(&self, guild_id: Snowflake) -> Result<(), MusicJoinError> {
        if !self.staging(guild_id) {
            return Err(MusicJoinError::NotStagingGuild);
        }
        let Some(guild) = Id::new_checked(guild_id) else {
            return Err(MusicJoinError::InvalidSnowflake);
        };
        self.manual_leave.store(true, Ordering::Relaxed);
        self.send_voice_command(leave_command(guild))
    }

    /// Send one voice command through the live sender slot, recording it.
    fn send_voice_command(&self, command: UpdateVoiceState) -> Result<(), MusicJoinError> {
        let slot = self.sender.lock().expect("music sender lock");
        let Some(sender) = slot.as_ref() else {
            return Err(MusicJoinError::NoSender);
        };
        sender.command(&command).map_err(|_| MusicJoinError::SendFailed)?;
        drop(slot);
        self.commands_sent.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            msg = "music_voice_command",
            guild = command.d.guild_id.get(),
            channel = command.d.channel_id.map(twilight_model::id::Id::get),
            "music voice command sent"
        );
        *self.last_command.lock().expect("music command lock") = Some(command);
        Ok(())
    }

    /// Feed one dispatch into the pairing cache (staging guild, self only),
    /// queue node work on completions, and drive auto-join on `READY`.
    fn observe_music(&self, event: &Event, cache: &DefaultInMemoryCache) {
        let bot_id = cache.current_user().map(|user| user.id.get());
        match event {
            Event::Ready(_) => self.on_ready(cache, bot_id),
            Event::Resumed => {
                if let Some(bot) = bot_id {
                    self.enqueue(WorkerJob::RePatch { bot_id: bot });
                }
            }
            Event::VoiceServerUpdate(update) => {
                let guild_id = update.guild_id.get();
                if !self.staging(guild_id) {
                    return;
                }
                let Some(endpoint) = update.endpoint.as_deref() else {
                    return;
                };
                let mut pairing = self.pairing.lock().expect("music pairing lock");
                pairing.observe_server(guild_id, &update.token, endpoint);
                let complete = pairing.complete(guild_id);
                drop(pairing);
                if let (Some(bot), Some(update)) = (bot_id, complete) {
                    self.enqueue_patch(bot, update);
                }
            }
            Event::VoiceStateUpdate(update) => {
                let Some(guild) = update.guild_id else {
                    return;
                };
                let guild_id = guild.get();
                if !self.staging(guild_id) {
                    return;
                }
                if bot_id != Some(update.user_id.get()) {
                    return;
                }
                let channel = update.channel_id.map(|id| id.get());
                match channel {
                    Some(_) => {
                        let mut pairing = self.pairing.lock().expect("music pairing lock");
                        pairing.observe_state(guild_id, &update.session_id, channel);
                        let complete = pairing.complete(guild_id);
                        drop(pairing);
                        if let (Some(bot), Some(update)) = (bot_id, complete) {
                            self.enqueue_patch(bot, update);
                        }
                    }
                    None => self.on_self_leave(guild_id),
                }
            }
            _ => {}
        }
    }

    /// `READY`: replay patched handshakes, then auto-join when the bot is
    /// nowhere in voice. Never reset the pairing here (only our own sends
    /// reset it): a gateway reconnect keeps the voice session, and a reset
    /// would orphan a later region-move refresh with no state half to pair.
    fn on_ready(&self, cache: &DefaultInMemoryCache, bot_id: Option<Snowflake>) {
        let Some(bot) = bot_id else {
            return;
        };
        self.enqueue(WorkerJob::RePatch { bot_id: bot });
        let (Some(staging), Some(channel)) = (self.staging_guild, self.auto_join_channel) else {
            return;
        };
        let (Some(bot_id), Some(guild_id)) = (Id::new_checked(bot), Id::new_checked(staging))
        else {
            return;
        };
        if cache.voice_state(bot_id, guild_id).is_some() {
            return;
        }
        if let Err(error) = self.join(staging, channel) {
            tracing::warn!(msg = "music_auto_join_failed", cause = %error);
        }
    }

    /// Observed self-leave: clear the pairing, `DELETE` the player, and
    /// rejoin unless a manual leave asked us out. The spike protocol cycles
    /// joins by disconnecting the bot; only `/stop` (follow-up) stays out.
    fn on_self_leave(&self, guild_id: Snowflake) {
        self.pairing.lock().expect("music pairing lock").reset(guild_id);
        self.enqueue(WorkerJob::Delete { guild_id });
        if self.manual_leave.load(Ordering::Relaxed) {
            return;
        }
        let Some(channel) = self.auto_join_channel else {
            return;
        };
        if let Err(error) = self.join(guild_id, channel) {
            tracing::warn!(msg = "music_auto_rejoin_failed", cause = %error);
        }
    }
}

impl MusicForwarder {
    /// Wrap the voice sink, attaching the forwarder only when `TWO_MUSIC=1`
    /// with a validated node config. Gate off or node unconfigured: returns
    /// `inner` unchanged (same `Arc`, tap-only runtime).
    pub fn wrap(
        inner: Option<Arc<dyn VoiceEventSink>>,
        staging_guild: Option<Snowflake>,
    ) -> Option<Arc<dyn VoiceEventSink>> {
        Self::wrap_with_vars(inner, staging_guild, &std::env::vars().collect())
    }

    /// [`MusicForwarder::wrap`] with an explicit env map (tests, staged config).
    pub fn wrap_with_vars(
        inner: Option<Arc<dyn VoiceEventSink>>,
        staging_guild: Option<Snowflake>,
        vars: &HashMap<String, String>,
    ) -> Option<Arc<dyn VoiceEventSink>> {
        let Some((config, auto_join_channel)) = Self::attach_config(vars) else {
            return inner;
        };
        Some(Arc::new(Self::with_client(
            inner,
            staging_guild,
            auto_join_channel,
            NodeClient::new(config),
        )))
    }

    /// Validated attach inputs, or `None` when the inner sink passes through
    /// (gate off, or node unconfigured). A bad channel id warns and yields
    /// `None` for auto-join while the `PATCH` path stays live.
    fn attach_config(
        vars: &HashMap<String, String>,
    ) -> Option<(NodeConfig, Option<Snowflake>)> {
        if !MusicGates::from_map(vars).enabled {
            return None;
        }
        let config = match NodeConfig::from_map(vars) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(msg = "music_node_unconfigured", cause = %error);
                return None;
            }
        };
        let auto_join_channel = match vars.get(MUSIC_CHANNEL_VAR) {
            None => None,
            Some(raw) => match parse_channel_id(raw) {
                Some(channel) => Some(channel),
                None => {
                    tracing::warn!(msg = "music_channel_id_invalid");
                    None
                }
            },
        };
        Some((config, auto_join_channel))
    }
}

impl VoiceEventSink for MusicForwarder {
    fn set_command_identities(
        &self,
        identities: two_bot_discord::command_identity::CommandIdentities,
    ) {
        if let Some(inner) = self.inner.as_ref() {
            inner.set_command_identities(identities);
        }
    }

    fn handle(&self, event: &Event, cache: &DefaultInMemoryCache) {
        self.handle_received(event, cache, tokio::time::Instant::now());
    }

    fn handle_received(
        &self,
        event: &Event,
        cache: &DefaultInMemoryCache,
        received: tokio::time::Instant,
    ) {
        if let Some(inner) = self.inner.as_ref() {
            inner.handle_received(event, cache, received);
        }
        self.observe_music(event, cache);
    }

    fn presence(&self, update: &twilight_model::gateway::payload::incoming::PresenceUpdate) {
        if let Some(inner) = self.inner.as_ref() {
            inner.presence(update);
        }
    }

    fn disconnect(&self) {
        // Forward only: the node holds its own voice connection across our
        // transport loss, and `RESUMED` re-`PATCH`es it. Never tear down node
        // state here; a blip must not cost the player.
        if let Some(inner) = self.inner.as_ref() {
            inner.disconnect();
        }
    }

    fn needs_bootstrap(&self, cache: &DefaultInMemoryCache) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.needs_bootstrap(cache))
    }

    fn kick_vote(
        &self,
        interaction: Interaction,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        match self.inner.as_ref() {
            Some(inner) => inner.kick_vote(interaction),
            None => Box::pin(async { false }),
        }
    }

    fn refresh_gateway_sender(&self, sender: &MessageSender) {
        *self.sender.lock().expect("music sender lock") = Some(sender.clone());
    }
}

/// Serial node worker: one `PATCH`/`DELETE` at a time, in gateway order, so
/// a leave's `DELETE` can never overtake the join's `PATCH` behind it.
async fn worker_loop(
    mut jobs: mpsc::Receiver<WorkerJob>,
    client: NodeClient,
    patched: Arc<Mutex<HashMap<Snowflake, PlayerVoiceUpdate>>>,
) {
    let mut session: Option<NodeSession> = None;
    while let Some(job) = jobs.recv().await {
        match job {
            WorkerJob::Patch { bot_id, update } => {
                patch_one(&client, &mut session, &patched, bot_id, &update).await;
            }
            WorkerJob::Delete { guild_id } => {
                patched.lock().expect("music patched lock").remove(&guild_id);
                if let Some(current) = session.as_ref() {
                    let id = current.id().expose().clone();
                    if let Err(error) = client.destroy_player(&id, guild_id).await {
                        tracing::warn!(msg = "music_player_delete_failed", cause = %error);
                    }
                }
            }
            WorkerJob::RePatch { bot_id } => {
                let replay: Vec<PlayerVoiceUpdate> =
                    patched.lock().expect("music patched lock").values().cloned().collect();
                for update in replay {
                    patch_one(&client, &mut session, &patched, bot_id, &update).await;
                }
            }
        }
    }
}

/// Usable session id, reconnecting lazily when missing or dead. `None` drops
/// the job with a warning; the next job retries (the node may simply not be
/// up yet while staging provisions around us).
async fn ensure_session(
    client: &NodeClient,
    session: &mut Option<NodeSession>,
    bot_id: Snowflake,
) -> Option<String> {
    if !session.as_ref().is_some_and(NodeSession::is_live) {
        *session = None;
    }
    if session.is_none() {
        match client.connect_session(bot_id).await {
            Ok(fresh) => *session = Some(fresh),
            Err(error) => {
                tracing::warn!(msg = "music_session_connect_failed", cause = %error);
                return None;
            }
        }
    }
    session.as_ref().map(|live| live.id().expose().clone())
}

/// `PATCH` one handshake, reconnecting and retrying once on an unknown
/// session (the node restarted under us). Records successes for dedupe and
/// reconnect replays.
async fn patch_one(
    client: &NodeClient,
    session: &mut Option<NodeSession>,
    patched: &Arc<Mutex<HashMap<Snowflake, PlayerVoiceUpdate>>>,
    bot_id: Snowflake,
    update: &PlayerVoiceUpdate,
) {
    let Some(id) = ensure_session(client, session, bot_id).await else {
        return;
    };
    match client.patch_player_voice(&id, update).await {
        Ok(PatchOutcome::Updated) => {
            patched
                .lock()
                .expect("music patched lock")
                .insert(update.guild_id, update.clone());
        }
        Ok(PatchOutcome::SessionUnknown) => {
            *session = None;
            let Some(id) = ensure_session(client, session, bot_id).await else {
                return;
            };
            match client.patch_player_voice(&id, update).await {
                Ok(PatchOutcome::Updated) => {
                    patched
                        .lock()
                        .expect("music patched lock")
                        .insert(update.guild_id, update.clone());
                }
                Ok(PatchOutcome::SessionUnknown) => {
                    tracing::warn!(msg = "music_patch_unknown_after_reconnect");
                }
                Err(error) => {
                    tracing::warn!(msg = "music_patch_failed", cause = %error);
                }
            }
        }
        Err(error) => {
            tracing::warn!(msg = "music_patch_failed", cause = %error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twilight_gateway::{Intents, Shard, ShardId};

    const STAGING: Snowflake = 10;
    const OTHER_GUILD: Snowflake = 11;
    const CHANNEL: Snowflake = 30;
    const BOT: Snowflake = 999;
    const HUMAN: Snowflake = 20;

    fn vars_with(music: &str) -> HashMap<String, String> {
        [
            (String::from("TWO_MUSIC"), String::from(music)),
            (
                String::from(LAVALINK_URL_VAR),
                String::from("http://lavalink.internal:2333"),
            ),
            (
                String::from(LAVALINK_PASSWORD_VAR),
                String::from("fixture-node-password"),
            ),
        ]
        .into_iter()
        .collect()
    }

    fn server_event(guild: u64, token: &str, endpoint: Option<&str>) -> Event {
        Event::VoiceServerUpdate(
            serde_json::from_value(serde_json::json!({
                "guild_id": guild.to_string(),
                "token": token,
                "endpoint": endpoint,
            }))
            .expect("valid server fixture"),
        )
    }

    fn state_event(guild: u64, user: u64, session: &str, channel: Option<u64>) -> Event {
        Event::VoiceStateUpdate(Box::new(
            serde_json::from_value(serde_json::json!({
                "guild_id": guild.to_string(),
                "user_id": user.to_string(),
                "channel_id": channel.map(|c| c.to_string()),
                "session_id": session,
                "deaf": false, "mute": false, "self_deaf": false,
                "self_mute": false, "self_video": false, "suppress": false,
                "member": {"user": {"id": user.to_string(), "username": "x", "discriminator": "0", "bot": false},
                    "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false},
            }))
            .expect("valid state fixture"),
        ))
    }

    fn ready_event(bot: u64) -> Event {
        Event::Ready(Box::new(
            serde_json::from_value(serde_json::json!({
                "v": 10,
                "user": {"id": bot.to_string(), "username": "bot", "discriminator": "0", "bot": true},
                "session_id": "fixture-ready",
                "resume_gateway_url": "wss://gateway.discord.gg",
                "guilds": [],
                "application": {"id": "1111", "flags": 0}
            }))
            .expect("valid ready fixture"),
        ))
    }

    /// Cache carrying the bot identity (and only it): the sink path resolves
    /// selfhood from here, never from fixtures. Explicit resource types, like
    /// production: identity plus voice states, nothing else.
    fn cache_with_bot(bot: u64) -> DefaultInMemoryCache {
        let cache = DefaultInMemoryCache::builder()
            .resource_types(
                twilight_cache_inmemory::ResourceType::USER_CURRENT
                    | twilight_cache_inmemory::ResourceType::VOICE_STATE,
            )
            .build();
        cache.update(&ready_event(bot));
        cache
    }

    use std::collections::VecDeque;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_websockets::{Message as WsMessage, ServerBuilder};
    use two_bot_core::music_node::{LAVALINK_PASSWORD_VAR, LAVALINK_URL_VAR};

    /// Scripted Lavalink double on loopback: one listener routes REST (a
    /// minimal hand-rolled HTTP responder) and the session WebSocket (a real
    /// upgrade) by peeking the request line. No live node anywhere here.
    struct MockNode {
        base: String,
        state: Arc<MockState>,
    }

    struct MockState {
        patches: Mutex<Vec<Vec<u8>>>,
        patch_auth: Mutex<Vec<Option<String>>>,
        deletes: Mutex<Vec<String>>,
        patch_script: Mutex<VecDeque<u16>>,
        accepts: AtomicU64,
    }

    impl MockNode {
        async fn start() -> Self {
            Self::start_scripted(VecDeque::new()).await
        }

        /// Mock whose `PATCH` answers play back `patch_script` in order
        /// (default 204 once exhausted).
        async fn start_scripted(patch_script: VecDeque<u16>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback bind");
            let base = format!("http://{}", listener.local_addr().expect("loopback addr"));
            let state = Arc::new(MockState {
                patches: Mutex::new(Vec::new()),
                patch_auth: Mutex::new(Vec::new()),
                deletes: Mutex::new(Vec::new()),
                patch_script: Mutex::new(patch_script),
                accepts: AtomicU64::new(0),
            });
            let serving = Arc::clone(&state);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let state = Arc::clone(&serving);
                    tokio::spawn(async move {
                        serve_conn(stream, state).await;
                    });
                }
            });
            Self { base, state }
        }

        fn client(&self) -> NodeClient {
            NodeClient::new(
                NodeConfig::from_map(&HashMap::from([
                    (String::from(LAVALINK_URL_VAR), self.base.clone()),
                    (
                        String::from(LAVALINK_PASSWORD_VAR),
                        String::from("fixture-node-password"),
                    ),
                ]))
                .expect("mock config"),
            )
        }

        fn forwarder(&self, channel: Option<Snowflake>) -> MusicForwarder {
            MusicForwarder::with_client(None, Some(STAGING), channel, self.client())
        }

        fn patch_bodies(&self) -> Vec<serde_json::Value> {
            self.state
                .patches
                .lock()
                .expect("mock lock")
                .iter()
                .map(|raw| serde_json::from_slice(raw).expect("mock records JSON"))
                .collect()
        }
    }

    async fn serve_conn(stream: tokio::net::TcpStream, state: Arc<MockState>) {
        match peek_request_line(&stream).await {
            Some((method, path)) if method == "GET" && path == "/v4/websocket" => {
                serve_ws(stream, state).await;
            }
            Some(_) => serve_http(stream, state).await,
            None => {}
        }
    }

    /// The request line without consuming a byte (`peek` leaves the stream
    /// pristine for the WebSocket handshake that follows).
    async fn peek_request_line(stream: &TcpStream) -> Option<(String, String)> {
        let read = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = stream.peek(&mut buf).await.ok()?;
                if let Some(end) = buf[..n].windows(2).position(|w| w == b"\r\n") {
                    let line = String::from_utf8_lossy(&buf[..end]);
                    let mut parts = line.split(' ');
                    return Some((parts.next()?.to_owned(), parts.next()?.to_owned()));
                }
                if n >= buf.len() {
                    return None;
                }
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), read).await.ok()?
    }

    async fn serve_http(mut stream: TcpStream, state: Arc<MockState>) {
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_len = loop {
            let Ok(n) = stream.read(&mut chunk).await else {
                return;
            };
            if n == 0 {
                return;
            }
            raw.extend_from_slice(&chunk[..n]);
            if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            if raw.len() > 65536 {
                return;
            }
        };
        let head = String::from_utf8_lossy(&raw[..head_len]).into_owned();
        let mut request_parts = head.lines().next().unwrap_or("").split(' ');
        let (method, path) = (
            request_parts.next().unwrap_or(""),
            request_parts.next().unwrap_or("").to_owned(),
        );
        let header = |name: &str| {
            head.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
        };
        let content_len: usize = header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut body = raw[head_len..].to_vec();
        while body.len() < content_len {
            let Ok(n) = stream.read(&mut chunk).await else {
                return;
            };
            if n == 0 {
                return;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(content_len);
        let status: u16 = match method {
            "PATCH" => {
                state.patches.lock().expect("mock lock").push(body);
                state.patch_auth.lock().expect("mock lock").push(header("authorization"));
                state.patch_script.lock().expect("mock lock").pop_front().unwrap_or(204)
            }
            "DELETE" => {
                state.deletes.lock().expect("mock lock").push(path);
                204
            }
            _ => 404,
        };
        let reason = match status {
            200 => "OK",
            204 => "No Content",
            401 => "Unauthorized",
            404 => "Not Found",
            _ => "Error",
        };
        let response =
            format!("HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    }

    async fn serve_ws(stream: TcpStream, state: Arc<MockState>) {
        let Ok((_request, mut ws)) = ServerBuilder::new().accept(stream).await else {
            return;
        };
        // Distinct session id per accept: proves reconnects open fresh sessions.
        let n = state.accepts.fetch_add(1, Ordering::Relaxed);
        let ready = format!(r#"{{"op":"ready","resumed":false,"sessionId":"mock-session-{n}"}}"#);
        if ws.send(WsMessage::text(ready)).await.is_err() {
            return;
        }
        while ws.next().await.is_some() {}
    }

    /// Poll `done` until true, failing the test (not hanging it) on timeout.
    async fn poll_until(note: &str, mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect(note);
    }

    /// Live gateway sender for send-path tests: a real `Shard` that never
    /// connects (its channel buffers our commands). The caller holds the
    /// shard: dropping it closes the channel, which is the `SendFailed` case.
    fn live_sender() -> (Shard, MessageSender) {
        crate::gateway::ensure_crypto_provider();
        let shard = Shard::new(ShardId::ONE, String::from("fixture"), Intents::empty());
        let sender = shard.sender();
        (shard, sender)
    }

    /// Recording inner sink: proves delegation call by call.
    #[derive(Default)]
    struct FakeInner {
        calls: Mutex<Vec<String>>,
        bootstrap: AtomicBool,
        vote: AtomicBool,
    }

    impl FakeInner {
        fn record(&self, what: &str) {
            self.calls.lock().expect("fake lock").push(what.to_owned());
        }

        fn called(&self, what: &str) -> bool {
            self.calls.lock().expect("fake lock").iter().any(|call| call == what)
        }
    }

    impl VoiceEventSink for FakeInner {
        fn set_command_identities(
            &self,
            _identities: two_bot_discord::command_identity::CommandIdentities,
        ) {
            self.record("identities");
        }

        fn handle(&self, _event: &Event, _cache: &DefaultInMemoryCache) {
            self.record("handle");
        }

        fn handle_received(
            &self,
            _event: &Event,
            _cache: &DefaultInMemoryCache,
            _received: tokio::time::Instant,
        ) {
            self.record("handle_received");
        }

        fn presence(&self, _update: &twilight_model::gateway::payload::incoming::PresenceUpdate) {
            self.record("presence");
        }

        fn disconnect(&self) {
            self.record("disconnect");
        }

        fn needs_bootstrap(&self, _cache: &DefaultInMemoryCache) -> bool {
            self.record("needs_bootstrap");
            self.bootstrap.load(Ordering::Relaxed)
        }

        fn kick_vote(
            &self,
            _interaction: Interaction,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
            self.record("kick_vote");
            let vote = self.vote.load(Ordering::Relaxed);
            Box::pin(async move { vote })
        }
    }

    #[test]
    fn builders_shape_join_and_leave() {
        let guild = Id::new(STAGING);
        let channel = Id::new(CHANNEL);
        let join = join_command(guild, channel);
        assert_eq!(join.d.guild_id, guild);
        assert_eq!(join.d.channel_id, Some(channel));
        assert!(join.d.self_deaf);
        assert!(!join.d.self_mute);
        let wire: serde_json::Value = serde_json::to_value(&join).expect("command serializes");
        assert_eq!(wire["op"], 4);
        assert_eq!(wire["d"]["guild_id"], STAGING.to_string());
        assert_eq!(wire["d"]["channel_id"], CHANNEL.to_string());
        let leave = leave_command(guild);
        assert_eq!(leave.d.guild_id, guild);
        assert_eq!(leave.d.channel_id, None);
        let wire: serde_json::Value = serde_json::to_value(&leave).expect("command serializes");
        assert_eq!(wire["d"]["channel_id"], serde_json::Value::Null);
    }

    #[test]
    fn channel_id_parses_snowflakes_only() {
        assert_eq!(parse_channel_id("123456789012345678"), Some(123456789012345678));
        assert_eq!(parse_channel_id("007"), Some(7), "leading zeros accepted");
        assert_eq!(parse_channel_id("  42  "), Some(42), "trimmed like isSnowflake");
        assert_eq!(
            parse_channel_id("18446744073709551615"),
            Some(18446744073709551615)
        );
        for raw in ["", "  ", "0", "00", "abc", "12a34", "18446744073709551616", "-1", "+1"] {
            assert_eq!(parse_channel_id(raw), None, "refuse {raw:?}");
        }
    }

    #[test]
    fn join_fence_needs_music_and_staging() {
        assert!(join_allowed(STAGING, Some(STAGING), true));
        assert!(!join_allowed(OTHER_GUILD, Some(STAGING), true));
        assert!(!join_allowed(STAGING, Some(OTHER_GUILD), true));
        assert!(!join_allowed(STAGING, None, true));
        assert!(!join_allowed(STAGING, Some(STAGING), false));
    }

    #[test]
    fn wrap_passes_through_without_gate_or_node() {
        let inner = Arc::new(FakeInner::default());
        let held: Arc<dyn VoiceEventSink> = Arc::clone(&inner) as Arc<dyn VoiceEventSink>;
        // Gate off: same Arc back.
        let out = MusicForwarder::wrap_with_vars(
            Some(Arc::clone(&held)),
            Some(STAGING),
            &vars_with("0"),
        )
        .expect("passthrough");
        assert!(Arc::ptr_eq(&out, &held));
        // Gate on but node unconfigured: same Arc back.
        let out = MusicForwarder::wrap_with_vars(
            Some(Arc::clone(&held)),
            Some(STAGING),
            &HashMap::from([(String::from("TWO_MUSIC"), String::from("1"))]),
        )
        .expect("passthrough");
        assert!(Arc::ptr_eq(&out, &held));
    }

    #[test]
    fn attach_config_parses_channel_and_warns_through_invalid() {
        let mut vars = vars_with("1");
        assert_eq!(MusicForwarder::attach_config(&vars).expect("attach").1, None);
        vars.insert(String::from(MUSIC_CHANNEL_VAR), String::from("123"));
        assert_eq!(
            MusicForwarder::attach_config(&vars).expect("attach").1,
            Some(123)
        );
        vars.insert(String::from(MUSIC_CHANNEL_VAR), String::from("nope"));
        let (config, channel) = MusicForwarder::attach_config(&vars).expect("patch path stays live");
        assert_eq!(channel, None, "invalid channel disables auto-join only");
        assert!(config.player_url("s", STAGING).contains("/v4/sessions/s/players/10"));
        assert_eq!(MusicForwarder::attach_config(&vars_with("0")), None);
    }

    #[tokio::test]
    async fn complete_handshake_patches_once_with_auth() {
        let node = MockNode::start().await;
        let forwarder = node.forwarder(None);
        let cache = cache_with_bot(BOT);
        forwarder.handle(&server_event(STAGING, "token", Some("endpoint")), &cache);
        assert_eq!(node.patch_bodies().len(), 0, "server half alone patches nothing");
        forwarder.handle(&state_event(STAGING, BOT, "session", Some(CHANNEL)), &cache);
        poll_until("patch lands", || node.patch_bodies().len() == 1).await;
        let bodies = node.patch_bodies();
        assert_eq!(bodies[0]["voice"]["token"], "token");
        assert_eq!(bodies[0]["voice"]["sessionId"], "session");
        assert_eq!(bodies[0]["voice"]["channelId"], CHANNEL.to_string());
        assert_eq!(forwarder.patched_count(), 1);
        let auth = node.state.patch_auth.lock().expect("mock lock");
        assert_eq!(auth.as_slice(), [Some(String::from("fixture-node-password"))]);
        assert_eq!(node.state.accepts.load(Ordering::Relaxed), 1, "one session opened");
    }

    #[tokio::test]
    async fn duplicate_halves_skip_and_refresh_repatches() {
        let node = MockNode::start().await;
        let forwarder = node.forwarder(None);
        let cache = cache_with_bot(BOT);
        forwarder.handle(&server_event(STAGING, "token", Some("endpoint")), &cache);
        forwarder.handle(&state_event(STAGING, BOT, "session", Some(CHANNEL)), &cache);
        poll_until("first patch lands", || node.patch_bodies().len() == 1).await;
        // Redelivered halves (mute frames, retries) must not re-PATCH.
        forwarder.handle(&server_event(STAGING, "token", Some("endpoint")), &cache);
        forwarder.handle(&state_event(STAGING, BOT, "session", Some(CHANNEL)), &cache);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(node.patch_bodies().len(), 1, "identical handshake skips the node");
        // A region-move refresh is a NEW handshake and must re-PATCH.
        forwarder.handle(&server_event(STAGING, "token", Some("new-endpoint")), &cache);
        poll_until("refresh repatches", || node.patch_bodies().len() == 2).await;
        assert_eq!(node.patch_bodies()[1]["voice"]["endpoint"], "new-endpoint");
    }

    #[tokio::test]
    async fn self_leave_deletes_and_clears() {
        let node = MockNode::start().await;
        let forwarder = node.forwarder(None);
        let cache = cache_with_bot(BOT);
        forwarder.handle(&server_event(STAGING, "token", Some("endpoint")), &cache);
        forwarder.handle(&state_event(STAGING, BOT, "session", Some(CHANNEL)), &cache);
        poll_until("patch lands", || node.patch_bodies().len() == 1).await;
        forwarder.handle(&state_event(STAGING, BOT, "session", None), &cache);
        poll_until("delete lands", || !node.state.deletes.lock().expect("mock lock").is_empty())
            .await;
        let deletes = node.state.deletes.lock().expect("mock lock");
        assert_eq!(deletes.len(), 1);
        assert!(deletes[0].ends_with("/players/10"), "{}", deletes[0]);
        assert_eq!(forwarder.patched_count(), 0);
        assert_eq!(forwarder.commands_sent(), 0, "no auto-join channel, no rejoin");
    }

    #[tokio::test]
    async fn resumed_replays_patched_handshakes() {
        let node = MockNode::start().await;
        let forwarder = node.forwarder(None);
        let cache = cache_with_bot(BOT);
        forwarder.handle(&server_event(STAGING, "token", Some("endpoint")), &cache);
        forwarder.handle(&state_event(STAGING, BOT, "session", Some(CHANNEL)), &cache);
        poll_until("patch lands", || node.patch_bodies().len() == 1).await;
        forwarder.handle(&Event::Resumed, &cache);
        poll_until("reconnect replays", || node.patch_bodies().len() == 2).await;
        assert_eq!(node.state.accepts.load(Ordering::Relaxed), 1, "session survives resume");
    }

    #[tokio::test]
    async fn unknown_session_reconnects_and_retries_once() {
        let node = MockNode::start_scripted(VecDeque::from([404, 204])).await;
        let forwarder = node.forwarder(None);
        let cache = cache_with_bot(BOT);
        forwarder.handle(&server_event(STAGING, "token", Some("endpoint")), &cache);
        forwarder.handle(&state_event(STAGING, BOT, "session", Some(CHANNEL)), &cache);
        poll_until("retry lands", || node.patch_bodies().len() == 2).await;
        assert_eq!(
            node.state.accepts.load(Ordering::Relaxed),
            2,
            "404 reconnects the session, then the retry succeeds"
        );
        assert_eq!(forwarder.patched_count(), 1);
    }

    #[tokio::test]
    async fn dead_node_drops_jobs_without_phantom_records() {
        let client = NodeClient::new(
            NodeConfig::from_map(&HashMap::from([
                (String::from(LAVALINK_URL_VAR), String::from("http://127.0.0.1:9")),
                (
                    String::from(LAVALINK_PASSWORD_VAR),
                    String::from("fixture-node-password"),
                ),
            ]))
            .expect("closed-port config"),
        );
        let forwarder = MusicForwarder::with_client(None, Some(STAGING), None, client);
        let cache = cache_with_bot(BOT);
        forwarder.handle(&server_event(STAGING, "token", Some("endpoint")), &cache);
        forwarder.handle(&state_event(STAGING, BOT, "session", Some(CHANNEL)), &cache);
        // Refused-fast locally; 500 ms is ample even loaded. A dropped job
        // must leave no success record behind.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(forwarder.patched_count(), 0);
    }

    #[tokio::test]
    async fn ready_auto_joins_only_out_of_voice() {
        let node = MockNode::start().await;
        let forwarder = node.forwarder(Some(CHANNEL));
        let (_shard, sender) = live_sender();
        forwarder.refresh_gateway_sender(&sender);
        // Out of voice: READY joins the configured channel.
        let cache = cache_with_bot(BOT);
        forwarder.handle(&ready_event(BOT), &cache);
        assert_eq!(forwarder.commands_sent(), 1);
        let sent = forwarder.last_command().expect("join sent");
        assert_eq!(sent.d.guild_id.get(), STAGING);
        assert_eq!(sent.d.channel_id.map(|id| id.get()), Some(CHANNEL));
        // In voice: READY leaves placement alone (a human may have moved us).
        cache.update(&state_event(STAGING, BOT, "session", Some(CHANNEL)));
        forwarder.handle(&ready_event(BOT), &cache);
        assert_eq!(forwarder.commands_sent(), 1, "no second join while in voice");
        // The auto-join reset the pairing first: halves arriving now pair fresh.
        forwarder.handle(&server_event(STAGING, "token", Some("endpoint")), &cache);
        forwarder.handle(&state_event(STAGING, BOT, "session", Some(CHANNEL)), &cache);
        poll_until("post-join patch lands", || node.patch_bodies().len() == 1).await;
    }

    #[tokio::test]
    async fn ready_without_channel_never_joins() {
        let node = MockNode::start().await;
        let forwarder = node.forwarder(None);
        let (_shard, sender) = live_sender();
        forwarder.refresh_gateway_sender(&sender);
        forwarder.handle(&ready_event(BOT), &cache_with_bot(BOT));
        assert_eq!(forwarder.commands_sent(), 0);
    }

    #[tokio::test]
    async fn leave_rejoins_until_manual() {
        let node = MockNode::start().await;
        let forwarder = node.forwarder(Some(CHANNEL));
        let (_shard, sender) = live_sender();
        forwarder.refresh_gateway_sender(&sender);
        let cache = cache_with_bot(BOT);
        forwarder.handle(&server_event(STAGING, "token", Some("endpoint")), &cache);
        forwarder.handle(&state_event(STAGING, BOT, "session", Some(CHANNEL)), &cache);
        poll_until("patch lands", || node.patch_bodies().len() == 1).await;
        // External leave: DELETE plus an auto-rejoin.
        forwarder.handle(&state_event(STAGING, BOT, "session", None), &cache);
        poll_until("delete lands", || !node.state.deletes.lock().expect("mock lock").is_empty())
            .await;
        let sent = forwarder.last_command().expect("rejoin sent");
        assert_eq!(sent.d.channel_id.map(|id| id.get()), Some(CHANNEL));
        // Manual leave: the next observed leave stays out.
        forwarder.leave(STAGING).expect("manual leave sends");
        assert_eq!(
            forwarder.last_command().expect("leave sent").d.channel_id,
            None
        );
        forwarder.handle(&state_event(STAGING, BOT, "session", None), &cache);
        poll_until("second delete lands", || node.state.deletes.lock().expect("mock lock").len() == 2)
            .await;
        assert_eq!(
            forwarder.last_command().expect("still the leave").d.channel_id,
            None,
            "no rejoin after a manual leave"
        );
    }

    #[test]
    fn sender_lifecycle_gates_sends() {
        crate::gateway::ensure_crypto_provider();
        let node_client = NodeClient::new(
            NodeConfig::from_map(&vars_with("1")).expect("fixture config"),
        );
        let forwarder = MusicForwarder::with_client(None, Some(STAGING), None, node_client);
        assert_eq!(forwarder.join(STAGING, CHANNEL), Err(MusicJoinError::NoSender));
        assert_eq!(forwarder.leave(STAGING), Err(MusicJoinError::NoSender));
        assert_eq!(
            forwarder.join(OTHER_GUILD, CHANNEL),
            Err(MusicJoinError::NotStagingGuild)
        );
        assert_eq!(forwarder.join(STAGING, 0), Err(MusicJoinError::InvalidSnowflake));
        let (shard, sender) = live_sender();
        forwarder.refresh_gateway_sender(&sender);
        forwarder.join(STAGING, CHANNEL).expect("join sends");
        assert_eq!(forwarder.commands_sent(), 1);
        drop(shard);
        assert_eq!(forwarder.join(STAGING, CHANNEL), Err(MusicJoinError::SendFailed));
    }

    #[tokio::test]
    async fn foreign_dispatches_stay_quiet() {
        let node = MockNode::start().await;
        let forwarder = node.forwarder(Some(CHANNEL));
        let cache = cache_with_bot(BOT);
        // Other guild, other member, endpoint-less server, guildless state.
        forwarder.handle(&server_event(OTHER_GUILD, "token", Some("endpoint")), &cache);
        forwarder.handle(&state_event(OTHER_GUILD, BOT, "session", Some(CHANNEL)), &cache);
        forwarder.handle(&state_event(STAGING, HUMAN, "session", Some(CHANNEL)), &cache);
        forwarder.handle(&server_event(STAGING, "token", None), &cache);
        let guildless = Event::VoiceStateUpdate(Box::new(
            serde_json::from_value(serde_json::json!({
                "user_id": BOT.to_string(),
                "channel_id": CHANNEL.to_string(),
                "session_id": "session",
                "deaf": false, "mute": false, "self_deaf": false,
                "self_mute": false, "self_video": false, "suppress": false,
            }))
            .expect("guildless fixture"),
        ));
        forwarder.handle(&guildless, &cache);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(node.patch_bodies().len(), 0);
        assert_eq!(node.state.accepts.load(Ordering::Relaxed), 0, "no session opened");
        assert_eq!(forwarder.patched_count(), 0);
    }

    #[tokio::test]
    async fn inner_sink_sees_everything_first() {
        let node = MockNode::start().await;
        let inner = Arc::new(FakeInner::default());
        let forwarder = MusicForwarder::with_client(
            Some(Arc::clone(&inner) as Arc<dyn VoiceEventSink>),
            Some(STAGING),
            None,
            node.client(),
        );
        let cache = cache_with_bot(BOT);
        forwarder.handle(&server_event(STAGING, "token", Some("endpoint")), &cache);
        assert!(inner.called("handle_received"));
        let update: twilight_model::gateway::payload::incoming::PresenceUpdate =
            serde_json::from_value(serde_json::json!({
                "user": {"id": HUMAN.to_string()},
                "guild_id": STAGING.to_string(),
                "status": "online",
                "activities": [],
                "client_status": {}
            }))
            .expect("presence");
        forwarder.presence(&update);
        assert!(inner.called("presence"));
        forwarder.disconnect();
        assert!(inner.called("disconnect"));
        assert!(!forwarder.needs_bootstrap(&cache));
        assert!(inner.called("needs_bootstrap"));
        let vote = forwarder
            .kick_vote(crate::command_runtime_tests::slash("kick", Some(CHANNEL), Vec::new()))
            .await;
        assert!(!vote);
        assert!(inner.called("kick_vote"));
        // Node state survives transport loss: disconnect must not clear it.
        forwarder.handle(&state_event(STAGING, BOT, "session", Some(CHANNEL)), &cache);
        poll_until("patch lands", || node.patch_bodies().len() == 1).await;
        forwarder.disconnect();
        assert_eq!(forwarder.patched_count(), 1, "disconnect keeps node state");
    }
}
