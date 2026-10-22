//! Staging-only music voice-update capture (dry-run).
//!
//! Gateway tap for the P0 external-voice-node spike (see
//! `docs/music-voice-node-spike.md`): it feeds `VoiceServerUpdate`
//! (token/endpoint) and self `VoiceStateUpdate` (session/channel) halves
//! into the pairing cache, behind exact `TWO_MUSIC=1` and the staging-guild
//! fence. When both halves arrive it logs one structured line and drops
//! the update — no HTTP client, no PATCH, no gateway send in this slice.
//!
//! The tap wraps the voice sink so the rooms path is untouched; with the
//! gate off [`MusicVoiceTap::wrap`] returns the inner sink unchanged.
//! The dry-run log lines carry guild, channel and `has_token` only —
//! token and endpoint values never appear in logs.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_gateway::Event;
use twilight_model::application::interaction::Interaction;
use two_bot::voice_rooms::VoiceEventSink;
use two_bot_core::music_voice::{is_staging_guild, MusicGates, PlayerVoiceUpdate, VoicePairing};
use two_bot_core::Snowflake;

/// Voice-sink decorator capturing the Lavalink handshake (dry-run).
///
/// Built only when `TWO_MUSIC=1` (see [`MusicVoiceTap::wrap`]); every
/// observed dispatch additionally passes the staging-guild fence, so a
/// misrouted dispatch can never pair outside staging.
pub struct MusicVoiceTap {
    inner: Option<Arc<dyn VoiceEventSink>>,
    staging_guild: Option<Snowflake>,
    pairing: Mutex<VoicePairing>,
    last_logged: Mutex<HashMap<Snowflake, PlayerVoiceUpdate>>,
    dry_runs: AtomicU64,
    clears: AtomicU64,
}

impl MusicVoiceTap {
    /// Wrap the voice sink, attaching capture only when `TWO_MUSIC=1`.
    ///
    /// Gate off: returns `inner` unchanged (same `Arc`, no tap). Gate on:
    /// returns a decorator forwarding every sink call to `inner` while
    /// capturing voice halves for `staging_guild`.
    pub fn wrap(
        inner: Option<Arc<dyn VoiceEventSink>>,
        staging_guild: Option<Snowflake>,
    ) -> Option<Arc<dyn VoiceEventSink>> {
        Self::wrap_with_vars(inner, staging_guild, &std::env::vars().collect())
    }

    /// [`MusicVoiceTap::wrap`] with an explicit env map (tests, staged config).
    pub fn wrap_with_vars(
        inner: Option<Arc<dyn VoiceEventSink>>,
        staging_guild: Option<Snowflake>,
        vars: &HashMap<String, String>,
    ) -> Option<Arc<dyn VoiceEventSink>> {
        if !MusicGates::from_map(vars).enabled {
            return inner;
        }
        Some(Arc::new(Self::new(inner, staging_guild)))
    }

    /// Tap over `inner` without consulting the gate (tests, staged wiring).
    #[must_use]
    pub fn new(inner: Option<Arc<dyn VoiceEventSink>>, staging_guild: Option<Snowflake>) -> Self {
        Self {
            inner,
            staging_guild,
            pairing: Mutex::new(VoicePairing::new()),
            last_logged: Mutex::new(HashMap::new()),
            dry_runs: AtomicU64::new(0),
            clears: AtomicU64::new(0),
        }
    }

    /// Completed handshakes logged and dropped (dry-run observability).
    #[cfg(test)]
    #[must_use]
    pub fn dry_runs(&self) -> u64 {
        self.dry_runs.load(Ordering::Relaxed)
    }

    /// Pairings cleared by a self leave (dry-run observability).
    #[cfg(test)]
    #[must_use]
    pub fn clears(&self) -> u64 {
        self.clears.load(Ordering::Relaxed)
    }

    /// The complete handshake for `guild_id`, if both halves arrived.
    ///
    /// The dry-run slice only logs completions; the follow-up client card
    /// removes the test gate and PATCHes from this same accessor.
    #[cfg(test)]
    #[must_use]
    pub fn complete(&self, guild_id: Snowflake) -> Option<PlayerVoiceUpdate> {
        self.pairing
            .lock()
            .expect("music pairing lock")
            .complete(guild_id)
    }

    /// Feed one dispatch into the pairing cache (staging guild, self only).
    ///
    /// Anything else — unset staging guild, another guild, another member,
    /// unknown bot identity, a server half without endpoint — is ignored
    /// silently. Each distinct completed handshake logs one structured line
    /// (guild, channel, `has_token`) and is dropped: dry-run only.
    /// Redelivered identical halves (mute toggles, duplicate dispatches)
    /// do not re-log; a changed half or a rejoin after leave logs again.
    fn observe(&self, event: &Event, bot_id: Option<Snowflake>) {
        match event {
            Event::VoiceServerUpdate(update) => {
                let guild_id = update.guild_id.get();
                if !is_staging_guild(guild_id, self.staging_guild) {
                    return;
                }
                let Some(endpoint) = update.endpoint.as_deref() else {
                    return;
                };
                let completed = {
                    let mut pairing = self.pairing.lock().expect("music pairing lock");
                    pairing.observe_server(guild_id, &update.token, endpoint);
                    pairing.complete(guild_id)
                };
                if let Some(update) = completed {
                    self.note_completed(&update);
                }
            }
            Event::VoiceStateUpdate(update) => {
                let Some(guild) = update.guild_id else {
                    return;
                };
                let guild_id = guild.get();
                if !is_staging_guild(guild_id, self.staging_guild) {
                    return;
                }
                if bot_id != Some(update.user_id.get()) {
                    return;
                }
                let channel = update.channel_id.map(|id| id.get());
                match channel {
                    Some(_) => {
                        let completed = {
                            let mut pairing = self.pairing.lock().expect("music pairing lock");
                            pairing.observe_state(guild_id, &update.session_id, channel);
                            pairing.complete(guild_id)
                        };
                        if let Some(update) = completed {
                            self.note_completed(&update);
                        }
                    }
                    None => {
                        let had_pairing = {
                            let mut pairing = self.pairing.lock().expect("music pairing lock");
                            let had = pairing.contains(guild_id);
                            pairing.observe_state(guild_id, &update.session_id, None);
                            had
                        };
                        self.last_logged
                            .lock()
                            .expect("music pairing lock")
                            .remove(&guild_id);
                        if had_pairing {
                            self.clears.fetch_add(1, Ordering::Relaxed);
                            tracing::info!(
                                msg = "music_voice_pairing_cleared",
                                guild = guild_id,
                                has_token = false,
                                "music voice pairing cleared on leave (dry-run)"
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Log a completed handshake unless it repeats the last logged one.
    ///
    /// Discord re-emits self voice state on every mute/deafen/video toggle
    /// with the same session and channel, and duplicate server halves
    /// arrive on retries: without this, one handshake logs a line per
    /// dispatch instead of one line per handshake. A half that actually
    /// changed (a new endpoint after a region move) is a new handshake
    /// and logs again.
    fn note_completed(&self, update: &PlayerVoiceUpdate) {
        let mut last_logged = self.last_logged.lock().expect("music pairing lock");
        if last_logged.get(&update.guild_id) == Some(update) {
            return;
        }
        last_logged.insert(update.guild_id, update.clone());
        self.dry_runs.fetch_add(1, Ordering::Relaxed);
        log_dry_run(update.guild_id, update.channel_id);
    }
}

/// One structured dry-run line per completed handshake. Snowflake ids and a
/// boolean only — token and endpoint values never reach the logs.
fn log_dry_run(guild_id: Snowflake, channel_id: Snowflake) {
    tracing::info!(
        msg = "music_voice_pairing_dry_run",
        guild = guild_id,
        channel = channel_id,
        has_token = true,
        "music voice handshake complete (dry-run, dropped)"
    );
}

impl VoiceEventSink for MusicVoiceTap {
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
        let bot_id = cache.current_user().map(|user| user.id.get());
        self.observe(event, bot_id);
    }

    fn presence(&self, update: &twilight_model::gateway::payload::incoming::PresenceUpdate) {
        if let Some(inner) = self.inner.as_ref() {
            inner.presence(update);
        }
    }

    fn disconnect(&self) {
        if let Some(inner) = self.inner.as_ref() {
            inner.disconnect();
        }
        if let Some(guild_id) = self.staging_guild {
            self.pairing
                .lock()
                .expect("music pairing lock")
                .reset(guild_id);
            self.last_logged
                .lock()
                .expect("music pairing lock")
                .remove(&guild_id);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAGING: u64 = 10;
    const OTHER_GUILD: u64 = 11;
    const CHANNEL: u64 = 30;
    const BOT: u64 = 999;
    const HUMAN: u64 = 20;

    fn vars_with(music: &str) -> HashMap<String, String> {
        [(String::from("TWO_MUSIC"), String::from(music))]
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
            .unwrap(),
        )
    }

    fn state_event(guild: u64, user: u64, session: &str, channel: Option<u64>) -> Event {
        Event::VoiceStateUpdate(Box::new(
            serde_json::from_value(serde_json::json!({
                "guild_id": guild.to_string(),
                "user_id": user.to_string(),
                "channel_id": channel.map(|id| id.to_string()),
                "session_id": session,
                "deaf": false, "mute": false,
                "self_deaf": false, "self_mute": false, "self_video": false,
                "suppress": false,
            }))
            .unwrap(),
        ))
    }

    fn seeded_cache(bot_id: u64) -> DefaultInMemoryCache {
        let cache = DefaultInMemoryCache::new();
        let ready: twilight_model::gateway::payload::incoming::Ready =
            serde_json::from_value(serde_json::json!({
                "application": {"flags": 0, "id": "100"},
                "guilds": [],
                "resume_gateway_url": "wss://gateway.discord.gg",
                "session_id": "seed",
                "user": {
                    "id": bot_id.to_string(),
                    "username": "bot",
                    "discriminator": "0000",
                    "mfa_enabled": false,
                    "avatar": null,
                },
                "v": 10,
            }))
            .unwrap();
        cache.update(&Event::Ready(ready));
        cache
    }

    struct CountingSink {
        handled: AtomicU64,
        presences: AtomicU64,
        disconnects: AtomicU64,
        identities: AtomicU64,
        bootstrap: bool,
        kick_answer: bool,
    }

    impl CountingSink {
        fn new() -> Self {
            Self {
                handled: AtomicU64::new(0),
                presences: AtomicU64::new(0),
                disconnects: AtomicU64::new(0),
                identities: AtomicU64::new(0),
                bootstrap: false,
                kick_answer: false,
            }
        }
    }

    impl VoiceEventSink for CountingSink {
        fn set_command_identities(
            &self,
            _identities: two_bot_discord::command_identity::CommandIdentities,
        ) {
            self.identities.fetch_add(1, Ordering::Relaxed);
        }

        fn handle(&self, _event: &Event, _cache: &DefaultInMemoryCache) {
            self.handled.fetch_add(1, Ordering::Relaxed);
        }

        fn presence(&self, _update: &twilight_model::gateway::payload::incoming::PresenceUpdate) {
            self.presences.fetch_add(1, Ordering::Relaxed);
        }

        fn disconnect(&self) {
            self.disconnects.fetch_add(1, Ordering::Relaxed);
        }

        fn needs_bootstrap(&self, _cache: &DefaultInMemoryCache) -> bool {
            self.bootstrap
        }

        fn kick_vote(
            &self,
            _interaction: Interaction,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
            let answer = self.kick_answer;
            Box::pin(async move { answer })
        }
    }

    fn tap(staging: Option<Snowflake>) -> MusicVoiceTap {
        MusicVoiceTap::new(None, staging)
    }

    #[test]
    fn gate_off_returns_inner_unchanged() {
        let inner: Arc<dyn VoiceEventSink> = Arc::new(CountingSink::new());
        for off in ["", "0", "true", "01", " 1", "1 "] {
            let wrapped = MusicVoiceTap::wrap_with_vars(
                Some(Arc::clone(&inner)),
                Some(STAGING),
                &vars_with(off),
            );
            assert!(
                wrapped.is_some_and(|sink| Arc::ptr_eq(&sink, &inner)),
                "TWO_MUSIC={off:?} must return the inner sink unchanged"
            );
        }
        let wrapped =
            MusicVoiceTap::wrap_with_vars(Some(Arc::clone(&inner)), Some(STAGING), &HashMap::new());
        assert!(
            wrapped.is_some_and(|sink| Arc::ptr_eq(&sink, &inner)),
            "unset TWO_MUSIC must return the inner sink unchanged"
        );
    }

    #[test]
    fn gate_on_wraps_sink() {
        let inner: Arc<dyn VoiceEventSink> = Arc::new(CountingSink::new());
        let wrapped =
            MusicVoiceTap::wrap_with_vars(Some(Arc::clone(&inner)), Some(STAGING), &vars_with("1"));
        let sink = wrapped.expect("gate on must attach the tap");
        assert!(
            !Arc::ptr_eq(&sink, &inner),
            "gate on must return the decorator, not the inner sink"
        );
    }

    #[test]
    fn gate_on_without_inner_builds_music_only_sink() {
        let wrapped = MusicVoiceTap::wrap_with_vars(None, Some(STAGING), &vars_with("1"));
        assert!(
            wrapped.is_some(),
            "gate on with no voice sink must still attach capture"
        );
        let cache = DefaultInMemoryCache::new();
        assert!(
            !wrapped.expect("attached").needs_bootstrap(&cache),
            "music-only sink never needs the rooms bootstrap"
        );
    }

    #[test]
    fn gate_off_without_inner_stays_none() {
        assert!(MusicVoiceTap::wrap_with_vars(None, Some(STAGING), &vars_with("0")).is_none());
    }

    #[test]
    fn server_then_state_completes_dry_run() {
        let tap = tap(Some(STAGING));
        tap.observe(&server_event(STAGING, "token", Some("endpoint")), Some(BOT));
        assert_eq!(tap.dry_runs(), 0, "server half alone is not ready");
        tap.observe(
            &state_event(STAGING, BOT, "session", Some(CHANNEL)),
            Some(BOT),
        );
        assert_eq!(tap.dry_runs(), 1, "both halves log one dry-run line");
        let update = tap.complete(STAGING).expect("both halves arrived");
        assert_eq!(update.guild_id, STAGING);
        assert_eq!(update.channel_id, CHANNEL);
    }

    #[test]
    fn state_then_server_completes_dry_run() {
        let tap = tap(Some(STAGING));
        tap.observe(
            &state_event(STAGING, BOT, "session", Some(CHANNEL)),
            Some(BOT),
        );
        assert_eq!(tap.dry_runs(), 0, "state half alone is not ready");
        tap.observe(&server_event(STAGING, "token", Some("endpoint")), Some(BOT));
        assert_eq!(tap.dry_runs(), 1, "either arrival order completes");
        assert!(tap.complete(STAGING).is_some());
    }

    #[test]
    fn non_self_state_is_ignored() {
        let tap = tap(Some(STAGING));
        tap.observe(&server_event(STAGING, "token", Some("endpoint")), Some(BOT));
        tap.observe(
            &state_event(STAGING, HUMAN, "human-session", Some(CHANNEL)),
            Some(BOT),
        );
        assert_eq!(tap.dry_runs(), 0, "another member's state never pairs");
        assert_eq!(tap.complete(STAGING), None);
    }

    #[test]
    fn unknown_bot_is_ignored() {
        let tap = tap(Some(STAGING));
        tap.observe(&server_event(STAGING, "token", Some("endpoint")), None);
        tap.observe(&state_event(STAGING, BOT, "session", Some(CHANNEL)), None);
        assert_eq!(
            tap.dry_runs(),
            0,
            "without a bot identity no state can be confirmed self"
        );
        assert_eq!(tap.complete(STAGING), None);
    }

    #[test]
    fn non_staging_guild_is_ignored() {
        let tap = tap(Some(STAGING));
        tap.observe(
            &server_event(OTHER_GUILD, "token", Some("endpoint")),
            Some(BOT),
        );
        tap.observe(
            &state_event(OTHER_GUILD, BOT, "session", Some(CHANNEL)),
            Some(BOT),
        );
        assert_eq!(tap.dry_runs(), 0, "other guilds never pair");
        assert_eq!(tap.complete(OTHER_GUILD), None);
        assert!(
            !tap.pairing.lock().unwrap().contains(OTHER_GUILD),
            "neither half may create a pending entry outside staging"
        );
    }

    #[test]
    fn unconfigured_guild_fence_denies_everything() {
        let tap = tap(None);
        tap.observe(&server_event(STAGING, "token", Some("endpoint")), Some(BOT));
        tap.observe(
            &state_event(STAGING, BOT, "session", Some(CHANNEL)),
            Some(BOT),
        );
        assert_eq!(tap.dry_runs(), 0, "prod (no staging var) must never pair");
        assert_eq!(tap.complete(STAGING), None);
        assert!(
            !tap.pairing.lock().unwrap().contains(STAGING),
            "neither half may create a pending entry without a staging guild"
        );
    }

    #[test]
    fn leave_clears_pairing_and_counts() {
        let tap = tap(Some(STAGING));
        tap.observe(&server_event(STAGING, "token", Some("endpoint")), Some(BOT));
        tap.observe(
            &state_event(STAGING, BOT, "session", Some(CHANNEL)),
            Some(BOT),
        );
        assert!(tap.complete(STAGING).is_some());
        tap.observe(&state_event(STAGING, BOT, "session", None), Some(BOT));
        assert_eq!(tap.clears(), 1, "self leave logs one cleared line");
        assert_eq!(tap.complete(STAGING), None, "leave drops the pairing");
    }

    #[test]
    fn leave_without_pairing_is_silent() {
        let tap = tap(Some(STAGING));
        tap.observe(&state_event(STAGING, BOT, "session", None), Some(BOT));
        assert_eq!(tap.clears(), 0, "a leave with no pairing clears nothing");
        tap.observe(&server_event(STAGING, "token", Some("endpoint")), Some(BOT));
        tap.observe(&state_event(STAGING, BOT, "session", None), Some(BOT));
        assert_eq!(tap.clears(), 1, "a leave drops the pending half");
        tap.observe(&state_event(STAGING, BOT, "session", None), Some(BOT));
        assert_eq!(tap.clears(), 1, "a redelivered leave clears nothing new");
    }

    #[test]
    fn missing_endpoint_skips_server_half() {
        let tap = tap(Some(STAGING));
        tap.observe(&server_event(STAGING, "token", None), Some(BOT));
        tap.observe(
            &state_event(STAGING, BOT, "session", Some(CHANNEL)),
            Some(BOT),
        );
        assert_eq!(
            tap.dry_runs(),
            0,
            "a server half without endpoint cannot complete"
        );
        assert_eq!(tap.complete(STAGING), None);
    }

    #[test]
    fn repeat_halves_do_not_relog() {
        let tap = tap(Some(STAGING));
        tap.observe(&server_event(STAGING, "token", Some("endpoint")), Some(BOT));
        tap.observe(
            &state_event(STAGING, BOT, "session", Some(CHANNEL)),
            Some(BOT),
        );
        assert_eq!(tap.dry_runs(), 1);
        // Mute toggles and duplicate dispatches redeliver identical halves.
        tap.observe(&server_event(STAGING, "token", Some("endpoint")), Some(BOT));
        tap.observe(
            &state_event(STAGING, BOT, "session", Some(CHANNEL)),
            Some(BOT),
        );
        assert_eq!(tap.dry_runs(), 1, "an unchanged handshake logs once");
    }

    #[test]
    fn changed_half_relogs() {
        let tap = tap(Some(STAGING));
        tap.observe(
            &server_event(STAGING, "token", Some("old-endpoint")),
            Some(BOT),
        );
        tap.observe(
            &state_event(STAGING, BOT, "session", Some(CHANNEL)),
            Some(BOT),
        );
        assert_eq!(tap.dry_runs(), 1);
        tap.observe(
            &server_event(STAGING, "token", Some("new-endpoint")),
            Some(BOT),
        );
        assert_eq!(tap.dry_runs(), 2, "a refreshed endpoint is a new handshake");
    }

    #[test]
    fn rejoin_after_leave_relogs() {
        let tap = tap(Some(STAGING));
        tap.observe(&server_event(STAGING, "token", Some("endpoint")), Some(BOT));
        tap.observe(
            &state_event(STAGING, BOT, "session", Some(CHANNEL)),
            Some(BOT),
        );
        assert_eq!(tap.dry_runs(), 1);
        tap.observe(&state_event(STAGING, BOT, "session", None), Some(BOT));
        tap.observe(&server_event(STAGING, "token", Some("endpoint")), Some(BOT));
        tap.observe(
            &state_event(STAGING, BOT, "session", Some(CHANNEL)),
            Some(BOT),
        );
        assert_eq!(tap.dry_runs(), 2, "leave clears the dedup memory");
    }

    #[test]
    fn non_voice_events_are_ignored() {
        let tap = tap(Some(STAGING));
        tap.observe(&Event::Resumed, Some(BOT));
        assert_eq!(tap.dry_runs(), 0);
        assert_eq!(tap.clears(), 0);
    }

    #[test]
    fn inner_sink_still_receives_events() {
        let inner = Arc::new(CountingSink::new());
        let tap = MusicVoiceTap::new(
            Some(Arc::clone(&inner) as Arc<dyn VoiceEventSink>),
            Some(STAGING),
        );
        let cache = seeded_cache(BOT);
        tap.handle_received(
            &state_event(STAGING, BOT, "session", Some(CHANNEL)),
            &cache,
            tokio::time::Instant::now(),
        );
        assert_eq!(
            inner.handled.load(Ordering::Relaxed),
            1,
            "the decorator must forward every dispatch to the rooms sink"
        );
    }

    #[test]
    fn handle_resolves_self_from_cache() {
        let tap = tap(Some(STAGING));
        let cache = seeded_cache(BOT);
        tap.handle_received(
            &server_event(STAGING, "token", Some("endpoint")),
            &cache,
            tokio::time::Instant::now(),
        );
        tap.handle(&state_event(STAGING, BOT, "session", Some(CHANNEL)), &cache);
        assert_eq!(
            tap.dry_runs(),
            1,
            "self state via the cache identity completes the pairing"
        );
    }

    #[test]
    fn handle_ignores_non_self_from_cache() {
        let tap = tap(Some(STAGING));
        let cache = seeded_cache(BOT);
        tap.handle_received(
            &server_event(STAGING, "token", Some("endpoint")),
            &cache,
            tokio::time::Instant::now(),
        );
        tap.handle_received(
            &state_event(STAGING, HUMAN, "human-session", Some(CHANNEL)),
            &cache,
            tokio::time::Instant::now(),
        );
        assert_eq!(tap.dry_runs(), 0);
    }

    #[test]
    fn sink_lifecycle_forwards_to_inner() {
        let inner = Arc::new(CountingSink::new());
        let tap = MusicVoiceTap::new(
            Some(Arc::clone(&inner) as Arc<dyn VoiceEventSink>),
            Some(STAGING),
        );
        let cache = seeded_cache(BOT);
        let presence: twilight_model::gateway::payload::incoming::PresenceUpdate =
            serde_json::from_value(serde_json::json!({
                "guild_id": STAGING.to_string(),
                "status": "online",
                "client_status": {},
                "user": {"id": HUMAN.to_string()},
            }))
            .unwrap();
        tap.presence(&presence);
        tap.disconnect();
        tap.set_command_identities(two_bot_discord::command_identity::CommandIdentities::default());
        assert!(!tap.needs_bootstrap(&cache));
        assert_eq!(inner.presences.load(Ordering::Relaxed), 1);
        assert_eq!(inner.disconnects.load(Ordering::Relaxed), 1);
        assert_eq!(inner.identities.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn bootstrap_request_from_inner_is_preserved() {
        let inner: Arc<dyn VoiceEventSink> = Arc::new(CountingSink {
            bootstrap: true,
            ..CountingSink::new()
        });
        let sink = MusicVoiceTap::wrap_with_vars(Some(inner), Some(STAGING), &vars_with("1"))
            .expect("gate on attaches the tap");
        assert!(
            sink.needs_bootstrap(&DefaultInMemoryCache::new()),
            "a cold-resume bootstrap request must pass through the music tap"
        );
    }

    #[test]
    fn disconnect_requires_fresh_halves_and_resets_dedup() {
        for with_inner in [false, true] {
            for (had_server, had_state) in [(true, false), (false, true), (true, true)] {
                for server_first in [false, true] {
                    let inner = Arc::new(CountingSink::new());
                    let tap = MusicVoiceTap::new(
                        with_inner.then(|| Arc::clone(&inner) as Arc<dyn VoiceEventSink>),
                        Some(STAGING),
                    );
                    let cache = seeded_cache(BOT);
                    let server = server_event(STAGING, "token", Some("endpoint"));
                    let state = state_event(STAGING, BOT, "session", Some(CHANNEL));
                    if had_server {
                        tap.handle(&server, &cache);
                    }
                    if had_state {
                        tap.handle(&state, &cache);
                    }
                    let before = u64::from(had_server && had_state);
                    assert_eq!(tap.dry_runs(), before);

                    tap.disconnect();
                    assert_eq!(
                        inner.disconnects.load(Ordering::Relaxed),
                        u64::from(with_inner)
                    );
                    assert!(
                        !tap.pairing.lock().unwrap().contains(STAGING),
                        "disconnect drops both complete and pending handshakes"
                    );
                    assert_eq!(tap.complete(STAGING), None);
                    assert_eq!(tap.dry_runs(), before, "disconnect never logs a completion");
                    assert_eq!(tap.clears(), 0, "disconnect is not a self leave");

                    let (first, second) = if server_first {
                        (&server, &state)
                    } else {
                        (&state, &server)
                    };
                    tap.handle(first, &cache);
                    assert_eq!(
                        tap.complete(STAGING),
                        None,
                        "one fresh half cannot pair with stale data"
                    );
                    assert_eq!(tap.dry_runs(), before);
                    tap.handle(second, &cache);
                    assert!(tap.complete(STAGING).is_some());
                    assert_eq!(
                        tap.dry_runs(),
                        before + 1,
                        "even an identical handshake logs again after disconnect"
                    );
                }
            }
        }
    }

    #[test]
    fn sink_lifecycle_without_inner_is_inert() {
        let tap = tap(Some(STAGING));
        let cache = seeded_cache(BOT);
        let presence: twilight_model::gateway::payload::incoming::PresenceUpdate =
            serde_json::from_value(serde_json::json!({
                "guild_id": STAGING.to_string(),
                "status": "online",
                "client_status": {},
                "user": {"id": HUMAN.to_string()},
            }))
            .unwrap();
        tap.presence(&presence);
        tap.disconnect();
        tap.set_command_identities(two_bot_discord::command_identity::CommandIdentities::default());
        assert!(!tap.needs_bootstrap(&cache));
    }

    #[tokio::test]
    async fn kick_vote_forwards_to_inner() {
        let inner = Arc::new(CountingSink {
            kick_answer: true,
            ..CountingSink::new()
        });
        let decorated = MusicVoiceTap::new(
            Some(Arc::clone(&inner) as Arc<dyn VoiceEventSink>),
            Some(STAGING),
        );
        let interaction: Interaction = serde_json::from_value(serde_json::json!({
            "id": "1",
            "application_id": "2",
            "authorizing_integration_owners": {"0": STAGING.to_string()},
            "entitlements": [],
            "type": 2,
            "token": "token",
            "data": {"id": "3", "name": "kick", "type": 1},
        }))
        .unwrap();
        assert!(decorated.kick_vote(interaction).await);
        let music_only = tap(None);
        let interaction: Interaction = serde_json::from_value(serde_json::json!({
            "id": "1",
            "application_id": "2",
            "authorizing_integration_owners": {"0": STAGING.to_string()},
            "entitlements": [],
            "type": 2,
            "token": "token",
            "data": {"id": "3", "name": "kick", "type": 1},
        }))
        .unwrap();
        assert!(
            !music_only.kick_vote(interaction).await,
            "without an inner sink the router answers"
        );
    }
}
