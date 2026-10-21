//! Staging-only music voice-node spike: Lavalink v4 voice-payload builders.
//!
//! Pure foundation for the P0 external-voice-node spike (see
//! `docs/music-voice-node-spike.md`): the `TWO_MUSIC` gate, the per-guild
//! pairing of `VoiceServerUpdate` (token/endpoint) with self
//! `VoiceStateUpdate` (session/channel), and the Lavalink v4
//! `PATCH /v4/sessions/{sessionId}/players/{guildId}` voice body
//! (token, endpoint, sessionId, channelId — DAVE needs all four).
//!
//! No HTTP here and no gateway wiring yet: the follow-up slice owns the
//! REST/WS client and the staging-guild send path. Everything in this module
//! is framework-free so the spike stays unit-testable.

use std::collections::HashMap;

use crate::Snowflake;

/// `TWO_MUSIC=1` — attach the music voice-update forwarder (staging only).
pub struct MusicGates {
    /// Forwarder attached. Exact `"1"` only, mirroring [`crate::VoiceGates`].
    pub enabled: bool,
}

impl MusicGates {
    /// Read gates from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_map(&std::env::vars().collect())
    }

    /// Read gates from an explicit map (tests, staged config).
    #[must_use]
    pub fn from_map(vars: &HashMap<String, String>) -> Self {
        Self {
            enabled: vars.get("TWO_MUSIC").is_some_and(|v| v == "1"),
        }
    }
}

/// Half-open Lavalink voice handshake, keyed by guild.
///
/// Discord delivers the two halves on separate dispatches: `VoiceServerUpdate`
/// carries token/endpoint, self `VoiceStateUpdate` carries session/channel.
/// A guild is PATCH-ready only once both halves have arrived.
#[derive(Debug, Default)]
pub struct VoicePairing {
    by_guild: HashMap<Snowflake, PartialVoice>,
}

#[derive(Debug, Default)]
struct PartialVoice {
    token: Option<String>,
    endpoint: Option<String>,
    session_id: Option<String>,
    channel_id: Option<Snowflake>,
}

impl VoicePairing {
    /// Empty pairing cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a `VoiceServerUpdate` half. A repeat replaces the previous
    /// server half (Discord rotates the endpoint on region moves).
    pub fn observe_server(&mut self, guild_id: Snowflake, token: &str, endpoint: &str) {
        let entry = self.by_guild.entry(guild_id).or_default();
        entry.token = Some(token.to_owned());
        entry.endpoint = Some(endpoint.to_owned());
    }

    /// Record a self `VoiceStateUpdate` half. `None` channel means the bot
    /// left voice: the guild entry is dropped (the follow-up slice destroys
    /// the player instead of PATCHing).
    pub fn observe_state(
        &mut self,
        guild_id: Snowflake,
        session_id: &str,
        channel_id: Option<Snowflake>,
    ) {
        match channel_id {
            Some(channel) => {
                let entry = self.by_guild.entry(guild_id).or_default();
                entry.session_id = Some(session_id.to_owned());
                entry.channel_id = Some(channel);
            }
            None => {
                self.by_guild.remove(&guild_id);
            }
        }
    }

    /// The complete handshake for `guild_id`, if both halves arrived.
    #[must_use]
    pub fn complete(&self, guild_id: Snowflake) -> Option<PlayerVoiceUpdate> {
        let partial = self.by_guild.get(&guild_id)?;
        Some(PlayerVoiceUpdate {
            guild_id,
            token: partial.token.clone()?,
            endpoint: partial.endpoint.clone()?,
            session_id: partial.session_id.clone()?,
            channel_id: partial.channel_id?,
        })
    }
}

/// Complete voice handshake for one guild, ready to PATCH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerVoiceUpdate {
    /// Guild the player belongs to.
    pub guild_id: Snowflake,
    /// Discord voice token (`VoiceServerUpdate`).
    pub token: String,
    /// Discord voice endpoint (`VoiceServerUpdate`).
    pub endpoint: String,
    /// Bot voice session id (self `VoiceStateUpdate`).
    pub session_id: String,
    /// Voice channel the bot is joining (self `VoiceStateUpdate`).
    pub channel_id: Snowflake,
}

impl PlayerVoiceUpdate {
    /// `PATCH` path for this guild on a Lavalink session.
    #[must_use]
    pub fn player_path(&self, lavalink_session: &str) -> String {
        let guild_id = self.guild_id;
        format!("/v4/sessions/{lavalink_session}/players/{guild_id}")
    }

    /// Request body: `{"voice": {token, endpoint, sessionId, channelId}}`.
    /// Snowflakes stay strings, matching every other Lavalink v4 id field.
    #[must_use]
    pub fn voice_body(&self) -> serde_json::Value {
        serde_json::json!({
            "voice": {
                "token": self.token.clone(),
                "endpoint": self.endpoint.clone(),
                "sessionId": self.session_id.clone(),
                "channelId": self.channel_id.to_string(),
            }
        })
    }
}

/// True only when `guild_id` is the configured single guild. The forwarder
/// calls this before every PATCH so a misrouted dispatch can never drive
/// music outside the staging guild.
#[must_use]
pub fn is_staging_guild(guild_id: Snowflake, configured: Option<Snowflake>) -> bool {
    configured.is_some_and(|staging| staging == guild_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars_with(music: &str) -> HashMap<String, String> {
        [(String::from("TWO_MUSIC"), String::from(music))]
            .into_iter()
            .collect()
    }

    #[test]
    fn gates_enable_only_on_exact_1() {
        assert!(MusicGates::from_map(&vars_with("1")).enabled);
        for off in ["", "0", "true", "01", " 1", "1 "] {
            assert!(
                !MusicGates::from_map(&vars_with(off)).enabled,
                "TWO_MUSIC={off:?} must not enable music"
            );
        }
        assert!(!MusicGates::from_map(&HashMap::new()).enabled);
    }

    #[test]
    fn pairing_needs_both_halves() {
        let mut pairing = VoicePairing::new();
        pairing.observe_server(10, "token", "endpoint");
        assert_eq!(pairing.complete(10), None, "server half alone is not ready");
        pairing.observe_state(10, "session", Some(30));
        assert_eq!(
            pairing.complete(10),
            Some(PlayerVoiceUpdate {
                guild_id: 10,
                token: String::from("token"),
                endpoint: String::from("endpoint"),
                session_id: String::from("session"),
                channel_id: 30,
            })
        );
    }

    #[test]
    fn pairing_state_arrives_first() {
        let mut pairing = VoicePairing::new();
        pairing.observe_state(10, "session", Some(30));
        assert_eq!(pairing.complete(10), None, "state half alone is not ready");
        pairing.observe_server(10, "token", "endpoint");
        let update = pairing.complete(10).expect("both halves arrived");
        assert_eq!(update.session_id, "session");
        assert_eq!(update.token, "token");
    }

    #[test]
    fn pairing_leave_clears_only_that_guild() {
        let mut pairing = VoicePairing::new();
        pairing.observe_server(10, "token", "endpoint");
        pairing.observe_state(10, "session", Some(30));
        pairing.observe_server(11, "token", "endpoint");
        pairing.observe_state(11, "session", Some(31));
        assert!(pairing.complete(10).is_some());
        pairing.observe_state(10, "session", None);
        assert_eq!(pairing.complete(10), None, "leave drops the guild entry");
        assert!(
            pairing.complete(11).is_some(),
            "sibling guild keeps its entry"
        );
    }

    #[test]
    fn pairing_server_refresh_replaces_endpoint() {
        let mut pairing = VoicePairing::new();
        pairing.observe_server(10, "token", "old-endpoint");
        pairing.observe_state(10, "session", Some(30));
        pairing.observe_server(10, "token", "new-endpoint");
        let update = pairing.complete(10).expect("still complete after refresh");
        assert_eq!(update.endpoint, "new-endpoint");
    }

    #[test]
    fn player_path_shape() {
        let update = PlayerVoiceUpdate {
            guild_id: 10,
            token: String::from("token"),
            endpoint: String::from("endpoint"),
            session_id: String::from("session"),
            channel_id: 30,
        };
        assert_eq!(
            update.player_path("lavalink-session"),
            "/v4/sessions/lavalink-session/players/10"
        );
    }

    #[test]
    fn voice_body_carries_dave_fields() {
        let update = PlayerVoiceUpdate {
            guild_id: 10,
            token: String::from("token"),
            endpoint: String::from("endpoint"),
            session_id: String::from("session"),
            channel_id: 30,
        };
        let body = update.voice_body();
        let voice = &body["voice"];
        assert_eq!(voice["token"], "token");
        assert_eq!(voice["endpoint"], "endpoint");
        assert_eq!(voice["sessionId"], "session");
        assert_eq!(voice["channelId"], "30", "snowflakes stay strings in v4");
    }

    #[test]
    fn staging_fence() {
        assert!(is_staging_guild(10, Some(10)));
        assert!(
            !is_staging_guild(10, Some(11)),
            "other guild never forwards"
        );
        assert!(
            !is_staging_guild(10, None),
            "unconfigured guild never forwards"
        );
    }
}
