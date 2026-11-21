//! Request and evidence policy for the voice-move executor.
//! Preparing a request does not authorize or prove a move.

use super::numeric_id;
use twilight_http::{
    request::{Request, TryIntoRequest},
    Client,
};
use two_bot_core::{
    internal_actions::{ActionError, ErrorCode},
    send_admission::{cooldown_from_delays, SendCooldown},
};

pub const VOICE_MOVE_TIMEOUT_MS: u64 = 2000;

/// Proven pre-wire refusals retain their distinct provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceMoveNotSent {
    Blocked,
    Storage,
    Guard,
    Deadline,
    Revalidation,
}

/// A result is evidence supplied by the executor, not proof by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceMovePatch {
    Moved,
    Denied,
    Stale,
    Limited {
        retry_after_ms: Option<u64>,
        settlement_confirmed: bool,
    },
    Unavailable,
    NotSent(VoiceMoveNotSent),
}

/// Build the Twilight request only; this performs no admission or transport.
pub fn prepare_voice_move(
    factory: &Client,
    guild_id: &str,
    member_id: &str,
    channel_id: &str,
) -> Result<Request, ActionError> {
    finish_request(
        factory
            .update_guild_member(numeric_id(guild_id)?.cast(), numeric_id(member_id)?.cast())
            .channel_id(Some(numeric_id(channel_id)?.cast()))
            .try_into_request(),
    )
}

fn finish_request<E>(request: Result<Request, E>) -> Result<Request, ActionError> {
    request.map_err(|_| {
        ActionError::new(
            ErrorCode::Malformed,
            "Invalid Discord request",
            "discord_request_invalid",
        )
    })
}

/// Headers alone can prove these refusals, but never success or no-send.
#[must_use]
pub fn header_refusal(status: u16) -> Option<VoiceMovePatch> {
    match status {
        403 => Some(VoiceMovePatch::Denied),
        400 | 404 => Some(VoiceMovePatch::Stale),
        _ => None,
    }
}

/// Convert the core token-wide policy to finite milliseconds or indefinite.
#[must_use]
pub fn retry_delay_ms(header: Option<f64>, body: Option<f64>) -> Option<u64> {
    match cooldown_from_delays(header, body) {
        SendCooldown::FiniteMs(ms) => Some(ms),
        SendCooldown::Indefinite => None,
    }
}

#[cfg(test)]
#[path = "voice_move_tests.rs"]
mod tests;
