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

use crate::{Secret, Snowflake};

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
    /// Redacted under `Debug`: the voice token authenticates the node.
    token: Option<Secret<String>>,
    endpoint: Option<String>,
    session_id: Option<String>,
    channel_id: Option<Snowflake>,
    /// A server half arrived after the pair completed and no state has
    /// confirmed it yet. Disambiguates a region move (same session, no
    /// state follows) from a rejoin arriving server-first (new state
    /// follows and must keep, not drop, this half).
    ///
    /// Kept halves are ambiguous by construction: a region move followed
    /// later by an externally-initiated new session looks identical to a
    /// server-first rejoin, so the new session transiently pairs with the
    /// previous server half until its own server half lands and replaces
    /// it (one extra PATCH, then healed). Dropping instead would stall a
    /// genuine server-first rejoin whose server half already arrived — a
    /// permanent silence versus a self-healing transient — so keep wins.
    server_after_complete: bool,
}

impl PartialVoice {
    fn is_complete(&self) -> bool {
        self.token.is_some()
            && self.endpoint.is_some()
            && self.session_id.is_some()
            && self.channel_id.is_some()
    }
}

impl VoicePairing {
    /// Empty pairing cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a `VoiceServerUpdate` half. A repeat replaces the previous
    /// server half (Discord rotates the endpoint on region moves).
    ///
    /// Replacement is immediate on purpose: a region move sends no state
    /// afterwards, so holding the new half pending would pin the player to
    /// a dead endpoint. When the pair was already complete the half is also
    /// flagged, so a following new-session state keeps it (rejoin arriving
    /// server-first) instead of dropping it and stalling.
    pub fn observe_server(&mut self, guild_id: Snowflake, token: &str, endpoint: &str) {
        let entry = self.by_guild.entry(guild_id).or_default();
        entry.server_after_complete = entry.is_complete();
        entry.token = Some(Secret::new(token.to_owned()));
        entry.endpoint = Some(endpoint.to_owned());
    }

    /// Record a self `VoiceStateUpdate` half. `None` channel means the bot
    /// left voice: the guild entry is dropped (the follow-up slice destroys
    /// the player instead of PATCHing).
    ///
    /// A new session id starts a new handshake generation: the server half
    /// paired with the previous session is stale and is dropped, so the new
    /// session can never reuse the prior token or endpoint — unless a server
    /// half arrived after the pair completed, which is assumed to be the new
    /// generation arriving server-first and is kept (a region move followed
    /// by an external move therefore transiently re-pairs the region half
    /// until the new session's own server half heals it; see the field
    /// docs). A pending server half with no
    /// session yet is kept as the matching half (initial join server-first),
    /// and a repeated same-session state keeps the pair (mute/deafen frames
    /// carry the same session).
    pub fn observe_state(
        &mut self,
        guild_id: Snowflake,
        session_id: &str,
        channel_id: Option<Snowflake>,
    ) {
        match channel_id {
            Some(channel) => {
                let entry = self.by_guild.entry(guild_id).or_default();
                let fresh_session = entry
                    .session_id
                    .as_deref()
                    .is_some_and(|previous| previous != session_id);
                if fresh_session && !entry.server_after_complete {
                    entry.token = None;
                    entry.endpoint = None;
                }
                entry.server_after_complete = false;
                entry.session_id = Some(session_id.to_owned());
                entry.channel_id = Some(channel);
            }
            None => {
                self.by_guild.remove(&guild_id);
            }
        }
    }

    /// Drop any pending handshake for `guild_id`. The REST/WS slice calls
    /// this before sending `UpdateVoiceState` (join/move): our own join
    /// always brings fresh halves, so resetting first means a rejoin can
    /// never emit a stale mix whichever half arrives first. A rejoin whose
    /// server half arrives first without a reset still pairs transiently
    /// with the previous session (indistinguishable from a region move),
    /// then self-heals when its state lands instead of stalling, so the
    /// reset call keeps our own joins exact.
    pub fn reset(&mut self, guild_id: Snowflake) {
        self.by_guild.remove(&guild_id);
    }

    /// Whether `guild_id` holds a live entry (pending or complete).
    ///
    /// Callers use this to tell a leave that drops a real handshake from
    /// a leave (or a redelivered leave) that clears nothing.
    #[must_use]
    pub fn contains(&self, guild_id: Snowflake) -> bool {
        self.by_guild.contains_key(&guild_id)
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
    /// Discord voice token (`VoiceServerUpdate`), redacted under `Debug`.
    /// Exposed only into the PATCH body at the send site.
    pub token: Secret<String>,
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
    /// The token is exposed only here, into the PATCH body.
    #[must_use]
    pub fn voice_body(&self) -> serde_json::Value {
        serde_json::json!({
            "voice": {
                "token": self.token.expose(),
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
    fn contains_tracks_live_guild_entries() {
        let mut pairing = VoicePairing::new();
        assert!(!pairing.contains(10), "empty cache holds no guild");
        pairing.observe_server(10, "token", "endpoint");
        assert!(pairing.contains(10), "a pending half is a live entry");
        pairing.observe_state(10, "session", None);
        assert!(!pairing.contains(10), "leave drops the guild entry");
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
                token: Secret::new(String::from("token")),
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
        assert_eq!(update.token.expose(), "token");
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
            token: Secret::new(String::from("token")),
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
            token: Secret::new(String::from("token")),
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

    #[test]
    fn debug_redacts_voice_token() {
        let mut pairing = VoicePairing::new();
        pairing.observe_server(10, "secret-voice-token", "endpoint");
        pairing.observe_state(10, "session", Some(30));
        // The failure messages name the output index, never the output: the
        // token must not flow into any panic message (CodeQL cleartext
        // logging), even in a test that asserts its absence.
        for (idx, output) in [
            format!("{pairing:?}"),
            format!("{pairing:#?}"),
            format!("{:?}", pairing.complete(10).expect("complete pair")),
            format!("{:#?}", pairing.complete(10).expect("complete pair")),
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                !output.contains("secret-voice-token"),
                "Debug output {idx} must never print the voice token"
            );
            assert!(
                output.contains("[REDACTED]"),
                "Debug output {idx} marks the redacted token"
            );
        }
    }

    #[test]
    fn new_session_starts_new_handshake_generation() {
        let mut pairing = VoicePairing::new();
        pairing.observe_server(10, "old-token", "old-endpoint");
        pairing.observe_state(10, "old-session", Some(30));
        assert!(pairing.complete(10).is_some());
        // State-first rejoin: no server half arrived after the old pair
        // completed, so the stored one is stale and must not combine with
        // the new session.
        pairing.observe_state(10, "new-session", Some(30));
        assert_eq!(
            pairing.complete(10),
            None,
            "new session drops the stale server half"
        );
        pairing.observe_server(10, "new-token", "new-endpoint");
        let update = pairing.complete(10).expect("fresh halves pair");
        assert_eq!(update.session_id, "new-session");
        assert_eq!(update.token.expose(), "new-token");
        assert_eq!(update.endpoint, "new-endpoint");
    }

    #[test]
    fn rejoin_server_first_keeps_new_server_half() {
        let mut pairing = VoicePairing::new();
        pairing.observe_server(10, "old-token", "old-endpoint");
        pairing.observe_state(10, "old-session", Some(30));
        assert!(pairing.complete(10).is_some());
        // Server-first rejoin without a reset: the flagged half is the new
        // generation's, so the new state keeps it instead of stalling for
        // a server update that never comes.
        pairing.observe_server(10, "new-token", "new-endpoint");
        pairing.observe_state(10, "new-session", Some(30));
        let update = pairing.complete(10).expect("server-first rejoin pairs");
        assert_eq!(update.session_id, "new-session");
        assert_eq!(update.token.expose(), "new-token");
        assert_eq!(update.endpoint, "new-endpoint");
    }

    #[test]
    fn region_move_promotes_endpoint_immediately() {
        let mut pairing = VoicePairing::new();
        pairing.observe_server(10, "old-token", "old-endpoint");
        pairing.observe_state(10, "session", Some(30));
        // Region moves send no state afterwards: the new half must PATCH
        // at once, paired with the unchanged session.
        pairing.observe_server(10, "new-token", "new-endpoint");
        let update = pairing.complete(10).expect("region move stays complete");
        assert_eq!(update.session_id, "session");
        assert_eq!(update.token.expose(), "new-token");
        assert_eq!(update.endpoint, "new-endpoint");
        // No state follows: that is the region-move premise. The
        // confirming-state path is pinned by
        // `same_session_state_disarms_post_completion_flag` instead.
    }

    #[test]
    fn region_then_external_move_heals_on_next_server_half() {
        let mut pairing = VoicePairing::new();
        pairing.observe_server(10, "old-token", "old-endpoint");
        pairing.observe_state(10, "session", Some(30));
        // Region move: no state follows, the new half PATCHes at once.
        pairing.observe_server(10, "region-token", "region-endpoint");
        let update = pairing.complete(10).expect("region move stays complete");
        assert_eq!(update.token.expose(), "region-token");
        // A moderator moves the bot: new session, new channel, arriving
        // state-first. Indistinguishable from a server-first rejoin, so the
        // region half transiently pairs with the new session (documented
        // tradeoff) instead of stalling for a server half that, in the
        // rejoin case, already arrived.
        pairing.observe_state(10, "moved-session", Some(31));
        let transient = pairing
            .complete(10)
            .expect("transient re-pair, not a stall");
        assert_eq!(transient.session_id, "moved-session");
        assert_eq!(transient.token.expose(), "region-token");
        // The move's own server half lands and heals the pair.
        pairing.observe_server(10, "moved-token", "moved-endpoint");
        let update = pairing.complete(10).expect("move heals on its server half");
        assert_eq!(update.session_id, "moved-session");
        assert_eq!(update.token.expose(), "moved-token");
        assert_eq!(update.endpoint, "moved-endpoint");
        assert_eq!(update.channel_id, 31);
    }

    #[test]
    fn same_session_state_disarms_post_completion_flag() {
        let mut pairing = VoicePairing::new();
        pairing.observe_server(10, "old-token", "old-endpoint");
        pairing.observe_state(10, "session", Some(30));
        pairing.observe_server(10, "region-token", "region-endpoint");
        // A redundant same-session state confirms the region half and
        // disarms the flag without dropping anything.
        pairing.observe_state(10, "session", Some(30));
        let update = pairing.complete(10).expect("confirmed pair stays");
        assert_eq!(update.endpoint, "region-endpoint");
        // A later new session now drops the confirmed half: the flag is
        // gone, so this is a fresh generation, not a server-first rejoin.
        // Deleting the disarm makes this pair instead of returning `None`.
        pairing.observe_state(10, "new-session", Some(30));
        assert_eq!(
            pairing.complete(10),
            None,
            "confirmed half is stale once the session turns over"
        );
        pairing.observe_server(10, "new-token", "new-endpoint");
        let update = pairing.complete(10).expect("fresh halves pair");
        assert_eq!(update.session_id, "new-session");
        assert_eq!(update.token.expose(), "new-token");
    }

    #[test]
    fn same_session_reobserve_keeps_server_half() {
        let mut pairing = VoicePairing::new();
        pairing.observe_server(10, "token", "endpoint");
        pairing.observe_state(10, "session", Some(30));
        // Redelivered self state (mute/deafen frames carry the same session)
        // must not invalidate the handshake.
        pairing.observe_state(10, "session", Some(30));
        assert!(
            pairing.complete(10).is_some(),
            "same-session redelivery keeps the pair"
        );
    }

    #[test]
    fn reset_drops_pending_handshake() {
        let mut pairing = VoicePairing::new();
        pairing.observe_server(10, "token", "endpoint");
        pairing.observe_state(10, "session", Some(30));
        pairing.observe_server(11, "token", "endpoint");
        pairing.reset(10);
        assert_eq!(pairing.complete(10), None, "reset clears the guild entry");
        pairing.observe_state(11, "session", Some(31));
        assert!(
            pairing.complete(11).is_some(),
            "sibling guild keeps its entry"
        );
        // Reset is the rejoin generation boundary: after it, fresh halves
        // pair in either order with no stale mix.
        pairing.observe_server(10, "rejoin-token", "rejoin-endpoint");
        assert_eq!(
            pairing.complete(10),
            None,
            "server-first rejoin waits for its state"
        );
        pairing.observe_state(10, "rejoin-session", Some(30));
        let update = pairing.complete(10).expect("reset halves pair");
        assert_eq!(update.session_id, "rejoin-session");
        assert_eq!(update.token.expose(), "rejoin-token");
    }
}
